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
}

/// The live listener: its cancel flag (flipped to stop) and the thread handle
/// (consulted to tell "still running" from "already exited").
struct Running {
    cancel: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl PairingManager {
    /// Build a manager sharing the control server's [`HostStatus`] mutex.
    pub fn new(status: Arc<Mutex<HostStatus>>) -> Arc<Self> {
        Arc::new(Self {
            status,
            inner: Mutex::new(None),
            epoch: AtomicU64::new(0),
        })
    }

    /// Open the pairing window: bring up the listener and surface the first PIN.
    /// **Idempotent** — if a listener is already live this is a no-op `Ok`. A
    /// bad address or identity-load failure returns `Err(msg)` for the panel.
    pub fn start(self: &Arc<Self>, addr: Option<String>) -> Result<(), String> {
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
        let me_evt = self.clone();
        let me_err = self.clone();
        let handle = std::thread::Builder::new()
            .name("pharos-pairing".into())
            .spawn(move || {
                let on_event = move |ev: PairEvent| me_evt.apply_event(epoch, ev);
                // GUI pairs one device at a time: stop the listener after the
                // first successful pairing (the CLI keeps accepting devices).
                let stop_after_pair = true;
                // Generous safety cap: a window opened from the panel and then
                // forgotten auto-closes after an hour rather than listening
                // indefinitely. A successful pair closes it immediately (above);
                // this only fires when nobody pairs in time.
                let max_window = Some(Duration::from_secs(60 * 60));
                if let Err(e) = pair::run_listener(
                    addr,
                    identity,
                    pin,
                    cancel_thread,
                    stop_after_pair,
                    max_window,
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
        tracing::info!("pairing window opened on {addr} (round {epoch})");
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
