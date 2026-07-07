// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Desktop gamepad capture (issue #14).
//!
//! The wire protocol, the transport back-channel, and both host injectors (Linux
//! uinput, Windows ViGEm Xbox 360) already carry gamepad events end to end — but
//! only the Android client ever produced them. This module closes that gap for
//! the desktop client: it drives a `gilrs::Gilrs` (evdev on Linux, XInput/DInput
//! on Windows, so one loop serves both host targets) on a dedicated thread and
//! translates each event into the canonical [`core_proto::gamepad`] index space,
//! then pushes it into the ACTIVE source's input channel.
//!
//! It is a deliberate 1:1 port of the Android `ControllerManager`: same button
//! and axis indices, same 0.06 center deadzone, same 0.004 dedup epsilon, same
//! RAW-index policy (the client sends 0=A/1=B/2=X/3=Y and the HOST owns Swap A/B
//! and Swap X/Y — doing it both places would double-swap), same 8-pad cap, and
//! the same Y-axis inversion so up is negative on the wire. That parity means a
//! pad behaves identically whether it is plugged into an Android client or this
//! desktop one.
//!
//! Gyro/motion is intentionally NOT sent: gilrs exposes no IMU API, so there is
//! nothing to read. `GamepadMotion` stays unused here, which matches the existing
//! desktop posture (motion aim is an Android-only feature today). Sticks, the
//! d-pad, triggers, buttons, and hotplug all work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gilrs::ev::EventType;
use gilrs::{Axis, Button, GamepadId, Gilrs};
use tokio::sync::mpsc::UnboundedSender;

use core_proto::{gamepad as canon, InputEvent};

/// Max virtual pads, matching Android's `MAX_PADS`. One host Xbox 360 pad per
/// slot; extra controllers past the cap are simply not forwarded.
const MAX_PADS: usize = 8;
/// Stick center deadzone (matches Android's `CENTER_DEADZONE`). Applied to the
/// four stick axes only; triggers and the (already discrete) hat pass through.
const CENTER_DEADZONE: f32 = 0.06;
/// Per-axis dedup epsilon (matches Android's `AXIS_EPSILON`): sub-epsilon jitter
/// is dropped so we don't flood the back-channel with noise.
const AXIS_EPSILON: f32 = 0.004;
/// Poll cadence when idle. gilrs is not async; we sleep briefly between drains so
/// the thread doesn't spin. Short enough to stay imperceptible for input.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// The reconnect-safe sink the capture thread writes into. It is an
/// `Arc<Mutex<Option<Sender>>>` and NOT a bare `Sender` on purpose: the active
/// source's `input_tx` is re-created on every (re)connect, so handing the thread
/// a `Sender` by value would leave the pads writing into a dead channel after the
/// first blip. The play loop swaps the inner `Option` whenever the active source's
/// channel is set or cleared, and clears it (`None`) when `CaptureMode::Off` mutes
/// forwarding — so the thread reads the current target every poll.
pub type GamepadSink = Arc<Mutex<Option<UnboundedSender<InputEvent>>>>;

/// Per-pad running state: the last value emitted for each canonical axis (for the
/// epsilon dedup) and the set of held d-pad directions (for backends that report
/// the d-pad as four buttons rather than a hat axis).
#[derive(Default)]
pub struct PadState {
    /// canonical axis index -> last emitted value.
    last_axis: HashMap<u8, f32>,
    /// Held d-pad direction buttons; used to synthesize hat axes 6/7 exactly like
    /// Android's `updateDpad()` when the backend has no analog hat.
    dpad: DpadHeld,
}

/// Held d-pad directions for the button-style d-pad synthesis path.
#[derive(Default)]
struct DpadHeld {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

/// A `Code`-free view of the gilrs events we actually translate. gilrs's own
/// `EventType` embeds a platform `Code` with no public constructor, so tests
/// can't build one; lifting the event into this enum first keeps the mapping
/// policy (indices, Y-invert, deadzone, epsilon dedup, d-pad synth) fully pure
/// and unit-testable while `map_event` stays the real gilrs entry point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PadEvent {
    /// A digital button transition (gilrs `ButtonPressed`/`ButtonReleased`).
    Button(Button, bool),
    /// An analog "button" value change (gilrs `ButtonChanged`); used for the
    /// XInput trigger path where the trigger is a 0..1 button.
    ButtonValue(Button, f32),
    /// An axis value change (gilrs `AxisChanged`): sticks, hat, evdev triggers.
    Axis(Axis, f32),
}

/// Translate one gilrs event into zero or more canonical [`InputEvent`]s for a
/// pad on `slot`. Connect/disconnect are handled by the caller (slot free-list +
/// VID:PID); every other event is lifted into [`PadEvent`] and mapped by the pure
/// [`map_pad_event`].
pub fn map_event(ev: &EventType, slot: u8, state: &mut PadState) -> Vec<InputEvent> {
    let pad = match ev {
        EventType::ButtonPressed(btn, _) => PadEvent::Button(*btn, true),
        EventType::ButtonReleased(btn, _) => PadEvent::Button(*btn, false),
        EventType::ButtonChanged(btn, value, _) => PadEvent::ButtonValue(*btn, *value),
        EventType::AxisChanged(axis, value, _) => PadEvent::Axis(*axis, *value),
        // Connect/disconnect drive slot bookkeeping in the caller; repeats, the
        // internal filter/force-feedback events, and any future (non_exhaustive)
        // variant are not forwarded.
        EventType::Connected
        | EventType::Disconnected
        | EventType::ButtonRepeated(..)
        | EventType::Dropped
        | EventType::ForceFeedbackEffectCompleted => return Vec::new(),
        _ => return Vec::new(),
    };
    map_pad_event(pad, slot, state)
}

/// The pure gilrs->canonical mapping. Device-free and `Code`-free so it is
/// directly unit-testable; all indices, the Y inversion, the deadzone, the
/// epsilon dedup, and the d-pad synth live here.
///
/// Returns an empty vec for anything we don't forward (unknown buttons/axes, a
/// sub-deadzone stick nudge, a sub-epsilon repeat).
pub fn map_pad_event(ev: PadEvent, slot: u8, state: &mut PadState) -> Vec<InputEvent> {
    match ev {
        // Digital buttons. RAW canonical indices; the host owns face swaps.
        PadEvent::Button(btn, pressed) => match button_index(btn) {
            Some(idx) => vec![InputEvent::GamepadButton {
                slot,
                button: idx,
                pressed,
            }],
            // A button-style d-pad direction → resync the synthesized hat axes.
            None => dpad_button(btn, pressed, slot, state),
        },
        // Analog trigger reported as a "button" with a 0..1 value (the common
        // XInput path, where the trigger maps to BTN_LT2/BTN_RT2). Route it to
        // the trigger axis; other ButtonChanged events (digital buttons) are
        // already covered by the Pressed/Released path, so ignore them here.
        PadEvent::ButtonValue(btn, value) => match trigger_axis(btn) {
            Some(axis) => axis_event(slot, axis, value, false, state),
            None => Vec::new(),
        },
        // Sticks, hat, and the LeftZ/RightZ analog-trigger path (the common evdev
        // mapping). Stick axes get the deadzone + Y inversion; triggers/hat don't.
        PadEvent::Axis(axis, value) => match axis_index(axis) {
            Some((idx, is_stick, invert)) => {
                let v = if invert { -value } else { value };
                axis_event(slot, idx, v, is_stick, state)
            }
            None => Vec::new(),
        },
    }
}

/// Emit an axis event with the deadzone (sticks only) and the epsilon dedup,
/// mirroring Android's `sendAxis`.
fn axis_event(slot: u8, idx: u8, raw: f32, is_stick: bool, state: &mut PadState) -> Vec<InputEvent> {
    let v = if is_stick && raw.abs() < CENTER_DEADZONE {
        0.0
    } else {
        raw
    };
    if let Some(last) = state.last_axis.get(&idx) {
        if (last - v).abs() < AXIS_EPSILON {
            return Vec::new();
        }
    }
    state.last_axis.insert(idx, v);
    vec![InputEvent::GamepadAxis {
        slot,
        axis: idx,
        value: v,
    }]
}

/// Synthesize hat axes 6/7 from a held-direction button, exactly like Android's
/// `updateDpad()`: right/down are positive. Runs the result through the same
/// epsilon dedup so a no-op resync stays silent.
fn dpad_button(btn: Button, pressed: bool, slot: u8, state: &mut PadState) -> Vec<InputEvent> {
    match btn {
        Button::DPadUp => state.dpad.up = pressed,
        Button::DPadDown => state.dpad.down = pressed,
        Button::DPadLeft => state.dpad.left = pressed,
        Button::DPadRight => state.dpad.right = pressed,
        _ => return Vec::new(),
    }
    let hx = (if state.dpad.right { 1.0 } else { 0.0 }) + (if state.dpad.left { -1.0 } else { 0.0 });
    let hy = (if state.dpad.down { 1.0 } else { 0.0 }) + (if state.dpad.up { -1.0 } else { 0.0 });
    let mut out = axis_event(slot, canon::AXIS_HAT_X, hx, false, state);
    out.extend(axis_event(slot, canon::AXIS_HAT_Y, hy, false, state));
    out
}

/// gilrs button -> canonical button index, or `None` for the d-pad directions
/// (handled as synthesized hat axes) and anything we don't forward.
///
/// gilrs uses SDL naming: on an Xbox pad `South`/`East`/`West`/`North` are the
/// PHYSICAL A/B/X/Y, and (confusingly) `LeftTrigger`/`RightTrigger` are the
/// SHOULDER bumpers (evdev BTN_LT/BTN_RT), while the analog triggers are
/// `LeftTrigger2`/`RightTrigger2`. So South->A(0) ... LeftTrigger->LB(4) is
/// correct and does NOT double-swap; the analog triggers go to the axes instead.
fn button_index(btn: Button) -> Option<u8> {
    Some(match btn {
        Button::South => canon::BTN_A,       // 0
        Button::East => canon::BTN_B,        // 1
        Button::West => canon::BTN_X,        // 2
        Button::North => canon::BTN_Y,       // 3
        Button::LeftTrigger => canon::BTN_LB, // 4 (the LB bumper, not the analog trigger)
        Button::RightTrigger => canon::BTN_RB, // 5 (the RB bumper)
        Button::Select => canon::BTN_BACK,   // 6
        Button::Start => canon::BTN_START,   // 7
        Button::Mode => canon::BTN_GUIDE,    // 8
        Button::LeftThumb => canon::BTN_LSTICK, // 9
        Button::RightThumb => canon::BTN_RSTICK, // 10
        _ => return None,
    })
}

/// gilrs analog-trigger button -> canonical trigger axis, for the XInput-style
/// path where the trigger arrives as a `ButtonChanged` with a 0..1 value.
fn trigger_axis(btn: Button) -> Option<u8> {
    match btn {
        Button::LeftTrigger2 => Some(canon::AXIS_LTRIGGER),  // 4
        Button::RightTrigger2 => Some(canon::AXIS_RTRIGGER), // 5
        _ => None,
    }
}

/// gilrs axis -> (canonical axis index, is_stick, invert). Sticks get the
/// deadzone; the Y sticks are inverted because gilrs reports up as positive while
/// the wire (evdev/Android) is down-positive. LeftZ/RightZ are the analog triggers
/// on the evdev mapping (0..1, no invert/deadzone). The hat rides axes 6/7.
fn axis_index(axis: Axis) -> Option<(u8, bool, bool)> {
    Some(match axis {
        Axis::LeftStickX => (canon::AXIS_LEFT_X, true, false), // 0
        Axis::LeftStickY => (canon::AXIS_LEFT_Y, true, true),  // 1, inverted
        Axis::RightStickX => (canon::AXIS_RIGHT_X, true, false), // 2
        Axis::RightStickY => (canon::AXIS_RIGHT_Y, true, true), // 3, inverted
        Axis::LeftZ => (canon::AXIS_LTRIGGER, false, false),   // 4
        Axis::RightZ => (canon::AXIS_RTRIGGER, false, false),  // 5
        Axis::DPadX => (canon::AXIS_HAT_X, false, false),      // 6
        Axis::DPadY => (canon::AXIS_HAT_Y, false, false),      // 7
        Axis::Unknown => return None,
    })
}

/// A stable slot free-list, matching Android's `used[]` + `allocSlot()`.
struct SlotTable {
    used: [bool; MAX_PADS],
    of: HashMap<GamepadId, u8>,
}

impl SlotTable {
    fn new() -> Self {
        Self {
            used: [false; MAX_PADS],
            of: HashMap::new(),
        }
    }

    /// Allocate (or return the existing) slot for a gamepad, or `None` if all
    /// slots are in use.
    fn alloc(&mut self, id: GamepadId) -> Option<u8> {
        if let Some(&s) = self.of.get(&id) {
            return Some(s);
        }
        let free = self.used.iter().position(|&u| !u)?;
        self.used[free] = true;
        let slot = free as u8;
        self.of.insert(id, slot);
        Some(slot)
    }

    /// Free a gamepad's slot; returns the freed slot for the disconnect event.
    fn free(&mut self, id: GamepadId) -> Option<u8> {
        let slot = self.of.remove(&id)?;
        self.used[slot as usize] = false;
        Some(slot)
    }
}

/// Push an event into the active source's channel, if forwarding is live. Reads
/// the shared sink each call so a reconnect (new channel) or an `Off` mute (sink
/// cleared to `None`) is picked up immediately, and drops the event otherwise.
fn emit(sink: &GamepadSink, ev: InputEvent) {
    if let Ok(guard) = sink.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(ev);
        }
    }
}

/// Spawn the capture thread. Owns a `gilrs::Gilrs`, polls it, allocates a stable
/// slot per controller, translates events via [`map_event`], and writes them into
/// the reconnect-safe `sink`. Call ONCE at startup; the thread runs for the life
/// of the process. If gilrs can't init on this platform (headless CI, missing
/// permissions) it logs and returns without spawning — the rest of the client is
/// unaffected, since the whole path is inert unless a pad is present.
pub fn run_gamepad_capture(sink: GamepadSink) {
    let mut gilrs = match Gilrs::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("gamepad capture disabled: gilrs init failed: {e:?}");
            return;
        }
    };
    std::thread::Builder::new()
        .name("trireme-gamepad".into())
        .spawn(move || {
            let mut slots = SlotTable::new();
            let mut states: HashMap<u8, PadState> = HashMap::new();
            // Adopt any pad already connected at startup so it doesn't need a
            // replug to be seen; gilrs won't re-emit Connected for these.
            let present: Vec<GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
            for id in present {
                announce_connect(&mut gilrs, &mut slots, &mut states, &sink, id);
            }
            loop {
                while let Some(gilrs::Event { id, event, .. }) = gilrs.next_event() {
                    match event {
                        EventType::Connected => {
                            announce_connect(&mut gilrs, &mut slots, &mut states, &sink, id);
                        }
                        EventType::Disconnected => {
                            if let Some(slot) = slots.free(id) {
                                states.remove(&slot);
                                tracing::info!("gamepad -slot{slot}");
                                emit(&sink, InputEvent::GamepadDisconnect { slot });
                            }
                        }
                        other => {
                            // Only forward for a pad that already holds a slot;
                            // a lingering event for an over-cap or freed pad is
                            // dropped.
                            if let Some(&slot) = slots.of.get(&id) {
                                let state = states.entry(slot).or_default();
                                for ev in map_event(&other, slot, state) {
                                    emit(&sink, ev);
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .expect("spawn gamepad capture thread");
}

/// Allocate a slot for a newly-seen pad and emit `GamepadConnect` with its
/// VID:PID (0 fallback), matching Android's `attach`. Over the 8-pad cap the pad
/// is silently ignored.
fn announce_connect(
    gilrs: &mut Gilrs,
    slots: &mut SlotTable,
    states: &mut HashMap<u8, PadState>,
    sink: &GamepadSink,
    id: GamepadId,
) {
    let Some(slot) = slots.alloc(id) else {
        tracing::warn!("gamepad ignored: all {MAX_PADS} slots in use");
        return;
    };
    let pad = gilrs.gamepad(id);
    let vendor = pad.vendor_id().unwrap_or(0);
    let product = pad.product_id().unwrap_or(0);
    states.insert(slot, PadState::default());
    tracing::info!(
        "gamepad +slot{slot}: {} {:04x}:{:04x}",
        pad.name(),
        vendor,
        product
    );
    emit(
        sink,
        InputEvent::GamepadConnect {
            slot,
            vendor,
            product,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the Android mapping tests: feed synthetic events through the pure
    // `map_pad_event` (the `Code`-free core of `map_event`; gilrs's `Code` has no
    // public constructor, so tests target this layer) and assert the canonical
    // output matches — same indices, same Y inversion, same deadzone, same
    // epsilon dedup — so desktop and Android stay in lockstep against the host.

    #[test]
    fn south_is_a_pressed() {
        let mut st = PadState::default();
        let out = map_pad_event(PadEvent::Button(Button::South, true), 0, &mut st);
        assert_eq!(
            out,
            vec![InputEvent::GamepadButton {
                slot: 0,
                button: canon::BTN_A,
                pressed: true
            }]
        );
    }

    #[test]
    fn face_buttons_are_raw_no_swap() {
        // East->B(1), West->X(2), North->Y(3): RAW canonical, host owns swaps.
        for (btn, idx) in [
            (Button::East, canon::BTN_B),
            (Button::West, canon::BTN_X),
            (Button::North, canon::BTN_Y),
        ] {
            let mut st = PadState::default();
            let out = map_pad_event(PadEvent::Button(btn, false), 1, &mut st);
            assert_eq!(
                out,
                vec![InputEvent::GamepadButton {
                    slot: 1,
                    button: idx,
                    pressed: false
                }]
            );
        }
    }

    #[test]
    fn shoulders_map_to_bumpers_not_triggers() {
        // gilrs's SDL naming: LeftTrigger/RightTrigger are the BUMPERS (LB/RB),
        // the analog triggers are LeftTrigger2/RightTrigger2. Guard the mapping so
        // a rename can't silently turn a bumper into a trigger.
        let mut st = PadState::default();
        let lb = map_pad_event(PadEvent::Button(Button::LeftTrigger, true), 0, &mut st);
        assert_eq!(
            lb,
            vec![InputEvent::GamepadButton {
                slot: 0,
                button: canon::BTN_LB,
                pressed: true
            }]
        );
        let rb = map_pad_event(PadEvent::Button(Button::RightTrigger, true), 0, &mut st);
        assert_eq!(
            rb,
            vec![InputEvent::GamepadButton {
                slot: 0,
                button: canon::BTN_RB,
                pressed: true
            }]
        );
    }

    #[test]
    fn left_stick_up_is_inverted_negative() {
        // gilrs reports up as POSITIVE; the wire is down-positive, so up must come
        // out NEGATIVE on axis 1 — the same sign Android sends.
        let mut st = PadState::default();
        let out = map_pad_event(PadEvent::Axis(Axis::LeftStickY, 0.8), 0, &mut st);
        assert_eq!(out.len(), 1);
        match out[0] {
            InputEvent::GamepadAxis { axis, value, .. } => {
                assert_eq!(axis, canon::AXIS_LEFT_Y);
                assert!(value < 0.0, "up must be negative on the wire, got {value}");
            }
            _ => panic!("expected GamepadAxis"),
        }
    }

    #[test]
    fn left_trigger_full_is_axis_four_one() {
        // The XInput-style path: trigger arrives as ButtonChanged(LeftTrigger2, 1.0).
        let mut st = PadState::default();
        let out = map_pad_event(PadEvent::ButtonValue(Button::LeftTrigger2, 1.0), 0, &mut st);
        assert_eq!(
            out,
            vec![InputEvent::GamepadAxis {
                slot: 0,
                axis: canon::AXIS_LTRIGGER,
                value: 1.0
            }]
        );
    }

    #[test]
    fn left_trigger_via_leftz_axis() {
        // The evdev-style path: same trigger arrives as AxisChanged(LeftZ, 1.0).
        let mut st = PadState::default();
        let out = map_pad_event(PadEvent::Axis(Axis::LeftZ, 1.0), 0, &mut st);
        assert_eq!(
            out,
            vec![InputEvent::GamepadAxis {
                slot: 0,
                axis: canon::AXIS_LTRIGGER,
                value: 1.0
            }]
        );
    }

    #[test]
    fn dpad_right_button_synthesizes_hat_x() {
        // Backends that report the d-pad as buttons: DPadRight held -> hat X = +1.
        let mut st = PadState::default();
        let out = map_pad_event(PadEvent::Button(Button::DPadRight, true), 0, &mut st);
        assert!(out.contains(&InputEvent::GamepadAxis {
            slot: 0,
            axis: canon::AXIS_HAT_X,
            value: 1.0,
        }));
    }

    #[test]
    fn dpad_right_via_axis() {
        // Backends with an analog hat: DPadX = +1 -> hat X = +1 directly.
        let mut st = PadState::default();
        let out = map_pad_event(PadEvent::Axis(Axis::DPadX, 1.0), 0, &mut st);
        assert_eq!(
            out,
            vec![InputEvent::GamepadAxis {
                slot: 0,
                axis: canon::AXIS_HAT_X,
                value: 1.0
            }]
        );
    }

    #[test]
    fn sub_deadzone_stick_is_dropped() {
        // A stick deflection below the 0.06 center deadzone snaps to 0. From a
        // rest state (no prior emit) that 0 is within epsilon of nothing, so it
        // still emits the first 0 — but a SECOND sub-deadzone nudge is silent.
        let mut st = PadState::default();
        // First emit establishes last=0.0 (deadzoned).
        let first = map_pad_event(PadEvent::Axis(Axis::LeftStickX, 0.03), 0, &mut st);
        assert_eq!(
            first,
            vec![InputEvent::GamepadAxis {
                slot: 0,
                axis: canon::AXIS_LEFT_X,
                value: 0.0
            }]
        );
        // A further sub-deadzone nudge is also 0.0 → deduped to nothing.
        let second = map_pad_event(PadEvent::Axis(Axis::LeftStickX, 0.05), 0, &mut st);
        assert!(second.is_empty(), "sub-deadzone repeat must be dropped");
    }

    #[test]
    fn sub_epsilon_delta_is_dropped() {
        // Establish a real value, then move it by less than the 0.004 epsilon.
        let mut st = PadState::default();
        let _ = map_pad_event(PadEvent::Axis(Axis::LeftStickX, 0.50), 0, &mut st);
        let tiny = map_pad_event(PadEvent::Axis(Axis::LeftStickX, 0.502), 0, &mut st);
        assert!(tiny.is_empty(), "sub-epsilon delta must be dropped");
    }
}
