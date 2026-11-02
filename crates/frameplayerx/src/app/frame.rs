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

    /// A flat screen, the control bar, the adjust panel and the browse panel
    /// back where they start, wherever they were moved. A 180°/360°
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

    /// Squeezing a grip on the library panel, the control bar or the
    /// adjust panel moves it round the viewer with that hand; where it is
    /// let go is saved: the bar's relative to a flat screen, the adjust
    /// panel's relative to the bar, so each goes along when what it is
    /// placed from moves. Squeezing one on the keyboard moves the panel it
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
        // picture; the library panel is placed in the room.
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
            MAIN | ADJUST => Some((panel, hand)),
            BAR | TRACKS if self.playback.is_some() => Some((BAR, hand)),
            _ => None,
        });
        if let Some((panel, hand)) = grab
            && let Some((_, rot)) = hands[hand].aim
        {
            let start = match panel {
                MAIN => self.offsets.main,
                ADJUST => self.offsets.adjust,
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
        // Play all, a playlist and what was queued play through, any list
        // with Autoplay; Loop video keeps to the one playing.
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

    /// The performance overlay while Settings › General turns it on: low and
    /// to the left in the view, repainted twice a second.
    fn perf_overlay(&mut self, renderer: &mut Renderer, input: &FrameInput) {
        if !self.settings.perf_overlay {
            self.perf.visible = false;
            self.perf_monitor = None;
            self.frame_times.clear();
            self.perf_video = None;
            return;
        }
        let sample = self
            .perf_monitor
            .get_or_insert_with(crate::perf::Monitor::start)
            .latest();
        if let Some(ms) = input.frame_ms {
            self.frame_times.push_back(ms);
            let second = input
                .display_hz
                .map_or(90, |hz| hz.round().max(1.0) as usize);
            while self.frame_times.len() > second {
                self.frame_times.pop_front();
            }
        }
        if let Some((pos, rot)) = input.head {
            self.perf.pose =
                Mat4::from_rotation_translation(rot, pos) * Mat4::from_translation(PERF_OFFSET);
        }
        self.perf.visible = true;
        if !self.perf.needs_paint() {
            return;
        }
        let frame_ms = (!self.frame_times.is_empty()).then(|| {
            let mut t: Vec<f32> = self.frame_times.iter().copied().collect();
            t.sort_by(f32::total_cmp);
            (t[t.len() / 2], t[t.len() - 1])
        });
        let video = self.playback.as_ref().and_then(|p| {
            let st = p.player.stats();
            let now = Instant::now();
            let last = self
                .perf_video
                .replace((now, st.frames_shown, st.frames_dropped));
            let (at, shown, dropped) = last?;
            let s = now.duration_since(at).as_secs_f32().max(1e-3);
            let v = p.player.info().video_stream()?;
            let (badge, _) =
                ui::player::stream_info_labels(v, &st, self.settings.hardware_decoding);
            Some((
                st.frames_shown.saturating_sub(shown) as f32 / s,
                st.frames_dropped.saturating_sub(dropped) as f32 / s,
                badge,
            ))
        });
        if self.playback.is_none() {
            self.perf_video = None;
        }
        let overlay = ui::perf::Overlay {
            frame_ms,
            display_hz: input.display_hz,
            video,
            sample,
        };
        if let Err(e) = self
            .perf
            .paint(renderer, input.time, |ctx| ui::perf::overlay(ctx, &overlay))
        {
            log::error!("performance overlay: {e}");
        }
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
        // Said where it is seen: the toast shows on the library panel.
        if browser && let Some(notice) = self.update_notice.take() {
            self.ui.toast(notice);
        }
        self.panels[BAR].visible = playing && self.ui_visible && !self.show_browser;
        self.panels[ADJUST].visible = self.panels[BAR].visible && self.ui.adjust_open;
        self.panels[TRACKS].visible = self.panels[BAR].visible && self.ui.tracks_open;
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
    }

    /// Pointer rays to panel events, grips moving panels, and controller
    /// commands; returns the haptic pulses to play.
    fn route_input(&mut self, input: &FrameInput, anchor: Mat4) -> Vec<(usize, f32, i64)> {
        let routed = {
            let [a, b, c, d, e] = &mut self.panels;
            self.pointer
                .route(&mut [a, b, c, d, e], &input.hands, input.dt)
        };
        let mut buzz = Vec::new();
        self.move_panels(routed.grab, &input.hands, anchor, &mut buzz);
        if let Some(hand) = routed.pressed {
            buzz.push((hand, 0.2, 8));
        }
        self.controller(
            input,
            routed.over_ui,
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
        for idx in [MAIN, BAR, ADJUST, TRACKS] {
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
        for p in self.panels.iter_mut() {
            p.update_fade(input.dt);
        }
        let mut order: Vec<usize> = (0..self.panels.len())
            .filter(|&i| self.panels[i].shown())
            .collect();
        order.sort_by(|&a, &b| {
            let da = (self.panels[a].pose.w_axis.truncate() - eye).length();
            let db = (self.panels[b].pose.w_axis.truncate() - eye).length();
            db.total_cmp(&da)
        });
        let mut layers = Vec::new();
        for i in order {
            let p = &self.panels[i];
            match p.id {
                // Fully shown: the compositor draws it. Fading: drawn into
                // the eyes, where opacity works.
                Some(panel) if input.layers && p.fully_shown() => layers.push(LayerDraw::Panel {
                    key: i as u64,
                    panel,
                    px: p.px,
                    pose: p.pose,
                    size: p.size,
                }),
                _ => quads.extend(p.quad()),
            }
        }
        // The performance overlay rides with the head, nearest of all.
        self.perf.update_fade(input.dt);
        if self.perf.shown() {
            match self.perf.id {
                Some(panel) if input.layers && self.perf.fully_shown() => {
                    layers.push(LayerDraw::Panel {
                        key: PERF_LAYER,
                        panel,
                        px: self.perf.px,
                        pose: self.perf.pose,
                        size: self.perf.size,
                    });
                }
                _ => quads.extend(self.perf.quad()),
            }
        }
        let any_ui = self.panels.iter().any(|p| p.visible);
        if input.layers {
            layers.extend(self.pointer.ray_layers(eye, any_ui));
        } else {
            quads.extend(self.pointer.quads(eye, any_ui));
        }
        (quads, layers)
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
            .filter(|_| pictures && p.format.stereo.is_stereo())
            .map(|v| crate::subtitles::StereoSubs {
                layout: p.format.stereo,
                canvas: (v.width, v.height),
                swap_eyes: p.format.eyes_swapped ^ s.swap_eyes,
            });
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

#[cfg(test)]
mod tests {
    use super::*;

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
