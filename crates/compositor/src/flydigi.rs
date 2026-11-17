// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Flydigi Vader 5 Pro gyro → the DSU motion server.
//!
//! A Vader forwarded over USB/IP (or plugged into the host) shows up as an
//! xpad Xbox pad, which carries no motion sensors. Its IMU only streams over
//! the controller's vendor HID interface (usage page `0xFFA0`, 32-byte
//! unnumbered reports), and only to a program that *acquires* the controller.
//! This module does what SDL's `SDL_hidapi_flydigi.c` does: find that
//! interface, acquire it, keep the claim alive, and decode the `0xEF` input
//! reports into gyro/accel samples the render loop forwards to the DSU server,
//! so emulators get the Vader's own gyro.
//!
//! The controller refuses acquisition unless its "Allow third-party apps to
//! take over mappings" setting is on (normally flipped in Flydigi's Space
//! Station app). When it's off, we turn it on with the same `0x11` frame Space
//! Station sends (every other setting left unchanged); see the
//! flydigi-vader-pro-5-ctl project's `docs/protocol-settings.md`.
//!
//! Only active while `config.toml [dsu] enabled` is on: with the DSU server
//! off nothing is opened or acquired, and switching it off releases the
//! controller so it goes back to its own mapping.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

/// Flydigi's current USB vendor id (Vader 5 Pro, Apex 5/6).
const FLYDIGI_V2_VENDOR: u32 = 0x37D7;
const MAGIC: [u8; 2] = [0x5A, 0xA5];
const CMD_GET_INFO: u8 = 0x01;
const CMD_GET_STATUS: u8 = 0x10;
const CMD_SET_STATUS: u8 = 0x11;
const CMD_ACQUIRE: u8 = 0x1C;
const INPUT_REPORT: u8 = 0xEF;
/// SDL re-sends acquire/info/status this often to keep the claim.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// No input report for this long ⇒ release + re-acquire (SDL's recovery).
const REACQUIRE_AFTER: Duration = Duration::from_secs(1);
const REACQUIRE_EVERY: Duration = Duration::from_secs(2);
const RESCAN: Duration = Duration::from_secs(2);
const STANDARD_GRAVITY: f32 = 9.806_65;

/// What the reader thread reports to the render loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// A Vader was acquired and is streaming; its USB ids key its saved
    /// controller profile (invert / axis choices from Trireme's gyro screen).
    Connected { vendor: u16, product: u16 },
    /// It went away (unplugged, read error, or the DSU server was switched off).
    Disconnected,
    /// One IMU sample in Trireme's canonical frame (gyro rad/s, accel m/s²),
    /// the same units and frame a client's `GamepadMotion` arrives in.
    Sample { gyro: [f32; 3], accel: [f32; 3] },
}

/// Handle the render loop drains each frame.
pub struct FlydigiGyro {
    rx: Receiver<Msg>,
}

impl FlydigiGyro {
    /// Spawn the scanner/reader thread. Never blocks; idle without a Vader.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::sync_channel(1024);
        if let Err(e) = std::thread::Builder::new()
            .name("pharos-flydigi".into())
            .spawn(move || manager_loop(&tx))
        {
            tracing::warn!("flydigi: couldn't spawn reader thread: {e}");
        }
        Self { rx }
    }

    /// Everything reported since the last call, in order.
    pub fn drain(&self) -> Vec<Msg> {
        self.rx.try_iter().collect()
    }
}

fn dsu_enabled() -> bool {
    host_core::config::load().dsu.enabled
}

fn manager_loop(tx: &SyncSender<Msg>) {
    let mut warned_perm: Option<PathBuf> = None;
    loop {
        if dsu_enabled() {
            if let Some((path, product)) = find_vendor_interface() {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)
                {
                    Ok(dev) => {
                        warned_perm = None;
                        run_device(dev, &path, product, tx);
                    }
                    Err(e) => {
                        if warned_perm.as_ref() != Some(&path) {
                            tracing::warn!("flydigi: can't open {}: {e}", path.display());
                            warned_perm = Some(path);
                        }
                    }
                }
            }
        }
        std::thread::sleep(RESCAN);
    }
}

/// The hidraw node of a Flydigi V2 controller's vendor interface (report
/// descriptor opens with usage page `0xFFA0`), with the controller's product id.
fn find_vendor_interface() -> Option<(PathBuf, u16)> {
    let entries = std::fs::read_dir("/sys/class/hidraw").ok()?;
    for entry in entries.flatten() {
        let dev = entry.path().join("device");
        let Ok(uevent) = std::fs::read_to_string(dev.join("uevent")) else {
            continue;
        };
        // HID_ID=bus:vendor:product, each in hex.
        let Some(product) = uevent.lines().find_map(|l| {
            let mut id = l.strip_prefix("HID_ID=")?.split(':').skip(1);
            let vendor = u32::from_str_radix(id.next()?, 16).ok()?;
            let product = u32::from_str_radix(id.next()?, 16).ok()?;
            (vendor == FLYDIGI_V2_VENDOR).then_some(product as u16)
        }) else {
            continue;
        };
        let desc = std::fs::read(dev.join("report_descriptor")).unwrap_or_default();
        if desc.starts_with(&[0x06, 0xA0, 0xFF]) {
            return Some((Path::new("/dev").join(entry.file_name()), product));
        }
    }
    None
}

/// A command frame: magic, command, length (= 2 + payload), payload, and the
/// 8-bit additive checksum over command..payload (Space Station's `Crc`).
fn frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut a = vec![MAGIC[0], MAGIC[1], cmd, (2 + payload.len()) as u8];
    a.extend_from_slice(payload);
    let sum = a[2..].iter().fold(0u8, |s, b| s.wrapping_add(*b));
    a.push(sum);
    a
}

/// The acquire/release request as SDL sends it (no checksum; a 20-byte tag).
fn acquire_frame(acquire: bool) -> Vec<u8> {
    let mut a = vec![MAGIC[0], MAGIC[1], CMD_ACQUIRE, 23, u8::from(acquire)];
    a.extend_from_slice(b"Pharos");
    a
}

/// Write one unnumbered output report (hidraw wants a leading report-id 0).
fn send(dev: &mut File, body: &[u8]) -> std::io::Result<()> {
    let mut buf = [0u8; 32];
    let n = body.len().min(31);
    buf[1..=n].copy_from_slice(&body[..n]);
    dev.write_all(&buf)
}

fn run_device(mut dev: File, path: &Path, product: u16, tx: &SyncSender<Msg>) {
    use std::os::fd::AsRawFd;
    let fd = dev.as_raw_fd();
    let mut announced = false;
    let mut allow_sent = false;
    let mut next_heartbeat = Instant::now();
    let mut last_input = Instant::now();
    let mut next_reacquire = Instant::now();
    let mut next_config_check = Instant::now() + RESCAN;
    let mut buf = [0u8; 64];
    let _ = send(&mut dev, &frame(CMD_GET_STATUS, &[]));
    loop {
        let now = Instant::now();
        if now >= next_config_check {
            next_config_check = now + RESCAN;
            if !dsu_enabled() {
                let _ = send(&mut dev, &acquire_frame(false));
                break;
            }
        }
        if now >= next_heartbeat {
            let _ = send(&mut dev, &acquire_frame(true));
            let _ = send(&mut dev, &frame(CMD_GET_INFO, &[]));
            let _ = send(&mut dev, &frame(CMD_GET_STATUS, &[]));
            next_heartbeat = now + HEARTBEAT;
        }
        if now >= last_input + REACQUIRE_AFTER && now >= next_reacquire {
            let _ = send(&mut dev, &acquire_frame(false));
            let _ = send(&mut dev, &acquire_frame(true));
            next_reacquire = now + REACQUIRE_EVERY;
        }

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for an fd `dev` keeps open.
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready < 0 || pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if ready == 0 {
            continue;
        }
        let n = match dev.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break, // unplugged
        };
        let data = &buf[..n];
        if n < 31 || data[..2] != MAGIC {
            continue;
        }
        match data[2] {
            CMD_GET_STATUS => {
                if data[9] != 1 && !allow_sent {
                    // "Allow third-party apps to take over mappings" is off; turn
                    // it on (0xFF = leave the other four flags unchanged).
                    tracing::info!(
                        "flydigi: enabling third-party control on {} so its gyro can be read",
                        path.display()
                    );
                    let _ = send(
                        &mut dev,
                        &frame(CMD_SET_STATUS, &[0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
                    );
                    allow_sent = true;
                    next_heartbeat = Instant::now();
                }
            }
            // The controller's own status-changed notice: ask again.
            CMD_SET_STATUS => {
                let _ = send(&mut dev, &frame(CMD_GET_STATUS, &[]));
            }
            INPUT_REPORT => {
                last_input = Instant::now();
                if !announced {
                    tracing::info!("flydigi: gyro streaming from {}", path.display());
                    let connected = Msg::Connected {
                        vendor: FLYDIGI_V2_VENDOR as u16,
                        product,
                    };
                    if tx.send(connected).is_err() {
                        return;
                    }
                    announced = true;
                }
                let (gyro, accel) = decode_imu(data);
                // Drop on a full queue rather than stall the reader.
                let _ = tx.try_send(Msg::Sample { gyro, accel });
            }
            _ => {}
        }
    }
    if announced {
        tracing::info!("flydigi: {} released", path.display());
        let _ = tx.send(Msg::Disconnected);
    }
}

/// Decode an `0xEF` input report's IMU into Trireme's canonical frame.
///
/// SDL's Vader 5 Pro mapping gives gyro (rad/s) and accel (m/s²) in SDL's
/// gamepad frame (x right, y up, z toward the player). The DSU server's
/// default axis map (`-y,x,z`) expects the canonical frame of a landscape
/// phone, so hand it `[sdl_y, -sdl_x, sdl_z]`, which that map turns back into
/// the SDL frame.
fn decode_imu(d: &[u8]) -> ([f32; 3], [f32; 3]) {
    let s = |i: usize| f32::from(i16::from_le_bytes([d[i], d[i + 1]]));
    // HIDAPI_RemapVal(v, INT16_MIN, INT16_MAX, -scale, scale), scale = 2000°/s.
    let scale = 2000.0_f32.to_radians();
    let remap = |v: f32| (v + 32768.0) / 65535.0 * 2.0 * scale - scale;
    // Pitch is at bytes 21..22 and yaw at 17..18: the reverse of SDL's
    // labels, as tested live in Lichii (pitch came out as yaw).
    let sdl_gyro = [remap(s(21)), remap(s(17)), remap(-s(19))];
    let g = STANDARD_GRAVITY / 4096.0;
    let sdl_accel = [s(23) * g, s(27) * g, -s(25) * g];
    let canon = |v: [f32; 3]| [v[1], -v[0], v[2]];
    (canon(sdl_gyro), canon(sdl_accel))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented "allow third-party control" frame, byte for byte.
    #[test]
    fn set_status_frame_matches_space_station() {
        assert_eq!(
            frame(CMD_SET_STATUS, &[0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
            [0x5A, 0xA5, 0x11, 0x07, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x15]
        );
    }

    /// A Vader lying flat (a real capture: accel ≈ +4096 at bytes 27..28, SDL's
    /// "up" axis) reads ≈ +1 g up, which is canonical x, with no rotation.
    #[test]
    fn decodes_a_flat_controller() {
        let mut d = [0u8; 32];
        d[..3].copy_from_slice(&[0x5A, 0xA5, INPUT_REPORT]);
        d[27..29].copy_from_slice(&4122i16.to_le_bytes());
        let (gyro, accel) = decode_imu(&d);
        for v in gyro {
            assert!(v.abs() < 0.001, "gyro {gyro:?}");
        }
        assert!((accel[0] - STANDARD_GRAVITY).abs() < 0.1, "accel {accel:?}");
        assert!(
            accel[1].abs() < 0.01 && accel[2].abs() < 0.01,
            "accel {accel:?}"
        );
    }
}
