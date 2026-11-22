// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! `trireme-usbexport` — the Trireme USB export side.
//!
//! Claims selected local USB devices via libusb and services USB/IP URBs against
//! them over the standalone QUIC relay. Devices are chosen by the user from a
//! persisted settings file (Pharos address + which devices to share), identified
//! by a friendly name even though the wire uses vid:pid / bus-addr underneath.
//!
//!   trireme-usbexport list [config]        — show devices + which are shared
//!   trireme-usbexport run [config]          — share everything in the config
//!   trireme-usbexport <pharos:port> <vid:pid>  — share one device directly
//!
//! Each shared device gets its own relay connection (the importer loops over
//! them), so sharing N devices is N independent sessions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use core_usbip::proto::*;
use rusb::{DeviceHandle, GlobalContext, TransferType};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Persisted on the Trireme device. Toggling a device = adding/removing its
/// `vid:pid` from `share`. (On Android this same list drives the manifest
/// device-filter so attaching needs no permission pop-up.)
#[derive(Debug, Deserialize)]
struct Config {
    pharos: String,
    #[serde(default)]
    share: Vec<String>,
}

const DEFAULT_CONFIG: &str = "trireme-usb.toml";

fn load_config(path: &str) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
    toml::from_str(&text).with_context(|| format!("parse {path}"))
}

/// libusb error → USB/IP status (negative errno, as the kernel expects).
fn usbip_status(e: rusb::Error) -> i32 {
    use rusb::Error::*;
    match e {
        Timeout => -110,
        Pipe => -32,
        NoDevice => -19,
        Access => -13,
        Busy => -16,
        Overflow => -75,
        NotFound => -2,
        Io => -5,
        _ => -71,
    }
}

fn speed_code(s: rusb::Speed) -> u32 {
    match s {
        rusb::Speed::Low => 1,
        rusb::Speed::Full => 2,
        rusb::Speed::High => 3,
        rusb::Speed::Super => 5,
        rusb::Speed::SuperPlus => 6,
        _ => 0,
    }
}

/// A human-readable name: the device's own Manufacturer/Product strings if it
/// reports them, else a vid:pid fallback. Opening read-only for strings doesn't
/// claim the device.
fn friendly_name(dev: &rusb::Device<GlobalContext>, dd: &rusb::DeviceDescriptor) -> String {
    let fallback = || format!("{:04x}:{:04x}", dd.vendor_id(), dd.product_id());
    let Ok(h) = dev.open() else { return fallback() };
    let t = Duration::from_millis(200);
    let Some(lang) = h.read_languages(t).ok().and_then(|l| l.into_iter().next()) else {
        return fallback();
    };
    let mfr = h.read_manufacturer_string(lang, dd, t).ok();
    let prod = h.read_product_string(lang, dd, t).ok();
    match (mfr, prod) {
        (Some(m), Some(p)) => format!("{} ({})", p.trim(), m.trim()),
        (_, Some(p)) => p.trim().to_string(),
        (Some(m), _) => m.trim().to_string(),
        _ => fallback(),
    }
}

fn class_name(code: u8) -> &'static str {
    match code {
        0x01 => "audio",
        0x03 => "HID",
        0x08 => "mass-storage",
        0x09 => "hub",
        0x0e => "video",
        0xe0 => "wireless",
        0xff => "vendor",
        _ => "other",
    }
}

/// How a device should be carried to Pharos — decided from its USB descriptor,
/// so we route correctly up front instead of failing and learning at runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Relayability {
    /// Plain USB/IP works (HID, mass-storage, hubs, vendor, …).
    Relay,
    /// A Bluetooth/wireless radio (interface class 0xE0). Don't relay it — the
    /// radio's HCI vendor commands + SMP bonding are too latency-sensitive for
    /// the relay (it times them out and resets the adapter). Pair the peripheral
    /// on the client and forward its decoded input (e.g. `gamepad-bridge`).
    PairAndForward,
    /// Has isochronous endpoints (USB audio / UVC webcam) — guaranteed-bandwidth
    /// streaming the relay doesn't service (iso is deliberately deferred).
    IsoUnsupported,
}

impl Relayability {
    fn label(self) -> &'static str {
        match self {
            Relayability::Relay => "relay",
            Relayability::PairAndForward => "pair-and-forward",
            Relayability::IsoUnsupported => "unsupported (iso)",
        }
    }
    fn note(self) -> &'static str {
        match self {
            Relayability::Relay => "plain USB/IP works",
            Relayability::PairAndForward => {
                "Bluetooth/wireless radio — pair it on the client and forward decoded \
                 input (e.g. gamepad-bridge); USB/IP can't carry BT bonding"
            }
            Relayability::IsoUnsupported => {
                "isochronous endpoints (USB audio / UVC webcam) — not serviced by the relay"
            }
        }
    }
}

/// Pure routing decision (unit-tested). Bluetooth wins over iso because a BT
/// radio also exposes a SCO (iso) interface, but we never relay it either way.
fn classify(has_bt: bool, has_iso: bool) -> Relayability {
    if has_bt {
        Relayability::PairAndForward
    } else if has_iso {
        Relayability::IsoUnsupported
    } else {
        Relayability::Relay
    }
}

/// Inspect a device's active config: Bluetooth/wireless class, or any
/// isochronous endpoint?
fn relayability(cfg: &rusb::ConfigDescriptor) -> Relayability {
    let mut has_bt = false;
    let mut has_iso = false;
    for iface in cfg.interfaces() {
        for desc in iface.descriptors() {
            if desc.class_code() == 0xe0 {
                has_bt = true;
            }
            for ep in desc.endpoint_descriptors() {
                if ep.transfer_type() == TransferType::Isochronous {
                    has_iso = true;
                }
            }
        }
    }
    classify(has_bt, has_iso)
}

/// Per-device libusb context shared across URB worker tasks.
struct Device {
    handle: DeviceHandle<GlobalContext>,
    ep_types: HashMap<u8, TransferType>,
    /// Claimed interface numbers — released + handed back to the kernel on exit.
    ifaces: Vec<u8>,
    /// Interrupt-IN endpoints (ep number, max packet size) — read continuously
    /// ahead of the host's requests so input is never waiting on the round-trip.
    int_in: Vec<(u8, usize)>,
}

/// Which reply lane a RET_SUBMIT travels on. Input (interrupt) and control
/// replies take the fast lane so a fat bulk transfer on a composite device
/// (e.g. a gamepad that also exposes an audio interface) can't head-of-line
/// block a tiny controller report on the shared QUIC stream — "QoS-lite".
enum Lane {
    Fast,
    Bulk,
}

fn lane_for(dev: &Device, ep: u8) -> Lane {
    if ep == 0 {
        return Lane::Fast; // control
    }
    match dev.ep_types.get(&ep) {
        Some(TransferType::Interrupt) => Lane::Fast,
        _ => Lane::Bulk,
    }
}

/// A single-slot, coalescing handoff from a read-ahead poller thread to the
/// async task that answers the host's interrupt-IN URB. The poller overwrites
/// with the freshest report; the consumer takes it (or waits if none yet).
///
/// Coalescing (keep-latest) is correct for state-snapshot HID — gamepads,
/// keyboards — where a stale report is worthless once a newer one exists, and
/// it bounds staleness to one poll. On a fast link the consumer drains before
/// the next report lands, so nothing is ever actually dropped. (A relative-delta
/// mouse under a saturated slow link is the one case this would coarsen; on
/// wired it never triggers.)
struct LatestSlot {
    latest: std::sync::Mutex<Option<Vec<u8>>>,
    notify: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
}

impl LatestSlot {
    fn new() -> Self {
        LatestSlot {
            latest: std::sync::Mutex::new(None),
            notify: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// Poller side: publish the freshest report (overwriting any unconsumed one).
    fn put(&self, data: Vec<u8>) {
        *self.latest.lock().unwrap() = Some(data);
        self.notify.notify_one();
    }
    /// Poller exited (device gone / relay ending) — wake any waiter to bail.
    fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }
    /// Consumer side: take the latest report, or wait until one arrives. Returns
    /// `None` if the poller closed with nothing pending.
    async fn take(&self) -> Option<Vec<u8>> {
        loop {
            if let Some(d) = self.latest.lock().unwrap().take() {
                return Some(d);
            }
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            self.notify.notified().await;
        }
    }
}

/// Continuously poll one interrupt-IN endpoint into its slot until `cancel`
/// fires or the device vanishes. Short read timeout so idle re-pends cheaply and
/// cancellation is responsive (no spurious -110 ever reaches the host — an idle
/// endpoint simply leaves the consumer's `take()` pending, which is exactly what
/// a real interrupt URB does).
fn spawn_reader(
    dev: Arc<Device>,
    ep: u8,
    max_pkt: usize,
    slot: Arc<LatestSlot>,
    cancel: CancellationToken,
) {
    std::thread::spawn(move || {
        let addr = 0x80 | ep;
        let mut buf = vec![0u8; max_pkt.max(64)];
        let timeout = Duration::from_millis(100);
        while !cancel.is_cancelled() {
            match dev.handle.read_interrupt(addr, &mut buf, timeout) {
                Ok(n) => slot.put(buf[..n].to_vec()),
                Err(rusb::Error::Timeout) => {} // idle — re-pend
                Err(rusb::Error::NoDevice) => break,
                Err(_) => std::thread::sleep(Duration::from_millis(5)), // transient
            }
        }
        slot.close();
    });
}

/// Hand the device back to the kernel cleanly (so a USB stick re-binds usb-storage,
/// a keyboard works locally again, …). Idempotent / best-effort.
fn reattach(handle: &DeviceHandle<GlobalContext>, ifaces: &[u8]) {
    for &n in ifaces {
        let _ = handle.release_interface(n);
        let _ = handle.attach_kernel_driver(n);
    }
}

fn service(dev: &Device, cmd: &CmdSubmit, out: &[u8]) -> RetSubmit {
    let mut ret = RetSubmit::for_request(cmd);
    let timeout = Duration::from_secs(5);
    let h = &dev.handle;

    if cmd.basic.ep == 0 {
        let bm = cmd.setup[0];
        let req = cmd.setup[1];
        let val = u16::from_le_bytes([cmd.setup[2], cmd.setup[3]]);
        let idx = u16::from_le_bytes([cmd.setup[4], cmd.setup[5]]);
        let wlen = u16::from_le_bytes([cmd.setup[6], cmd.setup[7]]) as usize;
        if bm & 0x80 != 0 {
            let mut buf = vec![0u8; wlen];
            match h.read_control(bm, req, val, idx, &mut buf, timeout) {
                Ok(n) => {
                    buf.truncate(n);
                    ret.actual_length = n as i32;
                    ret.data = buf;
                }
                Err(e) => ret.status = usbip_status(e),
            }
        } else {
            match h.write_control(bm, req, val, idx, out, timeout) {
                Ok(n) => ret.actual_length = n as i32,
                Err(e) => ret.status = usbip_status(e),
            }
        }
        return ret;
    }

    let ep = cmd.basic.ep as u8;
    let ttype = dev.ep_types.get(&ep).copied().unwrap_or(TransferType::Bulk);
    if cmd.is_in() {
        let addr = 0x80 | ep;
        let mut buf = vec![0u8; cmd.transfer_buffer_length.max(0) as usize];
        let r = match ttype {
            TransferType::Interrupt => h.read_interrupt(addr, &mut buf, timeout),
            _ => h.read_bulk(addr, &mut buf, timeout),
        };
        match r {
            Ok(n) => {
                buf.truncate(n);
                ret.actual_length = n as i32;
                ret.data = buf;
            }
            Err(e) => ret.status = usbip_status(e),
        }
    } else {
        let addr = ep;
        let r = match ttype {
            TransferType::Interrupt => h.write_interrupt(addr, out, timeout),
            _ => h.write_bulk(addr, out, timeout),
        };
        match r {
            Ok(n) => ret.actual_length = n as i32,
            Err(e) => ret.status = usbip_status(e),
        }
    }
    ret
}

/// Open `vid:pid`, detach kernel drivers, claim interfaces, map endpoint types.
fn claim(vid: u16, pid: u16) -> Result<(Device, core_usbip::hello::Hello)> {
    let handle = rusb::open_device_with_vid_pid(vid, pid).with_context(|| {
        format!(
            "device {vid:04x}:{pid:04x} not found / not openable (plugged in? if so, it's \
                 usually permissions: run with sudo, or add a udev rule such as \
                 SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{vid:04x}\", \
                 ATTR{{idProduct}}==\"{pid:04x}\", TAG+=\"uaccess\")"
        )
    })?;
    let dev = handle.device();
    let dd = dev.device_descriptor().context("descriptor")?;
    let name = friendly_name(&dev, &dd);
    let devid = (u32::from(dev.bus_number()) << 16) | u32::from(dev.address());
    let speed = speed_code(dev.speed());

    let mut ep_types = HashMap::new();
    let mut ifaces = Vec::new();
    let mut int_in: Vec<(u8, usize)> = Vec::new();
    let mut iface_triples = Vec::new();
    let cfg = dev.active_config_descriptor().context("active config")?;
    for iface in cfg.interfaces() {
        ifaces.push(iface.number());
        for (i, desc) in iface.descriptors().enumerate() {
            // The v2 hello carries the active alt-setting's class triple.
            if i == 0 {
                iface_triples.push(core_usbip::op::Interface {
                    class: desc.class_code(),
                    subclass: desc.sub_class_code(),
                    protocol: desc.protocol_code(),
                });
            }
            for ep in desc.endpoint_descriptors() {
                let num = ep.address() & 0x0f;
                ep_types.insert(num, ep.transfer_type());
                if ep.direction() == rusb::Direction::In
                    && ep.transfer_type() == TransferType::Interrupt
                    && !int_in.iter().any(|(e, _)| *e == num)
                {
                    int_in.push((num, ep.max_packet_size() as usize));
                }
            }
        }
    }
    // Everything a Windows importer must echo into OP_REP_IMPORT (hello v2).
    let v = dd.device_version();
    let hello = core_usbip::hello::Hello {
        name,
        devid,
        speed,
        desc: Some(core_usbip::hello::DeviceDesc {
            busnum: u32::from(dev.bus_number()),
            devnum: u32::from(dev.address()),
            id_vendor: dd.vendor_id(),
            id_product: dd.product_id(),
            bcd_device: (u16::from(v.major()) << 8)
                | (u16::from(v.minor()) << 4)
                | u16::from(v.sub_minor()),
            device_class: dd.class_code(),
            device_subclass: dd.sub_class_code(),
            device_protocol: dd.protocol_code(),
            configuration_value: cfg.number(),
            num_configurations: dd.num_configurations(),
            interfaces: iface_triples,
        }),
    };
    // Route check: warn loudly before relaying something the relay can't carry
    // well, and point at the right path. (We warn rather than hard-block so a
    // determined user can still experiment.)
    let route = relayability(&cfg);
    if route != Relayability::Relay {
        tracing::warn!(
            "'{}' [{vid:04x}:{pid:04x}] → {}: {}",
            hello.name,
            route.label(),
            route.note()
        );
    }

    let _ = handle.set_auto_detach_kernel_driver(true);
    for &n in &ifaces {
        handle
            .claim_interface(n)
            .with_context(|| format!("claim interface {n}"))?;
    }
    Ok((
        Device {
            handle,
            ep_types,
            ifaces,
            int_in,
        },
        hello,
    ))
}

/// Claim one device and relay its URBs over a dedicated connection until the
/// stream ends or `cancel` fires (toggle-off / shutdown). Always hands the
/// device back to the kernel on the way out. The friendly name rides the
/// handshake so Pharos can show it.
async fn relay_device(
    pharos: std::net::SocketAddr,
    vid: u16,
    pid: u16,
    cancel: CancellationToken,
) -> Result<()> {
    let (device, hello) = claim(vid, pid)?;
    let name = hello.name.clone();
    tracing::info!(
        "sharing '{name}' [{vid:04x}:{pid:04x}] (devid={:#010x}, speed={})",
        hello.devid,
        hello.speed
    );
    let ifaces = device.ifaces.clone();
    let int_in = device.int_in.clone();
    let device = Arc::new(device);

    // Connect — but bail (and reattach) if toggled off before Pharos answers.
    let link = tokio::select! {
        _ = cancel.cancelled() => { reattach(&device.handle, &ifaces); return Ok(()); }
        r = transport::usb_export_connect(pharos) => match r {
            Ok(l) => l,
            Err(e) => { reattach(&device.handle, &ifaces); return Err(e).context("connect to Pharos"); }
        },
    };

    let mut link = link;
    // Hello: v2 (descriptor block — what a Windows import bridge needs) when
    // the importer advertised it, else the legacy v1 triplet.
    let hello_bytes = if link.importer_v2() {
        hello.to_bytes_v2()
    } else {
        hello.to_bytes_v1()
    };
    link.send.write_all(&hello_bytes).await?;
    tracing::info!(
        "'{name}' attached on Pharos {} (hello {})",
        link.remote(),
        if link.importer_v2() { "v2" } else { "v1" }
    );

    // Read-ahead: one poller thread per interrupt-IN endpoint feeding a latest-
    // report slot, so input is captured continuously instead of only while the
    // host's URB is outstanding. `shutdown` stops the pollers (and wakes any
    // interrupt-IN consumer) when the relay ends, by cancel OR stream drop.
    let shutdown = CancellationToken::new();
    let mut slots: HashMap<u8, Arc<LatestSlot>> = HashMap::new();
    for (ep, max_pkt) in &int_in {
        let slot = Arc::new(LatestSlot::new());
        slots.insert(*ep, slot.clone());
        spawn_reader(device.clone(), *ep, *max_pkt, slot, shutdown.clone());
    }
    let slots = Arc::new(slots);

    // QoS-lite: a fast lane (interrupt + control) and a bulk lane. The writer is
    // biased to drain the fast lane first, so input replies overtake queued bulk.
    let (tx_fast, mut rx_fast) = mpsc::channel::<Vec<u8>>(256);
    let (tx_bulk, mut rx_bulk) = mpsc::channel::<Vec<u8>>(256);
    let mut send = link.send;
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                Some(b) = rx_fast.recv() => if send.write_all(&b).await.is_err() { break },
                Some(b) = rx_bulk.recv() => if send.write_all(&b).await.is_err() { break },
                else => break,
            }
        }
    });

    // In-flight CMD_SUBMITs → a per-URB cancel token. CMD_UNLINK cancels it so
    // the worker aborts and suppresses its now-stale RET_SUBMIT. Without this, a
    // reply arriving after vhci freed the URB triggers "cannot find a urb of
    // seqnum N" and resets the device — the controller-cycling bug.
    let pending: Arc<std::sync::Mutex<HashMap<u32, CancellationToken>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    let mut recv = link.recv;
    loop {
        let mut hdr = [0u8; HEADER_LEN];
        tokio::select! {
            _ = cancel.cancelled() => break,
            r = recv.read_exact(&mut hdr) => if r.is_err() { break },
        }
        match command_of(&hdr) {
            USBIP_CMD_SUBMIT => {
                let cmd = CmdSubmit::parse(&hdr).expect("checked");
                let mut out = vec![0u8; cmd.out_data_len()];
                if !out.is_empty() && recv.read_exact(&mut out).await.is_err() {
                    break;
                }
                if cmd.is_iso() {
                    // Drain the trailing iso_packet_descriptor array so the byte
                    // stream stays framed, then reject every packet cleanly.
                    // (rusb has no iso API; real iso servicing needs the libusb
                    // async/FFI path — see the iso note in core-usbip.)
                    let mut iso = vec![0u8; cmd.iso_descriptor_len()];
                    if !iso.is_empty() && recv.read_exact(&mut iso).await.is_err() {
                        break;
                    }
                    let _ = tx_fast
                        .send(RetSubmit::iso_reject(&cmd, -22).to_bytes())
                        .await;
                    continue;
                }
                let seqnum = cmd.basic.seqnum;
                let ep = cmd.basic.ep as u8;
                let tok = CancellationToken::new();
                pending.lock().unwrap().insert(seqnum, tok.clone());
                let pend = pending.clone();
                let tx = match lane_for(&device, ep) {
                    Lane::Fast => tx_fast.clone(),
                    Lane::Bulk => tx_bulk.clone(),
                };

                // Interrupt-IN: answer from the read-ahead slot (freshest report),
                // racing the host's unlink and relay shutdown.
                if cmd.is_in() && ep != 0 {
                    if let Some(slot) = slots.get(&ep).cloned() {
                        let sd = shutdown.clone();
                        tokio::spawn(async move {
                            let reply = tokio::select! {
                                _ = tok.cancelled() => None,
                                _ = sd.cancelled() => None,
                                data = slot.take() => data.map(|d| {
                                    let mut ret = RetSubmit::for_request(&cmd);
                                    ret.actual_length = d.len() as i32;
                                    ret.data = d;
                                    ret.to_bytes()
                                }),
                            };
                            pend.lock().unwrap().remove(&seqnum);
                            if let Some(bytes) = reply {
                                if !tok.is_cancelled() {
                                    let _ = tx.send(bytes).await;
                                }
                            }
                        });
                        continue;
                    }
                }

                // Control / bulk / interrupt-OUT: one blocking libusb call.
                let dev = device.clone();
                tokio::task::spawn_blocking(move || {
                    let bytes = service(&dev, &cmd, &out).to_bytes();
                    let cancelled = {
                        pend.lock().unwrap().remove(&seqnum);
                        tok.is_cancelled()
                    };
                    if !cancelled {
                        let _ = tx.blocking_send(bytes);
                    }
                });
            }
            USBIP_CMD_UNLINK => {
                let u = CmdUnlink::parse(&hdr).expect("checked");
                if let Some(tok) = pending.lock().unwrap().get(&u.unlink_seqnum) {
                    tok.cancel();
                }
                let _ = tx_fast.send(ret_unlink(&u, 0)).await;
            }
            other => {
                tracing::warn!("unexpected USB/IP command {other}; ending '{name}'");
                break;
            }
        }
    }
    // Stop pollers + wake interrupt-IN consumers, then drain in-flight replies.
    shutdown.cancel();
    drop(tx_fast);
    drop(tx_bulk);
    let _ = writer.await;
    reattach(&device.handle, &ifaces);
    tracing::info!("'{name}' relay ended; device returned to the kernel");
    Ok(())
}

/// Print attached devices with friendly names; flag the ones in `share`.
fn list(config: &str) {
    let shared: Vec<(u16, u16)> = load_config(config)
        .map(|c| {
            c.share
                .iter()
                .filter_map(|s| parse_vid_pid(s).ok())
                .collect()
        })
        .unwrap_or_default();
    let devices = match rusb::devices() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("enumerate: {e}");
            return;
        }
    };
    for dev in devices.iter() {
        let Ok(dd) = dev.device_descriptor() else {
            continue;
        };
        let (vid, pid) = (dd.vendor_id(), dd.product_id());
        let cfg = dev.active_config_descriptor().ok();
        let cls = cfg
            .as_ref()
            .and_then(|c| {
                c.interfaces()
                    .next()
                    .and_then(|i| i.descriptors().next().map(|d| d.class_code()))
            })
            .unwrap_or(dd.class_code());
        let route = cfg
            .as_ref()
            .map(relayability)
            .unwrap_or(Relayability::Relay);
        let tag = if shared.contains(&(vid, pid)) {
            "  [shared]"
        } else {
            ""
        };
        println!(
            "{:04x}:{:04x}  {:<32}  {:<12}  {:<16}{}",
            vid,
            pid,
            friendly_name(&dev, &dd),
            class_name(cls),
            route.label(),
            tag
        );
        if route != Relayability::Relay {
            println!("             ↳ {}", route.note());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "trireme_usbexport=info,info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("list") => {
            list(args.get(2).map(String::as_str).unwrap_or(DEFAULT_CONFIG));
            Ok(())
        }
        Some("run") => {
            supervise(
                args.get(2)
                    .map(String::as_str)
                    .unwrap_or(DEFAULT_CONFIG)
                    .to_string(),
            )
            .await
        }
        // Direct single-device mode: `<pharos:port> <vid:pid>`.
        Some(pharos_arg) if pharos_arg.contains(':') && pharos_arg.contains('.') => {
            let pharos = pharos_arg.parse().context("parse <pharos:port>")?;
            let (vid, pid) = parse_vid_pid(args.get(2).map(String::as_str).unwrap_or(""))?;
            let token = CancellationToken::new();
            spawn_signal_watch(token.clone());
            relay_device(pharos, vid, pid, token).await
        }
        _ => {
            eprintln!("usage: trireme-usbexport <list|run [config]|<pharos:port> <vid:pid>>");
            std::process::exit(2);
        }
    }
}

fn parse_vid_pid(s: &str) -> Result<(u16, u16)> {
    let (v, p) = s.split_once(':').context("expected vid:pid")?;
    Ok((u16::from_str_radix(v, 16)?, u16::from_str_radix(p, 16)?))
}

/// Supervisor: poll the config, keep one relay task per shared device alive —
/// start on toggle-on, cancel + reattach on toggle-off, reap + retry if a relay
/// drops (Pharos restart) — and reattach everything on Ctrl-C / SIGTERM.
async fn supervise(config_path: String) -> Result<()> {
    let shutdown = CancellationToken::new();
    spawn_signal_watch(shutdown.clone());
    tracing::info!("supervising {config_path} (edit `share` to toggle devices)");

    type Relay = (tokio::task::JoinHandle<()>, CancellationToken);
    let mut active: HashMap<(u16, u16), Relay> = HashMap::new();

    while !shutdown.is_cancelled() {
        match load_config(&config_path)
            .and_then(|c| Ok((c.pharos.parse::<std::net::SocketAddr>()?, c)))
        {
            Ok((pharos, cfg)) => {
                let desired: HashSet<(u16, u16)> = cfg
                    .share
                    .iter()
                    .filter_map(|s| parse_vid_pid(s).ok())
                    .collect();

                // Reap relays that ended on their own (Pharos dropped) so they retry below.
                let ended: Vec<_> = active
                    .iter()
                    .filter(|(_, (h, _))| h.is_finished())
                    .map(|(k, _)| *k)
                    .collect();
                for k in ended {
                    active.remove(&k);
                }
                // Toggle-on (and retry reaped): start a relay per newly-shared device.
                for &(vid, pid) in &desired {
                    active.entry((vid, pid)).or_insert_with(|| {
                        let token = CancellationToken::new();
                        let t = token.clone();
                        let h = tokio::spawn(async move {
                            if let Err(e) = relay_device(pharos, vid, pid, t).await {
                                tracing::warn!("{vid:04x}:{pid:04x}: {e}");
                            }
                        });
                        (h, token)
                    });
                }
                // Toggle-off: cancel relays no longer shared (cancellation reattaches).
                let gone: Vec<_> = active
                    .keys()
                    .filter(|k| !desired.contains(k))
                    .copied()
                    .collect();
                for k in gone {
                    if let Some((h, token)) = active.remove(&k) {
                        tracing::info!("unsharing {:04x}:{:04x}", k.0, k.1);
                        token.cancel();
                        let _ = h.await;
                    }
                }
            }
            Err(e) => tracing::error!("config: {e}"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }

    tracing::info!("shutdown — reattaching {} device(s)", active.len());
    for (_, (h, token)) in active.drain() {
        token.cancel();
        let _ = h.await;
    }
    Ok(())
}

/// Cancel `token` on Ctrl-C or SIGTERM so relays exit cleanly and reattach.
fn spawn_signal_watch(token: CancellationToken) {
    let t1 = token.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        t1.cancel();
    });
    #[cfg(unix)]
    tokio::spawn(async move {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
            token.cancel();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{classify, Relayability};

    #[test]
    fn routing_decision() {
        // ordinary device → plain relay
        assert_eq!(classify(false, false), Relayability::Relay);
        // pure iso device (USB audio / webcam) → not serviceable
        assert_eq!(classify(false, true), Relayability::IsoUnsupported);
        // Bluetooth radio → pair-and-forward
        assert_eq!(classify(true, false), Relayability::PairAndForward);
        // BT radios also expose a SCO/iso interface — BT still wins.
        assert_eq!(classify(true, true), Relayability::PairAndForward);
    }
}
