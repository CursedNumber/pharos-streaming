// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Reconnecting control **client** — the shared bridge a panel or tray uses to
//! reach a running host. Pulled out of `pharos-panel` so the panel and the tray
//! use ONE cross-platform implementation instead of duplicating it.
//!
//! It owns a single background thread holding the control stream, pumps
//! [`Request`]s out, forwards [`Event`]s back into a cached [`LinkState`], and
//! self-issues a `GetStatus` ~1 Hz so live banners stay fresh without the caller
//! polling. On connect failure / disconnect it retries every couple of seconds.
//! It **never panics**.
//!
//! ## Transport (local-first)
//! - **Local** is the default: a Unix-domain socket on Linux ([`crate::connect`]);
//!   on Windows there is no UDS, so the local host is reached over loopback TCP
//!   `127.0.0.1:`[`core_proto::ports::CONTROL`] — the port `pharos-host` serves.
//! - **Remote** (a host on another machine) is the opt-in: set
//!   `PHAROS_CONTROL_ADDR=<host[:port]>` and the client dials it over TCP.
//!
//! The contract callers see is the small [`Handle`] behind [`handle`], with
//! [`Handle::send_request`] (non-blocking) + [`Handle::drain_events`]; tabs read
//! the cached [`Handle::status`] / [`Handle::connected`] for live banners.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};

use crate::{Event, FrameTimingsSnapshot, HostStatus, Request};

/// A snapshot of the link to the host, cached for the UI to read cheaply every
/// frame. Cloneable and small.
#[derive(Debug, Clone, Default)]
pub struct LinkState {
    /// True while the worker holds a live socket to the host.
    pub connected: bool,
    /// The host's hostname, learned from the `Hello` handshake.
    pub host: Option<String>,
    /// The host's protocol version, learned from the `Hello` handshake. Lets the
    /// UI gate v2-only controls (pairing/relaunch) on an older host rather than
    /// send a request it would silently drop.
    pub host_version: Option<u32>,
    /// The most recent [`HostStatus`] the host reported (cleared on disconnect so
    /// a stale "streaming" banner never lingers).
    pub status: Option<HostStatus>,
    /// Most recent per-stage [`FrameTimingsSnapshot`]. Cleared on disconnect.
    pub frame_timings: Option<FrameTimingsSnapshot>,
    /// Per-stage rolling history of mean-µs values for sparkline rendering.
    pub frame_history: FrameHistory,
}

/// Number of samples in each stage's rolling history (the sparkline length).
pub const FRAME_HISTORY_LEN: usize = 30;

/// Per-stage rolling history of mean-µs samples. One `Vec` per stage, each
/// capped to [`FRAME_HISTORY_LEN`]. Order matches [`FrameTimingsSnapshot`]'s
/// field order so a UI can iterate stages and histories in lockstep.
#[derive(Debug, Clone, Default)]
pub struct FrameHistory {
    pub dispatch: Vec<u32>,
    pub ctrl: Vec<u32>,
    pub input: Vec<u32>,
    pub gyro: Vec<u32>,
    pub render: Vec<u32>,
    pub post: Vec<u32>,
    pub pace: Vec<u32>,
    pub status: Vec<u32>,
}

impl FrameHistory {
    fn push(&mut self, snap: &FrameTimingsSnapshot) {
        let series = [
            (&mut self.dispatch, snap.dispatch.mean_us),
            (&mut self.ctrl, snap.ctrl.mean_us),
            (&mut self.input, snap.input.mean_us),
            (&mut self.gyro, snap.gyro.mean_us),
            (&mut self.render, snap.render.mean_us),
            (&mut self.post, snap.post.mean_us),
            (&mut self.pace, snap.pace.mean_us),
            (&mut self.status, snap.status.mean_us),
        ];
        for (vec, sample) in series {
            vec.push(sample);
            if vec.len() > FRAME_HISTORY_LEN {
                let drop_n = vec.len() - FRAME_HISTORY_LEN;
                vec.drain(0..drop_n);
            }
        }
    }
}

/// The caller-facing handle: send requests, drain inbound events, read the
/// cached link state. Reachable from anywhere via [`handle`].
pub struct Handle {
    /// Outbound: caller → worker. `None` on a target with no worker.
    tx: Option<Sender<Request>>,
    /// Inbound: worker → caller, drained once per frame.
    rx: Mutex<Option<Receiver<Event>>>,
    /// Cached link state, updated as we drain events.
    link: Mutex<LinkState>,
}

impl Handle {
    /// Queue a request to the host. Non-blocking; silently dropped if the worker
    /// is gone (the UI reflects that via [`connected`](Handle::connected)).
    pub fn send_request(&self, req: Request) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(req);
        }
    }

    /// Drain every event the worker has produced since the last call, updating
    /// the cached [`LinkState`] as a side effect. Returns them in arrival order.
    pub fn drain_events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        let guard = match self.rx.lock() {
            Ok(g) => g,
            Err(_) => return out,
        };
        if let Some(rx) = guard.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(ev) => out.push(ev),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }
        drop(guard);
        if !out.is_empty() {
            if let Ok(mut link) = self.link.lock() {
                for ev in &out {
                    apply_event(&mut link, ev);
                }
            }
        }
        out
    }

    /// The cached link state (cheap clone). Use this for live banners.
    pub fn link(&self) -> LinkState {
        self.link.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Convenience: is a live socket to the host currently held?
    pub fn connected(&self) -> bool {
        self.link.lock().map(|l| l.connected).unwrap_or(false)
    }

    /// Convenience: the latest [`HostStatus`], if any.
    pub fn status(&self) -> Option<HostStatus> {
        self.link.lock().ok().and_then(|l| l.status.clone())
    }

    /// Convenience: the latest per-stage [`FrameTimingsSnapshot`], if any.
    pub fn frame_timings(&self) -> Option<FrameTimingsSnapshot> {
        self.link.lock().ok().and_then(|l| l.frame_timings.clone())
    }

    /// Convenience: the per-stage rolling sparkline history. Cleared on
    /// disconnect.
    pub fn frame_history(&self) -> FrameHistory {
        self.link
            .lock()
            .map(|l| l.frame_history.clone())
            .unwrap_or_default()
    }

    /// The connected host's protocol version (from the `Hello` handshake), if
    /// known yet.
    pub fn host_version(&self) -> Option<u32> {
        self.link.lock().ok().and_then(|l| l.host_version)
    }
}

/// Whether the connected host is new enough for the v2 control surface (the
/// pairing window and relaunch). An unknown version (still handshaking) is
/// treated optimistically as supported; a *known-older* host returns false.
pub fn host_supports_v2() -> bool {
    handle().host_version().map(|v| v >= 2).unwrap_or(true)
}

/// Update the cached link state from one inbound event.
fn apply_event(link: &mut LinkState, ev: &Event) {
    match ev {
        Event::Hello { version, host } => {
            link.connected = true;
            link.host = Some(host.clone());
            link.host_version = Some(*version);
        }
        Event::Status(s) => {
            link.connected = true;
            if link.host.is_none() {
                link.host = Some(s.hostname.clone());
            }
            link.status = Some(s.clone());
        }
        Event::MediaStarted { .. } | Event::MediaStopped | Event::Ack { .. } => {
            link.connected = true;
        }
        Event::Latency(_) => {
            link.connected = true;
        }
        Event::Error { .. } => {
            // An app-level error is not a disconnect; leave `connected` alone.
        }
        Event::FrameTimings(snap) => {
            link.connected = true;
            link.frame_history.push(snap);
            link.frame_timings = Some(snap.clone());
        }
    }
}

/// The process-wide handle, lazily initialised on first access. Initialising it
/// spawns the worker (unix/windows) or returns a no-op handle (other targets).
pub fn handle() -> &'static Handle {
    static CELL: OnceLock<Handle> = OnceLock::new();
    CELL.get_or_init(init)
}

// ---------------------------------------------------------------------------
// The real background worker (unix + windows)
// ---------------------------------------------------------------------------

#[cfg(any(unix, windows))]
fn init() -> Handle {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<Request>();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<Event>();
    spawn_worker(req_rx, ev_tx);
    Handle {
        tx: Some(req_tx),
        rx: Mutex::new(Some(ev_rx)),
        link: Mutex::new(LinkState::default()),
    }
}

/// The control transport: a local Unix socket (Linux, default), a local loopback
/// TCP connection (Windows, default — no UDS there), or a remote TCP connection
/// when `PHAROS_CONTROL_ADDR` is set. Both speak the identical newline-JSON
/// codec; only the byte pipe differs, and both support the per-read timeout +
/// writer clone the worker needs.
#[cfg(any(unix, windows))]
enum ControlStream {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
}

#[cfg(any(unix, windows))]
impl ControlStream {
    fn set_read_timeout(&self, d: Option<std::time::Duration>) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            ControlStream::Unix(s) => s.set_read_timeout(d),
            ControlStream::Tcp(s) => s.set_read_timeout(d),
        }
    }
    fn try_clone(&self) -> std::io::Result<ControlStream> {
        Ok(match self {
            #[cfg(unix)]
            ControlStream::Unix(s) => ControlStream::Unix(s.try_clone()?),
            ControlStream::Tcp(s) => ControlStream::Tcp(s.try_clone()?),
        })
    }
}

#[cfg(any(unix, windows))]
impl std::io::Read for ControlStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            ControlStream::Unix(s) => s.read(buf),
            ControlStream::Tcp(s) => s.read(buf),
        }
    }
}

#[cfg(any(unix, windows))]
impl std::io::Write for ControlStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            ControlStream::Unix(s) => s.write(buf),
            ControlStream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            ControlStream::Unix(s) => s.flush(),
            ControlStream::Tcp(s) => s.flush(),
        }
    }
}

/// Open the control transport: remote TCP if `PHAROS_CONTROL_ADDR` is set, else
/// the local transport — UDS on unix, loopback TCP on Windows.
#[cfg(any(unix, windows))]
fn open_control_stream() -> std::io::Result<ControlStream> {
    if let Some(addr) = remote_control_addr() {
        return crate::tcp_connect(addr).map(ControlStream::Tcp);
    }
    #[cfg(unix)]
    {
        crate::connect().map(ControlStream::Unix)
    }
    #[cfg(not(unix))]
    {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], core_proto::ports::CONTROL));
        crate::tcp_connect(addr).map(ControlStream::Tcp)
    }
}

/// The remote control endpoint from `PHAROS_CONTROL_ADDR`, or `None` for the
/// local transport. Accepts `host:port`, or a bare `host`/IP (defaulting to
/// [`core_proto::ports::CONTROL`]), and resolves names.
#[cfg(any(unix, windows))]
fn remote_control_addr() -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let raw = std::env::var("PHAROS_CONTROL_ADDR").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(a) = raw.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        return Some(a);
    }
    format!("{raw}:{}", core_proto::ports::CONTROL)
        .to_socket_addrs()
        .ok()?
        .next()
}

/// Spawn the reconnecting I/O thread. It loops: connect → handshake → pump
/// requests + self-issue `GetStatus` ~1 Hz → on any error, report disconnect and
/// retry after a short backoff. Never panics.
#[cfg(any(unix, windows))]
fn spawn_worker(req_rx: Receiver<Request>, ev_tx: Sender<Event>) {
    use std::io::BufReader;
    use std::time::{Duration, Instant};

    std::thread::Builder::new()
        .name("pharos-control".into())
        .spawn(move || {
            // The reconnect loop. We share `req_rx`/`ev_tx` across reconnects.
            loop {
                let stream = match open_control_stream() {
                    Ok(s) => s,
                    Err(_) => {
                        set_disconnected();
                        drain_pending(&req_rx);
                        if !sleep_or_stop(&req_rx, Duration::from_secs(2)) {
                            return; // caller gone — both senders dropped.
                        }
                        continue;
                    }
                };

                // Non-blocking read so the single thread can also service the
                // request queue; we poll with a short timeout instead.
                if stream
                    .set_read_timeout(Some(Duration::from_millis(150)))
                    .is_err()
                {
                    set_disconnected();
                    if !sleep_or_stop(&req_rx, Duration::from_secs(2)) {
                        return;
                    }
                    continue;
                }

                let mut writer = match stream.try_clone() {
                    Ok(w) => w,
                    Err(_) => {
                        set_disconnected();
                        if !sleep_or_stop(&req_rx, Duration::from_secs(2)) {
                            return;
                        }
                        continue;
                    }
                };
                let mut reader = BufReader::new(stream);

                // Handshake: say Hello, then ask for an initial status.
                if crate::write_request(
                    &mut writer,
                    &Request::Hello {
                        version: crate::PROTOCOL_VERSION,
                    },
                )
                .is_err()
                {
                    set_disconnected();
                    if !sleep_or_stop(&req_rx, Duration::from_secs(2)) {
                        return;
                    }
                    continue;
                }
                let _ = crate::write_request(&mut writer, &Request::GetStatus);

                set_connected();
                let mut last_poll = Instant::now();
                let mut alive = true;

                // Inner pump loop for this connection.
                while alive {
                    // 1. Forward any queued requests.
                    loop {
                        match req_rx.try_recv() {
                            Ok(req) => {
                                if crate::write_request(&mut writer, &req).is_err() {
                                    alive = false;
                                    break;
                                }
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return, // caller gone
                        }
                    }
                    if !alive {
                        break;
                    }

                    // 2. Self-issue GetStatus ~1 Hz when idle.
                    if last_poll.elapsed() >= Duration::from_secs(1) {
                        last_poll = Instant::now();
                        if crate::write_request(&mut writer, &Request::GetStatus).is_err() {
                            break;
                        }
                    }

                    // 3. Read one event (or time out and loop).
                    match crate::read_event(&mut reader) {
                        Ok(Some(ev)) => {
                            if ev_tx.send(ev).is_err() {
                                return; // caller gone
                            }
                        }
                        Ok(None) => {
                            alive = false; // clean EOF
                        }
                        Err(e) => {
                            if !is_timeout(&e) {
                                alive = false;
                            }
                        }
                    }
                }

                set_disconnected();
                if !sleep_or_stop(&req_rx, Duration::from_secs(2)) {
                    return;
                }
            }
        })
        .ok();
}

/// True if a socket error is just our read-timeout (so we keep the connection).
#[cfg(any(unix, windows))]
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Sleep for `dur`, bailing out early (false) if the caller dropped the request
/// sender (the channel disconnected). Returns true if the nap completed.
#[cfg(any(unix, windows))]
fn sleep_or_stop(req_rx: &Receiver<Request>, dur: std::time::Duration) -> bool {
    match req_rx.recv_timeout(dur) {
        Ok(_) => true, // a request arrived while disconnected; drop it, keep going
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
    }
}

/// Discard every queued request without blocking (used while disconnected).
#[cfg(any(unix, windows))]
fn drain_pending(req_rx: &Receiver<Request>) {
    while req_rx.try_recv().is_ok() {}
}

/// Flip the cached link state to connected (host learned later via `Hello`).
#[cfg(any(unix, windows))]
fn set_connected() {
    if let Ok(mut l) = handle().link.lock() {
        l.connected = true;
    }
}

/// Flip the cached link state to disconnected and clear stale status so the
/// banner can't keep claiming "streaming" after the host went away.
#[cfg(any(unix, windows))]
fn set_disconnected() {
    if let Ok(mut l) = handle().link.lock() {
        l.connected = false;
        l.status = None;
        l.host_version = None;
        l.frame_timings = None;
        l.frame_history = FrameHistory::default();
    }
}

// ---------------------------------------------------------------------------
// Fallback for exotic targets (neither unix nor windows): a no-op handle.
// ---------------------------------------------------------------------------

#[cfg(not(any(unix, windows)))]
fn init() -> Handle {
    Handle {
        tx: None,
        rx: Mutex::new(None),
        link: Mutex::new(LinkState::default()),
    }
}
