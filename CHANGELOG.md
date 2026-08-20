# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-20

First release.

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

[Unreleased]: https://github.com/NicolasSchuler/delgui/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/NicolasSchuler/delgui/releases/tag/v0.1.0
