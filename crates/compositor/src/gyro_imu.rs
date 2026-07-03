// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Host-side gyro→mouse from a USB/IP-imported or directly-paired controller IMU.
//!
//! Android won't hand a controller's gyroscope to apps, so the pad reaches the
//! host (over USB/IP or Bluetooth) where Linux's `hid-nintendo` /
//! `hid-playstation` drivers publish the IMU as a standard evdev device (name
//! ends "(IMU)" / "Motion Sensors"). This reads it on a background thread and
//! accumulates a relative-mouse delta the capture loop drains each frame — wrist
//! motion becomes pointer aim in any game, no Android sensor needed.
//!
//! Gyro is normalised by the device's reported axis resolution and integrated
//! over real time, so motion is proportional to actual rotation regardless of
//! cal scale (USB "user cal" ≈±5k vs BT "factory cal" ≈±3M) and sample rate
//! (USB/IP ≈226 Hz vs direct-BT ≈960 Hz).
//!
//! Tuning is a live [`core_proto::GyroTuning`] (persisted per controller in
//! `controller-profiles.toml`, editable from the client): [`GyroImu::set_tuning`]
//! swaps it in without restarting the reader, so slider moves on the TV take
//! effect immediately. The `PHAROS_GYRO_*` env vars still seed the initial feel
//! (handy for the standalone demo / debugging), overriding the built-in default:
//!   `PHAROS_GYRO_SENS`   base px/degree;
//!   `PHAROS_GYRO_SENS_X` / `_Y`  per-axis override (e.g. more left-right);
//!   `PHAROS_GYRO_ACCEL`  acceleration strength (0 = off, flat 1:1);
//!   `PHAROS_GYRO_ACCEL_POW`  attack-curve exponent (1 = linear; >1 keeps the
//!                            slow-aim end precise and only ramps up on flicks);
//!   `PHAROS_GYRO_ACCEL_REF`  rotation speed (deg/s) where the curve reaches 1×
//!                            strength — the "knee";
//!   `PHAROS_GYRO_ACCEL_MAX`  clamp on the extra gain so flicks can't explode;
//!   `PHAROS_GYRO_INVERT_X` / `_Y`  flip an axis (defaults: yaw-right → right,
//!                                  pitch nose-up → up);
//!   `PHAROS_GYRO_AXIS_H` / `_V`  pick which gyro axis (`rx`=roll / `ry`=pitch /
//!                                `rz`=yaw) drives horizontal / vertical.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use core_proto::GyroTuning;

// Gyro evdev codes (accel is ABS_X/Y/Z = 0/1/2; gyro is ABS_RX/RY/RZ = 3/4/5).
const ABS_GYRO: [u16; 3] = [0x03, 0x04, 0x05]; // [rx, ry, rz]

#[derive(Clone, Copy)]
struct Cfg {
    enabled: bool,  // gyro→mouse master switch for the active controller
    sens_x: f64,    // px per degree, horizontal
    sens_y: f64,    // px per degree, vertical
    accel: f64,     // acceleration strength (0 = off, flat 1:1)
    accel_pow: f64, // attack-curve exponent on normalised speed (1 = linear)
    accel_ref: f64, // rotation speed (deg/s) where normalised speed = 1 (knee)
    accel_max: f64, // clamp on the extra gain so flicks can't explode
    sgn_x: f64,     // horizontal sign; -1 default (yaw-right → right)
    sgn_y: f64,     // vertical sign; +1 default (pitch nose-up → up)
    h_axis: usize,  // index into the [rx, ry, rz] gyro triple
    v_axis: usize,
}

impl Cfg {
    /// Map a persisted/relayed [`GyroTuning`] into the reader's working config.
    /// `invert_*` choose the sign; `accel_ref` is guarded > 0 to avoid /0.
    fn from_tuning(t: &GyroTuning) -> Self {
        Cfg {
            enabled: t.enabled,
            sens_x: f64::from(t.sens_x),
            sens_y: f64::from(t.sens_y),
            accel: f64::from(t.accel).max(0.0),
            accel_pow: f64::from(t.accel_pow),
            accel_ref: nonzero_f64(f64::from(t.accel_ref), 100.0),
            accel_max: f64::from(t.accel_max),
            sgn_x: if t.invert_x { 1.0 } else { -1.0 },
            sgn_y: if t.invert_y { -1.0 } else { 1.0 },
            h_axis: (t.axis_h as usize).min(2),
            v_axis: (t.axis_v as usize).min(2),
        }
    }
}

/// Background reader of a controller IMU. The capture loop drains an accumulated
/// mouse delta each frame via [`take`]; the live config (shared with the reader
/// thread) is swapped in via [`set_tuning`] when a profile loads or the client
/// relays an edit.
pub struct GyroImu {
    accum: Arc<Mutex<(f64, f64)>>,
    cfg: Arc<Mutex<Cfg>>,
    /// Latest raw per-axis angular velocity `(rx, ry, rz)` in deg/s, plus a
    /// monotonically-bumped sequence so callers can tell "fresh sample" from
    /// "no IMU / stale". For host→client calibration telemetry.
    latest: Arc<Mutex<(f64, f64, f64, u64)>>,
    /// Master gate set from the compositor each frame based on whether ANY
    /// connected streamer's client opted in to gyro at the auth handshake.
    /// When false the reader thread idles in a 500ms sleep loop instead of
    /// holding the IMU evdev open and reading samples — no JNI / datagram
    /// traffic, no integration. The gate is also re-checked inside the tight
    /// fetch-events loop so a flip from on→off pauses promptly (within one
    /// fetch_events return) instead of waiting on a stalled read.
    gate: Arc<AtomicBool>,
}

impl GyroImu {
    /// Spawn the reader. It self-finds the IMU evdev when a controller appears
    /// and re-finds it across unplug/replug; idle (cheap) when none. The initial
    /// feel is the built-in default with any `PHAROS_GYRO_*` env overrides.
    ///
    /// The reader starts **gated off** (no IMU device open, no events fetched):
    /// the compositor calls [`set_gated`] each frame based on whether any
    /// connected client opted in to gyro at handshake, so a default-off client
    /// pays nothing.
    pub fn spawn() -> Self {
        let cfg = Arc::new(Mutex::new(Cfg::from_tuning(&tuning_from_env())));
        let accum = Arc::new(Mutex::new((0.0, 0.0)));
        let latest = Arc::new(Mutex::new((0.0, 0.0, 0.0, 0u64)));
        let gate = Arc::new(AtomicBool::new(false));
        let shared_accum = accum.clone();
        let shared_cfg = cfg.clone();
        let shared_latest = latest.clone();
        let shared_gate = gate.clone();
        thread::Builder::new()
            .name("gyro-imu".into())
            .spawn(move || imu_loop(&shared_accum, &shared_cfg, &shared_latest, &shared_gate))
            .ok();
        GyroImu {
            accum,
            cfg,
            latest,
            gate,
        }
    }

    /// Open or close the gyro reader's gate. When `on` is false the reader
    /// thread sleeps in 500ms ticks and never opens the IMU evdev; when `on`
    /// is true the next tick proceeds to its normal find-IMU + read loop.
    /// Cheap (one atomic store) — the compositor calls this per-frame.
    pub fn set_gated(&self, on: bool) {
        // Acquire/Release so the reader thread sees the flip on its next
        // read (the imu_loop also uses Acquire when checking).
        self.gate.store(on, Ordering::Release);
    }

    /// Latest raw gyro `(rx, ry, rz)` in deg/s and a freshness sequence (bumped
    /// each IMU sample). The capture loop streams this to the client's
    /// calibration visualizer. Returns all-zero seq 0 when no IMU has reported.
    pub fn latest_raw(&self) -> (f32, f32, f32, u64) {
        match self.latest.lock() {
            Ok(l) => (l.0 as f32, l.1 as f32, l.2 as f32, l.3),
            Err(_) => (0.0, 0.0, 0.0, 0),
        }
    }

    /// Swap in new tuning live (no reader restart). Called when a controller's
    /// saved profile loads on connect, or the client relays a slider edit.
    pub fn set_tuning(&self, t: &GyroTuning) {
        if let Ok(mut c) = self.cfg.lock() {
            *c = Cfg::from_tuning(t);
        }
        tracing::info!(
            "gyro tuning: enabled={} sens=({:.1},{:.1}) accel={} pow={} ref={} max={} \
             invert=({},{}) axes H={} V={}",
            t.enabled,
            t.sens_x,
            t.sens_y,
            t.accel,
            t.accel_pow,
            t.accel_ref,
            t.accel_max,
            t.invert_x,
            t.invert_y,
            t.axis_h,
            t.axis_v,
        );
    }

    /// Take and reset the accumulated `(dx, dy)` mouse delta (call once/frame).
    pub fn take(&self) -> (f64, f64) {
        match self.accum.lock() {
            Ok(mut g) => {
                let v = *g;
                *g = (0.0, 0.0);
                v
            }
            Err(_) => (0.0, 0.0),
        }
    }
}

/// Build the initial tuning: the built-in default, with any `PHAROS_GYRO_*` env
/// vars overriding individual fields (seeds the standalone demo / debugging).
fn tuning_from_env() -> GyroTuning {
    let mut t = GyroTuning::default();
    if let Some(base) = env_f64("PHAROS_GYRO_SENS") {
        t.sens_x = base as f32;
        t.sens_y = base as f32;
    }
    if let Some(v) = env_f64("PHAROS_GYRO_SENS_X") {
        t.sens_x = v as f32;
    }
    if let Some(v) = env_f64("PHAROS_GYRO_SENS_Y") {
        t.sens_y = v as f32;
    }
    // accel may legitimately be 0 (off), so accept >= 0 here.
    if let Some(v) = std::env::var("PHAROS_GYRO_ACCEL")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|v| *v >= 0.0)
    {
        t.accel = v;
    }
    if let Some(v) = env_f64("PHAROS_GYRO_ACCEL_POW") {
        t.accel_pow = v as f32;
    }
    if let Some(v) = env_f64("PHAROS_GYRO_ACCEL_REF") {
        t.accel_ref = v as f32;
    }
    if let Some(v) = env_f64("PHAROS_GYRO_ACCEL_MAX") {
        t.accel_max = v as f32;
    }
    if env_set("PHAROS_GYRO_INVERT_X") {
        t.invert_x = true;
    }
    if env_set("PHAROS_GYRO_INVERT_Y") {
        t.invert_y = true;
    }
    t.axis_h = axis_idx("PHAROS_GYRO_AXIS_H", t.axis_h as usize) as u8;
    t.axis_v = axis_idx("PHAROS_GYRO_AXIS_V", t.axis_v as usize) as u8;
    t
}

fn find_imu() -> Option<evdev::Device> {
    for (_path, dev) in evdev::enumerate() {
        let name = dev.name().unwrap_or("");
        if name.contains("IMU") || name.contains("Motion Sensors") {
            return Some(dev);
        }
    }
    None
}

fn imu_loop(
    accum: &Arc<Mutex<(f64, f64)>>,
    cfg_shared: &Arc<Mutex<Cfg>>,
    latest: &Arc<Mutex<(f64, f64, f64, u64)>>,
    gate: &Arc<AtomicBool>,
) {
    let mut seq: u64 = 0; // bumped per sample, persists across device re-finds
    loop {
        // Master gate: no connected client opted in to gyro this frame. Idle
        // in a 500ms sleep loop — don't touch the evdev, don't burn CPU. The
        // compositor flips the gate the moment a v2 client with wants_gyro=true
        // connects.
        if !gate.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let Some(mut dev) = find_imu() else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        // Per-axis resolution (units per deg/s on hid-nintendo) so one sensitivity
        // survives different cal scales.
        let res: [f64; 3] = match dev.get_abs_state() {
            Ok(a) => [
                nonzero(a[ABS_GYRO[0] as usize].resolution),
                nonzero(a[ABS_GYRO[1] as usize].resolution),
                nonzero(a[ABS_GYRO[2] as usize].resolution),
            ],
            Err(_) => [1.0, 1.0, 1.0],
        };
        tracing::info!(
            "gyro IMU: '{}' (res {res:?}) — active",
            dev.name().unwrap_or("?"),
        );
        // Latest angular velocity (deg/s) per gyro axis + last sample time, so we
        // integrate velocity × Δt into displacement (rate-independent).
        let mut g = [0.0f64; 3];
        let mut last: Option<std::time::SystemTime> = None;
        loop {
            // Gate flipped off mid-session (e.g. the only opted-in client
            // disconnected): drop out so the outer loop returns to the idle
            // 500ms sleep. We do NOT close the evdev explicitly — its drop
            // when `dev` goes out of scope at the outer `break` releases it.
            if !gate.load(Ordering::Acquire) {
                break;
            }
            let events = match dev.fetch_events() {
                Ok(e) => e,
                Err(_) => break, // device vanished (unplug) — re-find
            };
            // Re-read the live config once per batch so client edits apply
            // without restarting the reader (the lock is uncontended).
            let cfg = match cfg_shared.lock() {
                Ok(c) => *c,
                Err(_) => return, // poisoned — give up the thread
            };
            let mut dx = 0.0;
            let mut dy = 0.0;
            for ev in events {
                match ev.event_type() {
                    evdev::EventType::ABSOLUTE => {
                        let c = ev.code();
                        if let Some(i) = ABS_GYRO.iter().position(|&a| a == c) {
                            g[i] = f64::from(ev.value()) / res[i];
                        }
                    }
                    evdev::EventType::SYNCHRONIZATION => {
                        let ts = ev.timestamp();
                        // Publish the raw per-axis rate every sample (independent of
                        // `enabled`/mapping) so the calibration visualizer shows true
                        // motion even with gyro→mouse off.
                        seq = seq.wrapping_add(1);
                        if let Ok(mut l) = latest.lock() {
                            *l = (g[0], g[1], g[2], seq);
                        }
                        if let (Some(prev), true) = (last, cfg.enabled) {
                            let dt = ts
                                .duration_since(prev)
                                .map(|d| d.as_secs_f64())
                                .unwrap_or(0.0)
                                .min(0.05); // clamp gaps / first-sample spikes
                            let h = g[cfg.h_axis];
                            let v = g[cfg.v_axis];
                            // Acceleration as a shaped attack curve: extra gain is
                            // `strength · (speed/knee)^pow`, clamped. pow>1 keeps
                            // the slow-aim end near 1× (precise) and only ramps up
                            // as a flick's speed passes the knee.
                            let gain = 1.0 + accel_gain(&cfg, (h * h + v * v).sqrt());
                            dx += h * dt * cfg.sens_x * cfg.sgn_x * gain;
                            dy += v * dt * cfg.sens_y * cfg.sgn_y * gain;
                        }
                        last = Some(ts);
                    }
                    _ => {}
                }
            }
            if (dx != 0.0 || dy != 0.0)
                && accum
                    .lock()
                    .map(|mut a| {
                        a.0 += dx;
                        a.1 += dy;
                    })
                    .is_err()
            {
                return; // poisoned — give up the thread
            }
        }
    }
}

/// Extra gain (above 1.0) for a given combined rotation `speed` (deg/s): a
/// power curve on the speed normalised to the knee, scaled by strength and
/// clamped. Returns 0 when acceleration is disabled.
fn accel_gain(cfg: &Cfg, speed: f64) -> f64 {
    if cfg.accel <= 0.0 || speed <= 0.0 {
        return 0.0;
    }
    let norm = (speed / cfg.accel_ref).powf(cfg.accel_pow);
    (cfg.accel * norm).min(cfg.accel_max)
}

/// Resolution as f64, or 1.0 if the device didn't report one (avoids /0).
fn nonzero(r: i32) -> f64 {
    if r != 0 {
        f64::from(r)
    } else {
        1.0
    }
}

/// `v` if strictly positive, else `fallback` (guards a 0 knee → /0).
fn nonzero_f64(v: f64, fallback: f64) -> f64 {
    if v > 0.0 {
        v
    } else {
        fallback
    }
}

fn env_f64(var: &str) -> Option<f64> {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|v: &f64| *v > 0.0)
}

fn env_set(var: &str) -> bool {
    std::env::var_os(var).is_some()
}

fn axis_idx(var: &str, default: usize) -> usize {
    match std::env::var(var)
        .ok()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("rx" | "x" | "0") => 0,
        Some("ry" | "y" | "1") => 1,
        Some("rz" | "z" | "2") => 2,
        _ => default,
    }
}
