//! Turning parsed delta output into something egui can draw.

use deltapanes_core::ansi::{Color, Line, Style};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};
use unicode_width::UnicodeWidthStr;

/// Resolve delta's colours against a palette.
///
/// Indices 0-15 are the terminal's own theme colours, which delta uses for
/// structural elements (file headers, hunk rules, line numbers). A terminal
/// takes these from the user's colour scheme; we have to supply them, so these
/// are the values a default dark scheme would give.
pub struct Palette {
    pub ansi16: [Color32; 16],
    pub foreground: Color32,
    pub background: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            ansi16: [
                Color32::from_rgb(0x1c, 0x1c, 0x1c), // black
                Color32::from_rgb(0xd7, 0x54, 0x4f), // red
                Color32::from_rgb(0x6c, 0xb1, 0x5e), // green
                Color32::from_rgb(0xc7, 0xa1, 0x4a), // yellow
                Color32::from_rgb(0x58, 0x8f, 0xd4), // blue
                Color32::from_rgb(0xa8, 0x74, 0xc8), // magenta
                Color32::from_rgb(0x4f, 0xa9, 0xa9), // cyan
                Color32::from_rgb(0xc4, 0xc4, 0xc4), // white
                Color32::from_rgb(0x5c, 0x5c, 0x5c),
                Color32::from_rgb(0xef, 0x7b, 0x74),
                Color32::from_rgb(0x8d, 0xd0, 0x7c),
                Color32::from_rgb(0xe6, 0xc2, 0x6a),
                Color32::from_rgb(0x7d, 0xb0, 0xf0),
                Color32::from_rgb(0xc7, 0x96, 0xe6),
                Color32::from_rgb(0x74, 0xcb, 0xcb),
                Color32::from_rgb(0xf0, 0xf0, 0xf0),
            ],
            foreground: Color32::from_rgb(0xd4, 0xd4, 0xd4),
            background: Color32::from_rgb(0x1a, 0x1a, 0x1a),
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

/// Build a single laid-out block of text for the whole diff.
///
/// Keeping it as one `LayoutJob` is what lets egui handle selection and copy
/// across the entire view for free -- which the research flagged as the thing a
/// diff reviewer actually uses, and the reason to parse ANSI rather than embed
/// a terminal.
pub fn to_layout_job(lines: &[Line], columns: usize, font: FontId, palette: &Palette) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;

    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            job.append("\n", 0.0, TextFormat { font_id: font.clone(), ..Default::default() });
        }
        let mut used = 0usize;
        for span in &line.spans {
            used += UnicodeWidthStr::width(span.text.as_str());
            job.append(&span.text, 0.0, format_for(&span.style, font.clone(), palette));
        }
        // `ESC[K` asks the terminal to paint the current background out to the
        // right edge. delta already knows the column count -- we passed it via
        // --width -- so padding with spaces in that background reproduces it
        // exactly, and keeps the whole diff inside one selectable text block.
        if let Some(bg) = line.fill_to_eol {
            if columns > used {
                let pad = " ".repeat(columns - used);
                let style = Style { bg: Some(bg), ..Default::default() };
                job.append(&pad, 0.0, format_for(&style, font.clone(), palette));
            }
        }
    }
    job
}

fn format_for(style: &Style, font: FontId, palette: &Palette) -> TextFormat {
    let (mut fg, mut bg) = (
        style.fg.map(|c| palette.resolve(c)).unwrap_or(palette.foreground),
        style.bg.map(|c| palette.resolve(c)).unwrap_or(Color32::TRANSPARENT),
    );
    if style.reverse {
        let bg_solid = if bg == Color32::TRANSPARENT { palette.background } else { bg };
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
        underline: if style.underline { egui::Stroke::new(1.0, fg) } else { egui::Stroke::NONE },
        strikethrough: if style.strike { egui::Stroke::new(1.0, fg) } else { egui::Stroke::NONE },
        ..Default::default()
    }
}
