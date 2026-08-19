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
- `git` (deltapanes owns a hardened `git diff --no-index` step, then pipes the
  patch into delta for rendering)

If delta is missing, deltapanes says so and exits. It does not substitute its own
renderer.

## Usage

```
deltapanes                    # two empty panels
deltapanes a.rs b.rs          # prefilled, compares immediately
deltapanes --paste file.rs    # clipboard into panel A, file into panel B
deltapanes --watch a.rs b.rs  # follow both files as they change on disk
deltapanes --combine a.rs b.rs # start a result you can take differences into
deltapanes --hotkey           # register a system-wide paste hotkey
```

Files can also be dragged onto any panel — drop several at once and they fill
consecutive panels, each of which is outlined while you hover.

A panel opened from a file is still an editor. Type into it and it says *edited*,
and from then on what you see is what gets compared; the file is still there to
reload from or to discard your edits back to. Its ⋯ menu offers **follow changes
on disk**, which keeps up with atomic-rename saves — how most editors write — and
which stands aside while you have unsaved edits rather than overwriting them.

Nothing is written to disk unless you ask for it: panel contents reach delta over
pipes, the same way `delta <(pbpaste) file.rs` does, and the only thing that ever
writes a file is **Save** on a result you built. Preferences are remembered;
panel contents never are.

## Status

Milestone 7. Panels (two by default, up to six), paste and file input, building a
result out of the differences, drag-and-drop, file watching, automatic language detection, light and dark
themes that follow the system, font and size selection, gitconfig discovery,
keyboard operation and an optional system-wide hotkey. Not yet packaged — build
with `cargo build --release`.

Each panel shows the syntax delta will use for it as a small chip: the file's
extension, what the content was sniffed as, or `prose`. Click it to say
otherwise. Prose is detected as prose and left unhighlighted, which is what you
want when you are diffing paragraphs rather than code — the detector is
deliberately biased towards answering "don't know", because prose sprayed with
syntax colour is worse than prose left plain.

See [`docs/research.md`](docs/research.md) for the measurements the design rests
on — including a gotcha that affects the plain shell workflow too: **delta infers
syntax from the right-hand path only**, so `delta file.rs <(pbpaste)` silently
loses highlighting where `delta <(pbpaste) file.rs` keeps it.

## Panels

Two panels to start; `⌘N` adds more, up to six. delta is a two-way tool, so one
panel is the **baseline** — click its letter — and the rest are diffed against it,
one tab per pair. There is no N-way diff and there is not meant to be. The strip
under the panels always names the pair on screen, so the direction of the diff is
never something you have to infer from which side is red.

## Building a result

Two variants of a file, and you want some of this one and some of that one.
**Combine…**, under the diff, makes a **result** panel — seeded from any panel or
from nothing — and puts it in a band of its own below the diff.

The result is the baseline while you build it, so every diff on screen reads *my
result against a candidate*, and each difference carries one button that writes
the candidate's version into it. The diff is a patch from your result to that
candidate; taking a difference applies that hunk of it, which is why the green
side is always what the click gives you. Switch candidate with the tabs, take
from as many as you like, and type into the result for anything no panel
supplies. Then **Copy**, or **Save** it to a new file.

The result sits along the bottom by default, which costs the diff no width —
delta lays out against a column count, and side by side is the widest thing in
the window. Its ⋯ menu moves it to the **left** or **right** instead, which is
the better trade on a wide screen: at 1600 px the diff goes from 157 columns to
146, and the whole result is visible at once rather than a dozen lines of it.
Left puts it under the same half of the diff its own text appears in.

While you are building, the diff is computed with no context lines, so each
independent change is its own difference. It has to be: at git's default of three
lines of context, `examples/config_before.rs` and `config_after.rs` — thirteen
lines with four separate changes — come back as a *single* hunk, which is one
button for the whole file.

That diff is deltapanes' own — `git diff --no-index --unified=0`, piped into
delta rather than left for delta to run internally. The two are byte-identical (`delta A B` ≡
`git diff --no-index A B | delta`, measured), so the diff looks exactly the same;
what it buys is that the differences you take are read out of the diff rather than
out of its rendering, where a line of file content can look just like a hunk
header.

It is a two-way pick, not a three-way merge: there is no common ancestor and no
conflict detection, and there is not meant to be.

## Keys

| | |
| --- | --- |
| `⌘O` | open a file in the shown panel |
| `⌘W` | close the window, with an unsaved-content guard |
| `⌘⏎` | re-render now (re-reads files from disk first) |
| `⌘F` | find in the rendered diff |
| `⌘N` | add a panel |
| `⌘⇧W` | remove the shown panel |
| `⌘⇧V` | paste into a fresh panel |
| `⌘R` | make the shown panel the baseline |
| `⌘1`…`⌘6` | show that panel's diff |
| `⌘S` | save the result |
| `⌘⇧S` | save the result as a new file |
| `⌘Z` | undo the last take (only outside a text field) |
| `⌘⇧Z` | redo the last take (only outside a text field) |
| `⌘⌥S` `⌘L` `⌘\` | side by side · line numbers · wrap |
| `⌘,` | settings |
| `⌘/` | show this list |

Off macOS these are the same chords with `Ctrl`. Everything else lives in each
panel's ⋯ menu — open, reload, discard edits, follow the file, clear, remove.
Every operation that could replace unsaved panel text asks first, including the
remove-panel chord. (`⌘⌫` used to clear a panel, which is "delete to start of
line" in every macOS text field, and the panels *are* text fields.)

`--hotkey` registers `⌘⇧D` system-wide, which focuses the window and pastes the
clipboard into a fresh panel. It works only while deltapanes is running — an
application cannot arrange to be *launched* by a hotkey; that is a job for
launchd, Raycast, or a keyboard tool.

## Settings

**Appearance** follows your system light/dark setting by default, and either way
the whole window agrees with the diff inside it: the theme sets delta's own
`--light`/`--dark`, so its red and green backgrounds are the ones meant for that
mode, and the sixteen palette colours delta draws its hunk rules with come from
the same place as the app's accent. You can also pick the interface and diff
fonts from what is installed — deltapanes measures the one you choose and says so
if it is not really monospaced, since delta lays its output out in columns.

**delta** shows what your gitconfig already tells delta, which `[delta "name"]`
presets exist and lets you switch them on, and offers delta's syntax themes,
labelled light or dark with one click to match the window to them. Turning off
*use my [delta] gitconfig* passes `--no-gitconfig`, which is the only way to get
output independent of your gitconfig and working directory — `GIT_CONFIG_GLOBAL`
does not work, delta ignores it.

**Render pipeline** shows the owned `git diff … | delta …` shape and every flag
that affects the result. The `/dev/fd/N` operands name private in-memory panel
snapshots, so the display is explanatory rather than a directly runnable shell
command.

## Layout

| path | role |
| --- | --- |
| `crates/deltapanes-core` | delta invocation + ANSI→span parsing, no GUI deps |
| `crates/deltapanes` | egui frontend: state, theme, fonts, keymap, hotkey |
| `docs/research.md` | the measurements the design rests on |

`deltapanes-core` is deliberately frontend-agnostic so the ratatui fallback
stays a real option rather than an aspiration.

## Tests

```
cargo test
```

The suite runs against your installed delta and asserts, among other things,
that the parser understands every escape sequence delta emits. A delta upgrade
that changes its output fails a test rather than mis-rendering silently.
