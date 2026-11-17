// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! DSU ("cemuhook") motion server: hands the client's gyro to emulators.
//!
//! The virtual pad games see is an Xbox 360 pad, which has no motion sensors,
//! so a client controller's (or phone's) gyro had nowhere to go but the
//! gyro→mouse path. Emulators (Eden/Lichii, Cemu, Dolphin, …) read motion over
//! the DSU UDP protocol instead, defaulting to `127.0.0.1:26760`; this serves
//! that, fed from the same `GamepadMotion` samples, one DSU slot per pad slot.
//!
//! Toggled by `config.toml [dsu] enabled` (default off). The server thread
//! re-reads the config every couple of seconds, so flipping it in the panel
//! binds/unbinds the port live without restarting the host.
//!
//! Render-loop rule: [`DsuServer::motion`] is a non-blocking `try_send` into a
//! bounded channel. All socket work happens on the dedicated `dsu` thread.
//!
//! Units in: gyro rad/s, accel m/s² (Android `SensorEvent` units). Units out
//! (protocol): gyro deg/s, accel g. Axis mapping is `[dsu] gyro_axes` /
//! `accel_axes` (see [`AxisMap`]); the default assumes the client sends the
//! Trireme canonical frame (a phone held landscape, screen facing the player).

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use host_core::config::DsuConfig;

const SLOTS: usize = 4;
const PROTOCOL_VERSION: u16 = 1001;
const MSG_VERSION: u32 = 0x10_0000;
const MSG_PORTS: u32 = 0x10_0001;
const MSG_PAD_DATA: u32 = 0x10_0002;
/// Clients re-request pad data about once a second; drop silent ones after this.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// A slot with no motion for this long (and no connected pad) reads as empty.
const MOTION_STALE: Duration = Duration::from_secs(2);
const CONFIG_POLL: Duration = Duration::from_secs(2);
const STANDARD_GRAVITY: f32 = 9.806_65;

enum Event {
    Motion {
        slot: u8,
        gyro: [f32; 3],
        accel: [f32; 3],
    },
    Pad {
        slot: u8,
        connected: bool,
    },
}

/// Handle owned by the render loop; cheap to call per sample.
pub struct DsuServer {
    tx: SyncSender<Event>,
}

impl DsuServer {
    /// Spawn the server thread. It stays idle (no socket) while disabled.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel(256);
        if let Err(e) = thread::Builder::new()
            .name("dsu".into())
            .spawn(move || Server::new().run(rx))
        {
            tracing::warn!("dsu: couldn't spawn server thread: {e}");
        }
        Self { tx }
    }

    /// Forward one motion sample (gyro rad/s, accel m/s²). Drops on a full queue.
    pub fn motion(&self, slot: u8, gyro: [f32; 3], accel: [f32; 3]) {
        let _ = self.tx.try_send(Event::Motion { slot, gyro, accel });
    }

    /// A client pad appeared or left on `slot`.
    pub fn pad(&self, slot: u8, connected: bool) {
        let _ = self.tx.try_send(Event::Pad { slot, connected });
    }
}

/// `[dsu] gyro_axes` / `accel_axes`: three comma-separated `[-]x|y|z` tokens,
/// naming which input axis feeds DSU x, y and z (and its sign). DSU's frame is
/// a DualShock 4 held normally: x right, y up out of the face, z toward the
/// player. The default `-y,x,z` maps Android's frame for a phone in landscape
/// (top edge to the left) held upright with the screen facing the player.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisMap([(usize, f32); 3]);

impl AxisMap {
    pub const DEFAULT: &'static str = "-y,x,z";

    pub fn parse(s: &str) -> Option<Self> {
        let mut out = [(0, 1.0); 3];
        let mut parts = s.split(',');
        for slot in &mut out {
            let t = parts.next()?.trim();
            let (sign, axis) = match t.strip_prefix('-') {
                Some(rest) => (-1.0, rest),
                None => (1.0, t.strip_prefix('+').unwrap_or(t)),
            };
            let idx = match axis {
                "x" | "X" => 0,
                "y" | "Y" => 1,
                "z" | "Z" => 2,
                _ => return None,
            };
            *slot = (idx, sign);
        }
        parts.next().is_none().then_some(Self(out))
    }

    fn from_config(s: Option<&str>, what: &str) -> Self {
        let s = s.unwrap_or(Self::DEFAULT);
        Self::parse(s).unwrap_or_else(|| {
            tracing::warn!("dsu: bad {what} {s:?}; using {}", Self::DEFAULT);
            Self::parse(Self::DEFAULT).expect("default axis map parses")
        })
    }

    fn apply(&self, v: [f32; 3]) -> [f32; 3] {
        self.0.map(|(i, sign)| v[i] * sign)
    }
}

#[derive(Default)]
struct Slot {
    pad_connected: bool,
    last_motion: Option<Instant>,
}

impl Slot {
    fn live(&self, now: Instant) -> bool {
        self.pad_connected
            || self
                .last_motion
                .is_some_and(|t| now.duration_since(t) < MOTION_STALE)
    }
}

/// What one DSU client asked to receive, with the time of each request.
#[derive(Default)]
struct Subscription {
    all: Option<Instant>,
    slots: [Option<Instant>; SLOTS],
}

impl Subscription {
    fn wants(&self, slot: usize, now: Instant) -> bool {
        let fresh = |t: Option<Instant>| t.is_some_and(|t| now.duration_since(t) < CLIENT_TIMEOUT);
        fresh(self.all) || fresh(self.slots[slot])
    }

    fn expired(&self, now: Instant) -> bool {
        !(0..SLOTS).any(|s| self.wants(s, now))
    }
}

struct Server {
    sock: Option<UdpSocket>,
    bound: Option<SocketAddr>,
    gyro_axes: AxisMap,
    accel_axes: AxisMap,
    slots: [Slot; SLOTS],
    clients: HashMap<SocketAddr, Subscription>,
    packet_no: [u32; SLOTS],
    server_id: u32,
    epoch: Instant,
    next_config: Instant,
}

impl Server {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            sock: None,
            bound: None,
            gyro_axes: AxisMap::from_config(None, "gyro_axes"),
            accel_axes: AxisMap::from_config(None, "accel_axes"),
            slots: Default::default(),
            clients: HashMap::new(),
            packet_no: [0; SLOTS],
            server_id: std::process::id() ^ 0x5048_4153, // "PHAS"
            epoch: now,
            next_config: now,
        }
    }

    fn run(mut self, rx: Receiver<Event>) {
        loop {
            let now = Instant::now();
            if now >= self.next_config {
                self.next_config = now + CONFIG_POLL;
                let cfg = host_core::config::load();
                // This poll also carries the global gyro→mouse switch, so it
                // flips live without a restart (one atomic store).
                crate::host_input::GYRO_MOUSE_ENABLED
                    .store(cfg.gyro.mouse_enabled, std::sync::atomic::Ordering::Relaxed);
                self.apply_config(&cfg.dsu);
            }
            self.poll_requests();
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => {
                    self.handle(ev);
                    // Drain whatever else queued up behind it.
                    while let Ok(ev) = rx.try_recv() {
                        self.handle(ev);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn apply_config(&mut self, cfg: &DsuConfig) {
        self.gyro_axes = AxisMap::from_config(cfg.gyro_axes.as_deref(), "gyro_axes");
        self.accel_axes = AxisMap::from_config(cfg.accel_axes.as_deref(), "accel_axes");
        let want = cfg
            .enabled
            .then(|| format!("{}:{}", cfg.bind, cfg.port).parse::<SocketAddr>());
        match want {
            None => {
                if self.sock.take().is_some() {
                    tracing::info!("dsu: disabled, closed {:?}", self.bound.take());
                    self.clients.clear();
                }
            }
            Some(Err(e)) => tracing::warn!("dsu: bad bind address {}:{}: {e}", cfg.bind, cfg.port),
            Some(Ok(addr)) if self.bound == Some(addr) && self.sock.is_some() => {}
            Some(Ok(addr)) => {
                self.sock = None;
                self.clients.clear();
                match UdpSocket::bind(addr).and_then(|s| s.set_nonblocking(true).map(|()| s)) {
                    Ok(s) => {
                        tracing::info!("dsu: motion server listening on udp {addr}");
                        self.sock = Some(s);
                        self.bound = Some(addr);
                    }
                    Err(e) => {
                        // Retried on the next config poll (e.g. another DSU server
                        // holds the port and later exits).
                        tracing::warn!("dsu: can't bind udp {addr}: {e}");
                        self.bound = None;
                    }
                }
            }
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Pad { slot, connected } => {
                if let Some(s) = self.slots.get_mut(usize::from(slot)) {
                    s.pad_connected = connected;
                }
            }
            Event::Motion { slot, gyro, accel } => {
                let idx = usize::from(slot);
                if idx >= SLOTS {
                    return;
                }
                let now = Instant::now();
                self.slots[idx].last_motion = Some(now);
                if self.sock.is_none() {
                    return;
                }
                let gyro = self.gyro_axes.apply(gyro.map(f32::to_degrees));
                let accel = self.accel_axes.apply(accel.map(|a| a / STANDARD_GRAVITY));
                self.send_pad_data(idx, now, gyro, accel);
            }
        }
    }

    fn send_pad_data(&mut self, slot: usize, now: Instant, gyro: [f32; 3], accel: [f32; 3]) {
        self.clients.retain(|_, sub| !sub.expired(now));
        let targets: Vec<SocketAddr> = self
            .clients
            .iter()
            .filter(|(_, sub)| sub.wants(slot, now))
            .map(|(a, _)| *a)
            .collect();
        if targets.is_empty() {
            return;
        }
        self.packet_no[slot] = self.packet_no[slot].wrapping_add(1);
        let micros = now.duration_since(self.epoch).as_micros() as u64;
        let pkt = pad_data_packet(
            self.server_id,
            slot as u8,
            self.packet_no[slot],
            micros,
            gyro,
            accel,
        );
        if let Some(sock) = &self.sock {
            for t in targets {
                let _ = sock.send_to(&pkt, t);
            }
        }
    }

    fn poll_requests(&mut self) {
        let mut buf = [0u8; 128];
        loop {
            let Some(sock) = &self.sock else { return };
            let (n, from) = match sock.recv_from(&mut buf) {
                Ok(r) => r,
                Err(_) => return, // WouldBlock (or a transient error): nothing more now
            };
            let Some((msg, body)) = parse_request(&buf[..n]) else {
                continue;
            };
            let now = Instant::now();
            match msg {
                MSG_VERSION => {
                    let mut p = header(self.server_id, MSG_VERSION);
                    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
                    let _ = sock.send_to(&finish(p), from);
                }
                MSG_PORTS => {
                    let count = body
                        .get(..4)
                        .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
                        .unwrap_or(0)
                        .clamp(0, SLOTS as i32) as usize;
                    for &s in body.iter().skip(4).take(count) {
                        let s = usize::from(s);
                        if s >= SLOTS {
                            continue;
                        }
                        let mut p = header(self.server_id, MSG_PORTS);
                        slot_info(&mut p, s as u8, self.slots[s].live(now));
                        p.push(0);
                        let _ = sock.send_to(&finish(p), from);
                    }
                }
                MSG_PAD_DATA => {
                    let flags = body.first().copied().unwrap_or(0);
                    let slot = body.get(1).copied().map(usize::from);
                    let mac = body.get(2..8);
                    let sub = self.clients.entry(from).or_default();
                    if flags == 0 {
                        sub.all = Some(now);
                    }
                    if flags & 1 != 0 {
                        if let Some(s) = slot.filter(|s| *s < SLOTS) {
                            sub.slots[s] = Some(now);
                        }
                    }
                    if flags & 2 != 0 {
                        if let Some(s) = mac.and_then(slot_from_mac) {
                            sub.slots[s] = Some(now);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// Validate a client packet and return `(message type, body after the type)`.
fn parse_request(p: &[u8]) -> Option<(u32, &[u8])> {
    if p.len() < 20 || &p[..4] != b"DSUC" {
        return None;
    }
    let len = usize::from(u16::from_le_bytes([p[6], p[7]]));
    let p = p.get(..16 + len)?;
    let want = u32::from_le_bytes(p[8..12].try_into().ok()?);
    let mut zeroed = p.to_vec();
    zeroed[8..12].fill(0);
    if crc32fast::hash(&zeroed) != want {
        return None;
    }
    let msg = u32::from_le_bytes(p[16..20].try_into().ok()?);
    Some((msg, &p[20..]))
}

fn header(server_id: u32, msg: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(100);
    p.extend_from_slice(b"DSUS");
    p.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    p.extend_from_slice(&[0, 0]); // length, filled by `finish`
    p.extend_from_slice(&[0; 4]); // crc32, filled by `finish`
    p.extend_from_slice(&server_id.to_le_bytes());
    p.extend_from_slice(&msg.to_le_bytes());
    p
}

fn finish(mut p: Vec<u8>) -> Vec<u8> {
    let len = (p.len() - 16) as u16;
    p[6..8].copy_from_slice(&len.to_le_bytes());
    let crc = crc32fast::hash(&p);
    p[8..12].copy_from_slice(&crc.to_le_bytes());
    p
}

/// The 11-byte slot description shared by port-info and pad-data replies.
fn slot_info(p: &mut Vec<u8>, slot: u8, live: bool) {
    p.push(slot);
    p.push(if live { 2 } else { 0 }); // state: connected / not connected
    p.push(if live { 2 } else { 0 }); // model: full gyro
    p.push(if live { 2 } else { 0 }); // connection: bluetooth
    p.extend_from_slice(&slot_mac(slot));
    p.push(if live { 0x05 } else { 0 }); // battery: full
}

/// A stable fake MAC per slot, so MAC-based subscriptions resolve.
fn slot_mac(slot: u8) -> [u8; 6] {
    [0x02, 0x50, 0x48, 0x41, 0x52, slot] // locally administered, "PHAR"
}

fn slot_from_mac(mac: &[u8]) -> Option<usize> {
    (0..SLOTS).find(|&s| slot_mac(s as u8)[..] == *mac)
}

fn pad_data_packet(
    server_id: u32,
    slot: u8,
    packet_no: u32,
    micros: u64,
    gyro: [f32; 3],
    accel: [f32; 3],
) -> Vec<u8> {
    let mut p = header(server_id, MSG_PAD_DATA);
    slot_info(&mut p, slot, true);
    p.push(1); // connected
    p.extend_from_slice(&packet_no.to_le_bytes());
    // Buttons, home, touch button: none (the uinput pad carries buttons).
    p.extend_from_slice(&[0, 0, 0, 0]);
    p.extend_from_slice(&[128; 4]); // sticks centred
    p.extend_from_slice(&[0; 12]); // analog d-pad / face / shoulders
    p.extend_from_slice(&[0; 12]); // two touch points, inactive
    p.extend_from_slice(&micros.to_le_bytes());
    for v in accel.iter().chain(gyro.iter()) {
        p.extend_from_slice(&v.to_le_bytes());
    }
    finish(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_data_is_100_bytes_with_valid_crc() {
        let p = pad_data_packet(7, 1, 42, 123, [1.0, 2.0, 3.0], [0.0, 1.0, 0.0]);
        assert_eq!(p.len(), 100);
        assert_eq!(&p[..4], b"DSUS");
        assert_eq!(u16::from_le_bytes([p[6], p[7]]), 84);
        let mut z = p.clone();
        z[8..12].fill(0);
        assert_eq!(
            crc32fast::hash(&z),
            u32::from_le_bytes(p[8..12].try_into().unwrap())
        );
        // Gyro yaw (DSU y) is the second gyro float, at the very end - 8.
        assert_eq!(f32::from_le_bytes(p[92..96].try_into().unwrap()), 2.0);
    }

    #[test]
    fn request_roundtrip() {
        // Build a client request the same way a server reply is framed.
        let mut req = header(9, MSG_PAD_DATA);
        req[..4].copy_from_slice(b"DSUC");
        req.extend_from_slice(&[1, 2, 0, 0, 0, 0, 0, 0]);
        let req = finish(req);
        let (msg, body) = parse_request(&req).expect("valid request");
        assert_eq!(msg, MSG_PAD_DATA);
        assert_eq!(&body[..2], &[1, 2]);
        let mut bad = req.clone();
        bad[20] ^= 1;
        assert!(
            parse_request(&bad).is_none(),
            "crc must reject a flipped byte"
        );
    }

    #[test]
    fn client_subscribes_and_receives_motion() {
        let mut srv = Server::new();
        srv.apply_config(&DsuConfig {
            enabled: true,
            port: 0, // ephemeral
            ..DsuConfig::default()
        });
        let addr = srv.sock.as_ref().unwrap().local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut req = header(1, MSG_PAD_DATA);
        req[..4].copy_from_slice(b"DSUC");
        req.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // subscribe to all slots
        client.send_to(&finish(req), addr).unwrap();
        // Give the datagram a moment to land, then let the server read it.
        let deadline = Instant::now() + Duration::from_secs(2);
        while srv.clients.is_empty() && Instant::now() < deadline {
            srv.poll_requests();
        }

        // 90 deg/s of yaw on an upright landscape phone = rotation about its x.
        let rad = 90f32.to_radians();
        srv.handle(Event::Motion {
            slot: 0,
            gyro: [rad, 0.0, 0.0],
            accel: [STANDARD_GRAVITY, 0.0, 0.0],
        });
        let mut buf = [0u8; 128];
        let (n, _) = client.recv_from(&mut buf).expect("pad data arrives");
        assert_eq!(n, 100);
        let f = |o: usize| f32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        assert!((f(80) - 1.0).abs() < 1e-4, "accel y = 1 g, got {}", f(80));
        assert!(
            (f(92) - 90.0).abs() < 1e-3,
            "gyro yaw = 90 deg/s, got {}",
            f(92)
        );
    }

    #[test]
    fn axis_map() {
        let m = AxisMap::parse("-y,x,z").unwrap();
        assert_eq!(m.apply([1.0, 2.0, 3.0]), [-2.0, 1.0, 3.0]);
        assert!(AxisMap::parse("x,y").is_none());
        assert!(AxisMap::parse("x,y,w").is_none());
        assert!(AxisMap::parse("x,y,z,x").is_none());
        assert_eq!(slot_from_mac(&slot_mac(3)), Some(3));
    }
}
