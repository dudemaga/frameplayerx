//! Shared components: buttons, chips, switches, settings rows, tabs and
//! toasts, all in the theme's colors and type. Hover and press animate (a
//! pointer ray needs clear feedback), and every target is at least 44
//! points, a comfortable aim at arm's length.

use super::theme::{self, Weight};
use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    Ui, Vec2, text::LayoutJob,
};

const ANIM: f32 = 0.12;

/// 0 → 1 as `on` turns true, eased over [`ANIM`].
fn anim(ui: &Ui, id: egui::Id, on: bool) -> f32 {
    ui.ctx().animate_bool_with_time(id, on, ANIM)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

/// A one-word label over a control while the pointer is on it. egui's own
/// tooltips wait for the pointer to rest, which a controller ray never
/// quite does, so they never showed in the headset.
pub trait Tip {
    fn tip(self, word: &str) -> Self;
}

impl Tip for Response {
    fn tip(self, word: &str) -> Response {
        // A control that can't be used says why too.
        let over = self.hovered() || (!self.enabled() && self.contains_pointer());
        let t = self
            .ctx
            .animate_bool_with_time(self.id.with("tip"), over, 0.1);
        if t > 0.01 {
            let painter = self.ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                self.id.with("tip"),
            ));
            let screen = self.ctx.screen_rect();
            // Wrapped to the panel, so a long one (the stream info's) is
            // read whole.
            let g = painter.layout(
                word.to_string(),
                theme::font(Weight::SemiBold, 15.0),
                theme::BG,
                screen.width() - 26.0,
            );
            let size = g.size() + Vec2::new(18.0, 10.0);
            // Above the control, or below it at the top edge of the panel.
            let above = self.rect.top() - size.y - 8.0;
            let y = if above >= screen.top() + 2.0 {
                above + (1.0 - t) * 4.0
            } else {
                self.rect.bottom() + 8.0 - (1.0 - t) * 4.0
            };
            let x = fit(
                self.rect.center().x - size.x / 2.0,
                size.x,
                screen.left() + 4.0,
                screen.right() - 4.0,
            );
            let r = Rect::from_min_size(Pos2::new(x, y), size);
            painter.rect_filled(r, CornerRadius::same(8), theme::TEXT.gamma_multiply(t));
            painter.galley_with_override_text_color(
                r.min + Vec2::new(9.0, 5.0),
                g,
                theme::BG.gamma_multiply(t),
            );
        }
        self
    }
}

fn dimensional_id() -> egui::Id {
    egui::Id::new("fp-dimensional")
}

/// Draws this panel's widgets with depth: raised cards and keys, sunken
/// tracks and trays (the adjust panel). Other panels stay flat.
pub fn set_dimensional(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(dimensional_id(), on));
}

fn dimensional(ui: &Ui) -> bool {
    ui.ctx()
        .data(|d| d.get_temp::<bool>(dimensional_id()))
        .unwrap_or(false)
}

/// Room round a floating panel's body in its panel, for the shadow the
/// body casts on the scene.
/// Where a box `width` wide that would start at `x` goes to stay between
/// `lo` and `hi`: against `lo` when it is wider than that, where
/// `f32::clamp` would panic.
pub fn fit(x: f32, width: f32, lo: f32, hi: f32) -> f32 {
    x.min(hi - width).max(lo)
}

/// `text` on one line, cut short with "…" past `max_width`.
pub fn one_line(
    painter: &egui::Painter,
    text: String,
    font: egui::FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text, font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    painter.layout_job(job)
}

/// A title on one line, cut short past `max_width` and then whole in a tip.
pub fn title_line(
    ui: &mut Ui,
    title: &str,
    font: egui::FontId,
    color: Color32,
    max_width: f32,
) -> Response {
    let g = one_line(ui.painter(), title.to_string(), font, color, max_width);
    let cut = g.elided;
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    ui.painter().galley(rect.min, g, color);
    if cut { resp.tip(title) } else { resp }
}

/// A floating pill for a status message, at `anchor` of its panel.
pub fn status_pill(ctx: &egui::Context, anchor: Align2, offset: Vec2, add: impl FnOnce(&mut Ui)) {
    egui::Area::new(egui::Id::new("status"))
        .anchor(anchor, offset)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::SURFACE_3)
                .corner_radius(26)
                .inner_margin(egui::Margin::symmetric(20, 10))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(150),
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(32.0);
                        add(ui);
                    });
                });
        });
}

/// Where a toast's text wraps, in points.
const TOAST_WIDTH: f32 = 760.0;

/// A toast's symbol and text, in a [`status_pill`].
pub fn toast_line(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(super::icons::INFO)
            .font(theme::icon(22.0))
            .color(theme::ACCENT_HOVER),
    );
    // A floating pill has no width of its own to wrap at.
    let font = egui::TextStyle::Body.resolve(ui.style());
    let color = ui.visuals().text_color();
    ui.label(
        ui.painter()
            .layout(text.to_string(), font, color, TOAST_WIDTH),
    );
}

pub const SHADOW_ROOM: egui::Margin = egui::Margin {
    left: 20,
    right: 20,
    top: 8,
    bottom: 28,
};

/// Paints a panel's body as a raised slab, lit along its top edge and
/// casting a soft shadow, under everything else in `ctx`; returns the
/// body's rect (the panel less [`SHADOW_ROOM`]).
pub fn panel_slab(ctx: &egui::Context, radius: f32) -> Rect {
    let rect = ctx.screen_rect() - SHADOW_ROOM;
    slab(ctx, rect, radius);
    rect
}

/// [`panel_slab`]'s body at `rect`, for a panel that shows less of itself.
pub fn slab(ctx: &egui::Context, rect: Rect, radius: f32) {
    let p = ctx.layer_painter(egui::LayerId::background());
    p.add(
        egui::epaint::Shadow {
            offset: [0, 12],
            blur: 30,
            spread: 0,
            color: Color32::from_black_alpha(140),
        }
        .as_shape(rect, CornerRadius::same(radius as u8)),
    );
    super::depth::raised(
        &p,
        rect,
        radius,
        super::depth::Raised {
            top: Color32::from_rgb(29, 36, 46),
            bottom: Color32::from_rgb(14, 18, 24),
            light: Color32::from_white_alpha(30),
            lift: 0.6,
            glow: None,
        },
    );
    super::depth::edge(
        &p,
        rect,
        radius,
        2.0,
        Color32::from_black_alpha(140),
        super::depth::Side::Bottom,
        false,
    );
}

/// What a button is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The one main action on a screen: accent fill.
    Primary,
    /// Other actions: raised surface.
    Secondary,
    /// Low-emphasis actions: no fill until hovered.
    Ghost,
    /// Destructive actions.
    Danger,
}

/// A rounded button with an optional leading icon.
pub fn button(ui: &mut Ui, icon: Option<&str>, label: &str, kind: Kind) -> Response {
    button_sized(ui, icon, label, kind, 48.0)
}

pub fn button_sized(
    ui: &mut Ui,
    icon: Option<&str>,
    label: &str,
    kind: Kind,
    height: f32,
) -> Response {
    let font = theme::font(Weight::SemiBold, (height * 0.375).max(15.0));
    let text = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, Color32::WHITE);
    let icon_g = icon.map(|i| {
        ui.painter()
            .layout_no_wrap(i.to_string(), theme::icon(height * 0.46), Color32::WHITE)
    });
    let pad = height * 0.42;
    let gap = if label.is_empty() { 0.0 } else { 10.0 };
    let content = text.size().x + icon_g.as_ref().map(|g| g.size().x + gap).unwrap_or(0.0);
    let size = Vec2::new((content + pad * 2.0).max(height), height);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) && dimensional(ui) && kind != Kind::Ghost {
        let (face, fg) = key_face_kind(ui, &resp, height * 0.25, false, kind);
        let mut x = face.center().x - content / 2.0;
        let p = ui.painter();
        if let Some(g) = icon_g {
            let y = face.center().y - g.size().y / 2.0;
            p.galley_with_override_text_color(Pos2::new(x, y), g.clone(), fg);
            x += g.size().x + gap;
        }
        let y = face.center().y - text.size().y / 2.0;
        p.galley_with_override_text_color(Pos2::new(x, y), text, fg);
    } else if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let down = resp.is_pointer_button_down_on();
        let (bg, fg) = match kind {
            Kind::Primary => (mix(theme::ACCENT, theme::ACCENT_HOVER, t), Color32::WHITE),
            Kind::Secondary => (mix(theme::SURFACE_2, theme::SURFACE_3, t), theme::TEXT),
            Kind::Ghost => (
                Color32::from_white_alpha((t * 16.0) as u8),
                mix(theme::TEXT_2, Color32::WHITE, t),
            ),
            Kind::Danger => (
                mix(theme::SURFACE_2, theme::ERROR.gamma_multiply(0.25), t),
                theme::ERROR,
            ),
        };
        let bg = if down {
            mix(bg, Color32::BLACK, 0.2)
        } else {
            bg
        };
        let r = rect.expand(t * 1.5);
        let radius = CornerRadius::same((height * 0.25) as u8);
        let p = ui.painter();
        p.rect_filled(r, radius, bg);
        if t > 0.0 && kind != Kind::Ghost {
            p.rect_stroke(
                r,
                radius,
                Stroke::new(1.5_f32, Color32::from_white_alpha((t * 60.0) as u8)),
                StrokeKind::Outside,
            );
        }
        let mut x = r.center().x - content / 2.0;
        if let Some(g) = icon_g {
            let y = r.center().y - g.size().y / 2.0;
            p.galley_with_override_text_color(Pos2::new(x, y), g.clone(), fg);
            x += g.size().x + gap;
        }
        let y = r.center().y - text.size().y / 2.0;
        p.galley_with_override_text_color(Pos2::new(x, y), text, fg);
    }
    resp
}

/// A round icon-only button. `selected` marks a toggle that is on.
pub fn icon_button(ui: &mut Ui, icon: &str, size: f32, selected: bool) -> Response {
    if dimensional(ui) {
        return key(ui, icon, size, selected);
    }
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let down = resp.is_pointer_button_down_on();
        let p = ui.painter();
        let c = rect.center();
        let radius = size / 2.0 + t * 1.5;
        let bg = if selected {
            mix(
                theme::ACCENT.gamma_multiply(0.22),
                theme::ACCENT.gamma_multiply(0.34),
                t,
            )
        } else {
            Color32::from_white_alpha((t * 20.0) as u8 + if down { 14 } else { 0 })
        };
        p.circle_filled(c, radius, bg);
        if t > 0.0 {
            p.circle_stroke(
                c,
                radius,
                Stroke::new(1.5_f32, Color32::from_white_alpha((t * 50.0) as u8)),
            );
        }
        let fg = if selected {
            theme::ACCENT_HOVER
        } else {
            mix(theme::TEXT, Color32::WHITE, t)
        };
        p.text(c, Align2::CENTER_CENTER, icon, theme::icon(size * 0.48), fg);
    }
    resp
}

const KEY_TOP: Color32 = Color32::from_rgb(58, 69, 83);
const KEY_BOTTOM: Color32 = Color32::from_rgb(35, 42, 51);
const KEY_TOP_HOVER: Color32 = Color32::from_rgb(72, 84, 98);
const KEY_BOTTOM_HOVER: Color32 = Color32::from_rgb(45, 54, 65);
const KEY_TOP_ON: Color32 = Color32::from_rgb(31, 76, 114);
const KEY_BOTTOM_ON: Color32 = Color32::from_rgb(21, 52, 82);
const KEY_FG_ON: Color32 = Color32::from_rgb(125, 200, 255);
/// The sunken trays that hold groups of keys.
pub const WELL: Color32 = Color32::from_rgb(10, 13, 17);

/// Paints the raised body of a key filling `resp.rect` (less a margin for
/// its shadow), lifted under the pointer and sunk while pressed; returns
/// the body's rect and the colour for what goes on it.
pub fn key_face(ui: &Ui, resp: &Response, radius: f32, selected: bool) -> (Rect, Color32) {
    key_face_kind(ui, resp, radius, selected, Kind::Secondary)
}

/// [`key_face`] for a button of `kind`: Primary is an accent key, Danger
/// has red text.
pub fn key_face_kind(
    ui: &Ui,
    resp: &Response,
    radius: f32,
    selected: bool,
    kind: Kind,
) -> (Rect, Color32) {
    if kind == Kind::Primary {
        let t = anim(ui, resp.id, resp.hovered());
        let down = resp.is_pointer_button_down_on();
        let dy = if down { 0.5 } else { -1.5 * t };
        let rect = resp.rect.shrink(3.0).translate(Vec2::new(0.0, dy));
        let look = super::depth::Raised {
            top: mix(
                Color32::from_rgb(92, 190, 255),
                Color32::from_rgb(128, 208, 255),
                t,
            ),
            bottom: mix(
                Color32::from_rgb(16, 112, 196),
                Color32::from_rgb(24, 132, 224),
                t,
            ),
            light: Color32::from_white_alpha(130),
            lift: if down { 0.2 } else { 0.7 + 0.5 * t },
            glow: Some(theme::ACCENT.gamma_multiply(0.3 + 0.2 * t)),
        };
        super::depth::raised(ui.painter(), rect, radius.min(rect.height() / 2.0), look);
        return (rect, Color32::WHITE);
    }
    let t = anim(ui, resp.id, resp.hovered());
    let s = anim(ui, resp.id.with("sel"), selected);
    let down = resp.is_pointer_button_down_on();
    let dy = if down { 0.5 } else { -1.5 * t };
    let rect = resp.rect.shrink(3.0).translate(Vec2::new(0.0, dy));
    let shade = |c: Color32| {
        if down {
            mix(c, Color32::BLACK, 0.15)
        } else {
            c
        }
    };
    let look = super::depth::Raised {
        top: shade(mix(mix(KEY_TOP, KEY_TOP_HOVER, t), KEY_TOP_ON, s)),
        bottom: shade(mix(mix(KEY_BOTTOM, KEY_BOTTOM_HOVER, t), KEY_BOTTOM_ON, s)),
        light: mix(
            Color32::from_white_alpha((70.0 + 30.0 * t) as u8),
            KEY_FG_ON.gamma_multiply(0.4),
            s,
        ),
        lift: if down { 0.2 } else { 0.6 + 0.6 * t },
        glow: (s > 0.0).then(|| theme::ACCENT.gamma_multiply(0.35 * s)),
    };
    super::depth::raised(ui.painter(), rect, radius.min(rect.height() / 2.0), look);
    let fg = mix(mix(theme::TEXT, Color32::WHITE, t), KEY_FG_ON, s);
    let fg = if kind == Kind::Danger {
        theme::ERROR
    } else {
        fg
    };
    (rect, fg)
}

/// A raised round key, for the control bar. `selected` marks a toggle
/// that is on.
pub fn key(ui: &mut Ui, icon: &str, size: f32, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = key_face(ui, &resp, size, selected);
        ui.painter().text(
            face.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(size * 0.46),
            fg,
        );
    }
    resp
}

/// A [`key`] with the filled `inner` icon small inside the outline `icon`:
/// a play sign in a circular arrow is Autoplay.
pub fn key_nested(ui: &mut Ui, icon: &str, inner: &str, size: f32, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = key_face(ui, &resp, size, selected);
        let p = ui.painter();
        p.text(
            face.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(size * 0.46),
            fg,
        );
        // A play sign's middle is left of its box's.
        p.text(
            face.center() + Vec2::splat(size * 0.01),
            Align2::CENTER_CENTER,
            inner,
            theme::icon_fill(size * 0.18),
            fg,
        );
    }
    resp
}

/// A raised pill key with a short label, for the control bar.
pub fn pill_key(ui: &mut Ui, label: &str, selected: bool) -> Response {
    let g = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::SemiBold, 17.0),
        Color32::WHITE,
    );
    let size = Vec2::new((g.size().x + 34.0).max(52.0), 46.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = key_face(ui, &resp, size.y, selected);
        ui.painter()
            .galley_with_override_text_color(face.center() - g.size() / 2.0, g, fg);
    }
    resp
}

/// A sunken tray holding a group of keys on the control bar.
pub fn well<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let slot = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin::same(5))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            add(ui)
        });
    let r = inner.response.rect;
    ui.painter().set(
        slot,
        egui::Shape::Vec(super::depth::sunken_shapes(r, r.height() / 2.0, WELL)),
    );
    inner.inner
}

/// A chip: a short choice in a row of choices (filters, formats, tabs).
pub fn chip(ui: &mut Ui, label: &str, selected: bool) -> Response {
    chip_icon(ui, None, label, selected)
}

/// How wide a chip without an icon is.
pub fn chip_width(ui: &Ui, label: &str) -> f32 {
    let text = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::Medium, 16.0),
        Color32::WHITE,
    );
    (text.size().x + 32.0).max(40.0)
}

pub fn chip_icon(ui: &mut Ui, icon: Option<&str>, label: &str, selected: bool) -> Response {
    let h = 40.0;
    let text = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::Medium, 16.0),
        Color32::WHITE,
    );
    let icon_g = icon.map(|i| {
        ui.painter()
            .layout_no_wrap(i.to_string(), theme::icon(19.0), Color32::WHITE)
    });
    let gap = if icon_g.is_some() && !label.is_empty() {
        8.0
    } else {
        0.0
    };
    let content = text.size().x + icon_g.as_ref().map(|g| g.size().x + gap).unwrap_or(0.0);
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new((content + 32.0).max(h), h), Sense::click());
    if ui.is_rect_visible(rect) && dimensional(ui) {
        let (face, fg) = key_face(ui, &resp, h / 2.0, selected);
        let p = ui.painter();
        let mut x = face.center().x - content / 2.0;
        if let Some(g) = icon_g {
            p.galley_with_override_text_color(
                Pos2::new(x, face.center().y - g.size().y / 2.0),
                g.clone(),
                fg,
            );
            x += g.size().x + gap;
        }
        p.galley_with_override_text_color(
            Pos2::new(x, face.center().y - text.size().y / 2.0),
            text,
            fg,
        );
    } else if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let s = anim(ui, resp.id.with("sel"), selected);
        let bg = mix(mix(theme::SURFACE_2, theme::SURFACE_3, t), theme::TEXT, s);
        let fg = mix(mix(theme::TEXT_2, Color32::WHITE, t), theme::BG, s);
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same((h / 2.0) as u8), bg);
        let mut x = rect.center().x - content / 2.0;
        if let Some(g) = icon_g {
            p.galley_with_override_text_color(
                Pos2::new(x, rect.center().y - g.size().y / 2.0),
                g.clone(),
                fg,
            );
            x += g.size().x + gap;
        }
        p.galley_with_override_text_color(
            Pos2::new(x, rect.center().y - text.size().y / 2.0),
            text,
            fg,
        );
    }
    resp
}

pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(56.0, 32.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) && dimensional(ui) {
        // A sunken slot that fills with the accent, a raised knob in it.
        let h = anim(ui, resp.id, resp.hovered());
        let t = anim(ui, resp.id.with("on"), *on);
        let p = ui.painter();
        super::depth::sunken(p, rect, 16.0, WELL);
        if t > 0.0 {
            let fill = rect.shrink(1.0);
            p.rect_filled(
                fill,
                CornerRadius::same(15),
                theme::ACCENT.gamma_multiply(t),
            );
            super::depth::fill_vgradient(
                p,
                fill,
                15.0,
                Color32::from_rgb(98, 196, 255).gamma_multiply(t),
                Color32::from_rgb(22, 132, 226).gamma_multiply(t),
            );
            super::depth::edge(
                p,
                rect,
                16.0,
                4.0,
                Color32::from_black_alpha(120),
                super::depth::Side::Top,
                false,
            );
        }
        let x = egui::lerp((rect.left() + 16.0)..=(rect.right() - 16.0), t);
        knob(p, Pos2::new(x, rect.center().y - h), 12.0 + h);
    } else if ui.is_rect_visible(rect) {
        let h = anim(ui, resp.id, resp.hovered());
        let t = anim(ui, resp.id.with("on"), *on);
        let p = ui.painter();
        let track = mix(
            mix(theme::SURFACE_3, Color32::from_rgb(58, 68, 82), h),
            theme::ACCENT,
            t,
        );
        p.rect_filled(rect, CornerRadius::same(16), track);
        let r = 12.0 + h;
        let x = egui::lerp((rect.left() + 16.0)..=(rect.right() - 16.0), t);
        p.circle_filled(
            Pos2::new(x, rect.center().y + 1.0),
            r,
            Color32::from_black_alpha(60),
        );
        p.circle_filled(Pos2::new(x, rect.center().y), r, Color32::WHITE);
    }
    resp
}

/// Small spaced capitals over a group ("PLAYBACK").
pub fn section_label(ui: &mut Ui, text: &str) {
    let mut job = LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: theme::font(Weight::SemiBold, 13.0),
            color: theme::TEXT_3,
            extra_letter_spacing: 1.4,
            ..Default::default()
        },
    );
    ui.label(job);
}

/// A raised group of rows.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    if dimensional(ui) {
        let slot = ui.painter().add(egui::Shape::Noop);
        let inner = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(4, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                add(ui)
            });
        let look = super::depth::Raised {
            top: Color32::from_rgb(30, 37, 47),
            bottom: Color32::from_rgb(21, 26, 33),
            light: Color32::from_white_alpha(26),
            lift: 0.5,
            glow: None,
        };
        ui.painter().set(
            slot,
            egui::Shape::Vec(super::depth::raised_shapes(inner.response.rect, 14.0, look)),
        );
        return inner.inner;
    }
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(14)
        .inner_margin(egui::Margin::symmetric(4, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner
}

/// Rows inside a [`card`]: a title, an optional description and a control
/// on the right, with hairlines between rows.
pub struct Rows<'a> {
    ui: &'a mut Ui,
    n: usize,
}

pub fn rows(ui: &mut Ui, add: impl FnOnce(&mut Rows)) {
    card(ui, |ui| add(&mut Rows { ui, n: 0 }));
}

const ROW_MARGIN: egui::Margin = egui::Margin::symmetric(16, 12);
const ROW_MIN_HEIGHT: f32 = 48.0;
const CONTROL_GAP: f32 = 12.0;

/// Width of a row's text column, leaving room for a slider row's control.
fn text_column(row_width: f32) -> f32 {
    (row_width - 420.0).max(row_width * 0.4)
}

impl Rows<'_> {
    /// The next row's frame, after a separator from the previous one.
    fn next_row<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> R {
        if self.n > 0 {
            separator(self.ui);
        }
        self.n += 1;
        egui::Frame::new()
            .inner_margin(ROW_MARGIN)
            .show(self.ui, add)
            .inner
    }

    pub fn row<R>(
        &mut self,
        title: &str,
        desc: Option<&str>,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        self.next_row(|ui| {
            // Text on the left and the control on the right, both
            // centred on the row's height.
            let w = ui.available_width();
            let text_w = text_column(w);
            let title_g = ui.painter().layout(
                title.to_string(),
                theme::font(Weight::Medium, 18.0),
                theme::TEXT,
                text_w,
            );
            let desc_g = desc.map(|d| {
                ui.painter().layout(
                    d.to_string(),
                    theme::font(Weight::Regular, 15.0),
                    theme::TEXT_2,
                    text_w,
                )
            });
            let text_h =
                title_g.size().y + desc_g.as_ref().map(|g| 3.0 + g.size().y).unwrap_or(0.0);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(w, text_h.max(ROW_MIN_HEIGHT)), Sense::hover());
            let mut y = rect.center().y - text_h / 2.0;
            let p = ui.painter();
            let title_h = title_g.size().y;
            p.galley(Pos2::new(rect.left(), y), title_g, theme::TEXT);
            y += title_h + 3.0;
            if let Some(g) = desc_g {
                p.galley(Pos2::new(rect.left(), y), g, theme::TEXT_2);
            }
            let control_rect = Rect::from_min_max(
                Pos2::new(rect.left() + text_w + CONTROL_GAP, rect.top()),
                rect.max,
            );
            let mut c = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(control_rect)
                    .layout(Layout::right_to_left(Align::Center)),
            );
            c.spacing_mut().item_spacing.x = 10.0;
            control(&mut c)
        })
    }

    /// A [`row`](Self::row) with more under its text (choices that need the
    /// width), the control still on the right.
    pub fn row_with<R>(
        &mut self,
        title: &str,
        desc: Option<&str>,
        below: impl FnOnce(&mut Ui),
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        self.next_row(|ui| {
            let w = ui.available_width();
            let text_w = text_column(w);
            let top = ui.cursor().min;
            let mut left = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(Rect::from_min_size(top, Vec2::new(text_w, f32::INFINITY)))
                    .layout(Layout::top_down(Align::Min)),
            );
            left.spacing_mut().item_spacing.y = 3.0;
            left.label(
                egui::RichText::new(title)
                    .font(theme::font(Weight::Medium, 18.0))
                    .color(theme::TEXT),
            );
            if let Some(d) = desc {
                left.label(
                    egui::RichText::new(d)
                        .font(theme::font(Weight::Regular, 15.0))
                        .color(theme::TEXT_2),
                );
            }
            left.add_space(8.0);
            below(&mut left);
            let h = left.min_rect().height().max(ROW_MIN_HEIGHT);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
            let control_rect = Rect::from_min_max(
                Pos2::new(rect.left() + text_w + CONTROL_GAP, rect.top()),
                rect.max,
            );
            let mut c = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(control_rect)
                    .layout(Layout::right_to_left(Align::Center)),
            );
            c.spacing_mut().item_spacing.x = 10.0;
            control(&mut c)
        })
    }

    /// A row with a read-only value on the right. A long value (a path, a
    /// list) wraps within its column and the row grows to fit.
    pub fn info(&mut self, title: &str, value: &str) {
        self.info_colored(title, value, theme::TEXT_2);
    }

    /// [`Rows::info`] with the value in `color`.
    pub fn info_colored(&mut self, title: &str, value: &str, color: Color32) {
        self.next_row(|ui| {
            let w = ui.available_width();
            let title_g = ui.painter().layout(
                title.to_string(),
                theme::font(Weight::Medium, 18.0),
                theme::TEXT,
                text_column(w),
            );
            // The value takes what the title leaves, unlike a control's
            // fixed column.
            let value_w = w - title_g.size().x - 4.0 * CONTROL_GAP;
            let mut job = LayoutJob::simple(
                value.to_string(),
                theme::font(Weight::Regular, 18.0),
                color,
                value_w,
            );
            job.halign = Align::RIGHT;
            let value_g = ui.painter().layout_job(job);
            let (title_h, value_h) = (title_g.size().y, value_g.size().y);
            let h = title_h.max(value_h).max(ROW_MIN_HEIGHT);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
            let p = ui.painter();
            let mid = rect.center().y;
            p.galley(
                Pos2::new(rect.left(), mid - title_h / 2.0),
                title_g,
                theme::TEXT,
            );
            // Right-aligned rows lie left of x = 0: anchor at the right edge.
            p.galley(Pos2::new(rect.right(), mid - value_h / 2.0), value_g, color);
        });
    }

    /// A row with a switch; true when it changed.
    pub fn switch(&mut self, title: &str, desc: Option<&str>, on: &mut bool) -> bool {
        self.row(title, desc, |ui| switch(ui, on).changed())
    }

    /// A row with a slider, its value and a reset button; true when changed.
    #[allow(clippy::too_many_arguments)]
    pub fn slider(
        &mut self,
        title: &str,
        desc: Option<&str>,
        value: &mut f32,
        range: std::ops::RangeInclusive<f32>,
        default: f32,
        suffix: &str,
        decimals: usize,
    ) -> bool {
        self.row(title, desc, |ui| {
            slider_control(ui, value, range, default, suffix, decimals)
        })
    }

    /// A full-width line of content (a hint, a list) inside the card.
    pub fn content<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> R {
        self.next_row(|ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
    }
}

/// The line between rows: a hairline, or a groove when dimensional.
fn separator(ui: &Ui) {
    let x = ui.max_rect().x_range();
    let x = (x.min + 16.0)..=(x.max - 16.0);
    let y = ui.cursor().top();
    if dimensional(ui) {
        ui.painter().hline(
            x.clone(),
            y - 0.5,
            Stroke::new(1.0_f32, Color32::from_black_alpha(120)),
        );
        ui.painter().hline(
            x,
            y + 0.5,
            Stroke::new(1.0_f32, Color32::from_white_alpha(12)),
        );
    } else {
        ui.painter()
            .hline(x, y, Stroke::new(1.0_f32, theme::STROKE));
    }
}

/// Slider, value and reset, laid out right to left (inside a row).
pub fn slider_control(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    default: f32,
    suffix: &str,
    decimals: usize,
) -> bool {
    let mut changed = false;
    let reset = (*value - default).abs() > 1e-4;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    if reset {
        let t = anim(ui, resp.id, resp.hovered());
        ui.painter().circle_filled(
            rect.center(),
            19.0,
            Color32::from_white_alpha((t * 20.0) as u8),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            super::icons::ARROW_COUNTER_CLOCKWISE,
            theme::icon(20.0),
            mix(theme::TEXT_2, Color32::WHITE, t),
        );
        if resp.tip(tr!("adjust-reset")).clicked() {
            *value = default;
            changed = true;
        }
    }
    ui.add_sized(
        [74.0, 32.0],
        egui::Label::new(
            egui::RichText::new(format!("{value:.decimals$}{suffix}"))
                .font(theme::font(Weight::Medium, 16.0))
                .color(theme::TEXT_2),
        ),
    );
    // What the row leaves it, so at a large interface size it doesn't run
    // over the text beside it.
    let width = ui.available_width().clamp(120.0, 240.0);
    let resp = slider(ui, value, range.clone(), width);
    changed |= resp.changed();
    if resp.hovered() {
        let ctx = ui.ctx().clone();
        let dir = ui.input_mut(|i| {
            let mut n = |key| i.count_and_consume_key(egui::Modifiers::NONE, key) as f32;
            n(egui::Key::ArrowRight) - n(egui::Key::ArrowLeft)
        });
        if dir != 0.0 {
            // A step of the shown precision, landing on it.
            let step = 10f32.powi(-(decimals as i32));
            *value = (((*value / step).round() + dir) * step).clamp(*range.start(), *range.end());
            changed = true;
            ctx.request_repaint();
        }
    }
    changed
}

const OVER_SLIDER: &str = "over-slider";

/// How far (in points, along its direction) a press goes to start scrolling a
/// list; from there on it is no longer a click. About a degree and a half:
/// clicks wander far less up and down than sideways, and only 1 of 35 clicks
/// recorded on the headset went further.
const LIST_REACH: f32 = 25.0;

/// A let-go slower than this (points a second) is the trigger's tip, not a
/// flick of the list.
const FLICK: f32 = 500.0;

/// How fast a flicked list slows (points a second squared), and the speed it
/// stops at: as egui's.
const COAST_FRICTION: f32 = 1000.0;
const COAST_STOP: f32 = 20.0;

const LIST_TOOK_PRESS: &str = "list-took-press";

/// What a list keeps from pass to pass to follow a press.
#[derive(Clone, Copy)]
struct ListGrip {
    id: egui::Id,
    rect: Rect,
    /// How far it can scroll.
    max: f32,
    /// A press began on it, and not on a slider or a scroll bar.
    armed: bool,
    scrolling: bool,
    /// Points a second, coasting on from a flick.
    velocity: f32,
}

/// Follows a press on the list `key` remembers, ahead of the list's pass so
/// it shows this frame.
fn drag_list(ui: &Ui, key: egui::Id, axis: usize) {
    let ctx = ui.ctx();
    let Some(mut g) = ctx.data(|d| d.get_temp::<ListGrip>(key)) else {
        return;
    };
    let Some(mut state) = egui::scroll_area::State::load(ctx, g.id) else {
        return;
    };
    let (pressed, down, origin, pos, delta, velocity, dt) = ctx.input(|i| {
        (
            i.pointer.any_pressed(),
            i.pointer.any_down(),
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.delta()[axis],
            i.pointer.velocity()[axis],
            i.stable_dt,
        )
    });
    if pressed {
        g.armed = g.max > 0.0
            && origin.is_some_and(|o| g.rect.contains(o))
            && !over_slider(ctx)
            && ctx.dragged_id().is_none();
    }
    let mut by = 0.0;
    if down {
        g.velocity = 0.0;
        if g.armed
            && !g.scrolling
            && let (Some(o), Some(p)) = (origin, pos)
            && (p - o)[axis].abs() > LIST_REACH
        {
            g.scrolling = true;
            ctx.data_mut(|d| d.insert_temp(egui::Id::new(LIST_TOOK_PRESS), true));
        }
        if g.scrolling {
            by = delta;
        }
    } else {
        // Let go off its panel, the pointer's speed means nothing.
        let on_panel = pos.is_some_and(|p| ctx.screen_rect().contains(p));
        if g.scrolling && on_panel && velocity.abs() > FLICK {
            g.velocity = velocity;
        }
        g.scrolling = false;
        g.armed = false;
        by = g.velocity * dt;
        let slower = g.velocity.abs() - COAST_FRICTION * dt;
        g.velocity = if slower < COAST_STOP {
            0.0
        } else {
            slower.copysign(g.velocity)
        };
        if g.velocity != 0.0 {
            ctx.request_repaint();
        }
    }
    if by != 0.0 {
        state.offset[axis] = (state.offset[axis] - by).clamp(0.0, g.max);
        state.store(ctx, g.id);
    }
    ctx.data_mut(|d| d.insert_temp(key, g));
}

fn show_list_along<R>(
    area: egui::ScrollArea,
    ui: &mut Ui,
    axis: usize,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let key = ui.next_auto_id().with("list");
    drag_list(ui, key, axis);
    let out = area
        .scroll_source(egui::scroll_area::ScrollSource {
            scroll_bar: true,
            drag: false,
            mouse_wheel: true,
        })
        .show(ui, add);
    let max = (out.content_size[axis] - out.inner_rect.size()[axis]).max(0.0);
    let (id, rect) = (out.id, out.inner_rect);
    ui.data_mut(|d| {
        let last = d.get_temp::<ListGrip>(key);
        d.insert_temp(
            key,
            ListGrip {
                id,
                rect,
                max,
                armed: last.is_some_and(|g| g.armed),
                scrolling: last.is_some_and(|g| g.scrolling),
                velocity: last.map_or(0.0, |g| g.velocity),
            },
        );
    });
    out
}

/// Scroll areas that scroll by their bar, the thumbstick and a press past
/// [`LIST_REACH`] along them, which they follow point for point until it lets
/// go. egui's own drag scrolling starts with the press, so every click would
/// nudge the list.
pub trait ListArea {
    /// Shows a list that scrolls up and down.
    fn show_list<R>(
        self,
        ui: &mut Ui,
        add: impl FnOnce(&mut Ui) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R>;

    /// Shows a row that scrolls sideways.
    fn show_rail<R>(
        self,
        ui: &mut Ui,
        add: impl FnOnce(&mut Ui) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R>;
}

impl ListArea for egui::ScrollArea {
    fn show_list<R>(
        self,
        ui: &mut Ui,
        add: impl FnOnce(&mut Ui) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R> {
        show_list_along(self, ui, 1, add)
    }

    fn show_rail<R>(
        self,
        ui: &mut Ui,
        add: impl FnOnce(&mut Ui) -> R,
    ) -> egui::scroll_area::ScrollAreaOutput<R> {
        show_list_along(self, ui, 0, add)
    }
}

/// Whether a list has just taken the press from what it was on, once.
pub fn list_took_press(ctx: &egui::Context) -> bool {
    let id = egui::Id::new(LIST_TOOK_PRESS);
    ctx.data_mut(|d| {
        let took = d.get_temp::<bool>(id).unwrap_or(false);
        d.remove::<bool>(id);
        took
    })
}

/// Whether the pointer was on a [`slider`] in the panel's last pass: a
/// trigger press on it moves the slider instead of scrolling a list, and the
/// thumbstick steps it (the arrow keys a [`slider_control`] takes) instead of
/// paging.
pub fn over_slider(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<u64>(egui::Id::new(OVER_SLIDER)))
        .is_some_and(|pass| pass + 1 == ctx.cumulative_pass_nr())
}

/// A press this close to a slider's knob, in points, takes hold of it where
/// it is.
const GRAB_REACH: f32 = 30.0;

/// A slider: a slim rail filled in the accent up to a white knob. Pulled
/// on, the knob moves as far as the pointer does.
pub fn slider(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    let inset = 11.0;
    let rail_x = (rect.left() + inset)..=(rect.right() - inset);
    let len = rail_x.end() - rail_x.start();
    let fraction = |v: f32| {
        if hi > lo {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    if resp.hovered() {
        let pass = ui.ctx().cumulative_pass_nr();
        ui.data_mut(|d| d.insert_temp(egui::Id::new(OVER_SLIDER), pass));
    }
    // Where a press took the knob, where the pointer was when it last could
    // still have been a click, and the value then.
    let grab = resp.id.with("grab");
    if resp.is_pointer_button_down_on()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let (origin, may_click) = ui.input(|i| {
            (
                i.pointer.press_origin().unwrap_or(p),
                i.pointer.could_any_button_be_click(),
            )
        });
        let knob = egui::lerp(rail_x.clone(), fraction(*value));
        let (from_x, v0) = ui.data_mut(|d| {
            let (from_x, v0) = match d.get_temp::<(Pos2, f32, f32)>(grab) {
                Some(g) if g.0 == origin => (g.1, g.2),
                // Pressed on or near the knob, it is taken where it is;
                // further along the rail it jumps there first.
                _ => {
                    let v0 = if (origin.x - knob).abs() <= GRAB_REACH {
                        *value
                    } else {
                        lo + ((origin.x - rail_x.start()) / len).clamp(0.0, 1.0) * (hi - lo)
                    };
                    (origin.x, v0)
                }
            };
            // A click's wander doesn't move it; a drag goes on from there.
            let from_x = if may_click { p.x } else { from_x };
            d.insert_temp(grab, (origin, from_x, v0));
            (from_x, v0)
        });
        let v = (v0 + (p.x - from_x) / len * (hi - lo)).clamp(lo, hi);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    } else {
        ui.data_mut(|d| d.remove::<(Pos2, f32)>(grab));
    }
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered() || resp.dragged());
        let x = egui::lerp(rail_x.clone(), fraction(*value));
        let h = 7.0 + t * 2.0;
        let y = rect.center().y;
        let rail = Rect::from_x_y_ranges(rail_x.clone(), (y - h / 2.0)..=(y + h / 2.0));
        let done = Rect::from_x_y_ranges(*rail_x.start()..=x, rail.y_range());
        super::depth::sunken(ui.painter(), rail, h / 2.0, WELL);
        rail_fill(ui.painter(), rail, done);
        knob(ui.painter(), Pos2::new(x, y), 9.0 + t * 2.0);
    }
    resp
}

/// The accent fill over the `done` part of a sunken rail (seek bar,
/// sliders), glowing a little.
pub fn rail_fill(p: &egui::Painter, rail: Rect, done: Rect) {
    let r = rail.height() / 2.0;
    if done.width() > 0.5 {
        let done = done.intersect(rail);
        p.add(
            egui::epaint::Shadow {
                offset: [0, 0],
                blur: 10,
                spread: 0,
                color: theme::ACCENT.gamma_multiply(0.3),
            }
            .as_shape(done, CornerRadius::same(r as u8)),
        );
        p.rect_filled(done, CornerRadius::same(r as u8), theme::ACCENT);
        super::depth::fill_vgradient(
            p,
            done,
            r,
            Color32::from_rgb(98, 196, 255),
            Color32::from_rgb(22, 132, 226),
        );
    }
}

/// The white knob on a rail: a raised disc of radius `r` at `c`.
pub fn knob(p: &egui::Painter, c: Pos2, r: f32) {
    super::depth::raised(
        p,
        Rect::from_center_size(c, Vec2::splat(r * 2.0)),
        r,
        super::depth::Raised {
            top: Color32::WHITE,
            bottom: Color32::from_rgb(208, 215, 224),
            light: Color32::WHITE,
            lift: 0.8,
            glow: None,
        },
    );
}

/// A row of equal segments, each an icon over a label, one selected;
/// returns the segment clicked.
pub fn segmented(ui: &mut Ui, items: &[(&str, &str)], selected: usize) -> Option<usize> {
    let h = 66.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let dim = dimensional(ui);
    if dim {
        super::depth::sunken(ui.painter(), rect, 16.0, WELL);
    } else {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(16), theme::SURFACE);
    }
    let n = items.len().max(1) as f32;
    let w = (rect.width() - 8.0) / n;
    // At a large interface size a label may not fit its segment: then all
    // show their icons alone, the labels as tips.
    let label_font = theme::font(Weight::SemiBold, 13.0);
    let labelled = items.iter().all(|(_, label)| {
        let g = ui
            .painter()
            .layout_no_wrap(label.to_string(), label_font.clone(), Color32::WHITE);
        g.size().x <= w - 8.0
    });
    let mut clicked = None;
    for (i, (icon, label)) in items.iter().enumerate() {
        let seg = Rect::from_min_size(
            Pos2::new(rect.left() + 4.0 + i as f32 * w, rect.top() + 4.0),
            Vec2::new(w, h - 8.0),
        );
        let resp = ui.interact(seg, ui.id().with(("segment", i)), Sense::click());
        let resp = if labelled { resp } else { resp.tip(label) };
        let t = anim(ui, resp.id, resp.hovered());
        let s = anim(ui, resp.id.with("sel"), i == selected);
        let p = ui.painter();
        if dim {
            // The chosen tab is a raised key in the tray; the others rise
            // a little under the pointer.
            let lift = (t * (1.0 - s)).max(s);
            if lift > 0.01 {
                let down = resp.is_pointer_button_down_on();
                let face = seg.shrink(2.0).translate(Vec2::new(0.0, -t * (1.0 - s)));
                super::depth::raised(
                    p,
                    face,
                    11.0,
                    super::depth::Raised {
                        top: mix(KEY_TOP, KEY_TOP_HOVER, t).gamma_multiply(lift),
                        bottom: mix(KEY_BOTTOM, KEY_BOTTOM_HOVER, t).gamma_multiply(lift),
                        light: Color32::from_white_alpha((60.0 * lift) as u8),
                        lift: if down { 0.2 } else { 0.7 * lift },
                        glow: None,
                    },
                );
            }
        } else {
            let bg = Color32::from_white_alpha((t * 10.0) as u8).lerp_to_gamma(theme::SURFACE_3, s);
            p.rect_filled(seg, CornerRadius::same(12), bg);
        }
        let fg = mix(mix(theme::TEXT_2, theme::TEXT, t), Color32::WHITE, s);
        let lift = if labelled { 10.0 } else { 0.0 };
        p.text(
            seg.center() - Vec2::new(0.0, lift),
            Align2::CENTER_CENTER,
            *icon,
            theme::icon(22.0),
            mix(fg, theme::ACCENT_HOVER, s),
        );
        if labelled {
            p.text(
                seg.center() + Vec2::new(0.0, 14.0),
                Align2::CENTER_CENTER,
                *label,
                label_font.clone(),
                fg,
            );
        }
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// A top-level navigation tab: icon and label, an accent bar under the
/// selected one. With `labelled` false, its icon alone, the label as a tip.
pub fn nav_tab(ui: &mut Ui, icon: &str, label: &str, selected: bool, labelled: bool) -> Response {
    let text = ui.painter().layout_no_wrap(
        if labelled { label } else { "" }.to_string(),
        theme::font(Weight::SemiBold, 18.0),
        Color32::WHITE,
    );
    let icon_g = ui
        .painter()
        .layout_no_wrap(icon.to_string(), theme::icon(22.0), Color32::WHITE);
    let gap = if labelled { 8.0 } else { 0.0 };
    let content = icon_g.size().x + gap + text.size().x;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(content + 28.0, 52.0), Sense::click());
    let resp = if labelled { resp } else { resp.tip(label) };
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let s = anim(ui, resp.id.with("sel"), selected);
        let dim = dimensional(ui);
        let p = ui.painter();
        let mut rect = rect;
        if dim {
            // The chosen tab is a raised key; others rise under the pointer.
            if selected || t > 0.01 {
                let (face, _) = key_face(ui, &resp, 12.0, false);
                rect = face;
            }
        } else {
            p.rect_filled(
                rect,
                CornerRadius::same(12),
                Color32::from_white_alpha((t * 12.0) as u8),
            );
        }
        let fg = mix(mix(theme::TEXT_2, theme::TEXT, t), Color32::WHITE, s);
        let mut x = rect.center().x - content / 2.0;
        p.galley_with_override_text_color(
            Pos2::new(x, rect.center().y - icon_g.size().y / 2.0),
            icon_g,
            mix(fg, theme::ACCENT_HOVER, s),
        );
        x += content - text.size().x;
        p.galley_with_override_text_color(
            Pos2::new(x, rect.center().y - text.size().y / 2.0),
            text,
            fg,
        );
        if s > 0.0 && !dim {
            let w = content * s;
            let bar = Rect::from_center_size(
                Pos2::new(rect.center().x, rect.bottom() - 2.0),
                Vec2::new(w, 3.0),
            );
            p.rect_filled(bar, CornerRadius::same(2), theme::ACCENT);
        }
    }
    resp
}

/// A vertical gradient filling `rect`.
pub fn vgradient(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

/// A horizontal gradient filling `rect`.
pub fn hgradient(painter: &egui::Painter, rect: Rect, left: Color32, right: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

/// A soft drop shadow under `rect`.
pub fn shadow(painter: &egui::Painter, rect: Rect, radius: u8, strength: f32) {
    if strength <= 0.0 {
        return;
    }
    let s = egui::epaint::Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha((strength * 170.0) as u8),
    };
    painter.add(s.as_shape(rect, CornerRadius::same(radius)));
}

/// A play triangle `size` tall, its centroid on `center` so it looks
/// centred in a circle (a glyph's box centre sits too far left).
pub fn play_mark(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let h = size;
    let w = h * 0.88;
    let left = center.x - w / 3.0;
    let pts = vec![
        Pos2::new(left, center.y - h / 2.0),
        Pos2::new(left + w, center.y),
        Pos2::new(left, center.y + h / 2.0),
    ];
    // A hairline in the same colour softens the points a little.
    painter.add(egui::Shape::convex_polygon(
        pts,
        color,
        Stroke::new(2.0_f32, color),
    ));
}

/// A pause mark (two rounded bars) `size` tall, centred on `center`.
pub fn pause_mark(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let bar = Vec2::new(size * 0.28, size);
    let gap = size * 0.22;
    for dx in [-(gap / 2.0 + bar.x / 2.0), gap / 2.0 + bar.x / 2.0] {
        painter.rect_filled(
            Rect::from_center_size(center + Vec2::new(dx, 0.0), bar),
            CornerRadius::same((bar.x * 0.3) as u8),
            color,
        );
    }
}

/// A small label on a translucent dark pill, for badges over pictures.
pub fn badge(painter: &egui::Painter, text: &str, anchor: Pos2, align: Align2) {
    let g = painter.layout_no_wrap(
        text.to_string(),
        theme::font(Weight::SemiBold, 13.0),
        Color32::WHITE,
    );
    let r = align.anchor_size(anchor, g.size() + Vec2::new(14.0, 6.0));
    painter.rect_filled(r, CornerRadius::same(7), Color32::from_black_alpha(170));
    painter.galley(r.min + Vec2::new(7.0, 3.0), g, Color32::WHITE);
}

/// A video format in a few characters, for badges: "2D", "180° 3D",
/// "360°", "Fisheye 190° 3D".
pub fn format_short(f: &fp_core::format::VideoFormat) -> String {
    use fp_core::format::{Projection, StereoLayout};
    let stereo = f.stereo != StereoLayout::Mono;
    let p = match f.projection {
        Projection::Flat => return if stereo { "3D".into() } else { "2D".into() },
        Projection::Equirect { h_fov, .. } => format!("{}°", h_fov.round()),
        Projection::Fisheye { fov } => {
            let fov = format!("{}", fov.round());
            tr!("format-short-fisheye", fov = fov)
        }
        Projection::Eac { h_fov } if h_fov >= 360.0 => "YouTube 360°".to_string(),
        Projection::Eac { h_fov } => format!("EAC {}°", h_fov.round()),
    };
    if stereo { format!("{p} 3D") } else { p }
}

/// Words that name a video's format in its file name, dropped from titles
/// ("Aurora Timelapse_360_TB" → "Aurora Timelapse").
const FORMAT_WORDS: &[&str] = &[
    "180",
    "190",
    "200",
    "220",
    "360",
    "2d",
    "3d",
    "3dh",
    "3dv",
    "sbs",
    "hsbs",
    "fsbs",
    "lr",
    "rl",
    "tb",
    "bt",
    "ou",
    "ab",
    "htb",
    "vr",
    "vr180",
    "vr360",
    "mono",
    "stereo",
    "fisheye",
    "fisheye190",
    "fisheye200",
    "mkx200",
    "mkx220",
    "rf52",
    "eac",
    "4k",
    "5k",
    "6k",
    "7k",
    "8k",
    "12k",
    "60fps",
    "30fps",
    "hevc",
    "h265",
    "x265",
    "h264",
    "x264",
    "av1",
];

/// A file-derived title as shown in the UI: separators as spaces, trailing
/// format tags dropped. Keeps the original when nothing else would remain.
pub fn display_title(raw: &str) -> String {
    let spaced: String = raw
        .chars()
        .map(|c| if c == '_' || c == '.' { ' ' } else { c })
        .collect();
    let is_tag = |w: &&str| {
        let w = w.trim_matches(['-', '(', ')', '[', ']']);
        FORMAT_WORDS.iter().any(|f| f.eq_ignore_ascii_case(w))
    };
    let mut words: Vec<&str> = spaced.split_whitespace().collect();
    if words.iter().all(is_tag) {
        return raw.to_string();
    }
    while words.last().is_some_and(is_tag) {
        words.pop();
    }
    let out = words.join(" ");
    let out = out.trim_end_matches([' ', '-']).to_string();
    if out.is_empty() { raw.to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_too_wide_to_fit_goes_against_the_left() {
        assert_eq!(fit(50.0, 20.0, 0.0, 100.0), 50.0);
        assert_eq!(fit(95.0, 20.0, 0.0, 100.0), 80.0);
        assert_eq!(fit(-5.0, 20.0, 0.0, 100.0), 0.0);
        // A seek bubble with a long marker name: wider than the bar.
        assert_eq!(fit(500.0, 2200.0, 48.0, 1100.0), 48.0);
    }

    #[test]
    fn titles_drop_format_tags() {
        assert_eq!(display_title("Aurora Timelapse_360_TB"), "Aurora Timelapse");
        assert_eq!(
            display_title("Sunset Beach Walk_180_LR"),
            "Sunset Beach Walk"
        );
        assert_eq!(display_title("my.trip.8K.180.SBS"), "my trip");
        assert_eq!(display_title("Garden Party"), "Garden Party");
        // A tag-only name stays as it was.
        assert_eq!(display_title("180_SBS"), "180_SBS");
        // Tags mid-title stay: only trailing ones are format.
        assert_eq!(display_title("Top 360 Moments_LR"), "Top 360 Moments");
    }

    #[test]
    fn the_thumbstick_steps_the_slider_it_points_at() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let pass = |events: Vec<egui::Event>, value: &mut f32| {
            let mut rect = Rect::NOTHING;
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    rect = ui
                        .horizontal(|ui| slider_control(ui, value, -10.0..=10.0, 0.0, "%", 1))
                        .response
                        .rect;
                });
            });
            rect
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut v = 1.23;
        let rect = pass(vec![], &mut v);
        assert!(!over_slider(&ctx));
        let on_rail = Pos2::new(rect.right() - 120.0, rect.center().y);
        pass(vec![egui::Event::PointerMoved(on_rail)], &mut v);
        assert!(over_slider(&ctx));

        // Steps of the shown precision, landing on it.
        pass(vec![key(egui::Key::ArrowRight)], &mut v);
        assert!((v - 1.3).abs() < 1e-5, "{v}");
        pass(
            vec![key(egui::Key::ArrowLeft), key(egui::Key::ArrowLeft)],
            &mut v,
        );
        assert!((v - 1.1).abs() < 1e-5, "{v}");

        let below = Pos2::new(on_rail.x, rect.bottom() + 100.0);
        pass(vec![egui::Event::PointerMoved(below)], &mut v);
        assert!(!over_slider(&ctx));
        pass(vec![key(egui::Key::ArrowRight)], &mut v);
        assert!((v - 1.1).abs() < 1e-5, "not pointed at: {v}");
    }

    #[test]
    fn a_press_on_the_knob_takes_it_where_it_is() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let pass = |events: Vec<egui::Event>, value: &mut f32| {
            let mut rect = Rect::NOTHING;
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    rect = ui
                        .horizontal(|ui| slider_control(ui, value, -10.0..=10.0, 0.0, "%", 1))
                        .response
                        .rect;
                });
            });
            rect
        };
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let moved = |pos| egui::Event::PointerMoved(pos);
        // The slider is the row's last 240 points: its rail runs from 229
        // to 11 points before the end, 0 in the middle.
        let mut v = 0.0;
        let rect = pass(vec![], &mut v);
        let knob = Pos2::new(rect.right() - 120.0, rect.center().y);
        let x = |dx: f32| knob + Vec2::new(dx, 0.0);

        // Pressed beside the knob, it stays, then goes as far as the
        // pointer: a quarter of the rail.
        pass(vec![moved(x(8.0)), button(x(8.0), true)], &mut v);
        assert_eq!(v, 0.0, "taken where it is");
        pass(vec![moved(x(8.0 + 54.5))], &mut v);
        assert!((v - 5.0).abs() < 1e-3, "{v}");
        pass(vec![button(x(62.5), false)], &mut v);
        assert!((v - 5.0).abs() < 1e-3, "{v}");

        // Pressed further along the rail, it jumps there: a quarter of the
        // way along is -5.
        let quarter = Pos2::new(rect.right() - 11.0 - 218.0 * 0.75, knob.y);
        pass(vec![moved(quarter), button(quarter, true)], &mut v);
        assert!((v + 5.0).abs() < 1e-3, "{v}");
        pass(vec![button(quarter, false)], &mut v);
        assert!((v + 5.0).abs() < 1e-3, "a click leaves it there: {v}");

        // A press's wander within the click distance doesn't move it, and a
        // drag goes on from there without a jump.
        pass(vec![moved(quarter), button(quarter, true)], &mut v);
        let near = quarter + Vec2::new(4.0, 0.0);
        pass(vec![moved(near)], &mut v);
        assert!((v + 5.0).abs() < 1e-3, "{v}");
        pass(vec![moved(near + Vec2::new(54.5, 0.0))], &mut v);
        assert!(v.abs() < 1e-3, "{v}");
        pass(vec![button(near + Vec2::new(54.5, 0.0), false)], &mut v);
    }
}
