//! What the headset holds: On this device, its folders and drives to
//! browse, and libraries, folders on it or a card whose videos are scanned
//! (probed, so every filter and order works).

use super::page;
use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea, Tip};
use super::{Action, Browse, LibraryGrid, View, icons};
use egui::{Align, Layout, RichText};
use fp_library::Sort;

/// On this device: its folders and drives.
#[derive(Default)]
pub struct LocalView {
    /// The folder being browsed, else the list of places.
    pub folder: Option<Browse>,
}

/// The headset's folders; true when Back was clicked.
pub fn page(ui: &mut egui::Ui, v: &mut View, l: &mut LocalView) -> bool {
    let title = super::sources::local_name();
    if let Some(b) = l.folder.as_mut() {
        let root = crate::services::builtin_sources()
            .into_iter()
            .find(|c| c.id() == b.source)
            .map(|c| c.name().to_string())
            .unwrap_or_default();
        return super::folders::page(ui, v, title, &root, b);
    }
    let back = page::header(ui, title, Some(tr!("sources-local-desc")), |_| {});
    roots(ui, v, l);
    back
}

/// A library: its videos, or its folders.
pub struct LibraryView {
    /// Showing the folders rather than the videos.
    pub folders: bool,
    pub grid: LibraryGrid,
    /// The folder being browsed in the folders tab.
    pub folder: Option<Browse>,
}

impl LibraryView {
    pub fn new(source: &str) -> LibraryView {
        LibraryView {
            folders: false,
            grid: LibraryGrid::of(source),
            folder: None,
        }
    }
}

/// Library `source`'s page; true when Back was clicked.
pub fn library_page(
    ui: &mut egui::Ui,
    v: &mut View,
    source: &str,
    name: &str,
    l: &mut LibraryView,
) -> bool {
    let Some(fp_sources::SourceConfig::Library(cfg)) = v.services.source_config(source).cloned()
    else {
        return true;
    };
    let present = crate::libraries::present(&cfg);
    let card = cfg
        .card
        .as_ref()
        .map_or("", |c| c.label.as_str())
        .to_string();
    let scanning = v.state.scans.get(source).cloned();
    if l.folders
        && let Some(b) = l.folder.as_mut()
    {
        let root = crate::libraries::display_location(&cfg.root());
        return super::folders::page(ui, v, name, &root, b);
    }
    let subtitle = match &scanning {
        Some(s) => s.clone(),
        None if !present => tr!("local-not-inserted", card = card.as_str()),
        None => crate::libraries::display_location(&cfg.root()),
    };
    let back = page::header(ui, name, Some(&subtitle), |ui| {
        if widgets::button_sized(
            ui,
            Some(icons::ARROWS_CLOCKWISE),
            tr!("local-scan-now"),
            Kind::Secondary,
            44.0,
        )
        .tip(tr!("local-scan-now-tip"))
        .clicked()
        {
            v.actions.push(Action::Rescan {
                source: source.to_string(),
                force: false,
            });
        }
        if l.grid.total > 0
            && widgets::button_sized(
                ui,
                Some(icons::PLAY),
                tr!("common-play-all"),
                Kind::Secondary,
                44.0,
            )
            .clicked()
        {
            let reqs = super::library::play_all(v, &l.grid);
            v.actions.push(Action::PlayAll(reqs, 0));
        }
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        widgets::chip_icon(ui, Some(icons::FILM_STRIP), tr!("local-videos"), true);
        if widgets::chip_icon(ui, Some(icons::FOLDERS), tr!("local-folders"), false).clicked() {
            l.folders = true;
            l.folder = Some(Browse {
                source: source.to_string(),
                loading: true,
                ..Default::default()
            });
            v.actions.push(Action::Browse {
                source: source.to_string(),
                location: None,
            });
        }
    });
    ui.add_space(8.0);
    super::library::grid_page(
        ui,
        v,
        &mut l.grid,
        &[
            (Sort::Added, tr!("sort-newest")),
            (Sort::Title, tr!("sort-title")),
            (Sort::Duration, tr!("filter-length")),
            (Sort::Rating, tr!("filter-rating")),
            (Sort::Random, tr!("sort-shuffle")),
        ],
        |ui, _| {
            let (title, body) = if scanning.is_some() {
                (tr!("local-scanning"), tr!("local-scanning-desc"))
            } else if !present {
                (
                    tr!("local-not-inserted-title"),
                    tr!("local-not-inserted-desc"),
                )
            } else {
                (tr!("local-empty"), tr!("local-empty-desc"))
            };
            page::empty_state(ui, icons::FILM_STRIP, title, body, |_| {});
        },
    );
    back
}

/// The places to browse: the home folder and the removable drives.
fn roots(ui: &mut egui::Ui, v: &mut View, l: &mut LocalView) {
    let mounts = crate::drives::removable_mounts();
    let drives = if mounts.is_empty() {
        tr!("local-no-drives").to_string()
    } else {
        mounts
            .iter()
            .filter_map(|m| m.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let builtin = crate::services::builtin_sources();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show_list(ui, |ui| {
            widgets::card(ui, |ui| {
                for (i, c) in builtin.iter().enumerate() {
                    if i > 0 {
                        let x = ui.max_rect().x_range();
                        ui.painter().hline(
                            (x.min + 84.0)..=(x.max - 16.0),
                            ui.cursor().top(),
                            egui::Stroke::new(1.0_f32, theme::STROKE),
                        );
                    }
                    let removable = c.id() == crate::services::REMOVABLE_SOURCE;
                    let (icon, desc) = if removable {
                        (icons::USB, drives.clone())
                    } else {
                        (icons::HARD_DRIVE, c.address())
                    };
                    let mut open = false;
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 16.0;
                                super::sources::icon_tile(ui, icon, 52.0, theme::ACCENT_HOVER);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(
                                        RichText::new(c.name())
                                            .font(theme::font(Weight::SemiBold, 19.0))
                                            .color(theme::TEXT),
                                    );
                                    ui.label(
                                        RichText::new(&desc)
                                            .font(theme::font(Weight::Regular, 15.0))
                                            .color(theme::TEXT_2),
                                    );
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    open = widgets::button_sized(
                                        ui,
                                        Some(icons::FOLDER_OPEN),
                                        tr!("sources-browse"),
                                        Kind::Secondary,
                                        44.0,
                                    )
                                    .clicked();
                                });
                            });
                        });
                    if open {
                        l.folder = Some(Browse {
                            source: c.id().to_string(),
                            loading: true,
                            ..Default::default()
                        });
                        v.actions.push(Action::Browse {
                            source: c.id().to_string(),
                            location: None,
                        });
                    }
                }
            });
        });
}
