//! Playback UI: the control bar, the adjustments panel and subtitles.

use super::theme::Weight;
use super::widgets::{Kind, ListArea, Tip};
use super::{Action, View, depth, fmt_time, icons, premium, theme, widgets};
use crate::playback::Quality;
use crate::subtitles::Pick;
use egui::{Align, Color32, Layout, RichText, Sense, Vec2};
use fp_core::format::{Projection, StereoLayout, VideoFormat};
use fp_core::view::ViewSettings;
use fp_media::PlayerState;

/// Format presets offered for overrides.
pub fn format_choices() -> Vec<(&'static str, VideoFormat)> {
    use StereoLayout::*;
    let f = |p, s| VideoFormat::new(p, s);
    vec![
        (tr!("format-flat-2d"), f(Projection::Flat, Mono)),
        (
            tr!("format-flat-3d-half-sbs"),
            f(Projection::Flat, HalfSideBySide),
        ),
        (tr!("format-flat-3d-sbs"), f(Projection::Flat, SideBySide)),
        (
            tr!("format-flat-3d-half-tb"),
            f(Projection::Flat, HalfTopBottom),
        ),
        (tr!("format-flat-3d-tb"), f(Projection::Flat, TopBottom)),
        ("180° SBS", f(Projection::EQUIRECT_180, SideBySide)),
        ("180° TB", f(Projection::EQUIRECT_180, TopBottom)),
        ("180° 2D", f(Projection::EQUIRECT_180, Mono)),
        ("360° 2D", f(Projection::EQUIRECT_360, Mono)),
        ("360° TB", f(Projection::EQUIRECT_360, TopBottom)),
        ("360° SBS", f(Projection::EQUIRECT_360, SideBySide)),
        (
            tr!("format-fisheye-190"),
            f(Projection::fisheye(190.0), SideBySide),
        ),
        (
            tr!("format-fisheye-200"),
            f(Projection::fisheye(200.0), SideBySide),
        ),
        (
            tr!("format-fisheye-220"),
            f(Projection::fisheye(220.0), SideBySide),
        ),
        (
            "EAC 360° (YouTube)",
            f(Projection::Eac { h_fov: 360.0 }, Mono),
        ),
        (
            "EAC 360° TB",
            f(Projection::Eac { h_fov: 360.0 }, TopBottom),
        ),
        (
            "EAC 180° SBS",
            f(Projection::Eac { h_fov: 180.0 }, SideBySide),
        ),
    ]
}

/// The floating transport bar: title and format, the seek bar, and three
/// groups of controls (browse and sound left, transport centred, view
/// right).
pub fn control_bar(ctx: &egui::Context, v: &mut View) {
    // The bar floats in its panel: the outer margin leaves room for its
    // shadow on the video.
    widgets::panel_slab(ctx, 28.0);
    let frame = egui::Frame::new()
        .outer_margin(widgets::SHADOW_ROOM)
        .inner_margin(egui::Margin::symmetric(BAR_MARGIN, 18));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        title_row(
            ui,
            pb,
            v.state,
            v.settings.hardware_decoding,
            &v.settings.subtitle_language,
        );
        ui.add_space(6.0);
        seek_row(ui, pb, v.state, v.actions);
        ui.add_space(8.0);

        // Controls: three groups on one row, transport always centred.
        let (row, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::hover());
        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(Layout::left_to_right(Align::Center)),
        );
        widgets::well(&mut left, |ui| {
            browse_and_sound(ui, pb, v.state, v.settings, v.actions);
        });
        transport(ui, row, pb, v.settings, v.queue, v.actions);
        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(Layout::right_to_left(Align::Center)),
        );
        widgets::well(&mut right, |ui| {
            view_keys(
                ui,
                pb,
                v.state,
                v.settings.passthrough,
                v.passthrough_available,
                v.settings.follow,
                v.actions,
            );
        });
    });
}

/// The control bar's side margin; the audio and subtitles picker lines up
/// with the end of what is inside it.
pub const BAR_MARGIN: i8 = 28;

/// The format and stream pills open the adjust panel's Format tab (again
/// closes it).
fn toggle_format_tab(state: &mut super::UiState) {
    let showing = state.adjust_open && state.adjust_tab == 0;
    state.adjust_open = !showing;
    state.adjust_tab = 0;
}

/// Title, status, format, and the audio and subtitles in use.
fn title_row(
    ui: &mut egui::Ui,
    pb: &crate::playback::Playback,
    state: &mut super::UiState,
    hardware_decoding: bool,
    language: &str,
) {
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if format_pill(ui, &widgets::format_short(&pb.format))
                .tip(tr!("player-change-format"))
                .clicked()
            {
                toggle_format_tab(state);
            }
            if let Some(video) = pb.player.info().video_stream() {
                let st = pb.player.stats();
                let (badge_text, tooltip) = stream_info_labels(video, &st, hardware_decoding);
                if stream_pill(ui, &badge_text, st.hardware)
                    .tip(&tooltip)
                    .clicked()
                {
                    toggle_format_tab(state);
                }
            }
            if let Some(label) = tracks_label(pb, language)
                && tracks_pill(ui, &label, state.tracks_open)
                    .tip(tr!("player-tracks"))
                    .clicked()
            {
                state.tracks_open = !state.tracks_open;
            }
            match (pb.player.error(), &pb.notice) {
                (Some(e), _) => {
                    warning_pill(ui, tr!("player-error-pill"), theme::ERROR).tip(&e);
                }
                (None, Some(n)) => {
                    warning_pill(ui, &n.label, theme::WARN).tip(&n.detail);
                }
                (None, None) => {}
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let buffering = pb.player.state() == PlayerState::Buffering;
                let room = ui.available_width() - if buffering { 28.0 } else { 0.0 };
                widgets::title_line(
                    ui,
                    &widgets::display_title(&pb.title),
                    theme::font(Weight::SemiBold, 21.0),
                    theme::TEXT,
                    room,
                );
                // Sized to fit the 30-point row: the default spinner
                // (44) grew the row, so every stutter bounced the seek
                // bar and buttons up and down.
                if buffering {
                    ui.add(egui::Spinner::new().size(20.0));
                }
            });
        });
    });
}

/// The latest toast, on its own panel over the video.
pub fn toast_panel(ctx: &egui::Context, v: &mut View) {
    if let Some((text, _)) = &v.state.toast {
        widgets::status_pill(ctx, egui::Align2::CENTER_CENTER, Vec2::ZERO, |ui| {
            widgets::toast_line(ui, text);
        });
    }
}

/// A warning on the title row: a symbol and a few words, the rest in its tip.
fn warning_pill(ui: &mut egui::Ui, label: &str, color: Color32) -> egui::Response {
    let painter = ui.painter();
    let icon = painter.layout_no_wrap(icons::WARNING.to_string(), theme::icon(16.0), color);
    let text = painter.layout_no_wrap(
        label.to_string(),
        theme::font(Weight::SemiBold, 14.0),
        color,
    );
    const GAP: f32 = 6.0;
    let width = icon.size().x + GAP + text.size().x;
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(width, text.size().y) + Vec2::new(20.0, 10.0),
        Sense::hover(),
    );
    pill_face(ui, &resp, false);
    let left = rect.center().x - width / 2.0;
    let top = |height: f32| rect.center().y - height / 2.0;
    let text_at = egui::pos2(left + icon.size().x + GAP, top(text.size().y));
    let icon_at = egui::pos2(left, top(icon.size().y));
    ui.painter().galley(icon_at, icon, color);
    ui.painter().galley(text_at, text, color);
    resp
}

/// Times either side of the seek bar.
fn seek_row(
    ui: &mut egui::Ui,
    pb: &mut crate::playback::Playback,
    state: &mut super::UiState,
    actions: &mut Vec<Action>,
) {
    let duration = pb.player.duration();
    let position = state.scrub.unwrap_or_else(|| pb.player.position());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        let time = |ui: &mut egui::Ui, t: f64, align: Align| {
            ui.allocate_ui_with_layout(
                Vec2::new(68.0, 36.0),
                Layout::left_to_right(Align::Center).with_main_align(align),
                |ui| {
                    ui.label(
                        RichText::new(fmt_time(t))
                            .font(theme::font(Weight::Medium, 16.0))
                            .color(theme::TEXT_2),
                    )
                },
            );
        };
        time(ui, position, Align::Min);
        let w = ui.available_width() - 68.0 - 14.0;
        seek_bar(ui, w, state, pb, actions);
        time(ui, duration, Align::Max);
    });
}

/// Browse, Up next, Loop video, Autoplay, speed and sound.
fn browse_and_sound(
    ui: &mut egui::Ui,
    pb: &crate::playback::Playback,
    state: &mut super::UiState,
    settings: &crate::settings::Settings,
    actions: &mut Vec<Action>,
) {
    if widgets::key(ui, icons::SQUARES_FOUR, 52.0, false)
        .tip(tr!("player-browse"))
        .clicked()
    {
        actions.push(Action::ShowBrowser(true));
    }
    if widgets::key(ui, icons::QUEUE, 52.0, false)
        .tip(tr!("player-up-next"))
        .clicked()
    {
        actions.push(Action::ShowUpNext);
    }
    if widgets::key(ui, icons::REPEAT_ONCE, 52.0, settings.loop_video)
        .tip(tr!("player-loop-video"))
        .clicked()
    {
        actions.push(Action::ToggleLoopVideo);
    }
    if widgets::key_nested(
        ui,
        icons::ARROW_CLOCKWISE,
        icons::PLAY,
        52.0,
        settings.autoplay,
    )
    .tip(tr!("player-autoplay"))
    .clicked()
    {
        actions.push(Action::ToggleAutoplay);
    }
    let speed = pb.player.speed();
    if widgets::pill_key(ui, &fmt_speed(speed), (speed - 1.0).abs() > 1e-3)
        .tip(tr!("player-speed"))
        .clicked()
    {
        actions.push(Action::SetSpeed(next_speed(speed)));
    }
    let vol = pb.player.volume();
    let speaker = if vol <= 0.001 {
        icons::SPEAKER_X
    } else if vol < 0.5 {
        icons::SPEAKER_LOW
    } else {
        icons::SPEAKER_HIGH
    };
    let key = widgets::key(ui, speaker, 52.0, false);
    if key.clicked() {
        if vol <= 0.001 {
            actions.push(Action::SetVolume(state.unmute_volume.take().unwrap_or(1.0)));
        } else {
            state.unmute_volume = Some(vol);
            actions.push(Action::SetVolume(0.0));
        }
    }
    volume_slider(ui, &key, vol, actions);
}

/// Seconds the volume slider stays after the ray leaves it and the
/// speaker key: time to cross from one to the other.
const VOLUME_LINGER: f64 = 0.4;

/// The volume slider, over the seek bar above the speaker `key`: shown
/// while the key or the slider is pointed at, or the slider is held. The
/// bar isn't redrawn every frame, so where the slider was decides as much
/// as when it was last pointed at.
fn volume_slider(ui: &egui::Ui, key: &egui::Response, vol: f32, actions: &mut Vec<Action>) {
    let ctx = ui.ctx();
    let id = key.id.with("volume");
    let (now, pointer, down) = ctx.input(|i| (i.time, i.pointer.hover_pos(), i.pointer.any_down()));
    let (seen, last, holding) = ctx.data(|d| {
        (
            d.get_temp::<f64>(id),
            d.get_temp::<egui::Rect>(id.with("rect")),
            d.get_temp::<bool>(id.with("held")).unwrap_or(false),
        )
    });
    let over = |r: egui::Rect| pointer.is_some_and(|p| r.expand(10.0).contains(p));
    let lingering = seen.is_some_and(|t| now - t <= VOLUME_LINGER);
    if !(key.hovered() || last.is_some_and(over) || (holding && down) || lingering) {
        // Closed: where it was no longer opens it.
        if last.is_some() {
            ctx.data_mut(|d| d.remove::<egui::Rect>(id.with("rect")));
        }
        return;
    }
    let mut held = false;
    let area = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::CENTER_BOTTOM)
        .fixed_pos(key.rect.center_top() - Vec2::new(0.0, 10.0))
        .constrain(true)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::SURFACE_3)
                .corner_radius(egui::CornerRadius::same(22))
                .inner_margin(egui::Margin::symmetric(14, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut v = vol;
                        let r = widgets::slider(ui, &mut v, 0.0..=1.5, 180.0);
                        if r.changed() {
                            actions.push(Action::SetVolume(v));
                        }
                        held = r.is_pointer_button_down_on();
                        ui.label(
                            RichText::new(format!("{:.0}%", v * 100.0))
                                .font(theme::font(Weight::SemiBold, 15.0))
                                .color(theme::TEXT_2),
                        );
                    });
                });
        });
    let rect = area.response.rect;
    held |= holding && down;
    ctx.data_mut(|d| {
        d.insert_temp(id.with("rect"), rect);
        d.insert_temp(id.with("held"), held);
        if key.hovered() || over(rect) || held {
            d.insert_temp(id, now);
        }
    });
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(VOLUME_LINGER));
}

/// Previous, rewind, play, forward and next, centred on the row.
fn transport(
    ui: &mut egui::Ui,
    row: egui::Rect,
    pb: &crate::playback::Playback,
    settings: &crate::settings::Settings,
    queue: &crate::queue::Queue,
    actions: &mut Vec<Action>,
) {
    let (step, loops) = (settings.seek_step, settings.loop_playlist);
    let paused = pb.player.is_paused() || pb.player.state() == PlayerState::Ended;
    let centre_w = 52.0 + 56.0 + 72.0 + 56.0 + 52.0 + 4.0 * 12.0;
    let centre = egui::Rect::from_center_size(row.center(), Vec2::new(centre_w, row.height()));
    let mut mid = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(centre)
            .layout(Layout::left_to_right(Align::Center)),
    );
    mid.spacing_mut().item_spacing.x = 12.0;
    queue_key(&mut mid, queue, -1, loops, actions);
    if skip_button(&mut mid, -step)
        .tip(tr!("player-rewind"))
        .clicked()
    {
        actions.push(Action::SeekRelative(-step));
    }
    if play_button(&mut mid, paused)
        .tip(if paused {
            tr!("player-play")
        } else {
            tr!("player-pause")
        })
        .clicked()
    {
        actions.push(Action::TogglePause);
    }
    if skip_button(&mut mid, step)
        .tip(tr!("player-forward"))
        .clicked()
    {
        actions.push(Action::SeekRelative(step));
    }
    queue_key(&mut mid, queue, 1, loops, actions);
}

/// The previous (`dir` -1) or next video in Up next, as the controller's
/// X and Y, round the list when it `loops`; dimmed when there is none that
/// way.
fn queue_key(
    ui: &mut egui::Ui,
    queue: &crate::queue::Queue,
    dir: i32,
    loops: bool,
    actions: &mut Vec<Action>,
) {
    let to = queue.neighbour(dir, loops).and_then(|i| queue.items.get(i));
    let (icon, name) = if dir < 0 {
        (icons::SKIP_BACK, tr!("player-previous"))
    } else {
        (icons::SKIP_FORWARD, tr!("player-next"))
    };
    let tip = match to {
        Some(req) => {
            let title = widgets::display_title(&req.shown_title());
            tr!("player-step-to", step = name, title = title)
        }
        None if queue.items.len() < 2 => tr!("player-no-other-videos").into(),
        None if dir < 0 => tr!("player-first-video").into(),
        None => tr!("player-last-video").into(),
    };
    let key = ui
        .add_enabled_ui(to.is_some(), |ui| widgets::key(ui, icon, 52.0, false))
        .inner;
    if key.tip(&tip).clicked() {
        actions.push(Action::StepQueue(dir));
    }
}

/// Close, hide, room, follow, bookmark and adjust.
fn view_keys(
    ui: &mut egui::Ui,
    pb: &crate::playback::Playback,
    state: &mut super::UiState,
    passthrough: bool,
    passthrough_available: bool,
    follow: bool,
    actions: &mut Vec<Action>,
) {
    if widgets::key(ui, icons::X, 52.0, false)
        .tip(tr!("player-close"))
        .clicked()
    {
        actions.push(Action::ClosePlayback);
    }
    if widgets::key(ui, icons::EYE_SLASH, 52.0, false)
        .tip(tr!("player-hide"))
        .clicked()
    {
        actions.push(Action::HideControls);
    }
    // Your room shows around a flat video's screen, or through a
    // side-by-side passthrough video's own mask.
    if passthrough_available {
        if pb.format.projection == Projection::Flat {
            if widgets::key(ui, icons::ARMCHAIR, 52.0, passthrough)
                .tip(tr!("player-room"))
                .clicked()
            {
                actions.push(Action::TogglePassthrough);
            }
        } else if let Some(f) = pb.mask_toggled()
            && widgets::key(ui, icons::ARMCHAIR, 52.0, pb.format.alpha_packed)
                .tip(tr!("player-room-mask"))
                .clicked()
        {
            actions.push(Action::SetFormat(Some(f)));
        }
    }
    // Only a flat screen follows: around 180°/360° video you look about.
    if pb.format.projection == Projection::Flat
        && widgets::key(ui, icons::BED, 52.0, follow)
            .tip(tr!("settings-follow"))
            .clicked()
    {
        actions.push(Action::ToggleFollow);
    }
    if widgets::key(ui, icons::BOOKMARK_SIMPLE, 52.0, false)
        .tip(tr!("player-bookmark"))
        .clicked()
    {
        actions.push(Action::AddBookmark);
    }
    if widgets::key(ui, icons::SLIDERS_HORIZONTAL, 52.0, state.adjust_open)
        .tip(tr!("player-adjust"))
        .clicked()
    {
        state.adjust_open = !state.adjust_open;
    }
}

const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

/// The next speed in [`SPEEDS`], wrapping round.
fn next_speed(speed: f64) -> f64 {
    SPEEDS
        .iter()
        .copied()
        .find(|s| *s > speed + 1e-3)
        .unwrap_or(SPEEDS[0])
}

fn fmt_speed(speed: f64) -> String {
    let s = format!("{speed:.2}");
    format!("{}×", s.trim_end_matches('0').trim_end_matches('.'))
}

/// A small raised pill naming the video's format.
fn format_pill(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(
        text.to_string(),
        theme::font(Weight::SemiBold, 14.0),
        theme::TEXT_2,
    );
    let (rect, resp) = ui.allocate_exact_size(g.size() + Vec2::new(20.0, 10.0), Sense::click());
    let t = pill_face(ui, &resp, false);
    let fg = theme::TEXT_2.lerp_to_gamma(theme::TEXT, t);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp
}

/// The raised body of a pill on the title row, lit while the pointer is
/// on it and blue while what it opens is `open`; returns how lit it is.
fn pill_face(ui: &egui::Ui, resp: &egui::Response, open: bool) -> f32 {
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let s = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("open"), open, 0.12);
    let lit = |a: Color32, b: Color32, on: Color32| a.lerp_to_gamma(b, t).lerp_to_gamma(on, s);
    depth::raised(
        ui.painter(),
        resp.rect,
        8.0,
        depth::Raised {
            top: lit(
                Color32::from_rgb(46, 54, 65),
                Color32::from_rgb(62, 72, 86),
                Color32::from_rgb(31, 76, 114),
            ),
            bottom: lit(
                Color32::from_rgb(33, 40, 49),
                Color32::from_rgb(44, 52, 63),
                Color32::from_rgb(21, 52, 82),
            ),
            light: Color32::from_white_alpha(26),
            lift: 0.3,
            glow: None,
        },
    );
    t
}

/// What the audio and subtitles pill says: the audio track when there is a
/// choice of them, then the subtitles; none when there is nothing to choose.
fn tracks_label(pb: &crate::playback::Playback, language: &str) -> Option<String> {
    let info = pb.player.info();
    let audio: Vec<_> = info.audio_streams().collect();
    let subtitles = crate::subtitles::rows(language, info, &pb.subtitle_files);
    if audio.len() < 2 && subtitles.is_empty() && pb.targets.is_empty() {
        return None;
    }
    let mut label = String::new();
    if !pb.targets.is_empty() {
        let quality = match (&pb.transcoding, &pb.request.quality) {
            (Some(t), _) => t.label.as_str(),
            (None, Quality::Original) => tr!("tracks-quality-original"),
            (None, _) => tr!("tracks-quality-automatic"),
        };
        label = format!("{}  {quality}      ", icons::GAUGE);
    }
    if audio.len() > 1 {
        let playing = pb.player.audio_stream();
        let name = audio.iter().find(|s| Some(s.index) == playing).map_or_else(
            || tr!("tracks-off").to_string(),
            |s| crate::subtitles::track_name(s),
        );
        label += &format!("{}  {name}      ", icons::SPEAKER_HIGH);
    }
    let shown = subtitles
        .iter()
        .find(|r| showing(pb, r.pick))
        .map_or(tr!("tracks-off"), |r| r.title.as_str());
    label += &format!("{}  {shown}   {}", icons::SUBTITLES, icons::CARET_UP);
    Some(label)
}

/// Whether `pick` is the subtitles showing.
fn showing(pb: &crate::playback::Playback, pick: Pick) -> bool {
    pb.subtitles_showing() == pick
}

/// The pill naming the audio and subtitles in use, which opens the picker
/// above the bar.
fn tracks_pill(ui: &mut egui::Ui, label: &str, open: bool) -> egui::Response {
    let g = widgets::one_line(
        ui.painter(),
        label.to_string(),
        theme::font(Weight::SemiBold, 14.0),
        theme::TEXT,
        320.0,
    );
    let (rect, resp) = ui.allocate_exact_size(g.size() + Vec2::new(24.0, 10.0), Sense::click());
    pill_face(ui, &resp, open);
    let fg = if open {
        theme::ACCENT_HOVER
    } else {
        theme::TEXT
    };
    ui.painter()
        .galley_with_override_text_color(rect.center() - g.size() / 2.0, g, fg);
    resp
}

/// Short summary label and detailed tooltip for a video stream.
/// `hardware_on` is the hardware decoding setting.
pub(crate) fn stream_info_labels(
    v: &fp_media::StreamInfo,
    st: &fp_media::Stats,
    hardware_on: bool,
) -> (String, String) {
    let codec = match v.codec.as_str() {
        "hevc" => "HEVC",
        "h264" => "H.264",
        "av1" => "AV1",
        "vp9" => "VP9",
        c => c,
    };
    // 3D Blu-ray: H.264 with a second view, decoded side by side.
    let codec = if v.mvc {
        format!("{codec} MVC")
    } else {
        codec.to_string()
    };
    let hw = if st.hardware { "HW" } else { "SW" };
    let bits = if v.bit_depth > 8 {
        format!(" {}-bit", v.bit_depth)
    } else {
        String::new()
    };
    let badge_text = format!("{hw} · {codec}{bits}");

    let decoder = decoder_text(v, st, hardware_on);
    let chroma = v.chroma();
    let pixel = if v.pixel_format.is_empty() {
        format!("{}-bit {chroma}", v.bit_depth)
    } else {
        format!("{} ({}-bit {chroma})", v.pixel_format, v.bit_depth)
    };
    let size = if v.mvc || v.frame_sequential {
        let (w, h) = (v.width / 2, v.height);
        tr!("player-size-per-eye", width = w, height = h)
    } else {
        format!("{}×{}", v.width, v.height)
    };
    let fps = format!("{:.2}", v.fps);
    let tooltip = tr!(
        "player-stream-tip",
        decoder = decoder,
        format = format!("{codec} · {pixel}"),
        size = size,
        fps = fps
    );
    (badge_text, tooltip)
}

/// The decoder playing the video and, for software, why not hardware.
fn decoder_text(v: &fp_media::StreamInfo, st: &fp_media::Stats, hardware_on: bool) -> String {
    if st.hardware {
        tr!(
            "player-decoder-hardware",
            decoder = st.video_decoder.as_str()
        )
    } else {
        let why = crate::playback::software_reason(v, hardware_on, st.hardware_error.as_deref());
        tr!(
            "player-decoder-software",
            decoder = st.video_decoder.as_str(),
            why = why
        )
    }
}

/// A small raised pill naming the video stream format and decoder (HW / SW).
fn stream_pill(ui: &mut egui::Ui, text: &str, hardware: bool) -> egui::Response {
    let text_color = if hardware {
        theme::TEXT_2
    } else {
        theme::WARN.gamma_multiply(0.9)
    };
    let g = ui.painter().layout_no_wrap(
        text.to_string(),
        theme::font(Weight::SemiBold, 14.0),
        text_color,
    );
    let (rect, resp) = ui.allocate_exact_size(g.size() + Vec2::new(20.0, 10.0), Sense::click());
    let t = pill_face(ui, &resp, false);
    let fg = text_color.lerp_to_gamma(theme::TEXT, t);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp
}

/// The big round play / pause button: a glossy accent dome that glows on
/// the bar.
fn play_button(ui: &mut egui::Ui, paused: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(72.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let down = resp.is_pointer_button_down_on();
        let r = 34.0 + t * 2.0 - if down { 2.0 } else { 0.0 };
        let c = rect.center() + Vec2::new(0.0, if down { 0.5 } else { -t });
        let dome = egui::Rect::from_center_size(c, Vec2::splat(r * 2.0));
        let lit = |a: Color32, b: Color32| a.lerp_to_gamma(b, t);
        depth::raised(
            ui.painter(),
            dome,
            r,
            depth::Raised {
                top: lit(
                    Color32::from_rgb(104, 198, 255),
                    Color32::from_rgb(140, 214, 255),
                ),
                bottom: lit(
                    Color32::from_rgb(14, 106, 190),
                    Color32::from_rgb(22, 128, 222),
                ),
                light: Color32::from_white_alpha(150),
                lift: if down { 0.3 } else { 0.9 },
                glow: Some(theme::ACCENT.gamma_multiply(0.45 + 0.25 * t)),
            },
        );
        if paused {
            widgets::play_mark(ui.painter(), c, 28.0, Color32::WHITE);
        } else {
            widgets::pause_mark(ui.painter(), c, 26.0, Color32::WHITE);
        }
    }
    resp
}

/// Seek back or forward by `secs`: a raised key with a circular arrow and
/// the seconds in it.
fn skip_button(ui: &mut egui::Ui, secs: f64) -> egui::Response {
    let icon = if secs < 0.0 {
        icons::ARROW_COUNTER_CLOCKWISE
    } else {
        icons::ARROW_CLOCKWISE
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = widgets::key_face(ui, &resp, 56.0, false);
        let c = face.center();
        let p = ui.painter();
        p.text(c, egui::Align2::CENTER_CENTER, icon, theme::icon(32.0), fg);
        p.text(
            c + Vec2::new(0.0, 1.0),
            egui::Align2::CENTER_CENTER,
            format!("{}", secs.abs().round() as i64),
            theme::font(Weight::Bold, 10.0),
            fg,
        );
    }
    resp
}

fn seek_bar(
    ui: &mut egui::Ui,
    width: f32,
    state: &mut super::UiState,
    pb: &mut crate::playback::Playback,
    actions: &mut Vec<Action>,
) {
    let duration = pb.player.duration().max(0.001);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click_and_drag());
    let active = resp.hovered() || resp.dragged();
    let t = ui.ctx().animate_bool_with_time(resp.id, active, 0.12);
    let painter = ui.painter();
    let h = 8.0 + t * 4.0;
    let track = egui::Rect::from_center_size(rect.center(), Vec2::new(rect.width(), h));
    let round = egui::CornerRadius::same((h / 2.0) as u8);
    depth::sunken(painter, track, h / 2.0, widgets::WELL);
    // Haptic heatmap just above the track.
    if let Some(hm) = &pb.heatmap {
        let n = hm.colors.len().max(1) as f32;
        let w = track.width() / n;
        for (i, c) in hm.colors.iter().enumerate() {
            let x = track.left() + i as f32 * w;
            let r = egui::Rect::from_min_max(
                egui::pos2(x, track.top() - 7.0),
                egui::pos2(x + w + 0.5, track.top() - 3.0),
            );
            painter.rect_filled(r, 0.0, Color32::from_rgb(c[0], c[1], c[2]));
        }
    }
    let pos = state.scrub.unwrap_or_else(|| pb.player.position());
    let frac = (pos / duration).clamp(0.0, 1.0) as f32;
    let to_x = |f: f32| track.left() + track.width() * f;
    // Where the pointer would seek to, previewed in a lighter tone.
    if let Some(p) = resp.hover_pos()
        && !resp.dragged()
    {
        let hx = p.x.clamp(track.left(), track.right());
        if hx > to_x(frac) {
            let ahead = egui::Rect::from_min_max(
                egui::pos2(to_x(frac), track.top()),
                egui::pos2(hx, track.bottom()),
            );
            painter.rect_filled(ahead, round, Color32::from_white_alpha(40));
        }
    }
    let done = egui::Rect::from_min_max(track.min, egui::pos2(to_x(frac), track.bottom()));
    widgets::rail_fill(painter, track, done);
    for (m, _) in &pb.markers {
        let x = to_x((*m / duration).clamp(0.0, 1.0) as f32);
        painter.circle_filled(egui::pos2(x, track.center().y), 3.0, theme::WARN);
    }
    let knob = egui::pos2(to_x(frac), track.center().y);
    widgets::knob(painter, knob, 8.0 + t * 4.0);

    let to_time = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * duration;
    if let Some(p) = resp.hover_pos().or(resp.interact_pointer_pos()) {
        let tm = to_time(p.x);
        let marker = pb.markers.iter().find(|(m, _)| {
            ((m / duration) as f32 * track.width() - (p.x - track.left())).abs() < 8.0
        });
        let label = match marker {
            Some((_, name)) => format!("{} · {name}", fmt_time(tm)),
            None => fmt_time(tm),
        };
        // A bubble above the track, on the panel's top layer. A marker's
        // name can be longer than the bar is wide.
        let room = ui.max_rect();
        let g = widgets::one_line(
            ui.painter(),
            label,
            theme::font(Weight::SemiBold, 15.0),
            theme::BG,
            room.width() - 16.0,
        );
        let size = g.size() + Vec2::new(16.0, 8.0);
        let x = widgets::fit(p.x - size.x / 2.0, size.x, room.left(), room.right());
        let bubble = egui::Rect::from_min_size(egui::pos2(x, track.top() - size.y - 10.0), size);
        let top = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("seek-time"),
        ));
        top.rect_filled(bubble, egui::CornerRadius::same(8), theme::TEXT);
        top.galley(bubble.min + Vec2::new(8.0, 4.0), g, theme::BG);
    }
    if resp.dragged()
        && let Some(p) = resp.interact_pointer_pos()
    {
        state.scrub = Some(to_time(p.x));
    }
    // A click seeks to where it was pressed: by the let-go the ray has moved
    // on a little, which along this bar could be minutes.
    let pressed_at = resp.id.with("pressed-at");
    if resp.is_pointer_button_down_on()
        && let Some(origin) = ui.input(|i| i.pointer.press_origin())
    {
        ui.data_mut(|d| d.insert_temp(pressed_at, origin));
    }
    if resp.drag_stopped() {
        if let Some(t) = state.scrub.take() {
            actions.push(Action::Seek(t));
        }
    } else if resp.clicked()
        && let Some(p) = ui
            .data(|d| d.get_temp::<egui::Pos2>(pressed_at))
            .or(resp.interact_pointer_pos())
    {
        // Snap to a marker when clicking right on it.
        let t = to_time(p.x);
        let snap = pb
            .markers
            .iter()
            .map(|m| m.0)
            .find(|m| ((m - t) / duration).abs() * (track.width() as f64) < 8.0);
        actions.push(Action::Seek(snap.unwrap_or(t)));
    }
}

fn tabs() -> [(&'static str, &'static str); 7] {
    [
        (icons::FRAME_CORNERS, tr!("adjust-format")),
        (icons::ARROWS_OUT_CARDINAL, tr!("adjust-position")),
        (icons::CUBE, tr!("adjust-stereo")),
        (icons::SUN, tr!("adjust-picture")),
        (icons::EYEGLASSES, tr!("settings-passthrough")),
        (icons::LIST, tr!("adjust-chapters")),
        (icons::VIBRATE, tr!("settings-haptics")),
    ]
}

/// Format, view adjustments, tracks and haptics for the open video.
pub fn adjust_panel(ctx: &egui::Context, v: &mut View) {
    widgets::panel_slab(ctx, 28.0);
    let frame = egui::Frame::new()
        .outer_margin(widgets::SHADOW_ROOM)
        .inner_margin(egui::Margin::same(22));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        ui.label(
            RichText::new(tr!("player-adjust"))
                .font(theme::font(Weight::Bold, 26.0))
                .color(theme::TEXT),
        );
        ui.add_space(4.0);
        // Haptics is the last tab, left out while haptics are off.
        let all = tabs();
        let tabs = &all[..all.len() - usize::from(!v.settings.haptics_enabled)];
        if v.state.adjust_tab >= tabs.len() {
            v.state.adjust_tab = 0;
        }
        if let Some(i) = widgets::segmented(ui, tabs, v.state.adjust_tab) {
            v.state.adjust_tab = i;
        }
        ui.add_space(10.0);
        // Save and reset stay at the bottom while the page scrolls.
        let footer = v.state.adjust_tab <= 4;
        let footer_h = if footer { 64.0 } else { 0.0 };
        let page = ui.available_height() - footer_h;
        egui::ScrollArea::vertical()
            .max_height(page)
            .auto_shrink([false, false])
            .show_list(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                let before = pb.settings;
                match v.state.adjust_tab {
                    0 => format_tab(ui, pb, v.settings.hardware_decoding, v.actions),
                    1 => {
                        let flat = pb.format.projection == Projection::Flat;
                        let moved = v.settings.panel_offsets != Default::default()
                            || (flat && (pb.settings.yaw, pb.settings.pitch) != (0.0, 0.0));
                        position_tab(ui, &mut pb.settings, moved, v.actions);
                    }
                    2 => stereo_tab(ui, &mut pb.settings),
                    3 => picture_tab(ui, &mut pb.settings),
                    4 => {
                        widgets::section_label(ui, tr!("settings-passthrough-videos"));
                        ui.label(
                            RichText::new(tr!("adjust-passthrough-desc")).color(theme::TEXT_2),
                        );
                        ui.add_space(6.0);
                        chroma_tab(
                            ui,
                            &mut pb.settings,
                            pb.format,
                            &v.settings.default_view,
                            v.passthrough_available,
                            v.actions,
                        );
                    }
                    5 => chapters_tab(ui, pb, v.actions),
                    _ => haptics_tab(ui, pb.script_count, v.settings, v.devices),
                }
                if pb.settings != before {
                    pb.settings_dirty = true;
                }
            });
        if footer {
            adjust_footer(ui, pb, v.actions);
        }
    });
}

/// Format tab: the detected format and overrides, lens or screen, and
/// what is decoding the video.
fn format_tab(
    ui: &mut egui::Ui,
    pb: &mut crate::playback::Playback,
    hardware_decoding: bool,
    actions: &mut Vec<Action>,
) {
    let s = &mut pb.settings;
    let d = ViewSettings::default();
    let user = pb.evidence == fp_core::format::Evidence::User;
    widgets::section_label(ui, tr!("adjust-format"));
    ui.label(
        RichText::new(tr!(
            "adjust-detected",
            format = super::format_label(&pb.detected.format),
            evidence = super::evidence_label(pb.detected.evidence)
        ))
        .color(theme::TEXT_2),
    );
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        if widgets::chip_icon(
            ui,
            Some(icons::MAGIC_WAND),
            tr!("adjust-format-automatic"),
            !user,
        )
        .clicked()
        {
            actions.push(Action::SetFormat(None));
        }
        for (label, f) in format_choices() {
            // The chips set the layout; a packed mask stays.
            if widgets::chip(ui, label, user && pb.format.same_layout(&f)).clicked() {
                actions.push(Action::SetFormat(Some(VideoFormat {
                    alpha_packed: pb.format.alpha_packed,
                    ..f
                })));
            }
        }
    });
    if let Projection::Fisheye { fov } = pb.format.projection {
        ui.add_space(6.0);
        widgets::section_label(ui, tr!("adjust-fisheye-lens"));
        widgets::rows(ui, |r| {
            let mut f = fov;
            if r.slider(
                tr!("adjust-field-of-view"),
                None,
                &mut f,
                120.0..=300.0,
                fov,
                "°",
                0,
            ) {
                actions.push(Action::SetFormat(Some(VideoFormat {
                    projection: Projection::Fisheye { fov: f.round() },
                    ..pb.format
                })));
            }
            r.slider(
                tr!("adjust-lens-k1"),
                None,
                &mut s.lens_k1,
                -0.5..=0.5,
                0.0,
                "",
                2,
            );
            r.slider(
                tr!("adjust-lens-k2"),
                None,
                &mut s.lens_k2,
                -0.5..=0.5,
                0.0,
                "",
                2,
            );
        });
        ui.add_space(6.0);
        widgets::section_label(ui, tr!("adjust-lens-centre"));
        let images: Vec<[&str; 2]> = match pb.format.stereo {
            StereoLayout::Mono => vec![[tr!("lens-across"), tr!("lens-up")]],
            StereoLayout::SideBySide | StereoLayout::HalfSideBySide => vec![
                [tr!("lens-left-across"), tr!("lens-left-up")],
                [tr!("lens-right-across"), tr!("lens-right-up")],
            ],
            StereoLayout::TopBottom | StereoLayout::HalfTopBottom => vec![
                [tr!("lens-top-across"), tr!("lens-top-up")],
                [tr!("lens-bottom-across"), tr!("lens-bottom-up")],
            ],
        };
        widgets::rows(ui, |r| {
            for (i, titles) in images.iter().enumerate() {
                for (j, title) in titles.iter().enumerate() {
                    // Shown in percent of the image.
                    let mut v = s.lens_centre[i][j] * 100.0;
                    if r.slider(title, None, &mut v, -10.0..=10.0, 0.0, "%", 1) {
                        s.lens_centre[i][j] = v / 100.0;
                    }
                }
            }
        });
        ui.label(RichText::new(tr!("adjust-lens-centre-desc")).color(theme::TEXT_3));
    }
    if pb.format.projection == Projection::Flat {
        ui.add_space(6.0);
        widgets::section_label(ui, tr!("adjust-screen"));
        widgets::rows(ui, |r| {
            r.slider(
                tr!("adjust-distance"),
                None,
                &mut s.screen_distance,
                1.0..=20.0,
                d.screen_distance,
                " m",
                1,
            );
            r.slider(
                tr!("adjust-width"),
                Some(tr!("settings-screen-width-desc")),
                &mut s.screen_width,
                0.5..=30.0,
                d.screen_width,
                " m",
                1,
            );
            r.slider(
                tr!("adjust-curvature"),
                None,
                &mut s.screen_curvature,
                0.0..=1.0,
                0.0,
                "",
                2,
            );
        });
    }
    if let Some(video) = pb.player.info().video_stream() {
        ui.add_space(6.0);
        widgets::section_label(ui, tr!("adjust-video-stream"));
        widgets::rows(ui, |r| {
            let st = pb.player.stats();
            let decoder = decoder_text(video, &st, hardware_decoding);
            let color = if st.hardware {
                theme::TEXT
            } else {
                theme::WARN
            };
            r.info_colored(tr!("adjust-decoder"), &decoder, color);
            let codec_name = match video.codec.as_str() {
                "hevc" => "HEVC (H.265)".to_string(),
                "h264" => "H.264 (AVC)".to_string(),
                "av1" => "AV1".to_string(),
                "vp9" => "VP9".to_string(),
                c => c.to_uppercase(),
            };
            let format_desc = if video.bit_depth > 8 {
                format!("{codec_name} {}-bit ({})", video.bit_depth, video.chroma())
            } else {
                format!("{codec_name} 8-bit ({})", video.chroma())
            };
            r.row(tr!("adjust-format"), None, |ui| {
                ui.label(RichText::new(format_desc).color(theme::TEXT_2));
            });
            r.row(tr!("adjust-resolution"), None, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} × {} @ {:.2} fps",
                        video.width, video.height, video.fps
                    ))
                    .color(theme::TEXT_2),
                );
            });
            if !video.pixel_format.is_empty() {
                r.row(tr!("adjust-pixel-format"), None, |ui| {
                    ui.label(RichText::new(&video.pixel_format).color(theme::TEXT_2));
                });
            }
        });
    }
}

fn position_tab(ui: &mut egui::Ui, s: &mut ViewSettings, moved: bool, actions: &mut Vec<Action>) {
    let d = ViewSettings::default();
    widgets::rows(ui, |r| {
        r.row(
            tr!("adjust-placement"),
            Some(tr!("adjust-placement-desc")),
            |ui| {
                let button = ui.add_enabled_ui(moved, |ui| {
                    widgets::button(
                        ui,
                        Some(icons::ARROW_COUNTER_CLOCKWISE),
                        tr!("adjust-put-back"),
                        Kind::Secondary,
                    )
                });
                if button.inner.clicked() {
                    actions.push(Action::ResetPlacement);
                }
            },
        );
    });
    widgets::rows(ui, |r| {
        r.slider(
            tr!("adjust-yaw"),
            None,
            &mut s.yaw,
            -180.0..=180.0,
            0.0,
            "°",
            0,
        );
        r.slider(
            tr!("adjust-pitch"),
            None,
            &mut s.pitch,
            -90.0..=90.0,
            0.0,
            "°",
            0,
        );
        r.slider(
            tr!("adjust-roll"),
            None,
            &mut s.roll,
            -45.0..=45.0,
            0.0,
            "°",
            0,
        );
        r.slider(
            tr!("adjust-zoom"),
            None,
            &mut s.zoom,
            0.5..=2.5,
            1.0,
            "×",
            2,
        );
        r.slider(
            tr!("settings-height"),
            Some(tr!("adjust-height-desc")),
            &mut s.height,
            -1.0..=1.0,
            d.height,
            " m",
            2,
        );
        r.slider(
            tr!("adjust-forward"),
            Some(tr!("adjust-forward-desc")),
            &mut s.forward,
            -1.0..=1.0,
            d.forward,
            " m",
            2,
        );
    });
    ui.label(RichText::new(tr!("adjust-position-hint")).color(theme::TEXT_3));
    ui.add_space(6.0);
    widgets::section_label(ui, tr!("settings-background"));
    background_controls(ui, &mut s.background);
}

fn stereo_tab(ui: &mut egui::Ui, s: &mut ViewSettings) {
    let d = ViewSettings::default();
    widgets::rows(ui, |r| {
        r.slider(
            tr!("adjust-depth"),
            Some(tr!("adjust-depth-desc")),
            &mut s.ipd_offset,
            -5.0..=5.0,
            0.0,
            "°",
            1,
        );
        r.slider(
            tr!("adjust-vertical-align"),
            None,
            &mut s.vertical_align,
            -3.0..=3.0,
            0.0,
            "°",
            1,
        );
        r.slider(
            tr!("adjust-rotation-align"),
            None,
            &mut s.rotation_align,
            -3.0..=3.0,
            0.0,
            "°",
            1,
        );
        r.switch(
            tr!("adjust-swap-eyes"),
            Some(tr!("adjust-swap-eyes-desc")),
            &mut s.swap_eyes,
        );
        r.slider(
            tr!("settings-subtitle-depth"),
            None,
            &mut s.subtitle_distance,
            0.8..=10.0,
            d.subtitle_distance,
            " m",
            1,
        );
    });
}

fn picture_tab(ui: &mut egui::Ui, s: &mut ViewSettings) {
    widgets::rows(ui, |r| {
        r.slider(
            tr!("adjust-brightness"),
            None,
            &mut s.brightness,
            -0.5..=0.5,
            0.0,
            "",
            2,
        );
        r.slider(
            tr!("adjust-contrast"),
            None,
            &mut s.contrast,
            0.5..=1.5,
            1.0,
            "",
            2,
        );
        r.slider(
            tr!("adjust-saturation"),
            None,
            &mut s.saturation,
            0.0..=2.0,
            1.0,
            "",
            2,
        );
        r.slider(
            tr!("adjust-gamma"),
            None,
            &mut s.gamma,
            0.5..=2.0,
            1.0,
            "",
            2,
        );
        r.slider(
            tr!("adjust-sharpen"),
            None,
            &mut s.sharpen,
            0.0..=1.0,
            0.0,
            "",
            2,
        );
    });
}

/// Save, reset and keyframes, below the page.
fn adjust_footer(ui: &mut egui::Ui, pb: &crate::playback::Playback, actions: &mut Vec<Action>) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let label = if pb.record_id.is_some() {
            tr!("adjust-save")
        } else {
            tr!("adjust-keep")
        };
        if ui
            .add_enabled_ui(pb.settings_dirty, |ui| {
                widgets::button(ui, Some(icons::CHECK), label, Kind::Primary)
            })
            .inner
            .clicked()
        {
            actions.push(Action::SaveView);
        }
        if widgets::button(
            ui,
            Some(icons::ARROW_COUNTER_CLOCKWISE),
            tr!("adjust-reset"),
            Kind::Secondary,
        )
        .clicked()
        {
            actions.push(Action::ResetView);
        }
        if widgets::icon_button(ui, icons::TIMER, 48.0, false)
            .tip(tr!("adjust-keyframe"))
            .clicked()
        {
            actions.push(Action::AddKeyframe);
        }
        if !pb.keyframes.frames.is_empty()
            && widgets::button(
                ui,
                None,
                &tr!("adjust-clear-keyframes", count = pb.keyframes.frames.len()),
                Kind::Ghost,
            )
            .clicked()
        {
            actions.push(Action::ClearKeyframes);
        }
    });
}

/// Chroma key colours offered as one tap.
fn key_colors() -> [(&'static str, [f32; 3]); 3] {
    [
        (tr!("key-green-screen"), [0.0, 1.0, 0.0]),
        (tr!("key-blue-screen"), [0.08, 0.04, 1.0]),
        (tr!("key-magenta"), [1.0, 0.0, 1.0]),
    ]
}

/// Passthrough tab: the video's own packed mask (`_alpha` videos) or a
/// chroma key, so the room shows where the video's background was. The key
/// follows Settings > Passthrough until it is changed here for this video.
fn chroma_tab(
    ui: &mut egui::Ui,
    s: &mut ViewSettings,
    format: VideoFormat,
    global: &ViewSettings,
    passthrough_available: bool,
    actions: &mut Vec<Action>,
) {
    widgets::rows(ui, |r| {
        let mut packed = format.alpha_packed;
        let desc = if packed && format.alpha_pack_scale().is_none() {
            tr!("adjust-mask-needs-sbs")
        } else {
            tr!("adjust-mask-desc")
        };
        if r.switch(tr!("adjust-mask"), Some(desc), &mut packed) {
            actions.push(Action::SetFormat(Some(VideoFormat {
                alpha_packed: packed,
                ..format
            })));
        }
        let mut own = s.own_key();
        if r.switch(
            tr!("adjust-own-key"),
            Some(if own {
                tr!("adjust-own-key-on")
            } else {
                tr!("adjust-own-key-off")
            }),
            &mut own,
        ) {
            if own {
                s.set_key(global);
            }
            s.key_own = Some(own);
            actions.push(Action::SaveKey);
        }
    });
    let mut shown = s.with_global_key(global);
    if chroma_controls(ui, &mut shown, passthrough_available) {
        s.set_key(&shown);
        s.key_own = Some(true);
        actions.push(Action::SaveKey);
    }
}

/// "Remove the background" and its colour and fine-tune controls, for one
/// video or the global default. True when anything changed.
pub(super) fn chroma_controls(
    ui: &mut egui::Ui,
    s: &mut ViewSettings,
    passthrough_available: bool,
) -> bool {
    let before = s.key();
    let d = ViewSettings::default();
    widgets::rows(ui, |r| {
        r.switch(
            tr!("adjust-chroma"),
            Some(if passthrough_available {
                tr!("adjust-chroma-desc")
            } else {
                tr!("adjust-chroma-desc-dark")
            }),
            &mut s.chroma_key,
        );
    });
    if s.chroma_key {
        premium::label(ui, tr!("adjust-chroma-colour-heading"));
        colour_picker(ui, &key_colors(), &mut s.key_color, d.key_color);
        premium::label(ui, tr!("adjust-fine-tune"));
        widgets::rows(ui, |r| {
            r.slider(
                tr!("adjust-similarity"),
                Some(tr!("adjust-similarity-desc")),
                &mut s.key_similarity,
                0.0..=1.0,
                d.key_similarity,
                "",
                2,
            );
            r.slider(
                tr!("adjust-edge-softness"),
                None,
                &mut s.key_smoothness,
                0.0..=0.5,
                d.key_smoothness,
                "",
                2,
            );
            r.slider(
                tr!("adjust-spill"),
                Some(tr!("adjust-spill-desc")),
                &mut s.key_spill,
                0.0..=1.0,
                d.key_spill,
                "",
                2,
            );
        });
    }
    s.key() != before
}

/// Background colours offered as one tap.
fn backgrounds() -> [(&'static str, [f32; 3]); 4] {
    [
        (tr!("background-black"), [0.0, 0.0, 0.0]),
        (tr!("background-dark-grey"), fp_core::view::BACKGROUND),
        (tr!("background-grey"), [0.25, 0.25, 0.25]),
        (tr!("background-navy"), [0.04, 0.06, 0.16]),
    ]
}

/// The colour around flat videos and the menus while the room doesn't
/// show, for the open video (the Position tab) or the one videos start
/// with (Settings > Video position).
pub(super) fn background_controls(ui: &mut egui::Ui, c: &mut [f32; 3]) {
    ui.label(
        RichText::new(tr!("settings-background-desc"))
            .font(theme::font(Weight::Regular, 15.0))
            .color(theme::TEXT_3),
    );
    colour_picker(ui, &backgrounds(), c, fp_core::view::BACKGROUND);
}

/// A colour, sRGB 0..1: `choices` as one-tap swatches, then a preview and
/// red, green and blue sliders that reset to `default`.
fn colour_picker(
    ui: &mut egui::Ui,
    choices: &[(&str, [f32; 3])],
    c: &mut [f32; 3],
    default: [f32; 3],
) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        for &(label, choice) in choices {
            let on = c.iter().zip(choice).all(|(a, b)| (a - b).abs() < 0.02);
            if premium::swatch(ui, label, choice, on).clicked() {
                *c = choice;
            }
        }
    });
    widgets::rows(ui, |r| {
        r.row(tr!("adjust-chroma-colour"), None, |ui| {
            let [cr, cg, cb] = c.map(|x| (x.clamp(0.0, 1.0) * 255.0) as u8);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(64.0, 32.0), Sense::hover());
            let p = ui.painter();
            let radius = egui::CornerRadius::same(8);
            p.rect_filled(rect, radius, Color32::from_rgb(cr, cg, cb));
            // A dark colour would vanish into the card without an edge.
            p.rect_stroke(
                rect,
                radius,
                (1.0, Color32::from_white_alpha(60)),
                egui::StrokeKind::Inside,
            );
        });
        for (i, label) in [tr!("adjust-red"), tr!("adjust-green"), tr!("adjust-blue")]
            .into_iter()
            .enumerate()
        {
            r.slider(label, None, &mut c[i], 0.0..=1.0, default[i], "", 2);
        }
    });
}

/// Chapters tab: the video's chapters and bookmarks to go to, and how
/// playback is going.
fn chapters_tab(ui: &mut egui::Ui, pb: &crate::playback::Playback, actions: &mut Vec<Action>) {
    if pb.markers.is_empty() {
        ui.label(RichText::new(tr!("adjust-no-chapters")).color(theme::TEXT_2));
    } else {
        widgets::section_label(ui, tr!("adjust-chapters-heading"));
        widgets::rows(ui, |r| {
            for (t, name) in &pb.markers {
                r.row(name, Some(fmt_time(*t).as_str()), |ui| {
                    if widgets::icon_button(ui, icons::PLAY, 40.0, false)
                        .tip(tr!("adjust-chapter-go"))
                        .clicked()
                    {
                        actions.push(Action::Seek(*t));
                    }
                });
            }
        });
    }
    let st = pb.player.stats();
    ui.label(
        RichText::new({
            let decoder = if st.hardware {
                tr!("adjust-stats-hardware", decoder = st.video_decoder.as_str())
            } else {
                st.video_decoder.clone()
            };
            let frames = st.frames_shown + st.frames_dropped;
            tr!(
                "adjust-stats",
                decoder = decoder,
                dropped = st.frames_dropped,
                frames = frames,
                stalls = st.stalls,
                audio = st.audio_decoder.as_str(),
                sink = st.audio_sink.as_str()
            )
        })
        .font(theme::font(Weight::Regular, 14.0))
        .color(theme::TEXT_3),
    );
}

const TRACK_ROW: f32 = 56.0;
const TRACK_PAD: f32 = 20.0;
const LIST_GAP: f32 = 10.0;

/// The audio and subtitles picker over the control bar: the two lists side
/// by side, each scrolling when long, and a row plays its track at once.
/// Its body is as tall as the longer list needs, standing on the panel's
/// bottom edge; a click on the rest of the panel closes it.
pub fn tracks_panel(ctx: &egui::Context, v: &mut View) {
    let Some(pb) = v.playback.as_deref() else {
        return;
    };
    let info = pb.player.info();
    let server_tracks = pb.transcoding.is_some() && !pb.source_tracks.is_empty();
    let audio: Vec<_> = info.audio_streams().collect();
    let subtitles = crate::subtitles::rows(&v.settings.subtitle_language, info, &pb.subtitle_files);
    let label_h = ctx.fonts(|f| f.row_height(&theme::font(Weight::SemiBold, 13.0)));
    let quality_rows = if pb.targets.is_empty() {
        0
    } else {
        2 + pb.targets.len()
    };
    let rows = audio
        .len()
        .max(if server_tracks {
            pb.source_tracks.len()
        } else {
            0
        })
        .max(1)
        .max(subtitles.len() + 1)
        .max(quality_rows) as f32;
    let tall = 2.0 * TRACK_PAD + label_h + LIST_GAP + 8.0 + rows * TRACK_ROW;
    let full = ctx.screen_rect() - widgets::SHADOW_ROOM;
    let body = egui::Rect::from_min_max(
        egui::pos2(full.left(), (full.bottom() - tall).max(full.top())),
        full.max,
    );
    widgets::slab(ctx, body, 24.0);
    if ctx.input(|i| {
        i.pointer.any_click() && i.pointer.interact_pos().is_some_and(|p| !body.contains(p))
    }) {
        v.state.tracks_open = false;
    }
    let actions = &mut *v.actions;
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ctx, |ui| {
            let inner = body.shrink(TRACK_PAD);
            let columns = if quality_rows > 0 { 3.0 } else { 2.0 };
            let w = (inner.width() - TRACK_PAD * (columns - 1.0)) / columns;
            let column = |i: f32| {
                let x = inner.left() + i * (w + TRACK_PAD);
                egui::Rect::from_min_size(egui::pos2(x, inner.top()), Vec2::new(w, inner.height()))
            };
            if quality_rows > 0 {
                quality_list(ui, column(2.0), pb, actions);
            }
            let playing = pb.player.audio_stream();
            track_list(
                ui,
                column(0.0),
                tr!("tracks-audio"),
                &pb.title,
                |ui, fresh| {
                    if server_tracks {
                        for s in pb.source_tracks.iter().filter(|s| !s.subtitle) {
                            let on = pb
                                .request
                                .source_audio
                                .as_deref()
                                .map_or(s.selected, |id| id == s.id);
                            let row = track_row(ui, &s.title, &s.codec, on);
                            if on && fresh {
                                row.scroll_to_me(Some(Align::Center));
                            }
                            if row.clicked() && !on {
                                actions.push(Action::SelectSourceAudio(s.id.clone()));
                            }
                        }
                        return;
                    }
                    if audio.is_empty() {
                        ui.label(RichText::new(tr!("tracks-no-audio")).color(theme::TEXT_3));
                    }
                    for s in &audio {
                        let mut title = crate::subtitles::track_name(s);
                        if s.ambisonic {
                            title += tr!("tracks-spatial");
                        }
                        let detail = format!("{} · {} ch", s.codec, s.channels);
                        let on = playing == Some(s.index);
                        let row = track_row(ui, &title, &detail, on);
                        if on && fresh {
                            row.scroll_to_me(Some(Align::Center));
                        }
                        if row.clicked() && !on {
                            actions.push(Action::SelectAudio(s.index));
                        }
                    }
                },
            );
            track_list(
                ui,
                column(1.0),
                tr!("settings-subtitles"),
                &pb.title,
                |ui, fresh| {
                    if server_tracks {
                        let off = pb
                            .request
                            .source_subtitle
                            .as_deref()
                            .is_none_or(|id| id == "0");
                        if track_row(ui, tr!("tracks-off"), "", off).clicked() && !off {
                            actions.push(Action::SelectSourceSubtitles(None));
                        }
                        for s in pb.source_tracks.iter().filter(|s| s.subtitle) {
                            let on = pb.request.source_subtitle.as_deref() == Some(s.id.as_str());
                            let row = track_row(ui, &s.title, &s.codec, on);
                            if on && fresh {
                                row.scroll_to_me(Some(Align::Center));
                            }
                            if row.clicked() && !on {
                                actions.push(Action::SelectSourceSubtitles(Some(s.id.clone())));
                            }
                        }
                        return;
                    }
                    let mut pick = |ui: &mut egui::Ui, title: &str, detail: &str, pick: Pick| {
                        let on = showing(pb, pick);
                        let row = track_row(ui, title, detail, on);
                        if on && fresh {
                            row.scroll_to_me(Some(Align::Center));
                        }
                        if row.clicked() && !on {
                            actions.push(Action::SelectSubtitles(pick));
                        }
                    };
                    pick(ui, tr!("tracks-off"), "", Pick::Off);
                    for row in &subtitles {
                        pick(ui, &row.title, &row.detail, row.pick);
                    }
                },
            );
        });
}

/// The Quality list: Automatic, Original, then the heights the server can
/// transcode to. Picking one plays it from where the video is.
fn quality_list(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    pb: &crate::playback::Playback,
    actions: &mut Vec<Action>,
) {
    let asked = &pb.request.quality;
    track_list(ui, rect, tr!("tracks-quality"), &pb.title, |ui, fresh| {
        let mut row = |ui: &mut egui::Ui, title: &str, detail: &str, quality: Quality| {
            let on = *asked == quality;
            let r = track_row(ui, title, detail, on);
            if on && fresh {
                r.scroll_to_me(Some(Align::Center));
            }
            if r.clicked() && !on {
                actions.push(Action::SelectQuality(quality));
            }
        };
        let automatic = match (&pb.transcoding, asked) {
            (Some(t), Quality::Automatic) => {
                tr!("tracks-quality-playing", quality = t.label.as_str())
            }
            _ => tr!("tracks-quality-file").to_string(),
        };
        row(
            ui,
            tr!("tracks-quality-automatic"),
            &automatic,
            Quality::Automatic,
        );
        row(ui, tr!("tracks-quality-original"), "", Quality::Original);
        for t in &pb.targets {
            let detail = if t.seekable {
                tr!("tracks-quality-transcode")
            } else {
                tr!("tracks-quality-no-seek")
            };
            row(ui, &t.label, detail, Quality::Transcode(t.id.clone()));
        }
    });
}

/// One of the picker's lists in `rect`, under its label. The first time it
/// shows for `video`, `add` is told to scroll to the row in use.
fn track_list(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    label: &str,
    video: &str,
    add: impl FnOnce(&mut egui::Ui, bool),
) {
    let mut ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    ui.spacing_mut().item_spacing.y = LIST_GAP;
    widgets::section_label(&mut ui, label);
    widgets::card(&mut ui, |ui| {
        let salt = (label, video);
        let fresh = egui::scroll_area::State::load(ui.ctx(), ui.make_persistent_id(salt)).is_none();
        egui::ScrollArea::vertical()
            .id_salt(salt)
            .auto_shrink([false, true])
            .show_list(ui, |ui| add(ui, fresh));
    });
}

/// A row of the picker: the whole row picks its track, and a check marks
/// the one in use.
fn track_row(ui: &mut egui::Ui, title: &str, detail: &str, on: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, TRACK_ROW), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let p = ui.painter();
        let face = rect.shrink(2.0);
        if on {
            p.rect_filled(
                face,
                egui::CornerRadius::same(10),
                theme::ACCENT.gamma_multiply(0.16),
            );
        }
        if t > 0.0 {
            p.rect_filled(
                face,
                egui::CornerRadius::same(10),
                Color32::from_white_alpha((t * 14.0) as u8),
            );
        }
        let text_w = w - 16.0 - 52.0;
        let line =
            |text: &str, font, color| widgets::one_line(p, text.to_string(), font, color, text_w);
        let title_g = line(title, theme::font(Weight::Medium, 17.0), theme::TEXT);
        let detail_g = (!detail.is_empty())
            .then(|| line(detail, theme::font(Weight::Regular, 14.0), theme::TEXT_2));
        let (title_h, detail_h) = (
            title_g.size().y,
            detail_g.as_ref().map_or(0.0, |g| 2.0 + g.size().y),
        );
        let x = rect.left() + 16.0;
        let y = rect.center().y - (title_h + detail_h) / 2.0;
        p.galley(egui::pos2(x, y), title_g, theme::TEXT);
        if let Some(g) = detail_g {
            p.galley(egui::pos2(x, y + title_h + 2.0), g, theme::TEXT_2);
        }
        if on {
            p.text(
                egui::pos2(rect.right() - 26.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                icons::CHECK,
                theme::icon(22.0),
                theme::ACCENT_HOVER,
            );
        }
    }
    resp
}

fn haptics_tab(
    ui: &mut egui::Ui,
    scripts: usize,
    settings: &mut crate::settings::Settings,
    devices: &[fp_haptics::DeviceStatus],
) {
    ui.label(
        RichText::new(if scripts == 0 {
            tr!("haptics-no-script").to_string()
        } else {
            tr!("haptics-scripts-loaded", count = scripts)
        })
        .color(theme::TEXT_2),
    );
    let mut offset = settings.haptics.offset_ms as f32;
    let l0 = settings
        .haptics
        .axes
        .entry(fp_haptics::Axis::L0)
        .or_default();
    let (mut lo, mut hi) = (l0.min * 100.0, l0.max * 100.0);
    let (mut a, mut b, mut o) = (false, false, false);
    let mut invert = l0.invert;
    widgets::rows(ui, |r| {
        o = r.slider(
            tr!("settings-timing-offset"),
            None,
            &mut offset,
            -500.0..=500.0,
            0.0,
            " ms",
            0,
        );
        a = r.slider(
            tr!("haptics-stroke-bottom"),
            None,
            &mut lo,
            0.0..=100.0,
            0.0,
            "%",
            0,
        );
        b = r.slider(
            tr!("haptics-stroke-top"),
            None,
            &mut hi,
            0.0..=100.0,
            100.0,
            "%",
            0,
        );
        r.switch(tr!("haptics-invert"), None, &mut invert);
    });
    let l0 = settings
        .haptics
        .axes
        .entry(fp_haptics::Axis::L0)
        .or_default();
    l0.invert = invert;
    if a || b {
        l0.min = (lo / 100.0).min(hi / 100.0 - 0.05).max(0.0);
        l0.max = (hi / 100.0).max(l0.min + 0.05).min(1.0);
    }
    if o {
        settings.haptics.offset_ms = offset.round() as i64;
    }
    widgets::section_label(ui, tr!("settings-devices"));
    if devices.is_empty() {
        ui.label(RichText::new(tr!("haptics-none-connected")).color(theme::TEXT_3));
    } else {
        widgets::rows(ui, |r| {
            for d in devices {
                let desc = if d.connected {
                    tr!("haptics-connected").to_string()
                } else {
                    d.last_error
                        .clone()
                        .unwrap_or_else(|| tr!("settings-device-not-connected").into())
                };
                r.row(&d.name, Some(desc.as_str()), |ui| {
                    let c = if d.connected { theme::OK } else { theme::ERROR };
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, c);
                });
            }
        });
    }
}

/// Subtitle cues, drawn large on a dark plate on a transparent panel. Text
/// is set by `shaper` once the fonts have loaded, by egui (Latin, Greek and
/// Cyrillic only) before. With `stereo`, the cues are a 3D video's pictures
/// (see [`stereo_pictures`]). Pictures are widened by the video's
/// `pixel_aspect`, as the video is.
pub fn subtitles(
    ctx: &egui::Context,
    cues: &[fp_media::subtitle::Cue],
    textures: &mut Vec<egui::TextureHandle>,
    shaper: Option<&mut super::subtitle_text::SubtitleText>,
    stereo: Option<&crate::subtitles::StereoSubs>,
    pixel_aspect: f32,
) {
    if let Some(s) = stereo {
        stereo_pictures(ctx, cues, textures, s);
        return;
    }
    let frame = egui::Frame::new().fill(Color32::TRANSPARENT);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let text: Vec<&str> = cues
            .iter()
            .map(|c| c.text.as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        textures.clear();
        for c in cues {
            for (i, b) in c.bitmaps.iter().enumerate() {
                if b.width == 0 || b.height == 0 || b.rgba.len() < (b.width * b.height * 4) as usize
                {
                    continue;
                }
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [b.width as usize, b.height as usize],
                    &b.rgba,
                );
                textures.push(ctx.load_texture(
                    format!("sub{i}"),
                    img,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            for t in textures.iter() {
                let size = t.size_vec2() * Vec2::new(pixel_aspect, 1.0);
                let scale = (ui.available_width() / size.x).min(1.0);
                ui.image((t.id(), size * scale));
            }
            if text.is_empty() {
                return;
            }
            let joined = text.join("\n");
            let (size, width) = (54.0, ui.available_width() - 40.0);
            let pad = Vec2::new(18.0, 8.0);
            if let Some(shaper) = shaper {
                let ppp = ctx.pixels_per_point();
                let img = shaper.render(&joined, size * ppp, width * ppp);
                if img.size[0] == 0 {
                    return;
                }
                let lines = Vec2::new(img.size[0] as f32, img.size[1] as f32) / ppp;
                let tex = ctx.load_texture("sub-text", img, egui::TextureOptions::LINEAR);
                let (rect, _) = ui.allocate_exact_size(lines + pad * 2.0, Sense::hover());
                ui.painter()
                    .rect_filled(rect, 12.0, Color32::from_black_alpha(150));
                ui.painter().image(
                    tex.id(),
                    egui::Rect::from_center_size(rect.center(), lines),
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                textures.push(tex);
                return;
            }
            let font = egui::FontId::proportional(size);
            let galley = ui.painter().layout(joined, font, Color32::WHITE, width);
            let (rect, _) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
            ui.painter()
                .rect_filled(rect, 12.0, Color32::from_black_alpha(150));
            ui.painter().galley(rect.min + pad, galley, Color32::WHITE);
        });
    });
}

/// A 3D video's picture subtitles: each eye's parts on its half of the
/// panel, squeezed to half width as in a side-by-side frame, since each eye
/// is shown its half stretched across the panel. They keep their places in
/// the frame, so the two copies keep the depth they were made with.
fn stereo_pictures(
    ctx: &egui::Context,
    cues: &[fp_media::subtitle::Cue],
    textures: &mut Vec<egui::TextureHandle>,
    s: &crate::subtitles::StereoSubs,
) {
    let frame = egui::Frame::new().fill(Color32::TRANSPARENT);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        textures.clear();
        let panel = ui.max_rect();
        let ((half_width, _), (sx, sy)) = s.half();
        // A point per pixel of the eye's picture as shown, as 2D pictures
        // get, with the frame centred on the panel.
        let g = 1.0;
        let left = (panel.width() - half_width as f32 * sx * g) / 2.0;
        let parts: Vec<_> = (0..2)
            .flat_map(|eye| {
                cues.iter()
                    .flat_map(|c| &c.bitmaps)
                    .filter_map(move |b| s.part(b, eye).map(|p| (eye, p)))
            })
            .collect();
        // The lowest picture rests on the panel's bottom edge.
        let Some(lowest) = parts
            .iter()
            .map(|(_, p)| (p.y + p.height as i32) as f32 * sy * g)
            .reduce(f32::max)
        else {
            return;
        };
        for (i, (eye, p)) in parts.iter().enumerate() {
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [p.width as usize, p.height as usize],
                &p.rgba,
            );
            let tex = ctx.load_texture(format!("sub{i}"), img, egui::TextureOptions::LINEAR);
            let min = egui::pos2(
                panel.left() + (*eye as f32 * panel.width() + left + p.x as f32 * sx * g) / 2.0,
                panel.bottom() - lowest + p.y as f32 * sy * g,
            );
            let size = Vec2::new(p.width as f32 * sx * g / 2.0, p.height as f32 * sy * g);
            ui.painter().image(
                tex.id(),
                egui::Rect::from_min_size(min, size),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            textures.push(tex);
        }
    });
}
