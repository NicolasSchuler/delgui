//! Parser for the ANSI subset `delta` emits.
//!
//! Empirically (see `docs/research.md`), across every rendering mode delta
//! supports it emits exactly three things: SGR (`ESC[..m`) in 4-bit, 256-colour
//! and 24-bit forms, EL (`ESC[K`) to extend the current background to the end of
//! the line, and OSC 8 hyperlinks. No cursor motion, no scroll regions, no
//! alternate screen. That is why this is a span parser and not a terminal.

use anstyle_parse::{DefaultCharAccumulator, Params, Parser, Perform};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    /// 4-bit and 256-colour palette entries, both normalised to a palette index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strike: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
    /// Target of the enclosing OSC 8 hyperlink, if any (`--hyperlinks`).
    pub link: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Line {
    pub spans: Vec<Span>,
    /// Background to paint from the last span to the right edge, set by `ESC[K`.
    ///
    /// delta uses this so that a removed/added line's colour reaches the edge of
    /// the terminal regardless of how long the text is. A renderer that ignores
    /// it produces ragged diff backgrounds, which is the most visible way to get
    /// delta's output subtly wrong.
    pub fill_to_eol: Option<Color>,
}

impl Line {
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Parse delta's stdout into styled lines.
pub fn parse(bytes: &[u8]) -> Vec<Line> {
    let mut parser = Parser::<DefaultCharAccumulator>::new();
    let mut sink = Sink::default();
    for &b in bytes {
        parser.advance(&mut sink, b);
    }
    sink.finish()
}

#[derive(Default)]
struct Sink {
    lines: Vec<Line>,
    current: Line,
    pending: String,
    style: Style,
    link: Option<String>,
    /// Escapes we did not recognise. The research predicts this stays empty;
    /// `unknown_escapes` exists so a regression shows up as a test failure
    /// rather than as silently mis-rendered output.
    unknown: Vec<String>,
}

impl Sink {
    fn flush_span(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.current.spans.push(Span {
            text: std::mem::take(&mut self.pending),
            style: self.style,
            link: self.link.clone(),
        });
    }

    fn newline(&mut self) {
        self.flush_span();
        self.lines.push(std::mem::take(&mut self.current));
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush_span();
        if !self.current.spans.is_empty() || self.current.fill_to_eol.is_some() {
            self.lines.push(std::mem::take(&mut self.current));
        }
        self.lines
    }

    fn sgr(&mut self, params: &Params) {
        // Flatten `38;2;r;g;b` (semicolon form, which is what delta emits) and
        // `38:2::r:g:b` (colon subparameter form) into one stream.
        let flat: Vec<u16> = params.iter().flat_map(|sub| sub.iter().copied()).collect();
        if flat.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            match flat[i] {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.reverse = true,
                9 => self.style.strike = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                29 => self.style.strike = false,
                30..=37 => self.style.fg = Some(Color::Indexed((flat[i] - 30) as u8)),
                90..=97 => self.style.fg = Some(Color::Indexed((flat[i] - 90 + 8) as u8)),
                39 => self.style.fg = None,
                40..=47 => self.style.bg = Some(Color::Indexed((flat[i] - 40) as u8)),
                100..=107 => self.style.bg = Some(Color::Indexed((flat[i] - 100 + 8) as u8)),
                49 => self.style.bg = None,
                38 | 48 => {
                    let is_fg = flat[i] == 38;
                    let (color, consumed) = match flat.get(i + 1) {
                        Some(2) => (
                            flat.get(i + 2)
                                .zip(flat.get(i + 3))
                                .zip(flat.get(i + 4))
                                .map(|((r, g), b)| Color::Rgb(*r as u8, *g as u8, *b as u8)),
                            4,
                        ),
                        Some(5) => (flat.get(i + 2).map(|n| Color::Indexed(*n as u8)), 2),
                        _ => (None, 1),
                    };
                    match color {
                        Some(c) if is_fg => self.style.fg = Some(c),
                        Some(c) => self.style.bg = Some(c),
                        None => self.unknown.push(format!("SGR {:?}", flat)),
                    }
                    i += consumed;
                }
                other => self.unknown.push(format!("SGR {other}")),
            }
            i += 1;
        }
    }
}

impl Perform for Sink {
    fn print(&mut self, c: char) {
        self.pending.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => {}
            b'\t' => self.pending.push('\t'),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: u8) {
        if !intermediates.is_empty() {
            self.unknown.push(format!("CSI ? {}", action as char));
            return;
        }
        match action {
            b'm' => {
                self.flush_span();
                self.sgr(params);
            }
            b'K' => {
                // Erase-in-line. delta only ever emits EL(0) ("to end of line"),
                // which means: paint the current background out to the edge.
                self.flush_span();
                self.current.fill_to_eol = self.style.bg;
            }
            other => self.unknown.push(format!("CSI {}", other as char)),
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 8 ; params ; URI  -- an empty URI closes the hyperlink.
        if params.first().map(|p| p == b"8").unwrap_or(false) {
            self.flush_span();
            self.link = match params.get(2) {
                Some(uri) if !uri.is_empty() => Some(String::from_utf8_lossy(uri).into_owned()),
                _ => None,
            };
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        // ST (`ESC \\`) terminates an OSC 8 hyperlink. anstyle-parse delivers the
        // OSC payload first and then hands us the terminator, so seeing it here
        // is normal rather than an unhandled escape.
        if byte == b'\\' {
            return;
        }
        self.unknown.push(format!("ESC {}", byte as char));
    }
}

/// Parse, and additionally report anything the parser did not understand.
///
/// Used by the fidelity tests: an empty `Vec<String>` is the guarantee that
/// this parser covers everything the installed delta emits.
pub fn parse_reporting_unknown(bytes: &[u8]) -> (Vec<Line>, Vec<String>) {
    let mut parser = Parser::<DefaultCharAccumulator>::new();
    let mut sink = Sink::default();
    for &b in bytes {
        parser.advance(&mut sink, b);
    }
    let unknown = std::mem::take(&mut sink.unknown);
    (sink.finish(), unknown)
}
