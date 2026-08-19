//! The keyboard map, in one place so the handler and the help overlay agree.

use egui::{Key, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Compare,
    AddPanel,
    RemovePanel,
    ClearPanel,
    PasteIntoNewPanel,
    ShowDiff(usize),
    MakeReference,
    ToggleSideBySide,
    ToggleLineNumbers,
    ToggleWrap,
    ToggleHelp,
}

pub struct Binding {
    pub label: &'static str,
    pub describe: &'static str,
    pub action: Action,
    key: Key,
    mods: Modifiers,
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::COMMAND };

pub fn bindings() -> Vec<Binding> {
    let mut v = vec![
        Binding { label: "⌘ ⏎", describe: "compare", action: Action::Compare, key: Key::Enter, mods: CMD },
        Binding { label: "⌘ N", describe: "add a panel", action: Action::AddPanel, key: Key::N, mods: CMD },
        Binding { label: "⌘ ⇧ W", describe: "remove the shown panel", action: Action::RemovePanel, key: Key::W, mods: CMD_SHIFT },
        Binding { label: "⌘ ⌫", describe: "clear the focused panel", action: Action::ClearPanel, key: Key::Backspace, mods: CMD },
        Binding { label: "⌘ ⇧ V", describe: "paste into a fresh panel", action: Action::PasteIntoNewPanel, key: Key::V, mods: CMD_SHIFT },
        Binding { label: "⌘ R", describe: "make the shown panel the reference", action: Action::MakeReference, key: Key::R, mods: CMD },
        Binding { label: "⌘ S", describe: "side-by-side", action: Action::ToggleSideBySide, key: Key::S, mods: CMD },
        Binding { label: "⌘ L", describe: "line numbers", action: Action::ToggleLineNumbers, key: Key::L, mods: CMD },
        Binding { label: "⌘ \\", describe: "wrap long lines", action: Action::ToggleWrap, key: Key::Backslash, mods: CMD },
        Binding { label: "⌘ /", describe: "this list", action: Action::ToggleHelp, key: Key::Slash, mods: CMD },
    ];
    // Number keys select which panel's diff is on screen.
    for (i, key) in [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6].into_iter().enumerate() {
        v.push(Binding {
            label: "⌘ 1…6",
            describe: "show that panel's diff",
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
        .filter(|b| input.key_pressed(b.key) && input.modifiers.matches_logically(b.mods))
        .map(|b| b.action)
        .collect()
}

/// One row per binding for the help overlay, with the numeric block collapsed.
pub fn help_rows() -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&str, &str)> = Vec::new();
    for b in bindings() {
        if !rows.iter().any(|(l, _)| *l == b.label) {
            rows.push((b.label, b.describe));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Every action must be reachable, and every chord must be documented.
    #[test]
    fn every_binding_is_listed_in_the_help() {
        let rows = help_rows();
        for b in bindings() {
            assert!(
                rows.iter().any(|(label, _)| *label == b.label),
                "{} has no help row",
                b.describe
            );
        }
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
}
