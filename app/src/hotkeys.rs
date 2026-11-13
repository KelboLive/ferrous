//! Global hotkeys (work while the app is in the background, e.g. in-game).

use std::collections::HashMap;
use std::str::FromStr;

use ferrous_core::{Hotkey, HotkeyAction};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

/// Validates a key combination like "Ctrl+Shift+R".
pub fn parse(keys: &str) -> Result<HotKey, String> {
    let normalised = keys.replace(' ', "");
    HotKey::from_str(&normalised).map_err(|e| e.to_string())
}

pub struct Hotkeys {
    manager: Option<GlobalHotKeyManager>,
    events: std::sync::mpsc::Receiver<GlobalHotKeyEvent>,
    registered: Vec<HotKey>,
    actions: HashMap<u32, HotkeyAction>,
    bound: Vec<Hotkey>,
}

impl Hotkeys {
    /// `wake` is repainted when a hotkey fires, so the UI needn't poll
    /// at a high rate just to notice key presses.
    pub fn new(wake: eframe::egui::Context) -> Self {
        let manager = GlobalHotKeyManager::new().map_err(|e| log::warn!("global hotkeys unavailable: {e}")).ok();
        let (tx, events) = std::sync::mpsc::channel();
        GlobalHotKeyEvent::set_event_handler(Some(move |ev| {
            let _ = tx.send(ev);
            wake.request_repaint();
        }));
        Self { manager, events, registered: Vec::new(), actions: HashMap::new(), bound: Vec::new() }
    }

    /// Re-registers if the bindings changed. Returns problems to show the user.
    pub fn sync(&mut self, bindings: &[Hotkey]) -> Vec<String> {
        if bindings == self.bound.as_slice() {
            return Vec::new();
        }
        self.bound = bindings.to_vec();
        let mut problems = Vec::new();
        let Some(m) = &self.manager else { return problems };
        for hk in self.registered.drain(..) {
            let _ = m.unregister(hk);
        }
        self.actions.clear();
        for b in bindings {
            if b.keys.trim().is_empty() {
                continue;
            }
            match parse(&b.keys) {
                Ok(hk) => match m.register(hk) {
                    Ok(()) => {
                        self.actions.insert(hk.id(), b.action.clone());
                        self.registered.push(hk);
                    }
                    Err(e) => problems.push(format!("Hotkey {} unavailable: {e}", b.keys)),
                },
                Err(e) => problems.push(format!("Hotkey \"{}\" isn't valid: {e}", b.keys)),
            }
        }
        problems
    }

    /// Actions triggered since the last call.
    pub fn poll(&self) -> Vec<HotkeyAction> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            if ev.state == HotKeyState::Pressed {
                if let Some(a) = self.actions.get(&ev.id) {
                    out.push(a.clone());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_friendly_combos() {
        assert!(parse("Ctrl+Shift+R").is_ok());
        assert!(parse("F9").is_ok());
        assert!(parse("Alt + 1").is_ok());
        assert_ne!(parse("Ctrl+R").unwrap().id(), parse("Ctrl+Shift+R").unwrap().id());
        assert!(parse("Ctrl+Banana").is_err());
    }
}
