// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Wire protocol for Pharos <-> Trireme (clean-slate, QUIC transport).
//!
//! See BATTLE_PLAN.md §5. This crate is transport-agnostic: it defines the
//! channel map, the message types, and their encoding. The QUIC plumbing
//! (quinn) lands at M1; here we nail down the contract and a wire codec.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod apps;
pub mod chord;
pub mod controllers;
pub mod latency;
pub mod ports;
pub mod profile;

pub use apps::{decode_app, encode_app, AppInfo, AppMsg};
pub use latency::{
    ClientBitratePref, ClientFpsPref, ClientQos, ClientResPref, ClockPing, ClockPong,
    FrameStageMeta, WireLatencyReport,
};
pub use profile::{ControllerProfile, GyroTuning};

/// Bumped on any breaking wire change. Both ends check this in [`ControlMsg::Hello`].
/// v2: `AppInfo` gained a trailing `running` field + `AppMsg` gained `SwitchTo`/
/// `EndProcess` (positional postcard → a skewed host/client misframes the app
/// catalog), so host + Trireme must be deployed in lockstep.
pub const PROTOCOL_VERSION: u32 = 2;

/// Logical channels, mapped onto QUIC streams/datagrams (BATTLE_PLAN §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channel {
    /// Reliable bidi stream: setup, capability negotiation, config, resume.
    Control,
    /// Reliable ordered stream: input events (must not drop).
    Input,
    /// Datagrams + app FEC: video (unreliable, deadline-aware).
    Video,
    /// Datagrams + app FEC: audio.
    Audio,
    /// One reliable stream per USB device — no cross-device head-of-line block.
    Usb(u32),
}

/// Messages on the control channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// First message each side sends. Carries protocol version for the check.
    Hello {
        protocol_version: u32,
        agent: String,
    },
    /// Stub: capability negotiation (codecs, modes, USB classes) — fleshed M1.
    Capabilities { hevc: bool, av1: bool },
    /// Request a session at a given mode. `ModeChange` may follow mid-session.
    SetupSession {
        width: u32,
        height: u32,
        refresh_mhz: u32,
    },
    /// Live resolution/refresh change (game-readiness invariant #3).
    ModeChange {
        width: u32,
        height: u32,
        refresh_mhz: u32,
    },
    /// Graceful teardown.
    Bye,
}

#[derive(Debug, Error)]
pub enum WireError {
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("decode failed: {0}")]
    Decode(String),
}

/// Encode a control message to bytes (postcard for now; revisit in §12).
pub fn encode(msg: &ControlMsg) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(msg).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a control message from bytes.
pub fn decode(bytes: &[u8]) -> std::result::Result<ControlMsg, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Client→host requested stream mode, carried in the video auth handshake (v3)
/// at connect (and, later, live via a `DG_SET_MODE` datagram). `width`/`height`
/// of 0 = **Auto** (the host keeps its own native canvas resolution).
/// `refresh_mhz` = the client panel's refresh in millihertz (60 Hz = 60_000;
/// 0 = unknown). The host caps its encode fps to `refresh_hz()` and (Tier-3)
/// targets `width`×`height`, scaling to fractions of it under throttle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl StreamMode {
    /// Fixed 12-byte big-endian wire form: width, height, refresh_mhz.
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&self.width.to_be_bytes());
        b[4..8].copy_from_slice(&self.height.to_be_bytes());
        b[8..12].copy_from_slice(&self.refresh_mhz.to_be_bytes());
        b
    }

    pub fn from_bytes(b: &[u8; 12]) -> Self {
        Self {
            width: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            height: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            refresh_mhz: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        }
    }

    /// Panel refresh in whole Hz (rounded), or 0 if unknown. The host caps its
    /// encode fps to this so it never feeds a panel more frames than it shows.
    pub fn refresh_hz(&self) -> u32 {
        self.refresh_mhz.saturating_add(500) / 1000
    }
}

// ---------- Input channel ---------------------------------------------------
//
// Trireme → Pharos input. Carried on a reliable ordered bidi stream within the
// existing video QUIC connection (BATTLE_PLAN §5). Each event is framed as
// `u32 BE length || postcard bytes` — same shape as the video AU framing.

/// Modifier state at the moment of a key event. Tracked client-side from
/// winit's `ModifiersChanged` so the host doesn't have to reconstruct it from
/// a stream of physical key presses (which races on retransmits / dropped
/// stream segments, even though the stream is reliable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// "Meta" = Win on Windows, Super on Linux, Command on macOS.
    pub meta: bool,
}

/// One input event from the Trireme client. The host (Pharos) translates these
/// into compositor seat actions (smithay `keyboard.input` / `pointer.motion`).
///
/// `Copy` was dropped in proto v4 once `ControllerTuning` started carrying an
/// owned `Option<String>` (the controller alias) inside its profile payload.
/// Most variants are still trivially-copyable shapes, but the enum itself must
/// be cloned where it used to be copied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// `scancode` is the Linux evdev code (the host adds the xkb `+8` offset).
    /// Trireme clients on non-Linux translate their native scancode → evdev.
    Key {
        scancode: u32,
        pressed: bool,
        mods: Modifiers,
    },
    /// Pointer position normalised 0..=1 over the source rectangle (Trireme
    /// has already removed the letterbox/window-margin offsets).
    PointerAbs { x: f32, y: f32 },
    /// Relative motion, pixels. For pointer-locked / game-style input.
    PointerRel { dx: f32, dy: f32 },
    /// Button index: 0=Left, 1=Right, 2=Middle, 3=Back, 4=Forward (matches
    /// the evdev BTN_LEFT/BTN_RIGHT/BTN_MIDDLE/BTN_SIDE/BTN_EXTRA order).
    PointerButton { button: u8, pressed: bool },
    /// Wheel deltas in lines/columns (1.0 = one notch). Trireme normalises
    /// platform units (Windows `WHEEL_DELTA=120` → 1.0, Wayland axis_discrete → 1.0).
    Scroll { dx: f32, dy: f32 },
    /// Gamepad button transition. `slot` selects which virtual pad (0-based, for
    /// multiple controllers). `button` is a canonical index (see [`gamepad`]) —
    /// Xbox layout: 0=A 1=B 2=X 3=Y 4=LB 5=RB 6=Back 7=Start 8=Guide 9=LStick
    /// 10=RStick. The host maps it to evdev `BTN_*`; the D-pad rides the hat axes.
    GamepadButton { slot: u8, button: u8, pressed: bool },
    /// Gamepad analog axis. `axis` is a canonical index (see [`gamepad`]):
    /// 0=LeftX 1=LeftY 2=RightX 3=RightY 4=LeftTrigger 5=RightTrigger
    /// 6=HatX 7=HatY. `value` is −1..=1 for sticks/hat (down/right positive,
    /// matching evdev and Android) and 0..=1 for triggers.
    GamepadAxis { slot: u8, axis: u8, value: f32 },
    /// A controller appeared on the client. The host classifies it by VID:PID
    /// (for the log/selector) and ensures a virtual Xbox 360 pad exists for
    /// `slot`. The client keeps the device name locally for its own selector.
    GamepadConnect { slot: u8, vendor: u16, product: u16 },
    /// A controller went away; the host drops the pad for `slot`.
    GamepadDisconnect { slot: u8 },
    /// Motion-sensor sample for gyro-aim: gyroscope (rad/s — `gx`=pitch,
    /// `gy`=yaw, `gz`=roll) + accelerometer (g). The host converts gyro into
    /// relative mouse motion. Accel is forwarded for future tilt/levelling use.
    GamepadMotion {
        slot: u8,
        gx: f32,
        gy: f32,
        gz: f32,
        ax: f32,
        ay: f32,
        az: f32,
    },
    /// Live controller tuning relayed from the client's settings UI: face swaps
    /// and gyro feel for the controller on `slot`. The host applies it immediately
    /// (pad swap + IMU reader) and persists it keyed by `vendor:product`, so the
    /// feel survives reconnects and client reinstalls. `vendor`/`product` carry
    /// the controller id so the host can key the saved profile directly.
    ControllerTuning {
        slot: u8,
        vendor: u16,
        product: u16,
        profile: ControllerProfile,
    },
}

/// Canonical gamepad index space, shared by every Trireme client and the host's
/// virtual-pad injector. Clients translate their platform codes (Android
/// `KEYCODE_BUTTON_*` / `MotionEvent` axes, evdev, …) into these; the host
/// translates these into evdev `BTN_*` / `ABS_*` on the virtual Xbox 360 pad.
pub mod gamepad {
    // Buttons (→ InputEvent::GamepadButton.button).
    pub const BTN_A: u8 = 0;
    pub const BTN_B: u8 = 1;
    pub const BTN_X: u8 = 2;
    pub const BTN_Y: u8 = 3;
    pub const BTN_LB: u8 = 4;
    pub const BTN_RB: u8 = 5;
    pub const BTN_BACK: u8 = 6;
    pub const BTN_START: u8 = 7;
    pub const BTN_GUIDE: u8 = 8;
    pub const BTN_LSTICK: u8 = 9;
    pub const BTN_RSTICK: u8 = 10;

    // Axes (→ InputEvent::GamepadAxis.axis).
    pub const AXIS_LEFT_X: u8 = 0;
    pub const AXIS_LEFT_Y: u8 = 1;
    pub const AXIS_RIGHT_X: u8 = 2;
    pub const AXIS_RIGHT_Y: u8 = 3;
    pub const AXIS_LTRIGGER: u8 = 4;
    pub const AXIS_RTRIGGER: u8 = 5;
    pub const AXIS_HAT_X: u8 = 6;
    pub const AXIS_HAT_Y: u8 = 7;
}

pub fn encode_input(ev: &InputEvent) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(ev).map_err(|e| WireError::Encode(e.to_string()))
}

pub fn decode_input(bytes: &[u8]) -> std::result::Result<InputEvent, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let msg = ControlMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            agent: "pharos/0.0.0".into(),
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn mode_change_roundtrip() {
        let msg = ControlMsg::ModeChange {
            width: 3840,
            height: 2160,
            refresh_mhz: 120_000,
        };
        assert_eq!(decode(&encode(&msg).unwrap()).unwrap(), msg);
    }

    #[test]
    fn input_event_roundtrip_key() {
        let ev = InputEvent::Key {
            // KEY_A in evdev (the host will add +8 for xkb).
            scancode: 30,
            pressed: true,
            mods: Modifiers {
                ctrl: true,
                shift: false,
                alt: false,
                meta: false,
            },
        };
        let bytes = encode_input(&ev).unwrap();
        assert_eq!(decode_input(&bytes).unwrap(), ev);
    }

    #[test]
    fn input_event_roundtrip_controller_tuning() {
        let mut profile = ControllerProfile {
            swap_ab: true,
            swap_xy: false,
            ..Default::default()
        };
        profile.gyro.sens_x = 18.5;
        profile.gyro.invert_y = true;
        profile.gyro.axis_h = 0;
        let ev = InputEvent::ControllerTuning {
            slot: 1,
            vendor: 0x057e,
            product: 0x2009,
            profile,
        };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
    }

    #[test]
    fn input_event_roundtrip_pointer() {
        let ev = InputEvent::PointerAbs { x: 0.5, y: 0.25 };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
        let ev = InputEvent::PointerButton {
            button: 0,
            pressed: true,
        };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
        let ev = InputEvent::Scroll { dx: 0.0, dy: -1.0 };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
    }

    #[test]
    fn input_event_roundtrip_gamepad() {
        let ev = InputEvent::GamepadButton {
            slot: 0,
            button: gamepad::BTN_A,
            pressed: true,
        };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
        let ev = InputEvent::GamepadAxis {
            slot: 1,
            axis: gamepad::AXIS_LEFT_X,
            value: -0.75,
        };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
        let ev = InputEvent::GamepadConnect {
            slot: 0,
            vendor: 0x045e,
            product: 0x028e,
        };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
        let ev = InputEvent::GamepadMotion {
            slot: 0,
            gx: 0.1,
            gy: -0.2,
            gz: 0.0,
            ax: 0.0,
            ay: 1.0,
            az: 0.0,
        };
        assert_eq!(decode_input(&encode_input(&ev).unwrap()).unwrap(), ev);
    }
}
