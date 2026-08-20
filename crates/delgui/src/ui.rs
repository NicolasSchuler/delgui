//! The handful of styled controls the app is built from.
//!
//! These exist so that "primary action", "quiet action" and "mode switch" are
//! decisions made once rather than re-derived at each call site -- which is how
//! the first version ended up drawing its one important button with the same
//! weight as a checkbox.

use egui::{
    Align, Button, Color32, CornerRadius, Frame, Layout, Margin, RichText, Stroke, TextStyle, Ui,
    Vec2,
};

use crate::theme::{CHORD, MICRO, STRONG, Tokens, radius};

/// The palette matching whatever theme egui resolved for this frame. Read from
/// the live visuals rather than stored, so an OS appearance change lands
/// immediately and without a copy of the theme state in the app.
pub fn tokens(ui: &Ui) -> Tokens {
    if ui.visuals().dark_mode {
        Tokens::dark()
    } else {
        Tokens::light()
    }
}

pub fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(STRONG.into()))
}

/// Uppercase tags -- `BASELINE`, `PASTED`. egui has no small-caps and no
/// letter-spacing, so the shape has to come from the string itself.
pub fn micro(text: impl AsRef<str>) -> RichText {
    RichText::new(text.as_ref().to_uppercase()).text_style(TextStyle::Name(MICRO.into()))
}

pub fn chord(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Name(CHORD.into()))
}

pub fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).text_style(TextStyle::Small)
}

/// A raised surface with a border: panels, the diff, the settings sections.
pub fn card(t: &Tokens, fill: Color32, outlined: bool) -> Frame {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(
            1.0,
            if outlined { t.accent } else { t.border_subtle },
        ))
        .corner_radius(radius::CARD)
}

/// The one action the app is for. Filled, so nothing else on screen competes.
pub fn primary(ui: &mut Ui, t: &Tokens, label: &str, shortcut: &str, enabled: bool) -> bool {
    let button = Button::new(RichText::new(label).color(t.on_accent))
        .shortcut_text(RichText::new(shortcut).color(t.on_accent.gamma_multiply(0.65)))
        .fill(t.accent_solid)
        .stroke(Stroke::NONE)
        .corner_radius(radius::CONTROL)
        .min_size(Vec2::new(0.0, 28.0));
    ui.add_enabled(enabled, button).clicked()
}

/// Quiet by default, outlined on hover. Everything that is not the primary
/// action and not a mode.
pub fn ghost(ui: &mut Ui, label: impl Into<String>) -> egui::Response {
    ui.add(
        Button::new(RichText::new(label).text_style(TextStyle::Body))
            .frame_when_inactive(false)
            .corner_radius(radius::CONTROL)
            .min_size(Vec2::new(0.0, 26.0)),
    )
}

/// A square, label-less button for a single glyph.
///
/// Only glyphs the font chain actually has may be passed here -- `✕` (U+2715)
/// is in none of egui's bundled fonts and came out as a tofu box, which is why
/// the app uses `×` throughout.
pub fn icon(ui: &mut Ui, glyph: &str, name: &str, tint: Option<Color32>) -> egui::Response {
    let mut text = RichText::new(glyph).text_style(TextStyle::Body);
    if let Some(c) = tint {
        text = text.color(c);
    }
    let response = ui.add(
        Button::new(text)
            .frame_when_inactive(false)
            .corner_radius(radius::CHIP)
            .min_size(Vec2::splat(26.0)),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, response.enabled(), name)
    });
    response
}

/// Mode switches, grouped in a sunken track.
///
/// These are not settings -- they are the view the user is looking through, and
/// they get toggled constantly. A row of stock checkboxes said "form"; this says
/// "mode", and it is one object instead of three loose ones.
pub fn segmented(ui: &mut Ui, t: &Tokens, items: &mut [(&str, &mut bool)]) -> bool {
    let mut changed = false;
    Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.border_subtle))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (label, on) in items.iter_mut() {
                    let button = Button::selectable(**on, *label)
                        .corner_radius(CornerRadius::same(5))
                        .min_size(Vec2::new(0.0, 24.0));
                    if ui.add(button).clicked() {
                        **on = !**on;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// A three-way choice where exactly one is on.
pub fn choice<T: PartialEq + Copy>(
    ui: &mut Ui,
    t: &Tokens,
    current: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.border_subtle))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (value, label) in options {
                    let button = Button::selectable(*current == *value, *label)
                        .corner_radius(CornerRadius::same(5))
                        .min_size(Vec2::new(0.0, 24.0));
                    if ui.add(button).clicked() && *current != *value {
                        *current = *value;
                        changed = true;
                    }
                }
            });
        });
    changed
}

#[derive(Clone, Copy)]
pub enum Tone {
    Error,
    /// Something the user asked for did not happen. Never fatal, always
    /// dismissible, and never in place of the diff.
    Warning,
}

/// A dismissible strip above the content, never in place of it: a watcher that
/// could not start is no reason to hide a perfectly good diff.
pub fn banner(ui: &mut Ui, t: &Tokens, tone: Tone, text: &str) -> bool {
    let (fill, edge, fg) = match tone {
        Tone::Error => (t.danger_quiet, t.danger, t.danger),
        Tone::Warning => (t.surface_raised, t.warning, t.warning),
    };
    let mut dismissed = false;
    let response = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(radius::CONTROL)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.add(egui::Label::new(RichText::new(text).color(fg)).wrap());
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    dismissed = icon(ui, "×", "Dismiss message", Some(fg))
                        .on_hover_text("Dismiss message")
                        .clicked();
                });
            });
        })
        .response;
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(match tone {
            Tone::Error => egui::accesskit::Role::Alert,
            Tone::Warning => egui::accesskit::Role::Status,
        });
        node.set_label(text);
        node.set_live(match tone {
            Tone::Error => egui::accesskit::Live::Assertive,
            Tone::Warning => egui::accesskit::Live::Polite,
        });
    });
    dismissed
}

/// Centred guidance for a region with nothing in it yet. A one-line hint in the
/// top-left corner reads as a status message; this reads as an invitation.
pub fn empty_state(ui: &mut Ui, t: &Tokens, title: &str, body: &str, rows: &[(&str, &str)]) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.22);
        ui.label(
            RichText::new(title)
                .text_style(TextStyle::Heading)
                .color(t.text_primary),
        );
        ui.add_space(6.0);
        ui.label(RichText::new(body).color(t.text_secondary));
        if !rows.is_empty() {
            ui.add_space(16.0);
            // Bounded so `vertical_centered` has something narrower than the
            // whole card to centre; a full-width grid would sit against the
            // left edge under a centred heading.
            ui.scope(|ui| {
                ui.set_max_width(300.0);
                egui::Grid::new("empty-rows")
                    .num_columns(2)
                    .spacing([14.0, 8.0])
                    .show(ui, |ui| {
                        for (key, what) in rows {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    chord(*key)
                                        .color(t.text_secondary)
                                        .background_color(t.surface_raised),
                                );
                            });
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.label(RichText::new(*what).color(t.text_muted));
                            });
                            ui.end_row();
                        }
                    });
            });
        }
    });
}

/// The two inputs matched. Deliberately not styled as an error or as emptiness:
/// "these are the same" is a real answer and often the one being looked for.
pub fn identical(ui: &mut Ui, t: &Tokens, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.22);
        ui.label(
            RichText::new("No differences")
                .text_style(TextStyle::Heading)
                .color(t.success),
        );
        ui.add_space(6.0);
        ui.label(RichText::new(body).color(t.text_secondary));
    });
}

/// A labelled row in the settings drawer: label on the left at a fixed width,
/// control on the right, so a column of them lines up.
pub fn field<R>(ui: &mut Ui, t: &Tokens, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_sized(
            [96.0, 24.0],
            egui::Label::new(RichText::new(label).color(t.text_secondary)),
        );
        add(ui)
    })
    .inner
}

pub fn section(ui: &mut Ui, t: &Tokens, title: &str) {
    ui.add_space(4.0);
    ui.label(micro(title).color(t.text_muted));
    ui.add_space(6.0);
}

/// A short-lived confirmation that assistive technology should announce even
/// when focus stays on the button that caused it.
pub fn status(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    let response = ui.label(small(text).color(color));
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::Status);
        node.set_label(text);
        node.set_live(egui::accesskit::Live::Polite);
    });
    response
}
