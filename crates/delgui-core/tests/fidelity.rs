//! Tests that pin the assumptions delgui rests on.
//!
//! Each of these corresponds to a finding in `docs/research.md`. They run
//! against the delta binary actually installed, so they double as a canary for
//! delta upgrades changing something underneath us.

use delgui_core::ansi::{self, Color};
use delgui_core::delta::{
    Appearance, Cancel, Delta, DeltaError, Granularity, Input, Options, Whitespace,
};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const LEFT: &str = "fn main() {\n\tlet s = \"héllo wörld\";\n\tprintln!(\"{}\", s);\n}\n";
const RIGHT: &str =
    "fn main() {\n\tlet s = \"héllo, wörld!\";\n\tlet n = 42;\n\tprintln!(\"{} {}\", s, n);\n}\n";

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
        (
            "side-by-side",
            Options {
                side_by_side: true,
                ..base()
            },
        ),
        (
            "side-by-side-no-line-numbers",
            Options {
                side_by_side: true,
                line_numbers: false,
                ..base()
            },
        ),
        (
            "no-line-numbers",
            Options {
                line_numbers: false,
                ..base()
            },
        ),
        (
            "narrow-wrap",
            Options {
                width: 40,
                side_by_side: true,
                ..base()
            },
        ),
        (
            "narrow-truncate",
            Options {
                width: 40,
                side_by_side: true,
                wrap: false,
                ..base()
            },
        ),
        (
            "hyperlinks",
            Options {
                extra_args: vec!["--hyperlinks".into()],
                ..base()
            },
        ),
        (
            "navigate",
            Options {
                extra_args: vec!["--navigate".into()],
                ..base()
            },
        ),
        (
            "color-only",
            Options {
                extra_args: vec!["--color-only".into()],
                ..base()
            },
        ),
        (
            "light",
            Options {
                extra_args: vec!["--light".into()],
                ..base()
            },
        ),
        (
            "theme-none",
            Options {
                syntax_theme: Some("none".into()),
                ..base()
            },
        ),
        (
            "raw",
            Options {
                extra_args: vec!["--raw".into()],
                ..base()
            },
        ),
    ]
}

/// The frontend runs the `git diff --no-index` step itself for every render,
/// not only while merging, because everything decided before delta sees a patch
/// -- the context width, where each hunk begins and ends -- never reaches
/// delta's argv and cannot be read back out of the rendering.
///
/// That is only safe because delta's own two-file mode does the same thing:
/// `delta a b` is documented as `diff -u a b | delta`, and this is the
/// measurement behind it. It also fires if a delta upgrade changes how the
/// internal diff is invoked, which would be a silent divergence otherwise.
#[test]
fn patch_path_matches_two_file_mode() {
    let d = delta();
    for (name, opts) in all_modes() {
        let (l, r) = buffers();
        let direct = d.render(&l, &r, &opts).expect("render");
        let patch = d.diff(&l, &r, &opts).expect("diff");
        let piped = d.render_patch(&patch, &opts).expect("render patch");
        assert_eq!(
            anonymous_descriptors(&direct),
            anonymous_descriptors(&piped),
            "mode {name:?}: owning the diff step changed the rendering",
        );
    }
}

/// The number in `/dev/fd/7` is whatever the kernel handed that spawn, and two
/// spawns are two numbers. It reaches the output only in `--color-only`, which
/// passes Git's `diff --git a/… b/…` header through untouched -- so comparing
/// two renderings means comparing everything except it.
fn anonymous_descriptors(rendered: &[u8]) -> String {
    let text = String::from_utf8_lossy(rendered);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("/dev/fd/") {
        out.push_str(&rest[..at]);
        out.push_str("/dev/fd/N");
        rest = rest[at + "/dev/fd/".len()..].trim_start_matches(|c: char| c.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

/// The same, for the inputs that reach delta as real paths -- where delta infers
/// the syntax from the filename rather than from `--default-language`, and does
/// it from a `+++ b/…` header on the piped path instead of from argv.
#[test]
fn patch_path_matches_two_file_mode_for_files_too() {
    let d = delta();
    let dir = unique_test_dir("patch-path");
    std::fs::create_dir_all(&dir).unwrap();
    let (pl, pr) = (dir.join("l.rs"), dir.join("r.rs"));
    std::fs::write(&pl, LEFT).unwrap();
    std::fs::write(&pr, RIGHT).unwrap();
    let opts = Options {
        inherit_gitconfig: false,
        ..Options::default()
    };
    let direct = d
        .render(&Input::Path(pl.clone()), &Input::Path(pr.clone()), &opts)
        .expect("render");
    let patch = d
        .diff(&Input::Path(pl), &Input::Path(pr), &opts)
        .expect("diff");
    let piped = d.render_patch(&patch, &opts).expect("render patch");
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        direct.windows(5).any(|w| w == b"38;2;"),
        "the fixture stopped being syntax-highlighted, so this proves nothing",
    );
    assert_eq!(
        String::from_utf8_lossy(&direct),
        String::from_utf8_lossy(&piped),
        "owning the diff step changed the rendering of file-backed panels",
    );
}

/// The one thing every comparable tool offers that delta's two-file mode cannot:
/// `delta -w a b` is not an ignore, it is delta reading `-w` as `--width` and
/// then refusing the filename as a column count. It is reachable only because
/// the app runs the diff step itself.
#[test]
fn whitespace_can_be_ignored_because_the_diff_step_is_ours() {
    let d = delta();
    let left = Input::Buffer("fn main() {\n    let s = 1;\n}\n".into());
    let right = Input::Buffer("fn main() {\n\tlet   s   =   1;\n}\n".into());
    let exact = Options {
        inherit_gitconfig: false,
        default_language: Some("rs".into()),
        ..Options::default()
    };
    assert!(
        !d.diff(&left, &right, &exact).expect("diff").is_empty(),
        "the fixture has to differ for this to prove anything",
    );

    for mode in [Whitespace::Amount, Whitespace::All] {
        let opts = Options {
            whitespace: mode,
            ..exact.clone()
        };
        let patch = d.diff(&left, &right, &opts).expect("diff");
        assert!(patch.is_empty(), "{mode:?} did not reach git diff");
        // An empty patch is an empty diff and not a refusal: delta is handed
        // nothing and says nothing, which is what "no differences" is drawn from.
        assert!(
            d.render_patch(&patch, &opts).expect("render").is_empty(),
            "{mode:?}: an ignored-away difference became an error",
        );
    }
}

/// `-I` is the rule-based half of the same idea, and the one tools like Beyond
/// Compare build a whole grammar language on. Here it is one pattern.
#[test]
fn lines_matching_a_pattern_are_not_differences() {
    let d = delta();
    let left = Input::Buffer("build 4919\nreal change\n".into());
    let right = Input::Buffer("build 5820\nreal change\n".into());
    let opts = Options {
        inherit_gitconfig: false,
        ignore_matching: Some("^build ".into()),
        ..Options::default()
    };
    assert!(
        d.diff(&left, &right, &opts).expect("diff").is_empty(),
        "the only changed line matched the pattern",
    );

    let left = Input::Buffer("build 4919\nreal change\n".into());
    let right = Input::Buffer("build 5820\nreal change, edited\n".into());
    assert!(
        !d.diff(&left, &right, &opts).expect("diff").is_empty(),
        "a change that is not all pattern is still a difference",
    );
}

/// The text delta picked out *inside* each changed line: the spans whose
/// background is not the one the rest of that line carries, which is the same
/// `ESC[K` colour the erase fill uses.
fn emphasized(rendered: &[u8]) -> Vec<String> {
    let lines = ansi::parse(rendered);
    ansi::body(&lines)
        .iter()
        .filter_map(|line| {
            // Only a removed or added line has one; context lines are unpainted.
            let base = line.fill_to_eol?;
            let text: String = line
                .spans
                .iter()
                .filter(|span| span.style.bg.is_some_and(|bg| bg != base))
                .map(|span| span.text.as_str())
                .collect();
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

/// What the granularity control actually does, measured rather than assumed.
///
/// The two flags behind it are delta's, and neither is validated against
/// anything: a renamed or dropped option would leave the control silently doing
/// nothing, which is precisely the failure this file exists to catch.
#[test]
fn granularity_narrows_what_is_emphasized() {
    let d = delta();
    let left = Input::Buffer("fn main() {\n    let greeting = \"hi\";\n}\n".into());
    let right = Input::Buffer("fn main() {\n    let greeting = \"hello\";\n}\n".into());
    let render = |granularity| {
        let opts = Options {
            inherit_gitconfig: false,
            default_language: Some("rs".into()),
            granularity,
            ..Options::default()
        };
        emphasized(&d.render(&left, &right, &opts).expect("render"))
    };

    // `hi` becoming `hello` shares an `h`, and only the finest setting says so.
    assert_eq!(render(Granularity::Character), ["i", "ello"]);
    assert_eq!(render(Granularity::Word), ["hi", "hello"]);
    assert!(
        render(Granularity::Line).is_empty(),
        "whole-line colouring picks nothing out inside the line",
    );
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
        .render(
            &l,
            &r,
            &Options {
                inherit_gitconfig: false,
                default_language: Some("rs".into()),
                ..Options::default()
            },
        )
        .expect("render");
    let has_rgb = ansi::parse(&out)
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|s| {
            matches!(s.style.fg, Some(Color::Rgb(..))) || matches!(s.style.bg, Some(Color::Rgb(..)))
        });
    assert!(
        has_rgb,
        "expected 24-bit colour; delta fell back to the 256-colour palette"
    );
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
    let opts = Options {
        inherit_gitconfig: false,
        ..Options::default()
    };
    let out = d.render(&l, &r, &opts).expect("render");
    let filled = ansi::parse(&out)
        .iter()
        .filter(|l| l.fill_to_eol.is_some())
        .count();
    assert!(
        filled > 0,
        "expected some lines to carry an EL background fill"
    );
}

/// A pasted buffer and the same bytes on disk must render identically, since
/// file-vs-paste is the headline use case.
#[test]
fn buffer_and_path_inputs_agree() {
    let d = delta();
    let dir = std::env::temp_dir().join(format!("delgui-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (pl, pr) = (dir.join("l.rs"), dir.join("r.rs"));
    std::fs::write(&pl, LEFT).unwrap();
    std::fs::write(&pr, RIGHT).unwrap();

    let opts = Options {
        inherit_gitconfig: false,
        default_language: Some("rs".into()),
        ..Options::default()
    };
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

/// The app's default mode is side-by-side, where delta turns line numbers on for
/// itself and offers no flag to say no -- so the toolbar checkbox was dead there
/// until `to_args` learned to empty both column formats instead.
#[test]
fn line_numbers_can_be_switched_off_in_side_by_side() {
    let d = delta();
    let (l, r) = buffers();
    let base = Options {
        inherit_gitconfig: false,
        side_by_side: true,
        default_language: Some("rs".into()),
        ..Options::default()
    };
    // Counted as "a digit sits in the gutter before the code", which ignores the
    // hunk header -- that is a line number too, and it stays either way.
    let numbered = |on: bool| {
        let out = d
            .render(
                &l,
                &r,
                &Options {
                    line_numbers: on,
                    ..base.clone()
                },
            )
            .expect("render");
        strip_ansi(&String::from_utf8_lossy(&out))
            .lines()
            .filter_map(|line| line.find("fn main").map(|at| line[..at].to_string()))
            .filter(|gutter| gutter.contains(|c: char| c.is_ascii_digit()))
            .count()
    };
    assert!(
        numbered(true) > 0,
        "side-by-side with line numbers should show some"
    );
    assert_eq!(
        numbered(false),
        0,
        "unticking line numbers left them on screen"
    );
}

/// delta decides its plus and minus backgrounds from a terminal query that a GUI
/// cannot answer, so the app states the mode outright. If this stopped working
/// the diff would silently keep dark backgrounds under a light window.
#[test]
fn appearance_picks_delta_s_colour_scheme() {
    let d = delta();
    let (l, r) = buffers();
    let base = Options {
        inherit_gitconfig: false,
        default_language: Some("rs".into()),
        ..Options::default()
    };
    let backgrounds = |mode| {
        let out = d
            .render(
                &l,
                &r,
                &Options {
                    appearance: Some(mode),
                    ..base.clone()
                },
            )
            .expect("render");
        ansi::parse(&out)
            .iter()
            .filter_map(|line| line.fill_to_eol)
            .collect::<Vec<_>>()
    };
    let dark = backgrounds(Appearance::Dark);
    let light = backgrounds(Appearance::Light);
    assert!(
        !dark.is_empty() && !light.is_empty(),
        "expected filled lines in both modes"
    );
    assert_ne!(
        dark, light,
        "--dark and --light produced the same diff backgrounds"
    );
    // The documented defaults, so a delta change here is visible rather than
    // merely different.
    assert!(
        dark.contains(&Color::Rgb(0x00, 0x28, 0x00)),
        "dark plus background: {dark:?}"
    );
    assert!(
        light.contains(&Color::Rgb(0xd0, 0xff, 0xd0)),
        "light plus background: {light:?}"
    );
}

/// A rejected flag exits 2 with empty stdout, which is byte-for-byte what two
/// identical inputs produce. Reading the status is the only way the GUI can tell
/// "nothing changed" from "delta refused".
#[test]
fn a_rejected_invocation_is_an_error_not_an_empty_diff() {
    let d = delta();
    let (l, r) = buffers();
    let opts = Options {
        inherit_gitconfig: false,
        extra_args: vec!["--not-a-delta-flag".into()],
        ..Options::default()
    };
    match d.render(&l, &r, &opts) {
        Err(DeltaError::Refused { code, message }) => {
            assert_eq!(code, Some(2));
            assert!(!message.is_empty(), "delta's complaint was dropped");
            assert!(
                !message.contains('\u{1b}'),
                "escape codes reached the message: {message:?}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A child may write a plausible prefix before discovering a fatal error. The
/// status remains authoritative: partial ANSI must never be presented as a
/// successful diff.
#[test]
fn partial_stdout_does_not_hide_a_delta_failure() {
    let dir = unique_test_dir("partial-output");
    std::fs::create_dir_all(&dir).unwrap();
    let fake_delta = dir.join("delta");
    write_executable(
        &fake_delta,
        "#!/bin/sh\nprintf 'partial rendered output\\n'\nprintf 'synthetic failure\\n' >&2\nexit 2\n",
    );
    let d = Delta {
        path: fake_delta,
        version: (0, 19, 2),
        version_string: "delta 0.19.2".into(),
    };
    let opts = Options {
        inherit_gitconfig: false,
        ..Options::default()
    };

    match d.render_patch(b"synthetic patch", &opts) {
        Err(DeltaError::Refused { code, message }) => {
            assert_eq!(code, Some(2));
            assert_eq!(message, "synthetic failure");
        }
        other => panic!("expected a refusal despite partial stdout, got {other:?}"),
    }
    std::fs::remove_dir_all(dir).ok();
}

/// The GUI owns the Git invocation. Neither the legacy external-diff variable
/// nor Git's environment-only config layer may replace it with a helper, even
/// when normal gitconfig inheritance is enabled.
#[test]
fn hostile_external_diff_environment_is_ignored() {
    const CHILD: &str = "DELGUI_EXTERNAL_DIFF_CHILD";
    const SENTINEL: &str = "DELGUI_EXTERNAL_DIFF_SENTINEL";
    if std::env::var_os(CHILD).is_some() {
        let sentinel = std::path::PathBuf::from(std::env::var_os(SENTINEL).unwrap());
        let d = delta();
        for inherit_gitconfig in [false, true] {
            let (left, right) = buffers();
            d.render(
                &left,
                &right,
                &Options {
                    inherit_gitconfig,
                    default_language: Some("rs".into()),
                    ..Options::default()
                },
            )
            .expect("owned diff should ignore external helpers");
            assert!(
                !sentinel.exists(),
                "an external diff helper ran with inherit_gitconfig={inherit_gitconfig}"
            );
        }
        return;
    }

    let dir = unique_test_dir("external-diff");
    std::fs::create_dir_all(&dir).unwrap();
    let helper = dir.join("hostile-diff");
    let sentinel = dir.join("helper-ran");
    write_executable(
        &helper,
        "#!/bin/sh\nprintf invoked > \"$DELGUI_EXTERNAL_DIFF_SENTINEL\"\nprintf 'not a unified diff\\n'\n",
    );

    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "hostile_external_diff_environment_is_ignored"])
        .env(CHILD, "1")
        .env(SENTINEL, &sentinel)
        .env("GIT_EXTERNAL_DIFF", &helper)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "diff.external")
        .env("GIT_CONFIG_VALUE_0", &helper)
        .status()
        .expect("spawn isolated test process");
    assert!(status.success(), "isolated helper-injection check failed");
    assert!(!sentinel.exists(), "external helper left its sentinel");
    std::fs::remove_dir_all(dir).ok();
}

/// Identical inputs are delta's way of saying "no difference": empty stdout and
/// exit 0. The GUI has to tell that apart from a failure, so it is pinned here.
#[test]
fn identical_inputs_render_nothing_and_succeed() {
    let d = delta();
    let same = || Input::Buffer(LEFT.as_bytes().to_vec());
    let opts = Options {
        inherit_gitconfig: false,
        ..Options::default()
    };
    assert!(
        d.render(&same(), &same(), &opts)
            .expect("render")
            .is_empty()
    );
}

/// Buffers reach delta as `/dev/fd/N`, and the read ends used to be leaked so
/// they would survive into the child. They do not need to: the child has its own
/// copy of the descriptor table once `spawn` returns. The leak cost two
/// descriptors per render and the GUI renders per keystroke, so a session ran
/// out of descriptors -- measurably, after about 123 of them -- and then stayed
/// broken. This is the guard.
#[test]
fn rendering_repeatedly_does_not_leak_descriptors() {
    let d = delta();
    let opts = Options {
        inherit_gitconfig: false,
        ..Options::default()
    };
    let render_once = || {
        let (l, r) = buffers();
        d.render(&l, &r, &opts).expect("render");
    };
    // Warm up first: the first calls settle whatever the runtime opens lazily.
    for _ in 0..5 {
        render_once();
    }
    let before = open_descriptors();
    for _ in 0..60 {
        render_once();
    }
    let after = open_descriptors();
    assert!(
        after <= before + 4,
        "60 renders went from {before} open descriptors to {after}; the pipes are leaking again"
    );
}

/// `/dev/fd/N` is resolved *by name*, late, by delta and by the `git diff` it
/// shells out to. Closing the descriptor as soon as `spawn` returns therefore
/// frees the number while a child is still going to look it up -- and a second
/// render on another thread gets handed that number, so the first one fails with
/// "could not access /dev/fd/3". Measured, not theorised: the suite failed only
/// when run in parallel.
#[test]
fn concurrent_renders_do_not_race_over_dev_fd_numbers() {
    let d = delta();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let d = &d;
                scope.spawn(move || {
                    for _ in 0..8 {
                        let (l, r) = buffers();
                        let opts = Options {
                            inherit_gitconfig: false,
                            ..Options::default()
                        };
                        d.render(&l, &r, &opts).expect("concurrent render");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker");
        }
    });
}

/// A render whose panels have changed is abandoned rather than waited out: the
/// GUI raises the `Cancel` and starts the render it actually wants. Against the
/// real pipeline, a dense pair that takes delta most of a second has to come
/// back `Cancelled` within a poll or two of being asked, whichever of its two
/// steps was running, and leave no descriptor behind -- the pumps feeding
/// `git diff` included, which are mid-write when that child is killed.
#[test]
fn a_cancelled_render_stops_promptly_and_leaks_nothing() {
    use std::time::{Duration, Instant};
    let d = delta();
    let opts = Options {
        inherit_gitconfig: false,
        side_by_side: true,
        ..Options::default()
    };
    // Two thirds of half a megabyte of lines changed: most of a second of
    // delta on an Apple M4, against the 100 ms before it is cancelled.
    let left: String = (0..12_000)
        .map(|i| format!("    let value_{i} = render({i}, \"panel\");\n"))
        .collect();
    let right: String = left
        .lines()
        .enumerate()
        .map(|(i, l)| match i % 3 {
            0 => format!("{l}\n"),
            _ => format!("{}\n", l.replacen("let ", "let mut ", 1)),
        })
        .collect();
    let cancel_after = |wait: Duration| {
        let cancel = Cancel::default();
        let raise = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(wait);
            raise.cancel();
        });
        let started = Instant::now();
        let (l, r) = (
            Input::Buffer(left.clone().into_bytes()),
            Input::Buffer(right.clone().into_bytes()),
        );
        let result = d
            .diff_cancellable(&l, &r, &opts, &cancel)
            .and_then(|patch| d.render_patch_cancellable(&patch, &opts, &cancel));
        canceller.join().unwrap();
        assert!(
            matches!(result, Err(DeltaError::Cancelled)),
            "a render cancelled after {wait:?} was not reported as cancelled: {:?}",
            result.map(|out| out.len())
        );
        // Noticed within one 10 ms poll; the rest is slack for a loaded
        // machine, and still well short of letting delta finish.
        assert!(
            started.elapsed() < wait + Duration::from_millis(500),
            "cancelled after {wait:?}, returned after {:?}",
            started.elapsed()
        );
    };
    // Early, which usually lands in the diff step with its pumps still
    // writing, and later, which lands in delta.
    cancel_after(Duration::from_millis(5));
    cancel_after(Duration::from_millis(100));
    let before = open_descriptors();
    for _ in 0..4 {
        cancel_after(Duration::from_millis(5));
        cancel_after(Duration::from_millis(100));
    }
    let after = open_descriptors();
    assert!(
        after <= before + 4,
        "8 cancelled renders went from {before} open descriptors to {after}"
    );
}

/// How many descriptors this process holds. `/dev/fd` lists exactly that on
/// macOS and Linux both, and reading it is cheaper than parsing `lsof`.
fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
}

/// A file that disappears from under a panel makes Git exit 1 with nothing on
/// stdout -- which is byte-identical to "these inputs are the same". Without
/// reading the status the GUI shows a blank pane and no reason for it.
#[test]
fn a_vanished_file_is_an_error_not_an_empty_diff() {
    let d = delta();
    let opts = Options {
        inherit_gitconfig: false,
        ..Options::default()
    };
    let present = Input::Buffer(LEFT.as_bytes().to_vec());
    let missing = Input::Path("/nonexistent/delgui/gone.rs".into());
    match d.render(&present, &missing, &opts) {
        Err(DeltaError::GitRefused { message, .. }) => {
            assert!(!message.is_empty(), "delta's complaint was dropped")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn unique_test_dir(label: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("delgui-{label}-{}-{nonce}", std::process::id()))
}

fn write_executable(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn version_floor_is_enforced() {
    let d = delta();
    assert!(
        (d.version.0, d.version.1) >= delgui_core::delta::MINIMUM_VERSION,
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
