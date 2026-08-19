//! Tests that pin the assumptions deltapanes rests on.
//!
//! Each of these corresponds to a finding in `docs/research.md`. They run
//! against the delta binary actually installed, so they double as a canary for
//! delta upgrades changing something underneath us.

use deltapanes_core::ansi::{self, Color};
use deltapanes_core::delta::{Delta, Input, Options};

const LEFT: &str = "fn main() {\n\tlet s = \"héllo wörld\";\n\tprintln!(\"{}\", s);\n}\n";
const RIGHT: &str = "fn main() {\n\tlet s = \"héllo, wörld!\";\n\tlet n = 42;\n\tprintln!(\"{} {}\", s, n);\n}\n";

fn delta() -> Delta {
    Delta::discover().expect("these tests require `delta` on PATH (brew install git-delta)")
}

fn buffers() -> (Input, Input) {
    (
        Input::Buffer(LEFT.as_bytes().to_vec()),
        Input::Buffer(RIGHT.as_bytes().to_vec()),
    )
}

/// Every rendering mode delta offers, so a parser gap shows up here rather than
/// as a mis-rendered diff.
fn all_modes() -> Vec<(&'static str, Options)> {
    let base = || Options {
        inherit_gitconfig: false,
        default_language: Some("rs".into()),
        ..Options::default()
    };
    vec![
        ("unified", base()),
        ("side-by-side", Options { side_by_side: true, ..base() }),
        ("no-line-numbers", Options { line_numbers: false, ..base() }),
        ("narrow-wrap", Options { width: 40, side_by_side: true, ..base() }),
        ("narrow-truncate", Options { width: 40, side_by_side: true, wrap: false, ..base() }),
        ("hyperlinks", Options { extra_args: vec!["--hyperlinks".into()], ..base() }),
        ("navigate", Options { extra_args: vec!["--navigate".into()], ..base() }),
        ("color-only", Options { extra_args: vec!["--color-only".into()], ..base() }),
        ("light", Options { extra_args: vec!["--light".into()], ..base() }),
        ("theme-none", Options { syntax_theme: Some("none".into()), ..base() }),
        ("raw", Options { extra_args: vec!["--raw".into()], ..base() }),
    ]
}

#[test]
fn parser_understands_every_escape_delta_emits() {
    let d = delta();
    for (name, opts) in all_modes() {
        let (l, r) = buffers();
        let out = d.render(&l, &r, &opts).expect("render");
        let (_, unknown) = ansi::parse_reporting_unknown(&out);
        assert!(
            unknown.is_empty(),
            "mode {name:?} produced escapes the parser does not handle: {unknown:?}\n\
             (delta {} -- if this fires after an upgrade, the parser needs extending)",
            d.version_string,
        );
    }
}

#[test]
fn parsing_preserves_visible_text_exactly() {
    let d = delta();
    for (name, opts) in all_modes() {
        let (l, r) = buffers();
        let out = d.render(&l, &r, &opts).expect("render");
        let parsed: String = ansi::parse(&out)
            .iter()
            .map(|line| line.plain_text())
            .collect::<Vec<_>>()
            .join("\n");
        let stripped = strip_ansi(&String::from_utf8_lossy(&out));
        assert_eq!(
            parsed.trim_end(),
            stripped.trim_end(),
            "mode {name:?}: span text diverged from delta's visible output"
        );
    }
}

/// A GUI process has no COLORTERM, and without it delta drops to 256 colours.
/// `Options::apply_env` injects it; this proves the injection actually lands.
#[test]
fn twenty_four_bit_colour_survives_an_empty_environment() {
    let d = delta();
    let (l, r) = buffers();
    let out = d
        .render(&l, &r, &Options { inherit_gitconfig: false, default_language: Some("rs".into()), ..Options::default() })
        .expect("render");
    let has_rgb = ansi::parse(&out)
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))) || matches!(s.style.bg, Some(Color::Rgb(..))));
    assert!(has_rgb, "expected 24-bit colour; delta fell back to the 256-colour palette");
}

/// delta infers syntax from the right-hand path only, so a pasted panel needs
/// `--default-language` or it renders unhighlighted. Both panels are buffers
/// here, which is the worst case.
#[test]
fn default_language_restores_highlighting_for_pasted_panels() {
    let d = delta();
    let count_colors = |lang: Option<&str>| {
        let (l, r) = buffers();
        let opts = Options {
            inherit_gitconfig: false,
            default_language: lang.map(String::from),
            ..Options::default()
        };
        let out = d.render(&l, &r, &opts).expect("render");
        let mut seen = std::collections::HashSet::new();
        for line in ansi::parse(&out) {
            for s in line.spans {
                if let Some(Color::Rgb(r, g, b)) = s.style.fg {
                    seen.insert((r, g, b));
                }
            }
        }
        seen.len()
    };
    let without = count_colors(None);
    let with = count_colors(Some("rs"));
    assert!(
        with > without,
        "--default-language=rs should add syntax colours ({with} vs {without})"
    );
}

/// `ESC[K` is how delta reaches the right edge with a diff background. Losing it
/// gives ragged backgrounds, the most visible way to render delta wrongly.
#[test]
fn end_of_line_background_fill_is_captured() {
    let d = delta();
    let (l, r) = buffers();
    let opts = Options { inherit_gitconfig: false, ..Options::default() };
    let out = d.render(&l, &r, &opts).expect("render");
    let filled = ansi::parse(&out).iter().filter(|l| l.fill_to_eol.is_some()).count();
    assert!(filled > 0, "expected some lines to carry an EL background fill");
}

/// A pasted buffer and the same bytes on disk must render identically, since
/// file-vs-paste is the headline use case.
#[test]
fn buffer_and_path_inputs_agree() {
    let d = delta();
    let dir = std::env::temp_dir().join(format!("deltapanes-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (pl, pr) = (dir.join("l.rs"), dir.join("r.rs"));
    std::fs::write(&pl, LEFT).unwrap();
    std::fs::write(&pr, RIGHT).unwrap();

    let opts = Options { inherit_gitconfig: false, default_language: Some("rs".into()), ..Options::default() };
    let from_paths = d
        .render(&Input::Path(pl), &Input::Path(pr), &opts)
        .expect("render");
    let (bl, br) = buffers();
    let from_buffers = d.render(&bl, &br, &opts).expect("render");

    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        String::from_utf8_lossy(&from_paths),
        String::from_utf8_lossy(&from_buffers),
        "pasted text rendered differently from the identical file on disk"
    );
}

#[test]
fn version_floor_is_enforced() {
    let d = delta();
    assert!(
        (d.version.0, d.version.1) >= deltapanes_core::delta::MINIMUM_VERSION,
        "discover() should have rejected {}",
        d.version_string
    );
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
