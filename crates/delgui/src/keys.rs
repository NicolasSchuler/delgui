//! The keyboard map, in one place so the handler and the help overlay agree.

use egui::{Key, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    OpenFile,
    CloseWindow,
    Quit,
    Compare,
    Find,
    NextMatch,
    PreviousMatch,
    NextChange,
    PreviousChange,
    AddPanel,
    RemovePanel,
    PasteIntoNewPanel,
    ShowDiff(usize),
    MakeReference,
    SwapSides,
    ToggleSideBySide,
    ToggleLineNumbers,
    ToggleWrap,
    ToggleSettings,
    ToggleHelp,
    SaveResult,
    SaveResultAs,
    TakeCurrentDifference,
    UndoTake,
    RedoTake,
}

pub struct Binding {
    /// Both spellings, not the picked one: `docs::reference` prints a table
    /// that has to come out the same on every platform, and `chord` resolves at
    /// compile time.
    pub mac: &'static str,
    pub other: &'static str,
    pub describe: &'static str,
    pub group: &'static str,
    pub action: Action,
    key: Key,
    mods: Modifiers,
}

impl Binding {
    /// The chord as this platform writes it.
    pub fn label(&self) -> &'static str {
        chord(self.mac, self.other)
    }
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::COMMAND
};
const CMD_ALT: Modifiers = Modifiers {
    alt: true,
    ..Modifiers::COMMAND
};

/// Chords are written the way the platform writes them.
///
/// It is not only convention: `⏎` is absent from the fonts egui bundles, so
/// relying on platform fallback can draw the primary shortcut as an empty box.
/// Writing the key name keeps the chord legible with the application's actual
/// font set on every platform.
const fn chord(mac: &'static str, elsewhere: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        elsewhere
    }
}

const COMPARE_CHORD: (&str, &str) = ("⌘Enter", "Ctrl+Enter");
const HELP_CHORD: (&str, &str) = ("⌘/", "Ctrl+/");
const SAVE_CHORD: (&str, &str) = ("⌘S", "Ctrl+S");
const TAKE_CHORD: (&str, &str) = ("⌘⇧Enter", "Ctrl+Shift+Enter");
const SWAP_CHORD: (&str, &str) = ("⌘⇧R", "Ctrl+Shift+R");
const PASTE_PANEL_CHORD: (&str, &str) = ("⌘⇧V", "Ctrl+Shift+V");

pub fn compare_label() -> &'static str {
    chord(COMPARE_CHORD.0, COMPARE_CHORD.1)
}

pub fn help_label() -> &'static str {
    chord(HELP_CHORD.0, HELP_CHORD.1)
}

pub fn paste_panel_label() -> &'static str {
    chord(PASTE_PANEL_CHORD.0, PASTE_PANEL_CHORD.1)
}

pub fn help_hint() -> &'static str {
    chord("Keyboard shortcuts  ⌘/", "Keyboard shortcuts  Ctrl+/")
}

/// ⌘S was the side-by-side toggle until the app could write a file. Once it can,
/// a user finishing a merge presses ⌘S and must not get a layout flip.
pub fn save_label() -> &'static str {
    chord(SAVE_CHORD.0, SAVE_CHORD.1)
}

pub fn take_label() -> &'static str {
    chord(TAKE_CHORD.0, TAKE_CHORD.1)
}

/// The pair strip's Swap button says its own chord, and the binding below is
/// built from the same pair: a hardcoded second spelling is how the two drift.
pub fn swap_label() -> &'static str {
    chord(SWAP_CHORD.0, SWAP_CHORD.1)
}

pub fn settings_hint() -> &'static str {
    chord("Settings  ⌘,", "Settings  Ctrl+,")
}

pub fn bindings() -> Vec<Binding> {
    let mut v = vec![
        Binding {
            mac: "⌘O",
            other: "Ctrl+O",
            describe: "open a file in the shown panel",
            group: "file",
            action: Action::OpenFile,
            key: Key::O,
            mods: CMD,
        },
        Binding {
            mac: "⌘W",
            other: "Ctrl+W",
            describe: "close the window",
            group: "file",
            action: Action::CloseWindow,
            key: Key::W,
            mods: CMD,
        },
        // Listed for every platform, live where there is no menu bar to own it
        // -- Linux and Windows. On macOS AppKit claims the chord for the
        // application menu's Quit item, which `crate::menu` repoints at the
        // window so that it lands on `close_requested` and the unsaved-work
        // guard, exactly as ⌘W does. Whichever path delivers it, quitting is
        // guarded; as shipped it was not, and took a panel of pasted text with
        // it without asking.
        Binding {
            mac: "⌘Q",
            other: "Ctrl+Q",
            describe: "quit",
            group: "file",
            action: Action::Quit,
            key: Key::Q,
            mods: CMD,
        },
        Binding {
            mac: COMPARE_CHORD.0,
            other: COMPARE_CHORD.1,
            describe: "re-render now",
            group: "compare",
            action: Action::Compare,
            key: Key::Enter,
            mods: CMD,
        },
        Binding {
            mac: "⌘F",
            other: "Ctrl+F",
            describe: "find in the diff",
            group: "compare",
            action: Action::Find,
            key: Key::F,
            mods: CMD,
        },
        // ⌘G/⌘⇧G are what every macOS app binds find-next to, and F7/⇧F7 -- what
        // the IDEs use for the next difference -- cannot be borrowed: a bare
        // keypress would fire while typing into a panel.
        Binding {
            mac: "⌘G",
            other: "Ctrl+G",
            describe: "next match",
            group: "compare",
            action: Action::NextMatch,
            key: Key::G,
            mods: CMD,
        },
        Binding {
            mac: "⌘⇧G",
            other: "Ctrl+Shift+G",
            describe: "previous match",
            group: "compare",
            action: Action::PreviousMatch,
            key: Key::G,
            mods: CMD_SHIFT,
        },
        Binding {
            mac: "⌘⌥↓",
            other: "Ctrl+Alt+Down",
            describe: "jump to the next difference",
            group: "compare",
            action: Action::NextChange,
            key: Key::ArrowDown,
            mods: CMD_ALT,
        },
        Binding {
            mac: "⌘⌥↑",
            other: "Ctrl+Alt+Up",
            describe: "jump to the previous difference",
            group: "compare",
            action: Action::PreviousChange,
            key: Key::ArrowUp,
            mods: CMD_ALT,
        },
        Binding {
            mac: "⌘R",
            other: "Ctrl+R",
            describe: "make the shown panel the baseline",
            group: "compare",
            action: Action::MakeReference,
            key: Key::R,
            mods: CMD,
        },
        Binding {
            mac: SWAP_CHORD.0,
            other: SWAP_CHORD.1,
            describe: "swap the two sides",
            group: "compare",
            action: Action::SwapSides,
            key: Key::R,
            mods: CMD_SHIFT,
        },
        Binding {
            mac: "⌘N",
            other: "Ctrl+N",
            describe: "add a panel",
            group: "panels",
            action: Action::AddPanel,
            key: Key::N,
            mods: CMD,
        },
        Binding {
            mac: "⌘⇧W",
            other: "Ctrl+Shift+W",
            describe: "remove the shown panel",
            group: "panels",
            action: Action::RemovePanel,
            key: Key::W,
            mods: CMD_SHIFT,
        },
        Binding {
            mac: PASTE_PANEL_CHORD.0,
            other: PASTE_PANEL_CHORD.1,
            describe: "paste into a fresh panel",
            group: "panels",
            action: Action::PasteIntoNewPanel,
            key: Key::V,
            mods: CMD_SHIFT,
        },
        Binding {
            mac: "⌘⌥S",
            other: "Ctrl+Alt+S",
            describe: "side by side",
            group: "view",
            action: Action::ToggleSideBySide,
            key: Key::S,
            mods: CMD_ALT,
        },
        Binding {
            mac: "⌘L",
            other: "Ctrl+L",
            describe: "line numbers",
            group: "view",
            action: Action::ToggleLineNumbers,
            key: Key::L,
            mods: CMD,
        },
        Binding {
            mac: "⌘\\",
            other: "Ctrl+\\",
            describe: "wrap long lines",
            group: "view",
            action: Action::ToggleWrap,
            key: Key::Backslash,
            mods: CMD,
        },
        Binding {
            mac: "⌘,",
            other: "Ctrl+,",
            describe: "settings",
            group: "view",
            action: Action::ToggleSettings,
            key: Key::Comma,
            mods: CMD,
        },
        Binding {
            mac: HELP_CHORD.0,
            other: HELP_CHORD.1,
            describe: "this list",
            group: "view",
            action: Action::ToggleHelp,
            key: Key::Slash,
            mods: CMD,
        },
        Binding {
            mac: SAVE_CHORD.0,
            other: SAVE_CHORD.1,
            describe: "save the result to a file",
            group: "result",
            action: Action::SaveResult,
            key: Key::S,
            mods: CMD,
        },
        Binding {
            mac: "⌘⇧S",
            other: "Ctrl+Shift+S",
            describe: "save the result as a new file",
            group: "result",
            action: Action::SaveResultAs,
            key: Key::S,
            mods: CMD_SHIFT,
        },
        Binding {
            mac: TAKE_CHORD.0,
            other: TAKE_CHORD.1,
            describe: "take the current difference into the result",
            group: "result",
            action: Action::TakeCurrentDifference,
            key: Key::Enter,
            mods: CMD_SHIFT,
        },
        Binding {
            mac: "⌘Z",
            other: "Ctrl+Z",
            describe: "undo the last take",
            group: "result",
            action: Action::UndoTake,
            key: Key::Z,
            mods: CMD,
        },
        Binding {
            mac: "⌘⇧Z",
            other: "Ctrl+Shift+Z",
            describe: "redo the last take",
            group: "result",
            action: Action::RedoTake,
            key: Key::Z,
            mods: CMD_SHIFT,
        },
    ];
    // Number keys select which panel's diff is on screen.
    for (i, key) in [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
    ]
    .into_iter()
    .enumerate()
    {
        v.push(Binding {
            mac: "⌘1…6",
            other: "Ctrl+1…6",
            describe: "show that panel's diff",
            group: "panels",
            action: Action::ShowDiff(i),
            key,
            mods: CMD,
        });
    }
    v
}

/// Actions triggered this frame. Text fields keep ordinary typing to themselves,
/// but every binding here carries a modifier, so they do not collide.
pub fn pressed(input: &egui::InputState) -> Vec<Action> {
    bindings()
        .into_iter()
        .filter(|b| {
            if b.action == Action::TakeCurrentDifference {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key,
                            pressed: true,
                            repeat: false,
                            modifiers,
                            ..
                        } if *key == b.key && modifiers.matches_exact(b.mods)
                    )
                })
            } else {
                input.key_pressed(b.key) && input.modifiers.matches_exact(b.mods)
            }
        })
        .map(|b| b.action)
        .collect()
}

pub struct HelpRow {
    pub label: &'static str,
    pub describe: &'static str,
    pub group: &'static str,
}

/// The help overlay's contents: every chord once, with the numeric block
/// collapsed, plus the things you do with the mouse -- which are half of how
/// the app is operated and were documented nowhere.
pub fn help_rows() -> Vec<HelpRow> {
    let mut rows: Vec<HelpRow> = Vec::new();
    for group in ["file", "compare", "panels", "result", "view"] {
        for b in bindings().into_iter().filter(|b| b.group == group) {
            if !rows.iter().any(|r| r.label == b.label()) {
                rows.push(HelpRow {
                    label: b.label(),
                    describe: b.describe,
                    group: b.group,
                });
            }
        }
    }
    rows.push(HelpRow {
        label: "drop",
        describe: "load files into the panels you drop them on",
        group: "mouse",
    });
    rows.push(HelpRow {
        label: "click A",
        describe: "make that panel the baseline",
        group: "mouse",
    });
    rows.push(HelpRow {
        label: "⋯",
        describe: "open, reload, follow on disk, clear, remove",
        group: "mouse",
    });
    rows.push(HelpRow {
        label: "Combine…",
        describe: "build a result you can take differences into",
        group: "result",
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(key: Key, modifiers: Modifiers) -> Vec<Action> {
        dispatch_on(&egui::Context::default(), key, modifiers)
    }

    fn dispatch_on(ctx: &egui::Context, key: Key, modifiers: Modifiers) -> Vec<Action> {
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::ModifiersChanged(modifiers));
        input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            // Egui derives this from `keys_down`, ignoring what an integration
            // supplied. A second press on the same Context is therefore the
            // deterministic way to exercise a repeat event.
            repeat: false,
            modifiers,
        });
        ctx.begin_pass(input);
        let actions = ctx.input(pressed);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();
        actions
    }

    /// Two bindings on the same chord means one of them silently never fires.
    #[test]
    fn no_chord_is_bound_twice() {
        let all = bindings();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(
                    !(a.key == b.key && a.mods == b.mods),
                    "{} and {} share a chord",
                    a.describe,
                    b.describe
                );
            }
        }
    }

    /// Every combination of the logical modifiers either selects the one exact
    /// binding for a key or selects nothing. In particular, Shift/Alt variants
    /// must never fall through to the plain Command action on the same key.
    #[test]
    fn modifier_combinations_match_exactly_without_overlap() {
        let all = bindings();
        let mut keys = Vec::new();
        for key in all.iter().map(|binding| binding.key) {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        for key in keys {
            for command in [false, true] {
                for shift in [false, true] {
                    for alt in [false, true] {
                        let pressed = Modifiers {
                            command,
                            shift,
                            alt,
                            ..Modifiers::NONE
                        };
                        let actual = dispatch(key, pressed);
                        let expected: Vec<Action> = all
                            .iter()
                            .filter(|binding| binding.key == key && binding.mods == pressed)
                            .map(|binding| binding.action)
                            .collect();

                        assert_eq!(
                            actual, expected,
                            "unexpected match for {key:?} with {pressed:?}"
                        );
                        assert!(
                            actual.len() <= 1,
                            "{key:?} with {pressed:?} dispatches {actual:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn standard_shortcut_variants_dispatch_only_the_intended_action() {
        let cases = [
            (Key::O, CMD, Action::OpenFile),
            (Key::W, CMD, Action::CloseWindow),
            (Key::F, CMD, Action::Find),
            (Key::W, CMD_SHIFT, Action::RemovePanel),
            (Key::S, CMD, Action::SaveResult),
            (Key::S, CMD_SHIFT, Action::SaveResultAs),
            (Key::Enter, CMD_SHIFT, Action::TakeCurrentDifference),
            (Key::S, CMD_ALT, Action::ToggleSideBySide),
            (Key::Z, CMD, Action::UndoTake),
            (Key::Z, CMD_SHIFT, Action::RedoTake),
        ];
        for (key, mods, expected) in cases {
            let actual = dispatch(key, mods);
            assert_eq!(actual, vec![expected], "wrong action for {key:?} {mods:?}");
        }

        let all = bindings();
        let unsupported = CMD_ALT.plus(Modifiers::SHIFT);
        assert!(
            all.iter()
                .all(|binding| binding.key != Key::S || !unsupported.matches_exact(binding.mods)),
            "Command+Option+Shift+S must not fall through to another S binding"
        );
    }

    /// Every action must be reachable, and every chord must be documented.
    #[test]
    fn every_binding_is_listed_in_the_help() {
        let rows = help_rows();
        for b in bindings() {
            assert!(
                rows.iter().any(|r| r.label == b.label()),
                "{} has no help row",
                b.describe
            );
        }
    }

    #[test]
    fn compare_chord_uses_only_bundled_font_safe_glyphs() {
        let compare = bindings()
            .into_iter()
            .find(|binding| binding.action == Action::Compare)
            .expect("compare has a binding");
        assert_eq!(compare.mac, "⌘Enter");
        assert_eq!(compare.other, "Ctrl+Enter");
        assert!(!compare.mac.contains('⏎'));
        assert_eq!(compare_label(), compare.label());
    }

    #[test]
    fn holding_the_take_chord_does_not_take_successive_differences() {
        let ctx = egui::Context::default();
        assert_eq!(
            dispatch_on(&ctx, Key::Enter, CMD_SHIFT),
            vec![Action::TakeCurrentDifference]
        );
        assert!(dispatch_on(&ctx, Key::Enter, CMD_SHIFT).is_empty());
    }

    /// The number keys are a block: one help row, one per selectable panel.
    #[test]
    fn panel_selection_covers_the_panel_limit() {
        let n = bindings()
            .iter()
            .filter(|b| matches!(b.action, Action::ShowDiff(_)))
            .count();
        assert_eq!(n, 6, "panel-selection chords should match MAX_PANELS");
    }

    /// Bindings all carry a modifier, so typing into a panel cannot trigger one.
    #[test]
    fn no_binding_is_a_bare_keypress() {
        for b in bindings() {
            assert!(b.mods.command, "{} would fire while typing", b.describe);
        }
    }

    /// ⌘⌫ is "delete to the start of the line" in every macOS text field, and
    /// the panels *are* text fields. It used to clear a panel outright, with no
    /// undo and an ambiguous target; clearing now lives in the panel's own menu.
    ///
    /// ⌘Z is the one exception, and only because `App::handle_keys` drops it
    /// while any text field has focus: inside a panel it is still that field's
    /// own undo, and only over the diff -- where egui has nothing bound and users
    /// press it anyway -- does it undo a take.
    #[test]
    fn no_binding_collides_with_a_standard_text_editing_chord() {
        let reserved = [
            (Key::Backspace, CMD),
            (Key::A, CMD),
            (Key::C, CMD),
            (Key::V, CMD),
            (Key::X, CMD),
            (Key::Z, CMD),
            (Key::Z, CMD_SHIFT),
        ];
        for b in bindings() {
            if matches!(b.action, Action::UndoTake | Action::RedoTake) {
                continue;
            }
            assert!(
                !reserved.iter().any(|(k, m)| *k == b.key && *m == b.mods),
                "{} steals a text-editing chord",
                b.describe
            );
        }
    }
}
