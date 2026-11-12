//! Settings screen: categories on the left, each a page of grouped rows.
//! Edits [`crate::settings::Settings`] in place; the app notices changes,
//! saves them and applies them to the subsystems.

use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea, Tip};
use super::{Action, SettingsTab, View, icons};
use crate::settings::HapticDeviceConfig;
use egui::{Align2, Color32, RichText, Sense, Vec2};

fn tabs() -> [(SettingsTab, &'static str, &'static str); 11] {
    [
        (
            SettingsTab::General,
            icons::GEAR_SIX,
            tr!("settings-general"),
        ),
        (
            SettingsTab::Playback,
            icons::PLAY_CIRCLE,
            tr!("settings-playback"),
        ),
        (
            SettingsTab::VideoPosition,
            icons::FRAME_CORNERS,
            tr!("settings-video-position"),
        ),
        (SettingsTab::Picture, icons::SUN, tr!("settings-picture")),
        (
            SettingsTab::Sound,
            icons::SPEAKER_HIGH,
            tr!("settings-sound"),
        ),
        (
            SettingsTab::Passthrough,
            icons::EYEGLASSES,
            tr!("settings-passthrough"),
        ),
        (
            SettingsTab::Controller,
            icons::GAME_CONTROLLER,
            tr!("settings-controller"),
        ),
        (
            SettingsTab::Haptics,
            icons::VIBRATE,
            tr!("settings-haptics"),
        ),
        (
            SettingsTab::Remote,
            icons::DEVICE_MOBILE,
            tr!("settings-remote"),
        ),
        (
            SettingsTab::Updates,
            icons::CLOUD_ARROW_DOWN,
            tr!("settings-updates"),
        ),
        (SettingsTab::About, icons::INFO, tr!("settings-about")),
    ]
}

pub fn settings(ui: &mut egui::Ui, v: &mut View) {
    let haptics_on = v.settings.haptics_enabled;
    // Haptics is listed only once turned on, in General.
    if v.state.settings_tab == SettingsTab::Haptics && !haptics_on {
        v.state.settings_tab = SettingsTab::General;
    }
    ui.add_space(12.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 28.0;
        ui.vertical(|ui| {
            ui.set_width(240.0);
            // At a large interface size the tabs outgrow the panel.
            egui::ScrollArea::vertical()
                .id_salt("settings-tabs")
                .show_list(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for (t, icon, label) in tabs() {
                        if t == SettingsTab::Haptics && !haptics_on {
                            continue;
                        }
                        if side_tab(ui, icon, label, v.state.settings_tab == t).clicked() {
                            v.state.settings_tab = t;
                        }
                    }
                });
        });
        ui.vertical(|ui| {
            egui::ScrollArea::vertical().show_list(ui, |ui| {
                ui.set_width(ui.available_width() - 8.0);
                ui.spacing_mut().item_spacing.y = 10.0;
                let title = tabs()
                    .iter()
                    .find(|t| t.0 == v.state.settings_tab)
                    .map(|t| t.2)
                    .unwrap_or("");
                ui.label(
                    RichText::new(title)
                        .font(theme::font(Weight::Bold, 30.0))
                        .color(theme::TEXT),
                );
                match v.state.settings_tab {
                    SettingsTab::General => general(ui, v),
                    SettingsTab::Playback => playback(ui, v),
                    SettingsTab::VideoPosition => video_position(ui, v),
                    SettingsTab::Picture => picture(ui, v),
                    SettingsTab::Sound => sound(ui, v),
                    SettingsTab::Passthrough => passthrough(ui, v),
                    SettingsTab::Controller => controller(ui, v),
                    SettingsTab::Haptics => haptics(ui, v),
                    SettingsTab::Remote => remote(ui, v),
                    SettingsTab::Updates => updates(ui, v),
                    SettingsTab::About => about(ui, v),
                }
                ui.add_space(24.0);
            });
        });
    });
}

/// A category in the sidebar: icon and label, filled when selected.
fn side_tab(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let s = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("sel"), selected, 0.12);
        let mut rect = rect;
        if selected || t > 0.01 {
            // The chosen category is a raised key; others rise under the
            // pointer.
            let (face, _) = widgets::key_face(ui, &resp, 12.0, false);
            rect = face;
        }
        let p = ui.painter();
        if s > 0.0 {
            let bar = egui::Rect::from_min_size(
                rect.left_center() - Vec2::new(0.0, 11.0 * s),
                Vec2::new(3.0, 22.0 * s),
            );
            p.rect_filled(bar, egui::CornerRadius::same(2), theme::ACCENT);
        }
        let fg = theme::TEXT_2
            .lerp_to_gamma(theme::TEXT, t)
            .lerp_to_gamma(Color32::WHITE, s);
        p.text(
            rect.left_center() + Vec2::new(30.0, 0.0),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(22.0),
            fg.lerp_to_gamma(theme::ACCENT_HOVER, s),
        );
        // A long name in another language: smaller, then cut short.
        let room = rect.width() - 62.0;
        let mut size: f32 = 17.0;
        while size > 14.0
            && p.layout_no_wrap(label.to_string(), theme::font(Weight::SemiBold, size), fg)
                .size()
                .x
                > room
        {
            size -= 1.0;
        }
        let g = widgets::one_line(
            p,
            label.to_string(),
            theme::font(Weight::SemiBold, size),
            fg,
            room,
        );
        p.galley(
            rect.left_center() + Vec2::new(54.0, -g.size().y / 2.0),
            g,
            fg,
        );
    }
    resp
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(12.0);
    widgets::section_label(ui, text);
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::font(Weight::Regular, 15.0))
            .color(theme::TEXT_3),
    );
}

fn controller(ui: &mut egui::Ui, v: &mut View) {
    use crate::bindings::Bindings;
    super::controller_map::controller_map(ui, v);
    heading(ui, tr!("settings-controls-fixed"));
    widgets::rows(ui, |r| {
        for (k, d) in [
            (tr!("settings-trigger"), tr!("settings-trigger-desc")),
            (
                tr!("settings-grip-trigger"),
                tr!("settings-grip-trigger-desc"),
            ),
            (tr!("settings-both-grips"), tr!("settings-both-grips-desc")),
            (
                tr!("settings-pointing-menu"),
                tr!("settings-pointing-menu-desc"),
            ),
            (
                tr!("settings-pointing-slider"),
                tr!("settings-pointing-slider-desc"),
            ),
        ] {
            r.row(k, None, |ui| {
                ui.label(RichText::new(d).color(theme::TEXT_2));
            });
        }
    });
    ui.add_space(6.0);
    let defaults = Bindings::default();
    ui.add_enabled_ui(v.settings.controls != defaults, |ui| {
        if widgets::button(
            ui,
            Some(icons::ARROW_COUNTER_CLOCKWISE),
            tr!("settings-reset-controls"),
            Kind::Secondary,
        )
        .clicked()
        {
            v.settings.controls = defaults;
            v.state.remap_open = None;
        }
    });
}

/// App-wide switches, the display, thumbnails, then the switches for
/// finding a problem.
fn general(ui: &mut egui::Ui, v: &mut View) {
    let s = &mut *v.settings;
    let open = &mut v.state.interface_languages_open;
    ui.add_space(12.0);
    widgets::rows(ui, |r| {
        let name = |code: &str| {
            crate::i18n::LANGUAGES
                .iter()
                .find(|l| l.0 == code)
                .map_or("English", |l| l.1)
        };
        let shown = match &s.language {
            Some(code) => name(code).to_string(),
            None => tr!(
                "settings-language-automatic",
                language = name(crate::i18n::automatic())
            ),
        };
        r.row(tr!("settings-language"), Some(&shown), |ui| {
            let label = if *open {
                tr!("common-done")
            } else {
                tr!("common-change")
            };
            if widgets::button(ui, None, label, Kind::Secondary).clicked() {
                *open = !*open;
            }
        });
        if *open {
            r.content(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    let auto = tr!("settings-language-automatic-chip");
                    if widgets::chip(ui, auto, s.language.is_none()).clicked() {
                        s.language = None;
                    }
                    for (code, name) in crate::i18n::LANGUAGES {
                        let on = s.language.as_deref() == Some(code);
                        if widgets::chip(ui, name, on).clicked() {
                            s.language = Some(code.to_string());
                        }
                    }
                });
                note(ui, tr!("settings-language-desc"));
            });
        }
        r.slider(
            tr!("settings-interface-size"),
            Some(tr!("settings-interface-size-desc")),
            &mut s.ui_scale,
            crate::settings::UI_SCALES,
            1.0,
            "×",
            2,
        );
        r.switch(
            tr!("settings-follow"),
            Some(tr!("settings-follow-desc")),
            &mut s.follow,
        );
        r.switch(
            tr!("settings-haptic-devices"),
            Some(tr!("settings-haptic-devices-desc")),
            &mut s.haptics_enabled,
        );
    });
    display(ui, &mut s.display);
    heading(ui, tr!("settings-thumbnails"));
    widgets::rows(ui, |r| {
        r.switch(
            tr!("settings-make-thumbnails"),
            Some(tr!("settings-make-thumbnails-desc")),
            &mut s.make_thumbnails,
        );
        r.switch(
            tr!("settings-resume-frames"),
            Some(tr!("settings-resume-frames-desc")),
            &mut s.resume_frames,
        );
        // The sizes under the text, so they fit at any interface size.
        r.row_with(
            tr!("settings-image-cache"),
            Some(tr!("settings-image-cache-desc")),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    for mb in crate::settings::IMAGE_CACHE_SIZES {
                        let label = if mb >= 1000 {
                            {
                                let gb = mb / 1000;
                                tr!("size-gb", n = gb)
                            }
                        } else {
                            tr!("size-mb", n = mb)
                        };
                        if widgets::chip(ui, &label, s.image_cache_mb == mb).clicked() {
                            s.image_cache_mb = mb;
                        }
                    }
                });
            },
            |ui| {
                if widgets::button(ui, Some(icons::TRASH), tr!("common-clear"), Kind::Secondary)
                    .clicked()
                {
                    v.actions.push(Action::ClearImageCache);
                }
            },
        );
    });
    heading(ui, tr!("settings-diagnostics"));
    widgets::rows(ui, |r| {
        r.switch(
            tr!("settings-hardware-decoding"),
            Some(tr!("settings-hardware-decoding-desc")),
            &mut s.hardware_decoding,
        );
        r.switch(
            tr!("settings-perf-overlay"),
            Some(tr!("settings-perf-panel-desc")),
            &mut s.perf_overlay,
        );
    });
}

/// Refresh rate, render resolution and foveated rendering. A row's
/// chips go right to left, so the first choice is added last.
fn display(ui: &mut egui::Ui, d: &mut fp_core::display::Display) {
    use fp_core::display::{EYE_RESOLUTIONS, Foveation, REFRESH_RATES};
    heading(ui, tr!("settings-display"));
    widgets::rows(ui, |r| {
        r.row(
            tr!("settings-refresh-rate"),
            Some(tr!("settings-refresh-rate-desc")),
            |ui| {
                for hz in REFRESH_RATES.into_iter().rev() {
                    if widgets::chip(ui, &format!("{hz} Hz"), d.refresh_rate == hz).clicked() {
                        d.refresh_rate = hz;
                    }
                }
            },
        );
        r.row(
            tr!("settings-render-resolution"),
            Some(tr!("settings-render-resolution-desc")),
            |ui| {
                for px in EYE_RESOLUTIONS.into_iter().rev() {
                    if widgets::chip(ui, &px.to_string(), d.eye_resolution == px).clicked() {
                        d.eye_resolution = px;
                    }
                }
            },
        );
        r.row(
            tr!("settings-foveation"),
            Some(tr!("settings-foveation-desc")),
            |ui| {
                for level in Foveation::ALL.into_iter().rev() {
                    let label = match level {
                        Foveation::Off => tr!("foveation-off"),
                        Foveation::Low => tr!("foveation-low"),
                        Foveation::High => tr!("foveation-high"),
                    };
                    if widgets::chip(ui, label, d.foveation == level).clicked() {
                        d.foveation = level;
                    }
                }
            },
        );
    });
}

fn playback(ui: &mut egui::Ui, v: &mut View) {
    let s = &mut *v.settings;
    ui.add_space(12.0);
    widgets::rows(ui, |r| {
        r.switch(
            tr!("settings-resume"),
            Some(tr!("settings-resume-desc")),
            &mut s.resume,
        );
        let mut step = s.seek_step as f32;
        if r.slider(
            tr!("settings-seek-step"),
            Some(tr!("settings-seek-step-desc")),
            &mut step,
            5.0..=60.0,
            10.0,
            " s",
            0,
        ) {
            s.seek_step = step.round() as f64;
        }
        r.slider(
            tr!("settings-auto-hide"),
            Some(tr!("settings-auto-hide-desc")),
            &mut s.auto_hide_secs,
            2.0..=15.0,
            4.0,
            " s",
            0,
        );
    });
    heading(ui, tr!("settings-default-format"));
    note(ui, tr!("settings-default-format-desc"));
    ui.add_space(6.0);
    if let Some(f) = super::player::format_list(ui, &s.default_format, false, s.fisheye_start) {
        s.default_format = f;
    }
    heading(ui, tr!("settings-fisheye-presets"));
    note(ui, tr!("settings-fisheye-presets-desc"));
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        let mut remove = None;
        for (i, angle) in s.fisheye_presets.iter().enumerate() {
            if widgets::chip(ui, &format!("{angle}°  {}", icons::X), false).clicked() {
                remove = Some(i);
            }
        }
        if let Some(i) = remove {
            s.remove_fisheye_preset(i);
        }
        if s.fisheye_presets_changed()
            && widgets::button_sized(ui, None, tr!("adjust-reset"), Kind::Ghost, 40.0).clicked()
        {
            s.reset_fisheye_presets();
        }
    });
    if !s.fisheye_presets.is_empty() {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            ui.label(RichText::new(tr!("settings-fisheye-start")).color(theme::TEXT_2));
            for &angle in &s.fisheye_presets {
                if widgets::chip(ui, &format!("{angle}°"), s.fisheye_start == angle).clicked() {
                    s.fisheye_start = angle;
                }
            }
        });
    }
}

/// Where videos start: the defaults for a video with no adjustments saved.
fn video_position(ui: &mut egui::Ui, v: &mut View) {
    let d = &mut v.settings.default_view;
    note(ui, tr!("settings-video-position-desc"));
    heading(ui, tr!("settings-flat-videos"));
    widgets::rows(ui, |r| {
        r.slider(
            tr!("settings-screen-distance"),
            None,
            &mut d.screen_distance,
            1.0..=20.0,
            4.0,
            " m",
            1,
        );
        r.slider(
            tr!("settings-screen-width"),
            Some(tr!("settings-screen-width-desc")),
            &mut d.screen_width,
            0.5..=30.0,
            6.0,
            " m",
            1,
        );
        r.slider(
            tr!("settings-screen-curvature"),
            None,
            &mut d.screen_curvature,
            0.0..=1.0,
            0.0,
            "",
            2,
        );
    });
    heading(ui, tr!("settings-vr-videos"));
    widgets::rows(ui, |r| {
        r.slider(
            tr!("adjust-zoom"),
            None,
            &mut d.zoom,
            0.5..=2.5,
            1.0,
            "×",
            2,
        );
        r.slider(
            tr!("settings-height"),
            Some(tr!("settings-height-desc")),
            &mut d.height,
            -1.0..=1.0,
            0.0,
            " m",
            2,
        );
        r.slider(
            tr!("adjust-forward"),
            Some(tr!("adjust-forward-desc")),
            &mut d.forward,
            -1.0..=1.0,
            0.0,
            " m",
            2,
        );
    });
    heading(ui, tr!("settings-background"));
    super::player::background_controls(ui, &mut d.background);
}

/// How videos look until they are adjusted: the picture, and a 3D video's
/// depth and eye alignment.
fn picture(ui: &mut egui::Ui, v: &mut View) {
    let d = &mut v.settings.default_view;
    note(ui, tr!("settings-picture-desc"));
    heading(ui, tr!("settings-all-videos"));
    super::player::picture_tab(ui, d);
    heading(ui, tr!("settings-3d-videos"));
    super::player::stereo_tab(ui, d, false);
}

fn sound(ui: &mut egui::Ui, v: &mut View) {
    let s = &mut *v.settings;
    heading(ui, tr!("settings-sound-heading"));
    widgets::rows(ui, |r| {
        r.slider(
            tr!("settings-volume"),
            None,
            &mut s.volume,
            0.0..=1.5,
            1.0,
            "",
            2,
        );
        r.row(
            tr!("settings-audio-device"),
            Some(tr!("settings-audio-device-desc")),
            |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.audio_device)
                        .margin(egui::Margin::symmetric(12, 9))
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(260.0),
                );
            },
        );
    });
    subtitles(ui, s, &mut v.state.languages_open);
}

/// Which subtitles videos start with, in which language, and how far away.
fn subtitles(ui: &mut egui::Ui, s: &mut crate::settings::Settings, languages_open: &mut bool) {
    use crate::settings::SubtitleMode;
    heading(ui, tr!("settings-subtitles"));
    note(
        ui,
        match s.subtitles {
            SubtitleMode::Off => tr!("settings-subtitles-off-desc"),
            SubtitleMode::Automatic => {
                tr!("settings-subtitles-automatic-desc")
            }
            SubtitleMode::Always => tr!("settings-subtitles-always-desc"),
        },
    );
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        for (label, mode) in [
            (tr!("settings-subtitles-off"), SubtitleMode::Off),
            (tr!("settings-subtitles-automatic"), SubtitleMode::Automatic),
            (tr!("settings-subtitles-always"), SubtitleMode::Always),
        ] {
            if widgets::chip(ui, label, s.subtitles == mode).clicked() {
                s.subtitles = mode;
            }
        }
    });
    ui.add_space(6.0);
    widgets::rows(ui, |r| {
        if s.subtitles != SubtitleMode::Off {
            let name = super::language_name(&s.subtitle_language)
                .unwrap_or_else(|| tr!("settings-language-not-set").to_string());
            r.row(tr!("settings-your-language"), Some(&name), |ui| {
                let label = if *languages_open {
                    tr!("common-done")
                } else {
                    tr!("common-change")
                };
                if widgets::button(ui, None, label, Kind::Secondary).clicked() {
                    *languages_open = !*languages_open;
                }
            });
            if *languages_open {
                r.content(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                        for (code, _, _) in fp_core::language::LANGUAGES {
                            let name = super::language_name(code).unwrap_or_default();
                            if widgets::chip(ui, &name, s.subtitle_language == *code).clicked() {
                                s.subtitle_language = code.to_string();
                            }
                        }
                    });
                });
            }
        }
        r.slider(
            tr!("settings-subtitle-depth"),
            Some(tr!("settings-subtitle-depth-desc")),
            &mut s.default_view.subtitle_distance,
            0.8..=10.0,
            2.5,
            " m",
            1,
        );
    });
}

fn passthrough(ui: &mut egui::Ui, v: &mut View) {
    let available = v.passthrough_available;
    let s = &mut *v.settings;
    heading(ui, tr!("settings-your-room"));
    widgets::rows(ui, |r| {
        if available {
            r.switch(
                tr!("settings-show-room"),
                Some(tr!("settings-show-room-desc")),
                &mut s.passthrough,
            );
        } else {
            r.row(
                tr!("settings-show-room"),
                Some(tr!("settings-show-room-unavailable")),
                |_| {},
            );
        }
    });
    ui.add_space(14.0);
    widgets::section_label(ui, tr!("settings-passthrough-videos"));
    ui.label(RichText::new(tr!("settings-passthrough-videos-desc")).color(theme::TEXT_2));
    ui.add_space(6.0);
    super::player::chroma_controls(ui, &mut s.default_view, available, false);
}

/// The kinds of haptic device, each with the hint its address field shows.
fn device_kinds() -> [(&'static str, &'static str); 3] {
    [
        ("Intiface Central", "ws://192.168.1.20:12345"),
        ("TCode (OSR2/SR6)", tr!("settings-device-hint-tcode")),
        ("The Handy", tr!("settings-device-hint-handy")),
    ]
}

fn haptics(ui: &mut egui::Ui, v: &mut View) {
    heading(ui, tr!("settings-devices"));
    let configs = v.settings.haptic_devices.clone();
    widgets::rows(ui, |r| {
        for (i, cfg) in configs.iter().enumerate() {
            let slot = v.services.haptic_device(cfg);
            let status = slot
                .and_then(|s| s.id)
                .and_then(|id| v.devices.iter().find(|d| d.id == id));
            let desc = match (status, slot) {
                (Some(d), _) if d.connected => {
                    let axes = format!("{:?}", d.axes);
                    tr!("settings-device-connected", axes = axes)
                }
                (Some(d), _) => d
                    .last_error
                    .clone()
                    .unwrap_or_else(|| tr!("settings-device-not-connected").into()),
                (None, Some(s)) => s
                    .error
                    .clone()
                    .unwrap_or_else(|| tr!("settings-device-not-connected").into()),
                (None, None) => tr!("settings-device-connecting").into(),
            };
            r.row(&cfg.label(), Some(desc.as_str()), |ui| {
                if widgets::icon_button(ui, icons::TRASH, 40.0, false)
                    .tip(tr!("settings-device-remove"))
                    .clicked()
                {
                    v.actions.push(Action::RemoveDevice(i));
                }
                let c = if status.is_some_and(|d| d.connected) {
                    theme::OK
                } else {
                    theme::TEXT_3
                };
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 5.0, c);
            });
        }
        r.content(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for (i, (k, _)) in device_kinds().iter().enumerate() {
                    if widgets::chip(ui, k, v.state.new_device_kind == i).clicked() {
                        v.state.new_device_kind = i;
                    }
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let hint = device_kinds()[v.state.new_device_kind.min(2)].1;
                ui.add(
                    egui::TextEdit::singleline(&mut v.state.new_device)
                        .margin(egui::Margin::symmetric(12, 9))
                        .hint_text(hint)
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(ui.available_width() - 180.0),
                );
                let text = v.state.new_device.trim().to_string();
                if ui
                    .add_enabled_ui(!text.is_empty(), |ui| {
                        widgets::button_sized(
                            ui,
                            Some(icons::PLUS),
                            tr!("settings-add-device"),
                            Kind::Secondary,
                            44.0,
                        )
                    })
                    .inner
                    .clicked()
                {
                    let cfg = match v.state.new_device_kind {
                        0 => HapticDeviceConfig::Buttplug {
                            url: if text.contains("://") {
                                text
                            } else {
                                format!("ws://{text}")
                            },
                        },
                        1 => HapticDeviceConfig::Tcode { endpoint: text },
                        _ => HapticDeviceConfig::Handy { key: text },
                    };
                    v.actions.push(Action::AddDevice(cfg));
                    v.state.new_device.clear();
                }
            });
        });
    });
    if !configs.is_empty()
        && widgets::button(
            ui,
            Some(icons::ARROWS_CLOCKWISE),
            tr!("settings-reconnect-all"),
            Kind::Secondary,
        )
        .clicked()
    {
        v.actions.push(Action::ReconnectDevices);
    }
    heading(ui, tr!("settings-timing-range"));
    let h = &mut v.settings.haptics;
    widgets::rows(ui, |r| {
        let mut offset = h.offset_ms as f32;
        if r.slider(
            tr!("settings-timing-offset"),
            Some(tr!("settings-timing-offset-desc")),
            &mut offset,
            -500.0..=500.0,
            0.0,
            " ms",
            0,
        ) {
            h.offset_ms = offset.round() as i64;
        }
        r.slider(
            tr!("settings-speed-limit"),
            None,
            &mut h.speed_limit,
            0.0..=1000.0,
            fp_haptics::HapticsSettings::default().speed_limit,
            " u/s",
            0,
        );
        r.switch(
            tr!("settings-vibration"),
            Some(tr!("settings-vibration-desc")),
            &mut h.stroke_to_vibration,
        );
    });
    note(
        ui,
        &tr!(
            "settings-scripts-found",
            folder = fp_core::dirs::interactive_dir().display().to_string()
        ),
    );
}

fn remote(ui: &mut egui::Ui, v: &mut View) {
    let r = &mut v.settings.remote;
    heading(ui, tr!("settings-remotes"));
    widgets::rows(ui, |rows| {
        rows.switch(
            tr!("settings-web-remote"),
            Some(tr!("settings-web-remote-desc")),
            &mut r.web_enabled,
        );
        rows.switch(
            tr!("settings-deovr-remote"),
            Some(tr!("settings-deovr-remote-desc")),
            &mut r.deovr_enabled,
        );
    });
    note(ui, tr!("settings-remotes-local"));
    if let Some(e) = &v.services.remote_error {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
    let Some(hub) = &v.services.remote else {
        return;
    };
    if let Some(a) = hub.deovr_addr() {
        note(
            ui,
            &tr!(
                "settings-deovr-api-clients",
                port = a.port(),
                count = hub.deovr_client_count()
            ),
        );
    }
    if hub.web_addr().is_none() {
        return;
    }
    heading(ui, tr!("settings-pair"));
    let Some(ip) = fp_remote::lan_addresses().first().copied() else {
        ui.label(RichText::new(tr!("settings-no-network")).color(theme::WARN));
        return;
    };
    let big = |ui: &mut egui::Ui, s: &str| {
        ui.label(
            RichText::new(s)
                .font(theme::font(Weight::SemiBold, 34.0))
                .color(theme::TEXT),
        );
    };
    widgets::card(ui, |ui| {
        egui::Frame::new()
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
                match hub.pairing_code() {
                    Ok(Some(code)) => {
                        ui.label(RichText::new(tr!("settings-pair-open")).color(theme::TEXT_2));
                        big(ui, &hub.pairing_address(ip));
                        ui.add_space(4.0);
                        ui.label(RichText::new(tr!("settings-pair-enter")).color(theme::TEXT_2));
                        big(ui, &format!("{} {}", &code[..3], &code[3..]));
                    }
                    Ok(None) => {
                        ui.label(RichText::new(tr!("settings-pair-stopped")).color(theme::WARN));
                        ui.add_space(6.0);
                        if widgets::button(
                            ui,
                            Some(icons::ARROWS_CLOCKWISE),
                            tr!("settings-pair-new-code"),
                            Kind::Secondary,
                        )
                        .clicked()
                        {
                            v.actions.push(Action::NewPairingCode);
                        }
                    }
                    Err(e) => {
                        ui.label(RichText::new(e.to_string()).color(theme::ERROR));
                    }
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(tr!("settings-pair-phones", count = hub.web_client_count()))
                            .color(theme::TEXT_2),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::button(
                            ui,
                            Some(icons::X),
                            tr!("settings-unpair-all"),
                            Kind::Secondary,
                        )
                        .clicked()
                        {
                            v.actions.push(Action::RegenerateToken);
                        }
                    });
                });
            });
    });
}

fn updates(ui: &mut egui::Ui, v: &mut View) {
    use super::UpdateStatus as U;
    use fp_updater::Channel;
    heading(ui, &format!("FramePlayerX {}", env!("CARGO_PKG_VERSION")));
    widgets::rows(ui, |r| {
        r.switch(
            tr!("settings-check-updates"),
            None,
            &mut v.settings.check_updates,
        );
        let beta_build = env!("CARGO_PKG_VERSION").contains('-');
        let about = match (v.settings.update_channel, beta_build) {
            (Channel::Beta, _) => tr!("settings-channel-beta-desc"),
            (Channel::Stable, false) => tr!("settings-channel-stable-desc"),
            (Channel::Stable, true) => {
                tr!("settings-channel-stable-on-beta-desc")
            }
        };
        r.row(tr!("settings-channel"), Some(about), |ui| {
            // Right to left: Stable shows first.
            for (c, label) in [
                (Channel::Beta, tr!("settings-channel-beta")),
                (Channel::Stable, tr!("settings-channel-stable")),
            ] {
                if widgets::chip(ui, label, v.settings.update_channel == c).clicked()
                    && v.settings.update_channel != c
                {
                    v.settings.update_channel = c;
                    // What's on offer depends on the channel: ask again.
                    if fp_updater::RELEASE_PUBLIC_KEY.is_some()
                        && !matches!(v.state.update, U::Downloading { .. } | U::Installed { .. })
                    {
                        v.actions.push(Action::CheckUpdates);
                    }
                }
            }
        });
    });
    if fp_updater::RELEASE_PUBLIC_KEY.is_none() {
        note(ui, tr!("settings-dev-build"));
        return;
    }
    ui.add_space(6.0);
    widgets::rows(ui, |r| match &v.state.update {
        U::Unknown => {
            r.row(tr!("settings-update-unknown"), None, |ui| {
                check_button(ui, v.actions);
            });
        }
        U::Checking => {
            r.row(tr!("settings-update-checking"), None, |ui| {
                ui.spinner();
            });
        }
        U::UpToDate => {
            r.row(tr!("settings-update-latest"), None, |ui| {
                check_button(ui, v.actions);
                ui.label(
                    RichText::new(icons::CHECK_CIRCLE)
                        .font(theme::icon(24.0))
                        .color(theme::OK),
                );
            });
        }
        U::Available { version, notes } => {
            r.row(
                &tr!("settings-update-available", version = version.as_str()),
                Some(notes.as_str()),
                |ui| {
                    if widgets::button(
                        ui,
                        Some(icons::DOWNLOAD_SIMPLE),
                        tr!("settings-update-install"),
                        Kind::Primary,
                    )
                    .clicked()
                    {
                        v.actions.push(Action::InstallUpdate);
                    }
                },
            );
        }
        U::Downloading { done, total } => {
            r.content(|ui| {
                ui.add(
                    egui::ProgressBar::new(if *total > 0 {
                        *done as f32 / *total as f32
                    } else {
                        0.0
                    })
                    .show_percentage(),
                );
            });
        }
        U::Installed { version } => {
            r.row(
                &tr!("settings-update-installed", version = version.as_str()),
                Some(tr!("settings-update-restart-desc")),
                |ui| {
                    if widgets::button(ui, None, tr!("settings-update-restart"), Kind::Primary)
                        .clicked()
                    {
                        v.actions.push(Action::Restart);
                    }
                },
            );
        }
        U::Failed(e) => {
            r.row(tr!("settings-update-failed"), Some(e.as_str()), |ui| {
                check_button(ui, v.actions);
            });
        }
    });
}

fn check_button(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    if widgets::button_sized(
        ui,
        Some(icons::ARROWS_CLOCKWISE),
        tr!("settings-update-check"),
        Kind::Secondary,
        44.0,
    )
    .clicked()
    {
        actions.push(Action::CheckUpdates);
    }
}

/// What the app tells About about the headset at start (main.rs), in the
/// interface's language.
fn about_text(text: &str) -> &str {
    match text {
        "OpenXR runtime" => tr!("about-runtime"),
        "Headset" => tr!("about-headset-name"),
        "Passthrough for apps" => tr!("about-passthrough"),
        "yes (WebXR immersive-ar possible)" => tr!("about-passthrough-yes"),
        "no (VR only)" => tr!("about-passthrough-no"),
        "Eye resolution" => tr!("about-eye-resolution"),
        "Controllers" => tr!("about-controllers"),
        other => other,
    }
}

/// The project's pages, as About lists them.
const GITHUB: &str = "github.com/remotesojourner/frameplayerx";
const ISSUES: &str = "github.com/remotesojourner/frameplayerx/issues/new/choose";
const SPONSOR: &str = "github.com/sponsors/remotesojourner";

fn about(ui: &mut egui::Ui, v: &mut View) {
    ui.label(
        RichText::new(tr!(
            "settings-about-tagline",
            version = env!("CARGO_PKG_VERSION")
        ))
        .color(theme::TEXT_2),
    );
    heading(ui, tr!("settings-about-headset"));
    widgets::rows(ui, |r| {
        for (k, val) in &v.state.about {
            r.info(about_text(k), about_text(val));
        }
        r.info(
            tr!("settings-about-settings-file"),
            &crate::settings::Settings::path().display().to_string(),
        );
        r.info(
            tr!("settings-about-log"),
            &crate::logger::log_path().display().to_string(),
        );
    });
    heading(ui, tr!("settings-about-links"));
    widgets::rows(ui, |r| {
        for (title, desc, address) in [
            (
                tr!("settings-about-github"),
                tr!("settings-about-github-desc"),
                GITHUB,
            ),
            (
                tr!("settings-about-issue"),
                tr!("settings-about-issue-desc"),
                ISSUES,
            ),
            (
                tr!("settings-about-sponsor"),
                tr!("settings-about-sponsor-desc"),
                SPONSOR,
            ),
        ] {
            r.row_with(
                title,
                Some(desc),
                |ui| {
                    let link = egui::Link::new(
                        RichText::new(format!("{}  {address}", icons::LINK))
                            .font(theme::font(Weight::Medium, 16.0))
                            .color(theme::ACCENT_HOVER),
                    );
                    if ui.add(link).clicked() {
                        v.actions.push(Action::OpenWebPage(address.into()));
                    }
                },
                |_| {},
            );
        }
    });
    ui.add_space(6.0);
    note(ui, tr!("settings-about-licences"));
}
