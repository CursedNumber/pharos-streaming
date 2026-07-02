//! mDNS browser — listens for Pharos hosts advertising `_pharos._udp.local.`
//! and forwards their details to the menu via an mpsc channel.
//!
//! The menu owns the `mpsc::Receiver` and drains it each frame, building a
//! list of "discovered" hosts that aren't yet in the persistent registry.
//! Tiles render those alongside saved servers; the first action on a
//! discovered tile (Connect or Pair) promotes it into the registry.
//!
//! We deliberately keep this layer dumb: it reports new resolutions and
//! disappearances, but doesn't keep state of its own. The menu decides what
//! to show and when to drop entries.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::sync::atomic::{AtomicBool, Ordering};

const SERVICE_TYPE: &str = "_pharos._udp.local.";

/// One Pharos host that just announced itself on the LAN.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// Stable id (service-instance name) — survives transient drops.
    pub instance: String,
    pub host: String,
    /// The advertised port — the host's *pairing* listener (the service is
    /// announced there). The stream port is separate; see [`stream_port`].
    pub port: u16,
    /// Port to stream to, from the `stream` TXT record. `None` on older hosts
    /// that don't publish it — callers fall back to the well-known default.
    pub stream_port: Option<u16>,
    /// `pubkey` TXT record (hex). `None` if the host didn't publish it,
    /// which means we'll have to pair before pinning.
    pub pubkey_hex: Option<String>,
    /// Optional friendly alias from the TXT records.
    pub alias: Option<String>,
}

/// Events the menu cares about. Only resolution and removal — the rest of
/// the mdns-sd event stream is too noisy for a UI.
#[derive(Debug, Clone)]
pub enum DiscoverEvent {
    Found(Discovered),
    Removed { instance: String },
}

/// Handle to a running browser. Drop it to cancel; the daemon thread reaps
/// itself on the next event.
pub struct Browser {
    pub rx: mpsc::Receiver<DiscoverEvent>,
    stop: Arc<AtomicBool>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Spawn the mDNS browser thread. Returns `Ok(None)` (logged) if the
/// daemon can't start so the menu degrades gracefully — discovery is
/// optional, manual Add still works.
pub fn start() -> Option<Browser> {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("mDNS daemon unavailable ({e}); discovery off");
            return None;
        }
    };
    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("mDNS browse failed ({e}); discovery off");
            return None;
        }
    };
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    std::thread::Builder::new()
        .name("trireme-mdns".into())
        .spawn(move || {
            tracing::info!("mDNS browser listening on {SERVICE_TYPE}");
            // try_recv + brief sleep so we can observe the stop flag without
            // taking a direct flume dep just to name `RecvTimeoutError`.
            loop {
                if stop_thread.load(Ordering::Acquire) {
                    break;
                }
                let event = match receiver.try_recv() {
                    Ok(e) => e,
                    Err(_) => {
                        if receiver.is_disconnected() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                };
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let instance = info.get_fullname().to_string();
                        let port = info.get_port();
                        // Prefer the first IPv4 we see; the test bench is
                        // dual-stack and IPv4 is friendlier for QUIC bind.
                        let host = info
                            .get_addresses()
                            .iter()
                            .find_map(|a| a.is_ipv4().then_some(a.to_string()))
                            .or_else(|| info.get_addresses().iter().next().map(|a| a.to_string()))
                            .unwrap_or_default();
                        let mut pubkey_hex = None;
                        let mut alias = None;
                        let mut stream_port = None;
                        for prop in info.get_properties().iter() {
                            let v = prop.val_str();
                            match prop.key() {
                                "pubkey" if !v.is_empty() => pubkey_hex = Some(v.to_string()),
                                "alias" if !v.is_empty() => alias = Some(v.to_string()),
                                "stream" => stream_port = v.parse::<u16>().ok(),
                                _ => {}
                            }
                        }
                        let _ = tx.send(DiscoverEvent::Found(Discovered {
                            instance,
                            host,
                            port,
                            stream_port,
                            pubkey_hex,
                            alias,
                        }));
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        let _ = tx.send(DiscoverEvent::Removed { instance: fullname });
                    }
                    _ => { /* SearchStarted / ServiceFound — not interesting */ }
                }
            }
            let _ = daemon.shutdown();
            tracing::info!("mDNS browser exiting");
        })
        .ok()?;
    Some(Browser { rx, stop })
}
