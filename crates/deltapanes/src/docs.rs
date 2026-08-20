//! `docs/reference.md`, built from the code it documents.
//!
//! The README went stale one commit after it was written, which is the ordinary
//! fate of a second copy of a fact. So the reference is not a second copy: every
//! chord, default, flag and limit in it is read out of the thing that defines
//! it, and the prose that cannot be -- what an option is *for* -- is guarded by
//! the compiler instead. Each `describe_*` below is an exhaustive `match` with
//! no wildcard arm, and the two `let … = Default::default()` destructurings name
//! every field, so adding an `Action`, an enum variant, a `Settings` field or an
//! `Options` field fails to compile until it has been documented.
//!
//! What is left is the file itself, which `tests::reference_is_current` pins.
//!
//! Nothing here may touch the environment. `Delta::discover` would bake the
//! local delta version into the document and `config::discover` the local
//! gitconfig, and then the golden file could only ever match on the machine that
//! wrote it. Defaults that genuinely differ by platform -- the interface font is
//! macOS's system font and egui's bundled stack everywhere else -- are described
//! rather than printed, and a chord is printed in both spellings.

use std::fmt::Write as _;

use deltapanes_core::delta::{Appearance, DeltaError, MINIMUM_VERSION, Options, PROCESS_LIMITS, Whitespace};

use crate::app;
use crate::keys::{self, Action};
use crate::settings::{Context, MONO_PT, ResultPlacement, Settings, ThemeChoice, UI_PT};

/// Where the generated file lives, relative to the workspace root.
pub const PATH: &str = "docs/reference.md";

/// What each command-line flag does. Keyed on `crate::FLAGS`, and
/// `tests::every_flag_is_documented` fails if one is added without a row.
fn describe_flag(flag: &str) -> Option<(&'static str, &'static str)> {
    Some(match flag {
        "--paste" => ("", "Load the clipboard into the first panel at startup. Nothing happens if the clipboard holds no text."),
        "--combine" => ("", "Start with a result panel, seeded from the first file, ready to take differences into."),
        "--watch" => ("", "Follow every file given on the command line, re-reading and re-diffing when it changes on disk."),
        "--hotkey" => ("", "Register a system-wide hotkey that focuses the window and pastes into a fresh panel. Failure to register is reported and the app continues without it."),
        "--mergetool" => ("BASE LOCAL REMOTE MERGED", "Resolve a merge conflict for Git. Exactly four paths, bound by position."),
        "--help" => ("", "Print usage to stdout and exit 0."),
        "-h" => ("", "Same as `--help`."),
        _ => return None,
    })
}

/// What each keyboard action does, beyond the four words the help overlay has
/// room for. Exhaustive on purpose: a new `Action` must be documented before it
/// compiles.
fn describe_action(action: Action) -> &'static str {
    match action {
        Action::OpenFile => "File picker for the panel whose diff is on screen.",
        Action::CloseWindow => "Close the window. Unsaved panel or result text is confirmed first.",
        Action::Compare => "Re-read any file-backed panels from disk and render now. This is also how a pair too large to render automatically gets rendered.",
        Action::Find => "Open the find bar over the rendered diff. A literal, case-sensitive substring scan; Esc closes it.",
        Action::NextMatch => "Move to the next find match, wrapping at the end.",
        Action::PreviousMatch => "Move to the previous find match, wrapping at the start.",
        Action::NextChange => "Scroll to the next difference. Works in every comparison, not only while building a result.",
        Action::PreviousChange => "Scroll to the previous difference.",
        Action::AddPanel => "Add an empty panel, up to the panel limit.",
        Action::RemovePanel => "Remove the panel whose diff is on screen, confirming if it holds unsaved text.",
        Action::PasteIntoNewPanel => "Add a panel and paste the clipboard into it.",
        Action::ShowDiff(_) => "Show that panel's diff against the baseline. One chord per panel, matching the panel limit.",
        Action::MakeReference => "Make the panel on screen the baseline every other panel is diffed against.",
        Action::ToggleSideBySide => "Toggle delta's two-column layout.",
        Action::ToggleLineNumbers => "Toggle line numbers.",
        Action::ToggleWrap => "Toggle wrapping of long lines.",
        Action::ToggleSettings => "Open or close the settings drawer.",
        Action::ToggleHelp => "Open or close the keyboard overlay.",
        Action::SaveResult => "Write the result to its file. With `--mergetool` this is what writes MERGED and answers Git.",
        Action::SaveResultAs => "Write the result to a new file, chosen in a dialog.",
        Action::UndoTake => "Undo the last take. Ignored while a text field has focus, where it is that field's own undo.",
        Action::RedoTake => "Redo the last undone take.",
    }
}

fn describe_context(context: Context) -> &'static str {
    match context {
        Context::Tight => "Only the changed lines, so each independent change is its own difference.",
        Context::Normal => "Git's own default.",
        Context::Whole => "The entire file with the changes marked. Large enough to cover any file the panel size limit admits.",
    }
}

fn describe_whitespace(whitespace: Whitespace) -> (&'static str, &'static str) {
    match whitespace {
        Whitespace::Exact => ("Exact", "Every space is a difference. Git's default."),
        Whitespace::Amount => ("Ignore amount", "`-b`. A line that gained indentation is still reported."),
        Whitespace::All => ("Ignore all", "`-w`. Whitespace anywhere, indentation included."),
    }
}

fn describe_theme(theme: ThemeChoice) -> &'static str {
    match theme {
        ThemeChoice::System => "Follow the OS, and keep following it when it changes.",
        ThemeChoice::Light => "Stay light whatever the OS is doing.",
        ThemeChoice::Dark => "Stay dark whatever the OS is doing.",
    }
}

fn describe_placement(placement: ResultPlacement) -> &'static str {
    match placement {
        ResultPlacement::Bottom => "A band under the diff. Costs the diff no width, which is the scarce dimension in a side-by-side render.",
        ResultPlacement::Left => "A full-height column on the left, under the same half of the diff the result's own text appears in.",
        ResultPlacement::Right => "A full-height column on the right. About eleven columns of diff at a 1600 px window, for the whole result at once.",
    }
}

fn describe_appearance(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => "`--dark`",
        Appearance::Light => "`--light`",
    }
}

/// Every way starting up can fail. Exhaustive so a new failure mode cannot be
/// added without saying what the user will see.
fn describe_error(error: &DeltaError) -> (&'static str, &'static str) {
    match error {
        DeltaError::NotFound => ("`delta` is not on PATH", "Fatal. deltapanes names the two install commands and exits; it does not substitute a renderer of its own."),
        DeltaError::TooOld { .. } => ("delta is older than the floor", "Fatal, naming the version found."),
        DeltaError::UnreadableVersion { .. } => ("`delta --version` said something unparseable", "Fatal."),
        DeltaError::Refused { .. } => ("delta refused the patch", "Reported in a banner; the previous diff stays on screen."),
        DeltaError::GitNotFound => ("`git` is not on PATH", "Fatal. Every render owns a `git diff --no-index` step, so Git is part of the pipeline, not an optional extra."),
        DeltaError::GitRefused { .. } => ("`git diff --no-index` refused", "Reported in a banner. Exit 1 *with* a patch means the inputs differ and is not an error."),
        DeltaError::TimedOut { .. } => ("a child outran the timeout", "The process group is killed and the failure is reported."),
        DeltaError::OutputTooLarge { .. } => ("a child outran the output cap", "Reported rather than buffered."),
        DeltaError::Io(_) => ("the child could not be spawned or read", "Reported."),
    }
}

fn bytes(n: usize) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{} MB", n / 1_000_000)
    } else if n.is_multiple_of(1024 * 1024) {
        format!("{} MiB", n / (1024 * 1024))
    } else {
        format!("{n} bytes")
    }
}

/// The whole document.
pub fn reference() -> String {
    let mut d = String::new();
    let s = Settings::default();
    // Named in full rather than with `..`: adding a persisted field must not
    // compile until it appears below.
    let Settings {
        theme,
        ui_font: _,
        ui_font_strong: _,
        mono_font: _,
        ui_pt,
        mono_pt,
        side_by_side,
        line_numbers,
        wrap,
        hunk_headers,
        context,
        whitespace,
        ignore_blank_lines,
        ignore_cr_at_eol,
        ref ignore_matching,
        ref syntax_theme,
        inherit_gitconfig,
        ref features,
        settings_open,
        result_placement,
    } = s;
    // Same contract for the argument builder.
    let o = Options::default();
    let Options {
        width,
        side_by_side: _,
        line_numbers: _,
        wrap: _,
        hunk_headers: _,
        marked_hunks: _,
        pin_hunk_structure: _,
        whitespace: _,
        ignore_blank_lines: _,
        ignore_cr_at_eol: _,
        ignore_matching: _,
        context: default_context,
        syntax_theme: _,
        appearance: _,
        default_language: _,
        features: _,
        inherit_gitconfig: _,
        working_dir: _,
        extra_args: _,
    } = o.clone();

    let onoff = |b: bool| if b { "on" } else { "off" };

    d.push_str("<!-- Generated by `crates/deltapanes/src/docs.rs`. Do not edit by hand. -->\n");
    d.push_str("<!-- Regenerate: DELTAPANES_BLESS=1 cargo test -p deltapanes docs:: -->\n\n");
    d.push_str("# deltapanes reference\n\n");
    d.push_str(
        "Every configuration option, keybinding and limit in deltapanes, generated from the\n\
         source that defines them. For why the design is the way it is, see\n\
         [`research.md`](research.md); for how to use the app, see the\n\
         [README](../README.md).\n\n",
    );

    // ---- Requirements -----------------------------------------------------
    let (major, minor) = MINIMUM_VERSION;
    let _ = write!(
        d,
        "## Requirements\n\n\
         | | |\n| --- | --- |\n\
         | `delta` | {major}.{minor} or newer, on `PATH` |\n\
         | `git` | any version, on `PATH` |\n\n\
         delta is a hard dependency. It is found by plain `PATH` lookup — there is no config \
         key, no environment override and no bundled copy — and `delta --version` is parsed at \
         startup. Git is required because deltapanes runs its own `git diff --no-index` step \
         before every render rather than letting delta shell out to one.\n\n\
         If either is missing, deltapanes prints to stderr, shows a dialog, and exits 1.\n\n"
    );
    d.push_str("| condition | what happens |\n| --- | --- |\n");
    // One of each variant, so the exhaustive match above is actually reached.
    for e in [
        DeltaError::NotFound,
        DeltaError::TooOld { found: String::new() },
        DeltaError::UnreadableVersion { output: String::new() },
        DeltaError::Refused { code: None, message: String::new() },
        DeltaError::GitNotFound,
        DeltaError::GitRefused { code: None, message: String::new() },
        DeltaError::TimedOut { program: "", after: PROCESS_LIMITS.timeout },
        DeltaError::OutputTooLarge { program: "", stream: "", limit: 0 },
    ] {
        let (what, then) = describe_error(&e);
        let _ = writeln!(d, "| {what} | {then} |");
    }
    d.push_str("\nThe `Io` case — the child could not be spawned or read — is reported the same way.\n\n");

    // ---- Command line -----------------------------------------------------
    d.push_str("## Command line\n\n```\ndeltapanes [OPTIONS] [FILE]...\n```\n\n");
    d.push_str("| flag | takes | what it does |\n| --- | --- | --- |\n");
    for flag in crate::FLAGS {
        let (takes, what) = describe_flag(flag).unwrap_or(("", "undocumented"));
        let takes = if takes.is_empty() { String::new() } else { format!("`{takes}`") };
        let _ = writeln!(d, "| `{flag}` | {takes} | {what} |");
    }
    let _ = write!(
        d,
        "\nEvery flag is a plain boolean: there is no `--flag=value` form and no `--` \
         separator. An unrecognised argument beginning with `-` is an error — usage goes to \
         stderr and the exit status is 2 — rather than being ignored, because a silently \
         dropped `--wach` starts an app that never watches.\n\n\
         Remaining arguments are file paths. They fill panels from the first empty one, adding \
         panels as needed up to the limit of {}. A file larger than {} is refused.\n\n",
        app::MAX_PANELS,
        bytes(app::MAX_PANEL_BYTES)
    );

    d.push_str("### As Git's diff and merge tool\n\n```sh\n");
    d.push_str("git config --global diff.tool deltapanes\ngit config --global difftool.deltapanes.cmd 'deltapanes \"$LOCAL\" \"$REMOTE\"'\ngit config --global merge.tool deltapanes\ngit config --global mergetool.deltapanes.cmd \\\n    'deltapanes --mergetool \"$BASE\" \"$LOCAL\" \"$REMOTE\" \"$MERGED\"'\ngit config --global mergetool.deltapanes.trustExitCode true\n```\n\n");
    d.push_str(
        "`--mergetool` takes exactly four paths and binds them **by position**, because an empty \
         ancestor is an ordinary both-sides-added conflict and must not be filled in by \
         \"the first empty panel\". BASE, LOCAL and REMOTE become panels; MERGED is the write \
         target and never a panel.\n\n\
         The result is seeded from the ancestor, not from Git's half-merged file — seeding from \
         that would mean diffing against its conflict markers — so each side arrives as \
         differences to take.\n\n\
         The exit status is the answer Git acts on under `trustExitCode`:\n\n\
         | status | meaning |\n| --- | --- |\n\
         | 0 | MERGED holds the merge. Git stages the file. |\n\
         | 1 | It does not. Git leaves the conflict. |\n\n\
         Whether MERGED currently holds the result is recomputed every frame, because a save \
         followed by more typing no longer does.\n\n",
    );

    // ---- Settings ---------------------------------------------------------
    d.push_str("## Settings\n\n");
    d.push_str("Opened with the settings chord. Everything here is remembered across restarts.\n\n");

    d.push_str("### Appearance\n\n| option | values | default | effect |\n| --- | --- | --- | --- |\n");
    let themes = ThemeChoice::ALL
        .iter()
        .map(|t| t.label())
        .collect::<Vec<_>>()
        .join(" · ");
    let _ = writeln!(
        d,
        "| Theme | {themes} | {} | Sets the window's appearance **and** delta's, so the diff \
         cannot disagree with the chrome around it. |",
        theme.label()
    );
    let _ = writeln!(
        d,
        "| Interface font | any installed family | the system UI font on macOS, egui's bundled \
         stack elsewhere | Chosen fonts are parsed before use, because epaint panics on \
         unparseable font data one frame later. |"
    );
    let _ = writeln!(
        d,
        "| Interface size | {}–{} pt | {ui_pt} pt | |",
        UI_PT.start(),
        UI_PT.end()
    );
    let _ = writeln!(
        d,
        "| Diff font | any installed family | delta's own monospace stack | Measured after \
         selection: a family that is not really monospaced is flagged, since delta lays its \
         output out in columns. |"
    );
    let _ = writeln!(
        d,
        "| Diff size | {}–{} pt | {mono_pt} pt | |\n",
        MONO_PT.start(),
        MONO_PT.end()
    );
    d.push_str("Sizes are clamped on load as well as in the UI, so a hand-edited settings file cannot produce a window with 2 pt text and no way back to the control that fixes it.\n\n");
    d.push_str("Each theme choice in full:\n\n| choice | delta is told | meaning |\n| --- | --- | --- |\n");
    for t in ThemeChoice::ALL {
        let flag = match t {
            // System resolves to one of the two at render time; there is no
            // third thing to pass, because delta has no "ask the terminal" flag
            // that a GUI could answer.
            ThemeChoice::System => "whichever the OS currently is",
            ThemeChoice::Light => describe_appearance(Appearance::Light),
            ThemeChoice::Dark => describe_appearance(Appearance::Dark),
        };
        let _ = writeln!(d, "| {} | {flag} | {} |", t.label(), describe_theme(t));
    }
    d.push_str(
        "\ndelta picks its plus and minus backgrounds by querying the terminal, which a GUI does \
         not have, so the app states the answer. This is a separate axis from the syntax theme, \
         and getting it wrong is how you end up with a light diff on dark chrome.\n\n",
    );

    d.push_str("### Syntax\n\n| option | values | default | maps to |\n| --- | --- | --- | --- |\n");
    let _ = writeln!(
        d,
        "| Theme | whatever `delta --list-syntax-themes` reports, each labelled light or dark | \
         {} | `--syntax-theme=NAME` |\n",
        match syntax_theme {
            Some(t) => t.clone(),
            None => "from your gitconfig, or delta's own default when inheritance is off".into(),
        }
    );
    d.push_str("A syntax theme's darkness is a separate axis from the window's. Picking one that disagrees offers a one-click switch rather than silently producing a light diff on dark chrome.\n\n");

    d.push_str("### Differences\n\nThese reach the `git diff` step and never delta's argv.\n\n");
    d.push_str("| option | values | default | maps to |\n| --- | --- | --- | --- |\n");
    let lines = |c: Context| match c {
        // A million is the argument, not the offer: it is "the whole file",
        // wide enough to cover anything the panel size limit admits.
        Context::Whole => "the file".to_string(),
        other => format!("{} lines", other.lines()),
    };
    let contexts = Context::ALL
        .iter()
        .map(|c| format!("{} ({})", c.label(), lines(*c)))
        .collect::<Vec<_>>()
        .join(" · ");
    let _ = writeln!(
        d,
        "| Show | {contexts} | {} | `--unified=N` |",
        context.label()
    );
    let ws = [Whitespace::Exact, Whitespace::Amount, Whitespace::All]
        .iter()
        .map(|w| describe_whitespace(*w).0)
        .collect::<Vec<_>>()
        .join(" · ");
    let _ = writeln!(
        d,
        "| Whitespace | {ws} | {} | nothing · `--ignore-space-change` · `--ignore-all-space` |",
        describe_whitespace(whitespace).0
    );
    let _ = writeln!(
        d,
        "| Ignore blank lines | on · off | {} | `--ignore-blank-lines` |",
        onoff(ignore_blank_lines)
    );
    let _ = writeln!(
        d,
        "| Ignore Windows line endings | on · off | {} | `--ignore-cr-at-eol` |",
        onoff(ignore_cr_at_eol)
    );
    let _ = writeln!(
        d,
        "| Ignore lines matching | a regular expression | {} | `--ignore-matching-lines=PATTERN`, as one argument so a leading dash stays part of the pattern |\n",
        if ignore_matching.is_empty() { "empty" } else { ignore_matching.as_str() }
    );
    d.push_str("Context widths in full:\n\n| choice | context | meaning |\n| --- | --- | --- |\n");
    for c in Context::ALL {
        let _ = writeln!(d, "| {} | {} | {} |", c.label(), lines(c), describe_context(c));
    }
    d.push_str("\nWhitespace settings in full:\n\n| choice | meaning |\n| --- | --- |\n");
    for w in [Whitespace::Exact, Whitespace::Amount, Whitespace::All] {
        let (label, meaning) = describe_whitespace(w);
        let _ = writeln!(d, "| {label} | {meaning} |");
    }
    d.push_str(
        "\nAn ignore is never invisible: what is being left out is printed beside the difference \
         count, and on the \"no differences\" screen — which would otherwise read as \"these files \
         are the same\" when it means \"the same apart from whitespace\".\n\n\
         All four are forced off while a result is being built. A take copies a difference's \
         lines exactly, and the check that makes a take correct requires the regions between \
         differences to be identical on both sides — which is precisely what an ignore makes \
         false. They are forced for that render only; the saved setting is untouched.\n\n",
    );

    d.push_str("### delta\n\n| option | values | default | maps to |\n| --- | --- | --- | --- |\n");
    let _ = writeln!(
        d,
        "| Hunk headers | on · off | {} | off is `--hunk-header-style=omit` |",
        onoff(hunk_headers)
    );
    let _ = writeln!(
        d,
        "| Use my `[delta]` gitconfig | on · off | {} | off is `--no-gitconfig`, plus environment scrubbing |",
        onoff(inherit_gitconfig)
    );
    let _ = writeln!(
        d,
        "| Feature presets | one checkbox per `[delta \"name\"]` section, plus anything named in `delta.features` | {} | `--features=\"a b c\"`, emitted even when empty so that unticking everything genuinely disables them |\n",
        match features {
            None => "seeded from your gitconfig on first run",
            Some(_) => "an explicit selection",
        }
    );
    d.push_str("Turning off gitconfig inheritance is the only way to get output that does not depend on your gitconfig or working directory: delta ignores `GIT_CONFIG_GLOBAL`. Flipping any of the three view toggles in the toolbar also turns it off, and says so — the toolbar cannot honestly claim to control a view that config is overriding.\n\n");

    // ---- Toolbar ----------------------------------------------------------
    d.push_str("## Toolbar\n\n| control | default | maps to |\n| --- | --- | --- |\n");
    let _ = writeln!(d, "| Side by side | {} | `--side-by-side` |", onoff(side_by_side));
    let _ = writeln!(
        d,
        "| Numbers | {} | `--line-numbers`. Off in side-by-side empties `--line-numbers-left-format` and `--line-numbers-right-format` instead, because delta has no `--no-line-numbers` and `--line-numbers=false` exits 2 |",
        onoff(line_numbers)
    );
    let _ = writeln!(d, "| Wrap | {} | off is `--wrap-max-lines=0` |", onoff(wrap));
    let _ = writeln!(
        d,
        "| Columns | {width} at startup | `--width=N`, recomputed from the window's pixel width and the glyph width, debounced by {} ms |\n",
        app::RESIZE_DEBOUNCE.as_millis()
    );
    d.push_str("Width is a column count, not a pixel measurement: delta lays its whole output out against it, so a resize is a re-render.\n\n");

    // ---- Not in the drawer -------------------------------------------------
    d.push_str("## Options outside the drawer\n\n");
    d.push_str("### Result placement\n\nIn the result band's ⋯ menu. Forced to Bottom below a 760 px window.\n\n| choice | meaning |\n| --- | --- |\n");
    for p in ResultPlacement::ALL {
        let _ = writeln!(d, "| {} | {} |", p.label(), describe_placement(p));
    }
    let _ = write!(
        d,
        "\nThe default is {}. Each placement has its own panel identity, since a height dragged \
         at the bottom is not a width at the side.\n\n",
        result_placement.label()
    );
    d.push_str(
        "### Per-panel\n\n\
         | control | where | notes |\n| --- | --- | --- |\n\
         | Syntax | the chip under each panel | The file's extension, a sniff of the content, or `prose`. Click to override; becomes `--default-language=NAME`. Note this is a *fallback* — it cannot override a real file extension, and delta infers syntax from the right-hand path only. |\n\
         | Follow changes on disk | the panel's ⋯ menu | File-backed panels only, and what `--watch` presets. Not remembered across restarts. |\n\n\
         Language detection is deliberately biased towards answering \"don't know\": prose is a \
         normal thing to paste, delta renders it fine unhighlighted, and prose sprayed with \
         syntax colour is worse than prose left plain.\n\n",
    );

    // ---- Always on ---------------------------------------------------------
    d.push_str("## What is always passed\n\n");
    let _ = write!(
        d,
        "With everything at its default, the pipeline is:\n\n```\ngit {} A B | delta {}\n```\n\n",
        o.git_diff_args().join(" "),
        o.to_args().join(" ")
    );
    d.push_str("| argument | why |\n| --- | --- |\n");
    d.push_str("| `--no-pager diff --no-index` | Two paths that are not in a repository. |\n");
    d.push_str("| `--no-color` | Colour would arrive inside the text about to be parsed. Git suppresses it off a terminal, but `color.diff = always` overrides that. |\n");
    d.push_str("| `--no-ext-diff` `--no-textconv` | Never run a configured helper against private panel content. |\n");
    d.push_str("| `--paging=never` | There is no pager. |\n");
    d.push_str("| `--file-style=omit` | There is one \"file\" in a two-buffer comparison and the app draws its own headers. |\n");
    let _ = writeln!(
        d,
        "| `--hunk-label=…` | One marked row per hunk, so the app can say where each difference was drawn. Set on every render, not only while merging, because Previous/Next change and the \"n of m\" counter need it too. The marks are taken back out before drawing. |"
    );
    let _ = write!(
        d,
        "| `--width={width}` | Replaced by the live column count. |\n\
         | `--unified={default_context}` | Replaced by the Show setting. |\n\n"
    );

    d.push_str("### Forced while building a result\n\nSet for that render only, never written to the saved settings.\n\n| forced | to | why |\n| --- | --- | --- |\n");
    d.push_str("| Context | 0 lines | At Git's default of three, a thirteen-line file with four independent changes comes back as a *single* hunk — one button for the whole file. |\n");
    d.push_str("| All four ignores | off | A take splices a difference's lines wholesale, and the correctness check requires the regions between differences to be identical on both sides. |\n");
    d.push_str("| Hunk headers | off | Merge mode draws its own control row where the header was. |\n");
    d.push_str("| Diff shape | `--diff-algorithm=myers --no-indent-heuristic` | A take is a pair of line ranges, so merge mode owns the structure it splices from. A plain render must *not* pin this: `diff.indentHeuristic` is on by default, it decides whether an added function arrives whole or split mid-comment, and delta's own two-file mode honours it. |\n\n");

    // ---- Persistence -------------------------------------------------------
    d.push_str("## What survives a restart\n\n");
    d.push_str("| platform | file |\n| --- | --- |\n");
    d.push_str("| macOS | `~/Library/Application Support/deltapanes/app.ron` |\n");
    d.push_str("| Linux | `~/.local/share/deltapanes/app.ron` |\n");
    d.push_str("| Windows | `%APPDATA%\\deltapanes\\data\\app.ron` |\n\n");
    d.push_str("RON, written on exit and on a 30-second timer. Unknown and missing fields are tolerated, so an older file keeps working.\n\n");
    d.push_str("Saved:\n\n");
    for field in [
        "theme", "ui_font", "ui_font_strong", "mono_font", "ui_pt", "mono_pt",
        "side_by_side", "line_numbers", "wrap", "hunk_headers", "context",
        "whitespace", "ignore_blank_lines", "ignore_cr_at_eol", "ignore_matching",
        "syntax_theme", "inherit_gitconfig", "features", "settings_open",
        "result_placement",
    ] {
        let _ = writeln!(d, "- `{field}`");
    }
    let _ = write!(
        d,
        "\nThe settings drawer being open is itself remembered: {}.\n\n",
        if settings_open { "it starts open" } else { "it starts closed" }
    );
    d.push_str(
        "**Not saved: panel contents, panel paths, which panel is the baseline, which pair is \
         shown, per-panel follow flags, the find query, the result, or the undo history.** This \
         is the point rather than an omission — the app exists partly so that text you would not \
         paste into a web diff can still be diffed, and writing that text to a settings directory \
         on a 30-second autosave timer would quietly undo it. Window geometry is remembered by \
         the windowing layer.\n\n\
         Two repairs happen on load, each announced: a saved syntax theme the installed delta no \
         longer lists is dropped, and a saved feature preset your gitconfig no longer defines is \
         disabled. A font that no longer parses falls back rather than crashing.\n\n",
    );

    // ---- Keys --------------------------------------------------------------
    d.push_str("## Keyboard\n\n");
    d.push_str("Chords are written the way each platform writes them.\n\n");
    for group in ["file", "compare", "panels", "result", "view"] {
        let _ = writeln!(d, "### {group}\n");
        d.push_str("| macOS | elsewhere | | |\n| --- | --- | --- | --- |\n");
        let mut seen: Vec<&str> = Vec::new();
        for b in keys::bindings().iter().filter(|b| b.group == group) {
            if seen.contains(&b.mac) {
                continue;
            }
            seen.push(b.mac);
            let _ = writeln!(
                d,
                "| `{}` | `{}` | {} | {} |",
                b.mac,
                b.other,
                b.describe,
                describe_action(b.action)
            );
        }
        d.push('\n');
    }
    d.push_str("### Mouse\n\n| | |\n| --- | --- |\n");
    for row in keys::help_rows().iter().filter(|r| r.group == "mouse") {
        let _ = writeln!(d, "| {} | {} |", row.label, row.describe);
    }
    d.push_str("| `Combine…` | build a result you can take differences into |\n");
    d.push_str("\nEsc closes the help overlay and the find bar. Every binding carries a modifier, so none can fire while you are typing into a panel — which is also why F7, what the IDEs bind to \"next difference\", is not available. No binding steals a standard text-editing chord; the two that look like exceptions, undo and redo, are ignored while a text field has focus, where they remain that field's own undo.\n\n");
    let _ = write!(
        d,
        "With `--hotkey`, `⌘⇧D` (`Super+Shift+D` off macOS) is registered system-wide and pastes \
         the clipboard into a fresh panel. It works only while deltapanes is already running: an \
         application cannot arrange to be *launched* by a hotkey.\n\n"
    );

    // ---- Environment -------------------------------------------------------
    d.push_str("## Environment\n\nApplied to both children of every render.\n\n");
    d.push_str("| variable | treatment |\n| --- | --- |\n");
    d.push_str("| `COLORTERM` | **Set to `truecolor`, always.** A GUI process inherits none, and without it delta silently drops from 24-bit to 256 colours — invisible until you compare with your terminal. |\n");
    d.push_str("| `TERM` | Set to `xterm-256color` only when unset. |\n");
    d.push_str("| `GIT_EXTERNAL_DIFF` | Removed, always. |\n");
    d.push_str("| `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*`, `GIT_CONFIG_VALUE_*` | Removed, always, so the launching environment cannot inject config into a diff of private content. |\n");
    d.push_str("| `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM` | Set to `/dev/null`, `/dev/null` and `1` only when gitconfig inheritance is off. delta ignores `GIT_CONFIG_GLOBAL`, but the diff step honours it. |\n");
    d.push_str("| `DELTA_FEATURES` | Read when discovering your configuration — a leading `+` appends to `delta.features`, otherwise it replaces. Removed from the children when inheritance is off. |\n\n");
    d.push_str("`DELTAPANES_BLESS` is read only by the test suite, to rewrite this file.\n\n");

    // ---- Limits ------------------------------------------------------------
    d.push_str("## Limits and timings\n\n| | | |\n| --- | --- | --- |\n");
    let _ = writeln!(d, "| Panels | {} | delta is two-way; more panels means more pairs against one baseline, and past a handful the columns are too narrow to read. |", app::MAX_PANELS);
    let _ = writeln!(d, "| Panel size | {} | Beyond this delta is the bottleneck: about 0.8 s at 2 MB, and it produces roughly seven times its input in ANSI. Refused rather than hung. |", bytes(app::MAX_PANEL_BYTES));
    let _ = writeln!(d, "| Auto-render ceiling | {} | Above this, editing stops re-rendering by itself and waits to be asked. |", bytes(app::AUTO_RENDER_BYTES));
    let _ = writeln!(d, "| Typing debounce | {} ms | |", app::EDIT_DEBOUNCE.as_millis());
    let _ = writeln!(d, "| Resize debounce | {} ms | |", app::RESIZE_DEBOUNCE.as_millis());
    let _ = writeln!(d, "| Watch debounce | {} ms | A save is rarely one filesystem event, and a file mid-write reads as truncated. |", app::WATCH_DEBOUNCE.as_millis());
    let _ = writeln!(d, "| Undo history | {} takes or {} | Snapshots of a buffer that may be megabytes, so it is bounded twice. |", app::UNDO_DEPTH, bytes(app::UNDO_BYTES));
    let _ = writeln!(d, "| Subprocess timeout | {} s | The child is killed as a process group. |", PROCESS_LIMITS.timeout.as_secs());
    let _ = writeln!(d, "| Subprocess stdout | {} | |", bytes(PROCESS_LIMITS.stdout_bytes));
    let _ = writeln!(d, "| Subprocess stderr | {} | |", bytes(PROCESS_LIMITS.stderr_bytes));
    d.push('\n');
    d.push_str("Rendering is single-flight: a render starts only when none is running, and the result of one whose inputs have since changed is dropped. Panel contents reach delta as `/dev/fd/N` pipes — no temporary file is ever written, and the only thing in the app that writes a file is an explicit Save.\n\n");

    // ---- Colophon ----------------------------------------------------------
    let _ = write!(
        d,
        "---\n\nGenerated from `crates/deltapanes/src/docs.rs`. Editing this file by hand will \
         fail `cargo test`.\n"
    );

    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn golden() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(PATH)
    }

    /// The generated file is committed, so a change to a default, a chord, a
    /// flag or a limit shows up as a diff in review rather than as documentation
    /// that quietly stopped being true.
    #[test]
    fn reference_is_current() {
        let generated = reference();
        let path = golden();
        if std::env::var_os("DELTAPANES_BLESS").is_some() {
            std::fs::write(&path, &generated).expect("could not write the reference");
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            committed, generated,
            "docs/reference.md is out of date -- regenerate it with \
             `DELTAPANES_BLESS=1 cargo test -p deltapanes docs::`"
        );
    }

    /// Adding a flag to the parser without a row here would document an app that
    /// has one more option than the one that ships.
    #[test]
    fn every_flag_is_documented() {
        let doc = reference();
        for flag in crate::FLAGS {
            assert!(
                describe_flag(flag).is_some(),
                "{flag} has no description in docs.rs"
            );
            assert!(doc.contains(flag), "{flag} is missing from the reference");
        }
    }

    /// Same contract for the keymap. The help overlay has its own version of
    /// this test; this one is about the document.
    #[test]
    fn every_binding_is_documented() {
        let doc = reference();
        for b in keys::bindings() {
            assert!(
                doc.contains(b.mac) && doc.contains(b.other),
                "{} is missing from the reference",
                b.describe
            );
            assert!(
                !describe_action(b.action).is_empty(),
                "{} has no description",
                b.describe
            );
        }
    }

    /// A chord table that resolved at compile time could only ever match the
    /// golden file on the platform that generated it.
    #[test]
    fn the_reference_is_the_same_on_every_platform() {
        let doc = reference();
        for b in keys::bindings() {
            assert!(
                doc.contains(b.mac),
                "{} lost its macOS spelling",
                b.describe
            );
            assert!(
                doc.contains(b.other),
                "{} lost its non-macOS spelling",
                b.describe
            );
        }
        // The font default is the one value that genuinely differs by platform.
        assert!(
            !doc.contains("SFNS"),
            "a platform-specific font path reached the reference"
        );
    }

    /// Every enum variant reaches its description, so the exhaustive matches are
    /// doing work rather than sitting unused.
    #[test]
    fn every_variant_reaches_the_document() {
        let doc = reference();
        for c in Context::ALL {
            assert!(doc.contains(describe_context(c)), "{:?} undocumented", c);
        }
        for p in ResultPlacement::ALL {
            assert!(doc.contains(describe_placement(p)), "{:?} undocumented", p);
        }
        for t in ThemeChoice::ALL {
            assert!(doc.contains(describe_theme(t)), "{:?} undocumented", t);
        }
        for w in [Whitespace::Exact, Whitespace::Amount, Whitespace::All] {
            assert!(doc.contains(describe_whitespace(w).1), "{:?} undocumented", w);
        }
        for a in [Appearance::Dark, Appearance::Light] {
            assert!(!describe_appearance(a).is_empty());
        }
    }
}
