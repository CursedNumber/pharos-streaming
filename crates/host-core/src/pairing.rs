// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

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
        self.start_inner(addr, true, Some(PANEL_WINDOW))
    }

    /// Open an **always-open** pairing listener for the host's whole lifetime
    /// (issue #17): it keeps accepting devices (rotating the PIN after each one)
    /// and never auto-closes. Host binaries call this once at startup so a
    /// PIN-bearing listener is live without the panel having to open one. Same
    /// idempotence + lockout rules as [`Self::start`].
    pub fn start_host_lifetime(self: &Arc<Self>, addr: Option<String>) -> Result<(), String> {
        self.start_inner(addr, false, None)
    }

    /// Shared body of [`Self::start`] / [`Self::start_host_lifetime`]. Refuses to
    /// spawn once the brute-force lockout has latched (`self.locked`): only a
    /// fresh process clears it, which is what makes the lockout require a
    /// relaunch even against a panel StartPairing.
    fn start_inner(
        self: &Arc<Self>,
        addr: Option<String>,
        stop_after_pair: bool,
        max_window: Option<Duration>,
    ) -> Result<(), String> {
        // Lockout gate: once tripped, stay down until the process restarts.
        if self.locked.load(Ordering::SeqCst) {
            return Err("pairing locked after failed attempts; relaunch the host to re-enable".into());
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

        *guard = Some(Running { cancel, handle });
        tracing::info!(
            "pairing listener opened on {addr} (round {epoch}, stop_after_pair={stop_after_pair})"
        );
        Ok(())
    }

    /// Close the pairing window. Idempotent; the listener thread observes the
    /// cancel flag within ~200ms and exits, withdrawing its mDNS broadcast.
    pub fn cancel(&self) {
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
        let p = st.pairing.get_or_insert_with(|| PairingState {
            active: true,
            addr: DEFAULT_PAIR_ADDR.to_string(),
            pin: None,
            last: None,
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
                p.last = Some(PairOutcome::Failed {
                    reason: format!(
                        "pairing locked: {attempts} failed attempts; relaunch the host to re-enable"
                    ),
                });
            }
            PairEvent::Stopped => {
                p.active = false;
                p.pin = None;
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
}
