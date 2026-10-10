# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-10

The first tagged release, with prebuilt binaries for macOS and Linux.

### Added

- `--version`, which prints the version and exits, and `-h` listed in the usage
  text as the short form of `--help`.
- **Swap** (`⌘⇧R`) compares the same pair the other way round, so what was
  removed is now added.
- A *Highlight* setting for how much of a changed line is picked out: single
  characters (the new default, so a `<` that became `<=` is not missed), whole
  words (delta's own default), or nothing within the line.
- Keyboard: `⌘⇧Enter` takes the current difference into the result, `⌘⇧L`
  toggles wrapping (`⌘\` still works), and `⌘Q` quits through the same
  unsaved-content check as closing the window — including from the macOS
  application menu.
- Tooltips naming the keyboard chord on the Side by side, Numbers and Wrap
  toggles and throughout the settings drawer.
- A first-run tip and a clickable shortcuts button on the empty screen.
- The window title names the pair on screen and says when a result is unsaved.
- Accessibility: the diff is exposed to screen readers as text while only the
  visible part is drawn, it is a single tab stop that scrolls with the page
  keys, find results are announced, the toolbar's tab order follows its visual
  order, and animations are off.

### Changed

- Long diffs scroll without drawing every line: only the part near the viewport
  is drawn, and a render result that is already stale is no longer laid out at
  all.
- The diff is now laid out only for the rows near the viewport, so large diffs
  no longer freeze the window when they arrive and use far less memory — a
  pair of 1 MB files went from 0.9 s and 1.4 GB to under 10 ms and 7 MB. While
  building a result, take controls that are off screen are no longer drawn.
- An in-flight render is cancelled when its input changes, instead of running
  to completion only to be discarded.
- delta's timeout now scales with the size of the input, and a timed-out render
  suggests a narrower context setting.
- Files given on the command line compare immediately even when one or both are
  empty, up to 1 MB of combined input; a larger pair says it is waiting for
  Compare instead of sitting blank. Compare also retries a render that failed.
- The toolbar's view toggles no longer switch off gitconfig inheritance. They
  override only the setting they control, and announce the one case — side by
  side, or line numbers in the unified view, forced on by your gitconfig — that
  still requires it.
- The settings drawer aligns its fields, and stacks them at narrow widths; panel
  headers keep their actions on one row however long the file name.

### Fixed

- Saving: a plain save refuses to overwrite a result file that changed on disk
  since it was last written, and a result whose file was changed or deleted
  counts as unsaved when the window closes.
- Merge tool: saving a result that is still the unchanged ancestor asks for
  confirmation first; **Export copy…** writes elsewhere without telling Git the
  conflict is resolved; a Git input that failed to load cannot seed or save a
  result; and replacing Git's target file from outside withdraws the resolution.
- Following files on disk works through symlinks, including a link whose target
  changes or whose directory is recreated, and recovers a lost directory watch.
  A failed reload keeps the buffer and marks the diff out of date.
- Opening or dropping a file never replaces the result panel, and a drop that
  includes an oversized file is rejected as a whole.
- Find lands on the right line inside a result, and keeps navigating after the
  query is refined.
- Holding the take chord takes one difference, not several, and Enter shortcuts
  no longer also press a focused toolbar button.
- The Linux CI build, which had failed on an import used only on macOS.

## [0.1.0] - 2026-08-20

The first version, never tagged or published: it is the state of the
repository at the commit linked below.

### Added

- Two to six editable panels, any one of which is the baseline the rest are
  diffed against, one tab per pair.
- Rendering by the real `delta` binary over pipes — no temporary files, and the
  `[delta]` section of your gitconfig applies.
- Building a result: `Combine…` makes a result panel that becomes the baseline,
  and each difference carries a button that takes the candidate's version into
  it. Undo and redo, and Save or Copy when done.
- `--mergetool BASE LOCAL REMOTE MERGED` for `git mergetool`, with the exit
  status `trustExitCode` acts on.
- Difference navigation and a find bar over the rendered diff.
- Diff filters: context width, whitespace, blank lines, CRLF, and a
  lines-matching pattern — always announced beside the difference count.
- File watching that survives atomic-rename saves, drag-and-drop onto panels,
  and an optional system-wide paste hotkey.
- Automatic language detection, biased towards leaving prose unhighlighted.
- Light and dark themes that follow the system and are stated to delta, so the
  diff cannot disagree with the window around it.
- Font selection with a monospace probe, since delta lays out in columns.
- `docs/reference.md`, generated from the source and pinned by a test.

### Requirements

- `delta` 0.18 or newer and `git`, both on `PATH`.
- Rust 1.87 to build (`std::io::pipe`).
- macOS and Linux. Windows is not supported.

[Unreleased]: https://github.com/NicolasSchuler/delgui/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/NicolasSchuler/delgui/releases/tag/v0.2.0
[0.1.0]: https://github.com/NicolasSchuler/delgui/tree/d88516d6c9c3afa01e724f5761afff7296131e49
