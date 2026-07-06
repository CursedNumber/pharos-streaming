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

/// Tracks what the remote client currently holds DOWN (evdev keycodes and
/// mouse buttons), so a disconnect can release everything. Without this, a
/// key-down whose matching up is lost to a disconnect, preemption, or a cut
/// input backlog stays pressed in the host session indefinitely (the stuck
/// Windows key from the 2026-07-05 validation).
pub struct Injector {
    held_keys: std::collections::HashSet<u32>,
    held_buttons: std::collections::HashSet<u8>,
    pads: PadBank,
}

impl Injector {
    pub fn new() -> Self {
        Injector {
            held_keys: std::collections::HashSet::new(),
            held_buttons: std::collections::HashSet::new(),
            pads: PadBank::new(),
        }
    }

    pub fn dispatch(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key {
                scancode, pressed, ..
            } => {
                if pressed {
                    self.held_keys.insert(scancode);
                } else {
                    self.held_keys.remove(&scancode);
                }
                key(scancode, pressed);
            }
            InputEvent::PointerAbs { x, y } => mouse_abs(x, y),
            InputEvent::PointerRel { dx, dy } => mouse_rel(dx, dy),
            InputEvent::PointerButton { button, pressed } => {
                if pressed {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
                mouse_button(button, pressed);
            }
            InputEvent::Scroll { dx, dy } => scroll(dx, dy),
            // Gamepad → a ViGEm virtual Xbox 360 pad per slot (issue #2).
            InputEvent::GamepadConnect {
                slot,
                vendor,
                product,
            } => self.pads.connect(slot, vendor, product),
            InputEvent::GamepadDisconnect { slot } => self.pads.disconnect(slot),
            InputEvent::GamepadButton {
                slot,
                button,
                pressed,
            } => self.pads.button(slot, button, pressed),
            InputEvent::GamepadAxis { slot, axis, value } => self.pads.axis(slot, axis, value),
            // Gyro→mouse aim on the Windows host is a future refinement (the
            // Linux compositor's path-2); raw motion has no XUSB representation.
            InputEvent::GamepadMotion { .. } | InputEvent::ControllerTuning { .. } => {}
        }
    }

    /// Release every key and button the remote still holds, and center every
    /// virtual pad. Called when the client disconnects or a new client
    /// preempts the session; the departing client can never send its key-ups
    /// after that point, and replaying a half-open hold into the next session
    /// (or the console user's lap) is always wrong.
    pub fn release_all(&mut self) {
        let n = self.held_keys.len() + self.held_buttons.len();
        for k in std::mem::take(&mut self.held_keys) {
            key(k, false);
        }
        for b in std::mem::take(&mut self.held_buttons) {
            mouse_button(b, false);
        }
        if n > 0 {
            tracing::info!("input: released {n} held keys/buttons on client change");
        }
        self.pads.neutral_all();
    }
}

// ── Virtual Xbox 360 pads (ViGEmBus) ─────────────────────────────────────────

/// Lazily-connected ViGEmBus client + one virtual Xbox 360 pad per wire slot.
/// Missing driver degrades to a warn-once no-op: streaming continues exactly
/// as before issue #2, just without pads.
struct PadBank {
    client: BusSlot,
    pads: std::collections::HashMap<u8, LivePad>,
}

enum BusSlot {
    Untried,
    Dead,
    Live(std::sync::Arc<vigem_client::Client>),
}

struct LivePad {
    target: vigem_client::Xbox360Wired<std::sync::Arc<vigem_client::Client>>,
    state: crate::pads::PadState,
}

impl PadBank {
    fn new() -> Self {
        PadBank {
            client: BusSlot::Untried,
            pads: std::collections::HashMap::new(),
        }
    }

    fn bus(&mut self) -> Option<std::sync::Arc<vigem_client::Client>> {
        if let BusSlot::Untried = self.client {
            self.client = match vigem_client::Client::connect() {
                Ok(c) => BusSlot::Live(std::sync::Arc::new(c)),
                Err(e) => {
                    tracing::warn!(
                        "ViGEmBus unavailable: {e} — gamepad events are ignored. Install the \
                         ViGEmBus driver (github.com/nefarius/ViGEmBus/releases) for controllers."
                    );
                    BusSlot::Dead
                }
            };
        }
        match &self.client {
            BusSlot::Live(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// The pad for `slot`, plugging a fresh virtual pad on first use (so pads
    /// work even if a client skips GamepadConnect).
    fn pad(&mut self, slot: u8) -> Option<&mut LivePad> {
        if !self.pads.contains_key(&slot) {
            let bus = self.bus()?;
            let mut target =
                vigem_client::Xbox360Wired::new(bus, vigem_client::TargetId::XBOX360_WIRED);
            if let Err(e) = target.plugin().and_then(|()| target.wait_ready()) {
                tracing::warn!("virtual pad plugin failed (slot {slot}): {e}");
                return None;
            }
            tracing::info!("virtual Xbox 360 pad plugged for slot {slot}");
            self.pads.insert(
                slot,
                LivePad {
                    target,
                    state: crate::pads::PadState::default(),
                },
            );
        }
        self.pads.get_mut(&slot)
    }

    fn connect(&mut self, slot: u8, vendor: u16, product: u16) {
        if self.pad(slot).is_some() {
            tracing::info!(
                "client controller {vendor:04x}:{product:04x} → virtual pad slot {slot}"
            );
        }
    }

    fn disconnect(&mut self, slot: u8) {
        // Dropping the target unplugs the virtual pad.
        if self.pads.remove(&slot).is_some() {
            tracing::info!("virtual pad slot {slot} unplugged (client controller went away)");
        }
    }

    fn button(&mut self, slot: u8, button: u8, pressed: bool) {
        if let Some(pad) = self.pad(slot) {
            if !pad.state.apply_button(button, pressed) {
                tracing::debug!("unmapped gamepad button index {button}");
                return;
            }
            push(pad);
        }
    }

    fn axis(&mut self, slot: u8, axis: u8, value: f32) {
        if let Some(pad) = self.pad(slot) {
            pad.state.apply_axis(axis, value);
            push(pad);
        }
    }

    /// Center sticks/triggers and release buttons on every pad (client change:
    /// a departed client must not leave a stick deflected in a game). Pads stay
    /// plugged; the next client's GamepadConnect reuses or replaces them.
    fn neutral_all(&mut self) {
        for (slot, pad) in &mut self.pads {
            if pad.state != crate::pads::PadState::default() {
                pad.state.neutral();
                push(pad);
                tracing::info!("virtual pad slot {slot} centered on client change");
            }
        }
    }
}

/// Send the shadow report to the driver. Failures are logged, not fatal: a
/// TDR-style bus hiccup shouldn't kill the input thread.
fn push(pad: &mut LivePad) {
    let report = vigem_client::XGamepad {
        buttons: vigem_client::XButtons {
            raw: pad.state.buttons,
        },
        left_trigger: pad.state.left_trigger,
        right_trigger: pad.state.right_trigger,
        thumb_lx: pad.state.thumb_lx,
        thumb_ly: pad.state.thumb_ly,
        thumb_rx: pad.state.thumb_rx,
        thumb_ry: pad.state.thumb_ry,
    };
    if let Err(e) = pad.target.update(&report) {
        tracing::warn!("virtual pad update failed: {e}");
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
