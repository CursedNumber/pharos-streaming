// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Pure input-translation logic shared by the FFI loop: pointer-button mapping,
//! the held-button mask accumulator, and absolute-coordinate scaling. Kept
//! FFI-free so it compiles and is unit-tested without `spice-gtk`.
//!
//! SPICE button *numbers* and button-mask *bits* are NOT the same indexing:
//! e.g. Right is button `3` but mask bit `1<<2`. We keep a single lookup so the
//! two can never drift, mirroring the VNC module's `PointerState` discipline.
//! Values are from `spice/enums.h` (`SpiceMouseButton` / `SpiceMouseButtonMask`).

// SpiceMouseButton (1-based).
pub const SPICE_MOUSE_BUTTON_LEFT: i32 = 1;
pub const SPICE_MOUSE_BUTTON_MIDDLE: i32 = 2;
pub const SPICE_MOUSE_BUTTON_RIGHT: i32 = 3;
pub const SPICE_MOUSE_BUTTON_UP: i32 = 4; // wheel up
pub const SPICE_MOUSE_BUTTON_DOWN: i32 = 5; // wheel down
pub const SPICE_MOUSE_BUTTON_SIDE: i32 = 6; // back
pub const SPICE_MOUSE_BUTTON_EXTRA: i32 = 7; // forward

// SpiceMouseButtonMask bits (note: UP/DOWN wheel masks 1<<3/1<<4 are momentary
// and never held, so they're not part of the persistent mask).
const MASK_LEFT: i32 = 1 << 0;
const MASK_MIDDLE: i32 = 1 << 1;
const MASK_RIGHT: i32 = 1 << 2;
const MASK_SIDE: i32 = 1 << 5;
const MASK_EXTRA: i32 = 1 << 6;

/// `core_proto::InputEvent::PointerButton.button` (0=Left,1=Right,2=Middle,
/// 3=Back,4=Forward) → `(SPICE button number, persistent mask bit)`.
pub fn map_button(button: u8) -> Option<(i32, i32)> {
    match button {
        0 => Some((SPICE_MOUSE_BUTTON_LEFT, MASK_LEFT)),
        1 => Some((SPICE_MOUSE_BUTTON_RIGHT, MASK_RIGHT)),
        2 => Some((SPICE_MOUSE_BUTTON_MIDDLE, MASK_MIDDLE)),
        3 => Some((SPICE_MOUSE_BUTTON_SIDE, MASK_SIDE)),
        4 => Some((SPICE_MOUSE_BUTTON_EXTRA, MASK_EXTRA)),
        _ => None,
    }
}

/// Fold a press/release into the held-button mask. Returns the new mask and the
/// SPICE button number to send, or `None` for an unmapped button. Releasing an
/// unheld button is idempotent (the mask never goes negative).
pub fn apply_button(mask: i32, button: u8, pressed: bool) -> Option<(i32, i32)> {
    let (spice_btn, bit) = map_button(button)?;
    let new_mask = if pressed { mask | bit } else { mask & !bit };
    Some((new_mask, spice_btn))
}

/// Normalised 0..=1 position over the surface → integer pixel coordinate,
/// clamped to `[0, dim-1]`. A zero/negative dimension maps to 0.
pub fn abs_to_pixel(norm: f32, dim: i32) -> i32 {
    if dim <= 1 {
        return 0;
    }
    (norm.clamp(0.0, 1.0) * (dim - 1) as f32).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_numbers_and_mask_bits_match_table() {
        assert_eq!(map_button(0), Some((1, 1)), "Left → btn 1 / mask 1");
        assert_eq!(map_button(1), Some((3, 4)), "Right → btn 3 / mask bit 1<<2");
        assert_eq!(
            map_button(2),
            Some((2, 2)),
            "Middle → btn 2 / mask bit 1<<1"
        );
        assert_eq!(map_button(3), Some((6, 32)), "Back → btn 6 / mask bit 1<<5");
        assert_eq!(
            map_button(4),
            Some((7, 64)),
            "Forward → btn 7 / mask bit 1<<6"
        );
        assert_eq!(map_button(5), None, "unmapped");
    }

    #[test]
    fn button_index_is_not_one_shl_button() {
        // Regression guard: Right is button 3 (not 1<<1) and mask 4 (not 1<<1).
        let (btn, mask) = map_button(1).unwrap();
        assert_ne!(btn, 1 << 1);
        assert_eq!((btn, mask), (3, 4));
    }

    #[test]
    fn mask_accumulates_and_clears() {
        let mut m = 0;
        // Press Left.
        let (nm, b) = apply_button(m, 0, true).unwrap();
        assert_eq!((nm, b), (1, 1));
        m = nm;
        // Press Right while Left held → mask 1|4 = 5, button 3.
        let (nm, b) = apply_button(m, 1, true).unwrap();
        assert_eq!((nm, b), (5, 3));
        m = nm;
        // Release Left → mask 4.
        let (nm, _) = apply_button(m, 0, false).unwrap();
        assert_eq!(nm, 4);
        m = nm;
        // Release Right → mask 0.
        let (nm, _) = apply_button(m, 1, false).unwrap();
        assert_eq!(nm, 0);
    }

    #[test]
    fn release_of_unheld_button_is_idempotent() {
        let (nm, _) = apply_button(0, 2, false).unwrap();
        assert_eq!(nm, 0, "releasing unheld Middle leaves mask 0");
        assert!(nm >= 0);
    }

    #[test]
    fn abs_scaling_clamps_and_rounds() {
        assert_eq!(abs_to_pixel(0.0, 1920), 0);
        assert_eq!(abs_to_pixel(1.0, 1920), 1919);
        assert_eq!(abs_to_pixel(0.5, 1920), 960); // 0.5*1919 = 959.5 → 960
        assert_eq!(abs_to_pixel(-1.0, 1920), 0, "clamps low");
        assert_eq!(abs_to_pixel(2.0, 1920), 1919, "clamps high");
        assert_eq!(abs_to_pixel(0.5, 1), 0, "degenerate dim");
        assert_eq!(abs_to_pixel(0.5, 0), 0, "zero dim");
    }
}
