// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! The Linux implementation of `gamepad-bridge` (evdev capture, uinput
//! replay, and the UDP wire loop); see `main.rs` for the crate docs.

use anyhow::{bail, Context, Result};
use evdev::{
    uinput::VirtualDeviceBuilder, AbsInfo, AbsoluteAxisType, AttributeSet, BusType, Device,
    EventType, InputEvent, InputId, Key, UinputAbsSetup,
};
use std::net::UdpSocket;
use std::time::Duration;

const PKT_CONFIG: u8 = 0x01;
const PKT_FRAME: u8 = 0x02;
const SYN_REPORT: u16 = 0; // EV_SYN / SYN_REPORT

pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("send") => run_send(
            args.get(2)
                .context("usage: send <host:port> [event-device]")?,
            args.get(3).map(String::as_str),
        ),
        Some("recv") => {
            let bind = args.get(2).context("usage: recv <bind:port> [--peer ip]")?;
            let peer = args
                .iter()
                .position(|a| a == "--peer")
                .and_then(|i| args.get(i + 1))
                .cloned();
            run_recv(bind, peer)
        }
        _ => {
            eprintln!(
                "usage:\n  gamepad-bridge send <host:port> [event-device]\n  \
                 gamepad-bridge recv <bind:port> [--peer ip]"
            );
            std::process::exit(2);
        }
    }
}

// ---- Pi side: read the pad, forward -----------------------------------------

fn run_send(dst: &str, dev_path: Option<&str>) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(dst)
        .with_context(|| format!("connect {dst}"))?;

    let mut device = open_gamepad(dev_path)?;
    let name = device.name().unwrap_or("gamepad").to_string();
    eprintln!("gamepad-bridge: forwarding '{name}' -> {dst}");
    if let Err(e) = device.grab() {
        eprintln!("gamepad-bridge: warning: could not grab device exclusively: {e}");
    }

    let config = build_config(&device, &name)?;
    sock.send(&config)?;

    // Resend config periodically so a recv that starts late / drops the first
    // packet can (re)build the virtual device even while the pad is idle.
    {
        let sock = sock.try_clone()?;
        let config = config.clone();
        std::thread::Builder::new()
            .name("gp-config".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(1000));
                let _ = sock.send(&config);
            })?;
    }

    let mut frame: Vec<u8> = Vec::with_capacity(256);
    let mut n: u16 = 0;
    loop {
        for ev in device.fetch_events()? {
            let t = ev.event_type().0;
            let c = ev.code();
            frame.extend_from_slice(&t.to_le_bytes());
            frame.extend_from_slice(&c.to_le_bytes());
            frame.extend_from_slice(&ev.value().to_le_bytes());
            n += 1;
            if t == EventType::SYNCHRONIZATION.0 && c == SYN_REPORT {
                let mut pkt = Vec::with_capacity(3 + frame.len());
                pkt.push(PKT_FRAME);
                pkt.extend_from_slice(&n.to_le_bytes());
                pkt.extend_from_slice(&frame);
                let _ = sock.send(&pkt);
                frame.clear();
                n = 0;
            }
        }
    }
}

/// Open the requested device, or auto-pick the first gamepad (has BTN_SOUTH and
/// an X axis, or a name containing "xbox"/"controller"/"gamepad"/"pad").
fn open_gamepad(path: Option<&str>) -> Result<Device> {
    if let Some(p) = path {
        return Device::open(p).with_context(|| format!("open {p}"));
    }
    let mut best: Option<Device> = None;
    for (_p, dev) in evdev::enumerate() {
        let looks_like_pad = dev
            .supported_keys()
            .is_some_and(|k| k.contains(Key::BTN_SOUTH))
            && dev
                .supported_absolute_axes()
                .is_some_and(|a| a.contains(AbsoluteAxisType::ABS_X));
        let named = dev
            .name()
            .map(|n| {
                let n = n.to_ascii_lowercase();
                n.contains("xbox")
                    || n.contains("controller")
                    || n.contains("gamepad")
                    || n.contains("pad")
            })
            .unwrap_or(false);
        if looks_like_pad || named {
            best = Some(dev);
            break;
        }
    }
    best.context("no gamepad evdev device found (is the controller paired+connected?)")
}

fn build_config(dev: &Device, name: &str) -> Result<Vec<u8>> {
    let mut p = vec![PKT_CONFIG];
    // Forward the controller's identity (bustype,vendor,product,version) so the
    // host's virtual pad mirrors it — vendor/product drive SDL/Proton's controller
    // mapping; without it the pad is a nameless joystick games' menus ignore.
    let id = dev.input_id();
    p.extend_from_slice(&id.bus_type().0.to_le_bytes());
    p.extend_from_slice(&id.vendor().to_le_bytes());
    p.extend_from_slice(&id.product().to_le_bytes());
    p.extend_from_slice(&id.version().to_le_bytes());
    let nb = name.as_bytes();
    let nlen = nb.len().min(255);
    p.push(nlen as u8);
    p.extend_from_slice(&nb[..nlen]);

    let keys: Vec<u16> = dev
        .supported_keys()
        .map(|s| s.iter().map(|k| k.code()).collect())
        .unwrap_or_default();
    p.extend_from_slice(&(keys.len() as u16).to_le_bytes());
    for k in &keys {
        p.extend_from_slice(&k.to_le_bytes());
    }

    // get_abs_state() returns a raw input_absinfo array indexed by axis code.
    let abs_state = dev.get_abs_state()?;
    let axes: Vec<AbsoluteAxisType> = dev
        .supported_absolute_axes()
        .map(|s| s.iter().collect())
        .unwrap_or_default();
    p.extend_from_slice(&(axes.len() as u16).to_le_bytes());
    for a in &axes {
        let info = &abs_state[a.0 as usize];
        p.extend_from_slice(&a.0.to_le_bytes());
        p.extend_from_slice(&info.value.to_le_bytes());
        p.extend_from_slice(&info.minimum.to_le_bytes());
        p.extend_from_slice(&info.maximum.to_le_bytes());
        p.extend_from_slice(&info.fuzz.to_le_bytes());
        p.extend_from_slice(&info.flat.to_le_bytes());
        p.extend_from_slice(&info.resolution.to_le_bytes());
    }
    Ok(p)
}

// ---- host side: uinput virtual device ---------------------------------------

fn run_recv(bind: &str, peer: Option<String>) -> Result<()> {
    let sock = UdpSocket::bind(bind).with_context(|| format!("bind {bind}"))?;
    eprintln!(
        "gamepad-bridge: listening on {bind}{}",
        peer.as_ref()
            .map(|p| format!(" (peer {p})"))
            .unwrap_or_default()
    );
    let mut vdev: Option<evdev::uinput::VirtualDevice> = None;
    let mut buf = [0u8; 4096];
    loop {
        let (len, src) = sock.recv_from(&mut buf)?;
        if let Some(ref p) = peer {
            if src.ip().to_string() != *p {
                continue;
            }
        }
        let pkt = &buf[..len];
        match pkt.first().copied() {
            Some(PKT_CONFIG) if vdev.is_none() => match build_uinput(&pkt[1..]) {
                Ok((d, name)) => {
                    eprintln!("gamepad-bridge: created virtual pad '{name}'");
                    vdev = Some(d);
                }
                Err(e) => eprintln!("gamepad-bridge: config build failed: {e:#}"),
            },
            Some(PKT_FRAME) => {
                if let Some(ref mut d) = vdev {
                    if let Ok(evs) = parse_frame(&pkt[1..]) {
                        let _ = d.emit(&evs);
                    }
                }
            }
            _ => {}
        }
    }
}

fn build_uinput(p: &[u8]) -> Result<(evdev::uinput::VirtualDevice, String)> {
    let mut r = Reader::new(p);
    let bustype = r.u16()?;
    let vendor = r.u16()?;
    let product = r.u16()?;
    let version = r.u16()?;
    let nlen = r.u8()? as usize;
    let name = String::from_utf8_lossy(r.take(nlen)?).into_owned();

    let n_keys = r.u16()? as usize;
    let mut keys = AttributeSet::<Key>::new();
    for _ in 0..n_keys {
        let code = r.u16()?;
        // Register ONLY gamepad/joystick buttons: BTN_JOYSTICK..BTN_THUMBR
        // (0x120..=0x13f) and BTN_DPAD_* (0x220..=0x22f). The Elite 2 via
        // hid-microsoft also reports keyboard keys; if we register those, udev's
        // input_id builtin classifies the virtual pad as ID_INPUT_KEYBOARD with no
        // ID_INPUT_JOYSTICK, so SDL/Steam/Proton (which enumerate joysticks) never
        // see it — the controller is invisible to the game.
        if (0x120..=0x13f).contains(&code) || (0x220..=0x22f).contains(&code) {
            keys.insert(Key::new(code));
        }
    }

    let n_abs = r.u16()? as usize;
    let mut abs_setups: Vec<UinputAbsSetup> = Vec::with_capacity(n_abs);
    for _ in 0..n_abs {
        let code = r.u16()?;
        let value = r.i32()?;
        let min = r.i32()?;
        let max = r.i32()?;
        let fuzz = r.i32()?;
        let flat = r.i32()?;
        let res = r.i32()?;
        let info = AbsInfo::new(value, min, max, fuzz, flat, res);
        abs_setups.push(UinputAbsSetup::new(AbsoluteAxisType(code), info));
    }

    let vname = format!("gamepad-bridge {name}");
    // Present as a *wired Xbox 360* (045e:028e, USB) regardless of the source id.
    // A uinput pad is evdev-only (no /dev/hidraw node); SDL/Proton's HIDAPI driver
    // *claims* many real controllers by VID:PID — notably Bluetooth Xbox pads
    // (045e:0b22) — and then can't read an evdev-only device, while SDL's evdev
    // driver skips that VID:PID believing HIDAPI owns it. The pad goes invisible to
    // the game (lsof: only kwin opens it). 045e:028e is NOT HIDAPI-claimed, so it's
    // read via the plain evdev/XInput path every game understands. We still forward
    // + log the real source id (set SDL_JOYSTICK_HIDAPI=0 on the game to use it).
    eprintln!(
        "gamepad-bridge: source pad {vendor:04x}:{product:04x} (bus {bustype}, ver {version:04x}) \
         — presenting as wired Xbox 360 045e:028e for evdev/XInput compatibility"
    );
    let mut builder = VirtualDeviceBuilder::new()?
        .name(&vname)
        .input_id(InputId::new(BusType(0x03), 0x045e, 0x028e, 0x0110))
        .with_keys(&keys)?;
    for a in &abs_setups {
        builder = builder.with_absolute_axis(a)?;
    }
    let dev = builder.build()?;
    Ok((dev, name))
}

fn parse_frame(p: &[u8]) -> Result<Vec<InputEvent>> {
    let mut r = Reader::new(p);
    let n = r.u16()? as usize;
    let mut evs = Vec::with_capacity(n);
    for _ in 0..n {
        let t = r.u16()?;
        let c = r.u16()?;
        let v = r.i32()?;
        evs.push(InputEvent::new(EventType(t), c, v));
    }
    Ok(evs)
}

// ---- tiny LE reader ----------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.i + n > self.b.len() {
            bail!("short packet");
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}
