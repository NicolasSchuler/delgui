//! Optional system-wide hotkey.
//!
//! On macOS this goes through Carbon's `RegisterEventHotKey`, which needs no
//! accessibility permission. It only works while deltapanes is running --
//! *launching* the app from a hotkey is a job for launchd, Raycast or a
//! keyboard tool, not something an application can arrange for itself.

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

pub struct Hotkey {
    // Held so the registration outlives this struct's creation.
    _manager: GlobalHotKeyManager,
    id: u32,
    pub label: String,
}

impl Hotkey {
    /// Register ⌘⇧D (Ctrl+Shift+D off macOS).
    pub fn register() -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        let hotkey = HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyD);
        manager.register(hotkey).map_err(|e| e.to_string())?;
        Ok(Self {
            _manager: manager,
            id: hotkey.id(),
            label: if cfg!(target_os = "macos") { "⌘⇧D".into() } else { "Super+Shift+D".into() },
        })
    }

    /// True when the hotkey fired since the last check.
    pub fn fired(&self) -> bool {
        let mut hit = false;
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.id == self.id && event.state == HotKeyState::Pressed {
                hit = true;
            }
        }
        hit
    }
}
