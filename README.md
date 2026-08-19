# deltapanes

A paste-first GUI frontend for [`delta`](https://github.com/dandavison/delta).

Two editable panels. Paste into both, press <kbd>⌘</kbd><kbd>⏎</kbd>, and get
delta's real output — same syntax highlighting, same word-level diff, same
side-by-side layout as in your terminal. Rendering is done by the actual `delta`
binary, so it honours your `[delta]` gitconfig section.

The gap it fills: delta assumes both sides already exist as paths. When they
live in a clipboard, a browser, or a log viewer, the workarounds are temp files,
process substitution, or a web diff tool you cannot paste confidential text into.

## Requirements

- `delta` ≥ 0.18 on `PATH` (`brew install git-delta`)
- `git` (delta shells out to `git diff --no-index`)

If delta is missing, deltapanes says so and exits. It does not substitute its own
renderer.

## Usage

```
deltapanes                    # two empty panels
deltapanes a.rs b.rs          # prefilled, compares immediately
deltapanes --paste file.rs    # clipboard into panel A, file into panel B
```

Nothing is written to disk: panel contents reach delta over pipes, the same way
`delta <(pbpaste) file.rs` does.

## Status

Milestone 1. Two panels, paste and file input, live flag toggles, delta-faithful
rendering. Not yet: file watching, N panels, packaging, OS hotkey.

See [`docs/research.md`](docs/research.md) for the measurements the design rests
on — including a gotcha that affects the plain shell workflow too: **delta infers
syntax from the right-hand path only**, so `delta file.rs <(pbpaste)` silently
loses highlighting where `delta <(pbpaste) file.rs` keeps it.

## Layout

| path | role |
| --- | --- |
| `crates/deltapanes-core` | delta invocation + ANSI→span parsing, no GUI deps |
| `crates/deltapanes` | egui frontend |
| `docs/research.md` | milestone 0 findings |

`deltapanes-core` is deliberately frontend-agnostic so the ratatui fallback
stays a real option rather than an aspiration.

## Tests

```
cargo test
```

The suite runs against your installed delta and asserts, among other things,
that the parser understands every escape sequence delta emits. A delta upgrade
that changes its output fails a test rather than mis-rendering silently.
