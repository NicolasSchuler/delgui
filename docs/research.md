# Milestone 0 — spike findings

Measured against **delta 0.19.2**, git 2.55.0, macOS 26.5, on 2026-08-19.
Every claim here came from running the binary, not from reading its docs.
The `delgui-core` test suite encodes the load-bearing ones as regressions.

## 1. delta's ANSI output is a closed, tiny grammar

Parsing delta's stdout across every rendering mode it offers — unified,
side-by-side, line numbers, `--hyperlinks`, `--navigate`, wrapped, truncated,
`--color-only`, `--light`, `--syntax-theme=none`, `--raw` — turns up exactly
three constructs:

| construct | meaning |
| --- | --- |
| `ESC[…m` (SGR) | 4-bit, 256-colour and 24-bit styling |
| `ESC[K` (EL) | extend the current background to the end of the line |
| `ESC]8;;URI ST` (OSC 8) | hyperlink, only under `--hyperlinks` |

There is **no cursor motion, no scroll region, no alternate screen**. Nothing
that needs a terminal's state machine.

**Consequence.** Of the three rendering strategies in the proposal, strategy 1
(parse to styled spans) wins on evidence rather than taste: embedding a terminal
emulator would buy fidelity for a grammar that contains none of the hard parts,
while giving up control of selection and search — the two things a diff reviewer
actually does. The parser is `crates/delgui-core/src/ansi.rs`, ~230 lines.
Supporting evidence: `egui_term` is v0.1.0 with ~2k downloads and untouched since
April 2025, so strategy 2 also has no maintained implementation on this stack.

`parse_reporting_unknown` returns every escape the parser did not recognise, and
`parser_understands_every_escape_delta_emits` asserts that list is empty in all
modes. A delta upgrade that adds a construct fails that test instead of silently
mis-rendering.

## 2. No temp files are needed

delta infers syntax highlighting **entirely from the filename it is given**, and
its two-file mode accepts `/dev/fd/N`. So panels are handed to delta over pipes,
which is what the shell already does for `delta <(pbpaste) file.rs`.

A finding that also affects the plain shell workflow: **delta infers the language
from the right-hand path only.**

| invocation | syntax colours |
| --- | --- |
| `delta <(pbpaste) file.rs` | 6 |
| `delta file.rs <(pbpaste)` | **1 — silently lost** |
| `delta <(pbpaste) <(pbpaste)` | 1 |
| `delta --default-language=rs <(pbpaste) <(pbpaste)` | 6 |

`--default-language` is a true fallback: it does *not* override a real
extension. That is the right semantics for a per-panel language selector, but it
also means a panel bound to a real file cannot have its language overridden this
way.

Because a pasted panel has no filename to infer from, `language::detect` sniffs
the content instead. It is biased towards returning `None`: pasting prose is a
normal thing to do, delta renders unhighlighted text fine, and prose mistaken
for code gets sprayed with meaningless colour. Requiring the user to type a
language for every paste is the failure mode this avoids.

`--file-style=omit` suppresses delta's own `dev/fd/63 ⟶ b.rs` header, since the
app draws its own panel headers.

## 3. `COLORTERM=truecolor` must be injected

| child environment | result |
| --- | --- |
| full shell environment | 24-bit colour |
| `TERM`/`COLORTERM` unset | **falls back to 256-colour** |
| `env -i` (what a GUI launched from Finder or a hotkey gets) | **falls back to 256-colour** |

A GUI process has no `COLORTERM`. Without injecting it, output differs from the
user's terminal in a way that is invisible until someone compares side by side —
directly defeating the "visually indistinguishable" success criterion.
`Options::apply_env` sets it; `twenty_four_bit_colour_survives_an_empty_environment`
guards it.

## 4. Config discovery

| source | honoured |
| --- | --- |
| `$HOME/.gitconfig` `[delta]` | yes, even outside a repo |
| repo-local `.git/config` `[delta]` | yes, when cwd is inside that repo |
| `DELTA_FEATURES` | yes |
| `GIT_CONFIG_GLOBAL` | **no** |
| `--no-gitconfig` | clean opt-out |

Two consequences. delta's output depends on the **working directory**, so
`Options::working_dir` is an explicit choice rather than an accident. And since
`GIT_CONFIG_GLOBAL` is ignored, the only way to sandbox delta for reproducible
tests is `--no-gitconfig` (or overriding `HOME`) — which is why every test in
`fidelity.rs` sets `inherit_gitconfig: false`.

## 5. Size limits are lower than they look

5% of lines changed, timed end to end:

| input | `git diff` | delta | ANSI out | lines |
| --- | --- | --- | --- | --- |
| 88 KB | 0.00 s | 0.04 s | 335 KB | 1.2 k |
| 2.3 MB | 0.02 s | **0.84 s** | 8.5 MB | 30 k |
| 19 MB | 0.16 s | **6.6 s** | 69 MB | 240 k |

delta is the bottleneck, not git, and it amplifies its input roughly sevenfold.
Since **every width change re-runs delta**, debouncing resize is a correctness
requirement at a few MB, not polish. `MAX_PANEL_BYTES` is set to 4 MB.

## 6. Flag drift is a smaller risk than assumed

delta releases: 0.14.0 (Aug 2022), 0.15.x (Dec 2022), 0.16.x (Jun 2023),
0.17.0 (Mar 2024), 0.18.x (Aug–Sep 2024), 0.19.x (Mar 2026). Roughly one or two
releases a year with an 18-month gap in the middle. A floor of 0.18 costs
nothing; `Delta::discover` enforces it and reports the version in the UI.

## 7. Assorted fidelity details

- delta expands tabs itself, so the renderer needs no tab-stop logic.
- CJK is padded by display width. A GUI font must therefore render CJK at exactly
  two cells or side-by-side columns will drift. **egui's default font has no CJK
  glyphs at all**, and §14 measures what the installed ones do instead.
- `\ No newline at end of file` is emitted normally and needs no special casing.
- Wrapping is on by default; `--wrap-max-lines=0` truncates instead.
- `delta A B` is byte-identical to `git diff --no-index A B | delta`, and shells
  out to git (falling back to `diff`) to get there. Both must be reachable from
  the child process.
- delta exits 1 when the inputs differ, like `diff`. Not an error — but see §12, because
  exit 1 also covers a file that is not there.

## 8. File watching watches directories, not files

Editors and formatters overwhelmingly save by writing a temporary file and
renaming it over the target. That swaps the inode, so a watch registered on the
file itself sees the first save and silently misses every one after it. The
watcher therefore registers each file's **parent directory** non-recursively and
filters events back down by name. `survives_an_atomic_rename_save` performs two
consecutive rename-saves and fails if the second goes unnoticed.

Two consequences worth knowing. macOS FSEvents delivers with latency and will
report writes from just *before* the watch was established, so the first poll
after registering has to be discarded or the app re-diffs spuriously on startup.
And a save is rarely a single event, while a file caught mid-write reads as
truncated -- so events are coalesced for 180 ms before re-reading.

## 9. Prior art

No GUI frontend for delta exists. The library route is closed by design: delta's
output is hardcoded to ANSI, per [issue #317](https://github.com/dandavison/delta/issues/317)
and [discussion #2128](https://github.com/dandavison/delta/discussions/2128).
A subprocess is not a compromise, it is the only interface.

## 10. delta has no negative flags, and side-by-side turns line numbers on for itself

Measured against 0.19.2:

| attempt | result |
| --- | --- |
| `--side-by-side` with no `--line-numbers` | line numbers appear anyway |
| `--line-numbers=false` | **exit 2**, `unexpected value 'false'` |
| `--no-line-numbers`, `--no-side-by-side` | **exit 2**, no such flag |
| `--line-numbers-left-format= --line-numbers-right-format=` | works — line numbers *and* the column separators disappear |

The same shape applies to everything a `[delta]` gitconfig can set: omitting a flag means "whatever
gitconfig says", not "off". `--features=` with an empty value **does** override `delta.features`
(verified against a fake `HOME`), which is why `to_args` emits it even when the list is empty --
otherwise unticking every feature box in the UI would silently leave them all on.

`line_numbers_can_be_switched_off_in_side_by_side` pins the working lever.

## 11. `--light`/`--dark` is a separate axis from `--syntax-theme`

| | `--dark` | `--light` |
| --- | --- | --- |
| minus background | `#3f0001` | `#ffe0e0` |
| plus background | `#002800` | `#d0ffd0` |
| default syntax theme | Monokai Extended | GitHub |

Two findings behind that table. **Choosing a light syntax theme flips the backgrounds on its own**
-- `--syntax-theme=GitHub` alone yields `#ffe0e0`/`#d0ffd0` -- so a GUI that offers the theme list
without a light mode can produce near-black text on near-black ground with one click. And
`--dark --syntax-theme=GitHub` is honoured as stated: the mode decides the ground, the theme decides
the code. A GUI has no terminal to be queried about, so it must say which one it is in.

Separately, parsing every SGR delta emits across `--dark`, `--light`, side-by-side, `--hyperlinks`
and `--navigate`: **delta's default output uses exactly one of the sixteen palette colours, index 4
(`SGR 34`)**, 238 occurrences, for the hunk-header rules, the side-by-side divider and the `╎`
markers. Everything else is truecolor or the 256-cube. The other fifteen are still reachable through
a gitconfig that names colours (`--minus-style "normal red"` emits `SGR 41`), so the palette keeps
all sixteen -- but index 4 is the one that has to agree with the app's accent.

`appearance_picks_delta_s_colour_scheme` pins the backgrounds.

## 12. Empty stdout means two different things

| invocation | exit | stdout |
| --- | --- | --- |
| identical inputs | 0 | **empty** |
| inputs differ | 1 | the diff |
| a path that does not exist | 1 | **empty**, stderr explains |
| an unreadable file | 128 | empty |
| an unknown flag | 2 | empty |

So "delta printed nothing" is *either* the answer or a failure, and the status alone does not
separate them either -- exit 1 covers both the normal case and a missing file. The rule that works
is **failing status *and* empty stdout**. Ignoring this rendered every delta failure as a blank pane
with no message, indistinguishable from "these two are the same".

## 13. `/dev/fd/N` is resolved by name, late

The read end of each pipe has to stay open in *our* process for as long as the child runs, not just
until `spawn` returns. delta shells out to `git diff --no-index`, and it is that grandchild that
opens the path -- milliseconds later. Closing the descriptor at `spawn` frees the *number*, a
concurrent render is handed it, and the first child fails with `could not access '/dev/fd/3'`. It
only reproduces under parallelism: the test suite passed single-threaded and failed with
`--test-threads` unset.

Leaking them instead is worse. Two descriptors per render, one render per keystroke: with
`RLIMIT_NOFILE` at 256 -- what `launchctl limit maxfiles` gives a Finder-launched GUI -- the app
broke permanently after **123 renders**, roughly two minutes of typing, and every later render
failed with "Too many open files". Both failure modes now have a test.

## 14. egui 0.36 font facts

egui 0.36 rasterises through `skrifa`/`harfrust`/`vello_cpu`, **not `ab_glyph`**: `.ttc`
collections and variable fonts both load, which matters because nearly every macOS system font ships
as a collection.

- The bundled families are `Ubuntu-Light` (proportional) and `Hack-Regular` (monospace), plus two
  emoji fonts. Between them they have **no `⏎`, `⌫`, `✕`, and no `→` in the proportional chain** --
  all four were on screen as tofu boxes, including in the primary button. `Hack` is not in the
  proportional fallback chain by default; adding it fixes the arrows and box-drawing.
- **There is no bold face in the binary**, and `RichText::strong()` resolves to a *colour*, not a
  weight. A weight hierarchy needs a font file. macOS's `/System/Library/Fonts/SFNS.ttf` is a
  variable font with `wght` 1-1000, so one file registered twice at different `FontTweak::coords`
  gives real weights.
- `Fonts::has_glyph` is unusable as a coverage check: it compares against the replacement face and
  so reports *every* glyph as missing whenever the chosen font also supplies `◻`. `glyph_width > 0`
  is the reliable signal.
- `glyph_width` is independent of `pixels_per_point`, so the column count derived from it moves only
  with the font size -- which is why a font-size slider has to go through the resize debounce.
- CJK, measured as `width('中') / width(' ')` at 13pt: Maple Mono NF CN **2.000**, Hiragino Sans GB
  1.661, STHeiti Light 1.661, AppleSDGothicNeo 1.437, no fallback at all 0 (drawn as a one-column
  box). delta pads CJK to two columns, so nothing but exactly 2.0 stays aligned.

## 15. Building a result: the structure has to come from the diff, not the render

Measured against delta 0.19.2 / git 2.55.0 on 2026-08-19, for the merge feature.

**Reading `@@` back out of delta's rendered output does not work.** It looks like it should:
`--hunk-header-style=raw` prints git's own header verbatim, and it is never wrapped or truncated
(intact at `--width=22`, in both wrap and truncate modes). Three things kill it:

| | |
| --- | --- |
| `--hunk-header-style` given twice | `error: … cannot be used multiple times`, exit 2, **empty stdout** — and `Options::to_args` already emits `=omit` whenever the *Hunk headers* checkbox is off, which is the default |
| `[delta] line-numbers-left-format = ""` | the gutter disappears, so a line of file content is drawn at column 0 and is indistinguishable from a header. Diffing two `.patch` files is enough |
| `[diff] context = 1` in gitconfig | changes what a hunk *is*, silently |
| `--hunk-label` | does **not** apply to the `raw` style — only to `line-number` |

**So merge mode runs the diff itself and pipes it to delta.** `delta A B` is byte-identical to
`git diff --no-index A B | delta` (md5, side-by-side + line numbers + `--file-style=omit`), which
§7 already recorded, so this changes what the app *knows*, not what it *draws*. In a unified diff
every body line carries a ` `/`+`/`-`/`\` prefix, so only a real header starts with `@@` — the
phantom-header problem cannot arise, and `--unified=0` is then ours to pass on the command line
rather than something to smuggle past gitconfig.

**Zero context is not a refinement, it is the feature.** Hunks per pair of examples:

| pair | `-U3` (git's default) | `-U1` | `-U0` |
| --- | --- | --- | --- |
| `examples/config_before.rs` → `config_after.rs` (13 lines, 4 changes) | **1** | 1 | **4** |
| a 233-line source file, 9 scattered changes | 7 | 9 | 9 |
| rendered rows for the first pair | 82 | 48 | 30 |

At the default, the app's own example pair is a single hunk: one button for the whole file. The
take unit is still git's — delgui never decides what a hunk is.

**What is still read out of the render** is only which rows each hunk occupies, and only as a
count: `--hunk-label=␟` marks exactly one row per hunk, at column 0, in every mode tested
(unified, side-by-side, with and without line numbers, `--width=24` wrapped and truncated, and
with the user's hunk headers switched off). `merge::locate` refuses to return anything if the
marks do not number exactly the hunks git reported, and `merge::verify` then checks that the
regions *between* hunks are identical on both sides — if that holds, every individual take is
correct by construction. `tests/merge.rs` pins all of it.

**delta prints a blank row between hunks**, which `locate` trims off the end of each span, since
merge mode draws its own separator. A *changed* empty line is not blank in that sense: it carries
the `ESC[K` fill that paints its background.

**`--no-gitconfig` does not sandbox the diff step by itself.** delgui therefore owns that step
for every render, states the algorithm/context/colour behavior explicitly, passes
`--no-ext-diff --no-textconv`, removes injected config and external-helper environment, and pipes
the resulting patch into delta. When `inherit_gitconfig` is off it also points global/system config
at `/dev/null`. A configured executable can no longer receive private panel snapshots.

**Cost.** 18 ms for a small pair, 27 ms for 4 000 lines with 109 hunks, plus about 10 ms for the
separate `git diff`. Two frames — which is why a take simply greys its controls until the next
render lands, instead of predicting where the remaining hunks moved to.

**egui, for the controls.** `Label` re-wraps a `LayoutJob` to `ui.available_width()` regardless of
`job.wrap.max_width`, so a diff laid out to exactly `--width` columns spills its last glyph onto a
line of its own the moment a scrollbar appears. `TextWrapMode::Extend` is required, and this was
already true of the single-label diff. `TextEditState`'s undoer is fed only while the field has
focus, at one-second granularity, so a burst of takes made with focus in the diff leaves its most
recent snapshot describing the text from *before all of them* — `clear_undoer` after every
programmatic write is what stops one ⌘Z throwing a merge away.

## 16. One render path, and what it buys

`delta a b` is documented as `diff -u a b | delta`, and it is: piping our own `git diff --no-index`
into `delta` produces **byte-identical** output for every mode the app offers, for buffers and for
real paths alike (`patch_path_matches_two_file_mode`). The one thing that differs is not the
rendering — under `--color-only` delta passes Git's `diff --git a/dev/fd/7 …` header straight
through, and two spawns get two descriptor numbers.

So the app runs the diff step itself for *every* render, not only while merging. What that buys is
everything decided before delta sees a patch, none of which reaches delta's argv or can be read back
out of the rendering:

- **Ignores.** `delta -w a b` is not an ignore. delta parses `-w` as `--width` and fails with
  *Invalid value for width: "a" is not an integer*. `git diff --no-index -w … | delta` suppresses a
  whitespace-only change exactly as expected. Same for `--ignore-blank-lines`,
  `--ignore-cr-at-eol` and `--ignore-matching-lines`.
- **Context width**, hence "changes only / some context / whole file".
- **Hunk ranges**, hence Previous/Next change outside merge mode.

What it costs is one process spawn, not a second diff — delta was already shelling out to Git.

**Two flags are not free to force everywhere.** `--diff-algorithm=myers --no-indent-heuristic` are
merge mode's, because a take is a pair of line ranges into a buffer. `diff.indentHeuristic` is on by
Git's default and delta's two-file mode honours it; forced off, an added function comes out split
mid-comment instead of whole:

```
  with the heuristic          without it
  }                           }
 +/*                           /*
 + * Brand new function       + * Brand new function
 + */                         + */
 +static void brand_new()     +static void brand_new()
```

`--no-ext-diff`, `--no-textconv` and `--no-color` are unconditional: they are safety and
parseability, not shape.

## 17. Marking every hunk is free, or nearly

`--hunk-label` is the only way to be told which row starts a hunk, and it applies to **whatever**
header style is in force — not only to `line-number`. Two cases, both measured:

- **Header off.** Ask for `--hunk-header-style=line-number --hunk-header-decoration-style=none`
  plus the label, then drop the marked rows: the result is byte-identical to
  `--hunk-header-style=omit`, in unified and side-by-side, at `-U0` and `-U3`
  (`dropping_the_marked_rows_restores_the_unmarked_rendering`).
- **Header on.** Keep delta's configured style, add the label, and take the mark off the front of
  the row. Everything is identical except the decoration rule, which delta sizes to the header
  text and so draws two characters wider: `─────┐` where it was `───┐`
  (`a_kept_marked_row_is_the_header_without_its_mark`).

Dropping a marker can expose a blank *context* line the marker was hiding, which `ansi::body_range`
would then trim — silently shifting every span by one. `merge::prepare_rows` re-trims and
recomputes, rather than assuming the body it was handed is still the body.

## 18. What `git mergetool` actually passes

Measured against real Git, with a stub in place of the app:

```
mergetool.<tool>.cmd = tool "$BASE" "$LOCAL" "$REMOTE" "$MERGED"

  BASE=f_BASE_98116.txt  LOCAL=f_LOCAL_98116.txt  REMOTE=f_REMOTE_98116.txt  MERGED=f.txt
    base line 2:   two
    local line 2:  MINE      (ours)
    remote line 2: SIDE      (theirs)
```

Three findings. The temp files **keep the original extension**, so delta still infers the syntax
from the filename and a merge does not arrive unhighlighted. `MERGED` is the working-tree file
itself, not a temp — so it collides with no panel's path and the app's "saving over an input"
refusal never fires on it. And with `trustExitCode = true`, exit 0 **stages** the file (`M  f.txt`)
while exit 1 leaves it conflicted (`UU f.txt`) — which is why the exit status is computed from
whether `MERGED` holds the merge *now*, not from whether a save ever happened.

## 19. Within-line granularity is two independent knobs

delta refines a removed/added line pair into emphasized spans, and **two separate options** decide
what comes out. `--max-line-distance` (default 0.6) decides whether a pair is close enough to be
refined at all; `--word-diff-regex` (default `\w+`) decides what a token is. Measured against
delta 0.19.2, on `"hi"` becoming `"hello"` (`granularity_narrows_what_is_emphasized`):

| flags | emphasized, removed side | added side |
| --- | --- | --- |
| defaults | `hi` | `hello` |
| `--word-diff-regex=.` | `i` | `ello` |
| `--max-line-distance=0` | — | — |

So character granularity is a regex change, not a distance change, and `--max-line-distance=0` is
how within-line highlighting is turned off entirely. Raising the distance to 1.0 changed nothing on
this fixture: it admits *more distant* pairs, it does not make an admitted pair finer. delta's own
`--help` suggests `--word-diff-regex="\S+" --max-line-distance=1.0` for the opposite end — closer
to `git --word-diff`.

The app exposes the pair as one control, because "how much of the line is highlighted" is one
question to a reader, and states only the flag that defines each choice. `--max-line-distance` is
deliberately left alone for the two regex choices: 0.6 against 0.8 is a tuning someone may have set
in their gitconfig on purpose, and it is not what the control is about. The corollary is that a
gitconfig `max-line-distance = 0` leaves the control nothing to size — the same way any other
inherited `[delta]` key wins, and only while gitconfig inheritance is on to say so.

Character granularity is the default. A one-character change — a `<` that became `<=`, one letter
in an identifier — is the one a reader is most likely to miss, and word granularity hides it inside
a marked word that looks like any other marked word.

## Open items

- **CJK/wide-glyph fidelity.** Measured in §14: no stock macOS CJK font is double-width, so no
  automatic fallback fixes this. `fonts::probe` now reports the ratio and the settings panel warns;
  the actual fix is a dual-width font such as Maple Mono NF CN or an Iosevka CJK build.
- **Per-line takes.** A difference is git's unit, and at zero context that is one contiguous
  change — which is fine until two edits land on the same line. The result panel is a text field,
  so the answer for now is to take the difference and then type; a finer unit would mean deciding
  what a hunk is, which is the line this app does not cross.
- **cwd policy.** When a panel is bound to a file inside a repo with its own
  `[delta]` config, should delta run there? Currently `working_dir` is unset. `--mergetool` makes
  this sharper rather than answering it: Git launches the tool at the repo root, so the process
  inherits the right directory by accident, and the panels are Git's temp files rather than the
  repo's.
- **Ignores are a reading aid only.** They are forced off while a result is being built (§16), so
  the one workflow where "ignore whitespace" would be most useful — merging two reformatted
  versions — is the one that cannot have it. Lifting that means teaching `merge::verify` what an
  ignored difference is, which is a different proposition from passing a flag.
