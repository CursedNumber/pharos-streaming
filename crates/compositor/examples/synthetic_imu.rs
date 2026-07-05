// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Synthetic controller IMU for validating the host→client gyro telemetry path
//! without a physical gyro pad. Creates a uinput device named "… IMU" with the
//! gyro axes `gyro_imu` reads (ABS_RX/RY/RZ, codes 3/4/5) and emits an
//! oscillating angular velocity, so the host's reader locks it, streams the
//! samples to the client, and the calibration visualiser animates.
//!
//!   cargo run --release --example synthetic_imu -p compositor
//!
//! Needs /dev/uinput write access (the `input` group). Ctrl-C to stop.

#[cfg(target_os = "linux")]
mod imp {
    use std::thread;
    use std::time::Duration;

    use evdev::{
        uinput::VirtualDeviceBuilder, AbsInfo, AbsoluteAxisType, BusType, EventType, InputEvent,
        InputId, UinputAbsSetup,
    };

    const EV_ABS: EventType = EventType(3);
    const EV_SYN: EventType = EventType(0);
    const ABS_RX: u16 = 0x03;
    const ABS_RY: u16 = 0x04;
    const ABS_RZ: u16 = 0x05;

    pub fn run() -> anyhow::Result<()> {
        // resolution = units per deg/s, so the host normalises value/res → deg/s.
        // Matches the Switch Pro's hid-nintendo scale used during bring-up.
        let res = 14247;
        let info = AbsInfo::new(0, -4_000_000, 4_000_000, 0, 0, res);

        let mut builder = VirtualDeviceBuilder::new()?
            .name("Pharos Synthetic IMU")
            .input_id(InputId::new(BusType(0x06), 0x057e, 0x2009, 1));
        for code in [ABS_RX, ABS_RY, ABS_RZ] {
            builder =
                builder.with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisType(code), info))?;
        }
        let mut dev = builder.build()?;
        eprintln!("Pharos Synthetic IMU up — oscillating yaw(RZ)/pitch(RY) (Ctrl-C to stop)");

        let r = f64::from(res);
        let mut t = 0.0f64;
        loop {
            t += 0.016;
            // Angular velocity in deg/s → raw value = deg/s × resolution.
            let yaw = (t * 1.6).sin() * 200.0 * r; // RZ → ←→
            let pitch = (t * 1.1).cos() * 120.0 * r; // RY → ↑↓
            let roll = (t * 0.7).sin() * 60.0 * r; // RX (unmapped)
            let _ = dev.emit(&[
                InputEvent::new(EV_ABS, ABS_RX, roll as i32),
                InputEvent::new(EV_ABS, ABS_RY, pitch as i32),
                InputEvent::new(EV_ABS, ABS_RZ, yaw as i32),
                InputEvent::new(EV_SYN, 0, 0),
            ]);
            thread::sleep(Duration::from_millis(16)); // ~60 Hz
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("synthetic_imu is a Linux-only example (evdev/uinput).");
    std::process::exit(1);
}
