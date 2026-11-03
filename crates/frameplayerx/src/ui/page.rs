//! What every browsing page is made of: headers, search fields, sort bars,
//! picture cards in grids and rows, banners and empty states.

use super::icons;
use super::theme::{self, Weight};
use super::thumbs::{Thumbs, paint_cover_rounded};
use super::widgets::{self, ListArea, Tip};
use egui::{Align, Align2, Color32, Layout, RichText, Sense, Vec2};

/// Picture of a landscape card.
pub const LANDSCAPE: Vec2 = Vec2::new(256.0, 144.0);
/// Picture of a poster card.
pub const PORTRAIT: Vec2 = Vec2::new(168.0, 252.0);
/// Space under a card's picture for its title and facts.
pub const CARD_TEXT: f32 = 56.0;
/// Between cards.
pub const GAP: f32 = 20.0;
/// Room a pointed-at card grows into (its lift, ring and shadow), kept
/// inside scrolling areas so it isn't cut off at their edges.
const LIFT_ROOM: i8 = 16;

/// A page that scrolls down, reaching [`LIFT_ROOM`] past `ui`'s sides with
/// the same room inside: what is on it stays in line, and a card lifted at
/// its edge shows whole.
pub fn scroll<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    scroll_from(ui, false, add)
}

/// [`scroll`], back at the top when `top`: a list sorted or filtered anew
/// starts from its beginning.
pub fn scroll_from<R>(ui: &mut egui::Ui, top: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .outer_margin(egui::Margin {
            left: -LIFT_ROOM,
            right: -LIFT_ROOM,
            ..Default::default()
        })
        .show(ui, |ui| {
            let mut area = egui::ScrollArea::vertical().auto_shrink(false);
            if top {
                area = area.vertical_scroll_offset(0.0);
            }
            area.show_list(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(LIFT_ROOM, 0))
                    .show(ui, add)
                    .inner
            })
            .inner
        })
        .inner
}

/// A page title with a back arrow and a line under it, actions on the
/// right; true when the arrow was clicked.
pub fn header(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: Option<&str>,
    actions: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut back = false;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        back = widgets::icon_button(ui, icons::ARROW_LEFT, 48.0, false)
            .tip(tr!("common-back"))
            .clicked();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(
                RichText::new(title)
                    .font(theme::font(Weight::Bold, 26.0))
                    .color(theme::TEXT),
            );
            if let Some(s) = subtitle.filter(|s| !s.is_empty()) {
                ui.label(
                    RichText::new(s)
                        .font(theme::font(Weight::Regular, 15.0))
                        .color(theme::TEXT_3),
                );
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            actions(ui);
        });
    });
    ui.add_space(8.0);
    back
}

/// The search field: a sunken box with a magnifying glass; true when the
/// text changed.
pub fn search_field(ui: &mut egui::Ui, text: &mut String, hint: &str, width: f32) -> bool {
    let mut changed = false;
    let slot = ui.painter().add(egui::Shape::Noop);
    let field = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(14, 0))
        .show(ui, |ui| {
            ui.set_height(44.0);
            // Left to right even among right-aligned actions.
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.label(
                    RichText::new(icons::MAGNIFYING_GLASS)
                        .font(theme::icon(20.0))
                        .color(theme::TEXT_2),
                );
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(hint)
                        .frame(false)
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(width),
                );
                changed |= r.changed();
                if !text.is_empty()
                    && widgets::icon_button(ui, icons::X, 30.0, false)
                        .tip(tr!("common-clear"))
                        .clicked()
                {
                    text.clear();
                    changed = true;
                }
            });
        });
    let r = field.response.rect;
    ui.painter().set(
        slot,
        egui::Shape::Vec(super::depth::sunken_shapes(r, 12.0, widgets::WELL)),
    );
    changed
}

/// The orders a list offers as ghost buttons, the chosen one underlined;
/// true when it changed. Lay it out in a horizontal ui.
pub fn sort_bar<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    items: &[(T, &str)],
    selected: &mut T,
) -> bool {
    let mut changed = false;
    ui.spacing_mut().item_spacing.x = 2.0;
    ui.label(
        RichText::new(icons::SORT_ASCENDING)
            .font(theme::icon(20.0))
            .color(theme::TEXT_3),
    );
    ui.add_space(4.0);
    for (s, label) in items {
        let r = widgets::button_sized(ui, None, label, widgets::Kind::Ghost, 38.0);
        if *selected == *s {
            let bar = egui::Rect::from_center_size(
                egui::pos2(r.rect.center().x, r.rect.bottom() - 2.0),
                Vec2::new(r.rect.width() - 28.0, 2.5),
            );
            ui.painter()
                .rect_filled(bar, egui::CornerRadius::same(1), theme::ACCENT);
        }
        if r.clicked() && *selected != *s {
            *selected = *s;
            changed = true;
        }
    }
    changed
}

/// Which way a list is sorted: a caret that flips it, its tip saying the
/// way it goes (`tips`: largest first, smallest first); true when flipped.
pub fn direction(ui: &mut egui::Ui, descending: &mut bool, tips: [&str; 2]) -> bool {
    let (icon, tip) = if *descending {
        (icons::CARET_DOWN, tips[0])
    } else {
        (icons::CARET_UP, tips[1])
    };
    let flipped = widgets::icon_button(ui, icon, 38.0, false)
        .tip(tip)
        .clicked();
    if flipped {
        *descending = !*descending;
    }
    flipped
}

/// Shuffle again, beside a shuffled list's sorts: true for a new order.
pub fn shuffle_again(ui: &mut egui::Ui) -> bool {
    ui.add_space(8.0);
    widgets::button_sized(
        ui,
        Some(icons::SHUFFLE),
        tr!("sort-shuffle-again"),
        widgets::Kind::Secondary,
        36.0,
    )
    .clicked()
}

/// Height of the browse panel's navigation, over the page.
pub const NAV_HEIGHT: f32 = 76.0;
/// Width of the filters drawer.
const DRAWER_WIDTH: f32 = 470.0;
/// Corners of the browse panel's body (see [`widgets::panel_slab`]).
const PANEL_RADIUS: u8 = 28;

/// The Filters button, lit and counting the filters that are on.
pub fn filters_button(ui: &mut egui::Ui, on: usize) -> egui::Response {
    let label = if on == 0 {
        tr!("filters").to_string()
    } else {
        format!("{}  {on}", tr!("filters"))
    };
    widgets::chip_icon(ui, Some(icons::SLIDERS_HORIZONTAL), &label, on > 0)
}

/// What a press on the filters that are on asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterEdit {
    /// Take the filter at this place off.
    Remove(usize),
    Clear,
}

/// The filters that are on, a chip each under the sorts (pressing one takes
/// it off), and Clear all; nothing while none is.
pub fn active_filters(ui: &mut egui::Ui, labels: &[String]) -> Option<FilterEdit> {
    if labels.is_empty() {
        return None;
    }
    let mut edit = None;
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(8.0);
        for (i, label) in labels.iter().enumerate() {
            if widgets::chip(ui, &format!("{label}  {}", icons::X), true).clicked() {
                edit = Some(FilterEdit::Remove(i));
            }
        }
        if widgets::button_sized(
            ui,
            None,
            tr!("filters-clear-all"),
            widgets::Kind::Ghost,
            40.0,
        )
        .clicked()
        {
            edit = Some(FilterEdit::Clear);
        }
    });
    edit
}

/// The filters drawer, over the right of the browse panel with the page
/// dimmed beside it; a press there, ✕ or Done closes it. `count` says how
/// many the filters keep, `add` draws its sections ([`filter_section`]).
/// True when Clear all was pressed.
pub fn filter_drawer(
    ui: &egui::Ui,
    open: &mut bool,
    count: &str,
    add: impl FnOnce(&mut egui::Ui),
) -> bool {
    let ctx = ui.ctx().clone();
    let body = ctx.screen_rect() - widgets::SHADOW_ROOM;
    let page = egui::Rect::from_min_max(egui::pos2(body.left(), body.top() + NAV_HEIGHT), body.max);
    let drawer = egui::Rect::from_min_max(
        egui::pos2(body.right() - DRAWER_WIDTH, page.top()),
        body.max,
    );
    egui::Area::new(egui::Id::new("filters-scrim"))
        .order(egui::Order::Middle)
        .fixed_pos(page.min)
        .show(&ctx, |ui| {
            let (r, resp) = ui.allocate_exact_size(page.size(), Sense::click());
            let corners = egui::CornerRadius {
                sw: PANEL_RADIUS,
                se: PANEL_RADIUS,
                ..Default::default()
            };
            ui.painter()
                .rect_filled(r, corners, Color32::from_black_alpha(130));
            if resp.clicked() {
                *open = false;
            }
        });
    let mut clear = false;
    egui::Area::new(egui::Id::new("filters"))
        .order(egui::Order::Foreground)
        .fixed_pos(drawer.min)
        .show(&ctx, |ui| {
            // Its parts are placed, not laid out: the area claims the whole
            // drawer, so the thumbstick's scrolling comes to it, not to the
            // dimmed page under it.
            ui.set_min_size(drawer.size());
            let corners = egui::CornerRadius {
                se: PANEL_RADIUS,
                ..Default::default()
            };
            let p = ui.painter();
            p.add(
                egui::epaint::Shadow {
                    offset: [-10, 0],
                    blur: 30,
                    spread: 0,
                    color: Color32::from_black_alpha(150),
                }
                .as_shape(drawer, corners),
            );
            p.rect_filled(drawer, corners, theme::SURFACE);
            let inner = drawer.shrink2(Vec2::new(24.0, 18.0));
            let title = egui::Rect::from_min_size(inner.min, Vec2::new(inner.width(), 48.0));
            let footer = egui::Rect::from_min_max(
                egui::pos2(inner.left(), inner.bottom() - 52.0),
                inner.max,
            );
            let sections = egui::Rect::from_min_max(
                egui::pos2(inner.left(), title.bottom() + 6.0),
                egui::pos2(inner.right(), footer.top() - 10.0),
            );
            p.hline(
                drawer.left()..=drawer.right(),
                footer.top() - 5.0,
                egui::Stroke::new(1.0_f32, theme::STROKE),
            );
            let mut part = |rect: egui::Rect| {
                ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(Layout::top_down(Align::Min)),
                )
            };
            part(title).horizontal(|ui| {
                ui.label(
                    RichText::new(tr!("filters"))
                        .font(theme::font(Weight::Bold, 24.0))
                        .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icons::X, 40.0, false)
                        .tip(tr!("player-close"))
                        .clicked()
                    {
                        *open = false;
                    }
                    if widgets::button_sized(
                        ui,
                        None,
                        tr!("filters-clear-all"),
                        widgets::Kind::Ghost,
                        40.0,
                    )
                    .clicked()
                    {
                        clear = true;
                    }
                });
            });
            egui::ScrollArea::vertical()
                .id_salt("filters")
                .max_height(sections.height())
                .auto_shrink([false, false])
                .show_list(&mut part(sections), |ui| {
                    ui.set_width(ui.available_width());
                    add(ui);
                    ui.add_space(12.0);
                });
            part(footer).horizontal(|ui| {
                ui.set_height(footer.height());
                ui.label(
                    RichText::new(count)
                        .font(theme::font(Weight::Medium, 16.0))
                        .color(theme::TEXT_2),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::button_sized(
                        ui,
                        None,
                        tr!("common-done"),
                        widgets::Kind::Primary,
                        44.0,
                    )
                    .clicked()
                    {
                        *open = false;
                    }
                });
            });
        });
    clear
}

/// A section of the filters drawer: its title over its choices, which wrap.
pub fn filter_section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(14.0);
    widgets::section_label(ui, title);
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(8.0);
        add(ui);
    });
}

/// A list section of the filters drawer as narrowed: what its box says,
/// and whether all of its choices show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListState {
    pub query: String,
    pub all: bool,
}

/// Sections with more choices than this get a box, and show [`FEW`] of them
/// until Show all, so the drawer's other sections stay near.
const LONG_LIST: usize = 12;
const FEW: usize = 8;

/// What a list section shows of its choices.
#[derive(Debug, PartialEq, Eq)]
pub struct ListView {
    /// Places of the choices shown, in their order.
    pub shown: Vec<usize>,
    /// Matching ones left out until Show all.
    pub hidden: usize,
    /// All show, and Show fewer would leave some out.
    pub can_fold: bool,
}

/// Which of `names` a list section shows: those matching its box (a long
/// list's), the chosen always, and at most [`FEW`] others until Show all.
pub fn list_view(names: &[&str], chosen: impl Fn(&str) -> bool, state: &ListState) -> ListView {
    let long = names.len() > LONG_LIST;
    let text = if long {
        state.query.trim().to_lowercase()
    } else {
        String::new()
    };
    let (mut shown, mut hidden, mut others) = (Vec::new(), 0, 0);
    for (i, name) in names.iter().enumerate() {
        let on = chosen(name);
        if !on && !text.is_empty() && !name.to_lowercase().contains(&text) {
            continue;
        }
        if on {
            shown.push(i);
        } else if !long || state.all || others < FEW {
            others += 1;
            shown.push(i);
        } else {
            hidden += 1;
        }
    }
    ListView {
        shown,
        hidden,
        can_fold: long && state.all && others > FEW,
    }
}

/// A section of the filters drawer listing `choices`, each a name and how
/// many have it when that is known. A long one has a box (`hint`) and shows
/// a few, those chosen kept in sight, with Show all. Returns the choice
/// pressed.
pub fn filter_list<'a>(
    ui: &mut egui::Ui,
    title: &str,
    hint: &str,
    choices: &[(&'a str, Option<usize>)],
    chosen: impl Fn(&str) -> bool,
    state: &mut ListState,
) -> Option<&'a str> {
    let names: Vec<&str> = choices.iter().map(|c| c.0).collect();
    ui.add_space(14.0);
    widgets::section_label(ui, title);
    ui.add_space(2.0);
    if names.len() > LONG_LIST {
        search_field(ui, &mut state.query, hint, 300.0);
        ui.add_space(4.0);
    }
    let view = list_view(&names, &chosen, state);
    let mut pressed = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(8.0);
        for &i in &view.shown {
            let (name, count) = choices[i];
            if filter_chip(ui, name, count, chosen(name)) {
                pressed = Some(name);
            }
        }
        let more = |ui: &mut egui::Ui, label: &str| {
            widgets::button_sized(ui, None, label, widgets::Kind::Ghost, 40.0).clicked()
        };
        if view.hidden > 0 {
            let n = view.shown.len() + view.hidden;
            let all = tr!("filters-show-all", count = n);
            if more(ui, &all) {
                state.all = true;
            }
        } else if view.can_fold && more(ui, tr!("filters-show-fewer")) {
            state.all = false;
        }
    });
    pressed
}

/// A choice in the filters drawer, with how many it keeps when that is
/// known; true when pressed.
pub fn filter_chip(ui: &mut egui::Ui, label: &str, count: Option<usize>, on: bool) -> bool {
    let text = match count {
        Some(n) => format!("{label}  {n}"),
        None => label.to_string(),
    };
    widgets::chip(ui, &text, on).clicked()
}

/// Under a list that has more to show: true once it comes into view, to
/// load the next lot; a spinner while that is on its way (`loading`).
pub fn load_more(ui: &mut egui::Ui, loading: bool) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 64.0), Sense::hover());
    if loading {
        let r = egui::Rect::from_center_size(rect.center(), Vec2::splat(28.0));
        ui.put(r, egui::Spinner::new().size(28.0));
        return false;
    }
    let seen = ui.is_rect_visible(rect);
    if seen {
        // The next lot is drawn on the next pass, and may want another.
        ui.ctx().request_repaint();
    }
    seen
}

/// Quiet text at the right end of a horizontal ui ("12 videos").
pub fn count_label(ui: &mut egui::Ui, text: &str) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        // Clear of a scroll bar along the edge.
        ui.add_space(10.0);
        ui.label(
            RichText::new(text)
                .font(theme::font(Weight::Medium, 15.0))
                .color(theme::TEXT_3),
        );
    });
}

/// "1 video", "12 videos".
/// A title over a row or grid, with a count beside it.
pub fn section_title(ui: &mut egui::Ui, title: &str, count: Option<usize>) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .font(theme::font(Weight::SemiBold, 22.0))
                .color(theme::TEXT),
        );
        if let Some(n) = count {
            ui.label(
                RichText::new(n.to_string())
                    .font(theme::font(Weight::Medium, 16.0))
                    .color(theme::TEXT_3),
            );
        }
    });
    ui.add_space(6.0);
}

/// A centred message with an icon and actions, for empty screens.
pub fn empty_state(
    ui: &mut egui::Ui,
    icon: &str,
    title: &str,
    body: &str,
    add_actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(70.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 48.0, theme::SURFACE_2);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(44.0),
            theme::ACCENT_HOVER,
        );
        ui.add_space(14.0);
        ui.label(
            RichText::new(title)
                .font(theme::font(Weight::Bold, 30.0))
                .color(theme::TEXT),
        );
        ui.add_space(4.0);
        ui.set_max_width(640.0);
        ui.label(RichText::new(body).color(theme::TEXT_2));
        ui.add_space(18.0);
        add_actions(ui);
    });
}

/// A spinner and a word while a page loads, or what went wrong.
pub fn status(ui: &mut egui::Ui, loading: bool, error: Option<&str>) {
    if loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(tr!("common-loading")).color(theme::TEXT_2));
        });
    }
    if let Some(e) = error {
        ui.label(RichText::new(format!("{}  {e}", icons::WARNING_CIRCLE)).color(theme::ERROR));
    }
}

/// Paints a picture (or a placeholder icon) into `rect`, a little above
/// the page.
pub fn picture(
    ui: &egui::Ui,
    thumbs: &mut Thumbs,
    key: Option<&str>,
    rect: egui::Rect,
    radius: u8,
    placeholder: &str,
) {
    picture_tinted(ui, thumbs, key, rect, radius, placeholder, theme::TEXT_3);
}

/// [`picture`] with the placeholder icon in `tint`.
pub fn picture_tinted(
    ui: &egui::Ui,
    thumbs: &mut Thumbs,
    key: Option<&str>,
    rect: egui::Rect,
    radius: u8,
    placeholder: &str,
    tint: Color32,
) {
    let painter = ui.painter();
    painter.add(
        egui::Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(130),
        }
        .as_shape(rect, egui::CornerRadius::same(radius)),
    );
    match key.and_then(|k| thumbs.get(k)) {
        Some(tex) => paint_cover_rounded(painter, rect, &tex, radius),
        None => {
            painter.rect_filled(rect, egui::CornerRadius::same(radius), theme::SURFACE_2);
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                placeholder,
                theme::icon((rect.height() * 0.28).clamp(24.0, 56.0)),
                tint,
            );
        }
    }
}

/// A picture card with a title and a line of facts under it.
pub struct Card<'a> {
    /// Size of the picture.
    pub size: Vec2,
    pub thumb: Option<&'a str>,
    /// Icon shown while there is no picture, and its colour.
    pub placeholder: &'static str,
    pub tint: Color32,
    pub title: &'a str,
    pub facts: &'a str,
    /// Text on the top-left corner of the picture (the format).
    pub badge: Option<&'a str>,
    /// Icon on the top-right corner (haptics, a lock).
    pub corner: Option<&'static str>,
    /// Watched fraction, drawn as a bar along the bottom.
    pub progress: Option<f32>,
    /// A check on the top-right corner.
    pub watched: bool,
    /// A heart before the title.
    pub favorite: bool,
    /// Greyed title (a missing file).
    pub dim: bool,
    /// Details: its title and facts open them, a "…" beside the title
    /// says so.
    pub more: bool,
    /// A play mark over the picture while pointed at.
    pub play: bool,
    /// A round button on the picture's top-right corner: icon and tip.
    pub action: Option<(&'static str, &'static str)>,
    /// Show [`Card::action`] all the time, not only while pointed at.
    pub action_always: bool,
}

impl Default for Card<'_> {
    fn default() -> Self {
        Card {
            size: LANDSCAPE,
            thumb: None,
            placeholder: icons::FILM_STRIP,
            tint: theme::TEXT_3,
            title: "",
            facts: "",
            badge: None,
            corner: None,
            progress: None,
            watched: false,
            favorite: false,
            dim: false,
            more: false,
            play: true,
            action: None,
            action_always: false,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct CardResponse {
    /// The card was clicked.
    pub open: bool,
    /// Its "…" was clicked.
    pub more: bool,
    /// Its corner button ([`Card::action`]) was clicked.
    pub action: bool,
    /// It is on screen.
    pub visible: bool,
}

/// A picture card: lifts when pointed at; see [`Card`].
pub fn card(ui: &mut egui::Ui, thumbs: &mut Thumbs, c: &Card) -> CardResponse {
    let (rect, resp) = ui.allocate_exact_size(c.size + Vec2::new(0.0, CARD_TEXT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return CardResponse::default();
    }
    let base = egui::Rect::from_min_size(rect.min, c.size);
    // With details, the title and facts open them; the picture plays.
    let text_area = egui::Rect::from_min_max(egui::pos2(rect.left(), base.bottom()), rect.max);
    let on_text = c.more && ui.rect_contains_pointer(text_area);
    // Pointed at, also while over one of its buttons.
    let pointed = ui.rect_contains_pointer(rect) && !on_text;
    let t = ui.ctx().animate_bool_with_time(resp.id, pointed, 0.14);
    // Lift: grows a little, casts a shadow, gains a ring.
    let img = egui::Rect::from_center_size(
        base.center() - Vec2::new(0.0, 3.0 * t),
        c.size * (1.0 + 0.04 * t),
    );
    widgets::shadow(ui.painter(), img, 12, t);
    picture_tinted(ui, thumbs, c.thumb, img, 12, c.placeholder, c.tint);
    let painter = ui.painter();
    if let Some(b) = c.badge {
        widgets::badge(
            painter,
            b,
            img.left_top() + Vec2::new(10.0, 10.0),
            Align2::LEFT_TOP,
        );
    }
    let action = c.action.filter(|_| c.action_always || pointed);
    if action.is_some() {
        // The button below takes the corner.
    } else if let Some(icon) = c.corner {
        widgets::badge(
            painter,
            icon,
            img.right_top() + Vec2::new(-10.0, 10.0),
            Align2::RIGHT_TOP,
        );
    } else if c.watched {
        let center = img.right_top() + Vec2::new(-22.0, 22.0);
        painter.circle_filled(center, 14.0, Color32::from_black_alpha(170));
        painter.text(
            center,
            Align2::CENTER_CENTER,
            icons::CHECK_CIRCLE,
            theme::icon_fill(24.0),
            theme::ACCENT_HOVER,
        );
    }
    if let Some(p) = c.progress.filter(|p| *p > 0.005) {
        let bar = egui::Rect::from_min_max(
            egui::pos2(img.left() + 12.0, img.bottom() - 10.0),
            egui::pos2(img.right() - 12.0, img.bottom() - 6.0),
        );
        painter.rect_filled(
            bar,
            egui::CornerRadius::same(2),
            Color32::from_black_alpha(140),
        );
        let mut done = bar;
        done.set_width(bar.width() * p.min(1.0));
        painter.rect_filled(done, egui::CornerRadius::same(2), theme::ACCENT);
    }
    if t > 0.01 {
        painter.rect_stroke(
            img,
            egui::CornerRadius::same(12),
            egui::Stroke::new(2.5_f32, Color32::from_white_alpha((t * 230.0) as u8)),
            egui::StrokeKind::Outside,
        );
        if c.play {
            painter.circle_filled(
                img.center(),
                28.0,
                Color32::from_black_alpha((t * 150.0) as u8),
            );
            widgets::play_mark(
                painter,
                img.center(),
                24.0,
                Color32::from_white_alpha((t * 255.0) as u8),
            );
        }
    }

    // Title and facts.
    let text_top = base.bottom() + 10.0;
    let title_color = if c.dim { theme::TEXT_3 } else { theme::TEXT };
    let mut title = c.title.to_string();
    if c.favorite {
        title = format!("{}  {title}", icons::HEART);
    }
    let room = rect.width() - if c.more { 40.0 } else { 4.0 };
    let line = |text: String, font: egui::FontId, color: Color32| {
        let mut job =
            egui::text::LayoutJob::single_section(text, egui::TextFormat::simple(font, color));
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        ui.fonts(|f| f.layout_job(job))
    };
    let title = line(title, theme::font(Weight::SemiBold, 16.0), title_color);
    let cut = title.elided.then_some(c.title);
    let facts = line(
        c.facts.to_string(),
        theme::font(Weight::Regular, 14.0),
        theme::TEXT_2,
    );
    let painter = ui.painter();
    painter.galley(egui::pos2(rect.left() + 2.0, text_top), title, title_color);
    painter.galley(
        egui::pos2(rect.left() + 2.0, text_top + 24.0),
        facts,
        theme::TEXT_2,
    );
    let mut more_clicked = false;
    if c.more {
        let more_resp = ui.interact(text_area, resp.id.with("more"), Sense::click());
        let m = ui
            .ctx()
            .animate_bool_with_time(more_resp.id, more_resp.hovered(), 0.12);
        let painter = ui.painter();
        painter.rect_filled(
            text_area.expand2(Vec2::new(4.0, 0.0)),
            egui::CornerRadius::same(10),
            Color32::from_white_alpha((m * 14.0) as u8),
        );
        painter.text(
            egui::pos2(rect.right() - 18.0, text_top + 14.0),
            Align2::CENTER_CENTER,
            icons::DOTS_THREE,
            theme::icon(24.0),
            theme::TEXT_2.lerp_to_gamma(Color32::WHITE, m.max(t * 0.6)),
        );
        more_clicked = match cut {
            Some(title) => more_resp.tip(&format!("{title}\n{}", tr!("jf-details"))),
            None => more_resp.tip(tr!("jf-details")),
        }
        .clicked();
    } else if let Some(title) = cut {
        ui.interact(text_area, resp.id.with("title"), Sense::hover())
            .tip(title);
    }
    let mut action_clicked = false;
    if let Some((icon, tip)) = action {
        let r = egui::Rect::from_center_size(
            img.right_top() + Vec2::new(-26.0, 26.0),
            Vec2::splat(40.0),
        );
        let a = ui.interact(r, resp.id.with("action"), Sense::click());
        let h = ui.ctx().animate_bool_with_time(a.id, a.hovered(), 0.12);
        let painter = ui.painter();
        painter.circle_filled(
            r.center(),
            20.0,
            Color32::from_black_alpha(180).lerp_to_gamma(theme::ACCENT, h),
        );
        painter.text(
            r.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(22.0),
            Color32::WHITE,
        );
        action_clicked = a.tip(tip).clicked();
    }
    CardResponse {
        open: resp.clicked() && !more_clicked && !action_clicked,
        more: more_clicked,
        action: action_clicked,
        visible: true,
    }
}

/// Lays `n` items `width` wide out in rows that fill the width.
pub fn grid(ui: &mut egui::Ui, n: usize, width: f32, mut add: impl FnMut(&mut egui::Ui, usize)) {
    let per_row = ((ui.available_width() + GAP) / (width + GAP))
        .floor()
        .max(1.0) as usize;
    for start in (0..n).step_by(per_row) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for i in start..(start + per_row).min(n) {
                add(ui, i);
            }
        });
        ui.add_space(14.0);
    }
}

/// A titled row of `n` items that scrolls sideways; nothing when empty.
pub fn carousel(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    n: usize,
    mut add: impl FnMut(&mut egui::Ui, usize),
) {
    if n == 0 {
        return;
    }
    section_title(ui, title, count);
    // Room for a pointed-at card, as in `scroll`, and above it for its lift.
    egui::Frame::new()
        .outer_margin(egui::Margin {
            left: -LIFT_ROOM,
            right: -LIFT_ROOM,
            top: -8,
            bottom: 0,
        })
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt(title)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show_rail(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin {
                            left: LIFT_ROOM,
                            right: LIFT_ROOM,
                            top: 8,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = GAP;
                                for i in 0..n {
                                    add(ui, i);
                                }
                            });
                        });
                });
        });
    ui.add_space(18.0);
}

/// A wide picture across the page with `content` over its left side;
/// returns the click on the picture.
pub fn banner(
    ui: &mut egui::Ui,
    thumbs: &mut Thumbs,
    key: Option<&str>,
    height: f32,
    content: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let painter = ui.painter();
    match key.and_then(|k| thumbs.get_wide(k)) {
        Some(tex) => paint_cover_rounded(painter, rect, &tex, 18),
        None => {
            painter.rect_filled(rect, egui::CornerRadius::same(18), theme::SURFACE_2);
        }
    }
    // Scrims: the text side and the bottom.
    widgets::hgradient(
        painter,
        egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + rect.width() * 0.72, rect.bottom()),
        ),
        Color32::from_rgba_unmultiplied(13, 16, 21, 238),
        Color32::TRANSPARENT,
    );
    widgets::vgradient(
        painter,
        egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 120.0), rect.max),
        Color32::TRANSPARENT,
        Color32::from_rgba_unmultiplied(13, 16, 21, 190),
    );
    // Square scrim corners would show: redraw the rounded frame over them.
    painter.rect_stroke(
        rect.expand(4.0),
        egui::CornerRadius::same(22),
        egui::Stroke::new(8.0_f32, theme::BG),
        egui::StrokeKind::Inside,
    );
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(40.0, 30.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    inner.set_max_width(rect.width() * 0.58);
    content(&mut inner);
    resp
}

/// Small clickable pills ("Sci-Fi", "Jane Doe"); returns the one clicked.
pub fn pills(ui: &mut egui::Ui, items: &[String]) -> Option<String> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        for item in items {
            if widgets::chip(ui, item, false).clicked() {
                clicked = Some(item.clone());
            }
        }
    });
    clicked
}

/// A rating as a star and its value ("★ 4.5").
pub fn rating(value: f32) -> String {
    let v = format!("{value:.1}");
    format!("{}  {}", icons::STAR, v.trim_end_matches(".0"))
}

/// Text under a details title.
pub fn facts(ui: &mut egui::Ui, parts: &[String]) {
    let parts: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return;
    }
    ui.label(
        RichText::new(parts.join("  ·  "))
            .font(theme::font(Weight::Medium, 16.0))
            .color(theme::TEXT_2),
    );
}

/// A long description, wrapped.
pub fn prose(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::font(Weight::Regular, 16.0))
            .color(theme::TEXT_2),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_shows_a_few_and_what_is_chosen() {
        let names: Vec<String> = (0..20).map(|i| format!("Name {i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let none = |_: &str| false;
        let mut state = ListState::default();
        let v = list_view(&names, none, &state);
        assert_eq!(
            (v.shown, v.hidden, v.can_fold),
            ((0..8).collect(), 12, false)
        );
        // One chosen past the few stays in sight, in its place.
        let v = list_view(&names, |n| n == "Name 15", &state);
        assert_eq!(v.shown, [0, 1, 2, 3, 4, 5, 6, 7, 15]);
        assert_eq!(v.hidden, 11);
        // The box narrows; few matches need no Show all.
        state.query = "name 1".into();
        let v = list_view(&names, none, &state);
        assert_eq!(v.shown, [1, 10, 11, 12, 13, 14, 15, 16]);
        assert_eq!(v.hidden, 3);
        state.query = "NAME 19".into();
        assert_eq!(list_view(&names, none, &state).shown, [19]);
        // Show all, then Show fewer.
        state.query.clear();
        state.all = true;
        let v = list_view(&names, none, &state);
        assert_eq!((v.shown.len(), v.hidden, v.can_fold), (20, 0, true));
        // A short list shows whole, its box (there is none) ignored.
        let short = &names[..10];
        state = ListState {
            query: "zzz".into(),
            all: false,
        };
        assert_eq!(list_view(short, none, &state).shown.len(), 10);
    }

    #[test]
    fn words() {
        assert_eq!(rating(4.5), format!("{}  4.5", icons::STAR));
        assert_eq!(rating(8.0), format!("{}  8", icons::STAR));
    }
}

/// What a [`list_row`] was asked to do.
#[derive(Clone, Copy, Default)]
pub struct RowResponse {
    pub open: bool,
    pub up: bool,
    pub down: bool,
    pub remove: bool,
}

/// A video in an ordered list (Up next, a playlist): its picture, title and
/// facts, marked while it is the `current` one, with buttons on the right to
/// move it up or down and take it out (not the current one). A click
/// anywhere else opens it.
pub fn list_row(
    ui: &mut egui::Ui,
    thumbs: &mut Thumbs,
    thumb: Option<&str>,
    title: &str,
    facts: &str,
    current: bool,
    (first, last): (bool, bool),
) -> RowResponse {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 88.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return RowResponse::default();
    }
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let fill = if current {
        theme::SURFACE_2
    } else {
        theme::SURFACE.gamma_multiply(t)
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(14), fill);
    if current {
        let bar = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, 16.0), Vec2::new(4.0, 56.0));
        ui.painter()
            .rect_filled(bar, egui::CornerRadius::same(2), theme::ACCENT);
    }
    let pic = egui::Rect::from_min_size(rect.min + Vec2::new(14.0, 8.0), Vec2::new(128.0, 72.0));
    picture(ui, thumbs, thumb, pic, 10, icons::FILM_STRIP);
    // Buttons from the right: take out, down, up.
    let mut out = RowResponse::default();
    let mut x = rect.right() - 12.0;
    let mut button = |icon: &str, tip: &str, shown: bool, id: &str| -> bool {
        x -= 48.0;
        if !shown {
            return false;
        }
        let r =
            egui::Rect::from_center_size(egui::pos2(x + 24.0, rect.center().y), Vec2::splat(44.0));
        let b = ui.interact(r, resp.id.with(id), Sense::click());
        let h = ui.ctx().animate_bool_with_time(b.id, b.hovered(), 0.12);
        ui.painter().circle_filled(
            r.center(),
            22.0,
            Color32::from_white_alpha((10.0 + h * 20.0) as u8),
        );
        ui.painter().text(
            r.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(22.0),
            theme::TEXT_2.lerp_to_gamma(Color32::WHITE, h),
        );
        b.tip(tip).clicked()
    };
    out.remove = button(icons::X, "Take out", !current, "remove");
    out.down = button(icons::CARET_DOWN, "Move down", !last, "down");
    out.up = button(icons::CARET_UP, "Move up", !first, "up");
    let room = (x - pic.right() - 30.0).max(40.0);
    let line = |text: &str, font: egui::FontId, color: Color32| {
        let mut job = egui::text::LayoutJob::single_section(
            text.to_string(),
            egui::TextFormat::simple(font, color),
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        ui.fonts(|f| f.layout_job(job))
    };
    let title = line(title, theme::font(Weight::SemiBold, 18.0), theme::TEXT);
    let facts = line(facts, theme::font(Weight::Regular, 15.0), theme::TEXT_2);
    let left = pic.right() + 18.0;
    ui.painter()
        .galley(egui::pos2(left, rect.center().y - 24.0), title, theme::TEXT);
    ui.painter().galley(
        egui::pos2(left, rect.center().y + 3.0),
        facts,
        theme::TEXT_2,
    );
    out.open = resp.clicked() && !(out.up || out.down || out.remove);
    out
}
