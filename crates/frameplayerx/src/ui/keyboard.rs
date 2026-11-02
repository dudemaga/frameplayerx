//! A virtual keyboard panel for text fields (search, addresses, passwords).

use super::theme::{self, Weight};
use super::{icons, widgets};
use egui::{Align2, Event, Key, Modifiers, Sense, Vec2};

const ROWS: [&str; 4] = ["1234567890-", "qwertyuiop/", "asdfghjkl:_", "zxcvbnm.@?&"];
/// The "#+=" page: every ASCII symbol (`-` stays on the number row), for
/// passwords.
const SYMBOL_ROWS: [&str; 4] = ["1234567890-", "!@#$%^&*()+", "=[]{}\\|;:'\"", "<>,.?/_~`€£"];
const KEY_H: f32 = 60.0;
const GAP: f32 = 7.0;

/// How a key looks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Char,
    /// Shift, arrows, delete: a darker key.
    Function,
    /// Enter: the accent.
    Action,
    /// A function key that is on (shift).
    On,
}

/// One key; true on the frame its press begins (rather than release, so moving
/// on to the next key before letting go never drops the letter). `label` may be
/// an icon.
fn key(ui: &mut egui::Ui, label: &str, w: f32, style: Style, icon: bool) -> bool {
    let (_, resp) = ui.allocate_exact_size(Vec2::new(w, KEY_H), Sense::click());
    // Raised keys: Enter is the accent, shift glows while it is on, and
    // function keys keep a quieter label.
    let kind = if style == Style::Action {
        widgets::Kind::Primary
    } else {
        widgets::Kind::Secondary
    };
    let (face, fg) = widgets::key_face_kind(ui, &resp, 12.0, style == Style::On, kind);
    let fg = if style == Style::Function {
        fg.lerp_to_gamma(theme::TEXT_2, 0.5)
    } else {
        fg
    };
    let p = ui.painter();
    let font = if icon {
        theme::icon(26.0)
    } else if label.chars().count() > 1 {
        theme::font(Weight::SemiBold, 19.0)
    } else {
        theme::font(Weight::Medium, 25.0)
    };
    p.text(face.center(), Align2::CENTER_CENTER, label, font, fg);
    resp.contains_pointer() && ui.input(|i| i.pointer.primary_pressed())
}

/// Draws the keyboard; returns the events to send to the focused panel and
/// whether the keyboard should close. `symbols` shows the symbols page.
pub fn keyboard(ctx: &egui::Context, shift: &mut bool, symbols: &mut bool) -> (Vec<Event>, bool) {
    let mut out = Vec::new();
    let mut close = false;
    let before = (*shift, *symbols);
    let shifted = *shift;
    widgets::panel_slab(ctx, 24.0);
    let frame = egui::Frame::new()
        .outer_margin(widgets::SHADOW_ROOM)
        .inner_margin(egui::Margin::same(14));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(GAP, GAP);
        // 11 keys plus a 1.4-wide function key on the widest row.
        let key_w = (ui.available_width() - GAP * 12.0) / 12.4;
        let rows = if *symbols { &SYMBOL_ROWS } else { &ROWS };
        for (i, row) in rows.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.add_space(i as f32 * key_w * 0.2);
                for c in row.chars() {
                    let ch = if shifted {
                        c.to_uppercase().next().unwrap_or(c)
                    } else {
                        c
                    };
                    if key(ui, &ch.to_string(), key_w, Style::Char, false) {
                        out.push(Event::Text(ch.to_string()));
                        *shift = false;
                    }
                }
                if i == 0 && key(ui, icons::BACKSPACE, key_w * 1.4, Style::Function, true) {
                    out.push(key_event(Key::Backspace));
                }
                if i == 1 && key(ui, icons::KEY_RETURN, key_w * 1.2, Style::Action, true) {
                    out.push(key_event(Key::Enter));
                    close = true;
                }
            });
        }
        ui.horizontal(|ui| {
            let shift_style = if *shift { Style::On } else { Style::Function };
            if key(ui, icons::ARROW_FAT_UP, key_w * 1.5, shift_style, true) {
                *shift = !*shift;
            }
            let page = if *symbols { "abc" } else { "#+=" };
            if key(ui, page, key_w * 1.5, Style::Function, false) {
                *symbols = !*symbols;
            }
            if key(ui, icons::CARET_LEFT, key_w, Style::Function, true) {
                out.push(key_event(Key::ArrowLeft));
            }
            if key(ui, icons::CARET_RIGHT, key_w, Style::Function, true) {
                out.push(key_event(Key::ArrowRight));
            }
            if key(ui, tr!("keyboard-space"), key_w * 2.9, Style::Char, false) {
                out.push(Event::Text(" ".into()));
            }
            if key(ui, ".com", key_w * 1.5, Style::Function, false) {
                out.push(Event::Text(".com".into()));
            }
            if key(
                ui,
                tr!("common-done"),
                ui.available_width(),
                Style::Function,
                false,
            ) {
                close = true;
            }
        });
    });
    if (*shift, *symbols) != before {
        ctx.request_repaint();
    }
    (out, close)
}

fn key_event(key: Key) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Panel, Pointer};
    use fp_xr::Hand;
    use glam::{Mat4, Quat, Vec2, Vec3};

    fn find_label(shape: &egui::Shape, want: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(t) if t.galley.text() == want => Some(t.pos + t.galley.size() * 0.5),
            egui::Shape::Vec(v) => v.iter().find_map(|s| find_label(s, want)),
            _ => None,
        }
    }

    struct Rig {
        kb: Panel,
        field: Panel,
        ptr: Pointer,
        hands: [Hand; 2],
        shift: bool,
        symbols: bool,
        text: String,
        shapes: Vec<egui::epaint::ClippedShape>,
        time: f64,
    }

    impl Rig {
        fn new() -> Self {
            let mut kb = Panel::new([1240, 492], Vec2::new(1.0, 0.4), 1.0);
            kb.pose = Mat4::from_translation(Vec3::new(0.0, -0.5, -1.0));
            kb.visible = true;
            let mut field = Panel::new([800, 200], Vec2::new(1.2, 0.3), 1.0);
            field.pose = Mat4::from_translation(Vec3::new(0.0, 0.2, -1.5));
            field.visible = true;
            let mut hands = [Hand::default(), Hand::default()];
            hands[1].active = true;
            let mut rig = Self {
                kb,
                field,
                ptr: Pointer::default(),
                hands,
                shift: false,
                symbols: false,
                text: String::new(),
                shapes: Vec::new(),
                time: 0.0,
            };
            // Warm up layout and focus the text field.
            rig.step();
            rig.step();
            rig
        }

        fn aim_at(&mut self, label: &str) {
            let pt = self
                .shapes
                .iter()
                .find_map(|c| find_label(&c.shape, label))
                .unwrap_or_else(|| panic!("missing key {label:?}"));
            let dir = self.kb.world_point(pt).normalize();
            self.hands[1].aim = Some((Vec3::ZERO, Quat::from_rotation_arc(Vec3::NEG_Z, dir)));
        }

        fn step(&mut self) {
            self.ptr
                .route(&mut [&mut self.kb, &mut self.field], &self.hands, 0.011);
            self.time += 0.011;
            let kb_in = self.kb.input(self.time);
            let shift = &mut self.shift;
            let symbols = &mut self.symbols;
            let mut events = Vec::new();
            let out = self.kb.ctx.run(kb_in, |ctx| {
                let (e, _) = keyboard(ctx, shift, symbols);
                events = e;
            });
            self.shapes = out.shapes;
            // Same order as App::paint_panels: keyboard events reach the
            // focused panel before it paints this frame.
            for e in events {
                self.field.push(e);
            }
            let field_in = self.field.input(self.time);
            let text = &mut self.text;
            let _ = self.field.ctx.run(field_in, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::singleline(text));
                    if !r.has_focus() {
                        r.request_focus();
                    }
                });
            });
        }
    }

    #[test]
    fn fast_single_hand_sweeps_type_every_letter_immediately() {
        let mut r = Rig::new();

        // Press 'f': appears in the text box on the press frame, before the
        // trigger is let go.
        r.aim_at("f");
        r.hands[1].trigger = 1.0;
        r.step();
        assert_eq!(r.text, "f");

        // Whip across the keyboard to 'a' while the trigger is still bottomed
        // out (far past the click distance, turning the hold into a drag), then
        // feather the trigger halfway up and press 'a'.
        r.aim_at("a");
        r.step();
        assert_eq!(r.text, "f", "holding a key while sweeping does not repeat");
        r.hands[1].trigger = 0.4;
        r.step();
        r.hands[1].trigger = 0.65;
        r.step();
        assert_eq!(r.text, "fa");

        // Sweep completely off the keyboard before releasing, then whip to 's'
        // and 't' back-to-back.
        r.hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        r.step();
        r.hands[1].trigger = 0.0;
        r.step();
        r.aim_at("s");
        r.hands[1].trigger = 0.9;
        r.step();
        assert_eq!(r.text, "fas");
        r.aim_at("t");
        r.step();
        r.hands[1].trigger = 0.42;
        r.step();
        r.hands[1].trigger = 0.8;
        r.step();
        assert_eq!(r.text, "fast");
    }

    #[test]
    fn rapid_double_clicks_and_modifiers_work_on_press() {
        let mut r = Rig::new();

        // Shift held across multiple frames toggles once, capitalises the next
        // letter on press, and drops back to lowercase.
        r.aim_at(icons::ARROW_FAT_UP);
        r.hands[1].trigger = 1.0;
        r.step();
        r.step();
        r.hands[1].trigger = 0.0;
        r.step();
        assert!(r.shift);

        r.aim_at("A");
        r.hands[1].trigger = 0.85;
        r.step();
        assert_eq!(r.text, "A");
        assert!(!r.shift);
        r.hands[1].trigger = 0.0;
        r.step();

        // Rapid double-pull on the same key without fully lifting the finger.
        r.aim_at("l");
        r.hands[1].trigger = 0.85;
        r.step();
        r.hands[1].trigger = 0.4;
        r.step();
        r.hands[1].trigger = 0.7;
        r.step();
        r.hands[1].trigger = 0.0;
        r.step();
        assert_eq!(r.text, "All");

        // Symbols page toggle and symbol key on press.
        r.aim_at("#+=");
        r.hands[1].trigger = 0.9;
        r.step();
        r.hands[1].trigger = 0.0;
        r.step();
        assert!(r.symbols);
        r.aim_at("!");
        r.hands[1].trigger = 0.9;
        r.step();
        assert_eq!(r.text, "All!");
    }
}
