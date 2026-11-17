// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Shared host-side input plumbing — gamepad slots, gyro→mouse, controller
//! profiles. Lifted out of the (now-deleted) single-output `run_capture_zc`
//! so the multi-output backend (the only remaining backend) owns the same
//! gamepad + gyro lifecycle the single-output path used to have.
//!
//! The struct bundles everything the per-frame loop needs to handle the
//! gamepad-related `InputEvent` variants, plus the per-frame IMU drain
//! (`drain_gyro_per_frame`) and the raw-gyro-sample stream the client's
//! calibration visualizer reads (`stream_raw_gyro`).

use std::collections::HashMap;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{dispatch_gyro_mouse, dsu, gamepad, gyro_imu, CalloopData};
use app_launcher::GyroMode;
use core_proto::InputEvent;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use tracing::info;

/// Live mirror of `config.toml [gyro] mouse_enabled` (default off), refreshed
/// by the DSU thread's config poll. Off = gyro→mouse never fires; an app set to
/// [`GyroMode::Mouse`] is treated as [`GyroMode::Enabled`] (raw, via DSU).
pub static GYRO_MOUSE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Connect/disconnect state for one controller this host has observed, keyed
/// `(vendor, product)`. `connected` flips on every `GamepadConnect`/`Disconnect`;
/// `last_connected_ms` records the UNIX-ms timestamp of the most recent
/// `connected = true` transition (so the controllers tab can sort
/// most-recently-seen first even when the pad is currently away).
#[derive(Debug, Clone, Default)]
pub struct ControllerLiveness {
    pub connected: bool,
    pub last_connected_ms: i64,
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Owns every piece of state the gamepad/gyro path mutates: the per-slot
/// virtual uinput pads, the saved per-controller tuning (face swaps + gyro
/// feel), the absolute gyro cursor, the last raw-gyro sequence streamed to
/// the client, a tiny log-throttle counter, and the IMU reader handle.
pub struct HostInput {
    pub pads: gamepad::PadManager,
    pub controller_profiles: host_core::controller_profiles::ControllerProfiles,
    pub gyro_imu: gyro_imu::GyroImu,
    /// DSU motion server: client gyro → emulators (`config.toml [dsu]`).
    pub dsu: dsu::DsuServer,
    /// Absolute cursor position used by gyro→mouse aim (clamped to output).
    pub gyro_cursor: (f64, f64),
    /// Throttle counter for the gyro→mouse diagnostic log.
    pub gyro_log: u32,
    /// Last raw-IMU sequence number streamed to the client; bumps so we only
    /// send fresh samples.
    pub last_gyro_seq: u64,
    /// Per-controller liveness (connected? last seen?), keyed `(vendor, product)`.
    /// Populated from every `GamepadConnect`/`Disconnect` event and surfaced to
    /// the panel via the live `HostStatus.controllers` field.
    pub liveness: HashMap<(u16, u16), ControllerLiveness>,
    /// `slot → (vendor, product)` of the last `GamepadConnect` on that slot.
    /// Needed because `GamepadDisconnect` only carries the slot, not the ids;
    /// we look the ids up here so the matching liveness entry flips to
    /// `connected = false` without losing its `last_connected_ms` stamp.
    pub slot_ids: HashMap<u8, (u16, u16)>,
    /// Gyro policy for the *currently active* foreground app, sourced from
    /// `apps.toml`. Defaults to [`GyroMode::Enabled`] (raw to the game) so
    /// behaviour matches the pre-per-app world until something explicitly
    /// changes it via [`HostInput::set_app_gyro_mode`].
    pub active_app_gyro_mode: GyroMode,
    /// Spawned launched-app children we own (the [`std::process::Child`]
    /// returned by `app_launcher::launch_slug`), keyed by their `apps.toml` slug.
    /// Per-frame [`HostInput::reap_exited_apps`] calls `try_wait()` on each and
    /// — when a process exits — resets the active app's gyro mode to the
    /// per-app default. Reliable, decoupled from xdg-shell window lifecycle,
    /// and self-contained (no extra threads).
    pub running_apps: HashMap<String, Child>,
    /// Children DISPLACED from `running_apps` by a re-launch under the same slug.
    /// Dropping a `Child` does NOT reap the process, so a displaced handle would
    /// leave a ZOMBIE on exit — which latched `steam_session_active` and
    /// suppressed the game-redirect marker (see `lesson-zombie-pid-latches-state`,
    /// `pid_alive`). We keep the handle here and `try_wait()` it every frame
    /// alongside `running_apps` until it exits, so the process is always reaped.
    orphaned_children: Vec<(String, Child)>,
}

impl HostInput {
    /// Initialize with the gyro cursor at the center of `width`x`height`.
    /// Lazily spawns the IMU reader thread (idle when no USB/IP IMU device
    /// is present) and loads the saved controller profiles from disk.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pads: gamepad::PadManager::new(),
            controller_profiles: host_core::controller_profiles::ControllerProfiles::load(),
            gyro_imu: gyro_imu::GyroImu::spawn(),
            dsu: dsu::DsuServer::spawn(),
            gyro_cursor: (f64::from(width) / 2.0, f64::from(height) / 2.0),
            gyro_log: 0,
            last_gyro_seq: 0,
            liveness: HashMap::new(),
            slot_ids: HashMap::new(),
            active_app_gyro_mode: GyroMode::default(),
            running_apps: HashMap::new(),
            orphaned_children: Vec::new(),
        }
    }

    /// Remember a freshly-spawned app child so per-frame
    /// [`HostInput::reap_exited_apps`] can detect its exit. The render loop
    /// calls this right after `app_launcher::launch_slug` returns an `Ok(Some)`
    /// — `Ok(None)` (the built-in `desktop` slug's empty command) has no child
    /// to track. If a process is *already* tracked under the same slug (rare:
    /// a re-launch while the previous instance is still alive) the previous
    /// handle is NOT dropped — dropping a `Child` doesn't wait on it, so the old
    /// process would zombie on exit (which latched `steam_session_active` and
    /// wedged the game-redirect for a gamescope session). We keep the displaced
    /// handle in `orphaned_children` so `reap_exited_apps` still reaps it.
    pub fn track_app_child(&mut self, slug: String, child: Child) {
        if let Some(old) = self.running_apps.insert(slug.clone(), child) {
            self.orphaned_children.push((slug, old));
        }
    }

    /// Per-frame: poll every tracked launched-app child non-blockingly. On
    /// exit (`try_wait()` returns `Ok(Some(_))`) — or on an I/O error from
    /// `try_wait` itself, which on Linux usually means the child is gone —
    /// drop the entry. If at least one app exited and no other tracked apps
    /// are still alive, reset the active gyro mode to the [`GyroMode::default()`]
    /// so the desktop's "Enabled" baseline returns. (We don't try to pop the
    /// gyro stack per-slug — `LaunchApp` overwrites the active mode in any
    /// case, so this matches the prior nesting story.)
    pub fn reap_exited_apps(&mut self) {
        let mut exited = false;
        self.running_apps
            .retain(|slug, child| match child.try_wait() {
                Ok(Some(status)) => {
                    info!("app '{slug}' exited (status {status})");
                    exited = true;
                    false
                }
                Err(e) => {
                    tracing::warn!("app '{slug}' try_wait failed: {e}; dropping handle");
                    exited = true;
                    false
                }
                Ok(None) => true,
            });
        // Reap displaced (re-launched-over) children too, so none zombies. These
        // don't drive the gyro-reset side effect — they're already superseded.
        self.orphaned_children
            .retain_mut(|(slug, child)| match child.try_wait() {
                Ok(Some(status)) => {
                    info!("displaced app '{slug}' reaped (status {status})");
                    false
                }
                Err(_) => false,
                Ok(None) => true,
            });
        if exited && self.running_apps.is_empty() {
            let default = GyroMode::default();
            if self.active_app_gyro_mode != default {
                info!("no tracked apps running; resetting gyro mode to {default:?}");
                self.active_app_gyro_mode = default;
            }
        }
    }

    /// Set the active foreground app's gyro policy. Called by the render loop
    /// on LaunchApp / app-exit / desktop return. The IMU reader keeps running
    /// regardless; this only gates whether gyro→mouse aim and raw-sample relay
    /// fire each frame.
    pub fn set_app_gyro_mode(&mut self, mode: GyroMode) {
        if self.active_app_gyro_mode != mode {
            tracing::info!("active app gyro mode → {mode:?}");
        }
        self.active_app_gyro_mode = mode;
    }

    /// The gyro mode actually in force: the app's choice, with Mouse demoted to
    /// Enabled while the global gyro→mouse switch is off.
    fn gyro_mode(&self) -> GyroMode {
        match self.active_app_gyro_mode {
            GyroMode::Mouse if !GYRO_MOUSE_ENABLED.load(Ordering::Relaxed) => GyroMode::Enabled,
            m => m,
        }
    }

    /// A `(vendor, product, liveness)` snapshot suitable for building the panel's
    /// `HostStatus.controllers` field each tick. Stable order isn't required —
    /// the panel sorts client-side by (connected, last_connected_ms, name).
    pub fn liveness_snapshot(&self) -> Vec<(u16, u16, ControllerLiveness)> {
        self.liveness
            .iter()
            .map(|(&(v, p), l)| (v, p, l.clone()))
            .collect()
    }

    /// Handle one `InputEvent`. The gamepad/gyro/controller-tuning variants
    /// stay here; everything else is routed back to the caller via
    /// `dispatch_other` so it can run `dispatch_input` against its own seat.
    pub fn dispatch_input_event<F>(
        &mut self,
        data: &mut CalloopData,
        width: u32,
        height: u32,
        focus_surface: Option<&WlSurface>,
        ev: InputEvent,
        mut dispatch_other: F,
    ) where
        F: FnMut(&mut CalloopData, InputEvent),
    {
        match ev {
            InputEvent::GamepadConnect {
                slot,
                vendor,
                product,
            } => {
                self.pads.connect(slot, vendor, product);
                self.dsu.pad(slot, true);
                // Apply this controller's saved feel: face swaps to the pad,
                // gyro tuning to the IMU reader. Unsaved controllers fall
                // back to the built-in default profile.
                let profile = self.controller_profiles.get(vendor, product);
                self.pads.set_swap(slot, profile.swap_ab, profile.swap_xy);
                self.gyro_imu.set_tuning(&profile.gyro);
                // Stamp liveness so the panel's Controllers tab can sort
                // connected-and-most-recently-seen first.
                let entry = self.liveness.entry((vendor, product)).or_default();
                entry.connected = true;
                entry.last_connected_ms = now_unix_ms();
                self.slot_ids.insert(slot, (vendor, product));
            }
            InputEvent::GamepadDisconnect { slot } => {
                self.pads.disconnect(slot);
                self.dsu.pad(slot, false);
                // Flip the matching liveness entry to `connected = false` but
                // preserve its `last_connected_ms` so "Saved" rows still sort
                // by when they last appeared.
                if let Some(ids) = self.slot_ids.remove(&slot) {
                    if let Some(entry) = self.liveness.get_mut(&ids) {
                        entry.connected = false;
                    }
                }
            }
            InputEvent::GamepadButton {
                slot,
                button,
                pressed,
            } => self.pads.button(slot, button, pressed),
            InputEvent::GamepadAxis { slot, axis, value } => self.pads.axis(slot, axis, value),
            InputEvent::GamepadMotion {
                slot,
                gx,
                gy,
                gz,
                ax,
                ay,
                az,
            } => {
                // Enabled = raw gyro reaches the game. The virtual Xbox pad has no
                // gyro axes, so "raw" means the DSU server, which emulators read
                // (a no-op while `[dsu]` is off).
                let mode = self.gyro_mode();
                if mode == GyroMode::Enabled {
                    self.dsu.motion(slot, [gx, gy, gz], [ax, ay, az]);
                }
                // A client pad's gyro becomes mouse aim ONLY when the active app opted
                // into GyroMode::Mouse (apps.toml). Enabled (the default) and Disabled
                // must NOT wander the cursor.
                // This mirrors the USB/IP IMU path's gate in `drain_gyro_per_frame`;
                // without it this arm ran unconditionally, so a phone-attached gyro pad
                // flooded the host cursor regardless of GyroMode or the client's
                // wants_gyro opt-in. `GyroMouse::delta` is stateless, so there's no
                // accumulator to keep drained in the non-Mouse modes.
                if mode == GyroMode::Mouse {
                    let (dx, dy) = self.pads.gyro.delta(gx, gy, gz);
                    if dx != 0.0 || dy != 0.0 {
                        self.gyro_log = self.gyro_log.wrapping_add(1);
                        if self.gyro_log % 30 == 1 {
                            info!(
                                "gyro→mouse: dx={dx:.1} dy={dy:.1} (gx={gx:.2} gy={gy:.2} gz={gz:.2})"
                            );
                        }
                        dispatch_gyro_mouse(
                            data,
                            width,
                            height,
                            focus_surface,
                            &mut self.gyro_cursor,
                            dx,
                            dy,
                        );
                    }
                }
            }
            InputEvent::ControllerTuning {
                slot,
                vendor,
                product,
                mut profile,
            } => {
                // Live edit from the client's settings UI: apply now and
                // persist as this controller's source of truth. The host
                // *owns* the alias (set from the panel — clients don't have
                // a rename surface yet, so they relay `alias = None`); merge
                // the existing alias back in so a tuning push from the client
                // never clobbers a host-set rename.
                if profile.alias.is_none() {
                    profile.alias = self.controller_profiles.get(vendor, product).alias;
                }
                self.pads.set_swap(slot, profile.swap_ab, profile.swap_xy);
                self.gyro_imu.set_tuning(&profile.gyro);
                self.controller_profiles.set(vendor, product, profile);
                if let Err(e) = self.controller_profiles.save() {
                    tracing::warn!(
                        "persisting controller profile {vendor:04x}:{product:04x}: {e:#}"
                    );
                }
            }
            other => dispatch_other(data, other),
        }
    }

    /// Host input backstop (issue #13): release ALL held gamepad state to neutral
    /// on every live virtual pad. Called when the connected client changes (its
    /// button-up / stick-return events can never arrive) or when a stale input
    /// backlog is purged, mirroring the Windows injector's `release_all` pad path.
    /// Also drops any pending gyro motion so a departed client's last aim-rate
    /// sample doesn't leak into the next session's cursor. The pads stay up (a new
    /// client re-sends `GamepadConnect`); this only zeroes their held controls.
    pub fn release_all_pads(&mut self) {
        self.pads.neutral_all();
        // Discard accumulated-but-undrained gyro motion (aim-rate integral) so it
        // can't snap the cursor when the next client connects.
        let _ = self.gyro_imu.take();
    }

    /// Open / close the gyro IMU reader's master gate. The compositor calls
    /// this every render frame with the OR of every connected streamer's
    /// per-client `wants_gyro` flag (set during the v2 auth handshake). When
    /// it goes false the reader thread idles in 500ms sleeps — the per-app
    /// [`GyroMode`] is a finer downstream filter, not the master switch.
    /// Cheap (one atomic store).
    pub fn set_gyro_gate(&self, on: bool) {
        self.gyro_imu.set_gated(on);
    }

    /// Per-frame: drain accumulated IMU motion → mouse aim. No-op when no
    /// USB/IP IMU device is connected (the IMU reader returns 0,0), and short-
    /// circuits when the active app's [`GyroMode`] isn't [`GyroMode::Mouse`] —
    /// raw-gyro games / disabled-gyro apps don't want the cursor wandering.
    /// We still `take()` the accumulated motion in those modes so it doesn't
    /// snowball into a jump if the user switches to Mouse mid-session.
    pub fn drain_gyro_per_frame(
        &mut self,
        data: &mut CalloopData,
        width: u32,
        height: u32,
        focus_surface: Option<&WlSurface>,
    ) {
        let (gdx, gdy) = self.gyro_imu.take();
        if self.gyro_mode() != GyroMode::Mouse {
            return;
        }
        if gdx != 0.0 || gdy != 0.0 {
            self.gyro_log = self.gyro_log.wrapping_add(1);
            if self.gyro_log % 30 == 1 {
                info!("IMU gyro→mouse: dx={gdx:.1} dy={gdy:.1}");
            }
            dispatch_gyro_mouse(
                data,
                width,
                height,
                focus_surface,
                &mut self.gyro_cursor,
                gdx,
                gdy,
            );
        }
    }

    /// Per-frame: if the IMU has a fresh sample, pack `(gx, gy, gz)` as 12
    /// little-endian-f32 bytes and ship it through the supplied sender —
    /// the client's calibration visualizer reads this. `send` is called at
    /// most once per frame; the sequence dedup keeps an idle IMU silent.
    ///
    /// Gated by the active app's [`GyroMode`]: [`Enabled`](GyroMode::Enabled)
    /// and [`Mouse`](GyroMode::Mouse) both relay raw samples so the calibration
    /// UI keeps working, but [`Disabled`](GyroMode::Disabled) stays quiet — the
    /// app explicitly opted out of gyro. We still bump `last_gyro_seq` so a
    /// queued backlog doesn't burst the moment the app switches back on.
    pub fn stream_raw_gyro<F>(&mut self, mut send: F)
    where
        F: FnMut(Vec<u8>),
    {
        let (gx, gy, gz, seq) = self.gyro_imu.latest_raw();
        if seq != 0 && seq != self.last_gyro_seq {
            self.last_gyro_seq = seq;
            if self.active_app_gyro_mode == GyroMode::Disabled {
                return;
            }
            let mut pkt = Vec::with_capacity(12);
            pkt.extend_from_slice(&gx.to_le_bytes());
            pkt.extend_from_slice(&gy.to_le_bytes());
            pkt.extend_from_slice(&gz.to_le_bytes());
            send(pkt);
        }
    }
}
