// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Input injection via Win32 `SendInput`.
//!
//! The Trireme back-channel carries `core_proto::InputEvent`s whose `Key`
//! `scancode` is a **Linux evdev keycode** (the wire format targets the Linux
//! Smithay seat; non-Linux clients translate their native scancode → evdev).
//! On Windows we translate evdev → PS/2 **Set-1** scancodes and inject them with
//! `KEYEVENTF_SCANCODE`, which is layout-independent and lands in the focused
//! application exactly like a real keyboard.
//!
//! Pointer events map to `MOUSEEVENTF_*`. Absolute motion uses normalized 0..1
//! coordinates scaled to the 0..65535 range over the primary display (Phase 1);
//! multi-monitor / virtual-display coordinate mapping arrives with the IddCx
//! display in Phase 2.

use core_proto::InputEvent;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

pub struct Injector;

impl Injector {
    pub fn new() -> Self {
        Injector
    }

    pub fn dispatch(&self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key {
                scancode, pressed, ..
            } => key(scancode, pressed),
            InputEvent::PointerAbs { x, y } => mouse_abs(x, y),
            InputEvent::PointerRel { dx, dy } => mouse_rel(dx, dy),
            InputEvent::PointerButton { button, pressed } => mouse_button(button, pressed),
            InputEvent::Scroll { dx, dy } => scroll(dx, dy),
            // Gamepad input isn't injected on the Windows host yet (would need a
            // ViGEm virtual pad); ignore for now so the protocol stays shared.
            InputEvent::GamepadButton { .. }
            | InputEvent::GamepadAxis { .. }
            | InputEvent::GamepadConnect { .. }
            | InputEvent::GamepadDisconnect { .. }
            | InputEvent::GamepadMotion { .. }
            | InputEvent::ControllerTuning { .. } => {}
        }
    }
}

impl Default for Injector {
    fn default() -> Self {
        Self::new()
    }
}

fn send(input: INPUT) {
    // SAFETY: a single fully-initialized INPUT; cbSize is its size.
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
}

fn key(evdev: u32, pressed: bool) {
    let Some((scan, extended)) = evdev_to_set1(evdev) else {
        tracing::debug!("dropping unmapped evdev keycode {evdev}");
        return;
    };
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    send(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn mouse(dx: i32, dy: i32, mouse_data: i32, flags: MOUSE_EVENT_FLAGS) {
    send(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn mouse_abs(x: f32, y: f32) {
    let nx = (x.clamp(0.0, 1.0) * 65535.0).round() as i32;
    let ny = (y.clamp(0.0, 1.0) * 65535.0).round() as i32;
    mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE);
}

fn mouse_rel(dx: f32, dy: f32) {
    mouse(dx.round() as i32, dy.round() as i32, 0, MOUSEEVENTF_MOVE);
}

fn mouse_button(button: u8, pressed: bool) {
    // Index order matches evdev BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA (proto §).
    let (flags, data) = match button {
        0 => (
            if pressed {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        ),
        1 => (
            if pressed {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        ),
        2 => (
            if pressed {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        ),
        // X buttons carry which button in mouseData: 1 = XBUTTON1 (Back),
        // 2 = XBUTTON2 (Forward).
        3 => (
            if pressed {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            1,
        ),
        4 => (
            if pressed {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            2,
        ),
        other => {
            tracing::debug!("dropping unknown pointer button {other}");
            return;
        }
    };
    mouse(0, 0, data, flags);
}

fn scroll(dx: f32, dy: f32) {
    // 1.0 == one notch == WHEEL_DELTA (120). Vertical wheel + horizontal hwheel.
    if dy != 0.0 {
        mouse(0, 0, (dy * 120.0).round() as i32, MOUSEEVENTF_WHEEL);
    }
    if dx != 0.0 {
        mouse(0, 0, (dx * 120.0).round() as i32, MOUSEEVENTF_HWHEEL);
    }
}

/// Translate a Linux evdev keycode to a PS/2 Set-1 scancode + whether it is an
/// `E0`-prefixed "extended" key.
///
/// evdev keycodes 1..=88 derive from the original XT/AT set and map **1:1** to
/// Set-1 scancodes (Esc=1, A=30→0x1E, Enter=28→0x1C, F1=59→0x3B, the numpad
/// block, …). The keys that are `E0`-prefixed in Set-1 (right Ctrl/Alt, the
/// gray arrow/navigation cluster, keypad Enter/Slash, the Win/Menu keys) have
/// evdev codes ≥ 96 and are listed explicitly.
fn evdev_to_set1(evdev: u32) -> Option<(u16, bool)> {
    if (1..=88).contains(&evdev) {
        return Some((evdev as u16, false));
    }
    let sc: u16 = match evdev {
        96 => 0x1C,  // KP_ENTER
        97 => 0x1D,  // RIGHTCTRL
        98 => 0x35,  // KP_SLASH
        100 => 0x38, // RIGHTALT (AltGr)
        102 => 0x47, // HOME
        103 => 0x48, // UP
        104 => 0x49, // PAGEUP
        105 => 0x4B, // LEFT
        106 => 0x4D, // RIGHT
        107 => 0x4F, // END
        108 => 0x50, // DOWN
        109 => 0x51, // PAGEDOWN
        110 => 0x52, // INSERT
        111 => 0x53, // DELETE
        125 => 0x5B, // LEFTMETA (LWin)
        126 => 0x5C, // RIGHTMETA (RWin)
        127 => 0x5D, // COMPOSE (Menu)
        _ => return None,
    };
    Some((sc, true))
}
