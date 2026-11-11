//! Each frame: where the panels go, painting them, and what the compositor
//! draws.

use super::*;

impl App {
    /// Forward becomes where the head faces, and the browse panel comes
    /// back in front, wherever it was moved. A flat screen comes back in
    /// front too (its yaw and pitch only place it; a 180°/360° picture keeps
    /// its turn), so the two line up. The adjust panel keeps its place
    /// beside the player.
    pub(super) fn recenter(&mut self) {
        self.reanchor = true;
        self.panel_drag = None;
        self.offsets.main = [0.0, 0.0];
        self.settings.panel_offsets = self.offsets;
        if let Some(p) = &mut self.playback
            && p.format.projection == Projection::Flat
            && (p.settings.yaw, p.settings.pitch) != (0.0, 0.0)
        {
            p.settings.yaw = 0.0;
            p.settings.pitch = 0.0;
            p.settings_dirty = true;
        }
    }

    /// A flat screen, and the panels that move, back where they start,
    /// wherever they were moved. A 180°/360°
    /// picture's turn is the video's own adjustment and stays (its Reset
    /// puts that back).
    pub(super) fn reset_placement(&mut self) {
        self.panel_drag = None;
        self.adjust_hold = None;
        self.offsets = Default::default();
        self.settings.panel_offsets = self.offsets;
        if let Some(p) = &mut self.playback
            && p.format.projection == Projection::Flat
            && (p.settings.yaw, p.settings.pitch) != (0.0, 0.0)
        {
            p.settings.yaw = 0.0;
            p.settings.pitch = 0.0;
            p.settings_dirty = true;
        }
    }

    /// The panel the keyboard types into: the library or adjust panel with
    /// a text field in focus.
    pub(super) fn keyboard_target(&self) -> Option<usize> {
        [MAIN, ADJUST]
            .into_iter()
            .find(|&i| self.panels[i].visible && self.panels[i].wants_keyboard())
    }

    /// Squeezing a grip on the library panel, the control bar, the adjust
    /// panel or the performance overlay moves it round the viewer with that
    /// hand; where it is let go is saved: the bar's relative to a flat screen,
    /// the adjust panel's relative to the bar, so each goes along when what it
    /// is placed from moves. Squeezing one on the keyboard moves the panel it
    /// types into, and the keyboard with it; on the audio and subtitles
    /// picker, the bar it sits on. The picture itself is dragged with grip
    /// and trigger (see `controls`).
    pub(super) fn move_panels(
        &mut self,
        grab: Option<(usize, usize)>,
        hands: &[Hand; 2],
        anchor: Mat4,
        buzz: &mut Vec<(usize, f32, i64)>,
    ) {
        use crate::controls::{GRIP, angles, wrap};
        // The adjust panel sits beside the control bar, turned with the
        // picture; the library panel and the overlay are placed in the room.
        let room = anchor.to_scale_rotation_translation().1;
        let rotation = |m: Mat4| m.to_scale_rotation_translation().1;
        // Each is moved about what it is placed from: the adjust panel
        // about the control bar, the bar about a flat screen.
        let beside_bar = rotation(self.player_anchor(anchor));
        let flat = self
            .playback
            .as_ref()
            .is_some_and(|p| p.format.projection == Projection::Flat);
        let under = if flat {
            rotation(self.picture_anchor(anchor))
        } else {
            room
        };
        let frame_of = move |panel: usize| match panel {
            ADJUST => beside_bar,
            BAR => under,
            _ => room,
        };
        let grab = grab.and_then(|(panel, hand)| match panel {
            KEYBOARD => self.keyboard_target().map(|t| (t, hand)),
            MAIN | ADJUST | PERF => Some((panel, hand)),
            BAR | TRACKS if self.playback.is_some() => Some((BAR, hand)),
            _ => None,
        });
        if let Some((panel, hand)) = grab
            && let Some((_, rot)) = hands[hand].aim
        {
            let start = match panel {
                MAIN => self.offsets.main,
                ADJUST => self.offsets.adjust,
                PERF => self.offsets.perf,
                _ => self.offsets.bar,
            };
            self.panel_drag = Some(PanelDrag {
                panel,
                hand,
                start: angles(rot, frame_of(panel)),
                from: start,
            });
            buzz.push((hand, 0.3, 14));
        }
        let Some(PanelDrag {
            panel,
            hand,
            start: (y0, p0),
            from: [oy, op],
        }) = self.panel_drag
        else {
            return;
        };
        let h = &hands[hand];
        if h.squeeze > GRIP * 0.6 {
            if let Some((_, rot)) = h.aim {
                let (y, p) = angles(rot, frame_of(panel));
                let yaw = wrap(oy + wrap(y - y0));
                let to = [yaw, (op + p - p0).clamp(-45.0, 45.0)];
                match panel {
                    MAIN => self.offsets.main = to,
                    ADJUST => self.offsets.adjust = to,
                    PERF => self.offsets.perf = to,
                    _ => self.offsets.bar = to,
                }
                self.last_activity = Instant::now();
            }
        } else {
            self.panel_drag = None;
            self.settings.panel_offsets = self.offsets;
        }
    }

    pub fn frame(&mut self, renderer: &mut Renderer, input: FrameInput) -> FrameOutput {
        self.update();
        if input.recentred {
            self.recenter();
        }
        let anchor = self.anchor(&input);
        let eye = input.head.map(|h| h.0).unwrap_or(anchor.w_axis.truncate());
        self.update_visibility();
        let kb_target = self.keyboard_target();
        self.panels[KEYBOARD].visible = kb_target.is_some();
        self.place_panels(anchor, kb_target);
        let buzz = self.route_input(&input, anchor);
        self.paint_panels(renderer, &input, kb_target);
        self.perf_overlay(renderer, &input);
        let (frame, video, passthrough) = self.video(renderer, &input, anchor);
        let (quads, layers) = self.compose(&input, eye);
        FrameOutput {
            frame,
            video,
            quads,
            layers,
            passthrough,
            buzz,
            quit: self.quit,
            restart: self.restart,
        }
    }

    /// Background work, remote commands and saving the open video's state.
    fn update(&mut self) {
        self.poll_jobs();
        // Up next plays on with Autoplay; Loop video keeps to the one
        // playing.
        if self
            .playback
            .as_ref()
            .is_some_and(|p| p.player.state() == PlayerState::Ended)
        {
            if self.settings.loop_video {
                self.play_again();
            } else if let Some(i) = self
                .queue
                .up_next(self.settings.autoplay, self.settings.loop_playlist)
            {
                self.play_queued(i);
            }
        }
        self.give_up_slow_open();
        self.services
            .set_worker_busy(self.playback.is_some() || self.ui.opening.is_some());
        self.sync_locks();
        self.check_mounts();
        self.remote_events();
        if let Some(p) = &mut self.playback {
            p.save_progress(&self.services.library, false);
            p.sync_watch();
            p.save_key(&self.services.library, false);
            p.watch_decoding(self.settings.hardware_decoding);
        }
        self.publish_status();
        self.publish_details();
    }

    /// The room's frame of reference for the panels and the video.
    fn anchor(&mut self, input: &FrameInput) -> Mat4 {
        if let Some((pos, rot)) = input.head {
            if self.reanchor || self.anchor.is_none() {
                // Level at start; a recenter faces where you look.
                self.anchor = Some(match self.anchor {
                    Some(_) => anchor_facing(pos, rot),
                    None => anchor_from_head(pos, rot),
                });
                self.reanchor = false;
            } else if let Some(anchor) = self.anchor
                && self.follows()
            {
                self.anchor = Some(follow(anchor, (pos, rot), input.dt));
            }
        }
        let mut anchor = self.anchor.unwrap_or(Mat4::IDENTITY);
        // A 180°/360° video is drawn from wherever the head is (it was
        // filmed from one spot), so it moves along when you lean; panels
        // fixed in the room then seem to slide against it. During such a
        // video the panels keep to the head's position (not its turning),
        // so they stay put relative to the picture. It never rolls, even
        // when following turned the anchor on its side.
        if let (Some((pos, _)), Some(p)) = (input.head, &self.playback)
            && p.format.projection != Projection::Flat
        {
            let (_, rot, _) = anchor.to_scale_rotation_translation();
            anchor = anchor_facing(pos, rot);
        }
        anchor
    }

    /// Whether the flat screen and panels follow the head: the setting is
    /// on, the video (if any) is flat (around a 180°/360° one you look
    /// about), and no panel is being moved.
    fn follows(&self) -> bool {
        self.settings.follow
            && self.panel_drag.is_none()
            && self
                .playback
                .as_ref()
                .is_none_or(|p| p.format.projection == Projection::Flat)
    }

    /// The picture: what its yaw and pitch turn (by dragging it, or the
    /// Position sliders). A flat screen and the subtitles are placed from
    /// it; a 180°/360° picture turns the same way. A flat screen starts
    /// upright, straight ahead at eye height.
    fn picture_anchor(&self, anchor: Mat4) -> Mat4 {
        match &self.playback {
            Some(p) => {
                let s = p.shown_settings(&self.settings.default_view);
                turned_with_picture(anchor, s.yaw, s.pitch)
            }
            None => anchor,
        }
    }

    /// The player, which the control bar is placed from (see
    /// [`player_frame`]).
    fn player_anchor(&self, anchor: Mat4) -> Mat4 {
        match &self.playback {
            Some(p) => {
                let s = p.shown_settings(&self.settings.default_view);
                let flat = p.format.projection == Projection::Flat;
                player_frame(anchor, flat, [s.yaw, s.pitch], self.offsets.bar)
            }
            None => player_frame(anchor, false, [0.0; 2], self.offsets.bar),
        }
    }

    /// The performance overlay while Settings › General turns it on, with new
    /// readings once a second. It paints what it last read, so painting for
    /// any other reason can't change a number.
    fn perf_overlay(&mut self, renderer: &mut Renderer, input: &FrameInput) {
        if !self.settings.perf_overlay {
            self.perf.visible = false;
            self.perf_monitor = None;
            self.perf_readings = None;
            return;
        }
        let sample = self
            .perf_monitor
            .get_or_insert_with(crate::perf::Monitor::start)
            .latest();
        let readings = self.perf_readings.get_or_insert_with(Default::default);
        readings.frame(input.frame_ms, input.display_hz);
        let now = Instant::now();
        if readings.note_due(now) {
            let open = self
                .playback
                .as_ref()
                .map(|p| (p, p.player.stats(), p.player.net()));
            let note = open.as_ref().map(|(p, st, net)| crate::perf::VideoNote {
                serial: st.serial,
                playing: p.player.state() == PlayerState::Playing,
                shown: st.frames_shown,
                stutter: st.frames_dropped + st.stalls,
                net: net.as_ref().map(|n| {
                    (
                        n.bytes.load(std::sync::atomic::Ordering::Relaxed),
                        n.retries.load(std::sync::atomic::Ordering::Relaxed),
                    )
                }),
            });
            if readings.note(now, note) {
                let hardware = self.settings.hardware_decoding;
                self.perf_shown = ui::perf::Overlay {
                    drawn: readings.drawn(),
                    display_hz: input.display_hz,
                    frame_ms: readings.frame_ms(),
                    video: open.as_ref().and_then(|(p, st, net)| {
                        perf_video(p, st, net.is_some(), readings, hardware)
                    }),
                    sample,
                };
                self.perf.request_repaint();
            }
        }
        self.perf.visible = true;
        if !self.perf.needs_paint() {
            return;
        }
        let shown = &self.perf_shown;
        let mut card = None;
        let painted = self.perf.paint(renderer, input.time, |ctx| {
            card = Some(ui::perf::overlay(ctx, shown));
        });
        if let Err(e) = painted {
            log::error!("performance overlay: {e}");
        }
        self.perf.solid = card;
    }

    /// Which panels show: the controls hide on their own while a video
    /// plays.
    fn update_visibility(&mut self) {
        let playing = self.playback.is_some();
        let paused = self
            .playback
            .as_ref()
            .is_none_or(|p| p.player.is_paused() || p.player.state() != PlayerState::Playing);
        if playing
            && self.ui_visible
            && !paused
            && !self.ui.adjust_open
            && !self.ui.tracks_open
            && !self.show_browser
            && self.last_activity.elapsed().as_secs_f32() > self.settings.auto_hide_secs
        {
            self.ui_visible = false;
        }
        let browser = !playing || (self.ui_visible && self.show_browser);
        self.panels[MAIN].visible = browser;
        // Said where it can be acted on, on the library panel.
        if browser && let Some(notice) = self.update_notice.take() {
            self.ui.toast(notice);
        }
        self.panels[BAR].visible = playing && self.ui_visible && !self.show_browser;
        self.panels[ADJUST].visible = self.panels[BAR].visible && self.ui.adjust_open;
        self.panels[TRACKS].visible = self.panels[BAR].visible && self.ui.tracks_open;
        let over_video = playing && !browser;
        let live = self
            .ui
            .toast
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed().as_secs_f32() < VIDEO_TOAST_SECS);
        self.panels[TOAST].visible = over_video && live;
        // Said and gone: it doesn't come back when the library opens.
        if over_video && !live {
            self.ui.toast = None;
        }
    }

    /// Where each panel goes: the control bar from the player (see
    /// `player_anchor`), the audio and subtitles picker on top of it, and
    /// the adjust panel at its own place beside the bar, upright, going
    /// wherever the bar goes.
    fn place_panels(&mut self, anchor: Mat4, kb_target: Option<usize>) {
        let bar_anchor = self.player_anchor(anchor);
        // The adjust panel holds still while the trigger is held on it: its
        // yaw and pitch sliders move a flat screen, and with it the bar and
        // this panel, away from the pointer. It catches up when let go.
        let held = self.pointer.pressed() == Some(ADJUST);
        let adjust_anchor = match self.adjust_hold.filter(|_| held) {
            Some(hold) => hold,
            None => {
                self.adjust_hold = held.then_some(bar_anchor);
                bar_anchor
            }
        };
        // To the right, clear of the control bar's end as it grows with the
        // interface size.
        let half_angle = |w: f32| (w / 2.0 / 1.05).atan().to_degrees();
        let bar = &self.panels[BAR];
        let beside = -52.0 - (half_angle(bar.size.x) - half_angle(bar.base_size().x));
        let [my, mp] = self.offsets.main;
        let [ay, ap] = self.offsets.adjust;
        self.panels[MAIN].pose = place_below(anchor, my, mp, 1.5, BELOW_DEG);
        self.panels[BAR].pose = place(bar_anchor, 0.0, 1.05, -0.32, 22.0);
        self.panels[TRACKS].pose = above_bar(&self.panels[BAR], &self.panels[TRACKS]);
        self.panels[TOAST].pose = toast_pose(
            &self.panels[BAR],
            self.panels[TRACKS]
                .visible
                .then(|| self.panels[TRACKS].size.y),
            &self.panels[TOAST],
        );
        self.panels[ADJUST].pose = place_around(adjust_anchor, beside + ay, ap, 1.15, 0.0);
        // The keyboard sits below the panel it types into, turned with it, so
        // it is where that panel was moved to.
        let (kb_from, [ky, kp]) = match kb_target {
            Some(ADJUST) => (adjust_anchor, [beside + ay, ap]),
            Some(_) => (anchor, [my, mp - BELOW_DEG]),
            None => (anchor, [0.0, 0.0]),
        };
        let kb_anchor = place_around(kb_from, ky, kp, 0.0, 0.0);
        self.panels[KEYBOARD].pose = place(kb_anchor, 0.0, 0.95, -0.55, 35.0);
        // From the same point as the library panel, so it stays in the room or
        // follows along with the menus.
        let start = perf_start_yaw(
            self.panels[MAIN].size.x,
            self.panels[BAR].size.x,
            self.perf.size.x,
        );
        let [py, pp] = self.offsets.perf;
        self.perf.pose = place_around(anchor, start + py, pp, PERF_DISTANCE, 0.0);
    }

    /// Pointer rays to panel events, grips moving panels, and controller
    /// commands; returns the haptic pulses to play.
    fn route_input(&mut self, input: &FrameInput, anchor: Mat4) -> Vec<(usize, f32, i64)> {
        self.pointer.rays_shown = self.any_ui();
        let routed = {
            let [a, b, c, d, e, _toast] = &mut self.panels;
            self.pointer
                .route(&mut [a, b, c, d, e, &mut self.perf], &input.hands, input.dt)
        };
        let mut buzz = Vec::new();
        self.move_panels(routed.grab, &input.hands, anchor, &mut buzz);
        if let Some(hand) = routed.pressed {
            buzz.push((hand, 0.2, 8));
        }
        self.controller(
            input,
            routed.over_ui || self.panel_drag.is_some(),
            routed.click_outside,
            anchor,
            &mut buzz,
        );
        buzz
    }

    /// Paints the panels that changed (and the keyboard), then carries out
    /// what they asked for.
    fn paint_panels(
        &mut self,
        renderer: &mut Renderer,
        input: &FrameInput,
        kb_target: Option<usize>,
    ) {
        let mut actions = Vec::new();
        if self.thumbs.poll(&self.panels[MAIN].ctx) {
            self.panels[MAIN].request_repaint();
        }
        // Paint the keyboard first so a pressed key's text reaches its target
        // panel and paints on the same frame.
        if let Some(target) = kb_target {
            let mut events = Vec::new();
            let mut close = false;
            let shift = &mut self.ui.keyboard_shift;
            let symbols = &mut self.ui.keyboard_symbols;
            if self.panels[KEYBOARD].needs_paint() {
                let r = self.panels[KEYBOARD].paint(renderer, input.time, |ctx| {
                    let (e, c) = ui::keyboard::keyboard(ctx, shift, symbols);
                    events = e;
                    close = c;
                });
                if let Err(e) = r {
                    log::error!("painting the keyboard: {e}");
                }
            }
            for e in events {
                self.panels[target].push(e);
            }
            if close {
                self.panels[target].push(escape());
            }
        }
        let devices = self.services.haptic_statuses();
        let passthrough_available = input.passthrough_available;
        for idx in [MAIN, BAR, ADJUST, TRACKS, TOAST] {
            if !self.panels[idx].needs_paint() {
                continue;
            }
            let App {
                panels,
                ui: state,
                settings,
                services,
                thumbs,
                playback,
                queue,
                ..
            } = self;
            let mut view = View {
                state,
                queue,
                settings,
                services,
                thumbs,
                playback: playback.as_mut(),
                actions: &mut actions,
                passthrough_available,
                devices: &devices,
            };
            let r = panels[idx].paint(renderer, input.time, |ctx| match idx {
                MAIN => ui::library::browser(ctx, &mut view),
                BAR => ui::player::control_bar(ctx, &mut view),
                TRACKS => ui::player::tracks_panel(ctx, &mut view),
                TOAST => ui::player::toast_panel(ctx, &mut view),
                _ => ui::player::adjust_panel(ctx, &mut view),
            });
            if let Err(e) = r {
                log::error!("painting panel: {e}");
            }
        }
        for a in actions {
            self.apply(a);
        }
        self.sync_settings();
    }

    /// The video frame and how to draw it, and whether the room shows.
    fn video(
        &mut self,
        renderer: &mut Renderer,
        input: &FrameInput,
        anchor: Mat4,
    ) -> (Option<Arc<VideoFrame>>, VideoParams, bool) {
        let shown = self
            .playback
            .as_ref()
            .map(|p| p.shown_settings(&self.settings.default_view));
        // The open video's colour, else the one videos start with. It is
        // sRGB; the eyes are cleared in linear light.
        let [r, g, b] = shown
            .unwrap_or(self.settings.default_view)
            .background
            .map(|c| egui::ecolor::linear_from_gamma(c.clamp(0.0, 1.0)));
        let mut video = VideoParams {
            background: [r, g, b, 1.0],
            ..VideoParams::default()
        };
        let mut frame = None;
        let flat_or_none = self
            .playback
            .as_ref()
            .is_none_or(|p| p.format.projection == Projection::Flat);
        // A chroma-keyed or alpha-packed video shows the room where its
        // background was, whatever its projection and whether or not
        // passthrough is on.
        let keyed = shown.is_some_and(|s| s.chroma_key)
            || self
                .playback
                .as_ref()
                .is_some_and(|p| p.format.alpha_pack_scale().is_some());
        let passthrough =
            input.passthrough_available && ((self.settings.passthrough && flat_or_none) || keyed);
        if passthrough {
            video.background = [0.0; 4];
        }
        let anchor_rot = anchor.to_scale_rotation_translation().1;
        let player = self.picture_anchor(anchor);
        if let (Some(p), Some(s)) = (&self.playback, shown) {
            frame = p.player.current_frame();
            video.format = p.format;
            video.screen_pose = place(player, 0.0, s.screen_distance, 0.0, 0.0);
            video.anchor = anchor_rot;
            video.settings = s;
            if let Some(v) = p.player.info().video_stream() {
                video.rotation = v.rotation;
                video.pixel_aspect = v.pixel_aspect as f32;
            }
            if let Some((_, rot)) = input.head {
                let rel = anchor_rot.inverse() * rot;
                p.player.set_head_orientation(rel.to_array());
            }
            self.subtitles(renderer, input.time, player, &s);
        } else {
            self.subs.visible = false;
        }
        (frame, video, passthrough)
    }

    /// What the compositor draws: panels back to front, then pointers,
    /// as quads in the eyes or (when it can) layers of their own.
    fn compose(&mut self, input: &FrameInput, eye: Vec3) -> (Vec<QuadDraw>, Vec<LayerDraw>) {
        let mut quads = Vec::new();
        // Subtitles fade in like the panels (a panel that never fades in is
        // never drawn).
        self.subs.update_fade(input.dt);
        if self.subs.visible
            && let Some(q) = self.subs.quad()
        {
            if self.subs_split {
                // Each eye sees its half of the panel, as with a 3D video.
                quads.push(QuadDraw {
                    uv: [Vec2::ZERO, Vec2::new(0.5, 1.0)],
                    eye: Some(0),
                    ..q
                });
                quads.push(QuadDraw {
                    uv: [Vec2::new(0.5, 0.0), Vec2::ONE],
                    eye: Some(1),
                    ..q
                });
            } else {
                quads.push(q);
            }
        }
        for p in self.panels.iter_mut().chain([&mut self.perf]) {
            p.update_fade(input.dt);
        }
        let mut order: Vec<(u64, &Panel)> = self
            .panels
            .iter()
            .enumerate()
            .map(|(i, p)| (i as u64, p))
            .chain([(PERF_LAYER, &self.perf)])
            .filter(|(_, p)| p.shown())
            .collect();
        let distance = |p: &Panel| (p.pose.w_axis.truncate() - eye).length();
        order.sort_by(|(_, a), (_, b)| distance(b).total_cmp(&distance(a)));
        let mut layers = Vec::new();
        for (key, p) in order {
            match p.id {
                // Fully shown: the compositor draws it. Fading: drawn into
                // the eyes, where opacity works.
                Some(panel) if input.layers && p.fully_shown() => layers.push(LayerDraw::Panel {
                    key,
                    panel,
                    px: p.px,
                    pose: p.pose,
                    size: p.size,
                }),
                _ => quads.extend(p.quad()),
            }
        }
        let any_ui = self.any_ui();
        if input.layers {
            layers.extend(self.pointer.ray_layers(eye, any_ui));
        } else {
            quads.extend(self.pointer.quads(eye, any_ui));
        }
        (quads, layers)
    }

    /// A panel that the rays are drawn for is up. The toast and the
    /// performance overlay don't count: they stay up while you watch.
    fn any_ui(&self) -> bool {
        self.panels
            .iter()
            .enumerate()
            .any(|(i, p)| i != TOAST && p.visible)
    }

    pub(super) fn subtitles(
        &mut self,
        renderer: &mut Renderer,
        time: f64,
        anchor: Mat4,
        s: &fp_core::view::ViewSettings,
    ) {
        if self.sub_shaper.is_none() {
            self.sub_shaper = self.sub_fonts.poll().pop();
        }
        let Some(p) = &self.playback else { return };
        let cues = p.player.subtitles();
        let pictures = cues.iter().any(|c| !c.bitmaps.is_empty());
        let video = p.player.info().video_stream();
        let pixel_aspect = video.map_or(1.0, |v| v.pixel_aspect as f32);
        let stereo = video
            .filter(|_| pictures)
            .and_then(|v| crate::subtitles::StereoSubs::of(v, &p.format, s.swap_eyes));
        self.subs_split = stereo.is_some();
        let key: String = cues
            .iter()
            .map(|c| format!("{}|{}|{};", c.start, c.text, c.bitmaps.len()))
            .chain(stereo.map(|s| format!("{s:?}")))
            .collect();
        self.subs.visible = !cues.is_empty();
        let d = s.subtitle_distance.clamp(0.5, 20.0);
        let w = 0.9 * d;
        self.subs.size = Vec2::new(w, w * 320.0 / 1600.0);
        self.subs.pose = place(anchor, 0.0, d, -0.2 * d, 0.0);
        if key != self.sub_key && self.subs.visible {
            self.sub_key = key;
            let textures = &mut self.sub_textures;
            let shaper = &mut self.sub_shaper;
            if let Err(e) = self.subs.paint(renderer, time, |ctx| {
                ui::player::subtitles(
                    ctx,
                    &cues,
                    textures,
                    shaper.as_mut(),
                    stereo.as_ref(),
                    pixel_aspect,
                );
            }) {
                log::error!("subtitles: {e}");
            }
        }
    }
}

/// The Escape key, which closes the focused text field's keyboard.
pub(super) fn escape() -> egui::Event {
    key(egui::Key::Escape)
}

/// A key press.
pub(super) fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// Where the audio and subtitles picker goes: on the control bar's plane,
/// right on top of it, its right edge over the end of what the bar holds.
/// Its bottom edge meets the top of the bar's body, so neither panel covers
/// the other's controls.
fn above_bar(bar: &Panel, picker: &Panel) -> Mat4 {
    let room = ui::widgets::SHADOW_ROOM;
    // Metres per point of each.
    let b = bar.size.x / bar.points().x;
    let p = picker.size.x / picker.points().x;
    let top = bar.size.y / 2.0 - room.top as f32 * b;
    let right = bar.size.x / 2.0 - (room.right as f32 + ui::player::BAR_MARGIN as f32) * b;
    let x = right + room.right as f32 * p - picker.size.x / 2.0;
    let y = top + picker.size.y / 2.0;
    bar.pose * Mat4::from_translation(Vec3::new(x, y, 0.0))
}

/// Centred above the control bar's body, and above the picker, `above` metres
/// tall, when it is open.
fn toast_pose(bar: &Panel, above: Option<f32>, toast: &Panel) -> Mat4 {
    let room = ui::widgets::SHADOW_ROOM.top as f32 * bar.size.x / bar.points().x;
    let y = bar.size.y / 2.0 - room + above.unwrap_or(0.0) + toast.size.y / 2.0;
    bar.pose * Mat4::from_translation(Vec3::new(0.0, y, 0.0))
}

/// Degrees left of forward where the performance overlay starts: just clear of
/// the library panel and the control bar, given their widths and its own.
fn perf_start_yaw(library: f32, bar: f32, perf: f32) -> f32 {
    let half = |width: f32, distance: f32| (width / 2.0 / distance).atan().to_degrees();
    half(library, 1.5).max(half(bar, 1.05)) + 2.0 + half(perf, PERF_DISTANCE)
}

/// The overlay's rows for the open video; none without a picture.
fn perf_video(
    p: &Playback,
    st: &fp_media::Stats,
    on_network: bool,
    readings: &crate::perf::Readings,
    hardware_decoding: bool,
) -> Option<ui::perf::Video> {
    use ui::perf::VideoState;
    let v = p.player.info().video_stream()?;
    let (decoder, _) = ui::player::stream_info_labels(v, st, hardware_decoding);
    let state = match p.player.state() {
        PlayerState::Playing => VideoState::Playing(readings.video_fps()),
        PlayerState::Paused => VideoState::Paused,
        PlayerState::Buffering => VideoState::Buffering,
        PlayerState::Ended | PlayerState::Failed => VideoState::Ended,
    };
    let mb_s = readings.mb_per_s();
    Some(ui::perf::Video {
        state,
        film_fps: v.fps as f32,
        dropped: st.frames_dropped,
        stalls: st.stalls,
        stuttered: readings.stuttered(),
        decoder,
        buffer: (st.read_ahead.0 as f32, st.read_ahead.1 as f32),
        read_to_end: st.read_to_end,
        network: on_network.then(|| ui::perf::Network {
            mb_s,
            idle: (st.read_enough || st.read_to_end) && mb_s.is_none_or(|m| m < 0.05),
            retries: readings.retries(),
            retried: readings.retried(),
        }),
        last_jump: st.last_jump.map(|j| ui::perf::LastJump {
            skip: j.skip,
            picture_s: j.picture.as_secs_f32(),
            first_read_s: j.first_read.map(|d| d.as_secs_f32()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toast_sits_above_the_bar_and_any_picker() {
        let mut bar = growing_panel(BAR_PX, BAR_PX[0] as f32 * BAR_M_PER_PX, 1.25);
        let mut toast = growing_panel([1100, 90], 1100.0 * BAR_M_PER_PX, 1.25);
        bar.pose = place(Mat4::IDENTITY, 0.0, 1.05, -0.32, 22.0);
        let height = |pose: Mat4| bar.pose.inverse().transform_point3(pose.w_axis.truncate());
        toast.pose = toast_pose(&bar, None, &toast);
        let alone = height(toast.pose);
        assert!(alone.x.abs() < 1e-5, "centred: {alone}");
        assert!(
            alone.y - toast.size.y / 2.0 > 0.0,
            "clear of the bar's middle"
        );
        let with_picker = height(toast_pose(&bar, Some(0.5), &toast));
        assert!((with_picker.y - alone.y - 0.5).abs() < 1e-5);
    }

    #[test]
    fn the_performance_overlay_starts_in_view_and_clear_of_the_other_panels() {
        let half = |width: f32, distance: f32| (width / 2.0 / distance).atan().to_degrees();
        let library = floating_panel([1600, 1000], 1.8, 1.25);
        for scale in [0.8, 1.0, 1.5] {
            let mut bar = growing_panel(BAR_PX, BAR_PX[0] as f32 * BAR_M_PER_PX, 1.25);
            let mut perf = perf_panel();
            bar.set_scale(scale);
            perf.set_scale(scale);
            let yaw = perf_start_yaw(library.size.x, bar.size.x, perf.size.x);
            let inner = yaw - half(perf.size.x, PERF_DISTANCE);
            assert!(inner > half(library.size.x, 1.5), "{scale}: {inner}");
            assert!(inner > half(bar.size.x, 1.05), "{scale}: {inner}");
            if scale == 1.0 {
                let outer = yaw + half(perf.size.x, PERF_DISTANCE);
                assert!(outer < 55.0, "{outer}° is beyond the edge of the view");
            }
        }
    }

    #[test]
    fn the_picker_sits_on_the_bar_without_covering_it() {
        for scale in [1.0, 1.4] {
            let mut bar = growing_panel(BAR_PX, BAR_PX[0] as f32 * BAR_M_PER_PX, 1.25);
            let mut picker = growing_panel([850, 560], 850.0 * BAR_M_PER_PX, 1.25);
            bar.set_scale(scale);
            picker.set_scale(scale);
            bar.pose = place(Mat4::IDENTITY, 0.0, 1.05, -0.32, 22.0);
            picker.pose = above_bar(&bar, &picker);
            let room = ui::widgets::SHADOW_ROOM;
            let pts = bar.points();
            let bar_top = bar.world_point(egui::pos2(pts.x / 2.0, room.top as f32));
            let picker_pts = picker.points();
            let picker_bottom = picker.world_point(egui::pos2(picker_pts.x / 2.0, picker_pts.y));
            // In the same plane, the picker's bottom edge on the bar's body's
            // top one, and its body's right end over the end of the bar's
            // content.
            let axis = |v: Vec3| bar.pose.transform_vector3(v);
            assert!(axis(Vec3::Z).abs_diff_eq(picker.pose.transform_vector3(Vec3::Z), 1e-5));
            assert!((bar_top - picker_bottom).dot(axis(Vec3::Z)).abs() < 1e-5);
            let gap = (picker_bottom - bar_top).dot(axis(Vec3::Y));
            assert!(gap.abs() < 1e-4, "{scale}: {gap}");
            let margin = room.right as f32 + ui::player::BAR_MARGIN as f32;
            let content_end = bar.world_point(egui::pos2(pts.x - margin, pts.y / 2.0));
            let body_end = picker.world_point(egui::pos2(
                picker_pts.x - room.right as f32,
                picker_pts.y / 2.0,
            ));
            let off = (content_end - body_end).dot(axis(Vec3::X));
            assert!(off.abs() < 1e-4, "{scale}: {off}");
        }
    }
}
