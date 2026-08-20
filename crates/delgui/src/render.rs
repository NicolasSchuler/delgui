//! Turning parsed delta output into something egui can draw.

use std::ops::Range;
use std::sync::Arc;

use delgui_core::ansi::{self, Color, Line, Style};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Color32, Context, CursorIcon, FontId, Galley, Key, Modifiers, OpenUrl, Response, Sense, Stroke,
    Ui, Vec2, Widget, WidgetInfo, WidgetType,
};
use unicode_width::UnicodeWidthStr;

use crate::theme::Tokens;

/// Resolve delta's colours against a palette.
///
/// Indices 0-15 are the terminal's own theme colours, which delta uses for
/// structural elements (file headers, hunk rules, line numbers). A terminal
/// takes these from the user's colour scheme; we have to supply them, so they
/// come from the app's theme -- which is also why index 4 is the chrome accent:
/// it is the only one delta reaches for by default.
pub struct Palette {
    pub ansi16: [Color32; 16],
    pub foreground: Color32,
    pub background: Color32,
}

impl Palette {
    /// The two are built together on purpose. `background` is not a colour of
    /// its own: it is *whatever the diff is drawn on*, and it is used to
    /// resolve reverse-video spans. Typing it independently is how it drifted
    /// one grey step away from the surface it was supposed to match.
    pub fn from_tokens(t: &Tokens) -> Self {
        Self {
            ansi16: t.ansi16,
            foreground: t.text_primary,
            background: t.surface_sunken,
        }
    }

    fn resolve(&self, c: Color) -> Color32 {
        match c {
            Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
            Color::Indexed(i) => xterm256(i, &self.ansi16),
        }
    }
}

/// The standard xterm 256-colour cube, with the first 16 deferred to the theme.
fn xterm256(i: u8, ansi16: &[Color32; 16]) -> Color32 {
    match i {
        0..=15 => ansi16[i as usize],
        16..=231 => {
            let n = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

/// True when delta printed nothing to draw: the two inputs are the same.
pub fn is_empty(lines: &[Line]) -> bool {
    ansi::body(lines).is_empty()
}

/// One OSC 8 link in a prepared diff, addressed in character offsets so it can
/// be hit-tested against an egui [`Galley`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hyperlink {
    pub char_range: Range<usize>,
    pub target: String,
}

/// The visual tail requested by ANSI erase-to-end-of-line (`ESC[K`).
///
/// It is metadata rather than spaces in the laid-out text: terminal erase cells
/// are paint, not file content, and must not turn into trailing whitespace when
/// the user copies a selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EraseFill {
    pub row: usize,
    pub from_column: usize,
    pub background: Color32,
}

/// A diff block laid out once and cheap to reuse on every frame.
///
/// Cache this alongside the render/style key and draw it with [`Self::label`].
/// The label keeps egui's cross-label selectable-text behavior, makes OSC 8
/// spans clickable, and paints `ESC[K` without adding synthetic spaces to the
/// copied text.
pub struct PreparedLayout {
    galley: Arc<Galley>,
    columns: usize,
    hyperlinks: Vec<Hyperlink>,
    erase_fills: Vec<EraseFill>,
}

impl PreparedLayout {
    pub fn label(&self, column_width: f32) -> PreparedLabel<'_> {
        PreparedLabel {
            prepared: self,
            column_width: column_width.max(1.0),
        }
    }

    pub fn text(&self) -> &str {
        self.galley.text()
    }

    #[cfg(test)]
    pub fn hyperlinks(&self) -> &[Hyperlink] {
        &self.hyperlinks
    }

    #[cfg(test)]
    pub fn erase_fills(&self) -> &[EraseFill] {
        &self.erase_fills
    }
}

/// Build and shape a reusable diff block.
///
/// This must run on the UI thread because font shaping belongs to egui's
/// [`Context`]. Rebuild when the rendered rows, column count, font, palette,
/// line height or pixels-per-point change; otherwise retain the returned value.
pub fn prepare_layout(
    ctx: &Context,
    rows: &[Line],
    columns: usize,
    font: FontId,
    palette: &Palette,
    line_height: f32,
) -> PreparedLayout {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.keep_trailing_whitespace = true;
    job.first_row_min_height = line_height;
    let plain = TextFormat {
        font_id: font.clone(),
        line_height: Some(line_height),
        ..Default::default()
    };
    let mut hyperlinks: Vec<Hyperlink> = Vec::new();
    let mut erase_fills = Vec::new();
    let mut char_offset = 0usize;

    for (row, line) in rows.iter().enumerate() {
        if row > 0 {
            job.append("\n", 0.0, plain.clone());
            char_offset += 1;
        }
        let mut used = 0usize;
        for span in &line.spans {
            let start = char_offset;
            used += UnicodeWidthStr::width(span.text.as_str());
            char_offset += span.text.chars().count();
            job.append(
                &span.text,
                0.0,
                format_for(&span.style, font.clone(), palette, line_height),
            );
            if let Some(target) = &span.link
                && start < char_offset
            {
                if let Some(last) = hyperlinks.last_mut()
                    && last.target == *target
                    && last.char_range.end == start
                {
                    last.char_range.end = char_offset;
                } else {
                    hyperlinks.push(Hyperlink {
                        char_range: start..char_offset,
                        target: target.clone(),
                    });
                }
            }
        }
        if let Some(bg) = line.fill_to_eol
            && columns > used
        {
            erase_fills.push(EraseFill {
                row,
                from_column: used,
                background: palette.resolve(bg),
            });
        }
    }

    let galley = ctx.fonts_mut(|fonts| fonts.layout_job(job));
    PreparedLayout {
        galley,
        columns,
        hyperlinks,
        erase_fills,
    }
}

/// The selectable widget backed by a cached [`PreparedLayout`].
pub struct PreparedLabel<'a> {
    prepared: &'a PreparedLayout,
    column_width: f32,
}

impl Widget for PreparedLabel<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let prepared = self.prepared;
        let galley = prepared.galley.clone();
        let size = Vec2::new(
            galley
                .size()
                .x
                .max(prepared.columns as f32 * self.column_width),
            galley.size().y,
        );
        let mut sense = Sense::hover();
        let selection = if ui.input(|i| i.has_touch_screen()) {
            Sense::click()
        } else {
            Sense::click_and_drag()
        };
        sense |= selection;
        let (rect, mut response) = ui.allocate_exact_size(size, sense);
        let galley_pos = rect.left_top();
        response
            .widget_info(|| WidgetInfo::labeled(WidgetType::Label, ui.is_enabled(), galley.text()));

        if ui.is_rect_visible(rect) {
            for fill in &prepared.erase_fills {
                let Some(row) = galley.rows.get(fill.row) else {
                    continue;
                };
                let row_rect = row.rect().translate(galley_pos.to_vec2());
                let start = (galley_pos.x + fill.from_column as f32 * self.column_width)
                    .max(row_rect.right());
                let end = galley_pos.x + prepared.columns as f32 * self.column_width;
                if start < end {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(start, row_rect.top()),
                            egui::pos2(end, row_rect.bottom()),
                        ),
                        0.0,
                        fill.background,
                    );
                }
            }

            egui::text_selection::LabelSelectionState::label_text_selection(
                ui,
                &response,
                galley_pos,
                galley.clone(),
                ui.visuals().text_color(),
                Stroke::NONE,
            );

            if response.has_focus() {
                let page = ui.clip_rect().height().max(48.0) * 0.9;
                let delta = ui.input_mut(|input| {
                    if input.consume_key(Modifiers::NONE, Key::PageDown) {
                        -page
                    } else if input.consume_key(Modifiers::NONE, Key::PageUp) {
                        page
                    } else if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                        -32.0
                    } else if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        32.0
                    } else if input.consume_key(Modifiers::NONE, Key::End) {
                        -1.0e9
                    } else if input.consume_key(Modifiers::NONE, Key::Home) {
                        1.0e9
                    } else {
                        0.0
                    }
                });
                if delta != 0.0 {
                    ui.scroll_with_delta(Vec2::new(0.0, delta));
                }
                ui.painter().rect_stroke(
                    response.rect.intersect(ui.clip_rect()),
                    0.0,
                    ui.visuals().widgets.active.bg_stroke,
                    egui::StrokeKind::Inside,
                );
            }

            let hovered = ui
                .input(|i| i.pointer.hover_pos())
                .filter(|p| rect.contains(*p))
                .map(|p| galley.cursor_from_pos(p - galley_pos).index.0)
                .and_then(|index| {
                    prepared
                        .hyperlinks
                        .iter()
                        .find(|link| link.char_range.contains(&index))
                });
            if let Some(link) = hovered {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                if response.clicked_with_open_in_background() {
                    ui.open_url(OpenUrl {
                        url: link.target.clone(),
                        new_tab: true,
                    });
                } else if response.clicked() {
                    ui.open_url(OpenUrl {
                        url: link.target.clone(),
                        new_tab: false,
                    });
                }
                if ui.style().url_in_tooltip {
                    response = response.on_hover_text(link.target.clone());
                }
            }
        }
        response.ctx.accesskit_node_builder(response.id, |node| {
            node.set_label("Rendered diff");
        });
        response
    }
}

/// Build one laid-out block of text for a run of rendered rows.
///
/// Keeping the diff in as few `LayoutJob`s as possible is what lets egui handle
/// selection and copy across the view for free -- which the research flagged as
/// the thing a diff reviewer actually uses, and the reason to parse ANSI rather
/// than embed a terminal. Merge mode draws one per hunk so it can cut a control
/// row in between; egui stitches selection across adjacent labels, so that stays
/// true there.
///
/// The rows are used exactly as given: trimming belongs to `ansi::body`, which
/// has to be the only place that decides where the body starts, or the row
/// indices the hunks were located against silently shift.
///
/// `columns` must be the width delta itself laid out against, not the window's
/// current width: the padding below has to land where delta thought the right
/// edge was, and during a re-render those two disagree.
#[cfg(test)]
pub fn to_layout_job(
    rows: &[Line],
    columns: usize,
    font: FontId,
    palette: &Palette,
    line_height: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let plain = TextFormat {
        font_id: font.clone(),
        // A blank line takes its height from the format of whatever *opened*
        // the paragraph -- which is this newline. Leave it unset and every empty
        // row in the diff is shorter than the rows around it.
        line_height: Some(line_height),
        ..Default::default()
    };

    for (idx, line) in rows.iter().enumerate() {
        if idx > 0 {
            job.append("\n", 0.0, plain.clone());
        }
        let mut used = 0usize;
        for span in &line.spans {
            used += UnicodeWidthStr::width(span.text.as_str());
            job.append(
                &span.text,
                0.0,
                format_for(&span.style, font.clone(), palette, line_height),
            );
        }
        // `ESC[K` asks the terminal to paint the current background out to the
        // right edge. delta already knows the column count -- we passed it via
        // --width -- so padding with spaces in that background reproduces it
        // exactly, and keeps the whole diff inside one selectable text block.
        if let Some(bg) = line.fill_to_eol
            && columns > used
        {
            let pad = " ".repeat(columns - used);
            let style = Style {
                bg: Some(bg),
                ..Default::default()
            };
            job.append(
                &pad,
                0.0,
                format_for(&style, font.clone(), palette, line_height),
            );
        }
    }
    job
}

fn format_for(style: &Style, font: FontId, palette: &Palette, line_height: f32) -> TextFormat {
    let (mut fg, mut bg) = (
        style
            .fg
            .map(|c| palette.resolve(c))
            .unwrap_or(palette.foreground),
        style
            .bg
            .map(|c| palette.resolve(c))
            .unwrap_or(Color32::TRANSPARENT),
    );
    if style.reverse {
        let bg_solid = if bg == Color32::TRANSPARENT {
            palette.background
        } else {
            bg
        };
        (fg, bg) = (bg_solid, fg);
    }
    if style.dim {
        fg = fg.gamma_multiply(0.6);
    }
    TextFormat {
        font_id: font,
        color: fg,
        background: bg,
        italics: style.italic,
        underline: if style.underline {
            egui::Stroke::new(1.0, fg)
        } else {
            egui::Stroke::NONE
        },
        strikethrough: if style.strike {
            egui::Stroke::new(1.0, fg)
        } else {
            egui::Stroke::NONE
        },
        line_height: Some(line_height),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delgui_core::ansi::Span;

    fn palette() -> Palette {
        Palette {
            ansi16: [Color32::BLACK; 16],
            foreground: Color32::WHITE,
            background: Color32::BLACK,
        }
    }

    fn span(text: &str, link: Option<&str>) -> Span {
        Span {
            text: text.into(),
            style: Style::default(),
            link: link.map(str::to_owned),
        }
    }

    #[test]
    fn prepared_layout_keeps_erase_padding_out_of_selectable_text() {
        egui::__run_test_ui(|ui| {
            let rows = [Line {
                spans: vec![span("abc", None)],
                fill_to_eol: Some(Color::Indexed(1)),
            }];
            let prepared = prepare_layout(
                ui.ctx(),
                &rows,
                8,
                FontId::monospace(12.0),
                &palette(),
                16.0,
            );
            assert_eq!(prepared.text(), "abc");
            assert_eq!(
                prepared.erase_fills(),
                &[EraseFill {
                    row: 0,
                    from_column: 3,
                    background: Color32::BLACK,
                }]
            );
            let response = ui.add(prepared.label(7.0));
            assert!(response.rect.width() >= 56.0);
        });
    }

    #[test]
    fn prepared_layout_carries_osc8_links_as_character_ranges() {
        egui::__run_test_ui(|ui| {
            let rows = [Line {
                spans: vec![
                    span("α", None),
                    span("β", Some("https://example.test/a")),
                    span("γ", Some("https://example.test/a")),
                    span("!", None),
                ],
                fill_to_eol: None,
            }];
            let prepared = prepare_layout(
                ui.ctx(),
                &rows,
                8,
                FontId::monospace(12.0),
                &palette(),
                16.0,
            );
            assert_eq!(prepared.text(), "αβγ!");
            assert_eq!(
                prepared.hyperlinks(),
                &[Hyperlink {
                    char_range: 1..3,
                    target: "https://example.test/a".into(),
                }]
            );
        });
    }

    #[test]
    fn legacy_layout_job_keeps_padding_until_the_owner_integrates_prepared_labels() {
        let rows = [Line {
            spans: vec![span("abc", None)],
            fill_to_eol: Some(Color::Indexed(1)),
        }];
        let job = to_layout_job(&rows, 8, FontId::monospace(12.0), &palette(), 16.0);
        assert_eq!(job.text, "abc     ");
    }
}
