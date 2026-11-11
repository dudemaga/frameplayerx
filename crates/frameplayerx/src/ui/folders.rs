//! Folders of a source that only has files and folders (SMB, WebDAV, HTTP,
//! DLNA, local folders): searched within the folder, sorted by name, date or
//! size, played one at a time or all in order.

use super::page;
use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea, Tip};
use super::{Action, Browse, View, fmt_date, fmt_size, fmt_time, icons};
use crate::playback::OpenRequest;
use egui::{Align2, Color32, Sense, Vec2};
use fp_core::source::{Entry, EntryKind};
use fp_sources::EntrySort;

/// A folder listing of `b` under `title` (the source), the folder it is in
/// (`root` at the top) underneath; true when Back was clicked.
pub fn page(ui: &mut egui::Ui, v: &mut View, title: &str, root: &str, b: &mut Browse) -> bool {
    let words: Vec<String> = b.filter.split_whitespace().map(str::to_lowercase).collect();
    let shown: Vec<Entry> = b
        .entries
        .iter()
        .filter(|e| e.kind != EntryKind::Other)
        .filter(|e| {
            let name = e.name.to_lowercase();
            words.iter().all(|w| name.contains(w.as_str()))
        })
        .cloned()
        .collect();
    let videos: Vec<&Entry> = shown
        .iter()
        .filter(|e| e.kind == EntryKind::Video)
        .collect();
    let subtitle = match &b.location {
        Some(l) => short_location(l),
        None => root.to_string(),
    };
    let (mut refresh, mut play_all) = (false, false);
    let back = page::header(ui, title, Some(&subtitle), |ui| {
        refresh = widgets::icon_button(ui, icons::ARROWS_CLOCKWISE, 44.0, false)
            .tip(tr!("feed-refresh"))
            .clicked();
        if !videos.is_empty() {
            play_all = widgets::button_sized(
                ui,
                Some(icons::PLAY),
                tr!("common-play-all"),
                Kind::Secondary,
                44.0,
            )
            .clicked();
        }
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        page::search_field(ui, &mut b.filter, tr!("folders-search"), 200.0);
        ui.add_space(12.0);
        let mut sort = v.settings.browse_sort;
        if page::sort_bar(
            ui,
            &[
                (EntrySort::Name, tr!("sort-name")),
                (EntrySort::Newest, tr!("sort-newest")),
                (EntrySort::Oldest, tr!("sort-oldest")),
                (EntrySort::Largest, tr!("sort-largest")),
                (EntrySort::Smallest, tr!("sort-smallest")),
            ],
            &mut sort,
        ) {
            v.settings.browse_sort = sort;
            fp_sources::sort_entries_by(&mut b.entries, sort);
        }
        page::count_label(ui, &counts(&shown));
    });
    ui.add_space(6.0);
    page::status(ui, b.loading, b.error.as_deref());

    let queue = |videos: &[&Entry]| -> Vec<OpenRequest> {
        videos
            .iter()
            .map(|x| OpenRequest {
                location: x.location.clone(),
                source_id: Some(b.source.clone()),
                entry: Some((*x).clone()),
                title: None,
                start_at: None,
                ..Default::default()
            })
            .collect()
    };
    if play_all {
        v.actions.push(Action::PlayAll(queue(&videos), 0));
    }
    if refresh {
        v.actions.push(Action::Browse {
            source: b.source.clone(),
            location: b.location.clone(),
        });
    }
    let mut go: Option<String> = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show_list(ui, |ui| {
            if shown.is_empty() && !b.loading && b.error.is_none() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    let text = if words.is_empty() {
                        tr!("folders-empty")
                    } else {
                        tr!("folders-no-match")
                    };
                    ui.label(egui::RichText::new(text).color(theme::TEXT_2));
                });
            }
            for e in &shown {
                let (open, more) = row(ui, v, e);
                if more {
                    v.state.picking = Some(super::Picking {
                        reqs: queue(&[e]),
                        title: e.name.clone(),
                    });
                } else if open {
                    match e.kind {
                        EntryKind::Directory => go = Some(e.location.clone()),
                        _ => {
                            if let Some(req) = queue(&[e]).pop() {
                                v.actions.push(Action::Open(req));
                            }
                        }
                    }
                }
            }
        });
    if let Some(loc) = go {
        b.stack.push(b.location.clone());
        b.location = Some(loc.clone());
        b.loading = true;
        b.filter.clear();
        v.actions.push(Action::Browse {
            source: b.source.clone(),
            location: Some(loc),
        });
    }
    back
}

/// "3 folders · 12 videos".
fn counts(shown: &[Entry]) -> String {
    let count = |k| shown.iter().filter(|e| e.kind == k).count();
    let (folders, videos) = (count(EntryKind::Directory), count(EntryKind::Video));
    let mut parts = Vec::new();
    if folders > 0 {
        parts.push(tr!("count-folders", count = folders));
    }
    if videos > 0 {
        parts.push(tr!("count-videos", count = videos));
    }
    parts.join("  ·  ")
}

/// One folder or video: whether it was clicked, and whether a video's "…"
/// was (Play next, Add to queue, Add to playlist).
fn row(ui: &mut egui::Ui, v: &mut View, e: &Entry) -> (bool, bool) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 88.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return (false, false);
    }
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(14),
        theme::SURFACE.gamma_multiply(t),
    );
    let thumb = egui::Rect::from_min_size(rect.min + Vec2::new(10.0, 8.0), Vec2::new(128.0, 72.0));
    match e.kind {
        EntryKind::Directory => {
            painter.rect_filled(thumb, egui::CornerRadius::same(10), theme::SURFACE_2);
            painter.text(
                thumb.center(),
                Align2::CENTER_CENTER,
                icons::FOLDER_SIMPLE,
                theme::icon_fill(34.0),
                theme::WARN,
            );
        }
        _ => match e.thumbnail_url.as_deref().and_then(|u| v.thumbs.get(u)) {
            Some(tex) => super::thumbs::paint_cover_rounded(&painter, thumb, &tex, 10),
            None => {
                painter.rect_filled(thumb, egui::CornerRadius::same(10), theme::SURFACE_2);
                painter.text(
                    thumb.center(),
                    Align2::CENTER_CENTER,
                    icons::FILM_STRIP,
                    theme::icon(28.0),
                    theme::TEXT_3,
                );
            }
        },
    }
    let title = if e.kind == EntryKind::Directory {
        e.name.clone()
    } else {
        widgets::display_title(&e.name)
    };
    painter.text(
        thumb.right_center() + Vec2::new(18.0, -12.0),
        Align2::LEFT_CENTER,
        title,
        theme::font(Weight::SemiBold, 18.0),
        theme::TEXT,
    );
    let mut facts = Vec::new();
    if e.kind == EntryKind::Directory {
        facts.push(tr!("sources-folder").to_string());
    }
    if let Some(f) = e.format {
        facts.push(widgets::format_short(&f));
    }
    if let Some(d) = e.duration {
        facts.push(fmt_time(d));
    }
    if let Some(s) = e.size {
        facts.push(fmt_size(s));
    }
    if let Some(t) = e.modified {
        facts.push(fmt_date(t));
    }
    if !e.scripts.is_empty() && v.settings.haptics_enabled {
        facts.push("haptics".into());
    }
    painter.text(
        thumb.right_center() + Vec2::new(18.0, 13.0),
        Align2::LEFT_CENTER,
        facts.join("  ·  "),
        theme::font(Weight::Regular, 15.0),
        theme::TEXT_2,
    );
    painter.text(
        rect.right_center() - Vec2::new(24.0, 0.0),
        Align2::CENTER_CENTER,
        if e.kind == EntryKind::Directory {
            icons::CARET_RIGHT
        } else {
            icons::PLAY
        },
        theme::icon(22.0),
        theme::TEXT_3.lerp_to_gamma(Color32::WHITE, t),
    );
    let mut more = false;
    if e.kind == EntryKind::Video {
        let r = egui::Rect::from_center_size(
            rect.right_center() - Vec2::new(80.0, 0.0),
            Vec2::splat(48.0),
        );
        let m = ui.interact(r, resp.id.with("more"), Sense::click());
        let h = ui.ctx().animate_bool_with_time(m.id, m.hovered(), 0.12);
        ui.painter().circle_filled(
            r.center(),
            24.0,
            Color32::from_white_alpha((h * 20.0) as u8),
        );
        ui.painter().text(
            r.center(),
            Align2::CENTER_CENTER,
            icons::DOTS_THREE,
            theme::icon(24.0),
            theme::TEXT_2.lerp_to_gamma(Color32::WHITE, h.max(t * 0.6)),
        );
        more = m.tip(tr!("folders-more-tip")).clicked();
    }
    (resp.clicked() && !more, more)
}

fn short_location(l: &str) -> String {
    let l = crate::libraries::display_location(l);
    let l = l.split('#').next().unwrap_or(&l);
    if l.chars().count() > 60 {
        let tail: String = l
            .chars()
            .rev()
            .take(57)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    } else {
        l.to_string()
    }
}
