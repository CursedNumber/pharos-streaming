// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Local control server: the compositor side of the `control-ipc` protocol.
//!
//! `pharos-panel` (the cross-platform desktop control app) connects over a Unix
//! domain socket and drives the live host: ask for status, play/stop media on
//! the virtual output, run the per-stage latency benchmark, nudge the layout.
//!
//! This module owns the *socket-facing* half. An accept thread does nothing but
//! `accept()` and hand each connection to its **own** serve thread, so no single
//! client can ever wedge the listener — a panel that connects and then stalls (or
//! stops reading) ties up only its own thread, while fresh panels keep connecting.
//! Each serve thread bridges every actionable request into a [`ControlCmd`] that
//! the render loop drains and answers via a per-command oneshot reply channel.
//! Read-only requests (`Hello`, `GetStatus`) are served entirely on the serve
//! thread from the shared [`HostStatus`] mutex — no round-trip through the render
//! loop — so the panel's status poll stays cheap and never stalls behind a slow
//! command.
//!
//! Resilience is load-bearing: the panel is optional. If the socket can't bind
//! (another host already running, a read-only runtime dir), [`ControlServer::spawn`]
//! logs and returns a dummy whose command receiver simply never yields — the
//! stream path runs unaffected. A panel disconnecting mid-session just ends its
//! serve thread; the accept loop never noticed it in the first place. Bounds keep
//! it honest under churn: a write timeout reaps a stuck writer, a reply timeout
//! reaps a render-loop stall, and [`MAX_CONTROL_CLIENTS`] caps total serve threads
//! so leaked connections can never starve the real panel.

use std::io::{BufReader, BufWriter};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use control_ipc::{Event, FrameTimingsSnapshot, HostStatus, LatencyReport, Request};

/// How long the control thread waits on the render loop for a latency-bench
/// reply before giving up and returning an error to the panel. A bench is a
/// ~few-second window plus the client's report round-trip, so 10s is generous.
const BENCH_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connection's serve thread waits on the render loop for a *non-bench*
/// command reply (play/stop/layout — all near-instant) before giving up. Generous
/// so a momentarily-busy render loop still answers, but bounded so a wedged render
/// loop can never pin a serve thread (and its connection slot) forever.
const CMD_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on concurrently-served panel connections. The panel is normally the only
/// client; the cap bounds thread growth if a client ever misbehaves (connects and
/// neither speaks nor closes) so the server can't be exhausted into refusing the
/// real panel. Excess connections are closed immediately.
const MAX_CONTROL_CLIENTS: usize = 8;

/// Write-side timeout per connection. A panel that connects but stops *reading*
/// would otherwise block its serve thread forever on a full send buffer; with this
/// the write errors out, the connection is dropped, and the slot is freed. Reads
/// stay blocking on purpose — a healthy panel poll may idle between requests, and
/// peer death on a Unix socket delivers EOF, which reaps the thread cleanly.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// A command the render loop must act on, each carrying a oneshot reply channel
/// the control thread blocks on so it can write the matching [`Event`] back to
/// the panel. Read-only requests never become a `ControlCmd` (the control thread
/// answers them directly from the shared status), so every variant here mutates
/// host state or measures it live.
pub enum ControlCmd {
    /// Spawn a media player as a Wayland client on the compositor socket.
    PlayMedia {
        path: String,
        fullscreen: bool,
        subtitles: control_ipc::Subtitles,
        audio: control_ipc::Audio,
        reply: Sender<Result<(), String>>,
    },
    /// Kill the currently-playing media child, if any.
    StopMedia { reply: Sender<Result<(), String>> },
    /// Launch a saved app (by `apps.toml` slug) into the virtual display. The
    /// render loop owns the live Wayland socket, so the spawn happens there.
    LaunchApp {
        slug: String,
        reply: Sender<Result<(), String>>,
    },
    /// Run the latency benchmark over `frames` samples; the report is produced
    /// later, when the client's `WireLatencyReport` comes back over the stream.
    RunBench {
        frames: u32,
        reply: Sender<Result<LatencyReport, String>>,
    },
    /// Record the requested layout mode (v1: file-driven, so this is advisory —
    /// the actual layout is applied by the panel writing `multi.toml`).
    SetMode {
        mode: control_ipc::LayoutMode,
        reply: Sender<Result<(), String>>,
    },
    /// Best-effort "re-read the on-disk layout" (v1: acked only).
    ReloadLayout { reply: Sender<Result<(), String>> },
    /// Enable/disable the live preview (host writes a downscaled JPEG ~2fps,
    /// auto-stopping after 20s).
    SetPreview {
        enabled: bool,
        reply: Sender<Result<(), String>>,
    },
    /// Restart the host in place. The render loop replies `Ok` (so the panel
    /// hears it before the socket dies), tears down its streamer/outputs, and
    /// re-execs the process with its original args — picking up a new on-disk
    /// layout/encoder config or recovering a wedged host.
    Relaunch { reply: Sender<Result<(), String>> },
    /// Set (or clear) a controller's user-given alias. The render loop owns the
    /// authoritative `ControllerProfiles`, so it updates the entry, persists it,
    /// and (once a host→client tuning relay exists) re-broadcasts the new alias
    /// to connected clients.
    SetControllerAlias {
        vendor: u16,
        product: u16,
        alias: Option<String>,
        reply: Sender<Result<(), String>>,
    },
    /// Fetch a one-shot per-stage frame-timings snapshot. The serve thread
    /// already extended the subscription deadline before sending us this cmd
    /// (it owns a clone of the deadline atomic), so by the time we run we're
    /// inside an active instrumentation window and the snapshot reflects
    /// live histograms. The render loop simply hands back a clone.
    GetFrameTimings {
        reply: Sender<Result<FrameTimingsSnapshot, String>>,
    },
    /// Switch the host's video codec (HEVC/AV1). The render loop persists the
    /// choice next to `multi.toml`, replies `Ok` (so the panel hears it before
    /// the socket dies), then tears down + re-execs — the fresh process reads
    /// the persisted codec and builds its NVENC sessions with it. NVENC can't
    /// change codec in place, so this rides the same relaunch path.
    SetVideoCodec {
        codec: control_ipc::VideoCodec,
        reply: Sender<Result<(), String>>,
    },
    /// W8: live pacing control applied to the running render loop (no relaunch).
    /// `auto` toggles the adaptive pacer; `fps` is the ceiling/fixed target.
    SetFps {
        auto: bool,
        fps: u32,
        reply: Sender<Result<(), String>>,
    },
    /// Live bitrate ceiling (kbit/s) applied in place to the running NVENC session.
    SetBitrate {
        kbps: u32,
        reply: Sender<Result<(), String>>,
    },
    /// Resolution ladder (Phase 2): set the target coded-height ceiling in px
    /// (`0` = Auto/native). Applied in place to the running NVENC session via
    /// dynamic-resolution reconfigure; the controller may drop below it.
    SetResolution {
        height: u32,
        reply: Sender<Result<(), String>>,
    },
    /// Resolution ladder (Phase 2): set the minimum coded-height the controller
    /// may auto-drop to (`0` = lowest rung). Floor of the adaptive range.
    SetResolutionFloor {
        height: u32,
        reply: Sender<Result<(), String>>,
    },
}

/// Handle to the running control server. Holding it documents ownership; the
/// thread is detached and dies with the process, so there's nothing to join.
pub struct ControlServer {
    _private: (),
}

impl ControlServer {
    /// Bind the control socket and spawn the accept thread. Returns the handle
    /// plus the receiver the render loop drains for [`ControlCmd`]s.
    ///
    /// The `framing_deadline` handle lets every serve thread arm the render
    /// loop's per-stage instrumentation on `GetFrameTimings`; the
    /// `controllers_deadline` does the same for the per-controller liveness
    /// vector on `SubscribeControllers`. Both default off so the cost is zero
    /// with no panel attached.
    ///
    /// Never panics: a bind failure is logged and a dummy server is returned
    /// whose command channel is immediately closed (the render loop's `try_recv`
    /// just yields nothing forever), so streaming is unaffected with no panel.
    pub fn spawn(
        status: Arc<Mutex<HostStatus>>,
        framing_deadline: crate::multi::SubscriberDeadline,
        controllers_deadline: crate::multi::SubscriberDeadline,
    ) -> (ControlServer, Receiver<ControlCmd>) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ControlCmd>();
        // The pairing window runs off the render loop (its own QUIC endpoint), so
        // it shares the status mutex directly rather than bouncing through a
        // ControlCmd. One manager per host run, shared across serve threads.
        // announce_mdns: the pairing window is this host's only mDNS presence
        // (the compositor has no standalone process-wide advertise like the
        // Windows host does), so `true` — the always-open listener below IS the
        // host's discovery presence, and there's no second registration to ghost.
        let pairing = crate::pairing::PairingManager::new(status.clone(), true);
        // Always-open, host-lifetime pairing (issue #17): a PIN-bearing listener
        // up at startup so a fresh Trireme pairs with no panel action and no
        // separate `pharos-compositor pair` terminal. It keeps accepting devices
        // (rotating the PIN) until the process exits or the lockout trips; the
        // panel's StartPairing still opens on-demand windows independently.
        match pairing.start_host_lifetime(None) {
            Ok(()) => tracing::info!("always-open pairing listener up (issue #17)"),
            Err(e) => tracing::warn!("always-open pairing listener not started: {e}"),
        }
        let listener = match control_ipc::bind() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    "control socket bind failed ({e}); panel control disabled (streaming unaffected)"
                );
                // cmd_tx drops here → cmd_rx yields nothing forever.
                return (ControlServer { _private: () }, cmd_rx);
            }
        };
        tracing::info!("control server on {}", control_ipc::socket_path().display());
        let spawned = std::thread::Builder::new()
            .name("pharos-control".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    status,
                    pairing,
                    framing_deadline,
                    controllers_deadline,
                    cmd_tx,
                )
            });
        if let Err(e) = spawned {
            tracing::warn!("control thread spawn failed ({e}); panel control disabled");
        }
        (ControlServer { _private: () }, cmd_rx)
    }
}

/// Decrements the live-connection counter when a serve thread exits — however it
/// exits (clean EOF, IO error, or panic). Held inside the serve closure so the
/// slot is freed for sure.
struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Accept panel clients and serve **each on its own thread**, so no single client
/// can wedge the listener: the accept loop is back at `accept()` the instant a
/// connection is handed off. A client that connects and then stalls (or stops
/// reading) only ever ties up its own serve thread — bounded by the write timeout
/// and [`MAX_CONTROL_CLIENTS`]. A dead client (EOF / IO error) is non-fatal.
fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    status: Arc<Mutex<HostStatus>>,
    pairing: Arc<crate::pairing::PairingManager>,
    framing_deadline: crate::multi::SubscriberDeadline,
    controllers_deadline: crate::multi::SubscriberDeadline,
    cmd_tx: Sender<ControlCmd>,
) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("control accept: {e}");
                continue;
            }
        };
        // Refuse (close) past the cap rather than let a leaked connection grow the
        // thread count without bound and eventually starve the real panel.
        if active.load(Ordering::Relaxed) >= MAX_CONTROL_CLIENTS {
            tracing::warn!(
                "control: refusing connection ({MAX_CONTROL_CLIENTS} clients already active)"
            );
            drop(stream); // closing the freshly-accepted stream gives the client EOF
            continue;
        }
        // A stuck *reader* on the client side must not pin our serve thread on a
        // full send buffer forever; reads stay blocking (idle polls are normal).
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

        active.fetch_add(1, Ordering::Relaxed);
        let guard = ConnGuard(active.clone());
        let status = status.clone();
        let pairing = pairing.clone();
        let framing_deadline = framing_deadline.clone();
        let controllers_deadline = controllers_deadline.clone();
        let cmd_tx = cmd_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("pharos-control-conn".into())
            .spawn(move || {
                let _guard = guard; // decrements `active` on any exit
                tracing::info!("panel connected");
                match serve_client(
                    stream,
                    &status,
                    &pairing,
                    &framing_deadline,
                    &controllers_deadline,
                    &cmd_tx,
                ) {
                    Ok(()) => tracing::info!("panel disconnected"),
                    Err(e) => tracing::debug!("panel session ended: {e}"),
                }
            });
        if let Err(e) = spawned {
            // A failed spawn dropped the closure (and its `ConnGuard`), so `active`
            // is already back down — nothing to undo, just note it.
            tracing::warn!("control: serve-thread spawn failed ({e}); connection dropped");
        }
    }
}

/// Serve one connected panel: one [`Event`] per [`Request`], in order, until EOF.
fn serve_client(
    stream: std::os::unix::net::UnixStream,
    status: &Arc<Mutex<HostStatus>>,
    pairing: &Arc<crate::pairing::PairingManager>,
    framing_deadline: &crate::multi::SubscriberDeadline,
    controllers_deadline: &crate::multi::SubscriberDeadline,
    cmd_tx: &Sender<ControlCmd>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    loop {
        // Lenient read: an unknown request from a *newer* panel replies Error and
        // keeps the link, instead of a deserialize error tearing it down (which
        // the panel would see as a silent reconnect blip with its action lost).
        let event = match control_ipc::read_request_lenient(&mut reader)? {
            None => return Ok(()), // clean EOF — panel closed
            Some(Ok(req)) => handle_request(
                req,
                status,
                pairing,
                framing_deadline,
                controllers_deadline,
                cmd_tx,
            ),
            Some(Err(e)) => {
                tracing::debug!("control: unparseable request ({e}); replying Error, keeping link");
                Event::Error {
                    msg: format!("unsupported or malformed request: {e}"),
                }
            }
        };
        control_ipc::write_event(&mut writer, &event)?;
    }
}

/// How long one `GetFrameTimings` request keeps frame-timing instrumentation
/// hot in the render loop. The panel re-asks every 1 Hz while its Performance
/// view is open, so a 5s TTL gives it ~5x headroom against a missed poll
/// (e.g. a slow render-loop tick) without paying the per-frame `Instant::now()`
/// cost forever when the panel is closed.
const FRAME_TIMINGS_TTL: Duration = Duration::from_secs(5);

/// Map one request to the single event we reply with. Read-only requests are
/// answered here from the shared status; actionable ones bounce through the
/// render loop via a [`ControlCmd`] + oneshot reply.
fn handle_request(
    req: Request,
    status: &Arc<Mutex<HostStatus>>,
    pairing: &Arc<crate::pairing::PairingManager>,
    framing_deadline: &crate::multi::SubscriberDeadline,
    controllers_deadline: &crate::multi::SubscriberDeadline,
    cmd_tx: &Sender<ControlCmd>,
) -> Event {
    match req {
        Request::Hello { version } => {
            if version != control_ipc::PROTOCOL_VERSION {
                tracing::warn!(
                    "panel protocol v{version} != host v{} (continuing best-effort)",
                    control_ipc::PROTOCOL_VERSION
                );
            }
            Event::Hello {
                version: control_ipc::PROTOCOL_VERSION,
                host: hostname(),
            }
        }
        Request::GetStatus => {
            let snapshot = status
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|_| default_status());
            Event::Status(snapshot)
        }
        Request::PlayMedia {
            path,
            fullscreen,
            subtitles,
            audio,
            ..
        } => {
            // `output` is ignored in v1 (single virtual output / mirror); the
            // render loop plays onto the shared compositor socket.
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::PlayMedia {
                    path,
                    fullscreen,
                    subtitles,
                    audio,
                    reply,
                })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: None,
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::StopMedia => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::StopMedia { reply }).is_err() {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: None,
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::RunLatencyBench { frames } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::RunBench { frames, reply }).is_err() {
                return Event::Error {
                    msg: "host render loop unavailable".into(),
                };
            }
            // The render loop only sends the reply once the client's report
            // arrives (or it gives up); bound our own wait so the panel can't
            // hang if the render loop never answers.
            match rx.recv_timeout(BENCH_REPLY_TIMEOUT) {
                Ok(Ok(report)) => Event::Latency(report),
                Ok(Err(e)) => Event::Error { msg: e },
                Err(_) => Event::Error {
                    msg: "latency bench timed out".into(),
                },
            }
        }
        Request::SetLayoutMode { mode } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::SetMode { mode, reply }).is_err() {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some("layout mode is file-driven; restart applies a new layout".into()),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::ReloadLayout => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::ReloadLayout { reply }).is_err() {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some("restart applies a new layout".into()),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetPreview { enabled } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::SetPreview { enabled, reply })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: None,
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::LaunchApp { slug } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::LaunchApp { slug, reply }).is_err() {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: None,
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::StartPairing { addr } => {
            // Driven directly (off the render loop). On success we reply with a
            // fresh Status so the panel's pairing card shows the PIN at once.
            match pairing.start(addr) {
                Ok(()) => {
                    let snapshot = status
                        .lock()
                        .map(|s| s.clone())
                        .unwrap_or_else(|_| default_status());
                    Event::Status(snapshot)
                }
                Err(e) => Event::Error { msg: e },
            }
        }
        Request::CancelPairing => {
            pairing.cancel();
            Event::Ack {
                ok: true,
                msg: None,
            }
        }
        Request::Relaunch => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::Relaunch { reply }).is_err() {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some("relaunching host".into()),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetControllerAlias {
            vendor,
            product,
            alias,
        } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::SetControllerAlias {
                    vendor,
                    product,
                    alias,
                    reply,
                })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: None,
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::GetFrameTimings => {
            // Arm the per-stage instrumentation *before* we ask for the
            // snapshot: if this is the first poke after a cold start, the
            // render loop's next iteration will start sampling, and our
            // snapshot here reads whatever's accumulated so far (often zero
            // on the first call — that's expected, the panel will see real
            // data on its next ~1Hz poll).
            framing_deadline.poke(FRAME_TIMINGS_TTL);
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::GetFrameTimings { reply }).is_err() {
                return Event::Error {
                    msg: "host render loop unavailable".into(),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(snap)) => Event::FrameTimings(snap),
                Ok(Err(e)) => Event::Error { msg: e },
                Err(_) => Event::Error {
                    msg: "frame-timings snapshot timed out".into(),
                },
            }
        }
        Request::SubscribeControllers { ttl_ms } => {
            // Arm the controllers-vector gate. The render loop's status push
            // checks `controllers_deadline.is_active()` and skips the
            // (expensive) `build_controllers_status` walk when it's stale —
            // exactly the same shape as `GetFrameTimings`/framing instrumentation.
            //
            // This is a pure-subscribe request (no payload needed back), so we
            // don't bounce through the render loop: the next ~1 Hz status push
            // will pick up the freshly-armed gate and rebuild the vector, and
            // the panel sees it on its next `GetStatus` poll. Reply
            // immediately with an Ack so the panel can move on.
            //
            // Clamp the TTL so a misbehaving panel can't keep the gate hot
            // forever: 30s is well above the panel's render-tick poll cadence
            // and matches the spirit of `FRAME_TIMINGS_TTL` (5s, but the
            // panel re-pokes at >>5 Hz on its render loop).
            let ttl = Duration::from_millis(ttl_ms.min(30_000) as u64);
            controllers_deadline.poke(ttl);
            Event::Ack {
                ok: true,
                msg: None,
            }
        }
        Request::SetVideoCodec { codec } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::SetVideoCodec { codec, reply })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some(format!(
                        "switching to {} — restarting encoder",
                        codec.label()
                    )),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetHdrMode { enabled } => {
            // HDR is a session-level property (the 10-bit NVENC format is fixed at
            // session create), so a live toggle needs a process relaunch — which
            // composes with the client `wants_hdr` negotiation and an app-declared
            // HDR surface (task 6 + the deferred session auto-switch). Accepted but
            // informational for now.
            tracing::info!(
                "control: SetHdrMode({enabled}) — HDR session switch not yet runtime-wired"
            );
            Event::Ack {
                ok: false,
                msg: Some(
                    "HDR engages from the client's HDR request + an app-declared HDR surface; a manual runtime toggle isn't wired yet"
                        .into(),
                ),
            }
        }
        Request::SetFps { auto, fps } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::SetFps { auto, fps, reply })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some(if auto {
                        format!("pace → auto (cap {fps})")
                    } else {
                        format!("pace → fixed {fps}")
                    }),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetBitrate { kbps } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx.send(ControlCmd::SetBitrate { kbps, reply }).is_err() {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some(format!("bitrate ceiling → {kbps} kbps")),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetResolution { height } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::SetResolution { height, reply })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some(if height == 0 {
                        "resolution → auto (native)".to_string()
                    } else {
                        format!("resolution ceiling → {height}p")
                    }),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetResolutionFloor { height } => {
            let (reply, rx) = std::sync::mpsc::channel();
            if cmd_tx
                .send(ControlCmd::SetResolutionFloor { height, reply })
                .is_err()
            {
                return Event::Ack {
                    ok: false,
                    msg: Some("host render loop unavailable".into()),
                };
            }
            match rx.recv_timeout(CMD_REPLY_TIMEOUT) {
                Ok(Ok(())) => Event::Ack {
                    ok: true,
                    msg: Some(if height == 0 {
                        "min resolution → none (lowest rung)".to_string()
                    } else {
                        format!("min resolution → {height}p")
                    }),
                },
                Ok(Err(e)) => Event::Ack {
                    ok: false,
                    msg: Some(e),
                },
                Err(_) => Event::Ack {
                    ok: false,
                    msg: Some("host render loop dropped the request".into()),
                },
            }
        }
        Request::SetStreamMode { mirror } => {
            // Plain flag — doesn't touch the NVENC session, so no render-loop
            // round-trip (unlike SetBitrate/SetFps). The status push reads it and
            // publishes HostStatus.stream_mode; the transport fan-out that
            // actually honors mirror is a later stage (MIRROR_EXTEND_PLAN.md).
            crate::multi::set_mirror_enabled(mirror);
            Event::Ack {
                ok: true,
                msg: Some(if mirror {
                    "stream mode → mirror (shared encode, clamped to weakest client)".into()
                } else {
                    "stream mode → single (per-client adaptive)".into()
                }),
            }
        }
        Request::SetInputEnabled { pubkey, enabled } => {
            // Live per-device input gate (the canvas green toggle). The set lives
            // in `transport` (one process, one seat), checked at the input task —
            // no render-loop round-trip, no NVENC.
            match parse_pubkey_hex(&pubkey) {
                Some(pk) => {
                    transport::set_input_enabled(pk, enabled);
                    Event::Ack {
                        ok: true,
                        msg: Some(format!(
                            "input {} for {}…",
                            if enabled { "enabled" } else { "disabled" },
                            &pubkey[..pubkey.len().min(8)]
                        )),
                    }
                }
                None => Event::Error {
                    msg: format!("bad pubkey hex (len {})", pubkey.len()),
                },
            }
        }
        Request::ForgetPeer { pubkey } => {
            // Issue #19: rewrite peers.toml without this peer (shared host-core
            // helper — atomic, clears primary if it named it) and close any live
            // session it holds. File + transport globals only, so it's handled
            // on the serve thread like SetInputEnabled — no render-loop
            // round-trip. Future connects are denied by the authorizer's
            // per-connection peers.toml re-read.
            match host_core::pair::forget_peer(&pubkey) {
                Ok(out) if out.removed => {
                    tracing::info!(
                        "panel: forgot peer {}… (primary={}, live sessions closed={})",
                        &pubkey[..pubkey.len().min(8)],
                        out.was_primary,
                        out.sessions_closed
                    );
                    Event::Ack {
                        ok: true,
                        msg: Some(if out.sessions_closed > 0 {
                            "peer forgotten; its live session was closed".into()
                        } else {
                            "peer forgotten".into()
                        }),
                    }
                }
                Ok(_) => Event::Ack {
                    ok: false,
                    msg: Some("no such paired peer".into()),
                },
                Err(e) => Event::Ack {
                    ok: false,
                    msg: Some(format!("forget peer: {e}")),
                },
            }
        }
        Request::SetPrimaryPeer { pubkey } => {
            // Issue #19: persist primary_pubkey in peers.toml and apply it to
            // the live pre-emption policy (the helper re-syncs the transport
            // global). None clears. Same serve-thread handling as ForgetPeer.
            match host_core::pair::set_primary_peer(pubkey.as_deref()) {
                Ok(()) => Event::Ack {
                    ok: true,
                    msg: Some(match pubkey.as_deref() {
                        Some(pk) => format!(
                            "primary client → {}… (others no longer preempt it)",
                            &pk[..pk.len().min(8)]
                        ),
                        None => "primary client cleared (newest-wins preempt)".into(),
                    }),
                },
                Err(e) => Event::Ack {
                    ok: false,
                    msg: Some(format!("set primary: {e}")),
                },
            }
        }
    }
}

/// Parse a 64-char hex Ed25519 pubkey (case-insensitive) into 32 bytes; `None`
/// if it isn't exactly 32 bytes of hex. Producers emit lowercase.
pub(crate) fn parse_pubkey_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Spawn a media player (mpv) as a Wayland client on the compositor's socket.
///
/// Never uses `?` — a bad path or a missing mpv must fail the *request*, never
/// kill the host. Returns the live `Child` on success, or a human error string.
/// `--no-config` keeps the user's mpv profile out of the host; `--loop=inf`
/// matches the panel's "play this clip" expectation; we point it at the
/// compositor's `WAYLAND_DISPLAY` and scrub `DISPLAY` so it can't fall back to
/// X11/XWayland.
pub fn spawn_media_player(
    path: &str,
    fullscreen: bool,
    subtitles: &control_ipc::Subtitles,
    audio: &control_ipc::Audio,
    socket_name: &std::ffi::OsStr,
) -> Result<std::process::Child, String> {
    let mut cmd = std::process::Command::new("mpv");
    cmd.arg("--no-config")
        .arg("--really-quiet")
        .arg("--loop=inf");
    if fullscreen {
        cmd.arg("--fullscreen");
    }
    // Audio track. `Auto` leaves mpv's default; `Off` mutes (no track);
    // `Track` forces a specific embedded track by 1-based id.
    match audio {
        control_ipc::Audio::Auto => {}
        control_ipc::Audio::Off => {
            cmd.arg("--aid=no");
        }
        control_ipc::Audio::Track(aid) => {
            cmd.arg(format!("--aid={aid}"));
        }
    }
    // Subtitles. `Auto` = mpv's default plus `--sub-auto=fuzzy` so it also picks
    // up loosely-named sidecars (`movie.en.srt`); `Off` disables tracks +
    // sidecar loading; `File` forces an explicit external file.
    match subtitles {
        control_ipc::Subtitles::Auto => {
            cmd.arg("--sub-auto=fuzzy");
        }
        control_ipc::Subtitles::Off => {
            cmd.arg("--sid=no").arg("--sub-auto=no");
        }
        control_ipc::Subtitles::Track(sid) => {
            cmd.arg(format!("--sid={sid}"));
        }
        control_ipc::Subtitles::File(sub) => {
            cmd.arg(format!("--sub-file={sub}"));
        }
    }
    cmd.arg(path)
        .env("WAYLAND_DISPLAY", socket_name)
        .env_remove("DISPLAY");
    cmd.spawn().map_err(|e| format!("spawn mpv {path}: {e}"))
}

/// Launch a saved app (by its `apps.toml` slug) into the virtual display on
/// `socket_name`. The render-loop side of [`ControlCmd::LaunchApp`], shared by
/// the single- and multi-output capture loops, and by the `launch-app` CLI verb
/// once it reaches the host. Mirrors [`spawn_media_player`]: never `?`, returns a
/// human error so a bad slug fails the request, not the host. A no-op entry (the
/// Desktop built-in, empty command) succeeds with `Ok(None)`. `client` is the
/// live client's pairing pubkey (hex), which picks its per-client app config.
pub fn launch_app(
    slug: &str,
    socket_name: &std::ffi::OsStr,
    output_fps: u32,
    output_size: Option<(u32, u32)>,
    client: Option<String>,
) -> Result<Option<std::process::Child>, String> {
    // `output_fps` is the virtual output's refresh (the single global pacer), so
    // an app's `FpsCap::Auto` resolves to "match the stream rate". `output_size`
    // is the primary output's native mode — it sizes a steam-session's gamescope
    // output to the streamed surface (issue #38).
    let mut ctx = app_launcher::LaunchContext::wayland(socket_name.to_os_string())
        .with_output_fps(output_fps)
        .with_client(client);
    if let Some((w, h)) = output_size {
        ctx = ctx.with_output_size(w, h);
    }
    app_launcher::launch_slug(slug, &ctx).map_err(|e| e.to_string())
}

/// Build the wire app catalog (the saved apps + built-in Desktop, each with its
/// box-art bytes) to push to Trireme clients on the app relay stream. Art files
/// over 2 MiB are skipped (the client draws a placeholder) so a stray huge image
/// can't bloat the relay. Reads `apps.toml` fresh each call.
pub fn build_app_catalog(running: &std::collections::HashSet<String>) -> Vec<core_proto::AppInfo> {
    use app_launcher::config::AppKind;
    let catalog = app_launcher::config::load_with_builtins();
    catalog
        .apps
        .iter()
        .map(|a| {
            let kind = match a.kind {
                AppKind::App => core_proto::apps::app_kind::APP,
                AppKind::Game => core_proto::apps::app_kind::GAME,
                AppKind::Desktop => core_proto::apps::app_kind::DESKTOP,
                AppKind::Media => core_proto::apps::app_kind::MEDIA,
            };
            let art = a
                .box_art
                .as_deref()
                .and_then(read_capped_art)
                .unwrap_or_default();
            core_proto::AppInfo {
                id: a.slug.clone(),
                name: a.name.clone(),
                kind,
                art,
                running: running.contains(&a.slug),
            }
        })
        .collect()
}

/// The set of currently-running app slugs (one /proc pass). The render loop polls
/// this ~1 Hz to re-push the catalog the moment an app starts or exits, so the
/// client's running highlight tracks reality without an apps.toml edit.
pub fn running_app_slugs() -> std::collections::HashSet<String> {
    let catalog = app_launcher::config::load_with_builtins();
    app_launcher::launch::running_slugs(&catalog.apps)
}

/// End (kill the whole process tree of) the app with this slug.
pub fn end_app(slug: &str) -> Result<(), String> {
    let catalog = app_launcher::config::load_with_builtins();
    let entry = catalog
        .apps
        .iter()
        .find(|a| a.slug == slug)
        .ok_or_else(|| format!("no such app `{slug}`"))?;
    app_launcher::launch::end_app(entry).map_err(|e| e.to_string())
}

/// The mtime of `apps.toml`, if present. The render loops poll this (~1 Hz) so a
/// panel edit re-pushes the catalog to connected clients without a host restart.
pub fn apps_toml_mtime() -> Option<std::time::SystemTime> {
    let p = app_launcher::apps_path().ok()?;
    std::fs::metadata(p).ok()?.modified().ok()
}

/// Read an art file if it's a sane size (≤2 MiB), else `None`.
fn read_capped_art(path: &str) -> Option<Vec<u8>> {
    const MAX_ART: u64 = 2 * 1024 * 1024;
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_ART {
        tracing::warn!(
            "app art {path} is {} bytes (>2MiB); skipping relay",
            meta.len()
        );
        return None;
    }
    std::fs::read(path).ok()
}

/// Drain control commands during the **pre-stream client-wait** (the window
/// before the first Trireme connects, where the render loop hasn't started
/// draining `ctrl_rx` yet). Honors [`ControlCmd::Relaunch`] — returning `true`
/// so the caller can break to teardown + re-exec — which is exactly the "recover
/// a wedged host / apply new config before streaming" case. Every other command
/// is answered with a "not streaming yet" error so the panel never hangs on the
/// 10s reply timeout, then surfaces a misleading "render loop dropped it". Never
/// blocks.
pub fn drain_prestream(ctrl_rx: &Receiver<ControlCmd>) -> bool {
    let mut relaunch = false;
    while let Ok(cmd) = ctrl_rx.try_recv() {
        match cmd {
            ControlCmd::Relaunch { reply } => {
                let _ = reply.send(Ok(()));
                relaunch = true;
            }
            ControlCmd::PlayMedia { reply, .. }
            | ControlCmd::StopMedia { reply }
            | ControlCmd::LaunchApp { reply, .. }
            | ControlCmd::SetMode { reply, .. }
            | ControlCmd::SetPreview { reply, .. }
            | ControlCmd::ReloadLayout { reply } => {
                let _ = reply.send(Err("host is waiting for a client to connect".into()));
            }
            ControlCmd::RunBench { reply, .. } => {
                let _ = reply.send(Err("host is waiting for a client to connect".into()));
            }
            ControlCmd::SetControllerAlias {
                vendor,
                product,
                alias,
                reply,
            } => {
                // No live HostInput yet (we're still pre-stream), but the on-disk
                // controller-profiles.toml is the source of truth either way —
                // load, mutate, save. The render loop reloads it on stream-start.
                let res = apply_alias_on_disk(vendor, product, alias);
                let _ = reply.send(res);
            }
            ControlCmd::GetFrameTimings { reply } => {
                // No render-loop sampling is happening yet (we're still
                // pre-stream), so the snapshot is honestly all-zero. Hand it
                // back rather than an error: the panel's view renders empty
                // rows, which is the correct truth.
                let _ = reply.send(Ok(FrameTimingsSnapshot::default()));
            }
            ControlCmd::SetVideoCodec { codec, reply } => {
                // Even while idle: persist the choice next to multi.toml and
                // relaunch, so a panel codec switch takes effect immediately
                // (the fresh process reads the override on start-up).
                let res = crate::pair::config_dir()
                    .map_err(|e| e.to_string())
                    .and_then(|dir| {
                        crate::multi::write_codec_override(&dir.join("multi.toml"), codec)
                            .map_err(|e| format!("persist video codec: {e}"))
                    });
                match res {
                    Ok(()) => {
                        tracing::info!(
                            "panel: video codec → {} (idle; relaunching encoder)",
                            codec.label()
                        );
                        let _ = reply.send(Ok(()));
                        relaunch = true;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            ControlCmd::SetFps { reply, .. } => {
                // No render loop yet (idle/pre-stream) — pacing applies to the live
                // loop only; set `fps`/`fps_auto` in multi.toml for the default.
                let _ = reply.send(Err(
                    "no active stream — pacing applies once a client is connected".into(),
                ));
            }
            ControlCmd::SetBitrate { reply, .. } => {
                let _ = reply.send(Err(
                    "no active stream — bitrate applies once a client is connected".into(),
                ));
            }
            ControlCmd::SetResolution { reply, .. } => {
                let _ = reply.send(Err(
                    "no active stream — resolution applies once a client is connected".into(),
                ));
            }
            ControlCmd::SetResolutionFloor { reply, .. } => {
                let _ = reply.send(Err(
                    "no active stream — resolution applies once a client is connected".into(),
                ));
            }
        }
    }
    relaunch
}

/// Update one controller's alias **directly on disk**, bypassing the live
/// `HostInput::controller_profiles`. Used by the pre-stream `drain_prestream`
/// path (no live HostInput exists yet) and as the fallback when the live
/// profile mutate path can't reach the render loop. The main render loop
/// updates its in-memory copy *and* persists; both paths converge on the same
/// `controller-profiles.toml`.
pub fn apply_alias_on_disk(vendor: u16, product: u16, alias: Option<String>) -> Result<(), String> {
    let mut profiles = host_core::controller_profiles::ControllerProfiles::load();
    let mut profile = profiles.get(vendor, product);
    profile.alias = alias.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });
    profiles.set(vendor, product, profile);
    profiles.save().map_err(|e| e.to_string())
}

/// Build the panel-facing per-controller liveness vector from the union of (a)
/// every entry in `controller-profiles.toml` (so a saved-but-absent controller
/// still shows up) and (b) every controller the host has seen this run (so a
/// newly-plugged pad that has never been tuned shows up too). Called once per
/// render-loop tick from inside the status-mutex critical section.
pub fn build_controllers_status(
    profiles: &host_core::controller_profiles::ControllerProfiles,
    liveness: &[(u16, u16, crate::host_input::ControllerLiveness)],
) -> Vec<control_ipc::ControllerStatus> {
    use std::collections::HashMap;
    let mut by_key: HashMap<(u16, u16), control_ipc::ControllerStatus> = HashMap::new();
    // (a) Saved profiles → seed every known controller with its alias.
    for (key, profile) in &profiles.controllers {
        let (v, p) = parse_vid_pid(key);
        by_key.insert(
            (v, p),
            control_ipc::ControllerStatus {
                vendor: v,
                product: p,
                alias: profile.alias.clone(),
                connected: false,
                last_connected_ms: None,
                bus: crate::gamepad::controller_bus(v, p),
            },
        );
    }
    // (b) Live liveness → overlay connected/last-seen; create rows for
    // controllers that have never been tuned.
    for (v, p, l) in liveness {
        let entry = by_key
            .entry((*v, *p))
            .or_insert_with(|| control_ipc::ControllerStatus {
                vendor: *v,
                product: *p,
                alias: None,
                connected: false,
                last_connected_ms: None,
                bus: crate::gamepad::controller_bus(*v, *p),
            });
        entry.connected = l.connected;
        if l.last_connected_ms != 0 {
            entry.last_connected_ms = Some(l.last_connected_ms);
        }
        // Re-probe each tick so an attach/detach (USB plug-in, BT (re)pair)
        // flips the pill live without waiting for a host restart. The probe
        // is a single `evdev::enumerate()` walk per controller — cheap at
        // controller counts (single digits).
        entry.bus = crate::gamepad::controller_bus(*v, *p);
    }
    by_key.into_values().collect()
}

/// Parse a `"vvvv:pppp"` profile-store key back into raw `(vendor, product)`.
/// Unparseable keys are silently dropped (zeroed) — they came from a broken
/// hand-edit and the row would be meaningless on the panel anyway.
fn parse_vid_pid(key: &str) -> (u16, u16) {
    let mut it = key.splitn(2, ':');
    let v = it
        .next()
        .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
        .unwrap_or(0);
    let p = it
        .next()
        .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
        .unwrap_or(0);
    (v, p)
}

/// Re-exec this process with its original argv (the `ControlCmd::Relaunch`
/// teardown). On success it never returns — `execvp` replaces the process image,
/// so the freshly-bound Wayland/QUIC/control sockets and NVENC sessions all come
/// up clean. The caller must drop its streamer/outputs first so those sockets
/// are released before the new image rebinds the same ports. Returns the
/// `io::Error` only if the exec itself failed (then the host should exit).
#[cfg(unix)]
pub fn reexec_self() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return e,
    };
    // If the binary was replaced on disk since launch (a `cargo build` between
    // start and Relaunch — the normal "rebuild then relaunch to apply" flow),
    // Linux reports the path with a trailing " (deleted)", which won't exec.
    // Strip it so we re-exec the freshly-built binary at the real path.
    let exe = match exe.to_str() {
        Some(s) if s.ends_with(" (deleted)") => {
            std::path::PathBuf::from(s.trim_end_matches(" (deleted)"))
        }
        _ => exe,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    tracing::info!("relaunch: re-exec {} {:?}", exe.display(), args);
    // `exec` only returns on failure.
    std::process::Command::new(exe).args(args).exec()
}

/// Map the wire latency report (codec four-CC + ms fields) the client sends back
/// into the panel-facing [`LatencyReport`] (codec as a string). All ms fields
/// copy across one-for-one.
pub fn wire_report_to_control(r: &core_proto::WireLatencyReport) -> LatencyReport {
    LatencyReport {
        codec: fourcc_to_string(r.codec_fourcc),
        samples: r.samples,
        rtt_ms: r.rtt_ms,
        clock_offset_ms: r.clock_offset_ms,
        compose_ms: r.compose_ms,
        encode_ms: r.encode_ms,
        host_send_ms: r.host_send_ms,
        nexus_ms: r.nexus_ms,
        nexus_uncertainty_ms: r.nexus_uncertainty_ms,
        buffer_ms: r.buffer_ms,
        decode_ms: r.decode_ms,
        present_ms: r.present_ms,
        total_ms: r.total_ms,
    }
}

/// Render a four-CC (`*b"hevc"`) as an upper-cased codec label (`"HEVC"`),
/// trimming any trailing NULs / spaces.
fn fourcc_to_string(fourcc: [u8; 4]) -> String {
    let s: String = fourcc
        .iter()
        .filter(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    let s = s.trim().to_ascii_uppercase();
    if s.is_empty() {
        "HEVC".into()
    } else {
        s
    }
}

/// A zeroed status used only as a fallback if the shared mutex is poisoned.
pub fn default_status() -> HostStatus {
    HostStatus {
        hostname: hostname(),
        streaming: false,
        clients: Vec::new(),
        outputs: Vec::new(),
        media: None,
        pairing: None,
        controllers: Vec::new(),
        video_codec: None,
        // The full compositor is controllable; only the Windows status-only host
        // sets this true.
        read_only: false,
        // The full compositor serves every control verb (issue #22): the panel
        // gates each button on the specific capability, so advertise them all.
        caps: control_ipc::HostCaps::all(),
        // Healthy until the encode classifier / watchdog says otherwise (P3+).
        gpu_health: control_ipc::GpuHealth::Healthy,
        stream_mode: control_ipc::StreamMode::Single,
        hdr_active: None,
        primary_pubkey: transport::primary_peer_hex(),
    }
}

/// Local hostname for the `Hello` reply and status snapshots. Mirrors host-core's
/// private helper (gethostname(3)); falls back to `"pharos"`.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes up to len-1 bytes + NUL into our buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..len]) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "pharos".into()
}

#[cfg(test)]
mod tests {
    //! Robustness tests for the accept loop. They drive the *real* [`accept_loop`]
    //! over a temp Unix socket (no env, no `bind()` — each test gets a unique path
    //! so they run in parallel) and assert the properties that were broken by the
    //! old single-threaded serve: one stalled client must not block another, the
    //! server must survive a long run of reconnects, a mid-message disconnect must
    //! be isolated, and leaked connections must be capped rather than unbounded.
    //!
    //! Only the read-only `Hello`/`GetStatus` path is exercised, so no render loop
    //! is needed — the held `Receiver<ControlCmd>` just keeps the channel open.

    use super::*;
    use control_ipc::{Event, Request, PROTOCOL_VERSION};
    use std::io::BufReader;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn unique_sock_path() -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pharos-ctl-test-{}-{n}.sock", std::process::id()))
    }

    /// Bind a temp control socket and run the real accept loop on it. The listener
    /// is bound *before* the accept thread starts, so a client can connect (and
    /// queue) immediately — no startup race. Returns the path plus the kept-alive
    /// command receiver.
    fn spawn_test_server() -> (std::path::PathBuf, Receiver<ControlCmd>) {
        let path = unique_sock_path();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test control socket");
        let status = Arc::new(Mutex::new(default_status()));
        let pairing = crate::pairing::PairingManager::new(status.clone(), true);
        // A standalone deadline handle whose backing atomic is dropped when the
        // test ends. The serve threads can `poke` it without affecting any
        // real render loop.
        let framing_deadline = test_subscriber_deadline();
        let controllers_deadline = test_subscriber_deadline();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ControlCmd>();
        std::thread::spawn(move || {
            accept_loop(
                listener,
                status,
                pairing,
                framing_deadline,
                controllers_deadline,
                cmd_tx,
            )
        });
        (path, cmd_rx)
    }

    /// A fresh, isolated [`SubscriberDeadline`] for tests — bypasses the real
    /// `FrameTimings::subscribers_handle()` so the tests don't need a render
    /// loop. Constructed via the type's `Clone` impl from a freshly-built
    /// `FrameTimings` (its `Drop` is a no-op).
    fn test_subscriber_deadline() -> crate::multi::SubscriberDeadline {
        crate::multi::test_only_subscriber_deadline()
    }

    /// Connect with a client-side read timeout so a server regression fails the
    /// test fast instead of hanging the whole suite.
    fn connect(path: &std::path::Path) -> UnixStream {
        let s = UnixStream::connect(path).expect("connect test control socket");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    /// One `Hello` round-trip on an existing stream; returns the server's reply.
    fn hello_on(stream: &UnixStream) -> std::io::Result<Option<Event>> {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        control_ipc::write_request(
            &mut writer,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )?;
        control_ipc::read_event(&mut reader)
    }

    /// Connect, `Hello`, return the reply, drop the connection (clean EOF).
    fn hello(path: &std::path::Path) -> Event {
        let s = connect(path);
        hello_on(&s)
            .expect("hello io")
            .expect("server should reply with an event")
    }

    #[test]
    fn serves_hello_and_status() {
        let (path, _rx) = spawn_test_server();
        assert!(
            matches!(hello(&path), Event::Hello { version, .. } if version == PROTOCOL_VERSION),
            "Hello must echo the protocol version",
        );
        // GetStatus is answered straight from the shared status mutex.
        let s = connect(&path);
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        control_ipc::write_request(&mut w, &Request::GetStatus).unwrap();
        assert!(matches!(
            control_ipc::read_event(&mut r).unwrap(),
            Some(Event::Status(_))
        ));
    }

    /// The headline regression test: with the old single-threaded serve, a client
    /// that connected and went silent pinned the accept loop forever, so the next
    /// client never got served. Per-connection threads must keep the silent one
    /// isolated.
    #[test]
    fn a_silent_client_does_not_block_others() {
        let (path, _rx) = spawn_test_server();
        // Connect and send *nothing* (no newline-terminated request), hold it open.
        let silent = UnixStream::connect(&path).unwrap();
        // A well-behaved client must still be served promptly.
        assert!(matches!(hello(&path), Event::Hello { .. }));
        drop(silent);
    }

    /// Survive a long run of connect → Hello → disconnect cycles (the documented
    /// failure was connection-refused after ~a dozen).
    #[test]
    fn survives_many_reconnect_cycles() {
        let (path, _rx) = spawn_test_server();
        for i in 0..64 {
            assert!(
                matches!(hello(&path), Event::Hello { .. }),
                "reconnect cycle {i} should be served",
            );
        }
    }

    /// A client that sends a partial (un-terminated) message then disconnects must
    /// only kill its own connection, never the server.
    #[test]
    fn partial_message_then_disconnect_is_isolated() {
        let (path, _rx) = spawn_test_server();
        {
            let mut bad = UnixStream::connect(&path).unwrap();
            use std::io::Write;
            bad.write_all(b"{\"kind\":\"Hel").unwrap(); // no newline → never a full line
                                                        // `bad` drops here → EOF mid-message; the server logs and moves on.
        }
        assert!(matches!(hello(&path), Event::Hello { .. }));
    }

    /// Leaked (silent, never-closing) connections are capped, and freeing a slot
    /// lets a fresh client in again — so a leak can never permanently lock out the
    /// real panel.
    #[test]
    fn caps_concurrent_clients_and_recovers() {
        let (path, _rx) = spawn_test_server();
        let mut held = Vec::new();
        for _ in 0..MAX_CONTROL_CLIENTS {
            // A completed Hello proves the server has accepted this connection and
            // is now parked reading — i.e. the slot is counted — before we move on.
            let s = connect(&path);
            assert!(matches!(hello_on(&s).unwrap(), Some(Event::Hello { .. })));
            held.push(s); // keep it open → stays counted
        }
        // At the cap: the next connection is refused (closed) → EOF, no reply.
        let extra = connect(&path);
        assert!(
            matches!(hello_on(&extra), Ok(None) | Err(_)),
            "a connection past the cap must be closed without a reply",
        );
        // Free one slot; the server decrements asynchronously, so retry briefly.
        drop(held.pop());
        let deadline = Instant::now() + Duration::from_secs(5);
        let recovered = loop {
            if let Ok(Some(Event::Hello { .. })) = hello_on(&connect(&path)) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(recovered, "a freed slot must accept a new client again");
    }
}
