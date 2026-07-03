// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Demo: drive this machine's real cursor from a controller's gyro, using the
//! exact `gyro_imu` reader that backs path-2 gyro→mouse in the streaming host.
//!
//! Needs a gyro-capable controller (Switch Pro / DualSense) attached over USB/IP
//! or Bluetooth so Linux's `hid-*` driver publishes its "(IMU)" evdev device.
//!
//!   cargo run --release --example gyro_mouse -p compositor
//!
//! Feel comes from the saved controller profile: set `PHAROS_GYRO_PROFILE` to a
//! `"vvvv:pppp"` id to apply that controller's entry from
//! `controller-profiles.toml` (default `057e:2009` = Switch Pro); falls back to
//! the built-in tuned default. The `PHAROS_GYRO_*` env vars still seed/override.
//! Ctrl-C to stop. (Reads /dev/input + writes /dev/uinput — needs the `input`
//! group or a uaccess ACL.)

use std::thread;
use std::time::Duration;

use evdev::{
    uinput::VirtualDeviceBuilder, AttributeSet, BusType, EventType, InputEvent, InputId, Key,
    RelativeAxisType,
};

const EV_REL: EventType = EventType(2);
const EV_SYN: EventType = EventType(0);
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // The real path-2 reader: finds the IMU, normalises by resolution, integrates.
    let gyro = compositor::gyro_imu::GyroImu::spawn();

    // Apply the saved profile for the chosen controller, exactly as the host
    // does on GamepadConnect — proves the persistence → live-apply path.
    let id = std::env::var("PHAROS_GYRO_PROFILE").unwrap_or_else(|_| "057e:2009".into());
    let profiles = host_core::controller_profiles::ControllerProfiles::load();
    if let Some(p) = profiles.controllers.get(&id) {
        tracing::info!("applying saved profile for {id} from controller-profiles.toml");
        gyro.set_tuning(&p.gyro);
    } else {
        tracing::info!("no saved profile for {id}; using env/default feel");
    }

    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::BTN_LEFT); // a button so libinput classifies us a pointer
    let mut rel = AttributeSet::<RelativeAxisType>::new();
    rel.insert(RelativeAxisType::REL_X);
    rel.insert(RelativeAxisType::REL_Y);
    let mut mouse = VirtualDeviceBuilder::new()?
        .name("Trireme gyro mouse")
        .input_id(InputId::new(BusType(0x03), 0x1209, 0x0001, 1))
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .build()?;

    eprintln!("gyro→mouse demo running — rotate the controller (Ctrl-C to stop)");

    // Carry sub-pixel remainders so slow motion isn't lost to integer truncation.
    let mut fx = 0.0f64;
    let mut fy = 0.0f64;
    loop {
        let (dx, dy) = gyro.take();
        fx += dx;
        fy += dy;
        let ix = fx as i32;
        let iy = fy as i32;
        fx -= f64::from(ix);
        fy -= f64::from(iy);
        if ix != 0 || iy != 0 {
            let _ = mouse.emit(&[
                InputEvent::new(EV_REL, REL_X, ix),
                InputEvent::new(EV_REL, REL_Y, iy),
                InputEvent::new(EV_SYN, 0, 0),
            ]);
        }
        thread::sleep(Duration::from_millis(8)); // ~120 Hz cursor updates
    }
}
