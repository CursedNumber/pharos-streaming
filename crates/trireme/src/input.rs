// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Input capture and the (configurable) filter that decides which winit events
//! get forwarded to Pharos, which Trireme handles locally, and which fall
//! through to the host OS.
//!
//! The user's contract is *not* Polaris/Nova: we never grab all client HID
//! unconditionally. The four supported modes (`focus | toggle | always | off`),
//! per-source in `sources.toml`, decide whether forwarding is active. Within
//! that, `local_escapes` (e.g. `F11`, `Alt+Tab`, `Ctrl+Shift+Escape`) and
//! `hotkeys` (e.g. `Ctrl+Alt+End` = toggle capture) are matched first — they
//! short-circuit forwarding so the user can always reach their own desktop and
//! their own player UI.

use serde::Deserialize;
use winit::event::{ElementState, MouseButton};
use winit::keyboard::KeyCode;

use core_proto::{InputEvent, Modifiers};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Forward whenever the Trireme window has keyboard focus *and* the pointer
    /// is inside a source cell. Default — natural for laptops.
    #[default]
    Focus,
    /// Off until the user triggers `hotkeys.toggle_capture`. On until they
    /// trigger it again.
    Toggle,
    /// Forward whenever connected (Polaris/Nova-style). Power-user opt-in.
    Always,
    /// Never forward — view-only.
    Off,
}

/// Trireme hotkeys handled inside the client (not forwarded, not passed to OS).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Hotkeys {
    /// Toggle `CaptureMode::Toggle`'s active state. Default: `Ctrl+Alt+End`.
    pub toggle_capture: Option<String>,
    /// Toggle fullscreen on the player window. Default: `F10`.
    pub toggle_fullscreen: Option<String>,
    /// Disconnect all sources and exit. Default: `Ctrl+Alt+Backspace`.
    pub disconnect: Option<String>,
}

/// `[input]` section of `sources.toml`. Everything has a sensible default so
/// existing configs keep working unchanged.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InputCfg {
    pub mode: CaptureMode,
    /// Chord strings that stay local — never forwarded. E.g. `["F11", "Alt+Tab"]`.
    pub local_escapes: Vec<String>,
    pub hotkeys: Hotkeys,
    /// Class gates — drop a class entirely if false.
    pub forward_keyboard: bool,
    pub forward_mouse: bool,
    pub forward_scroll: bool,
}

impl Default for InputCfg {
    fn default() -> Self {
        Self {
            mode: CaptureMode::Focus,
            local_escapes: default_local_escapes(),
            hotkeys: Hotkeys {
                toggle_capture: Some("Ctrl+Alt+End".into()),
                toggle_fullscreen: Some("F10".into()),
                disconnect: Some("Ctrl+Alt+Backspace".into()),
            },
            forward_keyboard: true,
            forward_mouse: true,
            forward_scroll: true,
        }
    }
}

fn default_local_escapes() -> Vec<String> {
    vec![
        // Windows SAS — the kernel takes these before we see them, but we list
        // them so config introspection reflects what stays local.
        "Ctrl+Alt+Delete".into(),
        "Ctrl+Shift+Escape".into(),
        "Win+L".into(),
        // App-level escape combos winit *would* otherwise forward.
        "F11".into(),
        "Alt+Tab".into(),
        "Alt+Shift+Tab".into(),
        "Win+Tab".into(),
    ]
}

/// A parsed key chord — modifier mask + the named main key. Comparison is
/// modifier-set-equal, not subset, so `Alt+Tab` matches Alt+Tab but not
/// Ctrl+Alt+Tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    pub key: String,
}

impl Chord {
    pub fn parse(s: &str) -> Option<Self> {
        // Delegate to the shared grammar so the client and the Pharos panel can
        // never disagree on what a valid chord is.
        let c = core_proto::chord::Chord::parse(s)?;
        Some(Chord {
            ctrl: c.ctrl,
            shift: c.shift,
            alt: c.alt,
            meta: c.meta,
            key: c.key,
        })
    }

    /// Match an incoming key event against this chord. We match on the
    /// canonical key name (matches PhysicalKey debug names like `F11`, `Tab`,
    /// `Delete`, `Backspace`, `KeyA`...) so config strings stay readable.
    pub fn matches(&self, mods: Modifiers, key_name: &str) -> bool {
        self.ctrl == mods.ctrl
            && self.shift == mods.shift
            && self.alt == mods.alt
            && self.meta == mods.meta
            && self.key.eq_ignore_ascii_case(key_name)
    }
}

/// Translate a winit `PhysicalKey::Code(KeyCode::...)` to the Linux evdev code
/// that Pharos's smithay seat consumes. Returns `None` for keys we don't have
/// a mapping for (extended multimedia keys, etc. — fine to drop for first cut).
pub fn keycode_to_evdev(code: KeyCode) -> Option<u32> {
    // Numbers from /usr/include/linux/input-event-codes.h (KEY_*). We list the
    // mapping inline rather than pulling a crate — it's a closed set and the
    // table is exactly the spec.
    Some(match code {
        KeyCode::Escape => 1,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Minus => 12,
        KeyCode::Equal => 13,
        KeyCode::Backspace => 14,
        KeyCode::Tab => 15,
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::BracketLeft => 26,
        KeyCode::BracketRight => 27,
        KeyCode::Enter => 28,
        KeyCode::ControlLeft => 29,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::Semicolon => 39,
        KeyCode::Quote => 40,
        KeyCode::Backquote => 41,
        KeyCode::ShiftLeft => 42,
        KeyCode::Backslash => 43,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Comma => 51,
        KeyCode::Period => 52,
        KeyCode::Slash => 53,
        KeyCode::ShiftRight => 54,
        KeyCode::NumpadMultiply => 55,
        KeyCode::AltLeft => 56,
        KeyCode::Space => 57,
        KeyCode::CapsLock => 58,
        KeyCode::F1 => 59,
        KeyCode::F2 => 60,
        KeyCode::F3 => 61,
        KeyCode::F4 => 62,
        KeyCode::F5 => 63,
        KeyCode::F6 => 64,
        KeyCode::F7 => 65,
        KeyCode::F8 => 66,
        KeyCode::F9 => 67,
        KeyCode::F10 => 68,
        KeyCode::NumLock => 69,
        KeyCode::ScrollLock => 70,
        KeyCode::Numpad7 => 71,
        KeyCode::Numpad8 => 72,
        KeyCode::Numpad9 => 73,
        KeyCode::NumpadSubtract => 74,
        KeyCode::Numpad4 => 75,
        KeyCode::Numpad5 => 76,
        KeyCode::Numpad6 => 77,
        KeyCode::NumpadAdd => 78,
        KeyCode::Numpad1 => 79,
        KeyCode::Numpad2 => 80,
        KeyCode::Numpad3 => 81,
        KeyCode::Numpad0 => 82,
        KeyCode::NumpadDecimal => 83,
        KeyCode::F11 => 87,
        KeyCode::F12 => 88,
        KeyCode::NumpadEnter => 96,
        KeyCode::ControlRight => 97,
        KeyCode::NumpadDivide => 98,
        KeyCode::PrintScreen => 99,
        KeyCode::AltRight => 100,
        KeyCode::Home => 102,
        KeyCode::ArrowUp => 103,
        KeyCode::PageUp => 104,
        KeyCode::ArrowLeft => 105,
        KeyCode::ArrowRight => 106,
        KeyCode::End => 107,
        KeyCode::ArrowDown => 108,
        KeyCode::PageDown => 109,
        KeyCode::Insert => 110,
        KeyCode::Delete => 111,
        KeyCode::Pause => 119,
        KeyCode::SuperLeft => 125,  // Win key (LMETA in evdev)
        KeyCode::SuperRight => 126, // RMETA
        KeyCode::ContextMenu => 127,
        _ => return None,
    })
}

/// Human-readable name for a key, matching the config's chord syntax.
/// Used to check `local_escapes` and hotkey strings against incoming events.
pub fn key_name(code: KeyCode) -> Option<&'static str> {
    Some(match code {
        KeyCode::Escape => "Escape",
        KeyCode::Tab => "Tab",
        KeyCode::Backspace => "Backspace",
        KeyCode::Delete => "Delete",
        KeyCode::Enter => "Enter",
        KeyCode::Space => "Space",
        KeyCode::F1 => "F1",
        KeyCode::F2 => "F2",
        KeyCode::F3 => "F3",
        KeyCode::F4 => "F4",
        KeyCode::F5 => "F5",
        KeyCode::F6 => "F6",
        KeyCode::F7 => "F7",
        KeyCode::F8 => "F8",
        KeyCode::F9 => "F9",
        KeyCode::F10 => "F10",
        KeyCode::F11 => "F11",
        KeyCode::F12 => "F12",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::ArrowUp => "Up",
        KeyCode::ArrowDown => "Down",
        KeyCode::ArrowLeft => "Left",
        KeyCode::ArrowRight => "Right",
        KeyCode::Insert => "Insert",
        KeyCode::PrintScreen => "PrintScreen",
        KeyCode::Pause => "Pause",
        KeyCode::ContextMenu => "Menu",
        // Letters and digits we name by their KeyCode debug form (e.g. "KeyA",
        // "Digit5") so users *can* bind them, even if it's unusual. Most
        // chords use the modifier+function-key pattern anyway.
        _ => return None,
    })
}

/// Decide what to do with a key event. Returns one of:
/// - `Forward(InputEvent)` — send to Pharos.
/// - `Local` — let the event fall through to the host OS (winit default).
/// - `Hotkey(name)` — Trireme handles it; client decides what to do.
/// - `Drop` — neither forward nor act locally (e.g. capture off, or class gate).
#[derive(Debug, Clone)]
pub enum KeyDecision {
    Forward(InputEvent),
    Local,
    Hotkey(&'static str),
    Drop,
}

pub fn classify_key(
    cfg: &InputCfg,
    capture_active: bool,
    state: ElementState,
    code: KeyCode,
    mods: Modifiers,
) -> KeyDecision {
    // Hotkeys first — these always take precedence so the user can always
    // reach the client UI even while capture is forwarding.
    if let Some(name) = key_name(code) {
        for (slot, label) in [
            (&cfg.hotkeys.toggle_capture, "toggle_capture"),
            (&cfg.hotkeys.toggle_fullscreen, "toggle_fullscreen"),
            (&cfg.hotkeys.disconnect, "disconnect"),
        ] {
            if let Some(chord) = slot.as_deref().and_then(Chord::parse) {
                if chord.matches(mods, name) && state == ElementState::Pressed {
                    return KeyDecision::Hotkey(label);
                }
            }
        }
        // Local escapes — never forwarded, fall through to OS.
        for esc in &cfg.local_escapes {
            if let Some(chord) = Chord::parse(esc) {
                if chord.matches(mods, name) {
                    return KeyDecision::Local;
                }
            }
        }
    }
    if !cfg.forward_keyboard || !capture_active {
        return KeyDecision::Drop;
    }
    let Some(scancode) = keycode_to_evdev(code) else {
        return KeyDecision::Drop;
    };
    KeyDecision::Forward(InputEvent::Key {
        scancode,
        pressed: matches!(state, ElementState::Pressed),
        mods,
    })
}

pub fn classify_mouse_button(
    cfg: &InputCfg,
    capture_active: bool,
    btn: MouseButton,
    state: ElementState,
) -> Option<InputEvent> {
    if !cfg.forward_mouse || !capture_active {
        return None;
    }
    let button = match btn {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(_) => return None,
    };
    Some(InputEvent::PointerButton {
        button,
        pressed: matches!(state, ElementState::Pressed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_parses_modifiers_and_key() {
        let c = Chord::parse("Ctrl+Alt+End").unwrap();
        assert!(c.ctrl && c.alt && !c.shift && !c.meta);
        assert_eq!(c.key, "End");
    }

    #[test]
    fn chord_matches_exact_modifier_set() {
        let c = Chord::parse("Alt+Tab").unwrap();
        let mods_alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert!(c.matches(mods_alt, "Tab"));
        // Ctrl+Alt+Tab is a different chord — must NOT match Alt+Tab.
        let mods_ctrl_alt = Modifiers {
            alt: true,
            ctrl: true,
            ..Default::default()
        };
        assert!(!c.matches(mods_ctrl_alt, "Tab"));
    }

    #[test]
    fn local_escape_short_circuits_forward() {
        let cfg = InputCfg::default();
        let mods_alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        let dec = classify_key(&cfg, true, ElementState::Pressed, KeyCode::Tab, mods_alt);
        assert!(matches!(dec, KeyDecision::Local), "Alt+Tab must be local");
    }

    #[test]
    fn hotkey_wins_over_forward() {
        let cfg = InputCfg::default();
        let mods = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let dec = classify_key(&cfg, true, ElementState::Pressed, KeyCode::End, mods);
        assert!(matches!(dec, KeyDecision::Hotkey("toggle_capture")));
    }

    #[test]
    fn capture_off_drops_non_special_keys() {
        let cfg = InputCfg::default();
        let dec = classify_key(
            &cfg,
            false,
            ElementState::Pressed,
            KeyCode::KeyA,
            Modifiers::default(),
        );
        assert!(matches!(dec, KeyDecision::Drop));
    }

    #[test]
    fn forwards_letter_when_active() {
        let cfg = InputCfg::default();
        let dec = classify_key(
            &cfg,
            true,
            ElementState::Pressed,
            KeyCode::KeyA,
            Modifiers::default(),
        );
        match dec {
            KeyDecision::Forward(InputEvent::Key {
                scancode, pressed, ..
            }) => {
                assert_eq!(scancode, 30); // KEY_A
                assert!(pressed);
            }
            _ => panic!("expected Forward"),
        }
    }
}
