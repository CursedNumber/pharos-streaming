//! USB/IP keyboard relay.
//!
//! A keyboard exported from the client over USB/IP is attached to the host
//! kernel via `vhci_hcd` and shows up as a host evdev device — which the
//! headless compositor wouldn't otherwise see (it has no libinput backend).
//!
//! We read **only** the vhci-imported keyboards: the host's *own* keyboard
//! (which drives the user's desktop) sits under a real USB/PCI controller, not
//! `vhci_hcd`, so it's never touched. The imported ones we **grab**, so their
//! keys drive the stream (the focused app, e.g. the media player) rather than
//! double-typing into the host desktop. Key transitions go to the render loop
//! over a channel, which injects them into the Wayland seat.
//!
//! Best-effort throughout: missing permissions / no devices simply yield no
//! events; a relay hiccup never disturbs the stream.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A key transition: (Linux evdev code, pressed).
pub type Key = (u16, bool);

/// Handle the render loop drains. Spawn once; `drain` each frame.
pub struct UsbKbd {
    rx: Receiver<Key>,
}

impl UsbKbd {
    /// Spawn the manager thread (rescans `/dev/input` for vhci keyboards) plus a
    /// reader thread per grabbed device. Never blocks; never panics.
    pub fn spawn() -> UsbKbd {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("pharos-usbkbd".into())
            .spawn(move || manager_loop(tx));
        UsbKbd { rx }
    }

    /// Take every key transition seen since the last call (in arrival order).
    pub fn drain(&self) -> Vec<Key> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

fn manager_loop(tx: Sender<Key>) {
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
                    Ok(d) if is_keyboard(&d) => d,
                    _ => continue,
                };
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
                tracing::info!("USB/IP keyboard relay reading {}", path.display());
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn reader_loop(mut dev: evdev::Device, tx: &Sender<Key>) {
    // Grab so the keys drive the stream, not the host desktop. It's a
    // vhci-imported device, so this never affects the local keyboard.
    let _ = dev.grab();
    loop {
        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(_) => break, // unplugged / read error
        };
        for ev in events {
            if ev.event_type() == evdev::EventType::KEY {
                // value 1=down, 0=up, 2=autorepeat. Forward down/up; let the
                // host's xkb handle repeat.
                let sent = match ev.value() {
                    1 => tx.send((ev.code(), true)),
                    0 => tx.send((ev.code(), false)),
                    _ => Ok(()),
                };
                if sent.is_err() {
                    return; // render loop gone
                }
            }
        }
    }
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
