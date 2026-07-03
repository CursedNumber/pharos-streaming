// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Game-controller classifier — shared by the host and every Trireme client.
//!
//! Identifies a controller model from its USB VID:PID (+ a name fallback for
//! pads that mode-switch and spoof another vendor's IDs — 8BitDo and Flydigi
//! commonly present as an Xbox pad in XInput mode). The result drives the
//! client's controller selector UI and per-model handling; the *presentation*
//! to the game is always a wired Xbox 360 (universal XInput), so this is about
//! identification + capabilities (gyro, touchpad, paddles), not the virtual
//! device layout.
//!
//! `has_gyro` here is a per-model *hint*; the actual sensor availability is
//! confirmed on the client (Android exposes a controller's gyro only when the
//! platform binds its sensors).

use serde::{Deserialize, Serialize};

/// A recognised controller family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerType {
    Xbox360,
    XboxOne, // One / Series S|X
    XboxElite,
    DualShock3,
    DualShock4,
    DualSense,
    SwitchPro,
    JoyCon,
    EightBitDo,
    Flydigi,
    Stadia,
    SteamController, // Steam Controller / Steam Deck
    Generic,
}

impl ControllerType {
    /// Human label for the selector UI.
    pub fn label(self) -> &'static str {
        match self {
            ControllerType::Xbox360 => "Xbox 360",
            ControllerType::XboxOne => "Xbox One/Series",
            ControllerType::XboxElite => "Xbox Elite",
            ControllerType::DualShock3 => "DualShock 3",
            ControllerType::DualShock4 => "DualShock 4",
            ControllerType::DualSense => "DualSense",
            ControllerType::SwitchPro => "Switch Pro",
            ControllerType::JoyCon => "Joy-Con",
            ControllerType::EightBitDo => "8BitDo",
            ControllerType::Flydigi => "Flydigi",
            ControllerType::Stadia => "Stadia",
            ControllerType::SteamController => "Steam Controller",
            ControllerType::Generic => "Generic gamepad",
        }
    }
}

/// What a controller can do, beyond the standard buttons/sticks/triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerCaps {
    /// Has a gyroscope (gyro→mouse is available). A *hint* — confirmed client-side.
    pub gyro: bool,
    /// Has a touchpad (DualShock 4 / DualSense).
    pub touchpad: bool,
    /// Has back paddles / extra buttons (Elite, DualSense Edge, Flydigi, some 8BitDo).
    pub paddles: bool,
    /// Has rumble / haptics.
    pub rumble: bool,
}

/// Classification result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerInfo {
    pub kind: ControllerType,
    pub caps: ControllerCaps,
}

impl ControllerInfo {
    fn new(kind: ControllerType, gyro: bool, touchpad: bool, paddles: bool) -> Self {
        ControllerInfo {
            kind,
            caps: ControllerCaps {
                gyro,
                touchpad,
                paddles,
                rumble: true,
            },
        }
    }
}

const VID_MICROSOFT: u16 = 0x045e;
const VID_SONY: u16 = 0x054c;
const VID_NINTENDO: u16 = 0x057e;
const VID_EIGHTBITDO: u16 = 0x2dc8;
const VID_GOOGLE: u16 = 0x18d1;
const VID_VALVE: u16 = 0x28de;

/// Classify a controller from its USB ids and reported name.
///
/// Name takes priority for the mode-switching brands (8BitDo / Flydigi), which
/// borrow Microsoft/Sony/Nintendo VIDs in their compatibility modes — the VID
/// alone would mislabel them as a first-party pad.
pub fn classify(vendor: u16, product: u16, name: &str) -> ControllerInfo {
    let lname = name.to_ascii_lowercase();

    // Name-first for the spoofers.
    if lname.contains("8bitdo") || lname.contains("8 bitdo") {
        // Most 8BitDo pads have no gyro; a few do — confirmed client-side.
        return ControllerInfo::new(ControllerType::EightBitDo, false, false, false);
    }
    if lname.contains("flydigi") {
        return ControllerInfo::new(ControllerType::Flydigi, true, false, true);
    }

    match vendor {
        VID_MICROSOFT => match product {
            0x028e | 0x028f | 0x0719 => {
                ControllerInfo::new(ControllerType::Xbox360, false, false, false)
            }
            // Elite (1st gen 0x02e3) / Elite Series 2 (0x0b00, 0x0b05, 0x0b22).
            0x02e3 | 0x0b00 | 0x0b05 | 0x0b22 => {
                ControllerInfo::new(ControllerType::XboxElite, false, false, true)
            }
            _ => ControllerInfo::new(ControllerType::XboxOne, false, false, false),
        },
        VID_SONY => match product {
            0x0268 => ControllerInfo::new(ControllerType::DualShock3, false, false, false),
            0x05c4 | 0x09cc | 0x0ba0 => {
                ControllerInfo::new(ControllerType::DualShock4, true, true, false)
            }
            0x0ce6 | 0x0df2 => ControllerInfo::new(ControllerType::DualSense, true, true, false),
            _ => ControllerInfo::new(ControllerType::DualShock4, true, true, false),
        },
        VID_NINTENDO => match product {
            0x2009 => ControllerInfo::new(ControllerType::SwitchPro, true, false, false),
            0x2006 | 0x2007 | 0x200e => {
                ControllerInfo::new(ControllerType::JoyCon, true, false, false)
            }
            _ => ControllerInfo::new(ControllerType::SwitchPro, true, false, false),
        },
        VID_EIGHTBITDO => ControllerInfo::new(ControllerType::EightBitDo, false, false, false),
        VID_GOOGLE if product == 0x9400 => {
            ControllerInfo::new(ControllerType::Stadia, false, false, false)
        }
        VID_VALVE => ControllerInfo::new(ControllerType::SteamController, true, true, true),
        _ => ControllerInfo::new(ControllerType::Generic, false, false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_party_by_vidpid() {
        assert_eq!(classify(0x045e, 0x028e, "").kind, ControllerType::Xbox360);
        assert_eq!(classify(0x045e, 0x0b12, "").kind, ControllerType::XboxOne);
        assert_eq!(classify(0x045e, 0x0b00, "").kind, ControllerType::XboxElite);
        assert_eq!(
            classify(0x054c, 0x09cc, "").kind,
            ControllerType::DualShock4
        );
        assert_eq!(classify(0x054c, 0x0ce6, "").kind, ControllerType::DualSense);
        assert_eq!(classify(0x057e, 0x2009, "").kind, ControllerType::SwitchPro);
    }

    #[test]
    fn gyro_caps() {
        assert!(classify(0x054c, 0x0ce6, "").caps.gyro); // DualSense
        assert!(classify(0x057e, 0x2009, "").caps.gyro); // Switch Pro
        assert!(classify(0x054c, 0x09cc, "").caps.touchpad); // DS4 touchpad
        assert!(!classify(0x045e, 0x028e, "").caps.gyro); // Xbox 360, no gyro
        assert!(classify(0x045e, 0x0b00, "").caps.paddles); // Elite paddles
    }

    #[test]
    fn name_overrides_spoofed_vid() {
        // 8BitDo in XInput mode reports Microsoft's VID — the name saves us.
        let i = classify(0x045e, 0x028e, "8BitDo Pro 2");
        assert_eq!(i.kind, ControllerType::EightBitDo);
        // Flydigi likewise; it has gyro.
        let f = classify(0x045e, 0x028e, "Flydigi Vader 3");
        assert_eq!(f.kind, ControllerType::Flydigi);
        assert!(f.caps.gyro);
    }

    #[test]
    fn unknown_is_generic() {
        assert_eq!(
            classify(0x1234, 0x5678, "Mystery Pad").kind,
            ControllerType::Generic
        );
    }
}
