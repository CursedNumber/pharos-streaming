//! Linux evdev keycode → SPICE wire scancode (PC "XT set-1" make codes).
//!
//! `spice_inputs_channel_key_press` expects an XT set-1 make code. For the
//! contiguous range `1..=83` the evdev keycodes are *historically identical* to
//! the XT set-1 make codes (the kernel's i8042/atkbd path inherited them), so
//! those map 1:1. Extended keys (arrows, nav cluster, right-side modifiers, the
//! keypad Enter/Divide, Super, Menu) are the 0xE0-prefixed XT codes; SPICE
//! encodes the prefix into the scancode as `low_byte | 0x100`, which is the
//! value this table stores directly (so right-ctrl `0xE01D` → `0x11D`).
//!
//! This is the inverse companion of `trireme::vncsrc::evdev_to_keysym` and
//! covers *exactly* the set of evdev codes the player can emit (see the
//! `keycode_to_evdev` table in `trireme::input`).

/// `0x100` ORed into a scancode marks an 0xE0-prefixed (extended) XT key.
pub const SPICE_SCANCODE_EXTENDED: u32 = 0x100;

/// Map a Linux evdev keycode to the SPICE wire scancode, or `None` for codes
/// the player never emits / that have no XT equivalent.
pub fn evdev_to_xt(evdev: u32) -> Option<u32> {
    let v: u32 = match evdev {
        // --- 1..=83: evdev == XT set-1 make code (1:1) ---
        1..=83 => evdev,
        // --- F11/F12 (the 84..=86 gap is KP_Equal/intl keys we don't emit) ---
        87 => 0x57, // F11
        88 => 0x58, // F12
        // --- extended 0xE0xx keys: the bare XT make code | the extended bit ---
        96 => 0x1c | SPICE_SCANCODE_EXTENDED, // KP_Enter    (0xE01C)
        97 => 0x1d | SPICE_SCANCODE_EXTENDED, // Right_Ctrl  (0xE01D)
        98 => 0x35 | SPICE_SCANCODE_EXTENDED, // KP_Divide   (0xE035)
        99 => 0x37 | SPICE_SCANCODE_EXTENDED, // Print/SysRq (0xE037; single-make, QEMU-ok)
        100 => 0x38 | SPICE_SCANCODE_EXTENDED, // Right_Alt   (0xE038)
        102 => 0x47 | SPICE_SCANCODE_EXTENDED, // Home        (0xE047)
        103 => 0x48 | SPICE_SCANCODE_EXTENDED, // Up          (0xE048)
        104 => 0x49 | SPICE_SCANCODE_EXTENDED, // Page_Up     (0xE049)
        105 => 0x4b | SPICE_SCANCODE_EXTENDED, // Left        (0xE04B)
        106 => 0x4d | SPICE_SCANCODE_EXTENDED, // Right       (0xE04D)
        107 => 0x4f | SPICE_SCANCODE_EXTENDED, // End         (0xE04F)
        108 => 0x50 | SPICE_SCANCODE_EXTENDED, // Down        (0xE050)
        109 => 0x51 | SPICE_SCANCODE_EXTENDED, // Page_Down   (0xE051)
        110 => 0x52 | SPICE_SCANCODE_EXTENDED, // Insert      (0xE052)
        111 => 0x53 | SPICE_SCANCODE_EXTENDED, // Delete      (0xE053)
        119 => 0x46 | SPICE_SCANCODE_EXTENDED, // Pause       (0xE046; single-make, QEMU-ok)
        125 => 0x5b | SPICE_SCANCODE_EXTENDED, // Left_Super  (0xE05B)
        126 => 0x5c | SPICE_SCANCODE_EXTENDED, // Right_Super (0xE05C)
        127 => 0x5d | SPICE_SCANCODE_EXTENDED, // Menu        (0xE05D)
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly the evdev codes the player can emit (kept in sync with
    /// `trireme::vncsrc`'s `emitted_evdev_codes`). If `trireme::input` grows a
    /// key, this list — and the table above — must grow with it.
    fn emitted_evdev_codes() -> Vec<u32> {
        let mut v: Vec<u32> = (1..=83).collect();
        v.extend([87, 88]);
        v.extend(96..=100);
        v.extend(102..=111);
        v.extend([119, 125, 126, 127]);
        v
    }

    #[test]
    fn every_emitted_code_maps() {
        for code in emitted_evdev_codes() {
            assert!(
                evdev_to_xt(code).is_some(),
                "evdev {code} (emitted by the player) has no XT scancode"
            );
        }
    }

    #[test]
    fn known_gaps_are_unmapped() {
        // Codes the player never emits / have no XT make code in our set.
        for code in [0u32, 84, 85, 86, 89, 90, 95, 101, 112, 120, 200] {
            assert_eq!(evdev_to_xt(code), None, "evdev {code} should be unmapped");
        }
    }

    #[test]
    fn base_keys_have_no_extended_bit() {
        for code in 1..=88u32 {
            if let Some(v) = evdev_to_xt(code) {
                assert_eq!(
                    v & SPICE_SCANCODE_EXTENDED,
                    0,
                    "evdev {code} is not extended"
                );
            }
        }
    }

    #[test]
    fn extended_keys_set_the_extended_bit() {
        let extended = [
            96u32, 97, 98, 99, 100, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 119, 125,
            126, 127,
        ];
        for code in extended {
            let v = evdev_to_xt(code).unwrap();
            assert_ne!(
                v & SPICE_SCANCODE_EXTENDED,
                0,
                "evdev {code} must be extended"
            );
            // The low byte is the bare XT make code (0xE0 prefix stripped).
            assert!(v & 0xff <= 0x5d, "evdev {code} low byte out of XT range");
        }
    }

    #[test]
    fn anchor_scancodes() {
        assert_eq!(evdev_to_xt(1), Some(0x01), "Escape");
        assert_eq!(evdev_to_xt(2), Some(0x02), "1");
        assert_eq!(evdev_to_xt(28), Some(0x1c), "Enter");
        assert_eq!(evdev_to_xt(30), Some(0x1e), "a");
        assert_eq!(evdev_to_xt(42), Some(0x2a), "Left_Shift");
        assert_eq!(evdev_to_xt(57), Some(0x39), "Space");
        assert_eq!(evdev_to_xt(59), Some(0x3b), "F1");
        assert_eq!(evdev_to_xt(88), Some(0x58), "F12");
        assert_eq!(evdev_to_xt(96), Some(0x11c), "KP_Enter (extended)");
        assert_eq!(evdev_to_xt(97), Some(0x11d), "Right_Ctrl (extended)");
        assert_eq!(evdev_to_xt(103), Some(0x148), "Up (extended)");
        assert_eq!(evdev_to_xt(111), Some(0x153), "Delete (extended)");
        assert_eq!(evdev_to_xt(125), Some(0x15b), "Left_Super (extended)");
    }
}
