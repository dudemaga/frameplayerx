//! The application: owns the services, the open video and the panels, and
//! turns controller input into UI events and playback commands each frame.

mod actions;
mod frame;
mod input;
mod plex;
mod remote;
mod sources;
mod tasks;

use frame::{escape, key};
use tasks::Job;

use crate::jobs::Jobs;
use crate::playback::{self, OpenRequest, Opened, Playback};
use crate::services::Services;
use crate::settings::{Settings, UI_SCALES};
use crate::ui::{
    self, Action, Browse, QuickConnect, Screen, SourcePage, SourceView, UiState, UpdateStatus, View,
};
use crate::world::{
    Panel, Pointer, anchor_facing, anchor_from_head, follow, place, place_around, place_below,
    player_frame, turned_with_picture,
};
use fp_core::PlaybackStatus;
use fp_core::format::Projection;
use fp_library::{Library, PlaylistId};
use fp_media::{PlayerState, VideoFrame};
use fp_render::{QuadDraw, Renderer, VideoParams};
use fp_xr::Hand;
use glam::{Mat4, Quat, Vec2, Vec3};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Per-frame input from the XR session (or the preview simulator).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameInput {
    /// Seconds since start.
    pub time: f64,
    pub dt: f32,
    pub head: Option<(Vec3, Quat)>,
    pub hands: [Hand; 2],
    pub passthrough_available: bool,
    /// The compositor can draw panels and rays as quad layers (OpenXR);
    /// otherwise (the desktop preview) everything is drawn into the eyes.
    pub layers: bool,
    /// Render-thread time of the previous frame, ms, and the display's
    /// refresh rate (OpenXR), for the performance overlay.
    pub frame_ms: Option<f32>,
    pub display_hz: Option<f32>,
    /// The system recentered (the Steam button held) since the last frame.
    pub recentred: bool,
}

/// A panel or pointer ray for the compositor to draw as its own quad layer,
/// so it stays put as the head moves.
#[derive(Clone, Copy, Debug)]
pub enum LayerDraw {
    Panel {
        /// Stable per panel: the layer's swapchain is kept under it.
        key: u64,
        panel: fp_render::PanelId,
        px: [u32; 2],
        pose: Mat4,
        size: Vec2,
    },
    Ray {
        /// The active hand's ray is the brighter one.
        active: bool,
        pose: (Vec3, Quat),
        size: Vec2,
    },
}

/// What to draw and do this frame.
pub struct FrameOutput {
    pub frame: Option<Arc<VideoFrame>>,
    pub video: VideoParams,
    pub quads: Vec<QuadDraw>,
    /// Drawn on top of the eyes' pictures, in order (see [`LayerDraw`]).
    pub layers: Vec<LayerDraw>,
    pub passthrough: bool,
    /// (hand, amplitude, milliseconds)
    pub buzz: Vec<(usize, f32, i64)>,
    pub quit: bool,
    /// With `quit`: start again.
    pub restart: bool,
}

const MAIN: usize = 0;
const BAR: usize = 1;
const ADJUST: usize = 2;
const KEYBOARD: usize = 3;
/// The audio and subtitles picker, above the control bar.
const TRACKS: usize = 4;
/// The control bar's body in pixels.
const BAR_PX: [u32; 2] = [1400, 268];
/// Metres per pixel of the control bar and of the picker on it.
const BAR_M_PER_PX: f32 = 1.2 / 1400.0;
/// The performance overlay's quad layer (the panels' are their indices).
const PERF_LAYER: u64 = 8;
/// Where the performance overlay sits from the eyes: low and to the left.
const PERF_OFFSET: Vec3 = Vec3::new(-0.3, -0.12, -0.85);
/// How far below where you face the browse panel sits, degrees: what is in
/// front of you is a little below eye level (the panel's top bar, Quit
/// among it, is then about level with your eyes). It stands upright there,
/// like a flat screen, rather than tilting back towards your eyes.
const BELOW_DEG: f32 = 7.0;

/// How long a video may take to open before the app gives up on it. A
/// server that never sends a frame (a live transcode it can't keep up with)
/// would otherwise leave "Opening …" up for good.
const OPEN_TIMEOUT: Duration = Duration::from_secs(45);

/// How often settings that couldn't be saved are tried again.
const SETTINGS_RETRY: Duration = Duration::from_secs(10);

pub struct App {
    pub settings: Settings,
    /// The settings last applied.
    saved: Settings,
    /// When saving the settings last failed; it is tried again.
    unsaved_since: Option<Instant>,
    /// The latest Quick Connect pairing code request.
    quick_connect_request: u64,
    services: Services,
    pub ui: UiState,
    thumbs: ui::thumbs::Thumbs,
    panels: [Panel; 5],
    subs: Panel,
    /// The performance overlay, its sampler while it shows, the last
    /// second of frame times, and the video's counters at its last paint.
    perf: Panel,
    perf_monitor: Option<crate::perf::Monitor>,
    frame_times: std::collections::VecDeque<f32>,
    perf_video: Option<(Instant, u64, u64)>,
    sub_key: String,
    sub_textures: Vec<egui::TextureHandle>,
    /// The subtitle panel holds a 3D video's pictures, an eye's on each half.
    subs_split: bool,
    /// Sets subtitle text in any script, once its fonts have loaded.
    sub_shaper: Option<ui::subtitle_text::SubtitleText>,
    sub_fonts: Jobs<ui::subtitle_text::SubtitleText>,
    pointer: Pointer,
    pub playback: Option<Playback>,
    open_seq: u64,
    /// When the video being opened was asked for.
    open_started: Instant,
    jobs: Jobs<Job>,
    anchor: Option<Mat4>,
    reanchor: bool,
    ui_visible: bool,
    show_browser: bool,
    last_activity: Instant,
    /// Each hand's ray direction when it last counted as moving.
    activity_aim: [Option<Vec3>; 2],
    controls: crate::controls::Controls,
    /// Yaw/pitch when a dome drag began.
    drag_start: Option<(f32, f32)>,
    panel_drag: Option<PanelDrag>,
    /// Where the adjust panel was placed from when the trigger went down on
    /// it (see `place_panels`).
    adjust_hold: Option<Mat4>,
    /// Where the library and adjust panels sit now (saved when let go).
    offsets: crate::settings::PanelOffsets,
    /// The list the open video came from, for next/previous.
    /// Up next.
    queue: crate::queue::Queue,
    last_status: Option<PlaybackStatus>,
    /// When the web remote's details were last built.
    remote_details_at: Instant,
    /// The open video's picture for the web remote, by its location.
    remote_picture: Option<(String, Option<std::path::PathBuf>)>,
    update: Option<fp_updater::Update>,
    /// Counts update checks, so a check for a channel the user has since
    /// left is dropped.
    update_check: u64,
    /// The check made at start, which says when it finds a newer version,
    /// and what it says, waiting for the library panel to show.
    start_check: Option<u64>,
    update_notice: Option<String>,
    /// Removable drives seen at the last check, and when that was.
    mounts: Vec<std::path::PathBuf>,
    mounts_checked: Instant,
    /// Drives FramePlayerX has tried to mount since they were inserted.
    mount_tried: Vec<String>,
    /// Feed scenes (source, URL) waiting for their details, and how many
    /// are being fetched.
    feed_queue: std::collections::VecDeque<(String, String)>,
    feed_busy: usize,
    quit: bool,
    restart: bool,
}

/// A panel held by a grip: which panel and hand, and the hand's aim angles
/// and the panel's offset when it was grabbed.
#[derive(Clone, Copy)]
struct PanelDrag {
    panel: usize,
    hand: usize,
    start: (f32, f32),
    from: [f32; 2],
}

fn panel(px: [u32; 2], width_m: f32, ppp: f32) -> Panel {
    let size = Vec2::new(width_m, width_m * px[1] as f32 / px[0] as f32);
    Panel::new(px, size, ppp)
}

/// A panel whose body is `px` pixels and `width_m` wide, floating with
/// room round it for its shadow (ui::widgets::SHADOW_ROOM) at the same
/// scale.
/// A [`floating_panel`] with a fixed layout (the control bar, the
/// keyboard), which grows with Settings › Interface size: its texture is
/// fine enough for the largest size from the start.
fn growing_panel(px: [u32; 2], width_m: f32, ppp: f32) -> Panel {
    let s = *UI_SCALES.end();
    let px = px.map(|n| (n as f32 * s).round() as u32);
    floating_panel(px, width_m, ppp * s).growing()
}

fn floating_panel(px: [u32; 2], width_m: f32, ppp: f32) -> Panel {
    let room = ui::widgets::SHADOW_ROOM;
    let extra = [
        ((room.left + room.right) as f32 * ppp).round() as u32,
        ((room.top + room.bottom) as f32 * ppp).round() as u32,
    ];
    let full = [px[0] + extra[0], px[1] + extra[1]];
    panel(full, width_m * full[0] as f32 / px[0] as f32, ppp)
}

impl App {
    pub fn new(settings: Settings, library: Library) -> App {
        crate::i18n::set_language(&settings.interface_language());
        let services = Services::new(&settings, library);
        let thumbs = ui::thumbs::Thumbs::new(services.opener.clone(), services.images.clone());
        let mut panels = [
            floating_panel([1600, 1000], 1.8, 1.25),
            growing_panel(BAR_PX, BAR_PX[0] as f32 * BAR_M_PER_PX, 1.25),
            floating_panel([900, 1020], 0.75, 1.25),
            growing_panel([1200, 456], 1.0, 1.25),
            // As fine as the bar it sits on, and growing with it.
            growing_panel([850, 560], 850.0 * BAR_M_PER_PX, 1.25),
        ];
        panels[BAR].refresh = Some(Duration::from_millis(250));
        panels[ADJUST].refresh = Some(Duration::from_millis(1000));
        panels[TRACKS].refresh = Some(Duration::from_millis(1000));
        panels[MAIN].refresh = Some(Duration::from_millis(1000));
        let subs = panel([1600, 320], 1.6, 1.0);
        let mut perf = panel([640, 300], 0.4, 1.0);
        perf.ctx
            .style_mut(|s| s.visuals.panel_fill = egui::Color32::TRANSPARENT);
        perf.refresh = Some(Duration::from_millis(500));
        subs.ctx
            .style_mut(|s| s.visuals.panel_fill = egui::Color32::TRANSPARENT);
        let sub_fonts = Jobs::new();
        sub_fonts.spawn("fp-fonts", ui::subtitle_text::SubtitleText::load);
        let mut app = App {
            saved: settings.clone(),
            unsaved_since: None,
            quick_connect_request: 0,
            settings,
            services,
            ui: UiState::default(),
            thumbs,
            panels,
            subs,
            perf,
            perf_monitor: None,
            frame_times: Default::default(),
            perf_video: None,
            sub_key: String::new(),
            sub_textures: Vec::new(),
            subs_split: false,
            sub_shaper: None,
            sub_fonts,
            pointer: Pointer::default(),
            playback: None,
            open_seq: 0,
            open_started: Instant::now(),
            jobs: Jobs::new(),
            anchor: None,
            reanchor: true,
            ui_visible: true,
            show_browser: true,
            last_activity: Instant::now(),
            activity_aim: [None; 2],
            controls: Default::default(),
            drag_start: None,
            panel_drag: None,
            adjust_hold: None,
            offsets: Default::default(),
            queue: Default::default(),
            last_status: None,
            remote_details_at: Instant::now(),
            remote_picture: None,
            update: None,
            update_check: 0,
            start_check: None,
            update_notice: None,
            mounts: crate::drives::removable_mounts(),
            // In the past, so the first check (and mount) runs at once.
            mounts_checked: Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now),
            mount_tried: Vec::new(),
            feed_queue: Default::default(),
            feed_busy: 0,
            quit: false,
            restart: false,
        };
        app.offsets = app.settings.panel_offsets;
        app.apply_ui_scale();
        app.check_last_run();
        app.scan_libraries(|_| true, false, false);
        app.connect_devices();
        if app.settings.check_updates && fp_updater::RELEASE_PUBLIC_KEY.is_some() {
            app.check_updates();
            app.start_check = Some(app.update_check);
        }
        app
    }

    /// Settings › Interface size, on every panel.
    fn apply_ui_scale(&mut self) {
        let scale = self
            .settings
            .ui_scale
            .clamp(*UI_SCALES.start(), *UI_SCALES.end());
        for p in self.panels.iter_mut() {
            p.set_scale(scale);
        }
    }

    pub fn set_about(&mut self, about: Vec<(String, String)>) {
        self.ui.about = about;
    }

    /// Opens a location (path or URL) as if picked in the UI.
    pub fn open(&mut self, req: OpenRequest) {
        self.apply(Action::Open(req));
    }

    /// One line about the open video and the UI, for the preview harness.
    pub fn status_line(&self) -> String {
        let ui = format!(
            "ui_visible={} browser={} adjust={} tracks={}",
            self.ui_visible, self.show_browser, self.ui.adjust_open, self.ui.tracks_open
        );
        match &self.playback {
            Some(p) => format!(
                "{} playing={} state={:?} pos={:.1}/{:.1} vol={:.2} zoom={:.2} yaw={:.1} pitch={:.1} \
                 queue={}/{} paused={} {ui}",
                p.title,
                p.transcoding.as_ref().map_or("file", |t| t.label.as_str()),
                p.player.state(),
                p.player.position(),
                p.player.duration(),
                p.player.volume(),
                p.settings.zoom,
                p.settings.yaw,
                p.settings.pitch,
                self.queue.pos + 1,
                self.queue.items.len(),
                p.player.is_paused()
            ),
            None => format!("no video, screen={:?} {ui}", self.ui.screen),
        }
    }

    /// World position of a fractional point on a visible panel (tests).
    fn panel_index(name: &str) -> Option<usize> {
        Some(match name {
            "main" => MAIN,
            "bar" => BAR,
            "adjust" => ADJUST,
            "keyboard" => KEYBOARD,
            "tracks" => TRACKS,
            _ => return None,
        })
    }

    /// Repaints every panel next frame (the preview changed the UI state).
    pub fn repaint_all(&mut self) {
        for p in self.panels.iter_mut() {
            p.request_repaint();
        }
    }

    /// The renderer's image of a visible panel, for preview screenshots.
    pub fn panel_id(&self, name: &str) -> Option<fp_render::PanelId> {
        let p = match name {
            "subs" => &self.subs,
            _ => &self.panels[Self::panel_index(name)?],
        };
        p.visible.then_some(p.id).flatten()
    }

    pub fn panel_point(&self, name: &str, x: f32, y: f32) -> Option<Vec3> {
        let i = Self::panel_index(name)?;
        let p = &self.panels[i];
        if !p.visible {
            return None;
        }
        let pts = p.points();
        Some(p.world_point(egui::pos2(x * pts.x, y * pts.y)))
    }

    /// Saves and applies settings edited through the UI.
    fn sync_settings(&mut self) {
        if self.settings == self.saved {
            if self
                .unsaved_since
                .is_some_and(|t| t.elapsed() > SETTINGS_RETRY)
            {
                self.save_settings();
            }
            return;
        }
        if self.settings.remote != self.saved.remote {
            self.services.apply_remote(&self.settings.remote);
        }
        if self.settings.haptics_enabled != self.saved.haptics_enabled
            || self.settings.haptics != self.saved.haptics
        {
            self.services.set_haptics(&self.settings);
            if self.settings.haptics_enabled && !self.saved.haptics_enabled {
                self.connect_devices();
            }
        }
        if self.settings.ui_scale != self.saved.ui_scale {
            self.apply_ui_scale();
        }
        if self.settings.language != self.saved.language {
            crate::i18n::set_language(&self.settings.interface_language());
            self.services.set_remote_language();
            // Chinese or Japanese forms of the characters they share.
            for p in self.panels.iter_mut() {
                ui::theme::set_fonts(&p.ctx);
            }
        }
        if self.settings.image_cache_mb != self.saved.image_cache_mb {
            self.services.images.set_limit(self.settings.image_cache_mb);
        }
        if (self.settings.make_thumbnails, self.settings.resume_frames)
            != (self.saved.make_thumbnails, self.saved.resume_frames)
        {
            self.services.set_worker_options(&self.settings);
        }
        self.saved = self.settings.clone();
        self.save_settings();
    }

    /// Writes the settings. A failure is shown once and tried again every
    /// [`SETTINGS_RETRY`] until it works.
    fn save_settings(&mut self) {
        match self.settings.save(&Settings::path()) {
            Ok(()) => self.unsaved_since = None,
            Err(e) => {
                if self.unsaved_since.is_none() {
                    log::warn!("saving settings: {e}");
                    self.ui
                        .toast(tr!("error-save-settings", error = e.to_string()));
                }
                self.unsaved_since = Some(Instant::now());
            }
        }
    }

    fn remote_events(&mut self) {
        for ev in self.services.remote_events() {
            match ev {
                fp_remote::RemoteEvent::Command(c) => {
                    use fp_core::PlayerCommand as C;
                    let a = match c {
                        C::Open { location } => Action::Open(OpenRequest {
                            location,
                            ..Default::default()
                        }),
                        C::Play => {
                            if let Some(p) = &self.playback {
                                p.player.play();
                            }
                            continue;
                        }
                        C::Pause => {
                            if let Some(p) = &self.playback {
                                p.player.pause();
                            }
                            continue;
                        }
                        C::TogglePause => Action::TogglePause,
                        C::Seek { position } => Action::Seek(position),
                        C::SeekRelative { delta } => Action::SeekRelative(delta),
                        C::SetSpeed { speed } => Action::SetSpeed(speed),
                        C::Stop => Action::ClosePlayback,
                        C::Recenter => Action::Recenter,
                        C::SetVolume { volume } => {
                            self.ui.unmute_volume = None;
                            Action::SetVolume(volume as f32)
                        }
                        C::ToggleMute => {
                            self.toggle_mute();
                            continue;
                        }
                        C::Previous => Action::StepQueue(-1),
                        C::Next => Action::StepQueue(1),
                        C::SelectAudio { stream } => Action::SelectAudio(stream),
                        C::SelectSubtitles { track } => {
                            use crate::subtitles::Pick;
                            Action::SelectSubtitles(match track {
                                None => Pick::Off,
                                Some(fp_core::SubtitleTrack::Stream(i)) => Pick::Stream(i),
                                Some(fp_core::SubtitleTrack::File(i)) => Pick::File(i),
                            })
                        }
                        C::PlayQueued { index } => Action::QueueJump(index),
                        // Set, not flipped: a tap the phone sends twice
                        // doesn't undo itself.
                        C::SetLoopVideo { on } => {
                            self.settings.loop_video = on;
                            continue;
                        }
                        C::SetAutoplay { on } => {
                            self.settings.autoplay = on;
                            continue;
                        }
                        C::SetLoopPlaylist { on } => {
                            self.settings.loop_playlist = on;
                            continue;
                        }
                    };
                    self.apply(a);
                }
                fp_remote::RemoteEvent::Text(t) => {
                    // Typed on the phone: into whichever panel has a focused field.
                    if let Some(p) = self
                        .panels
                        .iter_mut()
                        .find(|p| p.visible && p.wants_keyboard())
                    {
                        p.push(egui::Event::Text(t));
                    } else {
                        // A search of what was watched.
                        self.ui.history.search = t;
                        self.ui.screen = Screen::History;
                        self.ui.details = None;
                        self.ui.invalidate();
                        self.show_browser = true;
                        self.ui_visible = true;
                    }
                }
            }
        }
    }

    /// Publishes playback status to remotes and the haptics engine.
    fn publish_status(&mut self) {
        let Some(p) = &self.playback else {
            // Once, so remotes and haptics stop following the closed video.
            if self.last_status.take().is_some() {
                let idle = PlaybackStatus {
                    sampled_at_ms: fp_core::playback::now_ms(),
                    ..Default::default()
                };
                if let Some(h) = self.services.haptics() {
                    h.set_status(idle.clone());
                }
                if let Some(r) = &self.services.remote {
                    r.publish(&idle);
                }
            }
            return;
        };
        let s = p.status();
        let changed = match &self.last_status {
            Some(prev) => {
                fp_remote::significant_change(prev, &s, s.sampled_at_ms)
                    || s.sampled_at_ms.saturating_sub(prev.sampled_at_ms) > 1000
            }
            None => true,
        };
        if changed {
            if let Some(h) = self.services.haptics() {
                h.set_status(s.clone());
            }
            if let Some(r) = &self.services.remote {
                r.publish(&s);
            }
            self.last_status = Some(s);
        }
    }

    pub fn shutdown(mut self) {
        // Cut short the thumbnail being made, so stopping the worker below
        // does not wait for it.
        self.services.set_worker_busy(true);
        if let Some(p) = self.playback.take() {
            p.close(&self.services.library);
        }
        self.services.watch.flush(Duration::from_secs(3));
        self.sync_settings();
        if let Some(r) = self.services.remote.take() {
            r.stop();
        }
        if let Some(w) = self.services.worker.take() {
            w.stop();
        }
    }
}
