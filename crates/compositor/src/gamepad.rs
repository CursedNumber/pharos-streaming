// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Virtual Xbox 360 gamepad on the host.
//!
//! A Trireme client (e.g. the Bravia) captures its local controller and forwards
//! canonical [`core_proto::InputEvent::GamepadButton`] / `GamepadAxis` events
//! over the QUIC input back-channel. We replay them on a **uinput** virtual
//! device the game reads through evdev — the lowest-latency couch-gaming path
//! (the "Moonlight model"), same approach as the standalone `gamepad-bridge`.
//!
//! We present a *wired Xbox 360* (`045e:028e`, USB) with the exact button/axis
//! codes the kernel `xpad` driver reports, so SDL/Proton's controller DB maps it
//! correctly. Only gamepad buttons (`0x130..=0x13e`) are registered so udev's
//! `input_id` classifies it as `ID_INPUT_JOYSTICK` (a stray keyboard key makes
//! the pad invisible to games — see `gamepad-bridge`). The D-pad rides the hat
//! axes, like a real 360 pad.

use anyhow::{Context, Result};
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsInfo, AbsoluteAxisType, AttributeSet, BusType, EventType, InputEvent, InputId, Key,
    UinputAbsSetup,
};

// evdev event types (raw, matching gamepad-bridge's proven usage).
const EV_SYN: EventType = EventType(0);
const EV_KEY: EventType = EventType(1);
const EV_ABS: EventType = EventType(3);

// Absolute-axis evdev codes.
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

/// Canonical gamepad button index → evdev `BTN_*` code, matching what the kernel
/// `xpad` driver reports for a wired 360 pad (so SDL's `045e:028e` mapping fits).
fn button_code(idx: u8) -> Option<u16> {
    Some(match idx {
        0 => 0x130,  // BTN_SOUTH (A)
        1 => 0x131,  // BTN_EAST  (B)
        2 => 0x133,  // BTN_NORTH (X)
        3 => 0x134,  // BTN_WEST  (Y)
        4 => 0x136,  // BTN_TL    (LB)
        5 => 0x137,  // BTN_TR    (RB)
        6 => 0x13a,  // BTN_SELECT (Back/View)
        7 => 0x13b,  // BTN_START  (Start/Menu)
        8 => 0x13c,  // BTN_MODE   (Guide)
        9 => 0x13d,  // BTN_THUMBL (left stick click)
        10 => 0x13e, // BTN_THUMBR (right stick click)
        _ => return None,
    })
}

/// One uinput virtual Xbox 360 pad.
pub struct VirtualGamepad {
    dev: VirtualDevice,
}

impl VirtualGamepad {
    fn new(slot: u8) -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();
        for i in 0..=10u8 {
            if let Some(code) = button_code(i) {
                keys.insert(Key::new(code));
            }
        }

        // (code, min, max): sticks full int16, triggers 0..255, hat -1..1 — the
        // standard wired-360 ranges.
        let axes: [(u16, i32, i32); 8] = [
            (ABS_X, -32768, 32767),
            (ABS_Y, -32768, 32767),
            (ABS_RX, -32768, 32767),
            (ABS_RY, -32768, 32767),
            (ABS_Z, 0, 255),
            (ABS_RZ, 0, 255),
            (ABS_HAT0X, -1, 1),
            (ABS_HAT0Y, -1, 1),
        ];

        let name = format!("Trireme Xbox 360 pad {slot}");
        let mut builder = VirtualDeviceBuilder::new()
            .context("open /dev/uinput (needs the `input` group or a udev rule)")?
            .name(&name)
            .input_id(InputId::new(BusType(0x03), 0x045e, 0x028e, 0x0110))
            .with_keys(&keys)?;
        for (code, min, max) in axes {
            let info = AbsInfo::new(0, min, max, 0, 0, 0);
            builder =
                builder.with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType(code), info))?;
        }
        Ok(Self {
            dev: builder.build()?,
        })
    }

    fn emit(&mut self, ev: InputEvent) {
        // One SYN_REPORT after each event so readers see a complete frame.
        if let Err(e) = self.dev.emit(&[ev, InputEvent::new(EV_SYN, 0, 0)]) {
            tracing::debug!("uinput emit failed: {e}");
        }
    }

    /// Press/release a canonical button index (0=A..10=RStick).
    pub fn button(&mut self, button: u8, pressed: bool) {
        if let Some(code) = button_code(button) {
            self.emit(InputEvent::new(EV_KEY, code, i32::from(pressed)));
        }
    }

    /// Set a canonical analog axis to a normalised value.
    pub fn axis(&mut self, axis: u8, value: f32) {
        if let Some((code, v)) = axis_value(axis, value) {
            self.emit(InputEvent::new(EV_ABS, code, v));
        }
    }
}

/// One virtual pad per controller slot + the shared gyro→mouse converter. Pads
/// are created lazily (on Connect or the first event for a slot) so a stream
/// with no controller never touches /dev/uinput.
pub struct PadManager {
    pads: std::collections::HashMap<u8, VirtualGamepad>,
    pub gyro: GyroMouse,
    /// Per-slot `(swap_ab, swap_xy)` from the connected controller's saved
    /// profile (see [`set_swap`]). Set on connect / on a relayed client edit.
    swaps: std::collections::HashMap<u8, (bool, bool)>,
    /// Legacy global fallback (`PHAROS_GAMEPAD_SWAP_FACE`): applies A↔B + X↔Y to
    /// any slot without a per-slot profile. Per-slot settings take precedence.
    swap_face_global: bool,
}

impl Default for PadManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PadManager {
    pub fn new() -> Self {
        let swap_face_global = std::env::var_os("PHAROS_GAMEPAD_SWAP_FACE").is_some();
        if swap_face_global {
            tracing::info!(
                "PHAROS_GAMEPAD_SWAP_FACE set — default A↔B, X↔Y on pads without a profile"
            );
        }
        Self {
            pads: std::collections::HashMap::new(),
            gyro: GyroMouse::default(),
            swaps: std::collections::HashMap::new(),
            swap_face_global,
        }
    }

    /// Apply a controller's saved face-button swaps to its slot (from the host
    /// profile store on connect, or a relayed client edit). Overrides the global
    /// env fallback for that slot.
    pub fn set_swap(&mut self, slot: u8, swap_ab: bool, swap_xy: bool) {
        self.swaps.insert(slot, (swap_ab, swap_xy));
    }

    fn ensure(&mut self, slot: u8) -> Option<&mut VirtualGamepad> {
        use std::collections::hash_map::Entry;
        match self.pads.entry(slot) {
            Entry::Occupied(e) => Some(e.into_mut()),
            Entry::Vacant(e) => match VirtualGamepad::new(slot) {
                Ok(p) => {
                    tracing::info!("virtual pad slot {slot} up (uinput Xbox 360 045e:028e)");
                    Some(e.insert(p))
                }
                Err(err) => {
                    tracing::warn!("virtual pad slot {slot} init failed: {err:#}");
                    None
                }
            },
        }
    }

    /// A controller connected on the client. Classify it by VID:PID for the log
    /// and bring up its virtual Xbox 360 pad.
    ///
    /// We **always** create the virtual pad — even when the same `vendor:product`
    /// is already on the host kernel via USB/IP. The virtual is the canonical
    /// sink for button/stick events that arrive on the Trireme back-channel
    /// (the TV-side Android InputManager path); a kernel-attached "real" pad
    /// does NOT replace that channel — it only adds an extra parallel device
    /// the game can independently consume for gyro / extra buttons.
    ///
    /// We *tried* to suppress the virtual when the real pad was on the kernel
    /// (so games would see one pad instead of two), but that silently dropped
    /// every Path-A button press the user made on the TV side. The duplicate-
    /// pad concern is a smaller UX annoyance than "no buttons at all", and is
    /// managed at the game level (Steam Input / SDL2 dedup / per-game config),
    /// not by the host.
    pub fn connect(&mut self, slot: u8, vendor: u16, product: u16) {
        let info = core_proto::controllers::classify(vendor, product, "");
        tracing::info!(
            "controller slot {slot}: {vendor:04x}:{product:04x} → {} (presented as Xbox 360)",
            info.kind.label()
        );
        self.ensure(slot);
    }

    pub fn disconnect(&mut self, slot: u8) {
        self.swaps.remove(&slot);
        if self.pads.remove(&slot).is_some() {
            tracing::info!("controller slot {slot} disconnected");
        }
    }

    pub fn button(&mut self, slot: u8, button: u8, pressed: bool) {
        // Per-slot profile wins; else the legacy global env fallback.
        let (swap_ab, swap_xy) = self
            .swaps
            .get(&slot)
            .copied()
            .unwrap_or((self.swap_face_global, self.swap_face_global));
        let button = apply_swap(button, swap_ab, swap_xy);
        if let Some(p) = self.ensure(slot) {
            p.button(button, pressed);
        }
    }

    pub fn axis(&mut self, slot: u8, axis: u8, value: f32) {
        if let Some(p) = self.ensure(slot) {
            p.axis(axis, value);
        }
    }
}

/// Apply independent face-button swaps: A↔B (0↔1) when `swap_ab`, X↔Y (2↔3) when
/// `swap_xy`. Other indices pass through. Lets a Nintendo-physical-layout pad
/// match Xbox prompts on either pair separately.
fn apply_swap(b: u8, swap_ab: bool, swap_xy: bool) -> u8 {
    match b {
        0 if swap_ab => 1,
        1 if swap_ab => 0,
        2 if swap_xy => 3,
        3 if swap_xy => 2,
        other => other,
    }
}

/// Bus a controller with this `vendor:product` is currently exposed on, by
/// walking the live `evdev::enumerate()` list. Used by the panel-facing
/// controller status snapshot to render a USB / BT pill;
/// returns [`ControllerBus::Unknown`] if no live evdev matches (the saved-but-
/// absent case, where the pill is hidden client-side). USB takes precedence
/// over Bluetooth if both happen to be present for the same VID:PID — the
/// physical path is the one the game actually reaches.
pub fn controller_bus(vendor: u16, product: u16) -> control_ipc::ControllerBus {
    let mut bt = false;
    for (_path, dev) in evdev::enumerate() {
        let id = dev.input_id();
        if id.vendor() != vendor || id.product() != product {
            continue;
        }
        match id.bus_type() {
            BusType::BUS_USB => return control_ipc::ControllerBus::Usb,
            BusType::BUS_BLUETOOTH => bt = true,
            _ => {}
        }
    }
    if bt {
        control_ipc::ControllerBus::Bluetooth
    } else {
        control_ipc::ControllerBus::Unknown
    }
}

/// Converts gyroscope angular velocity (rad/s) into relative mouse motion
/// (gyro-aim). Yaw drives horizontal motion, pitch vertical; the exact
/// sensor-axis convention depends on how the pad is held, so `sensitivity` and
/// client-side inversion tune it. A small deadzone kills resting drift.
pub struct GyroMouse {
    pub sensitivity: f64,
    pub deadzone: f64,
}

impl Default for GyroMouse {
    fn default() -> Self {
        GyroMouse {
            sensitivity: 12.0,
            deadzone: 0.015,
        }
    }
}

impl GyroMouse {
    /// Map a gyro sample to a `(dx, dy)` mouse delta. Convention: yaw (`gz`) →
    /// horizontal, pitch (`gx`) → vertical, both inverted so tilting up/right
    /// moves the cursor up/right. Roll (`gy`) is unused for aim.
    pub fn delta(&self, gx: f32, _gy: f32, gz: f32) -> (f64, f64) {
        let dz = |v: f64| if v.abs() < self.deadzone { 0.0 } else { v };
        (
            -dz(gz as f64) * self.sensitivity,
            -dz(gx as f64) * self.sensitivity,
        )
    }
}

/// Canonical axis index + normalised value → (evdev ABS code, scaled value).
fn axis_value(axis: u8, value: f32) -> Option<(u16, i32)> {
    use core_proto::gamepad::*;
    let stick = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i32;
    let trigger = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as i32;
    let hat = |v: f32| v.round().clamp(-1.0, 1.0) as i32;
    Some(match axis {
        AXIS_LEFT_X => (ABS_X, stick(value)),
        AXIS_LEFT_Y => (ABS_Y, stick(value)),
        AXIS_RIGHT_X => (ABS_RX, stick(value)),
        AXIS_RIGHT_Y => (ABS_RY, stick(value)),
        AXIS_LTRIGGER => (ABS_Z, trigger(value)),
        AXIS_RTRIGGER => (ABS_RZ, trigger(value)),
        AXIS_HAT_X => (ABS_HAT0X, hat(value)),
        AXIS_HAT_Y => (ABS_HAT0Y, hat(value)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gyro_deadzone_and_scale() {
        let g = GyroMouse {
            sensitivity: 10.0,
            deadzone: 0.1,
        };
        // Below deadzone → no motion.
        assert_eq!(g.delta(0.05, 0.0, 0.05), (0.0, 0.0));
        // Yaw → horizontal, pitch → vertical, inverted.
        let (dx, dy) = g.delta(0.2, 0.0, 0.5);
        assert!((dx - (-5.0)).abs() < 1e-6);
        assert!((dy - (-2.0)).abs() < 1e-6);
    }
}
