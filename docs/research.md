# Milestone 0 — spike findings

Measured against **delta 0.19.2**, git 2.55.0, macOS 26.5, on 2026-08-19.
Every claim here came from running the binary, not from reading its docs.
The `deltapanes-core` test suite encodes the load-bearing ones as regressions.

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
actually does. The parser is `crates/deltapanes-core/src/ansi.rs`, ~230 lines.
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
  glyphs at all** — an open item.
- `\ No newline at end of file` is emitted normally and needs no special casing.
- Wrapping is on by default; `--wrap-max-lines=0` truncates instead.
- `delta A B` is byte-identical to `git diff --no-index A B | delta`, and shells
  out to git (falling back to `diff`) to get there. Both must be reachable from
  the child process.
- delta exits 1 when the inputs differ, like `diff`. Not an error.

## 8. Prior art

No GUI frontend for delta exists. The library route is closed by design: delta's
output is hardcoded to ANSI, per [issue #317](https://github.com/dandavison/delta/issues/317)
and [discussion #2128](https://github.com/dandavison/delta/discussions/2128).
A subprocess is not a compromise, it is the only interface.

## Open items

- **CJK/wide-glyph fidelity.** Needs a bundled monospace font with CJK coverage,
  or side-by-side misaligns on non-Latin text.
- **The 16 palette colours.** delta uses indices 0–15 for structural elements and
  a terminal takes those from the user's colour scheme. `render::Palette` supplies
  a default dark set; there is no way to read the user's real terminal theme.
- **cwd policy.** When a panel is bound to a file inside a repo with its own
  `[delta]` config, should delta run there? Currently `working_dir` is unset.
