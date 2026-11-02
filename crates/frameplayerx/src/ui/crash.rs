//! The page offered after a crash: what a report contains and where it
//! goes, the report itself on request, and the choice to send it.

use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea};
use super::{Action, View, icons};
use egui::RichText;

pub struct CrashPrompt {
    pub report: crate::crash::Report,
    /// The whole report is on the page.
    pub shown: bool,
    pub sending: bool,
    /// Why it could not be sent.
    pub error: Option<String>,
}

impl CrashPrompt {
    pub fn new(report: crate::crash::Report) -> CrashPrompt {
        CrashPrompt {
            report,
            shown: false,
            sending: false,
            error: None,
        }
    }
}

fn contains() -> [&'static str; 3] {
    [
        tr!("crash-contains-versions"),
        tr!("crash-contains-log"),
        tr!("crash-contains-not"),
    ]
}

fn goes() -> [&'static str; 2] {
    [tr!("crash-goes-developers"), tr!("crash-goes-nothing-else")]
}

pub fn prompt(ui: &mut egui::Ui, v: &mut View) {
    let actions = &mut *v.actions;
    let Some(c) = v.state.crash.as_mut() else {
        return;
    };
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_list(ui, |ui| {
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icons::WARNING_CIRCLE)
                        .font(theme::icon(34.0))
                        .color(theme::WARN),
                );
                ui.label(
                    RichText::new(tr!("crash-title"))
                        .font(theme::font(Weight::Bold, 32.0))
                        .color(theme::TEXT),
                );
            });
            ui.add_space(8.0);
            ui.label(
                RichText::new(tr!("crash-desc"))
                    .font(theme::font(Weight::Regular, 18.0))
                    .color(theme::TEXT_2),
            );
            ui.add_space(16.0);
            list(ui, tr!("crash-contains"), &contains());
            list(ui, tr!("crash-goes"), &goes());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if c.sending {
                    ui.spinner();
                    ui.label(RichText::new(tr!("crash-sending")).color(theme::TEXT_2));
                    return;
                }
                if widgets::button(ui, Some(icons::CHECK), tr!("crash-send"), Kind::Primary)
                    .clicked()
                {
                    actions.push(Action::SendCrashReport);
                }
                let label = if c.shown {
                    tr!("crash-hide-report")
                } else {
                    tr!("crash-show-report")
                };
                if widgets::button(ui, Some(icons::EYE), label, Kind::Secondary).clicked() {
                    c.shown = !c.shown;
                }
                if widgets::button(ui, None, tr!("crash-dont-send"), Kind::Ghost).clicked() {
                    actions.push(Action::DismissCrashReport);
                }
            });
            if let Some(e) = &c.error {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(tr!("crash-send-failed", error = e.as_str())).color(theme::ERROR),
                );
            }
            if c.shown {
                ui.add_space(14.0);
                widgets::section_label(ui, tr!("crash-report-heading"));
                egui::Frame::new()
                    .fill(theme::SURFACE_2)
                    .corner_radius(12)
                    .inner_margin(egui::Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(&c.report.text)
                                .font(egui::FontId::monospace(13.0))
                                .color(theme::TEXT_2),
                        );
                    });
            }
            ui.add_space(24.0);
        });
}

fn list(ui: &mut egui::Ui, title: &str, items: &[&str]) {
    widgets::section_label(ui, title);
    for item in items {
        ui.horizontal_top(|ui| {
            ui.label(
                RichText::new("•")
                    .font(theme::font(Weight::Bold, 18.0))
                    .color(theme::ACCENT),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(*item)
                        .font(theme::font(Weight::Regular, 17.0))
                        .color(theme::TEXT),
                )
                .wrap(),
            );
        });
    }
    ui.add_space(12.0);
}
