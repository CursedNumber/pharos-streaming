// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Pure gamepad state: canonical wire events (`core_proto::gamepad`) → an
//! XUSB (Xbox 360) report image (issue #2).
//!
//! This module is deliberately free of ViGEm types so the mapping logic
//! compiles and unit-tests on the Linux dev box; the ViGEmBus plumbing that
//! turns a [`PadState`] into a live virtual pad lives in `input.rs` (Windows
//! only). Wire conventions (see `core_proto`): sticks and hat are −1..=1 with
//! DOWN/RIGHT positive (evdev/Android); triggers are 0..=1. XUSB thumbsticks
//! are UP-positive, so the Y axes invert here; the D-pad is button bits fed
//! from the hat axes.

/// XUSB `wButtons` bits (the Xbox 360 wire report; ViGEm consumes these).
pub const XUSB_UP: u16 = 0x0001;
pub const XUSB_DOWN: u16 = 0x0002;
pub const XUSB_LEFT: u16 = 0x0004;
pub const XUSB_RIGHT: u16 = 0x0008;
pub const XUSB_START: u16 = 0x0010;
pub const XUSB_BACK: u16 = 0x0020;
pub const XUSB_LTHUMB: u16 = 0x0040;
pub const XUSB_RTHUMB: u16 = 0x0080;
pub const XUSB_LB: u16 = 0x0100;
pub const XUSB_RB: u16 = 0x0200;
pub const XUSB_GUIDE: u16 = 0x0400;
pub const XUSB_A: u16 = 0x1000;
pub const XUSB_B: u16 = 0x2000;
pub const XUSB_X: u16 = 0x4000;
pub const XUSB_Y: u16 = 0x8000;

/// Canonical button index (`core_proto::gamepad::BTN_*`) → XUSB bit.
/// `None` for indices this layout has no button for.
pub fn button_bit(button: u8) -> Option<u16> {
    use core_proto::gamepad as g;
    Some(match button {
        g::BTN_A => XUSB_A,
        g::BTN_B => XUSB_B,
        g::BTN_X => XUSB_X,
        g::BTN_Y => XUSB_Y,
        g::BTN_LB => XUSB_LB,
        g::BTN_RB => XUSB_RB,
        g::BTN_BACK => XUSB_BACK,
        g::BTN_START => XUSB_START,
        g::BTN_GUIDE => XUSB_GUIDE,
        g::BTN_LSTICK => XUSB_LTHUMB,
        g::BTN_RSTICK => XUSB_RTHUMB,
        _ => return None,
    })
}

/// Shadow of one virtual pad's XUSB report. Field layout mirrors
/// `XUSB_REPORT`; `input.rs` copies it verbatim into ViGEm's `XGamepad`.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct PadState {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

fn stick(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn trigger(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl PadState {
    /// Apply a canonical button event. Returns false for unknown indices
    /// (caller logs once; the report is unchanged).
    pub fn apply_button(&mut self, button: u8, pressed: bool) -> bool {
        let Some(bit) = button_bit(button) else {
            return false;
        };
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        true
    }

    /// Apply a canonical axis event. Sticks invert Y (wire is DOWN-positive,
    /// XUSB is UP-positive); the hat becomes D-pad bits with the same
    /// round-to-digital the Linux uinput pad uses. Unknown axes are ignored.
    pub fn apply_axis(&mut self, axis: u8, value: f32) {
        use core_proto::gamepad as g;
        match axis {
            g::AXIS_LEFT_X => self.thumb_lx = stick(value),
            g::AXIS_LEFT_Y => self.thumb_ly = stick(-value),
            g::AXIS_RIGHT_X => self.thumb_rx = stick(value),
            g::AXIS_RIGHT_Y => self.thumb_ry = stick(-value),
            g::AXIS_LTRIGGER => self.left_trigger = trigger(value),
            g::AXIS_RTRIGGER => self.right_trigger = trigger(value),
            g::AXIS_HAT_X => {
                self.buttons &= !(XUSB_LEFT | XUSB_RIGHT);
                match value.round() as i32 {
                    v if v < 0 => self.buttons |= XUSB_LEFT,
                    v if v > 0 => self.buttons |= XUSB_RIGHT,
                    _ => {}
                }
            }
            g::AXIS_HAT_Y => {
                self.buttons &= !(XUSB_UP | XUSB_DOWN);
                match value.round() as i32 {
                    v if v < 0 => self.buttons |= XUSB_UP,
                    v if v > 0 => self.buttons |= XUSB_DOWN,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Everything released and centered; used on client change so a departed
    /// client cannot leave a stick deflected or a trigger held in a game.
    pub fn neutral(&mut self) {
        *self = PadState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_proto::gamepad as g;

    #[test]
    fn buttons_map_and_clear() {
        let mut p = PadState::default();
        assert!(p.apply_button(g::BTN_A, true));
        assert!(p.apply_button(g::BTN_GUIDE, true));
        assert_eq!(p.buttons, XUSB_A | XUSB_GUIDE);
        assert!(p.apply_button(g::BTN_A, false));
        assert_eq!(p.buttons, XUSB_GUIDE);
        assert!(!p.apply_button(99, true)); // unknown index → untouched
        assert_eq!(p.buttons, XUSB_GUIDE);
    }

    #[test]
    fn stick_y_inverts_and_scales() {
        let mut p = PadState::default();
        // Wire: down-positive. Pushing the stick UP (wire −1) must be XUSB +32767.
        p.apply_axis(g::AXIS_LEFT_Y, -1.0);
        assert_eq!(p.thumb_ly, 32767);
        p.apply_axis(g::AXIS_LEFT_Y, 1.0);
        assert_eq!(p.thumb_ly, -32767);
        // X does not invert.
        p.apply_axis(g::AXIS_LEFT_X, 1.0);
        assert_eq!(p.thumb_lx, 32767);
        // Out-of-range clamps.
        p.apply_axis(g::AXIS_RIGHT_X, 2.0);
        assert_eq!(p.thumb_rx, 32767);
    }

    #[test]
    fn triggers_scale_0_255() {
        let mut p = PadState::default();
        p.apply_axis(g::AXIS_LTRIGGER, 1.0);
        p.apply_axis(g::AXIS_RTRIGGER, 0.5);
        assert_eq!(p.left_trigger, 255);
        assert_eq!(p.right_trigger, 128);
    }

    #[test]
    fn hat_becomes_dpad_bits() {
        let mut p = PadState::default();
        p.apply_axis(g::AXIS_HAT_X, 1.0);
        p.apply_axis(g::AXIS_HAT_Y, -1.0); // wire up = negative
        assert_eq!(p.buttons, XUSB_RIGHT | XUSB_UP);
        p.apply_axis(g::AXIS_HAT_X, 0.0);
        assert_eq!(p.buttons, XUSB_UP);
        p.apply_axis(g::AXIS_HAT_Y, 1.0);
        assert_eq!(p.buttons, XUSB_DOWN);
    }

    #[test]
    fn neutral_clears_everything() {
        let mut p = PadState::default();
        p.apply_button(g::BTN_B, true);
        p.apply_axis(g::AXIS_LTRIGGER, 1.0);
        p.apply_axis(g::AXIS_LEFT_X, -0.7);
        p.neutral();
        assert_eq!(p, PadState::default());
    }
}
