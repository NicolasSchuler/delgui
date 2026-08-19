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
deltapanes --watch a.rs b.rs  # re-diff whenever either file changes on disk
deltapanes --hotkey           # register a system-wide paste hotkey
```

Files can also be dragged onto either panel. A panel bound to a file gets a
`watch` toggle; watching follows atomic-rename saves, which is how most editors
write, so it keeps working after the first save.

Nothing is written to disk: panel contents reach delta over pipes, the same way
`delta <(pbpaste) file.rs` does.

## Status

Milestone 5. Panels (two by default, up to six), paste and file input,
drag-and-drop, file watching, automatic language detection, flag and theme
controls, gitconfig discovery, keyboard operation and an optional system-wide
hotkey. Not yet packaged — build with `cargo build --release`.

The `lang:` box is an override, not a requirement. A file panel uses its own
extension; a pasted panel is sniffed. Prose is detected as prose and left
unhighlighted, which is what you want when you are diffing paragraphs rather
than code — the detector is deliberately biased towards answering "don't know",
because prose sprayed with syntax colour is worse than prose left plain.

See [`docs/research.md`](docs/research.md) for the measurements the design rests
on — including a gotcha that affects the plain shell workflow too: **delta infers
syntax from the right-hand path only**, so `delta file.rs <(pbpaste)` silently
loses highlighting where `delta <(pbpaste) file.rs` keeps it.

## Panels

Two panels to start; `⌘N` adds more, up to six. delta is a two-way tool, so with
more than two panels one is the **reference** (click its letter) and the rest are
diffed against it, one tab per pair. There is no N-way diff and there is not
meant to be.

## Keys

| | |
| --- | --- |
| `⌘⏎` | compare |
| `⌘N` | add a panel |
| `⌘⇧W` | remove the shown panel |
| `⌘⌫` | clear the focused panel |
| `⌘⇧V` | paste into a fresh panel |
| `⌘R` | make the shown panel the reference |
| `⌘1`…`⌘6` | show that panel's diff |
| `⌘S` `⌘L` `⌘\` | side-by-side · line numbers · wrap |
| `⌘/` | show this list |

`--hotkey` registers `⌘⇧D` system-wide, which focuses the window and pastes the
clipboard into a fresh panel. It works only while deltapanes is running — an
application cannot arrange to be *launched* by a hotkey; that is a job for
launchd, Raycast, or a keyboard tool.

## delta configuration

The **delta config** button shows what your gitconfig already tells delta,
which `[delta "name"]` presets exist and lets you switch them on, and offers
delta's syntax themes. Turning off *inherit gitconfig* passes `--no-gitconfig`,
which is the only way to get output independent of your gitconfig and working
directory — `GIT_CONFIG_GLOBAL` does not work, delta ignores it.

## Layout

| path | role |
| --- | --- |
| `crates/deltapanes-core` | delta invocation + ANSI→span parsing, no GUI deps |
| `crates/deltapanes` | egui frontend, keymap, hotkey |
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
