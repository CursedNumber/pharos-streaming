// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Synthesise a virtual Xbox-style gamepad via uinput and hold one button for a
//! few seconds, so a game can be driven with no physical/paired controller.
//! Used to exercise the input path into a Pharos-streamed game (e.g. "hold B to
//! quit"). Mirrors the device layout `gamepad-bridge`'s `build_uinput` creates,
//! which Proton/SDL already recognise as a native game controller.
//!
//!   cargo run -p gamepad-bridge --example hold-button -- [BUTTON] [SECONDS]
//!     BUTTON: A B X Y LB RB START BACK GUIDE   (default B)
//!     SECONDS: hold duration                    (default 3)

#[cfg(target_os = "linux")]
mod imp {
    use std::{thread::sleep, time::Duration};

    use anyhow::{bail, Result};
    use evdev::{
        uinput::VirtualDeviceBuilder, AbsInfo, AbsoluteAxisType, AttributeSet, EventType,
        InputEvent, Key, UinputAbsSetup,
    };

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let btn = args
            .get(1)
            .map(|s| s.to_uppercase())
            .unwrap_or_else(|| "B".into());
        let secs: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3.0);

        // evdev gamepad codes: A=SOUTH, B=EAST, X=NORTH, Y=WEST (kernel xpad layout).
        let key = match btn.as_str() {
            "A" => Key::BTN_SOUTH,
            "B" => Key::BTN_EAST,
            "X" => Key::BTN_NORTH,
            "Y" => Key::BTN_WEST,
            "LB" => Key::BTN_TL,
            "RB" => Key::BTN_TR,
            "START" => Key::BTN_START,
            "BACK" | "SELECT" => Key::BTN_SELECT,
            "GUIDE" | "HOME" => Key::BTN_MODE,
            other => bail!("unknown button {other} (A B X Y LB RB START BACK GUIDE)"),
        };

        // Full Xbox-style button set so SDL/Proton classifies it as a game controller.
        let mut keys = AttributeSet::<Key>::new();
        for k in [
            Key::BTN_SOUTH,
            Key::BTN_EAST,
            Key::BTN_NORTH,
            Key::BTN_WEST,
            Key::BTN_TL,
            Key::BTN_TR,
            Key::BTN_SELECT,
            Key::BTN_START,
            Key::BTN_MODE,
            Key::BTN_THUMBL,
            Key::BTN_THUMBR,
        ] {
            keys.insert(k);
        }

        let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
        let trig = AbsInfo::new(0, 0, 255, 0, 0, 0);
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        let axes = [
            (AbsoluteAxisType::ABS_X, stick),
            (AbsoluteAxisType::ABS_Y, stick),
            (AbsoluteAxisType::ABS_RX, stick),
            (AbsoluteAxisType::ABS_RY, stick),
            (AbsoluteAxisType::ABS_Z, trig),
            (AbsoluteAxisType::ABS_RZ, trig),
            (AbsoluteAxisType::ABS_HAT0X, hat),
            (AbsoluteAxisType::ABS_HAT0Y, hat),
        ];

        let mut builder = VirtualDeviceBuilder::new()?
            .name("Pharos Synthetic Xbox Pad")
            .with_keys(&keys)?;
        for (code, info) in axes {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(code, info))?;
        }
        let mut dev = builder.build()?;

        eprintln!("[synth] virtual pad created; waiting 2s for SDL/Proton hotplug…");
        sleep(Duration::from_secs(2));

        eprintln!("[synth] HOLD {btn} ({key:?}) for {secs}s");
        dev.emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?;
        sleep(Duration::from_secs_f64(secs));
        dev.emit(&[InputEvent::new(EventType::KEY, key.code(), 0)])?;

        eprintln!("[synth] released {btn}; lingering 1s then removing device");
        sleep(Duration::from_secs(1));
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("hold-button is a Linux-only example (evdev/uinput).");
    std::process::exit(1);
}
