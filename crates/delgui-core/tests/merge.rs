//! Tests that pin the arithmetic a result panel is assembled by.
//!
//! Like `fidelity.rs`, these run against the delta and git actually installed:
//! the hunks under test are the ones those two binaries produced, not a fixture.
//! The property that matters is in `taking_every_hunk_reproduces_the_candidate`
//! -- every dangerous bug here is one integer, none of them is visible by
//! inspection, and all of them produce output that looks plausible in a buffer
//! the user is about to write to disk.

use delgui_core::ansi::{self, Line};
use delgui_core::delta::{Delta, Input, Options};
use delgui_core::merge::{self, Hunk};

fn delta() -> Delta {
    Delta::discover().expect("these tests require `delta` on PATH (brew install git-delta)")
}

/// Exactly what merge mode passes.
fn options() -> Options {
    Options {
        inherit_gitconfig: false,
        marked_hunks: true,
        // Merge mode asks for both: marks to find the hunks by, and no header,
        // because it draws its own control row there and delta's header
        // decoration would be drawn inside the hunk instead of above it.
        hunk_headers: false,
        pin_hunk_structure: true,
        context: 0,
        default_language: Some("rs".into()),
        ..Options::default()
    }
}

/// The two steps merge mode runs: our own diff, then delta over it.
fn pipeline(base: &str, cand: &str, opts: &Options) -> (Vec<Hunk>, Vec<Line>) {
    let delta = delta();
    let (left, right) = (
        Input::Buffer(base.as_bytes().to_vec()),
        Input::Buffer(cand.as_bytes().to_vec()),
    );
    let patch = delta.diff(&left, &right, opts).expect("git diff");
    let rendered = delta.render_patch(&patch, opts).expect("delta");
    (merge::parse(&patch), ansi::parse(&rendered))
}

fn rows(lines: &[Line]) -> &[Line] {
    ansi::body(lines)
}

/// base, candidate -- every shape of edit that has its own off-by-one.
const CASES: &[(&str, &str, &str)] = &[
    ("one line changed", "a\nb\nc\n", "a\nB\nc\n"),
    ("insert at the start", "b\nc\n", "a\nb\nc\n"),
    ("insert at the end", "a\nb\n", "a\nb\nc\n"),
    ("delete at the start", "a\nb\nc\n", "b\nc\n"),
    ("delete at the end", "a\nb\nc\n", "a\nb\n"),
    ("a single line, whole file", "x\n", "y\n"),
    ("empty into content", "", "one\ntwo\nthree\n"),
    ("content into empty", "one\ntwo\nthree\n", ""),
    ("no newline on both sides", "a\nb", "a\nB"),
    ("no newline on the base only", "a\nb", "a\nb\nc\n"),
    ("no newline on the candidate only", "a\nb\nc\n", "a\nb"),
    ("no newline, spliced in the middle", "a\nb\nc", "A\nb\nC"),
    ("crlf on both sides", "a\r\nb\r\n", "a\r\nB\r\n"),
    ("crlf against lf", "a\r\nb\r\n", "a\nB\n"),
    ("blank lines", "a\n\n\nb\n", "a\n\nb\n"),
    ("unicode", "héllo\nwörld\n", "héllo\nwörld!\n"),
    (
        "scattered changes",
        "l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n",
        "L0\nl1\nl2\nX\nl3\nl4\nl6\nl7\nL8\nl9\n",
    ),
    (
        // The reason the structure is read from the diff and not the render: a
        // line of content here is indistinguishable from a hunk header once the
        // styling is stripped.
        "two patches",
        "head\n@@ -1,5 +1,5 @@\nbody one\ntail\n",
        "head\n@@ -1,5 +1,5 @@\nbody two\ntail\n",
    ),
];

/// The property everything else rests on: the hunks are a complete, correct
/// description of the difference, so taking all of them lands exactly on the
/// candidate. Bottom-up, because taking a hunk renumbers the ones below it.
#[test]
fn taking_every_hunk_reproduces_the_candidate() {
    let opts = options();
    for (name, base, cand) in CASES {
        let (hunks, _) = pipeline(base, cand, &opts);
        assert!(
            !hunks.is_empty(),
            "{name}: no hunks for two different buffers"
        );
        assert!(
            merge::verify(base, cand, &hunks),
            "{name}: the hunks do not describe these buffers"
        );
        let mut got = base.to_string();
        for h in hunks.iter().rev() {
            got = merge::take(&got, cand, h).unwrap_or_else(|| panic!("{name}: take refused"));
        }
        assert_eq!(&got, cand, "{name}");
    }
}

/// The same in the other order, which is what a user clicking down the list
/// does: after each take the diff is recomputed, and the hunk that was first is
/// simply gone.
#[test]
fn taking_the_first_hunk_repeatedly_converges_on_the_candidate() {
    let opts = options();
    for (name, base, cand) in CASES {
        let mut got = base.to_string();
        let mut left = pipeline(&got, cand, &opts).0.len();
        let mut guard = left + 2;
        while left > 0 {
            let (hunks, _) = pipeline(&got, cand, &opts);
            assert_eq!(
                hunks.len(),
                left,
                "{name}: a take changed more than one hunk"
            );
            got = merge::take(&got, cand, &hunks[0]).expect("take");
            left -= 1;
            guard -= 1;
            assert!(guard > 0, "{name}: not converging");
        }
        assert_eq!(&got, cand, "{name}");
    }
}

/// Every render is marked now, not only the ones a result is being built from,
/// because that is where Previous/Next change gets its list of differences. This
/// is what makes that free: with the marked rows taken back out, what is left is
/// exactly what `--hunk-header-style=omit` would have drawn.
///
/// If this fails after a delta upgrade, plain comparisons have started rendering
/// differently from the way they always have -- which is worth failing over.
#[test]
fn dropping_the_marked_rows_restores_the_unmarked_rendering() {
    for side_by_side in [false, true] {
        for context in [0u32, 3] {
            let marked = Options {
                hunk_headers: false,
                side_by_side,
                context,
                ..options()
            };
            let unmarked = Options {
                marked_hunks: false,
                ..marked.clone()
            };
            for (name, base, cand) in CASES {
                let (hunks, lines) = pipeline(base, cand, &marked);
                let (drawn, spans) = merge::prepare_rows(rows(&lines), hunks.len(), false);
                assert!(
                    spans.is_some() || hunks.is_empty(),
                    "{name}: could not locate {} hunks at context {context}",
                    hunks.len(),
                );
                let (_, plain) = pipeline(base, cand, &unmarked);
                assert_eq!(
                    drawn,
                    ansi::body(&plain),
                    "{name}: side_by_side={side_by_side} context={context} -- \
                     asking delta to mark the hunks changed what is drawn",
                );
            }
        }
    }
}

/// The other half: with hunk headers on, the marked row stays and becomes the
/// header. The difference from an unmarked render is then confined to that row
/// and to the decoration rules delta draws around it, which it sizes to the
/// header text -- so they come out as wide as the label made it. That is the
/// whole price of being able to navigate a diff whose headers are switched on.
#[test]
fn a_kept_marked_row_is_the_header_without_its_mark() {
    let marked = Options {
        hunk_headers: true,
        ..options()
    };
    let unmarked = Options {
        marked_hunks: false,
        ..marked.clone()
    };
    let (name, base, cand) = CASES[16];
    let (hunks, lines) = pipeline(base, cand, &marked);
    let (drawn, spans) = merge::prepare_rows(rows(&lines), hunks.len(), true);
    let spans = spans.unwrap_or_else(|| panic!("{name}: marks do not match the hunks"));
    let (_, plain) = pipeline(base, cand, &unmarked);

    assert_eq!(
        drawn.len(),
        ansi::body(&plain).len(),
        "{name}: keeping the marked rows changed how many rows are drawn",
    );
    for span in &spans {
        assert!(
            !drawn[span.start].spans.iter().any(|s| s.text.contains('␟')),
            "{name}: the mark is still visible in the header it was written on",
        );
    }
    // Every row is untouched but the headers and the rules delta drew for them.
    let rule = |line: &Line| {
        let text: String = line.spans.iter().map(|s| s.text.as_str()).collect();
        !text.is_empty() && text.chars().all(|c| "─│┌┐└┘├┤┄┈".contains(c))
    };
    let headers: Vec<usize> = spans.iter().map(|s| s.start).collect();
    for (i, (ours, theirs)) in drawn.iter().zip(ansi::body(&plain)).enumerate() {
        if headers.contains(&i) || (rule(ours) && rule(theirs)) {
            continue;
        }
        assert_eq!(ours, theirs, "{name}: row {i} changed and is not a header");
    }
}

/// Each hunk has to be findable on screen, or the control row lands beside lines
/// it does not act on. This is the one thing still read out of the rendering, so
/// it is where a delta upgrade should draw blood.
#[test]
fn every_hunk_is_marked_exactly_once_in_the_rendering() {
    let modes = [
        ("unified", Options { ..options() }),
        (
            "side-by-side",
            Options {
                side_by_side: true,
                ..options()
            },
        ),
        (
            "no line numbers",
            Options {
                line_numbers: false,
                ..options()
            },
        ),
        (
            "side-by-side without line numbers",
            Options {
                side_by_side: true,
                line_numbers: false,
                ..options()
            },
        ),
        (
            "narrow, wrapped",
            Options {
                width: 24,
                ..options()
            },
        ),
        (
            "narrow, truncated",
            Options {
                width: 24,
                wrap: false,
                ..options()
            },
        ),
        (
            "hunk headers switched off by the user",
            Options {
                hunk_headers: false,
                ..options()
            },
        ),
    ];
    for (mode, opts) in modes {
        for (name, base, cand) in CASES {
            let (hunks, lines) = pipeline(base, cand, &opts);
            let located = merge::locate(rows(&lines), hunks.len());
            let located = located.unwrap_or_else(|| {
                panic!("{mode}/{name}: marks do not match {} hunks", hunks.len())
            });
            assert_eq!(located.len(), hunks.len(), "{mode}/{name}");
            // The spans are ordered and disjoint, and everything between them
            // is a marker row or one of delta's blank separators -- never a row
            // of the diff that belongs to no hunk.
            let drawn = rows(&lines);
            let mut previous = 0;
            for span in &located {
                assert!(span.start >= previous, "{mode}/{name}: spans overlap");
                for row in &drawn[previous..span.start.saturating_sub(1)] {
                    assert!(
                        row.spans.is_empty() && row.fill_to_eol.is_none(),
                        "{mode}/{name}: a row of the diff belongs to no hunk"
                    );
                }
                previous = span.end;
            }
            for row in &drawn[previous..] {
                assert!(
                    row.spans.is_empty() && row.fill_to_eol.is_none(),
                    "{mode}/{name}: rows left over after the last hunk"
                );
            }
        }
    }
}

/// The take unit must not depend on how the diff is drawn.
#[test]
fn side_by_side_and_unified_agree_on_the_hunks() {
    let (unified, _) = pipeline(CASES[16].1, CASES[16].2, &options());
    let (side, _) = pipeline(
        CASES[16].1,
        CASES[16].2,
        &Options {
            side_by_side: true,
            ..options()
        },
    );
    assert_eq!(unified, side);
}

/// Every render pipes our own `git diff` into delta now, so this no longer
/// guards a seam between two modes -- `patch_path_matches_two_file_mode` covers
/// that. What it still pins is the case merge mode is hardest on: a marked,
/// zero-context, side-by-side render of the fixture with the most hunks.
/// (research.md §7)
#[test]
fn piping_our_own_diff_renders_exactly_as_deltas_two_file_mode_does() {
    let delta = delta();
    let opts = Options {
        inherit_gitconfig: false,
        default_language: Some("rs".into()),
        side_by_side: true,
        ..Options::default()
    };
    let (base, cand) = (CASES[16].1, CASES[16].2);
    let (left, right) = (
        Input::Buffer(base.as_bytes().to_vec()),
        Input::Buffer(cand.as_bytes().to_vec()),
    );
    let direct = delta.render(&left, &right, &opts).expect("delta");
    let patch = delta.diff(&left, &right, &opts).expect("git diff");
    let piped = delta.render_patch(&patch, &opts).expect("delta");
    assert_eq!(direct, piped);
}

/// Context is what decides how coarse a take is, and the default is far too
/// coarse to pick from: four independent edits in a thirteen-line file arrive as
/// one hunk covering the whole file.
#[test]
fn zero_context_is_what_makes_the_changes_separately_takeable() {
    let (base, cand) = (
        include_str!("../../../examples/config_before.rs"),
        include_str!("../../../examples/config_after.rs"),
    );
    let coarse = pipeline(
        base,
        cand,
        &Options {
            context: 3,
            ..options()
        },
    )
    .0;
    let fine = pipeline(base, cand, &options()).0;
    assert_eq!(coarse.len(), 1, "git's default context merges the lot");
    assert!(
        fine.len() >= 4,
        "zero context should separate the four changes, got {}",
        fine.len()
    );
}

/// Two buffers that match produce no hunks and nothing to take, rather than an
/// empty hunk that would splice nothing.
#[test]
fn identical_buffers_have_nothing_to_take() {
    let (hunks, lines) = pipeline("same\ntext\n", "same\ntext\n", &options());
    assert!(hunks.is_empty());
    assert!(rows(&lines).is_empty());
    assert!(merge::verify("same\ntext\n", "same\ntext\n", &hunks));
}
