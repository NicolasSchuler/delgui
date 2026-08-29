//! One edit to the macOS application menu, because its Quit item destroys work.
//!
//! winit installs a standard application menu for us -- About, Services, Hide,
//! Show All, Quit -- and everything in it is wanted. The Quit item is not: its
//! action is `terminate:`, which tears the process down without ever asking a
//! window to close. `close_requested` is the only signal `App::quit_guard`
//! can hook, so it never runs, and ⌘Q takes a panel holding pasted text that
//! exists nowhere else with it. Measured against ⌘W on the same state, which
//! *is* guarded and offers Discard / Keep working.
//!
//! Rebuilding the menu ourselves would mean re-creating four items we already
//! have and localising their titles. Repointing one action does not:
//! `performClose:` travels up the responder chain to the key window and asks
//! it to close, which is exactly the path ⌘W already takes.
//!
//! The item keeps its ⌘Q key equivalent, so AppKit still claims the chord and
//! `Action::Quit` in the keymap is the live path only where there is no such
//! menu -- Linux and Windows. Both end in the same guarded close.

/// Point the application menu's Quit item at the window rather than at the
/// process. Safe to call on every frame; it does nothing once the swap is done,
/// and nothing at all if the menu is not there yet.
///
/// Returns whether the menu is now in the wanted state, which is what lets the
/// caller stop asking.
#[cfg(target_os = "macos")]
pub fn guard_quit() -> bool {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, sel};
    use objc2_app_kit::{NSApplication, NSMenuItem};

    // Called from the frame loop, which is the main thread -- but ask rather
    // than assert, since every AppKit call below requires it.
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    // The menu is built during `applicationDidFinishLaunching`, which may not
    // have run by the first frame. Absent is "not yet", not "never".
    let Some(menu) = app.mainMenu() else {
        return false;
    };
    let Some(submenu) = menu.itemAtIndex(0).and_then(|item| item.submenu()) else {
        return false;
    };
    // Found by action rather than by title: the title is localised, and
    // "Quit delgui" is only its English spelling.
    let quit: Option<Retained<NSMenuItem>> = submenu
        .itemArray()
        .iter()
        .find(|item| item.action() == Some(sel!(terminate:)))
        .map(|item| item.to_owned());
    match quit {
        Some(item) => {
            // SAFETY: a standard AppKit selector on a menu item we own, sent on
            // the main thread. `performClose:` is sent to nil, so it walks the
            // responder chain to the key window -- the same request the window's
            // own close button makes, and the one `quit_guard` can intercept.
            unsafe {
                item.setTarget(None);
                item.setAction(Some(sel!(performClose:)));
            }
            true
        }
        // No `terminate:` item left: either we already swapped it, or this
        // winit no longer installs one. Either way there is nothing to fix.
        None => true,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn guard_quit() -> bool {
    true
}
