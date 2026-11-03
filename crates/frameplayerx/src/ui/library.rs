//! The browser panel: navigation, home, watch history, video details, and
//! the library grid the history and the headset's own videos share.

use super::page::{self, Card};
use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea, Tip};
use super::{Action, FormatFilter, HomeRows, LibraryGrid, Screen, View, fmt_size, fmt_time, icons};
use crate::playback::OpenRequest;
use egui::{Align, Align2, Color32, Layout, RichText, Sense, Vec2};
use fp_core::format::{DetectedFormat, Evidence, VideoFormat};
use fp_library::{MediaRecord, Sort};

/// Page margin left and right of the content.
const MARGIN: i8 = 28;

/// Main browser panel.
pub fn browser(ctx: &egui::Context, v: &mut View) {
    widgets::panel_slab(ctx, 28.0);
    let room = widgets::SHADOW_ROOM;
    egui::TopBottomPanel::top("nav")
        .exact_height(page::NAV_HEIGHT)
        .frame(
            egui::Frame::new()
                .outer_margin(egui::Margin { bottom: 0, ..room })
                .inner_margin(egui::Margin::symmetric(MARGIN - 6, 0)),
        )
        .show_separator_line(false)
        .show(ctx, |ui| nav(ui, v));
    status_toast(ctx, v);
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .outer_margin(egui::Margin { top: 0, ..room })
                .inner_margin(egui::Margin {
                    left: MARGIN,
                    right: MARGIN,
                    top: 8,
                    bottom: 0,
                }),
        )
        .show(ctx, |ui| {
            // A hairline under the navigation.
            let r = ui.max_rect();
            ui.painter().hline(
                (r.left() - MARGIN as f32)..=(r.right() + MARGIN as f32),
                r.top() - 8.0,
                egui::Stroke::new(1.0_f32, theme::STROKE),
            );
            if v.state.crash.is_some() {
                super::crash::prompt(ui, v);
                return;
            }
            if v.state.picking.is_some() {
                super::playlists::picker(ui, v);
                return;
            }
            if let Some(id) = v.state.details {
                details(ui, v, id);
                return;
            }
            match v.state.screen {
                Screen::Home => home(ui, v),
                Screen::History => history(ui, v),
                Screen::Playlists => super::playlists::page(ui, v),
                Screen::Sources => super::sources::sources(ui, v),
                Screen::Settings => super::settings::settings(ui, v),
                Screen::UpNext => super::queue::page(ui, v),
            }
        });
}

/// The app icon (assets/logo.svg): an X opened in the middle for a play
/// button, on its rounded square. Drawn as shapes, so it's sharp at any
/// panel resolution.
fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    // The SVG's 512 grid.
    let k = size / 512.0;
    let at = |x: f32, y: f32| rect.min + egui::vec2(x, y) * k;
    gradient_square(
        p,
        rect,
        112.0 * k,
        theme::LOGO_SQUARE_TOP,
        theme::LOGO_SQUARE_BOTTOM,
    );
    let arm = |a: egui::Pos2, b: egui::Pos2, color: Color32| {
        p.line_segment([a, b], egui::Stroke::new(52.0 * k, color));
        p.circle_filled(a, 26.0 * k, color);
        p.circle_filled(b, 26.0 * k, color);
    };
    arm(at(118.0, 118.0), at(168.0, 168.0), theme::LOGO_BLUE);
    arm(at(344.0, 344.0), at(394.0, 394.0), theme::LOGO_BLUE);
    arm(at(394.0, 118.0), at(344.0, 168.0), theme::LOGO_VIOLET);
    arm(at(168.0, 344.0), at(118.0, 394.0), theme::LOGO_VIOLET);
    // The play button, with the corners the SVG's 22-wide stroke rounds.
    let play = [at(218.0, 186.0), at(336.0, 256.0), at(218.0, 326.0)];
    p.add(egui::Shape::convex_polygon(
        play.to_vec(),
        Color32::WHITE,
        egui::Stroke::NONE,
    ));
    for (&a, &b) in play.iter().zip(play.iter().cycle().skip(1)) {
        p.line_segment([a, b], egui::Stroke::new(22.0 * k, Color32::WHITE));
        p.circle_filled(a, 11.0 * k, Color32::WHITE);
    }
}

/// A rounded square shaded from `top` to `bottom`, its edge feathered over
/// a pixel like egui's own shapes (egui fills shapes with one colour only).
fn gradient_square(
    p: &egui::Painter,
    rect: egui::Rect,
    radius: f32,
    top: Color32,
    bottom: Color32,
) {
    use std::f32::consts::FRAC_PI_2;
    const STEPS: u32 = 8;
    let feather = 0.5 / p.ctx().pixels_per_point();
    let shade =
        |y: f32| top.lerp_to_gamma(bottom, ((y - rect.top()) / rect.height()).clamp(0.0, 1.0));
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.center(), shade(rect.center().y));
    // Corner centres clockwise from the top left, with where each arc starts.
    let r = egui::vec2(radius, radius);
    let corners = [
        (rect.left_top() + r, 2.0 * FRAC_PI_2),
        (
            rect.right_top() + egui::vec2(-radius, radius),
            3.0 * FRAC_PI_2,
        ),
        (rect.right_bottom() - r, 0.0),
        (rect.left_bottom() + egui::vec2(radius, -radius), FRAC_PI_2),
    ];
    for (centre, start) in corners {
        for s in 0..=STEPS {
            let a = start + FRAC_PI_2 * s as f32 / STEPS as f32;
            let n = egui::vec2(a.cos(), a.sin());
            let edge = centre + n * radius;
            mesh.colored_vertex(edge - n * feather, shade(edge.y));
            mesh.colored_vertex(edge + n * feather, Color32::TRANSPARENT);
        }
    }
    // A fan from the centre to the inner ring, and the feather outside it.
    let ring = 4 * (STEPS + 1);
    for i in 0..ring {
        let j = (i + 1) % ring;
        let (inner, outer) = (1 + 2 * i, 2 + 2 * i);
        let (next_inner, next_outer) = (1 + 2 * j, 2 + 2 * j);
        mesh.add_triangle(0, inner, next_inner);
        mesh.add_triangle(inner, outer, next_outer);
        mesh.add_triangle(inner, next_outer, next_inner);
    }
    p.add(mesh);
}

/// "FramePlayer" and the logo's blue-to-violet "X".
fn wordmark(ui: &mut egui::Ui, size: f32) {
    let font = theme::font(Weight::Bold, size);
    let name = ui
        .painter()
        .layout_no_wrap("FramePlayer".into(), font.clone(), theme::TEXT);
    let x = ui
        .painter()
        .layout_no_wrap("X".into(), font, Color32::WHITE);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(name.size().x + x.size().x, name.size().y),
        Sense::hover(),
    );
    let at = rect.min + egui::vec2(name.size().x, 0.0);
    let p = ui.painter();
    p.galley(rect.min, name, theme::TEXT);
    // egui has no gradient text: the X is drawn in thin vertical slices,
    // each clipped to its strip and given its own colour.
    const SLICES: usize = 12;
    let w = x.size().x / SLICES as f32;
    for i in 0..SLICES {
        // The outer strips reach past the X in case the glyph overhangs.
        let left = if i == 0 {
            at.x - size
        } else {
            at.x + i as f32 * w
        };
        let right = if i == SLICES - 1 {
            at.x + x.size().x + size
        } else {
            at.x + (i + 1) as f32 * w
        };
        let strip = egui::Rect::from_x_y_ranges(left..=right, rect.y_range());
        let color =
            theme::LOGO_BLUE.lerp_to_gamma(theme::LOGO_VIOLET, (i as f32 + 0.5) / SLICES as f32);
        p.with_clip_rect(strip)
            .galley_with_override_text_color(at, x.clone(), color);
    }
}

fn nav(ui: &mut egui::Ui, v: &mut View) {
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        // Brand.
        ui.add_space(6.0);
        logo(ui, 26.0);
        ui.add_space(4.0);
        wordmark(ui, 22.0);
        ui.add_space(28.0);
        let tabs = [
            (Screen::Home, icons::HOUSE, tr!("nav-home")),
            (Screen::History, icons::CLOCK, tr!("nav-history")),
            (Screen::Playlists, icons::PLAYLIST, tr!("nav-playlists")),
            (Screen::Sources, icons::HARD_DRIVES, tr!("nav-sources")),
            (Screen::Settings, icons::GEAR_SIX, tr!("nav-settings")),
        ];
        // At a large interface size the labels would run into the buttons
        // on the right: the tabs then show their icons alone.
        let labels: f32 = tabs
            .iter()
            .map(|(_, _, label)| {
                let font = theme::font(Weight::SemiBold, 18.0);
                ui.painter()
                    .layout_no_wrap(label.to_string(), font, Color32::WHITE)
                    .size()
                    .x
                    + 64.0
            })
            .sum();
        let right = if v.playback.is_some() { 270.0 } else { 120.0 };
        let labelled = ui.available_width() >= labels + right;
        for (s, icon, label) in tabs {
            let selected =
                v.state.screen == s && v.state.details.is_none() && v.state.picking.is_none();
            if widgets::nav_tab(ui, icon, label, selected, labelled).clicked() {
                v.state.screen = s;
                v.state.details = None;
                v.state.picking = None;
                match s {
                    Screen::Home => v.state.home = None,
                    // The tab is the list of playlists, whatever was open.
                    Screen::Playlists => {
                        v.state.playlist = None;
                        v.state.renaming = false;
                        v.state.playlists = None;
                    }
                    // The tab is the list of sources, whatever was open.
                    Screen::Sources => {
                        v.state.source = None;
                        v.state.source_form = None;
                        v.state.pin_form = None;
                    }
                    _ => {}
                }
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if v.playback.is_some() {
                if widgets::button(
                    ui,
                    Some(icons::PLAY),
                    tr!("library-back-to-video"),
                    Kind::Primary,
                )
                .clicked()
                {
                    v.actions.push(Action::ShowBrowser(false));
                }
            } else if widgets::icon_button(ui, icons::POWER, 48.0, false)
                .tip(tr!("library-quit"))
                .clicked()
            {
                v.actions.push(Action::Quit);
            }
            if v.passthrough_available
                && widgets::icon_button(ui, icons::ARMCHAIR, 48.0, v.settings.passthrough)
                    .tip(tr!("player-room"))
                    .clicked()
            {
                v.actions.push(Action::TogglePassthrough);
            }
            if let Some(s) = &v.state.worker_status {
                ui.label(
                    RichText::new(s)
                        .font(theme::font(Weight::Regular, 14.0))
                        .color(theme::TEXT_3),
                );
            }
        });
    });
}

/// Opening, an error, or a recent message: a floating pill at the bottom.
fn status_toast(ctx: &egui::Context, v: &mut View) {
    let toast = v
        .state
        .toast
        .as_ref()
        .filter(|(_, t)| t.elapsed().as_secs_f32() < 6.0)
        .map(|(s, _)| s.clone());
    let err = v.state.open_error.clone();
    let opening = v.state.opening.clone();
    if toast.is_none() && err.is_none() && opening.is_none() {
        return;
    }
    widgets::status_pill(ctx, Align2::CENTER_BOTTOM, Vec2::new(0.0, -24.0), |ui| {
        if let Some(o) = opening {
            ui.spinner();
            let title = widgets::display_title(&o);
            ui.label(tr!("library-opening", title = title));
        } else if let Some(e) = err {
            ui.label(
                RichText::new(icons::WARNING_CIRCLE)
                    .font(theme::icon(22.0))
                    .color(theme::ERROR),
            );
            ui.label(tr!("library-could-not-open", error = e.as_str()));
            if widgets::button_sized(ui, None, tr!("library-dismiss"), Kind::Ghost, 36.0).clicked()
            {
                v.state.open_error = None;
            }
        } else if let Some(t) = toast {
            widgets::toast_line(ui, &t);
        }
    });
}

pub(crate) fn request(r: &MediaRecord) -> OpenRequest {
    OpenRequest {
        location: r.location.clone(),
        source_id: (!r.source_id.is_empty()
            && !r.location.starts_with('/')
            && !r.location.starts_with(fp_core::volume::SCHEME))
        .then(|| r.source_id.clone()),
        entry: None,
        title: Some(r.title.clone()),
        start_at: None,
        ..Default::default()
    }
}

fn open_record(r: &MediaRecord) -> Action {
    Action::Open(request(r))
}

/// Opens `records[i]` with the rest of the list queued for next/previous.
fn open_in(records: &[MediaRecord], i: usize) -> Action {
    Action::OpenList(records.iter().map(request).collect(), i)
}

/// The format a video plays in: until something identifies it, the
/// default chosen in Settings, as playback uses.
fn shown_format(r: &MediaRecord, default: VideoFormat) -> DetectedFormat {
    let mut f = r.effective_format();
    if f.evidence == Evidence::Default {
        f.format = default;
    }
    f
}

/// "12:34 · 180° 3D".
pub(super) fn facts(r: &MediaRecord, default: VideoFormat) -> String {
    let mut f = Vec::new();
    if let Some(d) = r.duration {
        f.push(fmt_time(d));
    }
    f.push(widgets::format_short(&shown_format(r, default).format));
    f.join("  ·  ")
}

/// A video's card: play on click, details from its "…".
fn record_card(ui: &mut egui::Ui, v: &mut View, r: &MediaRecord) -> page::CardResponse {
    let thumb = r.picture();
    let title = widgets::display_title(&r.title);
    let facts = facts(r, v.settings.default_format);
    let badge = widgets::format_short(&shown_format(r, v.settings.default_format).format);
    page::card(
        ui,
        v.thumbs,
        &Card {
            thumb: thumb.as_deref(),
            title: &title,
            facts: &facts,
            badge: Some(&badge),
            corner: (!r.scripts.is_empty()).then_some(icons::WAVEFORM),
            progress: r.progress().map(|p| p as f32),
            watched: r.play_count > 0,
            favorite: r.favorite,
            dim: r.missing,
            more: true,
            ..Card::default()
        },
    )
}

/// Cards for `records` laid out in rows.
fn record_grid(ui: &mut egui::Ui, v: &mut View, records: &[MediaRecord]) {
    page::grid(ui, records.len(), page::LANDSCAPE.x, |ui, i| {
        let c = record_card(ui, v, &records[i]);
        if c.open {
            v.actions.push(open_in(records, i));
        }
        if c.more {
            v.state.details = Some(records[i].id);
        }
    });
}

/// A titled sideways row of cards.
fn record_row(ui: &mut egui::Ui, v: &mut View, title: &str, records: &[MediaRecord]) {
    page::carousel(ui, title, Some(records.len()), records.len(), |ui, i| {
        let c = record_card(ui, v, &records[i]);
        if c.open {
            v.actions.push(open_in(records, i));
        }
        if c.more {
            v.state.details = Some(records[i].id);
        }
    });
}

/// The featured video at the top of Home: what to resume, else the last
/// one watched.
fn hero(ui: &mut egui::Ui, v: &mut View, rows: &HomeRows) {
    let (r, caption, resume) = match (rows.continue_watching.first(), &rows.last_watched) {
        (Some(r), _) => (r, tr!("jf-continue"), true),
        (None, Some(r)) => (r, tr!("library-watch-again"), false),
        _ => return,
    };
    let key = r.picture();
    let resp = page::banner(ui, v.thumbs, key.as_deref(), 300.0, |inner| {
        inner.add_space(22.0);
        widgets::section_label(inner, caption);
        let room = inner.available_width();
        widgets::title_line(
            inner,
            &widgets::display_title(&r.title),
            theme::font(Weight::Bold, 42.0),
            Color32::WHITE,
            room,
        );
        let mut line = facts(r, v.settings.default_format);
        if resume && r.resume_position > 1.0 {
            let left = r.duration.unwrap_or(0.0) - r.resume_position;
            line = format!("{line}  ·  {}", tr!("jf-time-left", time = fmt_time(left)));
        }
        inner.label(
            RichText::new(line)
                .font(theme::font(Weight::Medium, 17.0))
                .color(theme::TEXT_2),
        );
        inner.add_space(18.0);
        inner.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let label = if resume {
                tr!("library-resume")
            } else {
                tr!("player-play")
            };
            if widgets::button(ui, Some(icons::PLAY), label, Kind::Primary).clicked() {
                v.actions.push(open_record(r));
            }
            if widgets::button(
                ui,
                Some(icons::INFO),
                tr!("library-details"),
                Kind::Secondary,
            )
            .clicked()
            {
                v.state.details = Some(r.id);
            }
        });
    });
    if resp.clicked() {
        v.state.details = Some(r.id);
    }
    ui.add_space(26.0);
}

fn home(ui: &mut egui::Ui, v: &mut View) {
    if v.state.home.is_none() {
        let lib = &v.services.library;
        let favorites = lib
            .search(&fp_library::Query {
                favorites_only: true,
                sort: Sort::LastPlayed,
                limit: Some(20),
                ..Default::default()
            })
            .unwrap_or_default();
        let last_watched = lib
            .search(&fp_library::Query {
                watched_only: true,
                sort: Sort::LastPlayed,
                limit: Some(1),
                ..Default::default()
            })
            .unwrap_or_default()
            .pop();
        v.state.home = Some(HomeRows {
            continue_watching: lib.continue_watching(20).unwrap_or_default(),
            last_watched,
            favorites,
        });
    }
    let Some(rows) = v.state.home.take() else {
        return;
    };
    page::scroll(ui, |ui| {
        ui.add_space(14.0);
        let nothing = rows.continue_watching.is_empty()
            && rows.last_watched.is_none()
            && rows.favorites.is_empty();
        if nothing {
            page::empty_state(
                ui,
                icons::PLAY_CIRCLE,
                tr!("library-nothing-watched"),
                tr!("library-home-empty-desc"),
                |_| {},
            );
            ui.add_space(24.0);
        } else {
            hero(ui, v, &rows);
        }
        record_row(ui, v, tr!("jf-continue"), &rows.continue_watching);
        record_row(ui, v, tr!("filter-favorites"), &rows.favorites);
        super::sources::source_row(ui, v, Screen::Home);
    });
    v.state.home = Some(rows);
}

/// Format, 3D and favourite chips and the search field over a library
/// grid; true when something changed.
fn filters(ui: &mut egui::Ui, g: &mut LibraryGrid) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        changed |= page::search_field(ui, &mut g.search, tr!("library-search"), 260.0);
        ui.add_space(10.0);
        for (f, label) in [
            (FormatFilter::All, tr!("filter-all")),
            (FormatFilter::Flat, tr!("filter-flat")),
            (FormatFilter::Vr180, "180°"),
            (FormatFilter::Vr360, "360°"),
            (FormatFilter::Fisheye, tr!("filter-fisheye")),
        ] {
            if widgets::chip(ui, label, g.format_filter == f).clicked() {
                g.format_filter = f;
                changed = true;
            }
        }
        ui.add_space(10.0);
        if widgets::chip_icon(ui, Some(icons::CUBE), "3D", g.stereo_only).clicked() {
            g.stereo_only = !g.stereo_only;
            changed = true;
        }
        if widgets::chip_icon(
            ui,
            Some(icons::HEART),
            tr!("filter-favorites"),
            g.favorites_only,
        )
        .clicked()
        {
            g.favorites_only = !g.favorites_only;
            changed = true;
        }
    });
    changed
}

/// A searchable, filterable, sortable grid of library videos, paged.
/// `empty` draws what an empty grid says when nothing filters it.
pub fn grid_page(
    ui: &mut egui::Ui,
    v: &mut View,
    g: &mut LibraryGrid,
    sorts: &[(Sort, &str)],
    empty: impl FnOnce(&mut egui::Ui, &mut View),
) {
    ui.add_space(4.0);
    let mut changed = filters(ui, g);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let picked = page::sort_bar(ui, sorts, &mut g.sort);
        let again = g.sort == Sort::Random && page::shuffle_again(ui);
        if picked || again {
            if g.sort == Sort::Random {
                // A new shuffle each time it is picked.
                g.base.random_seed = fp_core::playback::now_ms() as u64;
            }
            changed = true;
        }
        page::count_label(ui, &tr!("count-videos", count = g.total));
    });
    ui.add_space(12.0);
    if changed {
        g.page_size = 48;
        g.results = None;
    }
    if g.results.is_none() {
        let q = g.query();
        g.total = v
            .services
            .library
            .count(&fp_library::Query {
                limit: None,
                ..q.clone()
            })
            .unwrap_or(0);
        g.results = Some(v.services.library.search(&q).unwrap_or_default());
    }
    let results = g.results.take().unwrap_or_default();
    let filtered = !g.search.trim().is_empty()
        || g.favorites_only
        || g.stereo_only
        || g.format_filter != FormatFilter::All;
    let mut more = false;
    page::scroll_from(ui, changed, |ui| {
        ui.add_space(4.0);
        if results.is_empty() {
            if filtered {
                page::empty_state(
                    ui,
                    icons::MAGNIFYING_GLASS,
                    tr!("filter-nothing-matches"),
                    tr!("filter-nothing-matches-desc"),
                    |_| {},
                );
            } else {
                empty(ui, v);
            }
        }
        record_grid(ui, v, &results);
        more = results.len() < g.total && page::load_more(ui, false);
    });
    if more {
        g.page_size += 48;
    } else {
        g.results = Some(results);
    }
}

/// Everything watched, from every source.
fn history(ui: &mut egui::Ui, v: &mut View) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr!("nav-history"))
                .font(theme::font(Weight::Bold, 30.0))
                .color(theme::TEXT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let armed = v
                .state
                .clear_armed
                .is_some_and(|t| t.elapsed().as_secs_f32() < 4.0);
            let label = if armed {
                tr!("library-clear-confirm")
            } else {
                tr!("library-clear-history")
            };
            let kind = if armed { Kind::Danger } else { Kind::Secondary };
            if v.state.history.total > 0
                && widgets::button_sized(ui, Some(icons::TRASH), label, kind, 44.0).clicked()
            {
                if armed {
                    v.actions.push(Action::ClearHistory);
                    v.state.clear_armed = None;
                } else {
                    v.state.clear_armed = Some(std::time::Instant::now());
                }
            }
            if armed {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(500));
            }
        });
    });
    let mut g = std::mem::replace(&mut v.state.history, LibraryGrid::history());
    grid_page(
        ui,
        v,
        &mut g,
        &[
            (Sort::LastPlayed, tr!("sort-recently-watched")),
            (Sort::Title, tr!("sort-title")),
            (Sort::PlayCount, tr!("sort-most-watched")),
            (Sort::Rating, tr!("filter-rating")),
            (Sort::Duration, tr!("filter-length")),
        ],
        |ui, v| {
            page::empty_state(
                ui,
                icons::CLOCK,
                tr!("library-nothing-watched"),
                tr!("library-history-empty-desc"),
                |ui| {
                    if widgets::button(
                        ui,
                        Some(icons::HARD_DRIVES),
                        tr!("library-browse-sources"),
                        Kind::Primary,
                    )
                    .clicked()
                    {
                        v.state.screen = Screen::Sources;
                    }
                },
            );
        },
    );
    v.state.history = g;
}

fn details(ui: &mut egui::Ui, v: &mut View, id: fp_library::MediaId) {
    let Some(r) = v.services.library.get(id).ok().flatten() else {
        v.state.details = None;
        return;
    };
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show_list(ui, |ui| {
            ui.add_space(12.0);
            if widgets::button(ui, Some(icons::ARROW_LEFT), tr!("common-back"), Kind::Ghost)
                .clicked()
            {
                v.state.details = None;
            }
            ui.add_space(12.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                let (rect, _) = ui.allocate_exact_size(Vec2::new(512.0, 288.0), Sense::hover());
                widgets::shadow(ui.painter(), rect, 16, 0.6);
                let key = r.picture();
                page::picture(ui, v.thumbs, key.as_deref(), rect, 16, icons::FILM_STRIP);
                ui.vertical(|ui| details_side(ui, v, &r));
            });
            if let Some(about) = r.description.as_deref().filter(|d| !d.trim().is_empty()) {
                ui.add_space(24.0);
                ui.label(
                    RichText::new(about.trim())
                        .font(theme::font(Weight::Regular, 18.0))
                        .color(theme::TEXT_2),
                );
            }
            ui.add_space(28.0);
            widgets::section_label(ui, tr!("adjust-format"));
            ui.add_space(2.0);
            let fmt = shown_format(&r, v.settings.default_format);
            ui.label(
                RichText::new(tr!(
                    "adjust-detected",
                    format = super::format_label(&fmt.format),
                    evidence = super::evidence_label(fmt.evidence)
                ))
                .color(theme::TEXT_2),
            );
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                let current = r.user_format;
                if widgets::chip_icon(
                    ui,
                    Some(icons::MAGIC_WAND),
                    tr!("adjust-format-automatic"),
                    current.is_none(),
                )
                .clicked()
                {
                    v.actions.push(Action::SetUserFormat(r.id, None));
                }
                for f in super::player::format_choices() {
                    if widgets::chip(ui, f.0, current == Some(f.1)).clicked() {
                        v.actions.push(Action::SetUserFormat(r.id, Some(f.1)));
                    }
                }
            });
            ui.add_space(26.0);
            widgets::section_label(ui, tr!("library-file"));
            ui.add_space(2.0);
            ui.label(
                RichText::new(crate::libraries::display_location(&r.location))
                    .font(theme::font(Weight::Regular, 15.0))
                    .color(theme::TEXT_3),
            );
            if r.last_played.is_some() {
                ui.add_space(12.0);
                if widgets::button(
                    ui,
                    Some(icons::TRASH),
                    tr!("library-remove-from-history"),
                    Kind::Danger,
                )
                .clicked()
                {
                    v.actions.push(Action::RemoveFromHistory(r.id));
                    v.state.details = None;
                }
            }
            ui.add_space(24.0);
        });
}

/// Title, facts, actions and rating beside the details thumbnail.
fn details_side(ui: &mut egui::Ui, v: &mut View, r: &MediaRecord) {
    ui.spacing_mut().item_spacing.y = 8.0;
    ui.label(
        RichText::new(widgets::display_title(&r.title))
            .font(theme::font(Weight::Bold, 34.0))
            .color(theme::TEXT),
    );
    let mut f = vec![widgets::format_short(
        &shown_format(r, v.settings.default_format).format,
    )];
    if let Some(d) = r.duration {
        f.push(fmt_time(d));
    }
    if let (Some(w), Some(h)) = (r.width, r.height) {
        f.push(format!("{w}×{h}"));
    }
    if let Some(c) = &r.video_codec {
        f.push(c.to_uppercase());
    }
    if let Some(s) = r.size {
        f.push(fmt_size(s));
    }
    if r.play_count > 0 {
        f.push(tr!("library-watched-times", count = r.play_count));
    }
    if let Some(t) = r.last_played {
        f.push(tr!("library-last-watched", date = super::fmt_date(t)));
    }
    page::facts(ui, &f);
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let resume = r.resume_position > 1.0;
        let label = if resume {
            tr!("jf-resume-from", time = fmt_time(r.resume_position))
        } else {
            tr!("player-play").to_string()
        };
        if widgets::button(ui, Some(icons::PLAY), &label, Kind::Primary).clicked() {
            v.actions.push(open_record(r));
        }
        if resume
            && widgets::button(
                ui,
                Some(icons::ARROW_COUNTER_CLOCKWISE),
                tr!("jf-start-over"),
                Kind::Secondary,
            )
            .clicked()
            && let Action::Open(mut req) = open_record(r)
        {
            req.start_at = Some(0.0);
            v.actions.push(Action::Open(req));
        }
        let heart = if r.favorite {
            tr!("feed-favorite")
        } else {
            tr!("feed-add-favorite")
        };
        if widgets::chip_icon(ui, Some(icons::HEART), heart, r.favorite).clicked() {
            v.actions.push(Action::SetFavorite(r.id, !r.favorite));
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        super::queue::buttons(ui, v, &request(r));
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for n in 1..=5u8 {
            let on = n <= r.rating;
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
            let t = ui
                .ctx()
                .animate_bool_with_time(resp.id, resp.hovered(), 0.1);
            let size = 28.0 + t * 3.0;
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                icons::STAR,
                if on {
                    theme::icon_fill(size)
                } else {
                    theme::icon(size)
                },
                if on {
                    theme::WARN
                } else {
                    theme::TEXT_3.lerp_to_gamma(theme::WARN, t)
                },
            );
            if resp.tip(&format!("{n}/5")).clicked() {
                v.actions
                    .push(Action::SetRating(r.id, if r.rating == n { 0 } else { n }));
            }
        }
    });
    if !r.tags.is_empty() {
        ui.label(RichText::new(r.tags.join("  ·  ")).color(theme::TEXT_2));
    }
    if !r.scripts.is_empty() && v.settings.haptics_enabled {
        let n = r.scripts.len();
        ui.label(
            RichText::new(format!(
                "{}  {}",
                icons::WAVEFORM,
                tr!("library-haptic-scripts", count = n)
            ))
            .color(theme::TEXT_2),
        );
    }
    if let Some(e) = &r.probe_error {
        ui.label(
            RichText::new(format!(
                "{}  {}",
                icons::WARNING_CIRCLE,
                tr!("library-unreadable", error = e.as_str())
            ))
            .color(theme::ERROR),
        );
    }
    if r.missing {
        ui.label(
            RichText::new(format!("{}  {}", icons::WARNING, tr!("library-missing")))
                .color(theme::WARN),
        );
    }
}
