//! Sources: the list, the browser each kind gets, adding one, PINs.

use super::page::{self, Card};
use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea, Tip};
use super::{Action, Back, PinForm, QuickConnect, Screen, SourceForm, SourcePage, View, icons};
use egui::{Align, Align2, Color32, Layout, RichText, Sense, Vec2};
use fp_sources::{Credentials, SourceConfig, SourceKind, urlutil};

pub(super) fn kind_icon(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Local => icons::HARD_DRIVE,
        SourceKind::Library => icons::BOOKS,
        SourceKind::Http => icons::GLOBE,
        SourceKind::WebDav => icons::CLOUD,
        SourceKind::Dlna => icons::BROADCAST,
        SourceKind::DeoVr | SourceKind::HereSphere => icons::VIRTUAL_REALITY,
        SourceKind::MediaBrowser | SourceKind::Plex => icons::MONITOR_PLAY,
        SourceKind::Smb => icons::SHARE_NETWORK,
    }
}

/// What the headset itself holds, as the Sources list and Home name it.
pub(super) fn local_name() -> &'static str {
    tr!("sources-local")
}

/// A kind of source, as the lists and the add form name it.
pub(super) fn kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Local => tr!("source-kind-local"),
        SourceKind::Library => tr!("source-kind-library"),
        SourceKind::Http => "HTTP",
        SourceKind::WebDav => "WebDAV",
        SourceKind::Dlna => "DLNA",
        SourceKind::DeoVr => tr!("source-kind-deovr"),
        SourceKind::HereSphere => tr!("source-kind-heresphere"),
        SourceKind::Smb => tr!("source-kind-smb"),
        SourceKind::MediaBrowser => "Jellyfin/Emby",
        SourceKind::Plex => "Plex",
    }
}

/// Compact names for the source-type chips; descriptions keep the full label.
fn kind_chip_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::DeoVr => "DeoVR",
        SourceKind::HereSphere => "HereSphere",
        _ => kind_label(kind),
    }
}

/// A page title with an optional line under it and actions on the right.
fn page_header(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: Option<&str>,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new(title)
                    .font(theme::font(Weight::Bold, 30.0))
                    .color(theme::TEXT),
            );
            if let Some(s) = subtitle {
                ui.label(RichText::new(s).color(theme::TEXT_2));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(14.0);
}

/// A rounded square holding an icon, the leading mark of a list row.
pub(super) fn icon_tile(ui: &mut egui::Ui, icon: &str, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(12), theme::SURFACE_3);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        theme::icon(size * 0.5),
        color,
    );
}

/// A source's PIN state on the Sources page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lock {
    /// No PIN (and the headset itself, which can't have one).
    None,
    Locked,
    Unlocked,
}

fn lock_of(v: &View, id: &str) -> Lock {
    match v.settings.source_pins.contains_key(id) {
        false => Lock::None,
        true if v.state.unlocked_sources.contains(id) => Lock::Unlocked,
        true => Lock::Locked,
    }
}

/// One row of the Sources list.
struct Row {
    id: String,
    name: String,
    desc: String,
    /// What Home's tile says under the name.
    facts: String,
    icon: &'static str,
    /// The headset itself: no PIN, can't be removed.
    builtin: bool,
    lock: Lock,
}

/// The headset first, then the sources the user added.
fn rows(v: &View) -> Vec<Row> {
    let local = Row {
        id: crate::services::LOCAL_HUB.into(),
        name: local_name().into(),
        desc: tr!("sources-local-desc").into(),
        facts: tr!("sources-local-facts").into(),
        icon: icons::HARD_DRIVES,
        builtin: true,
        lock: Lock::None,
    };
    std::iter::once(local)
        .chain(v.services.source_configs.iter().map(|c| {
            let lock = lock_of(v, c.id());
            let kind = match c {
                SourceConfig::Library(l) if !crate::libraries::present(l) => {
                    tr!("sources-library-not-inserted")
                }
                _ => kind_label(c.kind()),
            };
            // The list's line and Home's; a locked source's address can say
            // what is in it, so it shows neither.
            let (desc, facts) = match lock {
                Lock::Locked => (
                    format!("{kind} · {}", tr!("sources-locked")),
                    tr!("sources-locked").to_string(),
                ),
                Lock::Unlocked => (
                    format!("{kind} · {} · {}", c.address(), tr!("sources-unlocked")),
                    format!("{kind} · {}", tr!("sources-unlocked")),
                ),
                Lock::None => (format!("{kind} · {}", c.address()), kind.to_string()),
            };
            Row {
                id: c.id().to_string(),
                name: c.name().to_string(),
                desc,
                facts,
                icon: kind_icon(c.kind()),
                builtin: false,
                lock,
            }
        }))
        .collect()
}

pub fn sources(ui: &mut egui::Ui, v: &mut View) {
    if v.state.pin_form.is_some() {
        pin_form(ui, v);
        return;
    }
    if v.state.source_form.is_some() {
        // Editing adds the PIN rows: taller than the panel.
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_list(ui, |ui| add_form(ui, v));
        return;
    }
    if v.state.source.is_some() {
        browse(ui, v);
        return;
    }
    page_header(ui, tr!("nav-sources"), Some(tr!("sources-desc")), |ui| {
        if widgets::button(ui, Some(icons::PLUS), tr!("sources-add"), Kind::Primary).clicked() {
            v.state.source_form = Some(SourceForm::default());
        }
    });
    egui::ScrollArea::vertical().show_list(ui, |ui| {
        widgets::section_label(ui, tr!("sources-heading"));
        ui.add_space(4.0);
        widgets::card(ui, |ui| {
            for (i, row) in rows(v).into_iter().enumerate() {
                if i > 0 {
                    let x = ui.max_rect().x_range();
                    ui.painter().hline(
                        (x.min + 84.0)..=(x.max - 16.0),
                        ui.cursor().top(),
                        egui::Stroke::new(1.0_f32, theme::STROKE),
                    );
                }
                list_row(ui, v, row);
            }
        });
    });
}

fn list_row(ui: &mut egui::Ui, v: &mut View, row: Row) {
    let Row {
        id,
        name,
        desc,
        icon,
        builtin,
        lock,
        ..
    } = row;
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                icon_tile(ui, icon, 52.0, theme::ACCENT_HOVER);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new(&name)
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
                    if lock == Lock::Locked {
                        if widgets::button_sized(
                            ui,
                            Some(icons::LOCK),
                            tr!("sources-unlock"),
                            Kind::Secondary,
                            44.0,
                        )
                        .clicked()
                        {
                            v.state.pin_form = Some(PinForm {
                                source: id.clone(),
                                name: name.clone(),
                                ..Default::default()
                            });
                        }
                        return;
                    }
                    if !builtin {
                        if widgets::icon_button(ui, icons::TRASH, 44.0, false)
                            .tip(tr!("sources-remove"))
                            .clicked()
                        {
                            v.actions.push(Action::RemoveSource(id.clone()));
                        }
                        if widgets::icon_button(ui, icons::PENCIL_SIMPLE, 44.0, false)
                            .tip(tr!("sources-edit"))
                            .clicked()
                        {
                            edit(v, &id);
                        }
                        let tip = if lock == Lock::Unlocked {
                            tr!("sources-lock")
                        } else {
                            tr!("sources-lock-pin")
                        };
                        if widgets::icon_button(ui, icons::LOCK_OPEN, 44.0, false)
                            .tip(tip)
                            .clicked()
                        {
                            lock_source(v, &id, &name);
                        }
                    }
                    if widgets::button_sized(
                        ui,
                        Some(icons::FOLDER_OPEN),
                        tr!("sources-browse"),
                        Kind::Secondary,
                        44.0,
                    )
                    .clicked()
                    {
                        v.actions.push(Action::OpenSource {
                            source: id.clone(),
                            back_to: Screen::Sources,
                        });
                    }
                });
            });
        });
}

/// The source being browsed, in the browser its kind gets.
fn browse(ui: &mut egui::Ui, v: &mut View) {
    let Some(mut sv) = v.state.source.take() else {
        return;
    };
    let kind = v
        .services
        .source_config(&sv.source)
        .map(|c| kind_label(c.kind()))
        .unwrap_or(tr!("sources-folder"));
    let back = match &mut sv.page {
        SourcePage::Folders(b) => super::folders::page(ui, v, &sv.name, kind, b),
        SourcePage::Local(l) => super::local::page(ui, v, l),
        SourcePage::Library(l) => super::local::library_page(ui, v, &sv.source, &sv.name, l),
        SourcePage::Catalog(j) => super::catalog::page(ui, v, &sv.source, &sv.name, j),
        SourcePage::Feed(f) => super::feeds::page(ui, v, &sv.source, &sv.name, f),
    };
    if back {
        match sv.back() {
            Back::Done => {}
            Back::Parent(a) => v.actions.push(*a),
            Back::Leave => {
                v.state.screen = sv.back_to;
                if sv.back_to == Screen::Home {
                    v.state.home = None;
                }
                return;
            }
        }
    }
    // Something on the page may have opened another source meanwhile.
    if v.state.source.is_none() {
        v.state.source = Some(sv);
    }
}

/// Locks a source with a PIN at once; one without a PIN gets the form to
/// choose one.
fn lock_source(v: &mut View, id: &str, name: &str) {
    if v.settings.source_pins.contains_key(id) {
        v.actions.push(Action::LockSource(id.to_string()));
    } else {
        v.state.screen = Screen::Sources;
        v.state.source = None;
        v.state.pin_form = Some(PinForm {
            source: id.to_string(),
            name: name.to_string(),
            set: true,
            ..Default::default()
        });
    }
}

/// Opens the form editing source `id`.
fn edit(v: &mut View, id: &str) {
    if let Some(cfg) = v.services.source_config(id) {
        v.state.source_form = Some(edit_form(cfg));
    }
}

/// Home's row of sources: each opens its browser, the last adds one.
/// Locked sources stay out of it until unlocked under Sources.
pub fn source_row(ui: &mut egui::Ui, v: &mut View, back_to: Screen) {
    let rows: Vec<Row> = rows(v)
        .into_iter()
        .filter(|r| r.lock != Lock::Locked)
        .collect();
    page::carousel(ui, tr!("nav-sources"), None, rows.len() + 1, |ui, i| {
        let Some(row) = rows.get(i) else {
            let c = page::card(
                ui,
                v.thumbs,
                &Card {
                    placeholder: icons::PLUS,
                    tint: theme::ACCENT_HOVER,
                    title: tr!("sources-add-a-source"),
                    facts: tr!("sources-add-a-source-facts"),
                    play: false,
                    ..Card::default()
                },
            );
            if c.open {
                v.state.screen = Screen::Sources;
                v.state.source = None;
                v.state.source_form = Some(SourceForm::default());
            }
            return;
        };
        // A padlock to lock it: always there with a PIN set, on pointing
        // otherwise (it then asks for one).
        let tip = if row.lock == Lock::Unlocked {
            tr!("sources-lock")
        } else {
            tr!("sources-lock-pin")
        };
        let lockable = (!row.builtin).then_some((icons::LOCK_OPEN, tip));
        let c = page::card(
            ui,
            v.thumbs,
            &Card {
                placeholder: row.icon,
                tint: theme::ACCENT_HOVER,
                title: &row.name,
                facts: &row.facts,
                play: false,
                action: lockable,
                action_always: row.lock == Lock::Unlocked,
                ..Card::default()
            },
        );
        if c.action {
            lock_source(v, &row.id, &row.name);
        } else if c.open {
            v.actions.push(Action::OpenSource {
                source: row.id.clone(),
                back_to,
            });
        }
    });
}

/// Unlocks a source, or sets, changes or removes its PIN.
fn pin_form(ui: &mut egui::Ui, v: &mut View) {
    let has_pin = v
        .state
        .pin_form
        .as_ref()
        .is_some_and(|f| v.settings.source_pins.contains_key(&f.source));
    let Some(f) = v.state.pin_form.as_mut() else {
        return;
    };
    let mut close = false;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        if widgets::icon_button(ui, icons::ARROW_LEFT, 48.0, false)
            .tip(tr!("common-cancel"))
            .clicked()
        {
            close = true;
        }
        let name = f.name.as_str();
        let title = match (f.set, has_pin) {
            (false, _) => tr!("pin-unlock-title", name = name),
            (true, false) => tr!("pin-lock-title", name = name),
            (true, true) => tr!("pin-change-title", name = name),
        };
        ui.label(
            RichText::new(title)
                .font(theme::font(Weight::Bold, 30.0))
                .color(theme::TEXT),
        );
    });
    ui.add_space(8.0);
    let help = if f.set {
        tr!("pin-lock-help")
    } else {
        tr!("pin-enter")
    };
    ui.label(RichText::new(help).color(theme::TEXT_2));
    ui.add_space(12.0);
    let field = |ui: &mut egui::Ui, value: &mut String| {
        let r = ui.add(
            egui::TextEdit::singleline(value)
                .margin(egui::Margin::symmetric(12, 9))
                .password(true)
                .char_limit(*crate::pin::LENGTH.end())
                .font(theme::font(Weight::Regular, 17.0))
                .desired_width(220.0),
        );
        value.retain(|c| c.is_ascii_digit());
        r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
    };
    let mut submit = false;
    widgets::rows(ui, |r| {
        let label = if f.set && has_pin {
            tr!("pin-new")
        } else {
            tr!("pin-label")
        };
        r.row(label, Some(tr!("pin-digits")), |ui| {
            submit |= field(ui, &mut f.pin);
        });
        if f.set {
            r.row(tr!("pin-again"), None, |ui| {
                submit |= field(ui, &mut f.confirm);
            });
        }
    });
    ui.add_space(12.0);
    let valid = crate::pin::valid(&f.pin);
    let matches = !f.set || f.pin == f.confirm;
    ui.horizontal(|ui| {
        let label = if f.set {
            tr!("pin-save")
        } else {
            tr!("sources-unlock")
        };
        let icon = if f.set { icons::LOCK } else { icons::LOCK_OPEN };
        let ok = valid && matches;
        if ui
            .add_enabled_ui(ok, |ui| {
                widgets::button(ui, Some(icon), label, Kind::Primary)
            })
            .inner
            .clicked()
            || (submit && ok)
        {
            v.actions.push(if f.set {
                Action::SetSourcePin {
                    source: f.source.clone(),
                    pin: Some(f.pin.clone()),
                }
            } else {
                Action::UnlockSource {
                    source: f.source.clone(),
                    pin: f.pin.clone(),
                }
            });
        }
        if f.wrong {
            ui.label(RichText::new(tr!("pin-wrong")).color(theme::WARN));
        } else if f.set && !matches && f.confirm.len() >= f.pin.len() && !f.confirm.is_empty() {
            ui.label(RichText::new(tr!("pin-mismatch")).color(theme::WARN));
        }
    });
    if !f.pin.is_empty() {
        f.wrong = false;
    }
    if close {
        v.state.pin_form = None;
    }
}

fn kinds() -> [(SourceKind, &'static str); 9] {
    [
        (SourceKind::Library, tr!("source-kind-library-desc")),
        (SourceKind::MediaBrowser, tr!("source-kind-jellyfin-desc")),
        (SourceKind::Plex, tr!("source-kind-plex-desc")),
        (SourceKind::DeoVr, tr!("source-kind-deovr-desc")),
        (SourceKind::HereSphere, tr!("source-kind-heresphere-desc")),
        (SourceKind::WebDav, tr!("source-kind-webdav-desc")),
        (SourceKind::Http, tr!("source-kind-http-desc")),
        (SourceKind::Smb, tr!("source-kind-smb-desc")),
        (SourceKind::Dlna, tr!("source-kind-dlna-desc")),
    ]
}

pub(super) fn address_hint(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Local | SourceKind::Library => "/home/steamos/Videos/Movies",
        SourceKind::MediaBrowser => "http://192.168.1.10:8096",
        SourceKind::Plex => "http://192.168.1.10:32400",
        SourceKind::DeoVr => "http://192.168.1.10:9999/deovr",
        SourceKind::HereSphere => "http://192.168.1.10:9999/heresphere",
        SourceKind::WebDav => "https://nas.local/dav/videos/",
        SourceKind::Http => "http://192.168.1.10:8080/videos/",
        SourceKind::Smb => "smb://nas.local/videos",
        SourceKind::Dlna => "",
    }
}

pub(super) fn name_hint(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::MediaBrowser => tr!("source-name-hint-jellyfin"),
        SourceKind::Plex => "Plex",
        SourceKind::DeoVr => tr!("source-name-hint-deovr"),
        SourceKind::HereSphere => tr!("source-name-hint-heresphere"),
        SourceKind::WebDav => tr!("source-name-hint-webdav"),
        SourceKind::Http => tr!("source-name-hint-http"),
        SourceKind::Smb => tr!("source-name-hint-smb"),
        SourceKind::Local | SourceKind::Library => tr!("source-name-hint-library"),
        SourceKind::Dlna => tr!("source-name-hint"),
    }
}

fn add_form(ui: &mut egui::Ui, v: &mut View) {
    let dlna_found = v.state.dlna_found.clone();
    let searching = v.state.dlna_searching;
    let pins = v.settings.source_pins.clone();
    let configs = v.services.source_configs.clone();
    let Some(f) = v.state.source_form.as_mut() else {
        return;
    };
    let editing = f.editing.clone();
    let cancel = form_header(ui, f, editing.as_ref());
    let id = match &editing {
        Some(c) => c.id().to_string(),
        None => format!(
            "{}-{}",
            format!("{:?}", f.kind).to_lowercase(),
            fp_core::playback::now_ms()
        ),
    };
    let save = if f.kind == SourceKind::Plex {
        super::plex::form(ui, f, id, v.actions)
    } else if let Some(SourceConfig::Dlna(_)) = &editing {
        dlna_edit_form(ui, f, id)
    } else if f.kind == SourceKind::Dlna {
        dlna_search(ui, v.actions, &dlna_found, searching, &id)
    } else if f.kind == SourceKind::Library {
        library_form(ui, f, id, &configs, v.actions)
    } else {
        server_form(ui, f, id, editing.as_ref(), v.actions)
    };
    if let Some(old) = &editing {
        pin_rows(ui, v, old.id(), old.name(), pins.contains_key(old.id()));
    }
    if let Some(cfg) = save {
        v.actions.push(match editing {
            Some(_) => Action::UpdateSource(cfg),
            None => Action::AddSource(cfg),
        });
        v.state.source_form = None;
    } else if cancel {
        v.state.source_form = None;
    }
}

/// The form's title and its source type; true when cancelled.
fn form_header(ui: &mut egui::Ui, f: &mut SourceForm, editing: Option<&SourceConfig>) -> bool {
    let mut cancel = false;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        if widgets::icon_button(ui, icons::ARROW_LEFT, 48.0, false)
            .tip(tr!("common-cancel"))
            .clicked()
        {
            cancel = true;
        }
        let title = match editing {
            Some(c) => tr!("source-edit-title", name = c.name()),
            None => tr!("sources-add-a-source").to_string(),
        };
        ui.label(
            RichText::new(title)
                .font(theme::font(Weight::Bold, 30.0))
                .color(theme::TEXT),
        );
    });
    ui.add_space(12.0);
    widgets::section_label(ui, tr!("source-type"));
    ui.add_space(4.0);
    if editing.is_some() {
        // A source keeps its type; add another for a different one.
        ui.horizontal(|ui| {
            widgets::chip_icon(ui, Some(kind_icon(f.kind)), kind_chip_label(f.kind), true);
        });
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            for (k, _) in kinds() {
                if widgets::chip_icon(ui, Some(kind_icon(k)), kind_chip_label(k), f.kind == k)
                    .clicked()
                {
                    f.kind = k;
                }
            }
        });
    }
    let help = kinds()
        .iter()
        .find(|(k, _)| *k == f.kind)
        .map(|(_, h)| *h)
        .unwrap_or("");
    ui.add_space(4.0);
    ui.label(RichText::new(help).color(theme::TEXT_2));
    ui.add_space(12.0);
    cancel
}

/// A library's name and folder, with folders worth having one for, and
/// when editing, its videos' upkeep.
fn library_form(
    ui: &mut egui::Ui,
    f: &mut SourceForm,
    id: String,
    configs: &[SourceConfig],
    actions: &mut Vec<Action>,
) -> Option<SourceConfig> {
    let mut save = None;
    let editing = f.editing.is_some();
    widgets::rows(ui, |r| {
        r.row(
            tr!("source-name"),
            Some(tr!("source-name-desc-folder")),
            |ui| text_field(ui, &mut f.name, name_hint(f.kind), false),
        );
        r.row(tr!("sources-folder"), None, |ui| {
            text_field(ui, &mut f.address, address_hint(f.kind), false);
        });
        let suggestions = crate::libraries::suggestions(configs);
        if !suggestions.is_empty() {
            r.content(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    for s in suggestions {
                        let text = s.display().to_string();
                        if widgets::chip_icon(ui, Some(icons::FOLDER_SIMPLE), &text, false)
                            .clicked()
                        {
                            f.address = text;
                        }
                    }
                });
            });
        }
    });
    ui.add_space(12.0);
    let built = build_config(f, id).and_then(|cfg| match &cfg {
        SourceConfig::Library(l) => match crate::libraries::overlap(l, configs) {
            Some(other) => Err(tr!("source-library-overlap", other = other)),
            None => Ok(cfg),
        },
        _ => Ok(cfg),
    });
    ui.horizontal(|ui| match built {
        Ok(cfg) => {
            if widgets::button(ui, Some(icons::CHECK), tr!("common-save"), Kind::Primary).clicked()
            {
                save = Some(cfg);
            }
        }
        Err(e) => {
            ui.add_enabled_ui(false, |ui| {
                widgets::button(ui, Some(icons::CHECK), tr!("common-save"), Kind::Primary)
            });
            if !f.address.is_empty() {
                ui.label(RichText::new(e).color(theme::WARN));
            }
        }
    });
    if let Some(SourceConfig::Library(l)) = f.editing.as_ref().filter(|_| editing) {
        ui.add_space(20.0);
        widgets::section_label(ui, tr!("source-videos"));
        ui.add_space(4.0);
        widgets::rows(ui, |r| {
            r.row(
                tr!("source-rebuild-thumbnails"),
                Some(tr!("source-rebuild-thumbnails-desc")),
                |ui| {
                    if widgets::button_sized(ui, None, tr!("source-rebuild"), Kind::Secondary, 44.0)
                        .clicked()
                    {
                        actions.push(Action::Rescan {
                            source: l.id.clone(),
                            force: true,
                        });
                    }
                },
            );
            r.row(tr!("source-retry-unreadable"), None, |ui| {
                if widgets::button_sized(ui, None, tr!("source-retry"), Kind::Secondary, 44.0)
                    .clicked()
                {
                    actions.push(Action::RetryFailed(l.id.clone()));
                }
            });
        });
    }
    save
}

/// The name and description address of an existing discovered server.
fn dlna_edit_form(ui: &mut egui::Ui, f: &mut SourceForm, id: String) -> Option<SourceConfig> {
    let mut save = None;
    widgets::rows(ui, |r| {
        r.row(
            tr!("source-name"),
            Some(tr!("source-name-desc-server")),
            |ui| text_field(ui, &mut f.name, name_hint(f.kind), false),
        );
        r.row(tr!("source-address"), None, |ui| {
            text_field(ui, &mut f.address, address_hint(f.kind), false);
        });
    });
    ui.add_space(12.0);
    ui.horizontal(|ui| match build_config(f, id) {
        Ok(cfg) => {
            if widgets::button(ui, Some(icons::CHECK), tr!("common-save"), Kind::Primary).clicked()
            {
                save = Some(cfg);
            }
        }
        Err(e) => {
            ui.add_enabled_ui(false, |ui| {
                widgets::button(ui, Some(icons::CHECK), tr!("common-save"), Kind::Primary)
            });
            if !f.address.is_empty() {
                ui.label(RichText::new(e).color(theme::WARN));
            }
        }
    });
    save
}

/// Searching the network for DLNA servers, and adding one.
fn dlna_search(
    ui: &mut egui::Ui,
    actions: &mut Vec<Action>,
    found: &[fp_sources::DlnaDevice],
    searching: bool,
    id: &str,
) -> Option<SourceConfig> {
    let mut save = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled_ui(!searching, |ui| {
                widgets::button(
                    ui,
                    Some(icons::MAGNIFYING_GLASS),
                    tr!("source-dlna-search"),
                    Kind::Primary,
                )
            })
            .inner
            .clicked()
        {
            actions.push(Action::DiscoverDlna);
        }
        if searching {
            ui.spinner();
        }
    });
    ui.add_space(8.0);
    if found.is_empty() {
        if !searching {
            ui.label(RichText::new(tr!("source-dlna-none")).color(theme::TEXT_3));
        }
    } else {
        widgets::rows(ui, |r| {
            for d in found {
                r.row(&d.friendly_name, Some(d.manufacturer.as_str()), |ui| {
                    if widgets::button_sized(
                        ui,
                        Some(icons::PLUS),
                        tr!("common-add"),
                        Kind::Secondary,
                        44.0,
                    )
                    .clicked()
                    {
                        save = Some(SourceConfig::Dlna(d.to_config(id.to_string())));
                    }
                });
            }
        });
    }
    save
}

/// The form for a server or folder: where it is and how to sign in.
fn server_form(
    ui: &mut egui::Ui,
    f: &mut SourceForm,
    id: String,
    editing: Option<&SourceConfig>,
    actions: &mut Vec<Action>,
) -> Option<SourceConfig> {
    let mut save = None;
    let local = f.kind == SourceKind::Local;
    let smb = f.kind == SourceKind::Smb;
    let is_mb = f.kind == SourceKind::MediaBrowser;
    if is_mb && f.use_quick_connect {
        poll_quick_connect(ui, f, actions);
    }
    widgets::rows(ui, |r| {
        r.row(
            tr!("source-name"),
            Some(tr!("source-name-desc-server")),
            |ui| text_field(ui, &mut f.name, name_hint(f.kind), false),
        );
        r.row(
            if local {
                tr!("sources-folder")
            } else {
                tr!("source-address")
            },
            None,
            |ui| {
                text_field(ui, &mut f.address, address_hint(f.kind), false);
            },
        );
        if is_mb {
            r.row(
                tr!("source-sign-in-with"),
                Some(tr!("source-sign-in-with-desc")),
                |ui| {
                    if widgets::chip(ui, tr!("source-password"), !f.use_quick_connect).clicked() {
                        f.use_quick_connect = false;
                    }
                    if widgets::chip(ui, "Quick Connect", f.use_quick_connect).clicked() {
                        f.use_quick_connect = true;
                    }
                },
            );
        }
        if !local {
            if is_mb && f.use_quick_connect {
                quick_connect_rows(r, f, actions);
            } else {
                r.row(
                    tr!("source-user-name"),
                    Some(tr!("source-user-name-desc")),
                    |ui| {
                        text_field(ui, &mut f.username, "", false);
                    },
                );
                r.row(tr!("source-password"), None, |ui| {
                    text_field(ui, &mut f.password, "", true);
                });
            }
            if !smb {
                r.switch(
                    tr!("source-self-signed"),
                    Some(tr!("source-self-signed-desc")),
                    &mut f.insecure_tls,
                );
                r.switch(
                    tr!("source-parallel-downloads"),
                    Some(tr!("source-parallel-downloads-desc")),
                    &mut f.parallel_downloads,
                );
            }
        }
    });
    ui.add_space(12.0);
    let built = build_config(f, id).map(|cfg| match editing {
        Some(old) => keep_hidden(cfg, old),
        None => cfg,
    });
    ui.horizontal(|ui| match built {
        Ok(cfg) => {
            if widgets::button(ui, Some(icons::CHECK), tr!("common-save"), Kind::Primary).clicked()
            {
                save = Some(cfg);
            }
        }
        Err(e) => {
            ui.add_enabled_ui(false, |ui| {
                widgets::button(ui, Some(icons::CHECK), tr!("common-save"), Kind::Primary)
            });
            if !f.address.is_empty() {
                ui.label(RichText::new(e).color(theme::WARN));
            }
        }
    });
    if !is_mb || !f.use_quick_connect {
        ui.add_space(8.0);
        ui.label(
            RichText::new(tr!("source-passwords-note"))
                .font(theme::font(Weight::Regular, 15.0))
                .color(theme::TEXT_3),
        );
    }
    save
}

pub(super) fn text_field(ui: &mut egui::Ui, value: &mut String, hint: &str, masked: bool) {
    ui.add(
        egui::TextEdit::singleline(value)
            .margin(egui::Margin::symmetric(12, 9))
            .password(masked)
            .hint_text(hint)
            .font(theme::font(Weight::Regular, 17.0))
            .desired_width(460.0),
    );
}

/// While a pairing code shows, asks the server every few seconds whether
/// it was approved.
fn poll_quick_connect(ui: &egui::Ui, f: &mut SourceForm, actions: &mut Vec<Action>) {
    if let QuickConnect::Waiting { secret, polled, .. } = &mut f.quick_connect {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
        if polled.elapsed() >= std::time::Duration::from_secs(2) {
            *polled = std::time::Instant::now();
            actions.push(Action::QuickConnectPoll {
                address: f.address.clone(),
                secret: secret.clone(),
                insecure_tls: f.insecure_tls,
            });
        }
    }
}

/// Signing in with Quick Connect: get a code, wait for it, or done.
fn quick_connect_rows(r: &mut widgets::Rows, f: &mut SourceForm, actions: &mut Vec<Action>) {
    let state = f.quick_connect.clone();
    if let QuickConnect::Authorized { .. } = state {
        r.row(
            "Quick Connect",
            Some(tr!("quick-connect-authorized-desc")),
            |ui| {
                if widgets::button_sized(ui, None, tr!("common-clear"), Kind::Secondary, 36.0)
                    .clicked()
                {
                    f.quick_connect = QuickConnect::Idle;
                }
                ui.label(
                    RichText::new(format!(
                        "{}  {}",
                        icons::CHECK_CIRCLE,
                        tr!("quick-connect-authorized")
                    ))
                    .font(theme::font(Weight::SemiBold, 17.0))
                    .color(theme::OK),
                );
            },
        );
    } else if let QuickConnect::Waiting { code, .. } = state {
        r.row(
            tr!("quick-connect-code"),
            Some(tr!("quick-connect-code-desc")),
            |ui| {
                if widgets::button_sized(ui, None, tr!("common-cancel"), Kind::Secondary, 36.0)
                    .clicked()
                {
                    f.quick_connect = QuickConnect::Idle;
                }
                ui.label(RichText::new(tr!("quick-connect-waiting")).color(theme::TEXT_3));
                ui.spinner();
                let spaced: String = code
                    .chars()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                ui.label(
                    RichText::new(spaced)
                        .font(theme::font(Weight::Bold, 26.0))
                        .color(theme::ACCENT),
                );
            },
        );
    } else {
        let desc = if let QuickConnect::Failed(err) = &state {
            tr!("quick-connect-error", error = err.as_str())
        } else {
            tr!("quick-connect-desc").to_string()
        };
        let requesting = state == QuickConnect::Requesting;
        r.row("Quick Connect", Some(&desc), |ui| {
            let can_initiate = !f.address.trim().is_empty() && !requesting;
            if ui
                .add_enabled_ui(can_initiate, |ui| {
                    widgets::button(
                        ui,
                        Some(icons::KEY_RETURN),
                        tr!("quick-connect-get-code"),
                        Kind::Secondary,
                    )
                })
                .inner
                .clicked()
            {
                f.quick_connect = QuickConnect::Requesting;
                actions.push(Action::QuickConnectInitiate {
                    address: f.address.clone(),
                    insecure_tls: f.insecure_tls,
                });
            }
            if requesting {
                ui.label(RichText::new(tr!("settings-device-connecting")).color(theme::TEXT_3));
                ui.spinner();
            }
        });
    }
}

/// The PIN part of the edit form: set one, change it or take it away.
fn pin_rows(ui: &mut egui::Ui, v: &mut View, id: &str, name: &str, has_pin: bool) {
    ui.add_space(20.0);
    widgets::section_label(ui, tr!("pin-label"));
    ui.add_space(4.0);
    let desc = if has_pin {
        tr!("pin-section-locked-desc")
    } else {
        tr!("pin-section-desc")
    };
    widgets::rows(ui, |r| {
        r.row(
            if has_pin {
                tr!("sources-locked")
            } else {
                tr!("pin-none")
            },
            Some(desc),
            |ui| {
                let form = |set| PinForm {
                    source: id.to_string(),
                    name: name.to_string(),
                    set,
                    ..Default::default()
                };
                if has_pin {
                    if widgets::button_sized(
                        ui,
                        Some(icons::TRASH),
                        tr!("pin-remove"),
                        Kind::Danger,
                        44.0,
                    )
                    .clicked()
                    {
                        v.actions.push(Action::SetSourcePin {
                            source: id.to_string(),
                            pin: None,
                        });
                    }
                    if widgets::button_sized(ui, None, tr!("pin-change"), Kind::Secondary, 44.0)
                        .clicked()
                    {
                        v.state.pin_form = Some(form(true));
                    }
                } else if widgets::button_sized(
                    ui,
                    Some(icons::LOCK),
                    tr!("pin-set"),
                    Kind::Secondary,
                    44.0,
                )
                .clicked()
                {
                    v.state.pin_form = Some(form(true));
                }
            },
        );
    });
}

/// The form filled in from a saved source, to edit it.
pub fn edit_form(cfg: &SourceConfig) -> SourceForm {
    let mut f = SourceForm {
        editing: Some(cfg.clone()),
        kind: cfg.kind(),
        name: cfg.name().to_string(),
        use_quick_connect: false,
        ..SourceForm::default()
    };
    let mut creds = |c: &Option<Credentials>| {
        if let Some(c) = c {
            f.username = c.username.clone();
            f.password = c.password.clone();
        }
    };
    match cfg {
        SourceConfig::Local(c) => f.address = c.root.display().to_string(),
        SourceConfig::Library(c) => f.address = library_address(c),
        SourceConfig::Http(c) | SourceConfig::WebDav(c) => {
            creds(&c.credentials);
            f.address = c.url.clone();
            f.insecure_tls = c.insecure_tls;
            f.parallel_downloads = c.parallel_downloads;
        }
        SourceConfig::Dlna(c) => f.address = c.location.clone(),
        SourceConfig::Plex(c) => {
            f.address = c.url.clone();
            f.insecure_tls = c.insecure_tls;
            f.parallel_downloads = c.parallel_downloads;
            f.plex.manual = c.auth.is_none();
            f.plex.token = c.access_token.clone();
            f.plex.connection = super::plex::ConnectionMode::from_config(c);
            f.plex.selected = Some(c.clone());
            f.plex.state = super::plex::LinkState::Connected;
        }
        SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => {
            creds(&c.credentials);
            f.address = c.url.clone();
            f.insecure_tls = c.insecure_tls;
            f.parallel_downloads = c.parallel_downloads;
        }
        SourceConfig::Smb(c) => {
            creds(&c.credentials);
            f.address = fp_sources::smb::smb_url(&c.host, c.port, &c.share, &c.path);
        }
        SourceConfig::MediaBrowser(c) => {
            creds(&c.credentials);
            f.address = c.url.clone();
            f.insecure_tls = c.insecure_tls;
            f.parallel_downloads = c.parallel_downloads;
            // Signed in with Quick Connect: kept until cleared.
            if c.credentials.is_none() {
                f.use_quick_connect = true;
                if let Some(token) = &c.access_token {
                    f.quick_connect = QuickConnect::Authorized {
                        token: token.clone(),
                        user_id: c.user_id.clone(),
                    };
                }
            }
        }
    }
    f
}

/// A library's folder as its form shows it: the path, or while its card is
/// out, the card's name and the folder on it.
fn library_address(c: &fp_sources::LibraryConfig) -> String {
    match fp_core::volume::resolve(&c.root()) {
        Some(path) => path.into_owned(),
        None => SourceConfig::Library(c.clone()).address(),
    }
}

/// What the form doesn't show, kept from the source being edited.
fn keep_hidden(mut new: SourceConfig, old: &SourceConfig) -> SourceConfig {
    match (&mut new, old) {
        (SourceConfig::DeoVr(n), SourceConfig::DeoVr(o))
        | (SourceConfig::HereSphere(n), SourceConfig::HereSphere(o)) => n.max_height = o.max_height,
        (SourceConfig::MediaBrowser(n), SourceConfig::MediaBrowser(o)) => {
            n.max_height = o.max_height;
        }
        _ => {}
    }
    new
}

/// Builds a source configuration from the form, validating the address.
pub fn build_config(f: &SourceForm, id: String) -> Result<SourceConfig, String> {
    if f.kind == SourceKind::Plex {
        return f
            .plex
            .selected
            .clone()
            .map(SourceConfig::Plex)
            .ok_or_else(|| tr!("plex-test-required").into());
    }
    let address = f.address.trim().to_string();
    if address.is_empty() {
        return Err(tr!("source-error-no-address").into());
    }
    let credentials = (!f.username.trim().is_empty())
        .then(|| Credentials::new(f.username.trim(), f.password.clone()));
    let default_name = |a: &str| {
        let host = a.split("://").nth(1).unwrap_or(a);
        host.split(['/', ':']).next().unwrap_or(host).to_string()
    };
    let name = if f.name.trim().is_empty() {
        default_name(&address)
    } else {
        f.name.trim().to_string()
    };
    let http_url = |a: &str| -> Result<String, String> {
        let url = urlutil::with_http_scheme(a.trim_end_matches('/'));
        if urlutil::is_http_url(&url) {
            Ok(url)
        } else {
            Err(tr!("source-error-http").into())
        }
    };
    Ok(match f.kind {
        SourceKind::Local | SourceKind::Library => {
            let path = std::path::Path::new(&address);
            let name = if f.name.trim().is_empty() {
                crate::libraries::default_name(path)
            } else {
                f.name.trim().to_string()
            };
            // Unchanged: kept as it is, also while its card is out.
            if let Some(SourceConfig::Library(old)) = &f.editing
                && address == library_address(old)
            {
                return Ok(SourceConfig::Library(fp_sources::LibraryConfig {
                    name,
                    ..old.clone()
                }));
            }
            SourceConfig::Library(crate::libraries::config_for(id, name, path)?)
        }
        SourceKind::Http | SourceKind::WebDav => {
            let c = fp_sources::HttpConfig {
                id,
                name,
                url: http_url(&address)?,
                credentials,
                insecure_tls: f.insecure_tls,
                parallel_downloads: f.parallel_downloads,
            };
            if f.kind == SourceKind::Http {
                SourceConfig::Http(c)
            } else {
                SourceConfig::WebDav(c)
            }
        }
        SourceKind::DeoVr | SourceKind::HereSphere => {
            let c = fp_sources::FeedConfig {
                id,
                name,
                url: http_url(&address)?,
                credentials,
                insecure_tls: f.insecure_tls,
                parallel_downloads: f.parallel_downloads,
                max_height: None,
            };
            if f.kind == SourceKind::DeoVr {
                SourceConfig::DeoVr(c)
            } else {
                SourceConfig::HereSphere(c)
            }
        }
        SourceKind::Smb => {
            let rest = address
                .strip_prefix("smb://")
                .unwrap_or(&address)
                .trim_start_matches(['/', '\\'])
                .replace('\\', "/");
            let mut parts = rest.splitn(3, '/');
            let hostport = parts.next().unwrap_or_default().to_string();
            let share = parts.next().unwrap_or_default().to_string();
            let path = parts
                .next()
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string();
            // No share: the whole server, which lists its shares.
            if hostport.is_empty() {
                return Err(tr!("source-error-smb").into());
            }
            let (host, port) = match hostport.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_string(),
                    Some(
                        p.parse::<u16>()
                            .map_err(|_| tr!("source-error-port").to_string())?,
                    ),
                ),
                None => (hostport, None),
            };
            SourceConfig::Smb(fp_sources::SmbConfig {
                id,
                name,
                host,
                port,
                share,
                path,
                credentials,
            })
        }
        SourceKind::MediaBrowser => {
            let (credentials, access_token, user_id) = if f.use_quick_connect {
                match &f.quick_connect {
                    QuickConnect::Authorized { token, user_id } => {
                        (None, Some(token.clone()), user_id.clone())
                    }
                    QuickConnect::Waiting { .. } => {
                        return Err(tr!("source-error-approve-code").into());
                    }
                    _ => return Err(tr!("source-error-get-code").into()),
                }
            } else if credentials.is_none() {
                return Err(tr!("source-error-credentials").into());
            } else {
                (credentials, None, None)
            };
            SourceConfig::MediaBrowser(fp_sources::MediaBrowserConfig {
                id,
                name,
                url: http_url(&address)?,
                credentials,
                insecure_tls: f.insecure_tls,
                parallel_downloads: f.parallel_downloads,
                max_height: None,
                access_token,
                user_id,
            })
        }
        SourceKind::Plex => return Err(tr!("plex-test-required").into()),
        SourceKind::Dlna => {
            let Some(SourceConfig::Dlna(old)) = &f.editing else {
                return Err(tr!("source-error-pick-server").into());
            };
            // A device description is a resource URL, not a server root;
            // keep its trailing slash and query exactly as entered.
            let location = urlutil::with_http_scheme(&address);
            if !urlutil::is_http_url(&location) || urlutil::split_credentials(&location).is_err() {
                return Err(tr!("source-error-http").into());
            }
            let mut config = old.clone();
            config.name = name;
            if location != old.location {
                config.location = location;
                config.control_url = None;
                config.service_type = None;
            }
            SourceConfig::Dlna(config)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_dlna_keeps_identity_and_refreshes_control_after_an_address_change() {
        let saved = fp_sources::DlnaConfig {
            id: "living-room".into(),
            name: "Living room".into(),
            location: "http://old.example:8200/description/".into(),
            control_url: Some("http://old.example:8200/control/content".into()),
            service_type: Some("urn:schemas-upnp-org:service:ContentDirectory:1".into()),
        };
        let mut form = edit_form(&SourceConfig::Dlna(saved.clone()));
        assert_eq!(form.address, saved.location);
        form.name = "  Media server  ".into();
        let renamed = build_config(&form, "unused-new-id".into()).unwrap();
        assert_eq!(
            renamed,
            SourceConfig::Dlna(fp_sources::DlnaConfig {
                name: "Media server".into(),
                ..saved.clone()
            })
        );

        form.address = " https://new.example:8443/device.xml?instance=2 ".into();
        assert_eq!(
            build_config(&form, "unused-new-id".into()).unwrap(),
            SourceConfig::Dlna(fp_sources::DlnaConfig {
                id: saved.id.clone(),
                name: "Media server".into(),
                location: "https://new.example:8443/device.xml?instance=2".into(),
                control_url: None,
                service_type: None,
            })
        );
        for address in ["", "ftp://new.example/device.xml", "http://"] {
            form.address = address.into();
            assert!(build_config(&form, saved.id.clone()).is_err(), "{address}");
        }

        // Adding still goes through discovery rather than this edit form.
        form.address = "http://new.example/device.xml".into();
        form.editing = None;
        assert!(build_config(&form, "new-server".into()).is_err());
    }

    #[test]
    fn editing_a_linked_plex_source_keeps_code_sign_in_without_a_device_key() {
        let mut cfg = fp_sources::PlexConfig {
            id: "plex".into(),
            name: "Plex".into(),
            url: "https://server.plex.direct:32400".into(),
            machine_identifier: "machine".into(),
            access_token: "server-token".into(),
            connections: Vec::new(),
            auth: Some(fp_sources::plex::auth::PlexCredentials::manual(
                "account-token".into(),
                "linked-client".into(),
            )),
            insecure_tls: false,
            parallel_downloads: false,
            max_height: None,
            local_only: false,
            allow_relay: true,
        };
        let saved = SourceConfig::Plex(cfg.clone());
        let form = edit_form(&saved);
        assert!(!form.plex.manual);
        assert!(matches!(
            form.plex.state,
            super::super::plex::LinkState::Connected
        ));
        assert_eq!(form.plex.selected.as_ref(), Some(&cfg));
        assert_eq!(form.editing.as_ref(), Some(&saved));

        cfg.auth = None;
        assert!(edit_form(&SourceConfig::Plex(cfg)).plex.manual);
    }

    #[test]
    fn form_validation() {
        let mut f = SourceForm {
            kind: SourceKind::Smb,
            address: "smb://nas:1445/videos/vr".into(),
            ..Default::default()
        };
        match build_config(&f, "x".into()).unwrap() {
            SourceConfig::Smb(c) => {
                assert_eq!(
                    (c.host.as_str(), c.port, c.share.as_str(), c.path.as_str()),
                    ("nas", Some(1445), "videos", "vr")
                );
                assert_eq!(c.name, "nas");
            }
            other => panic!("{other:?}"),
        }
        // The whole server, by address or as Windows writes it.
        for address in ["smb://192.168.0.10/", "smb://192.168.0.10", "\\\\nas"] {
            f.address = address.into();
            match build_config(&f, "x".into()).unwrap() {
                SourceConfig::Smb(c) => {
                    assert!(c.share.is_empty() && c.path.is_empty(), "{address}: {c:?}");
                    assert!(!c.host.is_empty(), "{address}");
                }
                other => panic!("{other:?}"),
            }
        }
        f.address = "smb://".into();
        assert!(build_config(&f, "x".into()).is_err());
        f.kind = SourceKind::DeoVr;
        f.address = "192.168.1.5:9999/deovr".into();
        f.username = "me".into();
        match build_config(&f, "x".into()).unwrap() {
            SourceConfig::DeoVr(c) => {
                assert_eq!(c.url, "http://192.168.1.5:9999/deovr");
                assert!(c.credentials.is_some());
            }
            other => panic!("{other:?}"),
        }
        f.kind = SourceKind::Library;
        f.address = "relative/path".into();
        assert!(build_config(&f, "x".into()).is_err());
        let dir = std::env::temp_dir();
        f.address = dir.join("fpx-no-such-folder").display().to_string();
        assert!(build_config(&f, "x".into()).is_err(), "no such folder");
        f.address = dir.display().to_string();
        f.name.clear();
        match build_config(&f, "x".into()).unwrap() {
            SourceConfig::Library(c) => {
                assert_eq!((c.folder.as_path(), c.card), (dir.as_path(), None));
                assert_eq!(c.name, dir.file_name().unwrap().to_string_lossy());
            }
            other => panic!("{other:?}"),
        }

        // MediaBrowser validation
        f.kind = SourceKind::MediaBrowser;
        f.address = "192.168.1.10:8096/".into();
        f.username.clear();
        f.password.clear();
        f.quick_connect = QuickConnect::Idle;
        assert!(build_config(&f, "mb-1".into()).is_err());

        f.quick_connect = QuickConnect::Authorized {
            token: "tok_abc".into(),
            user_id: Some("user_123".into()),
        };
        match build_config(&f, "mb-1".into()).unwrap() {
            SourceConfig::MediaBrowser(c) => {
                assert_eq!(c.url, "http://192.168.1.10:8096");
                assert_eq!(c.name, "192.168.1.10");
                assert_eq!(c.access_token.as_deref(), Some("tok_abc"));
                assert_eq!(c.user_id.as_deref(), Some("user_123"));
                assert!(c.credentials.is_none());
            }
            other => panic!("{other:?}"),
        }

        // MediaBrowser with username/password
        f.use_quick_connect = false;
        f.quick_connect = QuickConnect::Idle;
        assert!(build_config(&f, "mb-2".into()).is_err());
        f.username = "alice".into();
        f.password = "hunter2".into();
        match build_config(&f, "mb-2".into()).unwrap() {
            SourceConfig::MediaBrowser(c) => {
                assert_eq!(c.url, "http://192.168.1.10:8096");
                assert!(c.credentials.is_some());
                assert!(c.access_token.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn editing_gives_back_what_was_saved() {
        use fp_sources::{FeedConfig, HttpConfig, MediaBrowserConfig, SmbConfig};
        let creds = Some(Credentials::new("bob", "pw"));
        let saved = [
            SourceConfig::Smb(SmbConfig {
                id: "s".into(),
                name: "NAS".into(),
                host: "nas".into(),
                port: Some(1445),
                share: "videos".into(),
                path: "vr".into(),
                credentials: creds.clone(),
            }),
            SourceConfig::WebDav(HttpConfig {
                id: "w".into(),
                name: "Cloud".into(),
                url: "https://nas/dav".into(),
                credentials: creds.clone(),
                insecure_tls: true,
                parallel_downloads: false,
            }),
            SourceConfig::DeoVr(FeedConfig {
                id: "d".into(),
                name: "Feed".into(),
                url: "http://x:9999/deovr".into(),
                credentials: None,
                insecure_tls: false,
                parallel_downloads: false,
                max_height: Some(2160),
            }),
            SourceConfig::MediaBrowser(MediaBrowserConfig {
                id: "j".into(),
                name: "Jellyfin".into(),
                url: "http://jf:8096".into(),
                credentials: None,
                insecure_tls: false,
                parallel_downloads: false,
                max_height: None,
                access_token: Some("tok".into()),
                user_id: Some("u".into()),
            }),
            SourceConfig::Library(fp_sources::LibraryConfig {
                id: "l".into(),
                name: "Private".into(),
                folder: "/home/deck/Private".into(),
                card: None,
            }),
            // On a card that is out.
            SourceConfig::Library(fp_sources::LibraryConfig {
                id: "c".into(),
                name: "Clips".into(),
                folder: "Clips".into(),
                card: Some(fp_sources::CardRef {
                    id: "0000-0000".into(),
                    label: "SDCARD".into(),
                }),
            }),
        ];
        for c in saved {
            let f = edit_form(&c);
            assert_eq!(f.editing.as_ref(), Some(&c));
            let back = build_config(&f, c.id().into()).map(|n| keep_hidden(n, &c));
            assert_eq!(back.as_ref(), Ok(&c), "{c:?}");
        }
    }
}
