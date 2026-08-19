# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`deltapanes` — a GUI frontend for [`delta`](https://github.com/dandavison/delta). Two or more
editable panels are handed to the **real delta binary** over pipes and its ANSI output is parsed
back into styled spans for egui. delta is a hard dependency, not a fallback: if it is missing the
app prints why and exits. Never add a substitute renderer.

## Commands

```sh
cargo build --release
cargo run -- examples/config_before.rs examples/config_after.rs   # try it by hand
cargo test                                                        # whole workspace
cargo test -p deltapanes-core --test fidelity                     # delta-output regressions
cargo test -p deltapanes-core --test merge                        # the take arithmetic
cargo test -p deltapanes-core --test fidelity parser_understands   # one test by name substring
cargo test -p deltapanes-core watch::tests                         # unit tests inside a module
cargo clippy --workspace --all-targets
```

The code is hand-formatted in a deliberately compact style with no `rustfmt.toml`, so
`cargo fmt --check` does not pass. Do **not** run `cargo fmt` across the tree — it would reflow
files unrelated to your change. Match the surrounding layout by hand instead.

Tests run against **your installed delta** (`brew install git-delta`), not a fixture. They will
fail without it, and a delta upgrade that changes its output is *meant* to fail
`parser_understands_every_escape_delta_emits` rather than mis-render silently. Watch tests sleep
on real filesystem events (~0.4–5 s each), so the suite is not instant.

## Architecture

```
crates/deltapanes-core   no GUI dependencies — kept frontend-agnostic so a ratatui
  delta.rs               frontend stays possible
  ansi.rs
  merge.rs               hunks out of a unified diff, and splicing one into a buffer
  config.rs
  language.rs
  watch.rs
crates/deltapanes        egui/eframe frontend
  app.rs                 all state + the eframe::App::ui loop
  render.rs              parsed lines -> egui LayoutJobs (one, or one per hunk)
  theme.rs               colour/spacing/type tokens -> egui Visuals and Style
  fonts.rs               font discovery, installation, and monospace probing
  ui.rs                  the styled controls app.rs is assembled from
  settings.rs            what survives a restart (~/Library/Application Support/deltapanes)
  keys.rs                the single keymap table
  hotkey.rs
docs/research.md         measured findings; every non-obvious decision below traces here
```

The render path is: `Panel` → `Input::Buffer`/`Input::Path` → `Delta::render` (subprocess) →
`ansi::parse` → `Vec<Line>` → `render::to_layout_job` → egui. Rendering happens on a spawned
thread; results come back over an `mpsc` channel as a `Job` and land in `App::cache`, keyed by
panel index.

### Constraints that look arbitrary but are not

Each of these is load-bearing and pinned by a test or documented in `docs/research.md`. Do not
"simplify" them away without reading that file.

- **No temp files, ever.** Buffers reach delta as `/dev/fd/N` pipes. `Input::as_argument` clears
  close-on-exec so the fd survives into the child; the write ends are pumped on threads because a
  pipe holds only ~64 KiB and sequential writes would deadlock. **The read ends are held until the
  child exits, then dropped** — both halves matter. Dropping them right after `spawn` frees the
  number while delta, and the `git diff` it shells out to, still have to resolve `/dev/fd/N` *by
  name*, so a concurrent render gets handed that number and the first one fails with "could not
  access /dev/fd/3"; `concurrent_renders_do_not_race_over_dev_fd_numbers` is that regression.
  Leaking them instead — which this used to do — costs two descriptors per render, and the GUI
  renders per keystroke, so a few minutes of typing hit the 256-descriptor ceiling a
  Finder-launched process gets and broke the session;
  `rendering_repeatedly_does_not_leak_descriptors` is that one.
- **delta infers syntax from the right-hand path only.** So `App::resolve_language` consults the
  *shown* panel before the reference, and pasted panels need `--default-language`. Note
  `--default-language` is a fallback: it cannot override a real file extension.
- **`COLORTERM=truecolor` is injected** in `Options::apply_env`. A GUI process has none, and
  without it delta silently drops to 256 colours — invisible until someone compares with their
  terminal.
- **Side-by-side forces line numbers on, and delta has no flag to say no.**
  `--line-numbers=false` exits 2 and `--no-line-numbers` does not exist, so `Options::to_args`
  empties `--line-numbers-left-format` and `--line-numbers-right-format` instead. Without that the
  toolbar's line-number toggle is dead in the app's own default mode, which is how it shipped.
- **The app's theme has to be stated to delta.** delta picks its plus/minus backgrounds by querying
  the terminal, which a GUI has none of, and it flips them for a light *syntax theme* too. So
  `Options::appearance` passes `--dark`/`--light` from `ctx.theme()`, `render::Palette` is built
  from the same `theme::Tokens`, and ANSI index 4 *is* the chrome accent — it is the only palette
  entry delta reaches for by default, and it draws every hunk rule and column divider.
- **`ESC[K` (`Line::fill_to_eol`) must be honoured.** It is how a diff background reaches the right
  edge; ignoring it gives ragged backgrounds, the most visible way to get delta wrong.
- **Width is a column count, not pixels.** delta lays out against `--width`, so the UI converts
  available width via glyph width and re-runs delta on change — debounced (`RESIZE_DEBOUNCE`),
  because delta takes ~0.8 s at 2 MB and amplifies input ~7x.
- **`--no-gitconfig` is the only sandbox.** delta ignores `GIT_CONFIG_GLOBAL`, so every test sets
  `inherit_gitconfig: false`. delta's output also depends on the working directory, which is why
  `Options::working_dir` is explicit.
- **Watching registers parent directories, not files.** Editors save by rename, which swaps the
  inode; a file-level watch sees exactly one save. Events are coalesced (`WATCH_DEBOUNCE`) and the
  first poll after registering is discarded because FSEvents replays writes from just before.
- **Rendering is single-flight and debounced.** `App::schedule` starts a render only when none is
  running, and drops the result of one whose `RenderKey` no longer matches. Six call sites used to
  spawn freely behind one `bool`, so holding ⌘⏎ started twenty-five delta processes a second and
  the visible diff could go backwards in time. Typing waits `EDIT_DEBOUNCE`, resizing waits
  `RESIZE_DEBOUNCE`, and a pair over `AUTO_RENDER_BYTES` waits to be asked.
- **A failed render must be attributable.** `Job::Failed` carries the `RenderKey` and `App::failed`
  remembers it. Without that, a failure caches nothing, so the key never matches, so the next frame
  re-spawns — measured at ~7,500 subprocesses a second, forever.
- **`RenderKey` holds a revision counter, not panel text.** It is rebuilt every repaint to decide
  whether to re-render; cloning multi-megabyte buffers 60x/second to answer "did anything change?"
  is the bug this shape prevents. Call `App::touch()` (or bump `revision`) whenever panel contents
  change, and clear `cache` when panel *indices* shift.
- **Fonts are checked, not trusted.** egui's bundled families have no `⏎`, `⌫`, `✕` or `→` and no
  bold face at all (`RichText::strong()` is a colour, not a weight), so `fonts.rs` puts the system
  UI font in front of them and registers a second instance at `wght` 560 for emphasis. Anything the
  user picks goes through `read_fonts` first — epaint *panics* on unparseable data, one pass later,
  from inside eframe's event loop — and then through `fonts::probe`, which measures glyph widths
  because `Fonts::has_glyph` false-negatives on any font that also supplies `◻`.
- **Merge mode reads its structure from the diff, not from the rendering.** delta's two-file mode
  shells out to `git diff --no-index`; while a result is being built, `Delta::diff` runs that step
  itself and `Delta::render_patch` pipes the same bytes back in to be drawn. The two are
  byte-identical (`research.md` §7, §15), so this changes what the app knows and not what it
  draws. Do not "simplify" it back to parsing `@@` out of delta's output: `--hunk-header-style`
  cannot be passed twice and `to_args` already emits it, a `[delta]` section that empties the
  line-number columns makes a line of file content indistinguishable from a header, and
  `[diff] context` silently redefines what a hunk is. In a unified diff every body line is
  prefixed, so only a real header starts with `@@`.
- **Merge mode diffs at zero context, and that is the feature.** At git's default of three,
  `examples/config_before.rs` vs `config_after.rs` — thirteen lines, four independent changes —
  is a *single* hunk, i.e. one button for the whole file. `merge::locate` also trims the blank row
  delta prints between hunks, because merge mode draws its own separator.
- **The only thing read out of the merge rendering is where each hunk was drawn**, and only as a
  count: `--hunk-label` marks one row per hunk. `locate` returns nothing at all if the marks do
  not number exactly the hunks git reported, and `verify` then checks that the regions between
  hunks are identical on both sides — if that holds, every individual take is correct by
  construction. A failure hides the controls and says so; it never splices anyway.
- **A result panel is a panel with `result` set, and merge mode is the baseline being one.** There
  is no second flag to get out of step. It is excluded from `Panel::is_empty` and from
  `drop_targets`, or a paste, a dropped file or the global hotkey would overwrite it — the last
  one from another application, out of sight. `Panel::clear` keeps the flag, because an emptied
  result is the "start from nothing" case. Its `dirty` is not `edited`: `edited` means "diverged
  from the file this panel was opened from", which is false for a result that was never saved, and
  a take must set *both* or `compare_now` re-reads a saved result from disk and undoes it.
- **Merge flags are forced in `effective_options`, never on `self.opts`.** `App::save` persists
  `opts` verbatim, so forcing there would rewrite the user's toolbar defaults permanently. The
  render key is built from `Options::fingerprint`, not `to_args`, because the diff's context width
  never reaches delta's argv and the cache would otherwise serve a stale render forever.
- **Two undo histories over one buffer is a data-loss path.** egui's `TextEditState` undoer is fed
  only while the field has focus, at one-second granularity, so five takes made with focus in the
  diff leave its most recent snapshot holding the text from before all five. Every programmatic
  write to a panel calls `forget_text_undo`; ⌘Z is bound but ignored while any text field has
  focus, so inside a panel it is still that field's own undo.
- **A take must not move the diff under the pointer.** `ScrollArea` keeps a pixel offset, and a
  take deletes rows above it. `take_hunk` records the taken hunk's box and puts the offset back,
  or the next difference slides out from under the cursor on every single take.
- **`Label` re-wraps a `LayoutJob` to the available width** regardless of `job.wrap.max_width`, so
  a diff laid out to exactly `--width` columns spills its last glyph onto its own line as soon as
  a scrollbar appears. `TextWrapMode::Extend` is not optional.
- **The language detector is biased towards `None`.** Prose is a normal thing to paste and delta
  renders it fine unhighlighted; prose sprayed with syntax colour is worse than plain. Ties resolve
  to "don't know". Adding needles to `RULES` makes false positives more likely — the prose tests
  are the guardrail.

### Adding a keybinding

Add one `Binding` to `keys::bindings()` — with a `group`, which is how the help overlay sections
itself — and handle its `Action` in `App::handle_keys`. Write the label through `keys::chord()`:
`⏎` exists in macOS's system font and in none of the fonts egui bundles, so a hardcoded one is a
tofu box on Linux. Tests enforce no duplicate chords, no bare keypresses (they would fire while
typing into a panel), that `ShowDiff` covers `MAX_PANELS`, and that nothing steals a standard
text-editing chord — ⌘⌫ used to clear a panel outright, which is "delete to start of line" in every
macOS text field, and the panels *are* text fields.

### Building a result

`Combine…` in the pair strip makes a result panel, which becomes the baseline and gets a whole edge
of the window — not a column in the panel row, where the buffer being authored would get a sixth of
a 260 px strip. `ResultPlacement` chooses which edge: bottom costs the diff no width, a side costs
about 11 columns of 157 at a 1600 px window and shows the whole result at once. Each placement has
its own panel id, since egui remembers a panel's size against its id and a height dragged at the
bottom is not a width at the side. Each difference gets a control row in place of delta's hunk header;
taking one splices the candidate's lines into the result and schedules a render, and the controls
are live exactly while `is_fresh()` — there is no second notion of "is what I see current".

Saving is the only thing in the app that writes a file. It refuses a path another panel is bound
to, reports a failed write the way `Panel::bind` reports a failed read, and does not bind the path
as `Panel::path` — that would switch on *Reload from disk*, *Discard my edits* and *Follow changes
on disk* in the result's own menu, three one-click ways to destroy the merge.

### Panels

Two by default, up to `MAX_PANELS` (6). delta is two-way: one panel is the **reference** and the
rest are diffed against it, one tab per pair. There is no N-way diff and there is not meant to be.

A panel is a **buffer that may be backed by a file**, and `Panel::edited` is the difference.
Untouched, it goes to delta as a path, so delta infers the syntax itself. Typing into it sets
`edited`, and from then on the *buffer* is what gets compared — the path stays only to supply
`--default-language` and to offer "reload"/"discard my edits". Without that flag, typing into a
file-backed panel was silently discarded: delta re-read the file while the panel showed something
else. Following a file on disk (`Panel::watch`) skips edited panels, so a save cannot destroy
typing, and only file-backed panels offer it — there is nothing for a paste to follow.

## Style

Comments explain *why*, especially where a line encodes a measured finding about delta or the
platform; the existing code is dense with these and matching that density is the house style.
Commit messages follow the same rule — a short imperative subject, then prose explaining the
reasoning and what a test now pins down.

## Open items

Listed at the bottom of `docs/research.md`: CJK glyph coverage (measured — no stock macOS CJK font
is double-width, so `fonts::probe` warns rather than fixes), search inside the diff (⌘F, still
unbuilt, and half of why the architecture parses ANSI instead of embedding a terminal), whether
delta should run in a file panel's repo directory, and per-line takes (a difference is git's unit,
which at zero context is one contiguous change).
