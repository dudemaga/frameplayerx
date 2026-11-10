//! Session lifecycle, swapchains and the frame loop.

use crate::context::XrContext;
use crate::input::{Actions, InputState};
use crate::{Error, Result, XrContextExt};
use ash::vk::{self, Handle};
use fp_render::{EyeTarget, EyeView, Gpu};
use glam::{Quat, Vec3};
use openxr as xr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What happened while polling events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// Nothing that needs the app's attention.
    None,
    /// The session is running and frames should be produced.
    Running,
    /// The runtime asked us to stop (headset removed, app closed from the
    /// system menu): leave the loop.
    Exit,
}

struct Eye {
    swapchain: xr::Swapchain<xr::Vulkan>,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
}

/// A flat picture the compositor draws itself, at its own place in the
/// world (`XrCompositionLayerQuad`): UI panels and the pointer. The
/// compositor puts it at its pose for every displayed frame, so it stays put
/// as the head moves, where content in the projection layer is re-warped as
/// if it were far away.
struct QuadLayer {
    swapchain: xr::Swapchain<xr::Vulkan>,
    images: Vec<vk::Image>,
    width: u32,
    height: u32,
    /// Acquired this frame, to release before xrEndFrame.
    acquired: bool,
    /// An image has been released at least once: the layer can be shown.
    ready: bool,
}

/// A quad layer to show this frame.
#[derive(Clone, Copy, Debug)]
pub struct QuadSubmit {
    /// The key the layer was created under.
    pub key: u64,
    /// Centre and orientation in the play space; the picture faces +Z.
    pub pose: (Vec3, Quat),
    /// Width and height in metres.
    pub size: glam::Vec2,
}

pub struct XrSession {
    ctx: Arc<XrContext>,
    gpu: Arc<Gpu>,
    pub session: xr::Session<xr::Vulkan>,
    waiter: xr::FrameWaiter,
    stream: xr::FrameStream<xr::Vulkan>,
    space: xr::Space,
    view_space: xr::Space,
    eyes: Vec<Eye>,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    actions: Actions,
    state: xr::SessionState,
    running: bool,
    pub blend_mode: xr::EnvironmentBlendMode,
    event_buf: xr::EventDataBuffer,
    /// When the system's recenter (holding the Steam button) moves the play
    /// space; see [`XrSession::recentred_by`].
    recenter_at: Option<xr::Time>,
    /// Dashboard closes left before the first focus, and when the last was;
    /// see [`XrSession::close_dashboard_at_launch`].
    launch_closes: u32,
    launch_closed_at: Option<Instant>,
    quads: std::collections::HashMap<u64, QuadLayer>,
}

/// One frame in progress.
pub struct FrameCtx {
    pub state: xr::FrameState,
    /// Eye cameras for rendering (empty when the runtime says not to render).
    pub eyes: Option<[EyeView; 2]>,
    /// Head pose in the play space.
    pub head: Option<(Vec3, Quat)>,
    pub targets: Option<[EyeTarget; 2]>,
    views: Vec<xr::View>,
}

/// Preferred eye formats, sRGB first.
const FORMATS: [vk::Format; 2] = [vk::Format::R8G8B8A8_SRGB, vk::Format::B8G8R8A8_SRGB];

impl XrSession {
    /// Creates the session on `gpu` (which must have been created with
    /// `ctx.creator()`), with eye pictures `eye_resolution` pixels across.
    pub fn new(ctx: Arc<XrContext>, gpu: Arc<Gpu>, eye_resolution: u32) -> Result<XrSession> {
        let mut actions = Actions::new(&ctx.instance, ctx.enabled.ext_eye_gaze_interaction)?;
        let info = xr::vulkan::SessionCreateInfo {
            instance: gpu.instance.handle().as_raw() as _,
            physical_device: gpu.pdev.as_raw() as _,
            device: gpu.device.handle().as_raw() as _,
            queue_family_index: gpu.queue_family,
            queue_index: 0,
        };
        // SAFETY: the Vulkan objects were created through this runtime and
        // outlive the session (the app drops the session first).
        let (session, waiter, stream) =
            unsafe { ctx.instance.create_session::<xr::Vulkan>(ctx.system, &info) }
                .ctx("xrCreateSession")?;
        actions.attach(&session)?;
        // STAGE, as games use. With LOCAL SteamVR switches to seated
        // tracking, and the dashboard opened at Quit came up far too high.
        let base = if session
            .enumerate_reference_spaces()
            .ctx("xrEnumerateReferenceSpaces")?
            .contains(&xr::ReferenceSpaceType::STAGE)
        {
            xr::ReferenceSpaceType::STAGE
        } else {
            xr::ReferenceSpaceType::LOCAL
        };
        let space = session
            .create_reference_space(base, xr::Posef::IDENTITY)
            .ctx("play space")?;
        log::info!("play space {base:?}");
        let view_space = session
            .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
            .ctx("view space")?;
        let formats = session
            .enumerate_swapchain_formats()
            .ctx("xrEnumerateSwapchainFormats")?;
        let format = FORMATS
            .iter()
            .copied()
            .find(|f| formats.contains(&(f.as_raw() as u32)))
            .ok_or_else(|| Error::Runtime(format!("no sRGB swapchain format among {formats:?}")))?;
        let extent = eye_extent(&ctx, eye_resolution)?;
        let eyes = make_eyes(&session, &gpu, format, extent)?;
        let blend_mode = ctx
            .blend_modes
            .first()
            .copied()
            .unwrap_or(xr::EnvironmentBlendMode::OPAQUE);
        let ((rw, rh), (mw, mh)) = ctx.eye_sizes()?;
        log::info!(
            "XR session: {}x{} per eye (runtime recommends {rw}x{rh}, allows up to {mw}x{mh}),              {format:?}, blend {blend_mode:?}",
            extent.width,
            extent.height
        );
        Ok(XrSession {
            ctx,
            gpu,
            session,
            waiter,
            stream,
            space,
            view_space,
            eyes,
            extent,
            format,
            actions,
            state: xr::SessionState::UNKNOWN,
            running: false,
            blend_mode,
            event_buf: xr::EventDataBuffer::new(),
            recenter_at: None,
            launch_closes: 3,
            launch_closed_at: None,
            quads: std::collections::HashMap::new(),
        })
    }

    /// Makes the eye pictures `resolution` pixels across. Call between
    /// frames.
    pub fn set_eye_resolution(&mut self, resolution: u32) -> Result<()> {
        let extent = eye_extent(&self.ctx, resolution)?;
        if extent == self.extent {
            return Ok(());
        }
        let eyes = make_eyes(&self.session, &self.gpu, self.format, extent)?;
        self.gpu.wait_idle();
        for e in std::mem::replace(&mut self.eyes, eyes) {
            for v in e.views {
                // SAFETY: GPU idle; views were created by us.
                unsafe { self.gpu.device.destroy_image_view(v, None) };
            }
        }
        self.extent = extent;
        log::info!("eye pictures {}x{}", extent.width, extent.height);
        Ok(())
    }

    /// Runs the display at `hz`, through SteamVR's setting for this app: it
    /// changes the rate at once, while `XR_FB_display_refresh_rate` offers
    /// only the current rate and refuses requests.
    pub fn set_refresh_rate(&self, hz: u32) {
        self.ctx.set_steamvr_refresh_rate(hz);
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn state(&self) -> xr::SessionState {
        self.state
    }

    /// Whether the runtime supports a blend mode showing the real world.
    pub fn supports_passthrough_blend(&self) -> bool {
        self.ctx
            .blend_modes
            .contains(&xr::EnvironmentBlendMode::ALPHA_BLEND)
    }

    pub fn set_passthrough(&mut self, on: bool) {
        self.blend_mode = if on && self.supports_passthrough_blend() {
            xr::EnvironmentBlendMode::ALPHA_BLEND
        } else {
            xr::EnvironmentBlendMode::OPAQUE
        };
    }

    /// Asks the runtime to end the session; [`XrSession::poll`] reports
    /// [`SessionEvent::Exit`] once it has.
    pub fn request_exit(&self) {
        if self.running
            && let Err(e) = self.session.request_exit()
        {
            log::warn!("xrRequestExitSession: {e}");
        }
    }

    /// Closes SteamVR's dashboard while it covers the app before the app's
    /// first focus, 3 times at most, 2 s apart: Steam leaves it, with its
    /// game menu, in front of a non-Steam shortcut it starts, where SteamVR
    /// closes it for a store game. Call once per loop iteration.
    pub fn close_dashboard_at_launch(&mut self) {
        match self.state {
            xr::SessionState::FOCUSED => self.launch_closes = 0,
            xr::SessionState::VISIBLE
                if self.launch_closes > 0
                    && self
                        .launch_closed_at
                        .is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) =>
            {
                self.ctx.hide_steamvr_dashboard();
                self.launch_closes -= 1;
                self.launch_closed_at = Some(Instant::now());
            }
            _ => {}
        }
    }

    /// Processes runtime events. Call once per loop iteration.
    pub fn poll(&mut self) -> Result<SessionEvent> {
        let mut result = SessionEvent::None;
        while let Some(event) = self
            .ctx
            .instance
            .poll_event(&mut self.event_buf)
            .ctx("xrPollEvent")?
        {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    self.state = e.state();
                    log::info!("XR session state: {:?}", self.state);
                    match self.state {
                        xr::SessionState::READY => {
                            self.session
                                .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                                .ctx("xrBeginSession")?;
                            self.running = true;
                        }
                        xr::SessionState::STOPPING => {
                            self.session.end().ctx("xrEndSession")?;
                            self.running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            return Ok(SessionEvent::Exit);
                        }
                        _ => {}
                    }
                }
                xr::Event::ReferenceSpaceChangePending(e)
                    if matches!(
                        e.reference_space_type(),
                        xr::ReferenceSpaceType::STAGE | xr::ReferenceSpaceType::LOCAL
                    ) =>
                {
                    log::info!("system recenter");
                    self.recenter_at = Some(e.change_time());
                }
                xr::Event::InstanceLossPending(_) => return Ok(SessionEvent::Exit),
                _ => {}
            }
        }
        if self.running {
            result = SessionEvent::Running;
        }
        Ok(result)
    }

    /// Whether the system recentered and the moved play space is in use by
    /// `time` (a frame's predicted display time); true once per recenter.
    /// The space stays level, so it only turns to where the head faces.
    pub fn recentred_by(&mut self, time: xr::Time) -> bool {
        match self.recenter_at {
            Some(at) if time.as_nanos() >= at.as_nanos() => {
                self.recenter_at = None;
                true
            }
            _ => false,
        }
    }

    /// Waits for the next frame, locates the views and acquires swapchain
    /// images. Call [`XrSession::end_frame`] after submitting GPU work.
    pub fn begin_frame(&mut self) -> Result<FrameCtx> {
        let state = self.waiter.wait().ctx("xrWaitFrame")?;
        self.stream.begin().ctx("xrBeginFrame")?;
        let mut ctx = FrameCtx {
            state,
            eyes: None,
            head: None,
            targets: None,
            views: Vec::new(),
        };
        if !state.should_render {
            return Ok(ctx);
        }
        let (flags, views) = self
            .session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                state.predicted_display_time,
                &self.space,
            )
            .ctx("xrLocateViews")?;
        if !flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) || views.len() < 2 {
            return Ok(ctx);
        }
        if let Ok(loc) = self
            .view_space
            .locate(&self.space, state.predicted_display_time)
        {
            ctx.head = Some(crate::pose(loc.pose));
        }
        let eye = |v: &xr::View| {
            let (p, q) = crate::pose(v.pose);
            EyeView {
                view: fp_render::math::view(p, q),
                proj: fp_render::math::projection(
                    fp_render::math::Fov {
                        left: v.fov.angle_left,
                        right: v.fov.angle_right,
                        up: v.fov.angle_up,
                        down: v.fov.angle_down,
                    },
                    0.05,
                    200.0,
                ),
            }
        };
        ctx.eyes = Some([eye(&views[0]), eye(&views[1])]);
        let mut targets = Vec::new();
        for e in &mut self.eyes {
            let i = e.swapchain.acquire_image().ctx("xrAcquireSwapchainImage")? as usize;
            e.swapchain
                .wait_image(xr::Duration::INFINITE)
                .ctx("xrWaitSwapchainImage")?;
            targets.push(EyeTarget {
                image: e.images[i],
                view: e.views[i],
                extent: self.extent,
                final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            });
        }
        ctx.targets = Some([targets[0], targets[1]]);
        ctx.views = views;
        Ok(ctx)
    }

    /// Quad layers need RGBA8 sRGB swapchains (the format panels are
    /// painted in); otherwise everything stays in the projection layer.
    pub fn supports_quad_layers(&self) -> bool {
        self.format == vk::Format::R8G8B8A8_SRGB
    }

    fn quad_layer(&mut self, key: u64, width: u32, height: u32) -> Result<&mut QuadLayer> {
        let stale = self
            .quads
            .get(&key)
            .is_some_and(|q| (q.width, q.height) != (width, height));
        if stale {
            self.quads.remove(&key);
        }
        if !self.quads.contains_key(&key) {
            let swapchain = self
                .session
                .create_swapchain(&xr::SwapchainCreateInfo {
                    create_flags: xr::SwapchainCreateFlags::EMPTY,
                    usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | xr::SwapchainUsageFlags::TRANSFER_DST,
                    format: self.format.as_raw() as u32,
                    sample_count: 1,
                    width,
                    height,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })
                .ctx("xrCreateSwapchain (quad layer)")?;
            let images = swapchain
                .enumerate_images()
                .ctx("xrEnumerateSwapchainImages")?
                .into_iter()
                .map(vk::Image::from_raw)
                .collect();
            self.quads.insert(
                key,
                QuadLayer {
                    swapchain,
                    images,
                    width,
                    height,
                    acquired: false,
                    ready: false,
                },
            );
        }
        Ok(self.quads.get_mut(&key).expect("inserted above"))
    }

    /// The image to draw a quad layer's new picture into this frame
    /// (created on first use, `width`x`height`). The caller records the
    /// writes into this frame's GPU work; [`XrSession::end_frame`] releases
    /// it. Call at most once per key and frame.
    pub fn quad_layer_image(&mut self, key: u64, width: u32, height: u32) -> Result<vk::Image> {
        let q = self.quad_layer(key, width, height)?;
        let i = q
            .swapchain
            .acquire_image()
            .ctx("xrAcquireSwapchainImage (quad layer)")? as usize;
        q.swapchain
            .wait_image(xr::Duration::INFINITE)
            .ctx("xrWaitSwapchainImage (quad layer)")?;
        q.acquired = true;
        Ok(q.images[i])
    }

    /// Whether a quad layer has a picture to show.
    pub fn quad_layer_ready(&self, key: u64) -> bool {
        self.quads.get(&key).is_some_and(|q| q.ready || q.acquired)
    }

    /// A quad layer whose picture never changes: `fill` writes it once,
    /// waiting for the GPU, and it is released straight away.
    pub fn static_quad_layer(
        &mut self,
        key: u64,
        width: u32,
        height: u32,
        fill: impl FnOnce(vk::Image) -> std::result::Result<(), String>,
    ) -> Result<()> {
        if self.quad_layer_ready(key) {
            return Ok(());
        }
        let image = self.quad_layer_image(key, width, height)?;
        fill(image).map_err(Error::Runtime)?;
        let q = self.quads.get_mut(&key).expect("created above");
        q.swapchain
            .release_image()
            .ctx("xrReleaseSwapchainImage (quad layer)")?;
        q.acquired = false;
        q.ready = true;
        Ok(())
    }

    /// Releases the images and submits the projection layer, then `quads`
    /// on top of it in order.
    pub fn end_frame(&mut self, ctx: FrameCtx, quads: &[QuadSubmit]) -> Result<()> {
        let time = ctx.state.predicted_display_time;
        for q in self.quads.values_mut() {
            if q.acquired {
                q.swapchain
                    .release_image()
                    .ctx("xrReleaseSwapchainImage (quad layer)")?;
                q.acquired = false;
                q.ready = true;
            }
        }
        if ctx.targets.is_none() {
            return self
                .stream
                .end(time, self.blend_mode, &[])
                .ctx("xrEndFrame");
        }
        for e in &mut self.eyes {
            e.swapchain.release_image().ctx("xrReleaseSwapchainImage")?;
        }
        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.extent.width as i32,
                height: self.extent.height as i32,
            },
        };
        let views: Vec<xr::CompositionLayerProjectionView<xr::Vulkan>> = ctx
            .views
            .iter()
            .zip(&self.eyes)
            .map(|(v, e)| {
                xr::CompositionLayerProjectionView::new()
                    .pose(v.pose)
                    .fov(v.fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&e.swapchain)
                            .image_rect(rect)
                            .image_array_index(0),
                    )
            })
            .collect();
        let mut flags = xr::CompositionLayerFlags::EMPTY;
        if self.blend_mode == xr::EnvironmentBlendMode::ALPHA_BLEND {
            flags |= xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;
        }
        let layer = xr::CompositionLayerProjection::new()
            .layer_flags(flags)
            .space(&self.space)
            .views(&views);
        let quad_layers: Vec<xr::CompositionLayerQuad<xr::Vulkan>> = quads
            .iter()
            .filter_map(|s| {
                let q = self.quads.get(&s.key).filter(|q| q.ready)?;
                Some(
                    xr::CompositionLayerQuad::new()
                        // Rounded corners and the ray's soft edges are alpha.
                        .layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
                        .space(&self.space)
                        .eye_visibility(xr::EyeVisibility::BOTH)
                        .sub_image(
                            xr::SwapchainSubImage::new()
                                .swapchain(&q.swapchain)
                                .image_rect(xr::Rect2Di {
                                    offset: xr::Offset2Di { x: 0, y: 0 },
                                    extent: xr::Extent2Di {
                                        width: q.width as i32,
                                        height: q.height as i32,
                                    },
                                })
                                .image_array_index(0),
                        )
                        .pose(crate::to_pose(s.pose.0, s.pose.1))
                        .size(xr::Extent2Df {
                            width: s.size.x,
                            height: s.size.y,
                        }),
                )
            })
            .collect();
        let mut all: Vec<&xr::CompositionLayerBase<xr::Vulkan>> = vec![&*layer];
        all.extend(quad_layers.iter().map(|q| &**q));
        self.stream
            .end(time, self.blend_mode, &all)
            .ctx("xrEndFrame")
    }

    /// Controller state at `time`.
    pub fn input(&self, time: xr::Time) -> InputState {
        self.actions.read(&self.session, &self.space, time)
    }

    pub fn buzz(&self, hand: usize, amplitude: f32, millis: i64) {
        self.actions.buzz(&self.session, hand, amplitude, millis);
    }

    /// Interaction profiles that accepted bindings.
    pub fn bindings(&self) -> &[crate::BoundProfile] {
        &self.actions.bound
    }
}

/// The eye pictures' size for `resolution` pixels across: the runtime's
/// recommended shape, within its largest.
fn eye_extent(ctx: &XrContext, resolution: u32) -> Result<vk::Extent2D> {
    let ((rw, rh), (mw, mh)) = ctx.eye_sizes()?;
    let width = resolution.min(mw).max(1);
    let height = (u64::from(width) * u64::from(rh) / u64::from(rw.max(1))) as u32;
    Ok(vk::Extent2D {
        width,
        height: height.clamp(1, mh.max(1)),
    })
}

/// Each eye's swapchain at `extent`, with a view of every image.
fn make_eyes(
    session: &xr::Session<xr::Vulkan>,
    gpu: &Gpu,
    format: vk::Format,
    extent: vk::Extent2D,
) -> Result<Vec<Eye>> {
    let mut eyes = Vec::new();
    for _ in 0..2 {
        let swapchain = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                    | xr::SwapchainUsageFlags::SAMPLED,
                format: format.as_raw() as u32,
                sample_count: 1,
                width: extent.width,
                height: extent.height,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })
            .ctx("xrCreateSwapchain")?;
        let images: Vec<vk::Image> = swapchain
            .enumerate_images()
            .ctx("xrEnumerateSwapchainImages")?
            .into_iter()
            .map(vk::Image::from_raw)
            .collect();
        let mut views = Vec::new();
        for &img in &images {
            views.push(fp_render::mem::view(gpu, img, format, 0, 1)?);
        }
        eyes.push(Eye {
            swapchain,
            images,
            views,
        });
    }
    Ok(eyes)
}

impl Drop for XrSession {
    fn drop(&mut self) {
        self.gpu.wait_idle();
        for e in &self.eyes {
            for &v in &e.views {
                // SAFETY: GPU idle; views were created by us.
                unsafe { self.gpu.device.destroy_image_view(v, None) };
            }
        }
    }
}
