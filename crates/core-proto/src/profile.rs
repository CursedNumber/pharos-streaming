// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Per-controller tuning profile — shared by the client (editor UI), the wire
//! (the `ControllerTuning` message), and the host (applies + persists it).
//!
//! The host reads a controller's IMU directly (over USB/IP or Bluetooth), so
//! gyro feel is applied on the host; the client only edits these numbers and
//! relays them. Persisted host-side keyed by the canonical `"vvvv:pppp"`
//! controller id, so a pad gets its saved feel the moment it connects.

use serde::{Deserialize, Serialize};

/// Gyro→mouse tuning. Mirrors the `PHAROS_GYRO_*` knobs the host `gyro_imu`
/// reader honoured during bring-up, now persisted per controller and editable
/// from the client. `axis_h`/`axis_v` pick which raw gyro axis (0=rx, 1=ry,
/// 2=rz in the device's triple) drives horizontal / vertical — the physical
/// meaning (roll/pitch/yaw) depends on the controller, so it's user-mappable.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GyroTuning {
    /// Master switch for gyro→mouse on this controller.
    pub enabled: bool,
    /// Horizontal / vertical sensitivity (px per degree).
    pub sens_x: f32,
    pub sens_y: f32,
    /// Acceleration strength (0 = off, flat 1:1).
    pub accel: f32,
    /// Attack-curve exponent (1 = linear; >1 keeps slow aim precise).
    pub accel_pow: f32,
    /// Rotation speed (deg/s) where the curve reaches 1× strength (the knee).
    pub accel_ref: f32,
    /// Clamp on the extra gain so flicks can't explode.
    pub accel_max: f32,
    /// Flip horizontal / vertical direction.
    pub invert_x: bool,
    pub invert_y: bool,
    /// Which raw gyro axis drives horizontal / vertical (0=rx, 1=ry, 2=rz).
    pub axis_h: u8,
    pub axis_v: u8,
}

impl Default for GyroTuning {
    /// The feel dialled in live on the dev host (Switch Pro over BT): yaw→horizontal
    /// with more side-to-side speed, pitch→vertical, a precise low end that
    /// ramps on flicks.
    fn default() -> Self {
        Self {
            enabled: true,
            sens_x: 16.0,
            sens_y: 9.0,
            accel: 2.5,
            accel_pow: 2.0,
            accel_ref: 110.0,
            accel_max: 10.0,
            invert_x: false,
            invert_y: false,
            axis_h: 2, // RZ = yaw → horizontal
            axis_v: 1, // RY = pitch → vertical
        }
    }
}

/// Everything tunable for one controller: face-button swaps + gyro feel. A bare
/// `ControllerProfile::default()` is the out-of-the-box feel (no swaps).
///
/// `alias` is an optional user-given display name (added in proto v4). When set,
/// every UI prefers it over the auto-classifier label; `None` falls back to the
/// classifier label as before. `#[serde(default)]` keeps older TOML payloads
/// (no `alias` key) loading — both the on-disk `controller-profiles.toml` and
/// the JSON the panel exchanges via `control-ipc`. The field is *always*
/// emitted on the wire (postcard / `InputEvent::ControllerTuning`) because
/// postcard's positional format has no notion of skipped fields; clients and
/// hosts must rebuild together for that wire path.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ControllerProfile {
    /// Optional user-chosen display name. `None` = fall back to the classifier label.
    #[serde(default)]
    pub alias: Option<String>,
    /// Swap A↔B before injecting (Nintendo-physical → Xbox prompts).
    pub swap_ab: bool,
    /// Swap X↔Y before injecting.
    pub swap_xy: bool,
    pub gyro: GyroTuning,
}
