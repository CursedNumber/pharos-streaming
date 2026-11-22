// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Keyboard + pointer injection into the REAL desktop session mirrored by
//! [`crate::portal_mirror`], via libei (the `reis` crate).
//!
//! `portal_mirror::MirrorSession::connect_input()` hands back a Unix socket
//! fd for the portal's EIS (Emulated Input Server) — normally KWin/Mutter's
//! own compositor, NOT this process's smithay seat. This module is the
//! libei *client* (`ei`) side of that connection: it negotiates
//! `ei_seat`/`ei_device`, binds the pointer/pointer_absolute/button/scroll/
//! keyboard capabilities, and translates [`core_proto::InputEvent`] into the
//! matching `ei_*` requests — giving full KVM control of the mirrored
//! desktop through the SAME permission the video came from.
//!
//! Runs on its own thread with its own tiny poll loop (mirrors the
//! `portal_mirror` PipeWire-thread pattern: the render loop / QUIC input
//! task must never block on this). Commands cross via a bounded `mpsc`
//! channel; [`MirrorInput`]'s methods are non-blocking best-effort sends —
//! dropping an occasional input sample under extreme backpressure is far
//! better than stalling the input path (see the render-loop no-block rule).
//!
//! Coordinate/keycode conventions match the existing smithay seat path in
//! `lib.rs::dispatch_input` EXCEPT: libei wants raw Linux evdev keycodes
//! (`ei_keyboard.key`), NOT the xkb `+8`-offset keycodes smithay's
//! `Keycode` wants — so [`MirrorInput::key`] forwards `scancode` unmodified.
//! Button codes reuse the same `0x110 + index` evdev mapping
//! (`lib.rs::dispatch_input`'s `PointerButton` arm).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::Duration;

use anyhow::{Context as _, Result};
use reis::{ei, PendingRequestResult};

/// One command sent to the libei client thread. Coordinates/deltas mirror
/// `core_proto::InputEvent`'s field shapes so the call sites at the input
/// dispatch choke-point are a near-direct forward.
enum Cmd {
    /// Raw Linux evdev keycode (no xkb offset — see module docs).
    Key {
        keycode: u32,
        pressed: bool,
    },
    /// Normalised 0..=1 over the mirrored region (same convention as
    /// `InputEvent::PointerAbs`); scaled to the bound device's `ei_region`.
    PointerAbs {
        x: f32,
        y: f32,
    },
    PointerRel {
        dx: f32,
        dy: f32,
    },
    /// Evdev button code (already resolved by the caller — see
    /// `evdev_button` in `lib.rs::dispatch_input`).
    Button {
        code: u32,
        pressed: bool,
    },
    Scroll {
        dx: f32,
        dy: f32,
    },
    Stop,
}

/// Handle to the running libei client thread. Cheap to clone the sends
/// (methods take `&self`); dropping the LAST handle stops the thread.
pub struct MirrorInput {
    tx: SyncSender<Cmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MirrorInput {
    /// Take ownership of the EIS fd (from
    /// [`crate::portal_mirror::MirrorSession::connect_input`]) and start the
    /// libei client thread. Non-blocking: returns as soon as the thread is
    /// spawned, not once the handshake completes (the handshake/seat/device
    /// discovery happens asynchronously; commands sent before it's ready are
    /// simply dropped like any other backpressure case).
    pub fn spawn(eis_fd: OwnedFd) -> Result<Self> {
        let stream = UnixStream::from(eis_fd);
        stream
            .set_nonblocking(true)
            .context("mirror-input: set eis socket nonblocking")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Cmd>(256);
        let join = std::thread::Builder::new()
            .name("pharos-mirror-ei".into())
            .spawn(move || run_ei_thread(stream, rx))
            .context("mirror-input: spawn libei thread")?;
        Ok(Self {
            tx,
            join: Some(join),
        })
    }

    /// Best-effort, non-blocking. A full queue or a not-yet-ready connection
    /// silently drops the sample — the same policy the video side uses for
    /// backpressure, and far preferable to blocking the input path.
    pub fn key(&self, keycode: u32, pressed: bool) {
        self.send(Cmd::Key { keycode, pressed });
    }
    pub fn pointer_abs(&self, x: f32, y: f32) {
        self.send(Cmd::PointerAbs { x, y });
    }
    pub fn pointer_rel(&self, dx: f32, dy: f32) {
        self.send(Cmd::PointerRel { dx, dy });
    }
    pub fn button(&self, code: u32, pressed: bool) {
        self.send(Cmd::Button { code, pressed });
    }
    pub fn scroll(&self, dx: f32, dy: f32) {
        self.send(Cmd::Scroll { dx, dy });
    }

    fn send(&self, cmd: Cmd) {
        match self.tx.try_send(cmd) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                tracing::debug!("mirror-input: command queue full; dropping one input sample");
            }
        }
    }
}

impl Drop for MirrorInput {
    fn drop(&mut self) {
        // Best-effort: if the thread already died (portal session ended)
        // this just fails silently, which is fine — nothing left to stop.
        let _ = self.tx.try_send(Cmd::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------------
// libei client thread
// ---------------------------------------------------------------------------

/// A region an `ei_pointer_absolute` device accepts events within (logical
/// pixels; NOT necessarily the true desktop pixel size — see the `ei_region`
/// event docs). Used to scale normalised `PointerAbs` coordinates.
#[derive(Clone, Copy, Default)]
struct Region {
    offset_x: f64,
    offset_y: f64,
    width: f64,
    height: f64,
}

#[derive(Default)]
struct DeviceData {
    interfaces: HashMap<String, reis::Object>,
    region: Option<Region>,
    /// Whether `start_emulating` has been sent for this device (libei groups
    /// a burst of requests between start/stop; we treat the whole mirror
    /// session as one long burst per device rather than one per event).
    emulating: bool,
}

impl DeviceData {
    fn interface<T: reis::Interface>(&self) -> Option<T> {
        self.interfaces.get(T::NAME)?.clone().downcast()
    }
}

const INTERFACES: &[(&str, u32)] = &[
    ("ei_callback", 1),
    ("ei_connection", 1),
    ("ei_seat", 1),
    ("ei_device", 1),
    ("ei_pingpong", 1),
    ("ei_pointer", 1),
    ("ei_pointer_absolute", 1),
    ("ei_button", 1),
    ("ei_scroll", 1),
    ("ei_keyboard", 1),
];

/// Everything the client-thread loop needs to carry between iterations.
struct EiState {
    seats: HashMap<ei::Seat, HashMap<String, u64>>,
    devices: HashMap<ei::Device, DeviceData>,
    sequence: u32,
    last_serial: u32,
}

impl EiState {
    /// The first bound device advertising interface `T` — good enough for
    /// the common single-pointer/single-keyboard case; a multi-device EIS
    /// implementation would need per-device routing this doesn't attempt.
    fn device_with<T: reis::Interface>(&self) -> Option<(&ei::Device, &DeviceData, T)> {
        self.devices.iter().find_map(|(dev, data)| {
            let iface = data.interface::<T>()?;
            Some((dev, data, iface))
        })
    }

    /// Mark `dev` as emulating (once) so subsequent requests are valid.
    fn ensure_emulating(&mut self, dev: &ei::Device) {
        if let Some(data) = self.devices.get_mut(dev) {
            if !data.emulating {
                self.sequence += 1;
                dev.start_emulating(self.sequence, self.last_serial);
                data.emulating = true;
            }
        }
    }
}

fn emit(context: &ei::Context, state: &mut EiState, cmd: Cmd) {
    match cmd {
        Cmd::Key { keycode, pressed } => {
            if let Some((dev, _, kb)) = state.device_with::<ei::Keyboard>() {
                let dev = dev.clone();
                state.ensure_emulating(&dev);
                let ks = if pressed {
                    ei::keyboard::KeyState::Press
                } else {
                    ei::keyboard::KeyState::Released
                };
                kb.key(keycode, ks);
                dev.frame(state.last_serial, 0);
            }
        }
        Cmd::PointerAbs { x, y } => {
            if let Some((dev, data, abs)) = state.device_with::<ei::PointerAbsolute>() {
                let dev = dev.clone();
                let region = data.region.unwrap_or(Region {
                    offset_x: 0.0,
                    offset_y: 0.0,
                    width: 1.0,
                    height: 1.0,
                });
                state.ensure_emulating(&dev);
                let px = region.offset_x + f64::from(x) * region.width;
                let py = region.offset_y + f64::from(y) * region.height;
                abs.motion_absolute(px as f32, py as f32);
                dev.frame(state.last_serial, 0);
            }
        }
        Cmd::PointerRel { dx, dy } => {
            if let Some((dev, _, ptr)) = state.device_with::<ei::Pointer>() {
                let dev = dev.clone();
                state.ensure_emulating(&dev);
                ptr.motion_relative(dx, dy);
                dev.frame(state.last_serial, 0);
            }
        }
        Cmd::Button { code, pressed } => {
            if let Some((dev, _, btn)) = state.device_with::<ei::Button>() {
                let dev = dev.clone();
                state.ensure_emulating(&dev);
                let bs = if pressed {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                };
                btn.button(code, bs);
                dev.frame(state.last_serial, 0);
            }
        }
        Cmd::Scroll { dx, dy } => {
            if let Some((dev, _, sc)) = state.device_with::<ei::Scroll>() {
                let dev = dev.clone();
                state.ensure_emulating(&dev);
                sc.scroll(dx, dy);
                dev.frame(state.last_serial, 0);
            }
        }
        Cmd::Stop => unreachable!("Stop is handled by the caller before reaching emit()"),
    }
    let _ = context.flush();
}

fn handle_ei_event(request: ei::Event, state: &mut EiState) {
    match request {
        ei::Event::Handshake(handshake, req) => match req {
            ei::handshake::Event::HandshakeVersion { .. } => {
                handshake.handshake_version(1);
                handshake.name("pharos-mirror-input");
                handshake.context_type(ei::handshake::ContextType::Sender);
                for (interface, version) in INTERFACES {
                    handshake.interface_version(interface, *version);
                }
                handshake.finish();
            }
            ei::handshake::Event::Connection { serial, .. } => {
                state.last_serial = serial;
            }
            _ => {}
        },
        ei::Event::Connection(_conn, req) => match req {
            ei::connection::Event::Seat { seat } => {
                state.seats.insert(seat, HashMap::new());
            }
            ei::connection::Event::Ping { ping } => {
                ping.done(0);
            }
            _ => {}
        },
        ei::Event::Seat(seat, req) => {
            let Some(caps) = state.seats.get_mut(&seat) else {
                return;
            };
            match req {
                ei::seat::Event::Capability { mask, interface } => {
                    caps.insert(interface, mask);
                }
                ei::seat::Event::Done => {
                    // Bind every capability we asked interfaces for; a portal
                    // that only granted Keyboard or only Pointer simply won't
                    // have the other masks present, which is fine — `emit`
                    // no-ops for a capability with no bound device.
                    let mask = caps.values().fold(0u64, |acc, m| acc | m);
                    if mask != 0 {
                        seat.bind(mask);
                    }
                }
                ei::seat::Event::Device { device } => {
                    state.devices.insert(device, DeviceData::default());
                }
                _ => {}
            }
        }
        ei::Event::Device(device, req) => {
            let Some(data) = state.devices.get_mut(&device) else {
                return;
            };
            match req {
                ei::device::Event::Interface { object } => {
                    data.interfaces
                        .insert(object.interface().to_owned(), object);
                }
                ei::device::Event::Region {
                    offset_x,
                    offset_y,
                    width,
                    hight,
                    ..
                } => {
                    // First region wins — see `device_with`'s single-device
                    // routing caveat; a dual-head mirror needing per-monitor
                    // absolute devices is a follow-up.
                    if data.region.is_none() {
                        data.region = Some(Region {
                            offset_x: f64::from(offset_x),
                            offset_y: f64::from(offset_y),
                            width: f64::from(width),
                            height: f64::from(hight),
                        });
                    }
                }
                ei::device::Event::Resumed { serial } => {
                    state.last_serial = serial;
                }
                ei::device::Event::Paused { serial } | ei::device::Event::Destroyed { serial } => {
                    state.last_serial = serial;
                    data.emulating = false;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Poll `fd` for readability with a short timeout so the loop below can
/// interleave draining the command channel without busy-spinning. Returns
/// `false` on timeout/interrupt (caller just loops back to drain commands).
fn poll_readable(fd: std::os::fd::RawFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is a valid, exclusively-owned pollfd for the duration of
    // this call; `nfds=1` matches the single-element array.
    let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    n > 0 && (pfd.revents & libc::POLLIN) != 0
}

fn run_ei_thread(stream: UnixStream, rx: Receiver<Cmd>) {
    use std::os::fd::AsRawFd;

    let context = match ei::Context::new(stream) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("mirror-input: ei::Context::new failed: {e}");
            return;
        }
    };
    let _handshake = context.handshake();
    let _ = context.flush();
    let raw_fd = context.as_raw_fd();

    let mut state = EiState {
        seats: HashMap::new(),
        devices: HashMap::new(),
        sequence: 0,
        last_serial: u32::MAX,
    };

    loop {
        // Drain queued input first — latency-sensitive, never wait on it.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    for dev in state.devices.keys() {
                        dev.stop_emulating(state.last_serial);
                    }
                    let _ = context.flush();
                    return;
                }
                Ok(cmd) => emit(&context, &mut state, cmd),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        if !poll_readable(raw_fd, Duration::from_millis(8)) {
            continue;
        }
        if context.read().is_err() {
            tracing::debug!("mirror-input: eis connection closed");
            return;
        }
        while let Some(result) = context.pending_event() {
            match result {
                PendingRequestResult::Request(request) => handle_ei_event(request, &mut state),
                PendingRequestResult::ParseError(msg) => {
                    tracing::warn!("mirror-input: ei parse error: {msg}");
                }
                PendingRequestResult::InvalidObject(_) => {}
            }
        }
        let _ = context.flush();
    }
}
