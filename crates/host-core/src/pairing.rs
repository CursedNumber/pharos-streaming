// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Pairing-window manager: the host side of the panel's "pair a new device"
//! flow, shared by every Pharos host backend (the Linux Smithay `compositor`
//! and the Windows `pharos-win` host).
//!
//! `pharos-panel` sends [`control_ipc::Request::StartPairing`]; the host's
//! control server hands it to this manager, which brings up [`crate::pair`]'s
//! SPAKE2 listener on its **own thread** and mirrors the listener's live state —
//! the current PIN, the listen address, and the most recent attempt outcome —
//! into the shared [`HostStatus`]. The panel sees that via its ~1 Hz `GetStatus`
//! poll (and the immediate reply to `StartPairing`), so no server-push channel
//! is needed: the request/reply control protocol stays untouched.
//!
//! It runs entirely off the render loop — pairing has its own QUIC endpoint and
//! must keep working whether or not a video stream is live — so the control
//! serve threads drive it directly through a shared `Arc<PairingManager>`.
//! Resilience matches the rest of the control plane: a bad address or a failed
//! identity load fails the *request*, never the host; a listener that dies logs
//! and clears the active flag.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use control_ipc::{HostStatus, PairOutcome, PairingState};
use core_crypto::Identity;

use crate::pair::{self, PairEvent};

/// Panel-opened windows auto-close after this long without a pair (see
/// [`PairingManager::start`]); the host-lifetime listener passes `None`.
const PANEL_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Default pairing listen address — mirrors the `pharos-host pair` CLI default
/// so a device's saved host/port keeps working whichever way you pair.
pub const DEFAULT_PAIR_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(
    Ipv4Addr::UNSPECIFIED,
    core_proto::ports::PAIR,
));

/// Owns the pairing listener's lifecycle and mirrors its state into the shared
/// [`HostStatus`]. Cheap to clone the `Arc`; one instance per host run.
pub struct PairingManager {
    status: Arc<Mutex<HostStatus>>,
    inner: Mutex<Option<Running>>,
    /// Monotonic round id, bumped by every `start`/`cancel`. The listener thread
    /// stamps each event with the round it was spawned for; [`apply_event`]
    /// drops events whose round is no longer current. Without this a torn-down
    /// thread's trailing `Stopped`/`Failed`/`Listening` (it may not exit for
    /// seconds) could clobber a freshly-opened round's PIN/active state.
    epoch: AtomicU64,
    /// Whether an open window announces the host over mDNS — see [`Self::new`].
    announce_mdns: bool,
    /// Latched `true` when the brute-force lockout trips ([`PairEvent::LockedOut`]):
    /// from then on both [`Self::start`] and [`Self::start_host_lifetime`] refuse
    /// to spawn a listener, so the "relaunch to re-enable" contract holds even
    /// against a panel StartPairing. A fresh process starts with `false`, which
    /// is what clears it (issue #17).
    locked: AtomicBool,
}

/// The live listener: its cancel flag (flipped to stop) and the thread handle
/// (consulted to tell "still running" from "already exited").
struct Running {
    cancel: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
    /// True when this is the always-open, host-lifetime listener
    /// ([`PairingManager::start_host_lifetime`]) rather than a panel-opened
    /// 60-minute window ([`PairingManager::start`]). It makes [`PairingManager::cancel`]
    /// a no-op: with always-open pairing the listener is a permanent host
    /// property, so a stray panel Cancel must never withdraw mDNS and leave the
    /// host undiscoverable/unpairable until a relaunch (issue #17 BUG A). Only
    /// the 8-wrong-PIN lockout or a relaunch may stop it.
    host_lifetime: bool,
}

impl PairingManager {
    /// Build a manager sharing the control server's [`HostStatus`] mutex.
    ///
    /// `announce_mdns`: whether an open pairing window registers the host's
    /// mDNS instance. Pass `true` when the pairing window is the host's only
    /// discovery presence (the Linux compositor); `false` when the host
    /// already advertises for its whole lifetime (the Windows stream host) —
    /// a second registration conflict-renames to "… (2)" and the ghost
    /// outlives the window in client caches (see `pair::run_listener`).
    pub fn new(status: Arc<Mutex<HostStatus>>, announce_mdns: bool) -> Arc<Self> {
        Arc::new(Self {
            status,
            inner: Mutex::new(None),
            epoch: AtomicU64::new(0),
            announce_mdns,
            locked: AtomicBool::new(false),
        })
    }

    /// Open the pairing window: bring up the listener and surface the first PIN.
    /// **Idempotent** — if a listener is already live this is a no-op `Ok`. A
    /// bad address or identity-load failure returns `Err(msg)` for the panel.
    ///
    /// This is the panel-triggered variant: it pairs one device at a time
    /// (`stop_after_pair = true`) and auto-closes after [`PANEL_WINDOW`] if
    /// nobody pairs. See [`Self::start_host_lifetime`] for the always-open one.
    pub fn start(self: &Arc<Self>, addr: Option<String>) -> Result<(), String> {
        self.start_inner(addr, true, Some(PANEL_WINDOW), false)
    }

    /// Open an **always-open** pairing listener for the host's whole lifetime
    /// (issue #17): it keeps accepting devices (rotating the PIN after each one)
    /// and never auto-closes. Host binaries call this once at startup so a
    /// PIN-bearing listener is live without the panel having to open one. Same
    /// idempotence + lockout rules as [`Self::start`].
    pub fn start_host_lifetime(self: &Arc<Self>, addr: Option<String>) -> Result<(), String> {
        self.start_inner(addr, false, None, true)
    }

    /// Shared body of [`Self::start`] / [`Self::start_host_lifetime`]. Refuses to
    /// spawn once the brute-force lockout has latched (`self.locked`): only a
    /// fresh process clears it, which is what makes the lockout require a
    /// relaunch even against a panel StartPairing.
    ///
    /// `host_lifetime` marks the always-open listener so [`Self::cancel`] leaves
    /// it running (issue #17 BUG A); a panel-opened window passes `false` and
    /// cancels normally.
    fn start_inner(
        self: &Arc<Self>,
        addr: Option<String>,
        stop_after_pair: bool,
        max_window: Option<Duration>,
        host_lifetime: bool,
    ) -> Result<(), String> {
        // Lockout gate: once tripped, stay down until the process restarts.
        if self.locked.load(Ordering::SeqCst) {
            return Err(
                "pairing locked after failed attempts; relaunch the host to re-enable".into(),
            );
        }

        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "pairing state poisoned".to_string())?;

        // Already pairing? Leave the live round alone (don't rotate its PIN).
        // A handle that has *finished* is stale — fall through and start fresh.
        if let Some(r) = guard.as_ref() {
            if !r.handle.is_finished() && !r.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
        }

        let addr: SocketAddr = match addr.as_deref() {
            Some(a) => a
                .parse()
                .map_err(|e| format!("bad pair address '{a}': {e}"))?,
            None => DEFAULT_PAIR_ADDR,
        };

        let id_path = pair::identity_path().map_err(|e| format!("identity path: {e}"))?;
        let identity =
            Identity::load_or_create(&id_path).map_err(|e| format!("load identity: {e}"))?;

        // Claim a fresh round id so any trailing events from a prior thread are
        // ignored from here on (see `epoch`). Must precede the status prime.
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;

        // Mint the first PIN here so the StartPairing reply already carries it
        // (no "generating…" flash); the listener rotates it after each device.
        let pin = pair::generate_pin();
        self.set_pairing(PairingState {
            active: true,
            addr: addr.to_string(),
            pin: Some(pin.clone()),
            last: None,
            always_on: host_lifetime,
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let announce_mdns = self.announce_mdns;
        let me_evt = self.clone();
        let me_err = self.clone();
        let handle = std::thread::Builder::new()
            .name("pharos-pairing".into())
            .spawn(move || {
                let on_event = move |ev: PairEvent| me_evt.apply_event(epoch, ev);
                if let Err(e) = pair::run_listener(
                    addr,
                    identity,
                    pin,
                    cancel_thread,
                    stop_after_pair,
                    max_window,
                    announce_mdns,
                    on_event,
                ) {
                    // Bind failure (port in use) or a runtime error: report it
                    // through the same status field and clear the active flag.
                    tracing::warn!("pair listener exited: {e:#}");
                    me_err.apply_event(
                        epoch,
                        PairEvent::Failed {
                            reason: format!("listener error: {e}"),
                        },
                    );
                    me_err.apply_event(epoch, PairEvent::Stopped);
                }
            })
            .map_err(|e| format!("spawn pairing thread: {e}"))?;

        *guard = Some(Running {
            cancel,
            handle,
            host_lifetime,
        });
        tracing::info!(
            "pairing listener opened on {addr} (round {epoch}, stop_after_pair={stop_after_pair}, host_lifetime={host_lifetime})"
        );
        Ok(())
    }

    /// Close the pairing window. Idempotent; the listener thread observes the
    /// cancel flag within ~200ms and exits, withdrawing its mDNS broadcast.
    ///
    /// **No-op for the always-open, host-lifetime listener** (issue #17 BUG A):
    /// with always-open pairing the listener is a permanent host property (on
    /// Linux it's the compositor's only mDNS discovery presence), so a panel
    /// Cancel must never be able to tear it down and leave the host
    /// undiscoverable/unpairable until a relaunch. Only the 8-wrong-PIN lockout
    /// or a relaunch may stop it. A panel-opened 60-minute window
    /// (`host_lifetime = false`) still cancels normally.
    pub fn cancel(&self) {
        if let Ok(guard) = self.inner.lock() {
            if let Some(r) = guard.as_ref() {
                // Leave a live host-lifetime listener alone: don't cancel it,
                // don't clear inner, don't withdraw mDNS or drop the PIN. A
                // finished handle here is stale (the thread already exited, e.g.
                // a transient bind loss), so let that fall through to a real
                // close rather than pretend it's still running.
                if r.host_lifetime && !r.handle.is_finished() {
                    tracing::debug!("ignoring cancel: always-open pairing listener stays up");
                    return;
                }
            }
        }
        // Bump the round id first so any trailing event from the thread we're
        // about to detach is ignored by `apply_event`.
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(r) = guard.as_ref() {
                r.cancel.store(true, Ordering::Relaxed);
            }
            // Detach: the thread observes the cancel flag and frees its own
            // endpoint on exit. We don't join (the control thread mustn't block);
            // the epoch bump makes its late events inert regardless of timing.
            *guard = None;
        }
        if let Ok(mut st) = self.status.lock() {
            if let Some(p) = st.pairing.as_mut() {
                p.active = false;
                p.pin = None;
            }
        }
        tracing::info!("pairing window closed");
    }

    /// Fold one listener event into the shared status (PIN rotation / outcome).
    /// Events stamped with a superseded `round` are dropped, so a torn-down
    /// thread can't clobber a newer round's PIN/active state.
    fn apply_event(&self, round: u64, ev: PairEvent) {
        if self.epoch.load(Ordering::SeqCst) != round {
            return;
        }
        let Ok(mut st) = self.status.lock() else {
            return;
        };
        // Re-check under the lock: `cancel` bumps the epoch *before* it takes the
        // status lock, so this closes the window where a round-R event passed the
        // top check, then a cancel landed, and we'd otherwise resurrect round R.
        if self.epoch.load(Ordering::SeqCst) != round {
            return;
        }
        // `always_on` is primed by `start_inner` (via `set_pairing`) and left
        // untouched by the mutating arms below, so a host-lifetime round keeps
        // it `true` across PIN rotations; only the terminal arms clear it. This
        // insert-default is a belt-and-suspenders fallback for the (unreached
        // in practice) case where an event lands before the prime.
        let p = st.pairing.get_or_insert_with(|| PairingState {
            active: true,
            addr: DEFAULT_PAIR_ADDR.to_string(),
            pin: None,
            last: None,
            always_on: false,
        });
        match ev {
            PairEvent::Listening { addr, pin } => {
                p.active = true;
                p.addr = addr.to_string();
                p.pin = Some(pin);
            }
            PairEvent::Attempt { peer } => {
                p.last = Some(PairOutcome::Attempt {
                    peer: peer.to_string(),
                });
            }
            PairEvent::Paired { pubkey_hex, alias } => {
                p.last = Some(PairOutcome::Paired {
                    pubkey: pubkey_hex,
                    alias,
                });
            }
            PairEvent::Failed { reason } => {
                p.last = Some(PairOutcome::Failed { reason });
            }
            PairEvent::LockedOut { attempts } => {
                // Latch the lockout so any later start()/start_host_lifetime()
                // refuses until a relaunch. Surface a clearly-locked state to the
                // panel/tray: no active listener, no PIN, and a Failed outcome
                // whose reason spells out the relaunch requirement (reuses the
                // existing wire field — no new control-ipc variant needed).
                self.locked.store(true, Ordering::SeqCst);
                p.active = false;
                p.pin = None;
                // The listener is down for good (relaunch to re-enable): drop
                // the always-on flag so the panel/tray don't claim pairing is
                // still always available.
                p.always_on = false;
                p.last = Some(PairOutcome::Failed {
                    reason: format!(
                        "pairing locked: {attempts} failed attempts; relaunch the host to re-enable"
                    ),
                });
            }
            PairEvent::Stopped => {
                p.active = false;
                p.pin = None;
                p.always_on = false;
            }
        }
    }

    /// Replace the whole pairing field (used to prime the first PIN).
    fn set_pairing(&self, state: PairingState) {
        if let Ok(mut st) = self.status.lock() {
            st.pairing = Some(state);
        }
    }
}

#[cfg(test)]
mod tests {
    //! The lockout latch (issue #17): a `LockedOut` event must flip the manager
    //! into a locked state and make a subsequent `start` / `start_host_lifetime`
    //! refuse to spawn a listener until the process is relaunched. The pairing
    //! `run_loop`'s own failure-counting is exercised by the `pair.rs` tests via
    //! the `PHAROS_PAIR_MAX_FAILURES` override; here we test the manager half in
    //! isolation, driving `apply_event` directly (no real handshake needed).

    use super::*;

    fn blank_status() -> Arc<Mutex<HostStatus>> {
        Arc::new(Mutex::new(HostStatus {
            hostname: "test".into(),
            streaming: false,
            clients: vec![],
            outputs: vec![],
            media: None,
            pairing: None,
            controllers: vec![],
            video_codec: None,
            read_only: false,
            caps: control_ipc::HostCaps::all(),
            gpu_health: control_ipc::GpuHealth::Healthy,
            stream_mode: control_ipc::StreamMode::Single,
            hdr_active: None,
            primary_pubkey: None,
        }))
    }

    #[test]
    fn locked_out_event_latches_and_blocks_restart() {
        let mgr = PairingManager::new(blank_status(), false);
        // Not locked initially.
        assert!(!mgr.locked.load(Ordering::SeqCst));

        // Simulate the current round emitting LockedOut (round 0 is current with
        // no prior start; apply_event's epoch check passes).
        mgr.apply_event(0, PairEvent::LockedOut { attempts: 8 });

        // The latch is set and the shared status reflects a clearly-locked state:
        // no active listener, no PIN, and a Failed outcome naming the lockout.
        assert!(mgr.locked.load(Ordering::SeqCst));
        let st = mgr.status.lock().unwrap();
        let p = st.pairing.as_ref().expect("pairing state set");
        assert!(!p.active);
        assert!(p.pin.is_none());
        match &p.last {
            Some(PairOutcome::Failed { reason }) => {
                assert!(reason.starts_with("pairing locked"), "reason: {reason}");
                assert!(reason.contains("relaunch"));
            }
            other => panic!("expected a locked Failed outcome, got {other:?}"),
        }
        drop(st);

        // Both entry points now refuse until a relaunch (a fresh process would
        // build a new manager with locked=false).
        assert!(mgr.start(None).is_err(), "start must refuse when locked");
        assert!(
            mgr.start_host_lifetime(None).is_err(),
            "start_host_lifetime must refuse when locked"
        );
    }

    /// Isolate `PHAROS_CONFIG_DIR` to a unique temp dir so `start_inner`'s
    /// identity load/create doesn't touch the real config. Env is process-global,
    /// so this takes the shared env lock (also held by `pair.rs`'s run_loop tests)
    /// and returns the guard: the caller holds it for the whole test so no other
    /// env-mutating test can clobber `PHAROS_CONFIG_DIR` mid-run. Returns the dir
    /// (for cleanup) and the guard (to keep alive).
    fn isolate_config_dir(tag: &str) -> (std::path::PathBuf, std::sync::MutexGuard<'static, ()>) {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let guard = crate::pair::env_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pharos-pairmgr-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PHAROS_CONFIG_DIR", &dir);
        (dir, guard)
    }

    /// Wait (briefly) until the manager's live listener reports itself running,
    /// so `cancel()` under test sees a not-yet-finished handle rather than a
    /// still-spawning one.
    fn wait_active(mgr: &Arc<PairingManager>) {
        for _ in 0..100 {
            let up = mgr
                .inner
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| !r.handle.is_finished());
            if up {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("listener never came up");
    }

    /// BUG A (issue #17): a `cancel()` on the always-open, host-lifetime listener
    /// is a NO-OP: the listener stays up (inner still Some, status still active)
    /// so a stray panel Cancel can't brick discovery/pairing until a relaunch.
    #[test]
    fn cancel_is_noop_for_host_lifetime_listener() {
        let (dir, _env) = isolate_config_dir("hostlife");
        let mgr = PairingManager::new(blank_status(), false);
        // Bind an ephemeral loopback listener so no real port/mDNS is claimed.
        mgr.start_host_lifetime(Some("127.0.0.1:0".into()))
            .expect("start_host_lifetime");
        wait_active(&mgr);

        // A panel Cancel must not tear it down.
        mgr.cancel();

        assert!(
            mgr.inner.lock().unwrap().is_some(),
            "host-lifetime listener must survive cancel() (inner still Some)"
        );
        let st = mgr.status.lock().unwrap();
        let p = st.pairing.as_ref().expect("pairing state");
        assert!(
            p.active,
            "always-open listener stays active through cancel()"
        );
        assert!(p.always_on, "the always_on flag is surfaced to the panel");
        assert!(
            p.pin.is_some(),
            "the PIN is retained (nothing was torn down)"
        );
        drop(st);

        std::env::remove_var("PHAROS_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The complement: a panel-opened window (`host_lifetime = false`) still
    /// cancels normally: inner cleared, status inactive, PIN dropped.
    #[test]
    fn cancel_stops_a_panel_window_as_before() {
        let (dir, _env) = isolate_config_dir("panelwin");
        let mgr = PairingManager::new(blank_status(), false);
        mgr.start(Some("127.0.0.1:0".into())).expect("start");
        wait_active(&mgr);
        // The panel window is not always-on.
        assert!(
            !mgr.status
                .lock()
                .unwrap()
                .pairing
                .as_ref()
                .unwrap()
                .always_on
        );

        mgr.cancel();

        assert!(
            mgr.inner.lock().unwrap().is_none(),
            "a panel window must be torn down by cancel() (inner None)"
        );
        let st = mgr.status.lock().unwrap();
        let p = st.pairing.as_ref().expect("pairing state");
        assert!(!p.active, "cancelled panel window is inactive");
        assert!(p.pin.is_none(), "cancelled panel window drops its PIN");
        drop(st);

        std::env::remove_var("PHAROS_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
