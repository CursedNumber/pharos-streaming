// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! USB/IP keyboard + mouse relay.
//!
//! A keyboard or mouse exported from the client over USB/IP is attached to the
//! host kernel via `vhci_hcd` and shows up as a host evdev device — which the
//! headless compositor wouldn't otherwise see (it has no libinput backend).
//!
//! We read **only** the vhci-imported devices: the host's *own* keyboard and
//! mouse (which drive the user's desktop) sit under a real USB/PCI controller,
//! not `vhci_hcd`, so they're never touched. The imported ones we **grab**, so
//! they drive the stream (the focused app) rather than double-typing into, or
//! moving the cursor on, the host desktop. Events go to the render loop over a
//! channel, which injects them into the Wayland seat.
//!
//! Best-effort throughout: missing permissions / no devices simply yield no
//! events; a relay hiccup never disturbs the stream.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One relayed input event.
#[derive(Debug, Clone, PartialEq)]
pub enum UsbEvent {
    /// A key transition: (Linux evdev code, pressed).
    Key(u16, bool),
    /// A mouse event, already in the network back-channel's shape
    /// (`PointerRel` / `PointerButton` / `Scroll`), so the render loop feeds it
    /// through the same seat dispatch as a client's mouse.
    Pointer(core_proto::InputEvent),
}

/// Handle the render loop drains. Spawn once; `drain` each frame.
pub struct UsbKbd {
    rx: Receiver<UsbEvent>,
}

impl UsbKbd {
    /// Spawn the manager thread (rescans `/dev/input` for vhci keyboards and
    /// mice) plus a reader thread per grabbed device. Never blocks; never panics.
    pub fn spawn() -> UsbKbd {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("pharos-usbkbd".into())
            .spawn(move || manager_loop(tx));
        UsbKbd { rx }
    }

    /// Take every event seen since the last call (in arrival order).
    pub fn drain(&self) -> Vec<UsbEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

fn manager_loop(tx: Sender<UsbEvent>) {
    // Paths we've already claimed, so a rescan doesn't open one twice. A reader
    // removes its path on exit, so a re-plug is picked up again.
    let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    loop {
        if let Ok(entries) = std::fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_event = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("event"));
                if !is_event || !is_vhci(&path) {
                    continue;
                }
                if open.lock().map(|o| o.contains(&path)).unwrap_or(true) {
                    continue;
                }
                let dev = match evdev::Device::open(&path) {
                    Ok(d) if is_keyboard(&d) || is_mouse(&d) => d,
                    _ => continue,
                };
                let kind = if is_mouse(&dev) { "mouse" } else { "keyboard" };
                if let Ok(mut o) = open.lock() {
                    o.insert(path.clone());
                }
                let tx = tx.clone();
                let open = open.clone();
                let path2 = path.clone();
                let _ = std::thread::Builder::new()
                    .name("pharos-usbkbd-dev".into())
                    .spawn(move || {
                        reader_loop(dev, &tx);
                        if let Ok(mut o) = open.lock() {
                            o.remove(&path2);
                        }
                    });
                tracing::info!("USB/IP {kind} relay reading {}", path.display());
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn reader_loop(mut dev: evdev::Device, tx: &Sender<UsbEvent>) {
    use core_proto::InputEvent;
    // Grab so the device drives the stream, not the host desktop. It's a
    // vhci-imported device, so this never affects the local keyboard/mouse.
    let _ = dev.grab();
    // Motion and wheel arrive as separate REL events per axis; coalesce them
    // until the SYN_REPORT that closes the hardware report.
    let (mut dx, mut dy, mut wheel_v, mut wheel_h) = (0i32, 0i32, 0i32, 0i32);
    // Mouse buttons we've reported down, released if the device vanishes
    // mid-press so nothing stays stuck in the focused app.
    let mut held: HashSet<u8> = HashSet::new();
    loop {
        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(_) => break, // unplugged / read error
        };
        for ev in events {
            let out = match ev.kind() {
                evdev::InputEventKind::Key(key) => match mouse_button_index(key) {
                    // value 1=down, 0=up (mice don't autorepeat).
                    Some(button) if ev.value() <= 1 => {
                        let pressed = ev.value() == 1;
                        if pressed {
                            held.insert(button);
                        } else {
                            held.remove(&button);
                        }
                        Some(UsbEvent::Pointer(InputEvent::PointerButton {
                            button,
                            pressed,
                        }))
                    }
                    Some(_) => None,
                    // Keyboard key: value 1=down, 0=up, 2=autorepeat. Forward
                    // down/up; let the host's xkb handle repeat.
                    None => match ev.value() {
                        1 => Some(UsbEvent::Key(ev.code(), true)),
                        0 => Some(UsbEvent::Key(ev.code(), false)),
                        _ => None,
                    },
                },
                evdev::InputEventKind::RelAxis(axis) => {
                    match axis {
                        evdev::RelativeAxisType::REL_X => dx += ev.value(),
                        evdev::RelativeAxisType::REL_Y => dy += ev.value(),
                        evdev::RelativeAxisType::REL_WHEEL => wheel_v += ev.value(),
                        evdev::RelativeAxisType::REL_HWHEEL => wheel_h += ev.value(),
                        _ => {}
                    }
                    None
                }
                evdev::InputEventKind::Synchronization(_) => {
                    let mut batch = Vec::new();
                    if dx != 0 || dy != 0 {
                        batch.push(InputEvent::PointerRel {
                            dx: dx as f32,
                            dy: dy as f32,
                        });
                    }
                    if wheel_v != 0 || wheel_h != 0 {
                        // evdev REL_WHEEL is +1 per notch AWAY from the user;
                        // the wire convention is +dy = scroll down.
                        batch.push(InputEvent::Scroll {
                            dx: wheel_h as f32,
                            dy: -wheel_v as f32,
                        });
                    }
                    (dx, dy, wheel_v, wheel_h) = (0, 0, 0, 0);
                    for e in batch {
                        if tx.send(UsbEvent::Pointer(e)).is_err() {
                            return; // render loop gone
                        }
                    }
                    None
                }
                _ => None,
            };
            if let Some(out) = out {
                if tx.send(out).is_err() {
                    return; // render loop gone
                }
            }
        }
    }
    for button in held {
        let _ = tx.send(UsbEvent::Pointer(InputEvent::PointerButton {
            button,
            pressed: false,
        }));
    }
}

/// The wire's button index (0=Left, 1=Right, 2=Middle, 3=Back, 4=Forward) for
/// an evdev mouse button, or `None` for any other key.
fn mouse_button_index(key: evdev::Key) -> Option<u8> {
    match key {
        evdev::Key::BTN_LEFT => Some(0),
        evdev::Key::BTN_RIGHT => Some(1),
        evdev::Key::BTN_MIDDLE => Some(2),
        evdev::Key::BTN_SIDE => Some(3),
        evdev::Key::BTN_EXTRA => Some(4),
        _ => None,
    }
}

/// Heuristic: a mouse reports relative X/Y motion and has a left button.
fn is_mouse(dev: &evdev::Device) -> bool {
    dev.supported_relative_axes().is_some_and(|a| {
        a.contains(evdev::RelativeAxisType::REL_X) && a.contains(evdev::RelativeAxisType::REL_Y)
    }) && dev
        .supported_keys()
        .is_some_and(|k| k.contains(evdev::Key::BTN_LEFT))
}

/// Heuristic: a real keyboard has the standard letter + Enter keys.
fn is_keyboard(dev: &evdev::Device) -> bool {
    dev.supported_keys()
        .is_some_and(|k| k.contains(evdev::Key::KEY_A) && k.contains(evdev::Key::KEY_ENTER))
}

/// Is this evdev node backed by a USB/IP (`vhci_hcd`) import? Resolve its sysfs
/// device path and look for `vhci_hcd` in the ancestry.
fn is_vhci(event_path: &Path) -> bool {
    let Some(name) = event_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let sys = Path::new("/sys/class/input").join(name);
    std::fs::canonicalize(&sys)
        .map(|p| p.to_string_lossy().contains("vhci_hcd"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_proto::InputEvent;

    /// Drives `reader_loop` with a real uinput mouse (the vhci check is the only
    /// part skipped). Needs write access to /dev/uinput, so it's opt-in:
    /// `cargo test -p compositor --lib usbkbd -- --ignored`.
    #[test]
    #[ignore = "needs /dev/uinput"]
    fn uinput_mouse_becomes_pointer_events() {
        use evdev::{uinput::VirtualDeviceBuilder, AttributeSet, EventType, RelativeAxisType};
        let mut keys = AttributeSet::<evdev::Key>::new();
        keys.insert(evdev::Key::BTN_LEFT);
        keys.insert(evdev::Key::BTN_RIGHT);
        let mut axes = AttributeSet::<RelativeAxisType>::new();
        for a in [
            RelativeAxisType::REL_X,
            RelativeAxisType::REL_Y,
            RelativeAxisType::REL_WHEEL,
        ] {
            axes.insert(a);
        }
        let mut vdev = VirtualDeviceBuilder::new()
            .unwrap()
            .name("pharos-test-mouse")
            .with_keys(&keys)
            .unwrap()
            .with_relative_axes(&axes)
            .unwrap()
            .build()
            .unwrap();
        // Find the node udev created for it.
        std::thread::sleep(Duration::from_millis(300));
        let path = vdev
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .flatten()
            .find(|p| p.to_string_lossy().contains("event"))
            .unwrap();
        let dev = evdev::Device::open(&path).unwrap();
        assert!(is_mouse(&dev) && !is_keyboard(&dev));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || reader_loop(dev, &tx));
        std::thread::sleep(Duration::from_millis(100));
        let rel = |a: RelativeAxisType, v| evdev::InputEvent::new(EventType::RELATIVE, a.0, v);
        let key = |k: evdev::Key, v| evdev::InputEvent::new(EventType::KEY, k.code(), v);
        vdev.emit(&[
            rel(RelativeAxisType::REL_X, 5),
            rel(RelativeAxisType::REL_Y, -3),
        ])
        .unwrap();
        vdev.emit(&[key(evdev::Key::BTN_LEFT, 1)]).unwrap();
        vdev.emit(&[rel(RelativeAxisType::REL_WHEEL, 1)]).unwrap();
        // Unplug mid-press: the held button must be released.
        drop(vdev);

        let got: Vec<UsbEvent> = rx.iter().take(4).collect();
        assert_eq!(
            got,
            vec![
                UsbEvent::Pointer(InputEvent::PointerRel { dx: 5.0, dy: -3.0 }),
                UsbEvent::Pointer(InputEvent::PointerButton {
                    button: 0,
                    pressed: true
                }),
                UsbEvent::Pointer(InputEvent::Scroll { dx: 0.0, dy: -1.0 }),
                UsbEvent::Pointer(InputEvent::PointerButton {
                    button: 0,
                    pressed: false
                }),
            ]
        );
    }
}
