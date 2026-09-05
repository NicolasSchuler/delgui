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
        .shortcut_text(RichText::new(shortcut).color(t.on_accent))
        .fill(t.accent_solid)
        .stroke(Stroke::NONE)
        .corner_radius(radius::CONTROL)
        .min_size(Vec2::new(0.0, 28.0));
    ui.add_enabled(enabled, button).clicked()
}

/// A button that opens a drawer, with expanded state rather than a checkbox.
pub fn disclosure(ui: &mut Ui, label: &str, expanded: bool) -> egui::Response {
    let response = ui.add(
        Button::selectable(expanded, label)
            .corner_radius(radius::CONTROL)
            .min_size(Vec2::new(0.0, 26.0)),
    );
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.clear_toggled();
        node.set_expanded(expanded);
    });
    response
}

/// The mutually exclusive baseline choice, visually identified by its letter.
pub fn baseline_chip(ui: &mut Ui, label: &str, selected: bool) -> egui::Response {
    let response = ui.add(
        Button::selectable(selected, strong(label))
            .corner_radius(radius::CHIP)
            .min_size(Vec2::new(24.0, 22.0)),
    );
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_role(egui::accesskit::Role::RadioButton);
        node.set_label(format!("Use panel {label} as baseline"));
    });
    response
}

/// AccessKit's macOS adapter maps ScrollView to AXUnknown. A named Group keeps
/// these regions discoverable there without changing their scrolling or focus.
pub fn scroll_region_role() -> egui::accesskit::Role {
    if cfg!(target_os = "macos") {
        egui::accesskit::Role::Group
    } else {
        egui::accesskit::Role::ScrollView
    }
}

/// Quiet by default, outlined on hover. Everything that is not the primary
/// action and not a mode.
pub fn ghost(ui: &mut Ui, label: impl Into<String>) -> egui::Response {
    ghost_enabled(ui, label, true)
}

/// The same, but able to go grey. A ghost button carries no frame until it is
/// hovered, so "enabled" and "disabled" are told apart by the label colour
/// alone -- which is the whole signal, and reason enough not to leave a dead
/// control at full strength.
pub fn ghost_enabled(ui: &mut Ui, label: impl Into<String>, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
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

/// Make a [`egui::Slider`] visible. Call on the `Ui` the slider is added to.
///
/// A slider is the one control whose *fill* is the affordance -- a button has
/// its label and a checkbox its mark, but a track with no contrast is just an
/// empty box. egui paints the rail with `widgets.inactive.bg_fill`
/// (egui-0.36.1/src/widgets/slider.rs:775), which [`crate::theme::widget`] sets
/// to `surface_overlay` for every widget; the settings drawer's own ground is
/// `surface_raised`. Measured, that is `#1E2126` on `#191B1F` in dark -- and in
/// light both tokens are `#FFFFFF`, so the rail was drawn in exactly its own
/// background colour. What reached the screen was an outlined pill floating
/// beside a number, with no track, no range and no sense of position.
///
/// So: a rail that contrasts with any surface, and the trailing fill turned on,
/// which is what makes the current value readable at a glance rather than only
/// from the number beside it. `selection.bg_fill` is scoped rather than themed
/// because it is also the text-selection colour everywhere else.
pub fn slider_visuals(ui: &mut Ui, t: &Tokens) {
    let visuals = ui.visuals_mut();
    visuals.widgets.inactive.bg_fill = t.border_strong;
    visuals.selection.bg_fill = t.accent;
    visuals.slider_trailing_fill = true;
}

/// Mode switches, grouped in a sunken track.
///
/// These are not settings -- they are the view the user is looking through, and
/// they get toggled constantly. A row of stock checkboxes said "form"; this says
/// "mode", and it is one object instead of three loose ones.
pub fn segmented(ui: &mut Ui, t: &Tokens, items: &mut [(&str, &mut bool)]) -> bool {
    let mut changed = false;
    Frame::new()
        // Sunken, not `surface`. The toolbar these sit in is drawn on
        // `surface_raised`, against which `surface` is a shade away and the
        // track was invisible -- so three switched-on modes read as three loud
        // separate buttons rather than as one object, and on the empty screen
        // they were the brightest thing on it, next to a Compare button that is
        // correctly disabled because there is nothing to compare.
        .fill(t.surface_sunken)
        .stroke(Stroke::new(1.0, t.border_subtle))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            // Only the x is set: `ui.horizontal` never wraps, so this frame is
            // always one row and its `item_spacing.y` is unobservable. `choice`
            // does wrap, and there the y matters -- see the comment there.
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (label, on) in items.iter_mut() {
                    let button = Button::selectable(**on, *label)
                        .corner_radius(CornerRadius::same(5))
                        .min_size(Vec2::new(0.0, 24.0));
                    let response = ui.add(button);
                    if response.gained_focus() {
                        response.scroll_to_me(Some(Align::Center));
                    }
                    if response.clicked() {
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
            // Both axes. The frame's inner margin is 2 and the gap between
            // buttons is 2; the theme's 8px vertical rhythm (theme.rs
            // `spacing`) is the gap *between* stacked controls, not between the
            // rows of one of them. A wrapped row is placed at
            // `min_rect.bottom() + spacing.y` (egui-0.36.1/src/layout.rs:520),
            // so leaving y at 8 split the segmented control into two objects
            // the moment the drawer got narrow enough to wrap.
            ui.spacing_mut().item_spacing = Vec2::splat(2.0);
            ui.horizontal_wrapped(|ui| {
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
    // A path and an OS error can be much taller than a tiled window's diff.
    // Keep that explanation scrollable and leave room for the comparison.
    let message_height = (ui.available_height() * 0.5 - 16.0)
        .max(ui.text_style_height(&TextStyle::Body))
        .min(ui.text_style_height(&TextStyle::Body) * 3.0);
    let response = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(radius::CONTROL)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            egui::containers::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("message-details")
                        .min_scrolled_height(0.0)
                        .max_height(message_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.ctx().accesskit_node_builder(ui.unique_id(), |node| {
                                node.set_role(scroll_region_role());
                                node.set_label("Message details");
                            });
                            ui.add(egui::Label::new(RichText::new(text).color(fg)).wrap());
                        });
                },
                |ui| {
                    dismissed = icon(ui, "×", "Dismiss message", Some(fg))
                        .on_hover_text("Dismiss message")
                        .clicked();
                },
            );
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
    let _ = empty_state_scroll(ui, t, title, body, rows);
}

const EMPTY_STATE_COMPACT_HEIGHT: f32 = 320.0;
const EMPTY_STATE_COMPACT_TOP: f32 = 12.0;

fn empty_state_scroll(
    ui: &mut Ui,
    t: &Tokens,
    title: &str,
    body: &str,
    rows: &[(&str, &str)],
) -> egui::scroll_area::ScrollAreaOutput<(egui::Rect, egui::Rect)> {
    let viewport_height = ui.available_height();
    let top_space = if viewport_height < EMPTY_STATE_COMPACT_HEIGHT {
        let progress = ((viewport_height - 120.0)
            / (EMPTY_STATE_COMPACT_HEIGHT - 120.0))
            .clamp(0.0, 1.0);
        egui::lerp(
            EMPTY_STATE_COMPACT_TOP..=EMPTY_STATE_COMPACT_HEIGHT * 0.22,
            progress,
        )
    } else {
        viewport_height * 0.22
    };
    let content_width = ui.available_width();
    egui::ScrollArea::vertical()
        .id_salt("empty-state-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let accessibility = ui.unique_id();
            ui.ctx().accesskit_node_builder(accessibility, |node| {
                node.set_role(scroll_region_role());
                node.set_label("Getting started");
            });
            // A scroll area's content width follows its children. Hold it open
            // to the viewport so the existing centred hierarchy stays centred
            // rather than collapsing to the width of its longest label.
            ui.set_min_width(content_width);
            let contents = ui.vertical_centered(|ui| {
                ui.add_space(top_space);
                let title = ui
                    .label(
                        RichText::new(title)
                            .text_style(TextStyle::Heading)
                            .color(t.text_primary),
                    )
                    .rect;
                ui.add_space(6.0);
                let body = ui
                    .label(RichText::new(body).color(t.text_secondary))
                    .rect;
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
                (title, body)
            })
            .inner;
            let bounds = ui.clip_rect().intersect(ui.min_rect());
            ui.ctx().accesskit_node_builder(accessibility, |node| {
                node.set_bounds(egui::accesskit::Rect {
                    x0: f64::from(bounds.left()),
                    y0: f64::from(bounds.top()),
                    x1: f64::from(bounds.right()),
                    y1: f64::from(bounds.bottom()),
                });
            });
            contents
        })
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

/// A floor, not the column: the column grows to the widest label it was given,
/// so short-labelled drawers still align on the familiar 112 and a drawer whose
/// labels outgrow it widens rather than stacking.
const FIELD_LABEL_MIN_WIDTH: f32 = 112.0;
/// The height of an inline label's box, which is also the row's floor: a
/// drawer of one-line fields reads as a list of rows rather than as text of
/// varying leading.
const FIELD_ROW_HEIGHT: f32 = 24.0;
/// What the widest control in the drawer needs. The two size fields set
/// `slider_width = 112` and add a `Slider` with a `" pt"` suffix and a
/// drag-value box beside it, which lands near 175. Lowering this would let a
/// drawer go inline in a state where those sliders overflow it -- widen the
/// drawer instead.
const FIELD_CONTROL_MIN_WIDTH: f32 = 180.0;

pub fn control_width(ui: &Ui) -> f32 {
    ui.available_width().min(ui.clip_rect().width()).min(240.0)
}

/// Where a drawer's labels go, decided once for the whole drawer.
///
/// The decision cannot be per label. A row that stacks on its own -- because
/// its label happens to be the long one -- puts its control at the row's left
/// edge while every neighbouring control sits in a column further right, which
/// reads as a broken layout rather than as a narrow one.
#[derive(Clone, Copy)]
pub struct FieldColumn {
    width: f32,
    stacked: bool,
    /// Carried so `field` can check its label was one the column was measured
    /// from: a label the measurement never saw could be wider than the box it
    /// is drawn in.
    labels: &'static [&'static str],
}

/// Measure a drawer's label column and decide, once, whether its fields align
/// or stack.
///
/// Call this from the `Ui` the fields are direct children of: `available_width`
/// is read here and has to be the width the rows themselves will see.
pub fn field_column(ui: &Ui, labels: &'static [&'static str]) -> FieldColumn {
    // Growing the column to the widest label is also what removes the wrap
    // question from the inline branch: no label can exceed its own box, so the
    // fixed 24px row height in `field` is always enough.
    let width = widest_label(ui, labels).max(FIELD_LABEL_MIN_WIDTH).ceil();
    let available = ui.available_width().min(ui.clip_rect().width());
    FieldColumn {
        width,
        stacked: available < width + ui.spacing().item_spacing.x + FIELD_CONTROL_MIN_WIDTH,
        labels,
    }
}

/// What a measured column was measured against.
///
/// The `FontId` alone is not enough: `fonts::definitions` rebinds the same
/// `Proportional` family name to whichever face the user picked. Distinct faces
/// can also share the row height and `M` advance below while differing on the
/// actual settings labels, so `font_revision` is bumped whenever definitions
/// are replaced. `pixels_per_point` is in here because glyph advances are
/// rounded to physical pixels while laying out.
#[derive(Clone, PartialEq)]
struct ColumnKey {
    font: egui::FontId,
    font_revision: u64,
    ppp: u32,
    row_height: u32,
    glyph_width: u32,
}

#[derive(Clone)]
struct ColumnMemo {
    key: ColumnKey,
    width: f32,
    /// How often the labels have actually been shaped. Nothing reads this but
    /// the test, and it is here so "measured once per font change" is pinned
    /// rather than asserted in prose.
    measurements: u32,
}

/// Keyed on the label set, so two drawers with different labels keep their own
/// entries instead of overwriting each other's every frame.
fn column_id(labels: &'static [&'static str]) -> egui::Id {
    egui::Id::new(("delgui-field-column", labels))
}

fn font_revision_id() -> egui::Id {
    egui::Id::new("delgui-font-definitions-revision")
}

/// Invalidate measurements whose family names may now resolve to another face.
/// Call immediately after replacing egui's font definitions.
pub fn font_definitions_replaced(ctx: &egui::Context) {
    ctx.data_mut(|data| {
        let id = font_revision_id();
        let revision = data.get_temp::<u64>(id).unwrap_or_default().wrapping_add(1);
        data.insert_temp(id, revision);
    });
}

/// The widest of `labels`, shaped once per font change rather than once per
/// label per frame.
///
/// epaint's galley cache absorbs the repeated calls, but not the `String` and
/// `LayoutJob` allocated per label per repaint, and not the re-shaping that
/// follows an eviction -- the cache drops anything unused in a frame, so every
/// reopening of the drawer paid for the whole set again.
fn widest_label(ui: &Ui, labels: &'static [&'static str]) -> f32 {
    let body = TextStyle::Body.resolve(ui.style());
    // `fonts_mut` and `data_mut` both take the context lock, so the calls are
    // ordered rather than nested.
    let (row_height, glyph_width) = ui
        .ctx()
        .fonts_mut(|f| (f.row_height(&body), f.glyph_width(&body, 'M')));
    let key = ColumnKey {
        font: body.clone(),
        font_revision: ui
            .ctx()
            .data(|data| data.get_temp::<u64>(font_revision_id()))
            .unwrap_or_default(),
        ppp: ui.ctx().pixels_per_point().to_bits(),
        row_height: row_height.to_bits(),
        glyph_width: glyph_width.to_bits(),
    };
    let id = column_id(labels);
    let previous = ui.ctx().data(|d| d.get_temp::<ColumnMemo>(id));
    if let Some(memo) = &previous {
        if memo.key == key {
            return memo.width;
        }
    }
    let width = ui.ctx().fonts_mut(|f| {
        labels
            .iter()
            .map(|l| {
                f.layout_no_wrap((*l).to_owned(), body.clone(), Color32::WHITE)
                    .size()
                    .x
            })
            .fold(0.0_f32, f32::max)
    });
    let measurements = previous.map_or(0, |m| m.measurements) + 1;
    let memo = ColumnMemo {
        key,
        width,
        measurements,
    };
    ui.ctx().data_mut(|d| d.insert_temp(id, memo));
    width
}

/// A labelled row in the settings drawer. Whether the label sits in a column to
/// the left of the control or above it is the *drawer's* decision, taken once
/// in `field_column`: comfortable drawers align every control in one column,
/// narrow ones stack every one of them, and neither happens to one row alone.
pub fn field<R>(
    ui: &mut Ui,
    t: &Tokens,
    column: FieldColumn,
    label: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    // The column is exactly as wide as the widest label it was measured from,
    // so a label the measurement never saw could overflow the fixed box below.
    // `settings_drawer_stays_inside_its_supported_narrow_width` in `app.rs`
    // draws every call site in debug, which is where this fires if a new field
    // is added without being added to the drawer's label list.
    debug_assert!(
        column.labels.contains(&label),
        "field label {label:?} is not in the list its column was measured from",
    );
    if column.stacked {
        ui.vertical(|ui| {
            // `ui.vertical`'s cross align is `Min`, so this is already at the
            // row's left edge -- the same edge the inline branch puts it at.
            ui.label(RichText::new(label).color(t.text_secondary));
            add(ui)
        })
        .inner
    } else {
        ui.horizontal(|ui| {
            // An explicit box rather than `add_sized`, which cannot put the
            // text at the box's left edge however it is asked. `add_sized`
            // justifies the *response* to the column width -- which is what
            // holds the column open -- and then re-aligns the galley's own rect
            // inside it with the layout's `Center`
            // (`allocate_exact_size`, egui-0.36.1/src/ui.rs:1150-1156), so the
            // glyphs land halfway across the column and `Label::halign` never
            // gets a say: it aligns the text within its rect, and the rect is
            // the thing that moved. `set_min_size` is what keeps the column
            // open here, because the cursor advances by the child's `min_rect`
            // (`scope_dyn`, `ui.rs:2212`) and not by the size asked for.
            ui.allocate_ui_with_layout(
                Vec2::new(column.width, FIELD_ROW_HEIGHT),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.set_min_size(Vec2::new(column.width, FIELD_ROW_HEIGHT));
                    ui.label(RichText::new(label).color(t.text_secondary));
                },
            );
            add(ui)
        })
        .inner
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a drawer's label list. The long one is what the per-label
    /// predicate used to stack on its own.
    const LABELS: &[&str] = &["Theme", "Diff", "Interface size", "Ignore lines matching"];

    /// A context with the app's own fonts and type scale, so the tests measure
    /// what the app measures rather than egui's defaults.
    fn test_ctx(ui_pt: f32) -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        crate::theme::install(&ctx, ui_pt, 12.5);
        ctx
    }

    #[test]
    fn primary_shortcut_keeps_the_labels_full_contrast() {
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let ctx = test_ctx(13.0);
            ctx.set_theme(theme);
            let t = Tokens::of(theme);
            let mut output = ctx.run_ui(Default::default(), |ui| {
                primary(ui, &t, "Compare", "Ctrl+Enter", true);
            });
            let mut texts = Vec::new();
            fn collect<'a>(shape: &'a egui::epaint::Shape, texts: &mut Vec<&'a egui::epaint::TextShape>) {
                match shape {
                    egui::epaint::Shape::Text(text) => texts.push(text),
                    egui::epaint::Shape::Vec(shapes) => {
                        for shape in shapes { collect(shape, texts); }
                    }
                    _ => {}
                }
            }
            for shape in &output.shapes { collect(&shape.shape, &mut texts); }
            for label in ["Compare", "Ctrl+Enter"] {
                let text = texts.iter().find(|text| text.galley.text() == label)
                    .expect("button label is painted");
                assert_eq!(text.opacity_factor, 1.0);
                for section in &text.galley.job.sections {
                    assert_eq!(section.format.color, t.on_accent, "{label} in {theme:?}");
                }
            }
            output.textures_delta.clear();
        }
    }

    #[test]
    fn disclosure_and_baseline_controls_publish_their_actual_semantics() {
        for expanded in [false, true] {
            let ctx = test_ctx(13.0);
            ctx.enable_accesskit();
            let mut output = ctx.run_ui(Default::default(), |ui| {
                disclosure(ui, "Settings", expanded);
                baseline_chip(ui, "A", true);
                baseline_chip(ui, "B", false);
            });
            let update = output.platform_output.accesskit_update.as_ref().unwrap();
            let node = |label| &update.nodes.iter().find(|(_, node)| node.label() == Some(label))
                .expect("named control").1;
            let settings = node("Settings");
            assert_eq!(settings.role(), egui::accesskit::Role::Button);
            assert_eq!(settings.is_expanded(), Some(expanded));
            assert_eq!(settings.toggled(), None);
            for (label, state) in [
                ("Use panel A as baseline", egui::accesskit::Toggled::True),
                ("Use panel B as baseline", egui::accesskit::Toggled::False),
            ] {
                let baseline = node(label);
                assert_eq!(baseline.role(), egui::accesskit::Role::RadioButton);
                assert_eq!(baseline.toggled(), Some(state));
            }
            output.textures_delta.clear();
        }
    }

    #[test]
    fn long_error_keeps_dismiss_and_the_comparison_inside_a_short_view() {
        for ui_pt in [13.0, 20.0] {
            let ctx = test_ctx(ui_pt);
            ctx.enable_accesskit();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(688.0, 120.0));
            let message = format!(
                "Panel A could not reload: Could not read /tmp/{}: No such file. The previous snapshot is still shown; retry Reload from disk when the file is available.",
                "long-directory-name/".repeat(18),
            );
            for _ in 0..3 {
                let mut output = ctx.run_ui(egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                }, |ui| {
                    banner(ui, &Tokens::light(), Tone::Error, &message);
                    let diff = ui.label("Comparison remains reachable");
                    assert!(screen.contains_rect(diff.rect), "comparison clipped: {:?}", diff.rect);
                    assert!(diff.rect.top() <= 72.0, "message exceeded its half-view budget: {:?}", diff.rect);
                });
                let update = output.platform_output.accesskit_update.as_ref().unwrap();
                let dismiss = update.nodes.iter().find(|(_, n)| n.label() == Some("Dismiss message"))
                    .expect("dismiss action").1.bounds().unwrap();
                assert!(dismiss.x0 >= 0.0 && dismiss.x1 <= 688.0);
                assert!(dismiss.y0 >= 0.0 && dismiss.y1 <= 120.0);
                assert!(update.nodes.iter().any(|(_, n)| n.role() == scroll_region_role()
                    && n.label() == Some("Message details")));
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn settings_fields_stack_at_the_supported_narrow_width() {
        egui::__run_test_ui(|ui| {
            // Below any possible column + gap + control, whatever the font.
            ui.set_max_width(208.0);
            let column = field_column(ui, LABELS);
            assert!(column.stacked);

            let left = ui.cursor().left();
            let response = field(ui, &Tokens::dark(), column, "Theme", |ui| {
                ui.add_sized([180.0, 24.0], Button::new("System"))
            });
            assert!(response.rect.left() >= left);
            assert!(response.rect.right() <= left + 208.0);
        });
    }

    #[test]
    fn short_empty_state_keeps_guidance_visible_and_exposes_vertical_scroll() {
        let ctx = test_ctx(13.0);
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            let t = Tokens::dark();
            let rows = [
                ("⌘O", "open a file"),
                ("⌘V", "paste text"),
                ("drop", "drop a file"),
                ("⌘N", "add a panel"),
                ("⌘/", "show shortcuts"),
            ];
            let output = ui
                .allocate_ui(Vec2::new(420.0, 120.0), |ui| {
                    let expected_id = ui.id().with(egui::IdSalt::new("empty-state-scroll"));
                    let output = empty_state_scroll(
                        ui,
                        &t,
                        "Nothing to compare yet",
                        "Load content into two panels to begin.",
                        &rows,
                    );
                    assert_eq!(output.id, expected_id);
                    output
                })
                .inner;

            assert!(
                output.content_size.y > output.inner_rect.height(),
                "short empty state did not create a vertical scroll path: content {}, viewport {}",
                output.content_size.y,
                output.inner_rect.height(),
            );
            let (title, body) = output.inner;
            assert!(
                output.inner_rect.contains_rect(title),
                "title {title:?} is outside initial viewport {:?}",
                output.inner_rect,
            );
            assert!(
                output.inner_rect.contains_rect(body),
                "body {body:?} is outside initial viewport {:?}",
                output.inner_rect,
            );
        });
        let update = output
            .platform_output
            .accesskit_update
            .as_ref()
            .expect("AccessKit tree update");
        let scroll = update
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == scroll_region_role()
                    && node.label() == Some("Getting started")
            })
            .expect("named empty-state scroll view");
        let bounds = scroll.1.bounds().expect("empty-state scroll bounds");
        assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
        assert!(bounds.x1 - bounds.x0 <= 420.0);
        assert!(bounds.y1 - bounds.y0 <= 120.0);
        output.textures_delta.clear();
    }

    #[test]
    fn settings_fields_align_when_both_columns_fit() {
        egui::__run_test_ui(|ui| {
            // Derived from the measurement rather than hardcoded, so the test
            // says nothing about which font the test context resolved -- and
            // pins the threshold exactly, in both directions.
            let column = field_column(ui, LABELS);
            let gap = ui.spacing().item_spacing.x;
            let fits = column.width + gap + FIELD_CONTROL_MIN_WIDTH;
            ui.scope(|ui| {
                ui.set_max_width(fits + 1.0);
                assert!(!field_column(ui, LABELS).stacked);
            });
            ui.scope(|ui| {
                ui.set_max_width(fits - 1.0);
                assert!(field_column(ui, LABELS).stacked);
            });
        });
    }

    #[test]
    fn a_long_label_does_not_stack_a_row_on_its_own() {
        egui::__run_test_ui(|ui| {
            let column = field_column(ui, LABELS);
            assert!(!column.stacked);
            let t = Tokens::dark();
            let short = field(ui, &t, column, "Theme", |ui| {
                ui.add_sized([180.0, 24.0], Button::new("System"))
            });
            let long = field(ui, &t, column, "Ignore lines matching", |ui| {
                ui.add_sized([180.0, 24.0], Button::new("System"))
            });
            // The regression: with the decision taken per label, the long one
            // stacked and its control started at the row's left edge instead.
            assert_eq!(short.rect.left(), long.rect.left());
        });
    }

    #[test]
    fn the_label_column_fits_every_label_it_was_measured_from() {
        egui::__run_test_ui(|ui| {
            let column = field_column(ui, LABELS);
            let body = TextStyle::Body.resolve(ui.style());
            for label in LABELS {
                let shaped = ui
                    .ctx()
                    .fonts_mut(|f| {
                        f.layout_no_wrap((*label).to_owned(), body.clone(), Color32::WHITE)
                    })
                    .size()
                    .x;
                // Nothing in the inline branch can clip or wrap, which is what
                // lets `add_sized` keep a fixed 24px row height.
                assert!(
                    shaped <= column.width,
                    "{label:?} shaped to {shaped} in a {} column",
                    column.width,
                );
            }
        });
    }

    #[test]
    fn the_label_column_is_measured_once_until_the_type_scale_changes() {
        let ctx = test_ctx(13.0);
        let mut narrow = 0.0;
        for _ in 0..2 {
            ctx.run_ui(Default::default(), |ui| {
                narrow = field_column(ui, LABELS).width;
            })
            .textures_delta
            .clear();
        }
        let memo = ctx
            .data(|d| d.get_temp::<ColumnMemo>(column_id(LABELS)))
            .expect("the column was memoized");
        assert_eq!(memo.measurements, 1, "the labels were re-shaped per frame");

        ctx.all_styles_mut(|s| crate::theme::type_scale(s, 20.0, 12.5));
        let mut wide = 0.0;
        ctx.run_ui(Default::default(), |ui| {
            wide = field_column(ui, LABELS).width;
        })
        .textures_delta
        .clear();
        let memo = ctx
            .data(|d| d.get_temp::<ColumnMemo>(column_id(LABELS)))
            .expect("the column was memoized");
        assert_eq!(
            memo.measurements, 2,
            "a bigger type scale kept a stale column"
        );
        assert!(wide > narrow, "{wide} is not wider than {narrow}");
    }

    #[test]
    fn replacing_font_definitions_invalidates_equal_metric_columns() {
        let ctx = test_ctx(13.0);
        ctx.run_ui(Default::default(), |ui| {
            field_column(ui, LABELS);
        })
        .textures_delta
        .clear();
        let before = ctx
            .data(|data| data.get_temp::<ColumnMemo>(column_id(LABELS)))
            .expect("the column was memoized");
        assert_eq!(before.measurements, 1);

        // Reinstalling the same definitions deliberately keeps all observable
        // metrics equal. The explicit revision is therefore the only reason
        // the real labels are measured again, just as required for two
        // different faces that happen to share those probe metrics.
        ctx.set_fonts(crate::fonts::definitions(None, None, None));
        font_definitions_replaced(&ctx);
        ctx.run_ui(Default::default(), |ui| {
            field_column(ui, LABELS);
        })
        .textures_delta
        .clear();
        let after = ctx
            .data(|data| data.get_temp::<ColumnMemo>(column_id(LABELS)))
            .expect("the column was re-memoized");
        assert_eq!(after.measurements, 2);
        assert_ne!(after.key.font_revision, before.key.font_revision);
    }

    #[test]
    fn a_field_label_starts_at_the_same_left_edge_whether_it_stacks_or_not() {
        let ctx = test_ctx(13.0);
        let mut left = 0.0;
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.set_max_width(600.0);
            left = ui.cursor().left();
            let t = Tokens::dark();
            let inline = field_column(ui, LABELS);
            assert!(!inline.stacked);
            field(ui, &t, inline, "Theme", |ui| ui.label("inline"));
            let stacked = FieldColumn {
                stacked: true,
                ..inline
            };
            field(ui, &t, stacked, "Theme", |ui| ui.label("stacked"));
        });
        output.textures_delta.clear();
        // The painted position, not the allocated rect: `add_sized` justifies,
        // so the label's rect is the whole column box and its left edge is the
        // row's either way. Only the galley moves, and with the default
        // `Center` placement it lands ~40px right of where the stacked branch
        // puts the same text.
        let painted = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == "Theme" => {
                    Some(text.pos.x + text.galley.rect.left())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(painted.len(), 2, "expected both labels to be painted");
        for x in painted {
            assert!(
                (x - left).abs() < 0.5,
                "label painted at {x}, row starts at {left}",
            );
        }
    }

    #[test]
    fn a_wrapped_choice_keeps_its_rows_one_gap_apart() {
        egui::__run_test_ui(|ui| {
            let t = Tokens::dark();
            let mut current = 0usize;
            // The button's own size, measured rather than derived from the
            // frame: the padding around it is the 2px inner margin *and* the
            // 1px stroke, and an assertion about the gap between rows has no
            // business knowing either. Measured here, it cancels out below.
            let button = ui
                .scope(|ui| {
                    ui.add(
                        Button::selectable(true, "A")
                            .corner_radius(CornerRadius::same(5))
                            .min_size(Vec2::new(0.0, 24.0)),
                    )
                })
                .inner
                .rect
                .size();
            // One option, so one row -- and one row's worth of padding, which
            // the wrapped frame below has exactly as much of.
            let one = ui
                .scope(|ui| {
                    choice(ui, &t, &mut current, &[(0usize, "A")]);
                })
                .response
                .rect;
            // Wide enough for two of those buttons and the 2px gap between
            // them, so the third wraps to a second row.
            let two_wide = (one.width() - button.x) + 2.0 * button.x + 2.0 + 1.0;
            let wrapped = ui
                .scope(|ui| {
                    ui.set_max_width(two_wide);
                    choice(ui, &t, &mut current, &[(0usize, "A"), (1, "A"), (2, "A")]);
                })
                .response
                .rect;
            // Two rows two pixels apart -- one object. At the theme's 8px
            // vertical rhythm this came out 6px taller and read as two.
            assert!(
                (wrapped.height() - (one.height() + button.y + 2.0)).abs() < 0.01,
                "wrapped {} vs one row {} + button {} + 2",
                wrapped.height(),
                one.height(),
                button.y,
            );
        });
    }
}
