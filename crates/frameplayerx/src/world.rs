//! UI panels placed in the world, and controller pointing at them.

use egui::{Event, Modifiers, PointerButton, Pos2};
use fp_render::{PanelId, QuadDraw, QuadTexture, Renderer};
use fp_xr::Hand;
use glam::{Mat4, Quat, Vec2, Vec3};
use std::time::{Duration, Instant};

/// A flat UI surface rendered with egui.
pub struct Panel {
    pub ctx: egui::Context,
    pub id: Option<PanelId>,
    /// Texture size in pixels.
    pub px: [u32; 2],
    /// Pixels per egui point, before [`Panel::set_scale`].
    pub ppp: f32,
    /// Settings › Interface size.
    scale: f32,
    /// See [`Panel::growing`].
    grows: bool,
    base_size: Vec2,
    /// Size in metres.
    pub size: Vec2,
    /// Centre and orientation; the panel faces +Z of this transform.
    pub pose: Mat4,
    pub visible: bool,
    /// Nothing on it to press or scroll: a ray stops on it and a grip takes
    /// it, but the trigger and stick act as if pointing at nothing.
    pub display_only: bool,
    /// The part of it that shows, in egui points, when that's less than all
    /// of it: rays pass through the rest.
    pub solid: Option<egui::Rect>,
    pub opacity: f32,
    /// 0 hidden → 1 shown, following `visible` over a short fade.
    fade: f32,
    events: Vec<Event>,
    hovered: bool,
    last_paint: Option<Instant>,
    /// Repaint at least this often while visible (live content).
    pub refresh: Option<Duration>,
    repaint_requested: bool,
}

impl Panel {
    pub fn new(px: [u32; 2], size: Vec2, ppp: f32) -> Panel {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(ppp);
        ctx.options_mut(|o| o.input_options.max_click_dist = CLICK_REACH);
        crate::ui::theme::apply(&ctx);
        Panel {
            ctx,
            id: None,
            px,
            ppp,
            scale: 1.0,
            grows: false,
            base_size: size,
            size,
            pose: Mat4::IDENTITY,
            visible: false,
            display_only: false,
            solid: None,
            opacity: 1.0,
            fade: 0.0,
            events: Vec::new(),
            hovered: false,
            last_paint: None,
            refresh: None,
            repaint_requested: true,
        }
    }

    /// Size of the panel in egui points.
    pub fn points(&self) -> egui::Vec2 {
        let ppp = if self.grows {
            self.ppp
        } else {
            self.ppp * self.scale
        };
        egui::vec2(self.px[0] as f32 / ppp, self.px[1] as f32 / ppp)
    }

    /// A panel whose layout is fixed (it cannot scroll or wrap): the whole
    /// panel grows with [`Panel::set_scale`] instead of its contents. Its
    /// texture should already be fine enough for the largest scale.
    pub fn growing(mut self) -> Panel {
        self.grows = true;
        self
    }

    /// Size in metres at scale 1.
    pub fn base_size(&self) -> Vec2 {
        self.base_size
    }

    /// Makes text and controls `scale` times their size: laid out afresh in
    /// the same panel, or for a [`Panel::growing`] one, the panel itself
    /// that much bigger.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        if self.grows {
            self.size = self.base_size * scale;
        } else {
            // egui's zoom is its pixels per point: the panel's times the
            // scale.
            self.ctx.set_pixels_per_point(self.ppp * scale);
        }
        self.repaint_requested = true;
    }

    /// Ray hit: distance along the ray and position in egui points.
    pub fn hit(&self, origin: Vec3, dir: Vec3) -> Option<(f32, Pos2)> {
        let inv = self.pose.inverse();
        let o = inv.transform_point3(origin);
        let d = inv.transform_vector3(dir);
        if d.z.abs() < 1e-6 {
            return None;
        }
        let t = -o.z / d.z;
        if t <= 0.0 {
            return None;
        }
        let p = o + d * t;
        let (hw, hh) = (self.size.x / 2.0, self.size.y / 2.0);
        if p.x.abs() > hw || p.y.abs() > hh {
            return None;
        }
        let u = p.x / self.size.x + 0.5;
        let v = 0.5 - p.y / self.size.y;
        let pts = self.points();
        let pos = Pos2::new(u * pts.x, v * pts.y);
        if self.solid.is_some_and(|s| !s.contains(pos)) {
            return None;
        }
        Some((t, pos))
    }

    /// World position of a point given in egui points.
    pub fn world_point(&self, p: Pos2) -> Vec3 {
        let pts = self.points();
        let x = (p.x / pts.x - 0.5) * self.size.x;
        let y = (0.5 - p.y / pts.y) * self.size.y;
        self.pose.transform_point3(Vec3::new(x, y, 0.002))
    }

    pub fn push(&mut self, e: Event) {
        self.events.push(e);
        self.repaint_requested = true;
    }

    pub fn request_repaint(&mut self) {
        self.repaint_requested = true;
    }

    /// Whether egui should run this frame.
    pub fn needs_paint(&self) -> bool {
        if !self.visible {
            return false;
        }
        if self.repaint_requested || !self.events.is_empty() || self.last_paint.is_none() {
            return true;
        }
        match (self.refresh, self.last_paint) {
            (Some(r), Some(t)) => t.elapsed() >= r,
            _ => false,
        }
    }

    /// egui's input for one pass: the queued events, over the whole panel.
    pub(crate) fn input(&mut self, time: f64) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, self.points())),
            time: Some(time),
            events: std::mem::take(&mut self.events),
            focused: true,
            ..Default::default()
        }
    }

    /// Runs egui with the queued input and paints into the renderer.
    pub fn paint(
        &mut self,
        renderer: &mut Renderer,
        time: f64,
        ui: impl FnMut(&egui::Context),
    ) -> fp_render::Result<()> {
        if self.id.is_none() {
            self.id = Some(renderer.create_panel(self.px[0], self.px[1])?);
        }
        let input = self.input(time);
        let hovered = self.hovered;
        let mut ui = ui;
        let out = self.ctx.run(input, |ctx| {
            ui(ctx);
            // The pointer's dot, drawn with the panel so it sits exactly on
            // it (a quad layer would cover one drawn in the scene).
            if hovered && let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Debug,
                    egui::Id::new("pointer-dot"),
                ));
                painter.circle_filled(p, 7.0, egui::Color32::from_black_alpha(110));
                painter.circle_filled(p, 5.0, egui::Color32::WHITE);
            }
        });
        let repaint = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay < Duration::from_millis(100))
            .unwrap_or(false);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        if let Some(id) = self.id {
            renderer.paint_panel(id, &prims, &out.textures_delta, out.pixels_per_point)?;
        }
        self.last_paint = Some(Instant::now());
        self.repaint_requested = repaint;
        Ok(())
    }

    /// Moves the fade towards `visible`: in over 0.15 s, out over 0.25 s.
    pub fn update_fade(&mut self, dt: f32) {
        self.fade = if self.visible {
            (self.fade + dt / 0.15).min(1.0)
        } else {
            (self.fade - dt / 0.25).max(0.0)
        };
    }

    /// Visible and done fading in: nothing about its look is animating.
    pub fn fully_shown(&self) -> bool {
        self.visible && self.fade >= 1.0 && self.opacity >= 0.999
    }

    /// Drawn at all: visible, or still fading out.
    pub fn shown(&self) -> bool {
        self.fade > 0.01 || self.visible
    }

    pub fn quad(&self) -> Option<QuadDraw> {
        let id = self.id?;
        // Smoothstep, so the fade starts and ends gently.
        let f = self.fade * self.fade * (3.0 - 2.0 * self.fade);
        let opacity = self.opacity * f;
        (self.shown() && opacity > 0.01).then(|| {
            QuadDraw::panel(
                QuadTexture::Panel(id),
                self.pose,
                self.size.x,
                self.size.y,
                opacity,
            )
        })
    }

    pub fn wants_keyboard(&self) -> bool {
        self.ctx.wants_keyboard_input()
    }
}

/// Places a panel `distance` metres in front of `anchor` (a yaw-only pose at
/// eye height), `offset_y` metres up, rotated `yaw_deg` around the viewer
/// (positive = to the left). Positive `tilt_deg` turns the panel's face
/// upwards, for panels below eye level.
pub fn place(anchor: Mat4, yaw_deg: f32, distance: f32, offset_y: f32, tilt_deg: f32) -> Mat4 {
    anchor
        * Mat4::from_rotation_y(yaw_deg.to_radians())
        * Mat4::from_translation(Vec3::new(0.0, offset_y, -distance))
        * Mat4::from_rotation_x(-tilt_deg.to_radians())
}

/// A panel `distance` metres away, turned `yaw_deg` (left positive) and
/// `pitch_deg` (up positive) about the viewer, facing them.
pub fn place_around(
    anchor: Mat4,
    yaw_deg: f32,
    pitch_deg: f32,
    distance: f32,
    offset_y: f32,
) -> Mat4 {
    anchor
        * Mat4::from_rotation_y(yaw_deg.to_radians())
        * Mat4::from_rotation_x(pitch_deg.to_radians())
        * Mat4::from_translation(Vec3::new(0.0, offset_y, -distance))
}

/// A panel `below_deg` below where you face, `distance` metres away, but
/// upright like a flat screen rather than tilted back towards your eyes;
/// turned `yaw_deg` and `pitch_deg` about the viewer by where it was moved.
pub fn place_below(
    anchor: Mat4,
    yaw_deg: f32,
    pitch_deg: f32,
    distance: f32,
    below_deg: f32,
) -> Mat4 {
    let below = below_deg.to_radians();
    place_around(
        anchor,
        yaw_deg,
        pitch_deg,
        distance * below.cos(),
        -distance * below.sin(),
    )
}

/// `anchor` turned about the viewer the way the renderer turns a 180°/360°
/// picture for its yaw and pitch (degrees, left and up positive), so a panel
/// placed from it moves with the picture. Roll is left out: it levels a
/// crooked camera, and a panel rolled with it would no longer be level.
pub fn turned_with_picture(anchor: Mat4, yaw_deg: f32, pitch_deg: f32) -> Mat4 {
    anchor
        * Mat4::from_euler(
            glam::EulerRot::YXZ,
            yaw_deg.to_radians(),
            pitch_deg.to_radians(),
            0.0,
        )
}

/// The player the control bar is placed from: below a flat screen turned
/// `picture` (yaw and pitch, degrees), so it goes along when the screen
/// moves; in the room (`anchor`) during a 180°/360° video, whose yaw and
/// pitch turn only the picture. Then turned by where the bar itself was
/// moved, `bar`.
pub fn player_frame(anchor: Mat4, flat: bool, picture: [f32; 2], bar: [f32; 2]) -> Mat4 {
    let from = if flat {
        turned_with_picture(anchor, picture[0], picture[1])
    } else {
        anchor
    };
    place_around(from, bar[0], bar[1], 0.0, 0.0)
}

/// Yaw-only anchor at the head position.
pub fn anchor_from_head(position: Vec3, orientation: Quat) -> Mat4 {
    let (yaw, _, _) = orientation.to_euler(glam::EulerRot::YXZ);
    Mat4::from_rotation_translation(Quat::from_rotation_y(yaw), position)
}

/// Looking less than this far up or down, a recenter levels out: what
/// you'd see as straight ahead (a seated glance a little up or down) puts
/// a flat screen upright at eye height, not tilted and raised or lowered
/// with the glance. Lying back looks further up than this.
const LEVEL_WITHIN_DEG: f32 = 20.0;

/// The anchor a recenter sets: at the head position, facing where it looks,
/// up or down too (level when nearly so, see [`LEVEL_WITHIN_DEG`]), but
/// never rolled.
pub fn anchor_facing(position: Vec3, orientation: Quat) -> Mat4 {
    let d = orientation * Vec3::NEG_Z;
    let yaw = (-d.x).atan2(-d.z);
    let pitch = d.y.clamp(-1.0, 1.0).asin();
    let pitch = if pitch.abs() < LEVEL_WITHIN_DEG.to_radians() {
        0.0
    } else {
        pitch
    };
    Mat4::from_rotation_translation(
        Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch),
        position,
    )
}

/// How closely a followed anchor trails the head: the gap halves every this
/// many seconds.
const FOLLOW_HALF_LIFE_S: f32 = 0.1;

/// The Follow setting: `anchor` moved `dt` seconds' worth towards the head's
/// pose (turn, tilt and position), so what is placed from it stays in front
/// of you.
pub fn follow(anchor: Mat4, head: (Vec3, Quat), dt: f32) -> Mat4 {
    let (_, rot, pos) = anchor.to_scale_rotation_translation();
    let k = 1.0 - 0.5_f32.powf(dt.max(0.0) / FOLLOW_HALF_LIFE_S);
    Mat4::from_rotation_translation(rot.slerp(head.1, k), pos.lerp(head.0, k))
}

/// Per-hand trigger state with hysteresis.
#[derive(Default, Clone, Copy)]
struct Trigger {
    down: bool,
    /// Pulled past the press point on the same frame as the other hand: held
    /// back a frame so egui sees each press at its own spot.
    pending: bool,
}

const TRIGGER_PRESS: f32 = 0.6;
const TRIGGER_RELEASE: f32 = 0.45;

impl Trigger {
    /// Returns Some(pressed) on a transition.
    fn update(&mut self, value: f32, allow_press: bool) -> Option<bool> {
        if !self.down {
            if value > TRIGGER_PRESS || (self.pending && value >= TRIGGER_RELEASE) {
                if allow_press {
                    self.down = true;
                    self.pending = false;
                    return Some(true);
                }
                self.pending = true;
            } else if value < TRIGGER_RELEASE {
                self.pending = false;
            }
            None
        } else if value < TRIGGER_RELEASE {
            self.down = false;
            Some(false)
        } else {
            None
        }
    }
}

/// Where each hand points this frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Aim {
    pub origin: Vec3,
    pub dir: Vec3,
    /// Index into the panel list and the hit position.
    pub hit: Option<(usize, f32, Pos2)>,
}

/// How far (in panel points) the pointer may move during a press and still
/// click; beyond it, the press drags. egui's default of 6 is for a mouse: a
/// pulled trigger tips a controller further than that. This is about two
/// degrees of the ray (the main panel is about 18 points a degree).
const CLICK_REACH: f32 = 40.0;

#[derive(Default)]
pub struct Pointer {
    triggers: [Trigger; 2],
    /// Each hand's grip was held last frame.
    grips: [bool; 2],
    /// The hand driving the UI.
    pub active: usize,
    /// Panel that received the last press, so the release goes there too.
    pressed_panel: Option<usize>,
    /// The hand whose trigger made that press: only its let-go ends it.
    pressed_hand: Option<usize>,
    hovered_panel: Option<usize>,
    /// That panel is display-only.
    hovering_display_only: bool,
    /// The rays are being drawn. A display-only panel shows where one lands
    /// only then: it's up while you watch, with the rays hidden.
    pub rays_shown: bool,
    pub aims: [Option<Aim>; 2],
}

/// Result of routing controller input to panels.
#[derive(Default, Debug, Clone, Copy)]
pub struct Routed {
    /// The active hand points at some panel.
    pub over_ui: bool,
    /// A trigger was pressed on a panel this frame: which hand, for haptics.
    pub pressed: Option<usize>,
    /// A trigger was pressed while not pointing at any panel.
    pub click_outside: bool,
    /// A grip was squeezed while that hand pointed at a panel: (panel, hand),
    /// to move the panel.
    pub grab: Option<(usize, usize)>,
}

impl Pointer {
    /// Ends the press on its panel, where `hit` (the pressing hand's ray) is
    /// if that's still on the panel. A press let go off its panel isn't a
    /// click.
    fn let_go(&mut self, panels: &mut [&mut Panel], hit: Option<(usize, f32, Pos2)>) {
        self.pressed_hand = None;
        let Some(pi) = self.pressed_panel.take() else {
            return;
        };
        let pos = match hit {
            Some((p, _, pos)) if p == pi => pos,
            _ => Pos2::new(-1.0, -1.0),
        };
        panels[pi].push(Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        });
    }

    /// Routes the hands' rays and triggers into panel events.
    pub fn route(&mut self, panels: &mut [&mut Panel], hands: &[Hand; 2], dt: f32) -> Routed {
        let mut routed = Routed::default();
        for (i, h) in hands.iter().enumerate() {
            self.aims[i] = h.aim.map(|(pos, rot)| {
                let dir = rot * Vec3::NEG_Z;
                let mut best: Option<(usize, f32, Pos2)> = None;
                for (pi, p) in panels.iter().enumerate() {
                    if !p.visible {
                        continue;
                    }
                    if let Some((t, pt)) = p.hit(pos, dir)
                        && best.is_none_or(|b| t < b.1)
                    {
                        best = Some((pi, t, pt));
                    }
                }
                Aim {
                    origin: pos,
                    dir,
                    hit: best,
                }
            });
        }
        // A trigger press makes that hand the active one. If both cross the
        // press point on the same frame, the second waits a frame so egui
        // sees each press at its own spot.
        let mut transitions = [None, None];
        for (i, h) in hands.iter().enumerate() {
            let allow_press = transitions.iter().all(|t| *t != Some(true));
            transitions[i] = self.triggers[i].update(h.trigger, allow_press);
            if transitions[i] == Some(true) {
                self.active = i;
            }
        }
        if self.aims[self.active].is_none()
            && let Some(other) = (0..2).find(|&i| self.aims[i].is_some())
        {
            self.active = other;
        }
        let aim = self.aims[self.active];
        // The panels have one pointer, so a press is one hand's. The other
        // hand pressing lets go of it first, before the pointer moves to
        // where that hand points: typing with both hands, the next key is
        // often pressed before the last is let go. Without this egui saw a
        // second press on a button already down, gave that key's click to
        // the first, and the first hand's let-go was lost with it.
        if transitions[self.active] == Some(true)
            && let Some(h) = self.pressed_hand.filter(|&h| h != self.active)
        {
            let held = self.aims[h].and_then(|a| a.hit);
            self.let_go(panels, held);
        }
        let hit = aim.and_then(|a| a.hit);
        let display_only = |panels: &[&mut Panel], h: &(usize, f32, Pos2)| panels[h.0].display_only;
        // Hover changes.
        let hover = hit.filter(|h| self.rays_shown || !display_only(panels, h));
        self.hovering_display_only = hover.is_some_and(|h| display_only(panels, &h));
        let target = hover.map(|h| h.0);
        if self.hovered_panel != target {
            if let Some(old) = self.hovered_panel
                && let Some(p) = panels.get_mut(old)
            {
                p.push(Event::PointerGone);
                p.hovered = false;
            }
            self.hovered_panel = target;
        }
        if let Some((pi, _, pos)) = hover {
            let p = &mut panels[pi];
            routed.over_ui = !p.display_only;
            p.hovered = true;
            // egui takes a press as a click until the pointer has been too
            // far from it; a list has taken this one.
            if crate::ui::widgets::list_took_press(&p.ctx) {
                p.push(Event::PointerMoved(pos + egui::Vec2::splat(1e4)));
            }
            p.push(Event::PointerMoved(pos));
            let stick = hands[self.active].stick;
            if stick.y.abs() > 0.2 && !p.display_only {
                p.push(Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, stick.y * 900.0 * dt),
                    modifiers: Modifiers::NONE,
                });
            }
        }
        // Squeezing a grip while pointing at a panel grabs it; the trigger
        // doesn't click while that grip is held.
        for (i, h) in hands.iter().enumerate() {
            let grip = h.squeeze > crate::controls::GRIP;
            if grip
                && !self.grips[i]
                && let Some((pi, _, _)) = self.aims[i].and_then(|a| a.hit)
            {
                routed.grab = Some((pi, i));
            }
            self.grips[i] = grip;
        }
        let gripping = self.grips[self.active];
        match transitions[self.active] {
            Some(true) if gripping && hit.is_some() => {}
            Some(true) => {
                if let Some((pi, _, pos)) = hit.filter(|h| !display_only(panels, h)) {
                    panels[pi].push(Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    });
                    routed.pressed = Some(self.active);
                    self.pressed_panel = Some(pi);
                    self.pressed_hand = Some(self.active);
                } else {
                    routed.click_outside = true;
                }
            }
            _ => {}
        }
        // Let go by the hand that pressed, whether or not it is still the
        // active one.
        if let Some(h) = self.pressed_hand
            && transitions[h] == Some(false)
        {
            let held = self.aims[h].and_then(|a| a.hit);
            self.let_go(panels, held);
        }
        routed
    }

    /// The panel the active hand points at.
    pub fn hovered(&self) -> Option<usize> {
        self.hovered_panel.filter(|_| !self.hovering_display_only)
    }

    /// The panel the trigger is held on, from press to release.
    pub fn pressed(&self) -> Option<usize> {
        self.pressed_panel
    }

    /// Pointer rays and cursor dots to draw.
    pub fn quads(&self, eye: Vec3, show: bool) -> Vec<QuadDraw> {
        let mut out = Vec::new();
        if !show {
            return out;
        }
        for (i, aim) in self.aims.iter().enumerate() {
            let Some(a) = aim else { continue };
            let active = i == self.active;
            let len = a.hit.map(|h| h.1).unwrap_or(2.0).min(5.0);
            let end = a.origin + a.dir * len;
            let alpha = if active { 0.85 } else { 0.35 };
            let color = [0.55 * alpha, 0.75 * alpha, 1.0 * alpha, alpha];
            out.push(QuadDraw::line(
                a.origin + a.dir * 0.02,
                end,
                if active { 0.004 } else { 0.0025 },
                eye,
                color,
            ));
        }
        out
    }

    /// The rays as quad layers: each a thin strip from the hand to what it
    /// points at, turned to face the eye. (The dot is drawn on the panel.)
    pub fn ray_layers(&self, eye: Vec3, show: bool) -> Vec<crate::app::LayerDraw> {
        let mut out = Vec::new();
        if !show {
            return out;
        }
        for (i, aim) in self.aims.iter().enumerate() {
            let Some(a) = aim else { continue };
            let active = i == self.active;
            let len = a.hit.map(|h| h.1).unwrap_or(2.0).min(5.0) - 0.02;
            if len <= 0.01 {
                continue;
            }
            let start = a.origin + a.dir * 0.02;
            let centre = start + a.dir * (len / 2.0);
            let x = a.dir.normalize_or_zero();
            let to_eye = eye - centre;
            let z = (to_eye - x * to_eye.dot(x)).normalize_or_zero();
            if z == Vec3::ZERO {
                continue;
            }
            let y = z.cross(x);
            let rot = Quat::from_mat3(&glam::Mat3::from_cols(x, y, z));
            out.push(crate::app::LayerDraw::Ray {
                active,
                pose: (centre, rot),
                size: Vec2::new(len, if active { 0.006 } else { 0.004 }),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::widgets::ListArea;

    fn panel_in_front() -> Panel {
        let mut p = Panel::new([800, 400], Vec2::new(1.6, 0.8), 1.0);
        p.pose = Mat4::from_translation(Vec3::new(0.0, 0.0, -2.0));
        p.visible = true;
        p
    }

    #[test]
    fn ray_hits_map_to_points() {
        let p = panel_in_front();
        let (t, pt) = p.hit(Vec3::ZERO, Vec3::NEG_Z).unwrap();
        assert!((t - 2.0).abs() < 1e-5);
        assert!((pt.x - 400.0).abs() < 0.5 && (pt.y - 200.0).abs() < 0.5);
        // Upper-left corner region.
        let dir = (Vec3::new(-0.7, 0.35, -2.0)).normalize();
        let (_, pt) = p.hit(Vec3::ZERO, dir).unwrap();
        assert!(pt.x < 60.0 && pt.y < 30.0, "{pt:?}");
        assert!(p.hit(Vec3::ZERO, Vec3::Z).is_none(), "behind the viewer");
        assert!(
            p.hit(Vec3::ZERO, Vec3::new(1.0, 0.0, -0.1).normalize())
                .is_none(),
            "off to the side"
        );
        let back = p.world_point(pt);
        assert!((back.z + 2.0).abs() < 0.01);
    }

    /// A hand pointing at (x, y) in the points of [`panel_in_front`].
    fn at(x: f32, y: f32) -> Option<(Vec3, Quat)> {
        let dir = Vec3::new((x - 400.0) / 500.0, (200.0 - y) / 500.0, -2.0).normalize();
        Some((Vec3::ZERO, Quat::from_rotation_arc(Vec3::NEG_Z, dir)))
    }

    /// A frame's pass over the panel, a frame after the last.
    fn pass(
        p: &mut Panel,
        ptr: &mut Pointer,
        hands: &[Hand; 2],
        time: &mut f64,
        ui: impl FnMut(&mut egui::Ui),
    ) {
        ptr.route(&mut [&mut *p], hands, 0.011);
        *time += 0.011;
        let input = p.input(*time);
        let mut ui = ui;
        let _ = p.ctx.run(input, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |u| {
                    ui(u);
                });
        });
    }

    #[test]
    fn the_pointer_stays_on_the_ray() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        // The controller tips with the pull, wanders while held, tips back.
        let script = [
            (0.0, 400.0, 200.0),
            (0.3, 403.0, 204.0),
            (0.7, 406.0, 208.0),
            (1.0, 409.0, 212.0),
            (1.0, 414.0, 209.0),
            (0.6, 411.0, 206.0),
            (0.3, 405.0, 203.0),
            (0.0, 401.0, 200.0),
        ];
        let mut buttons = 0;
        for (v, x, y) in script {
            hands[1].trigger = v;
            hands[1].aim = at(x, y);
            p.events.clear();
            ptr.route(&mut [&mut p], &hands, 0.011);
            let ray = Pos2::new(x, y);
            for e in &p.events {
                let pos = match e {
                    Event::PointerMoved(pos) => *pos,
                    Event::PointerButton { pos, .. } => {
                        buttons += 1;
                        *pos
                    }
                    _ => continue,
                };
                assert!(pos.distance(ray) < 0.05, "{pos:?} at {ray:?}");
            }
        }
        assert_eq!(buttons, 2);
    }

    #[test]
    fn a_press_that_wanders_is_still_a_click_up_to_the_click_distance() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        let mut time = 0.0;
        let mut clicks = 0;
        let mut frame = |p: &mut Panel, ptr: &mut Pointer, hands: &[Hand; 2]| {
            pass(p, ptr, hands, &mut time, |ui| {
                let size = egui::vec2(800.0, 400.0);
                if ui
                    .allocate_exact_size(size, egui::Sense::click())
                    .1
                    .clicked()
                {
                    clicks += 1;
                }
            });
        };
        hands[1].aim = at(400.0, 200.0);
        frame(&mut p, &mut ptr, &hands);
        frame(&mut p, &mut ptr, &hands);
        let mut press_and_wander = |by: f32| {
            hands[1].aim = at(400.0, 200.0);
            frame(&mut p, &mut ptr, &hands);
            hands[1].trigger = 1.0;
            frame(&mut p, &mut ptr, &hands);
            hands[1].aim = at(400.0 + by, 200.0);
            frame(&mut p, &mut ptr, &hands);
            hands[1].trigger = 0.0;
            frame(&mut p, &mut ptr, &hands);
        };
        press_and_wander(CLICK_REACH - 5.0);
        press_and_wander(CLICK_REACH + 5.0);
        assert_eq!(clicks, 1);
    }

    /// A header, then a list of `rows` below it (50 points each): its scroll
    /// offset and clicked rows.
    fn list(ui: &mut egui::Ui, rows: usize, offset: &mut f32, clicks: &mut u32) {
        ui.allocate_exact_size(egui::vec2(800.0, 100.0), egui::Sense::hover());
        let out = egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_list(ui, |ui| {
                for _ in 0..rows {
                    let size = egui::vec2(800.0, 50.0);
                    if ui
                        .allocate_exact_size(size, egui::Sense::click())
                        .1
                        .clicked()
                    {
                        *clicks += 1;
                    }
                }
            });
        *offset = out.state.offset.y;
    }

    #[test]
    fn a_list_takes_a_press_along_its_direction_and_follows_it_until_the_let_go() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        let mut time = 0.0;
        let (mut offset, mut clicks) = (0.0, 0);
        let mut frame = |p: &mut Panel, ptr: &mut Pointer, hands: &[Hand; 2]| {
            pass(p, ptr, hands, &mut time, |ui| {
                list(ui, 40, &mut offset, &mut clicks);
            });
            (offset, clicks)
        };
        hands[1].aim = at(400.0, 350.0);
        for _ in 0..3 {
            frame(&mut p, &mut ptr, &hands);
        }

        // A click's wander up and down leaves the list where it is, and it is
        // still a click.
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        hands[1].aim = at(400.0, 330.0);
        assert_eq!(frame(&mut p, &mut ptr, &hands).0, 0.0);
        hands[1].trigger = 0.0;
        assert_eq!(frame(&mut p, &mut ptr, &hands), (0.0, 1));

        // Sideways is nothing to a list, up to the click distance.
        hands[1].aim = at(400.0, 350.0);
        frame(&mut p, &mut ptr, &hands);
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        hands[1].aim = at(435.0, 350.0);
        assert_eq!(frame(&mut p, &mut ptr, &hands).0, 0.0);
        hands[1].trigger = 0.0;
        assert_eq!(frame(&mut p, &mut ptr, &hands), (0.0, 2));

        // Past its reach it takes the press: point for point from there, with
        // no jump, and on past its own edge (the header starts at 100).
        hands[1].aim = at(400.0, 350.0);
        frame(&mut p, &mut ptr, &hands);
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        let mut last = 350.0;
        let mut want = 0.0;
        for y in [335.0, 325.0, 315.0, 290.0, 240.0, 140.0, 60.0] {
            hands[1].aim = at(400.0, y);
            if 350.0 - y > 25.0 {
                want += last - y;
            }
            last = y;
            let (got, _) = frame(&mut p, &mut ptr, &hands);
            assert!((got - want).abs() < 0.1, "at {y}: {got}, not {want}");
        }
        // Held still so as not to flick; a press it took isn't a click.
        for _ in 0..15 {
            frame(&mut p, &mut ptr, &hands);
        }
        hands[1].trigger = 0.0;
        frame(&mut p, &mut ptr, &hands);
        for _ in 0..20 {
            frame(&mut p, &mut ptr, &hands);
        }
        let (rested, clicked) = frame(&mut p, &mut ptr, &hands);
        assert!((rested - want).abs() < 0.1, "{rested}, not {want}");
        assert_eq!(clicked, 2);

        // Flicked, it coasts on after the let-go.
        hands[1].aim = at(400.0, 350.0);
        frame(&mut p, &mut ptr, &hands);
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        for y in [320.0, 290.0, 260.0, 230.0, 200.0] {
            hands[1].aim = at(400.0, y);
            frame(&mut p, &mut ptr, &hands);
        }
        hands[1].trigger = 0.0;
        let (flicked, _) = frame(&mut p, &mut ptr, &hands);
        for _ in 0..100 {
            frame(&mut p, &mut ptr, &hands);
        }
        let (coasted, _) = frame(&mut p, &mut ptr, &hands);
        assert!(coasted > flicked + 100.0, "{coasted} {flicked}");
    }

    #[test]
    fn a_list_let_go_off_its_panel_stays_where_it_is() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        let mut time = 0.0;
        let (mut offset, mut clicks) = (0.0, 0);
        let mut frame = |p: &mut Panel, ptr: &mut Pointer, hands: &[Hand; 2]| {
            pass(p, ptr, hands, &mut time, |ui| {
                list(ui, 40, &mut offset, &mut clicks);
            });
            offset
        };
        hands[1].aim = at(400.0, 350.0);
        for _ in 0..3 {
            frame(&mut p, &mut ptr, &hands);
        }
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        for y in [320.0, 300.0, 280.0] {
            hands[1].aim = at(400.0, y);
            frame(&mut p, &mut ptr, &hands);
        }
        let dragged = frame(&mut p, &mut ptr, &hands);
        hands[1].aim = at(400.0, -300.0);
        hands[1].trigger = 0.0;
        frame(&mut p, &mut ptr, &hands);
        hands[1].aim = at(400.0, 350.0);
        for _ in 0..30 {
            frame(&mut p, &mut ptr, &hands);
        }
        assert_eq!(frame(&mut p, &mut ptr, &hands), dragged);
    }

    #[test]
    fn a_list_that_fits_never_takes_a_press() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        let mut time = 0.0;
        let (mut offset, mut clicks) = (0.0, 0);
        let mut frame = |p: &mut Panel, ptr: &mut Pointer, hands: &[Hand; 2]| {
            pass(p, ptr, hands, &mut time, |ui| {
                list(ui, 4, &mut offset, &mut clicks);
            });
            (offset, clicks)
        };
        hands[1].aim = at(400.0, 250.0);
        for _ in 0..3 {
            frame(&mut p, &mut ptr, &hands);
        }
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        hands[1].aim = at(400.0, 220.0);
        frame(&mut p, &mut ptr, &hands);
        hands[1].trigger = 0.0;
        assert_eq!(frame(&mut p, &mut ptr, &hands), (0.0, 1));
    }

    #[test]
    fn a_press_on_a_slider_moves_the_slider_not_its_list() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        let mut time = 0.0;
        let (mut offset, mut value) = (0.0, 0.5);
        let mut frame = |p: &mut Panel, ptr: &mut Pointer, hands: &[Hand; 2]| {
            pass(p, ptr, hands, &mut time, |ui| {
                let out = egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show_list(ui, |ui| {
                        crate::ui::widgets::slider(ui, &mut value, 0.0..=1.0, 700.0);
                        ui.allocate_exact_size(egui::vec2(800.0, 2000.0), egui::Sense::hover());
                    });
                offset = out.state.offset.y;
            });
            (offset, value)
        };
        hands[1].aim = at(350.0, 18.0);
        for _ in 0..3 {
            frame(&mut p, &mut ptr, &hands);
        }
        assert!(crate::ui::widgets::over_slider(&p.ctx));
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands);
        // Within the click distance the knob stays.
        hands[1].aim = at(365.0, 25.0);
        let (_, v) = frame(&mut p, &mut ptr, &hands);
        assert!((v - 0.5).abs() < 1e-3, "{v}");
        // Beyond it, down and right: the knob moves, the list doesn't.
        for (x, y) in [(400.0, 40.0), (440.0, 60.0), (480.0, 80.0)] {
            hands[1].aim = at(x, y);
            frame(&mut p, &mut ptr, &hands);
        }
        let (scrolled, v) = frame(&mut p, &mut ptr, &hands);
        assert_eq!(scrolled, 0.0);
        assert!(v > 0.6, "{v}");
    }

    #[test]
    fn trigger_clicks_go_to_the_panel() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        hands[1].active = true;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(r.over_ui);
        assert_eq!(r.pressed, None);
        hands[1].trigger = 0.9;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert_eq!(r.pressed, Some(1));
        hands[1].trigger = 0.5; // hysteresis: still down
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert_eq!(r.pressed, None);
        // A quick double-click only needs to lift past 0.45 and pull past 0.6.
        hands[1].trigger = 0.4;
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].trigger = 0.65;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert_eq!(r.pressed, Some(1));
        hands[1].trigger = 0.1;
        ptr.route(&mut [&mut p], &hands, 0.016);
        let presses: Vec<bool> = p
            .events
            .iter()
            .filter_map(|e| match e {
                Event::PointerButton { pressed, .. } => Some(*pressed),
                _ => None,
            })
            .collect();
        assert_eq!(presses, vec![true, false, true, false]);
        assert_eq!(ptr.active, 1);
    }

    #[test]
    fn typing_with_both_hands_clicks_every_key() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        // Left hand on key A (left half), right hand on key B (right half).
        let at = |x: f32| {
            let dir = Vec3::new(x, 0.0, -2.0).normalize();
            Some((Vec3::ZERO, Quat::from_rotation_arc(Vec3::NEG_Z, dir)))
        };
        hands[0].aim = at(-0.4);
        hands[1].aim = at(0.4);
        let mut time = 0.0;
        let mut typed = Vec::new();
        let mut frame =
            |p: &mut Panel, ptr: &mut Pointer, hands: &[Hand; 2], typed: &mut Vec<char>| {
                ptr.route(&mut [&mut *p], hands, 0.016);
                time += 0.016;
                let input = p.input(time);
                let _ = p.ctx.run(input, |ctx| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(ctx, |ui| {
                            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                            ui.horizontal(|ui| {
                                for k in ['A', 'B'] {
                                    let size = egui::vec2(400.0, 400.0);
                                    let (_, r) = ui.allocate_exact_size(size, egui::Sense::click());
                                    if r.clicked() {
                                        typed.push(k);
                                    }
                                }
                            });
                        });
                });
            };
        // Laid out before egui can tell what the pointer is on.
        frame(&mut p, &mut ptr, &hands, &mut typed);
        frame(&mut p, &mut ptr, &hands, &mut typed);
        // A pressed, then B pressed before A is let go: a quick "AB".
        hands[0].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands, &mut typed);
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands, &mut typed);
        hands[0].trigger = 0.0;
        frame(&mut p, &mut ptr, &hands, &mut typed);
        hands[1].trigger = 0.0;
        frame(&mut p, &mut ptr, &hands, &mut typed);
        assert_eq!(typed, vec!['A', 'B']);

        // Both triggers crossing the press point on the exact same frame still
        // click both keys across consecutive frames.
        typed.clear();
        hands[0].trigger = 1.0;
        hands[1].trigger = 1.0;
        frame(&mut p, &mut ptr, &hands, &mut typed);
        frame(&mut p, &mut ptr, &hands, &mut typed);
        hands[0].trigger = 0.0;
        hands[1].trigger = 0.0;
        frame(&mut p, &mut ptr, &hands, &mut typed);
        assert_eq!(typed, vec!['A', 'B']);
    }

    #[test]
    fn a_display_only_panel_stops_the_ray_and_takes_a_grip_and_nothing_else() {
        let mut p = panel_in_front();
        p.display_only = true;
        let mut ptr = Pointer {
            rays_shown: true,
            ..Default::default()
        };
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        hands[1].stick = Vec2::new(0.0, 1.0);
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(ptr.aims[1].unwrap().hit.is_some(), "the ray stops on it");
        assert!(p.hovered, "with its dot");
        assert!(!r.over_ui, "the stick still seeks");
        assert_eq!(ptr.hovered(), None, "nothing to page");
        assert!(
            !p.events
                .iter()
                .any(|e| matches!(e, Event::MouseWheel { .. }))
        );

        hands[1].trigger = 0.9;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(r.click_outside, "the controls still come back");
        assert_eq!(r.pressed, None);
        assert_eq!(ptr.pressed(), None);
        hands[1].trigger = 0.1;
        ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(
            !p.events
                .iter()
                .any(|e| matches!(e, Event::PointerButton { .. }))
        );

        hands[1].squeeze = 1.0;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert_eq!(r.grab, Some((0, 1)));
    }

    #[test]
    fn with_the_rays_hidden_a_display_only_panel_shows_no_dot_but_a_grip_still_takes_it() {
        let mut p = panel_in_front();
        p.display_only = true;
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(!p.hovered);
        assert!(
            p.events.is_empty(),
            "no repaint for every frame it's aimed at"
        );
        hands[1].squeeze = 1.0;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert_eq!(r.grab, Some((0, 1)));
    }

    #[test]
    fn rays_pass_through_the_part_of_a_panel_that_doesnt_show() {
        let mut p = panel_in_front();
        let pts = p.points();
        assert!(p.hit(Vec3::ZERO, Vec3::NEG_Z).is_some());
        p.solid = Some(egui::Rect::from_min_size(
            Pos2::ZERO,
            egui::vec2(pts.x, pts.y / 4.0),
        ));
        assert!(
            p.hit(Vec3::ZERO, Vec3::NEG_Z).is_none(),
            "the middle is empty"
        );
        let up = Vec3::new(0.0, p.size.y * 0.4, -p.pose.w_axis.truncate().length());
        assert!(p.hit(Vec3::ZERO, up.normalize()).is_some(), "the top shows");
    }

    #[test]
    fn clicking_empty_space_is_reported() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[0].aim = Some((Vec3::ZERO, Quat::from_rotation_y(std::f32::consts::PI)));
        hands[0].trigger = 1.0;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(r.click_outside && !r.over_ui);
        assert_eq!(ptr.active, 0);
    }

    #[test]
    fn turned_panels_follow_the_picture() {
        let head = Vec3::new(0.0, 1.6, 0.0);
        let anchor = anchor_from_head(head, Quat::from_rotation_y(0.4));
        let anchor_rot = anchor.to_scale_rotation_translation().1;
        let (yaw, pitch) = (35.0_f32, -20.0_f32);
        // Where the renderer shows the picture's centre: rays are turned by
        // the inverse of the anchor's rotation, then of the picture's
        // (fp-render params.rs).
        let picture = (Mat4::from_quat(anchor_rot)
            * Mat4::from_euler(
                glam::EulerRot::YXZ,
                yaw.to_radians(),
                pitch.to_radians(),
                0.0,
            ))
        .transform_vector3(Vec3::NEG_Z);
        let turned = turned_with_picture(anchor, yaw, pitch);
        let ahead =
            (place(turned, 0.0, 1.0, 0.0, 0.0).transform_point3(Vec3::ZERO) - head).normalize();
        assert!(ahead.distance(picture) < 1e-4, "{ahead} vs {picture}");
        assert!(ahead.y < -0.3, "negative pitch moves it down");
        // The bar's spot below the view stays below it, at the same distance.
        let bar = place(turned, 0.0, 1.05, -0.32, 22.0).transform_point3(Vec3::ZERO);
        let rest = place(anchor, 0.0, 1.05, -0.32, 22.0).transform_point3(Vec3::ZERO);
        assert!(((bar - head).length() - (rest - head).length()).abs() < 1e-4);
        assert!((bar - head).dot(picture) > 0.9 * (bar - head).length());
        // No roll: the panel's right edge stays level.
        let right = turned.transform_vector3(Vec3::X);
        assert!(right.y.abs() < 1e-5, "{right}");
        assert_eq!(turned_with_picture(anchor, 0.0, 0.0), anchor);
    }

    #[test]
    fn placement_faces_the_viewer() {
        let anchor = anchor_from_head(
            Vec3::new(0.0, 1.6, 0.0),
            Quat::from_rotation_y(0.3) * Quat::from_rotation_x(0.4),
        );
        let m = place(anchor, 0.0, 2.0, 0.0, 0.0);
        let centre = m.transform_point3(Vec3::ZERO);
        assert!((centre.y - 1.6).abs() < 1e-4, "pitch ignored for placement");
        assert!(((centre - Vec3::new(0.0, 1.6, 0.0)).length() - 2.0).abs() < 1e-4);
        let normal = m.transform_vector3(Vec3::Z);
        assert!(
            normal.dot(Vec3::new(0.0, 1.6, 0.0) - centre) > 0.0,
            "faces the viewer"
        );
        let low = place(anchor, 0.0, 1.0, -0.5, 30.0);
        assert!(
            low.transform_vector3(Vec3::Z).y > 0.3,
            "tilted panels face up"
        );
        let left = place(anchor, 30.0, 1.0, 0.0, 0.0).transform_point3(Vec3::ZERO);
        let fwd = anchor.transform_vector3(Vec3::NEG_Z);
        let right = anchor.transform_vector3(Vec3::X);
        assert!(
            left.dot(fwd) > 0.0 && (left - Vec3::new(0.0, 1.6, 0.0)).dot(right) < 0.0,
            "positive yaw is to the left"
        );
    }

    #[test]
    fn a_scaled_panel_still_fills_its_texture() {
        let pass = |p: &mut Panel| {
            let input = p.input(0.0);
            let (mut size, mut ppp) = (egui::Vec2::ZERO, 0.0);
            let _ = p.ctx.run(input, |ctx| {
                size = ctx.screen_rect().size();
                ppp = ctx.pixels_per_point();
            });
            (size, ppp)
        };
        let mut p = Panel::new([1000, 600], Vec2::new(1.0, 0.6), 1.25);
        // egui lays out its very first pass at a size of its own; a panel
        // has been painted a few times before its scale can change.
        pass(&mut p);
        pass(&mut p);
        for scale in [1.4, 0.8, 1.0, 1.4] {
            p.set_scale(scale);
            let (size, ppp) = pass(&mut p);
            assert!((ppp - 1.25 * scale).abs() < 1e-5, "{scale}: ppp {ppp}");
            assert!((size.x * ppp - 1000.0).abs() < 0.5, "{scale}: {size:?}");
            assert!((size.y * ppp - 600.0).abs() < 0.5, "{scale}: {size:?}");
        }
    }

    #[test]
    fn recentering_faces_where_you_look_without_rolling() {
        let head = Vec3::new(0.2, 1.1, -0.3);
        // Turned 30° left, looking 40° up, head tilted on its side.
        let look = Quat::from_euler(
            glam::EulerRot::YXZ,
            30f32.to_radians(),
            40f32.to_radians(),
            0.0,
        );
        let rolled = look * Quat::from_rotation_z(70f32.to_radians());
        let a = anchor_facing(head, rolled);
        let ahead = a.transform_vector3(Vec3::NEG_Z);
        assert!((ahead - look * Vec3::NEG_Z).length() < 1e-5, "{ahead}");
        assert!(a.transform_vector3(Vec3::X).y.abs() < 1e-5, "no roll");
        assert_eq!(a.w_axis.truncate(), head);
        // A panel placed from it sits where you look, facing you.
        let m = place(a, 0.0, 2.0, 0.0, 0.0);
        let centre = m.transform_point3(Vec3::ZERO);
        assert!(((centre - head).normalize() - look * Vec3::NEG_Z).length() < 1e-5);
    }

    /// Where the control bar and the adjust panel beside it go, as the app
    /// places them (`place_panels`): (bar, adjust panel).
    fn player(anchor: Mat4, flat: bool, picture: [f32; 2], bar: [f32; 2]) -> (Mat4, Mat4) {
        let from = player_frame(anchor, flat, picture, bar);
        (
            place(from, 0.0, 1.05, -0.32, 22.0),
            place_around(from, -52.0, 0.0, 1.15, 0.0),
        )
    }

    fn centre(m: Mat4) -> Vec3 {
        m.transform_point3(Vec3::ZERO)
    }

    #[test]
    fn a_vr_pictures_yaw_and_pitch_leave_the_controls_where_they_are() {
        let anchor = anchor_from_head(Vec3::new(0.0, 1.2, 0.0), Quat::IDENTITY);
        let still = player(anchor, false, [0.0, 0.0], [0.0, 0.0]);
        let turned = player(anchor, false, [40.0, -25.0], [0.0, 0.0]);
        assert!(
            centre(still.0).distance(centre(turned.0)) < 1e-5,
            "bar moved"
        );
        assert!(
            centre(still.1).distance(centre(turned.1)) < 1e-5,
            "adjust panel moved"
        );
    }

    #[test]
    fn the_controls_go_with_a_flat_screen_and_the_adjust_panel_with_the_bar() {
        let anchor = anchor_from_head(Vec3::new(0.0, 1.2, 0.0), Quat::IDENTITY);
        let (bar, adjust) = player(anchor, true, [0.0, 0.0], [0.0, 0.0]);
        // The screen turned 30° left: the bar and adjust panel turn 30°
        // about the viewer with it.
        let (bar2, adjust2) = player(anchor, true, [30.0, 0.0], [0.0, 0.0]);
        let turn = Quat::from_rotation_y(30f32.to_radians());
        let eye = anchor.w_axis.truncate();
        let about_eye = |p: Vec3| eye + turn * (p - eye);
        assert!(about_eye(centre(bar)).distance(centre(bar2)) < 1e-4);
        assert!(about_eye(centre(adjust)).distance(centre(adjust2)) < 1e-4);
        // The bar moved on its own: the adjust panel goes along, the screen
        // (placed from the picture, not the bar) doesn't.
        let (bar3, adjust3) = player(anchor, true, [0.0, 0.0], [-20.0, 0.0]);
        let moved = Quat::from_rotation_y(-20f32.to_radians());
        assert!((eye + moved * (centre(bar) - eye)).distance(centre(bar3)) < 1e-4);
        assert!((eye + moved * (centre(adjust) - eye)).distance(centre(adjust3)) < 1e-4);
    }

    #[test]
    fn a_flat_screen_and_the_adjust_panel_start_upright_at_eye_height() {
        let eye = Vec3::new(0.0, 1.2, 0.0);
        // Recentered with a glance a little down.
        let glance = Quat::from_rotation_x(-8f32.to_radians());
        let anchor = anchor_facing(eye, glance);
        let screen = place(turned_with_picture(anchor, 0.0, 0.0), 0.0, 3.0, 0.0, 0.0);
        assert!(
            (centre(screen).y - eye.y).abs() < 1e-4,
            "screen not at eye height"
        );
        assert!(
            screen.transform_vector3(Vec3::Y).abs_diff_eq(Vec3::Y, 1e-5),
            "screen tilted"
        );
        let (_, adjust) = player(anchor, true, [0.0, 0.0], [0.0, 0.0]);
        assert!(
            adjust.transform_vector3(Vec3::Y).abs_diff_eq(Vec3::Y, 1e-5),
            "adjust tilted"
        );
    }

    #[test]
    fn the_browse_panel_starts_upright_a_little_below_where_you_face() {
        let eye = Vec3::new(0.0, 1.2, 0.0);
        let anchor = anchor_from_head(eye, Quat::IDENTITY);
        let panel = place_below(anchor, 0.0, 0.0, 1.5, 7.0);
        assert!(
            panel.transform_vector3(Vec3::Y).abs_diff_eq(Vec3::Y, 1e-5),
            "browse panel tilted"
        );
        let down = 7f32.to_radians();
        let expected = eye + Vec3::new(0.0, -down.sin(), -down.cos()) * 1.5;
        assert!(centre(panel).distance(expected) < 1e-4);
        // Moved up and aside, it still faces you.
        let moved = place_below(anchor, 30.0, 20.0, 1.5, 7.0);
        assert!(moved.transform_vector3(Vec3::Z).dot(eye - centre(moved)) > 0.0);
    }

    #[test]
    fn a_recenter_glancing_a_little_up_or_down_levels_out() {
        let head = Vec3::new(0.0, 1.2, 0.0);
        for deg in [-15.0_f32, -5.0, 10.0, 19.0] {
            let a = anchor_facing(head, Quat::from_rotation_x(deg.to_radians()));
            let ahead = a.transform_vector3(Vec3::NEG_Z);
            assert!(ahead.y.abs() < 1e-5, "{deg}° not levelled: {ahead}");
        }
        // Lying back looks further up: that is kept.
        let a = anchor_facing(head, Quat::from_rotation_x(60f32.to_radians()));
        assert!(a.transform_vector3(Vec3::NEG_Z).y > 0.8);
    }

    #[test]
    fn a_growing_panel_keeps_its_layout_and_grows() {
        let mut p = Panel::new([1500, 600], Vec2::new(1.0, 0.4), 1.5).growing();
        let points = p.points();
        p.set_scale(1.5);
        assert_eq!(p.points(), points, "same layout");
        assert_eq!(p.size, Vec2::new(1.5, 0.6));
        p.set_scale(1.0);
        assert_eq!(p.size, p.base_size());
    }

    #[test]
    fn follow_keeps_the_anchor_in_front_of_the_head() {
        let anchor = anchor_from_head(Vec3::new(0.0, 1.6, 0.0), Quat::IDENTITY);
        // Turned, lying back a little and on your side, and moved.
        let head = (
            Vec3::new(0.4, 1.0, -0.2),
            Quat::from_euler(glam::EulerRot::YXZ, 1.2, 0.5, 1.5),
        );
        let dt = 1.0 / 90.0;
        // It trails a little after one frame...
        let a = follow(anchor, head, dt);
        let (_, rot, pos) = a.to_scale_rotation_translation();
        assert!(rot.angle_between(Quat::IDENTITY) > 0.01);
        assert!(rot.angle_between(head.1) > 0.5);
        assert!(pos.distance(head.0) > 0.3);
        // ...and has caught up within a second.
        let a = (0..90).fold(anchor, |a, _| follow(a, head, dt));
        let (_, rot, pos) = a.to_scale_rotation_translation();
        assert!(rot.angle_between(head.1) < 0.01, "{rot}");
        assert!(pos.distance(head.0) < 0.01, "{pos}");
    }
}
