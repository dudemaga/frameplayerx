//! Controller commands that are not pointing at a panel.

use super::*;
use crate::controls::{Cmd, Context};

impl App {
    /// DeoVR-style controller bindings (see controls.rs).
    pub(super) fn controller(
        &mut self,
        input: &FrameInput,
        over_ui: bool,
        click_outside: bool,
        anchor: Mat4,
        buzz: &mut Vec<(usize, f32, i64)>,
    ) {
        let h = input.hands;
        let playing = self.playback.is_some();
        let anchor_rot = anchor.to_scale_rotation_translation().1;
        let cmds = self.controls.update(
            &h,
            anchor_rot,
            Context {
                over_ui,
                click_outside,
                playing,
                dt: input.dt,
            },
            &self.settings.controls,
        );
        self.track_activity(&h, over_ui, !cmds.is_empty());
        for c in cmds {
            self.run_cmd(c, input, buzz);
        }
    }

    /// Whether the user is doing something (which keeps the controls up).
    fn track_activity(&mut self, h: &[Hand; 2], over_ui: bool, commanded: bool) {
        // Pointing at a panel only counts as activity while the ray moves
        // or a trigger or grip is pulled: a hand resting with its ray on the
        // control bar must not keep it up forever.
        let mut moved = false;
        for (i, hand) in h.iter().enumerate() {
            if let Some((_, rot)) = hand.aim {
                let dir = rot * Vec3::NEG_Z;
                match self.activity_aim[i] {
                    Some(prev) if prev.angle_between(dir) < 4f32.to_radians() => {}
                    _ => {
                        moved |= self.activity_aim[i].is_some();
                        self.activity_aim[i] = Some(dir);
                    }
                }
            }
        }
        let pulling = h.iter().any(|h| h.trigger > 0.3 || h.squeeze > 0.5);
        if (over_ui && (moved || pulling)) || commanded {
            self.last_activity = Instant::now();
        }
    }

    /// Carries out one controller command.
    fn run_cmd(&mut self, c: Cmd, input: &FrameInput, buzz: &mut Vec<(usize, f32, i64)>) {
        let playing = self.playback.is_some();
        match c {
            Cmd::TogglePause => self.apply(Action::TogglePause),
            Cmd::Back => self.back(),
            Cmd::Menu => {
                if playing {
                    self.show_browser = !self.show_browser;
                    self.ui_visible = true;
                } else {
                    self.ui_visible = true;
                }
            }
            Cmd::Seek(dir) => {
                self.apply(Action::SeekRelative(self.settings.seek_step * dir as f64));
                buzz.push((self.pointer.active, 0.15, 10));
            }
            Cmd::Skip(dir) => {
                self.apply(Action::SeekRelative(60.0 * dir as f64));
                self.ui.toast(if dir > 0 {
                    tr!("toast-plus-minute")
                } else {
                    tr!("toast-minus-minute")
                });
                buzz.push((if dir > 0 { 1 } else { 0 }, 0.2, 14));
            }
            Cmd::Pitch(d) => {
                if let Some(p) = &mut self.playback {
                    p.settings.pitch = (p.settings.pitch + d).clamp(-90.0, 90.0);
                    p.settings_dirty = true;
                }
            }
            Cmd::Yaw(d) => {
                if let Some(p) = &mut self.playback {
                    p.settings.yaw = (p.settings.yaw + d + 180.0).rem_euclid(360.0) - 180.0;
                    p.settings_dirty = true;
                }
            }
            Cmd::Volume(d) => {
                if let Some(p) = &self.playback {
                    let v = (p.player.volume() + d).clamp(0.0, 1.5);
                    self.ui.unmute_volume = None;
                    self.apply(Action::SetVolume(v));
                    let percent = format!("{:.0}", v * 100.0);
                    self.ui.toast(tr!("toast-volume", percent = percent));
                }
            }
            Cmd::ToggleMute => self.toggle_mute(),
            Cmd::TogglePassthrough => {
                if !input.passthrough_available {
                    self.ui.toast(tr!("toast-no-room"));
                } else if let Some(f) = self.playback.as_ref().and_then(Playback::mask_toggled) {
                    self.apply(Action::SetFormat(Some(f)));
                    self.ui.toast(if f.alpha_packed {
                        tr!("toast-room-mask")
                    } else {
                        tr!("toast-room-off")
                    });
                } else {
                    self.apply(Action::TogglePassthrough);
                    let flat_or_none = self
                        .playback
                        .as_ref()
                        .is_none_or(|p| p.format.projection == Projection::Flat);
                    self.ui
                        .toast(match (self.settings.passthrough, flat_or_none) {
                            (false, _) => tr!("toast-room-off"),
                            (true, true) => tr!("toast-room-on"),
                            (true, false) => tr!("toast-room-on-flat"),
                        });
                }
            }
            Cmd::ToggleAdjust => {
                self.ui.adjust_open = !self.ui.adjust_open;
                if self.ui.adjust_open {
                    self.ui_visible = true;
                    self.show_browser = false;
                }
            }
            Cmd::Zoom(d) => {
                if let Some(p) = &mut self.playback {
                    p.settings.zoom = (p.settings.zoom + d).clamp(0.5, 2.5);
                    p.settings_dirty = true;
                }
            }
            Cmd::Height(d) => {
                if let Some(p) = &mut self.playback {
                    p.settings.height = (p.settings.height + d).clamp(-1.0, 1.0);
                    p.settings_dirty = true;
                    let metres = format!("{:+.2}", p.settings.height);
                    self.ui.toast(tr!("toast-height", metres = metres));
                }
            }
            Cmd::Next => self.step_queue(1),
            Cmd::Previous => self.step_queue(-1),
            Cmd::Chapter(dir) => self.step_chapter(dir),
            Cmd::Bookmark => self.apply(Action::AddBookmark),
            Cmd::ToggleSubtitles => self.toggle_subtitles(),
            Cmd::ResetImage => {
                if let Some(p) = &mut self.playback {
                    let d = self.settings.default_view;
                    (
                        p.settings.yaw,
                        p.settings.pitch,
                        p.settings.roll,
                        p.settings.zoom,
                        p.settings.height,
                        p.settings.forward,
                    ) = (d.yaw, d.pitch, d.roll, d.zoom, d.height, d.forward);
                    p.settings_dirty = true;
                    self.ui.toast(tr!("toast-view-reset"));
                }
            }
            Cmd::DragBegin => {
                self.drag_start = self
                    .playback
                    .as_ref()
                    .map(|p| (p.settings.yaw, p.settings.pitch));
                buzz.push((self.pointer.active, 0.25, 12));
            }
            Cmd::DragTo { yaw, pitch } => {
                if let (Some(p), Some((y0, p0))) = (&mut self.playback, self.drag_start) {
                    // The picture follows the hand.
                    p.settings.yaw = y0 + yaw;
                    p.settings.pitch = (p0 + pitch).clamp(-90.0, 90.0);
                    p.settings_dirty = true;
                }
            }
            Cmd::DragEnd => self.drag_start = None,
            Cmd::Recenter => {
                self.recenter();
                buzz.push((0, 0.3, 20));
                buzz.push((1, 0.3, 20));
            }
            Cmd::ToggleUi => {
                self.ui_visible = !self.ui_visible;
                if !self.ui_visible {
                    self.show_browser = false;
                }
            }
            Cmd::Page(dir) => {
                if let Some(i) = self.pointer.hovered() {
                    let p = &mut self.panels[i];
                    if ui::widgets::over_slider(&p.ctx) {
                        p.push(key(if dir > 0 {
                            egui::Key::ArrowRight
                        } else {
                            egui::Key::ArrowLeft
                        }));
                    } else {
                        let page = p.points().y * 0.8;
                        p.push(egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(0.0, -page * dir as f32),
                            modifiers: egui::Modifiers::NONE,
                        });
                    }
                }
            }
        }
    }

    /// Back (B by default), as DeoVR's: closes the innermost thing that is
    /// open.
    /// Mutes, or turns the sound back on at the volume before.
    pub(super) fn toggle_mute(&mut self) {
        let Some(p) = &self.playback else {
            return;
        };
        let vol = p.player.volume();
        if vol <= 0.001 {
            let v = self.ui.unmute_volume.take().unwrap_or(1.0);
            self.apply(Action::SetVolume(v));
            self.ui.toast(tr!("toast-sound-on"));
        } else {
            self.ui.unmute_volume = Some(vol);
            self.apply(Action::SetVolume(0.0));
            self.ui.toast(tr!("toast-muted"));
        }
    }

    pub(super) fn back(&mut self) {
        if self.panels[KEYBOARD].visible {
            if let Some(i) = self.keyboard_target() {
                self.panels[i].push(escape());
            }
            return;
        }
        let playing = self.playback.is_some();
        if playing && self.ui.tracks_open && self.ui_visible && !self.show_browser {
            self.ui.tracks_open = false;
        } else if playing && self.ui.adjust_open && self.ui_visible && !self.show_browser {
            self.ui.adjust_open = false;
        } else if (!playing || self.show_browser) && self.ui.details.is_some() {
            self.ui.details = None;
        } else if (!playing || self.show_browser) && self.ui.source_form.is_some() {
            self.ui.source_form = None;
        } else if (!playing || self.show_browser) && self.ui.pin_form.is_some() {
            self.ui.pin_form = None;
        } else if (!playing || self.show_browser)
            && self.ui.screen == Screen::Sources
            && self.ui.source.is_some()
        {
            // Up a folder, back a page, or out of the source.
            if let Some(a) = self.ui.source_back() {
                self.apply(a);
            }
        } else if playing && self.show_browser {
            // Library open over the video: back to the video.
            self.show_browser = false;
            self.ui_visible = true;
        } else if playing && self.ui_visible {
            // Player controls: back to the library, video keeps playing.
            self.show_browser = true;
            self.ui.invalidate();
        } else if playing {
            self.ui_visible = true;
        } else if self.ui.screen != Screen::Home {
            self.ui.screen = Screen::Home;
            self.ui.invalidate();
        }
        self.last_activity = Instant::now();
    }

    /// Next/previous video in the list the current one was opened from;
    /// round to the other end when Up next loops.
    pub(super) fn step_queue(&mut self, dir: i32) {
        if self.queue.items.len() < 2 {
            self.ui.toast(tr!("player-no-other-videos"));
            return;
        }
        match self.queue.neighbour(dir, self.settings.loop_playlist) {
            Some(i) => self.play_queued(i),
            None => self.ui.toast(if dir > 0 {
                tr!("player-last-video")
            } else {
                tr!("player-first-video")
            }),
        }
    }

    /// Plays the video at `i` in Up next, saying where it is in the list.
    pub(super) fn play_queued(&mut self, i: usize) {
        let Some(req) = self.queue.jump(i).cloned() else {
            return;
        };
        let (at, of) = (i + 1, self.queue.items.len());
        self.ui.toast(tr!("toast-queue-position", at = at, of = of));
        self.open_request(req);
    }

    /// The next or previous chapter or bookmark; within a few seconds of
    /// one's start, back goes to the one before, as players do.
    fn step_chapter(&mut self, dir: i32) {
        let Some(p) = &self.playback else { return };
        if p.markers.is_empty() {
            self.ui.toast(tr!("toast-no-chapters"));
            return;
        }
        let now = p.player.position();
        let to = if dir > 0 {
            p.markers.iter().find(|(t, _)| *t > now + 0.5)
        } else {
            p.markers
                .iter()
                .rev()
                .find(|(t, _)| *t < now - CHAPTER_GRACE)
        };
        match to.cloned() {
            Some((t, name)) => {
                self.apply(Action::Seek(t));
                self.ui.toast(name);
            }
            None if dir < 0 => {
                self.apply(Action::Seek(0.0));
                self.ui.toast(tr!("toast-beginning"));
            }
            None => self.ui.toast(tr!("toast-last-chapter")),
        }
    }

    /// Subtitles off, or back on (see [`crate::subtitles::toggled`]), as a
    /// pick kept with the video.
    fn toggle_subtitles(&mut self) {
        let Some(p) = &mut self.playback else { return };
        let showing = p.subtitles_showing();
        let language = &self.settings.subtitle_language;
        let info = p.player.info();
        let name = fp_sources::urlutil::last_segment(&p.request.location);
        let Some(pick) = crate::subtitles::toggled(
            showing,
            p.last_subtitles,
            language,
            info,
            &p.subtitle_files,
            &name,
        ) else {
            self.ui.toast(tr!("toast-no-subtitles"));
            return;
        };
        let title = crate::subtitles::rows(language, info, &p.subtitle_files)
            .into_iter()
            .find(|r| r.pick == pick)
            .map(|r| r.title);
        if showing != crate::subtitles::Pick::Off {
            p.last_subtitles = Some(showing);
        }
        self.apply(Action::SelectSubtitles(pick));
        self.ui.toast(match title {
            Some(t) => tr!("toast-subtitles", title = t),
            None => tr!("toast-subtitles-off").to_string(),
        });
    }
}

/// Seconds into a chapter within which Previous chapter goes to the one
/// before it rather than its start.
const CHAPTER_GRACE: f64 = 3.0;
