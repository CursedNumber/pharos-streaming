// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Pharos pair listener.
//!
//! `pharos-compositor pair [addr]` brings up a QUIC endpoint, loads (or
//! generates) the host's long-term Ed25519 identity, prints a six-digit PIN,
//! and waits for a Trireme to connect. On a successful handshake the peer's
//! pubkey is appended to `peers.toml` and the listener stays up to accept
//! more pairings (use Ctrl-C to exit). The crypto + state machine live in
//! `core-crypto`; the wire shape lives in `transport::pair_proto`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use core_crypto::{Identity, PairingError, PairingRole, PairingSession, PAIRING_PROTOCOL_VERSION};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use transport::pair_proto::PairMsg;

/// mDNS service type the Trireme menu browses for. Underscore-prefixed
/// service-name then transport per RFC 6763. We're UDP because QUIC is UDP,
/// and pairing's own bidi rides on the same UDP socket.
pub const MDNS_SERVICE_TYPE: &str = "_pharos._udp.local.";

/// File the host writes after each successful pairing. Mirrors Trireme's
/// `servers.toml` — there it's "which hosts do I know?", here it's "which
/// clients may connect to me?".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerRegistry {
    #[serde(default, rename = "peer")]
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// 32-byte Ed25519 pubkey, hex-encoded for human inspection.
    pub pubkey: String,
    /// Friendly name supplied by the client during pairing.
    #[serde(default)]
    pub alias: Option<String>,
    /// When this peer first paired with us (ISO-8601 hint, optional).
    #[serde(default)]
    pub paired_at: Option<String>,
}

impl PeerRegistry {
    fn upsert(&mut self, pubkey_hex: String, alias: Option<String>) {
        if let Some(p) = self.peers.iter_mut().find(|p| p.pubkey == pubkey_hex) {
            p.alias = alias;
            return;
        }
        self.peers.push(Peer {
            pubkey: pubkey_hex,
            alias,
            paired_at: None,
        });
    }
}

/// `~/.config/pharos/` on Linux / `%APPDATA%\pharos\` on Windows, or
/// `$PHAROS_CONFIG_DIR` if the user wants to override.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("PHAROS_CONFIG_DIR") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no platform config dir; set PHAROS_CONFIG_DIR"))?;
    Ok(base.join("pharos"))
}

pub fn identity_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("identity.toml"))
}

pub fn peers_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("peers.toml"))
}

fn load_peers(path: &Path) -> Result<PeerRegistry> {
    if !path.exists() {
        return Ok(PeerRegistry::default());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn save_peers(path: &Path, reg: &PeerRegistry) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let text = toml::to_string_pretty(reg).context("serialize peers")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// A fresh random six-digit pairing PIN. Public so the compositor's
/// `PairingManager` can mint the first PIN synchronously (and surface it to the
/// panel) before the listener thread starts rotating it.
pub fn generate_pin() -> String {
    let mut rng = rand::thread_rng();
    let n: u32 = rng.gen_range(0..1_000_000);
    format!("{n:06}")
}

/// An event the pair listener emits as it runs, so an embedder (the compositor's
/// `PairingManager`) can surface the live PIN + attempt outcomes to the panel.
/// The CLI [`run`] maps these straight back to the stdout banner + `tracing`.
#[derive(Debug, Clone)]
pub enum PairEvent {
    /// The listener is up (or rotated its PIN after a handshake) — `pin` is the
    /// code to enter on the Trireme device.
    Listening { addr: SocketAddr, pin: String },
    /// A device connected and the SPAKE2 handshake is starting.
    Attempt { peer: SocketAddr },
    /// A device finished pairing and was written to `peers.toml`.
    Paired {
        pubkey_hex: String,
        alias: Option<String>,
    },
    /// A pairing attempt failed (wrong PIN, protocol mismatch, hang-up, …).
    Failed { reason: String },
    /// The listener loop exited (cancelled or the endpoint died).
    Stopped,
}

/// A [`transport::ClientAuthorizer`] that admits only clients whose verified
/// pubkey is listed in `peers.toml`. Re-reads the file on each connection so a
/// freshly-paired device is admitted without restarting the host (hot-reload).
/// Anonymous or unknown clients are rejected.
pub fn peers_authorizer() -> transport::ClientAuthorizer {
    match peers_path() {
        Ok(p) => peers_authorizer_at(p),
        Err(_) => std::sync::Arc::new(|_| false),
    }
}

/// Like [`peers_authorizer`] but against an explicit `peers.toml` path — used
/// by tests, which can't rely on the platform config dir.
pub fn peers_authorizer_at(path: std::path::PathBuf) -> transport::ClientAuthorizer {
    std::sync::Arc::new(move |client: Option<[u8; 32]>| {
        let Some(pk) = client else {
            return false; // anonymous — no identity to match
        };
        let hexk = hex::encode(pk);
        match load_peers(&path) {
            Ok(reg) => reg
                .peers
                .iter()
                .any(|p| p.pubkey.eq_ignore_ascii_case(&hexk)),
            Err(e) => {
                warn!("peers.toml unreadable ({e:?}); rejecting client");
                false
            }
        }
    })
}

/// The authorizer the host binaries install. **Enforces `peers.toml` by
/// default** — only clients that paired (and thus present a known identity) may
/// stream. Set `PHAROS_ALLOW_UNPAIRED=1` to disable enforcement (dev/testing,
/// or unpaired headless clients like `trireme-kms`).
///
/// Bootstrap: pairing happens on the separate pair listener (`pharos-compositor
/// pair`), which writes `peers.toml` and is *not* gated by this — so the first
/// pair, then connect, works. A host that has never been paired with rejects
/// every client until you pair (or opt out).
pub fn client_authorizer() -> transport::ClientAuthorizer {
    if std::env::var_os("PHAROS_ALLOW_UNPAIRED").is_some() {
        warn!("PHAROS_ALLOW_UNPAIRED set — accepting ANY client (pairing NOT enforced)");
        transport::allow_any_client()
    } else {
        info!("enforcing pairing: only clients in peers.toml may stream (PHAROS_ALLOW_UNPAIRED=1 to disable)");
        peers_authorizer()
    }
}

/// Run the pair listener for the CLI (`pharos-compositor pair`). Blocks forever
/// (one Ctrl-C exits the process); maps each [`PairEvent`] back to the stdout
/// PIN banner + `tracing`. Thin wrapper over [`run_listener`].
pub fn run(addr: SocketAddr) -> Result<()> {
    let id_path = identity_path()?;
    let identity = Identity::load_or_create(&id_path)
        .with_context(|| format!("identity at {}", id_path.display()))?;
    info!(
        "Pharos identity: {} (saved at {})",
        identity.public_hex(),
        id_path.display()
    );
    // The CLI listener never cancels itself — the always-false flag matches the
    // prior run-until-killed behaviour. `stop_after_pair = false`: the CLI keeps
    // accepting devices (rotating the PIN) until killed, unlike the GUI.
    let never = Arc::new(AtomicBool::new(false));
    run_listener(
        addr,
        identity,
        generate_pin(),
        never,
        false,
        // CLI: no window cap — `pharos-compositor pair` runs until Ctrl-C.
        None,
        |ev| match ev {
            PairEvent::Listening { pin, .. } => announce_pin(&pin),
            PairEvent::Attempt { peer } => info!("incoming pair attempt from {peer}"),
            PairEvent::Paired { pubkey_hex, alias } => {
                info!("paired with {pubkey_hex} ({alias:?})")
            }
            PairEvent::Failed { reason } => warn!("pairing failed: {reason}"),
            PairEvent::Stopped => info!("pair listener stopped"),
        },
    )
}

/// Run the pair listener until `cancel` is set, invoking `on_event` for the
/// initial PIN, each PIN rotation, and every attempt outcome. Builds its own
/// current-thread tokio runtime and blocks the calling thread; the compositor's
/// `PairingManager` runs this on a dedicated thread and flips `cancel` to stop.
///
/// `initial_pin` is the first round's code — minted by the caller so it can be
/// surfaced to the panel synchronously before this thread is even scheduled.
pub fn run_listener(
    addr: SocketAddr,
    identity: Identity,
    initial_pin: String,
    cancel: Arc<AtomicBool>,
    stop_after_pair: bool,
    max_window: Option<Duration>,
    on_event: impl Fn(PairEvent),
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_loop(
        addr,
        identity,
        initial_pin,
        cancel,
        stop_after_pair,
        max_window,
        on_event,
    ))
}

async fn run_loop(
    addr: SocketAddr,
    identity: Identity,
    initial_pin: String,
    cancel: Arc<AtomicBool>,
    stop_after_pair: bool,
    max_window: Option<Duration>,
    on_event: impl Fn(PairEvent),
) -> Result<()> {
    // Bind error (e.g. port already in use) surfaces here so the manager can
    // mark the round failed; nothing is emitted before this point. A fast
    // cancel→restart can briefly race the previous round's detached thread,
    // which hasn't released the UDP socket yet (quinn binds without
    // SO_REUSEADDR) — so retry on EADDRINUSE for up to ~1.5s before giving up.
    let endpoint = {
        let mut attempt = 0u32;
        loop {
            match transport::pair_listen(addr) {
                Ok(ep) => break ep,
                Err(e) => {
                    let busy = e.to_string().to_lowercase().contains("in use");
                    attempt += 1;
                    if busy && attempt <= 10 {
                        warn!("pair listen {addr} busy (attempt {attempt}); retrying…");
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    };
    info!("pairing listener up on {addr}");

    // Announce ourselves over mDNS so Trireme's menu picks us up as a tile
    // without the user having to type host/port. _Held_ for the lifetime of
    // the listener; dropping it withdraws the broadcast.
    let _mdns = match announce_mdns(addr, &identity) {
        Ok(d) => Some(d),
        Err(e) => {
            warn!("mDNS announce failed: {e:?} (LAN discovery off; pair manually)");
            None
        }
    };

    let peers_path = peers_path()?;
    // One PIN per "round" — valid for the next client that connects, rotated
    // after each handshake (success or fail) so a guess can't be replayed.
    let mut pin = initial_pin;
    on_event(PairEvent::Listening {
        addr,
        pin: pin.clone(),
    });
    // Optional upper bound on how long the window stays open without a pair.
    // The GUI/panel arms a generous cap (an idle window is auto-closed so it
    // can't linger listening forever); the CLI passes `None` to run until
    // killed. A successful pair in one-shot mode breaks below regardless, so
    // this only fires when nobody pairs in time. Checked once per loop pass —
    // the 200ms idle wake honours it within ~200ms while idle.
    let deadline = max_window.map(|d| tokio::time::Instant::now() + d);
    while !cancel.load(Ordering::Relaxed) {
        if let Some(dl) = deadline {
            if tokio::time::Instant::now() >= dl {
                info!(
                    "pairing window ({}s) elapsed without a pair; closing listener",
                    max_window.map(|d| d.as_secs()).unwrap_or(0)
                );
                break;
            }
        }
        // Accept the next pairing attempt, but wake every 200ms so a `cancel`
        // set while we're idle is honoured promptly. `biased` polls the accept
        // first, so a connection that arrives is never starved by the timer; an
        // unresolved accept stays buffered in the endpoint for the next pass.
        let mut link = tokio::select! {
            biased;
            res = transport::pair_accept(&endpoint) => match res {
                Ok(l) => l,
                Err(e) => {
                    warn!("accept failed: {e:?}");
                    // Avoid a busy-spin if the endpoint is hard-erroring.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
        };
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let peer_addr = link.remote();
        info!("incoming pair attempt from {peer_addr}");
        on_event(PairEvent::Attempt { peer: peer_addr });
        // Bound the handshake: a SPAKE2 exchange is four small round-trips
        // (sub-second on a LAN), so a peer that connects then stalls must not
        // pin this thread for the full QUIC idle timeout (~20s) — which would
        // also stall a cancel(), since `cancel` isn't polled inside handle_one.
        let handshake = tokio::time::timeout(
            Duration::from_secs(10),
            handle_one(&mut link, &identity, &pin),
        )
        .await;
        let mut paired_ok = false;
        match handshake {
            Ok(Ok((peer_pub_hex, alias))) => {
                info!("paired with {peer_pub_hex} ({alias:?})");
                let mut reg = load_peers(&peers_path).unwrap_or_default();
                reg.upsert(peer_pub_hex.clone(), alias.clone());
                if let Err(e) = save_peers(&peers_path, &reg) {
                    warn!("save peers: {e:?}");
                } else {
                    info!("peers persisted to {}", peers_path.display());
                }
                on_event(PairEvent::Paired {
                    pubkey_hex: peer_pub_hex,
                    alias,
                });
                paired_ok = true;
            }
            Ok(Err(e)) => {
                warn!("pairing failed: {e:?}");
                // Best-effort abort message; ignore I/O errors.
                let _ = link
                    .send_msg(&PairMsg::Abort {
                        reason: format!("{e}"),
                    })
                    .await;
                on_event(PairEvent::Failed {
                    reason: format!("{e}"),
                });
            }
            Err(_elapsed) => {
                warn!("pairing handshake from {peer_addr} timed out");
                let _ = link
                    .send_msg(&PairMsg::Abort {
                        reason: "handshake timed out".into(),
                    })
                    .await;
                on_event(PairEvent::Failed {
                    reason: "handshake timed out".into(),
                });
            }
        }
        // Tidy close; ignore errors — peer may have hung up already.
        let _ = tokio::time::timeout(Duration::from_secs(1), link.close()).await;
        // One-shot mode (the GUI pairs one device at a time): stop after a
        // successful pairing rather than rotating the PIN for the next device.
        // The CLI passes `stop_after_pair = false` and keeps accepting devices.
        if paired_ok && stop_after_pair {
            break;
        }
        // Rotate the PIN so an attacker can't replay a guess.
        pin = generate_pin();
        on_event(PairEvent::Listening {
            addr,
            pin: pin.clone(),
        });
    }
    on_event(PairEvent::Stopped);
    Ok(())
}

fn announce_pin(pin: &str) {
    println!();
    println!("═══════════════════════════════════════════");
    println!("  Pairing PIN: \x1b[1m{pin}\x1b[0m");
    println!("  Enter on Trireme's Pair dialog.");
    println!("═══════════════════════════════════════════");
    println!();
    // Also log so the systemd journal captures it (stdout is enough for a
    // terminal user, but tests scrape `journalctl`).
    info!("pairing PIN: {pin}");
}

async fn handle_one(
    link: &mut transport::PairLink,
    identity: &Identity,
    pin: &str,
) -> Result<(String, Option<String>)> {
    // Step 1: receive HelloPair.
    let hello = link.recv_msg().await?;
    let (client_pub, alias) = match hello {
        PairMsg::HelloPair {
            version,
            client_pubkey,
            alias,
        } => {
            anyhow::ensure!(
                version == PAIRING_PROTOCOL_VERSION,
                "protocol version mismatch: client={version} expected={PAIRING_PROTOCOL_VERSION}"
            );
            (client_pubkey, alias)
        }
        other => anyhow::bail!("expected HelloPair, got {other:?}"),
    };

    // Step 2: start our SPAKE2, send HelloAck.
    let mut session =
        PairingSession::new(PairingRole::Host, pin.to_string(), identity.public_bytes());
    session.set_peer_pubkey(client_pub);
    let spake2_msg_b = session.start().map_err(map_pair_err)?;
    let host_alias = hostname_alias();
    link.send_msg(&PairMsg::HelloAck {
        host_pubkey: identity.public_bytes(),
        spake2_msg_b,
        host_alias,
    })
    .await?;

    // Step 3: receive Spake2Reply.
    let reply = link.recv_msg().await?;
    let spake2_msg_a = match reply {
        PairMsg::Spake2Reply { spake2_msg_a } => spake2_msg_a,
        PairMsg::Abort { reason } => anyhow::bail!("client aborted: {reason}"),
        other => anyhow::bail!("expected Spake2Reply, got {other:?}"),
    };
    session.finish(&spake2_msg_a).map_err(map_pair_err)?;

    // Step 4: receive ClientConfirm, verify, send HostConfirm.
    let confirm = link.recv_msg().await?;
    let client_mac = match confirm {
        PairMsg::ClientConfirm { mac } => mac,
        PairMsg::Abort { reason } => anyhow::bail!("client aborted: {reason}"),
        other => anyhow::bail!("expected ClientConfirm, got {other:?}"),
    };
    session.verify_peer_mac(&client_mac).map_err(map_pair_err)?;
    let host_mac = session.our_mac().map_err(map_pair_err)?;
    link.send_msg(&PairMsg::HostConfirm { mac: host_mac })
        .await?;

    Ok((hex::encode(client_pub), alias))
}

fn map_pair_err(e: PairingError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// Best-effort hostname for the HelloAck's `host_alias` field. Falls back
/// to `None` if the platform doesn't surface one easily.
fn hostname_alias() -> Option<String> {
    let h = std::env::var("HOSTNAME").ok()?;
    if h.is_empty() {
        None
    } else {
        Some(h)
    }
}

/// Hand-rolled `gethostname(3)` via libc on Unix, env on Windows. Used as
/// the mDNS `host_name` so resolvers see something stable, and as a
/// fallback alias if `HOSTNAME` isn't set. Returns `"pharos"` if every
/// strategy fails (mDNS rejects an empty host name).
fn hostname_or_default() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes up to len-1 bytes + NUL into our buffer.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let cstr_len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..cstr_len]) {
                let s = s.trim();
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    if let Some(h) = hostname_alias() {
        return h;
    }
    "pharos".into()
}

/// Build + register the mDNS announcement. Returns the `ServiceDaemon` so
/// the caller can keep it alive (drop = withdraw).
fn announce_mdns(addr: SocketAddr, identity: &Identity) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("ServiceDaemon::new")?;

    // mDNS hostnames must end with `.local.`. Sanitize: replace dots in the
    // local hostname (mDNS labels can't contain '.') and append the TLD.
    let raw_host = hostname_or_default();
    let mdns_host = format!("{}.local.", sanitize_label(&raw_host));

    // Service-instance name (the "friendly" name as displayed in clients).
    // Keep it readable; the type+domain identifies us as a Pharos host.
    let instance = format!("{} (pharos)", raw_host);

    // Resolve which IPs to publish. The library accepts an empty slice and
    // picks the bind address itself, but on a wildcard bind (0.0.0.0) we
    // need to enumerate non-loopback interfaces or clients see nothing
    // useful. Strategy: prefer the bind IP if it's specific; otherwise let
    // mdns-sd auto-discover by passing an empty list.
    let ips: Vec<std::net::IpAddr> = if addr.ip().is_unspecified() {
        Vec::new()
    } else {
        vec![addr.ip()]
    };

    // TXT records — keep them small (each ≤ 255 bytes per RFC 6763). The
    // pubkey is 64 hex chars + key length, still under the cap.
    let mut props: Vec<(String, String)> = vec![
        ("proto".into(), PAIRING_PROTOCOL_VERSION.to_string()),
        ("pubkey".into(), identity.public_hex()),
        // The service is advertised on the *pairing* port (this `addr`), but a
        // client streams on a different port. Publish it so discovery is fully
        // self-configuring — the client learns IP:stream-port + pubkey + name
        // and never has to be told the stream port. Clients fall back to the
        // well-known default if this record is absent (older hosts).
        ("stream".into(), core_proto::ports::STREAM.to_string()),
    ];
    if let Some(alias) = hostname_alias() {
        props.push(("alias".into(), alias));
    }

    let svc = ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        &instance,
        &mdns_host,
        ips.as_slice(),
        addr.port(),
        &props[..],
    )
    .context("ServiceInfo::new")?
    // Without auto-address, mdns-sd respects exactly the IPs we pass —
    // which is what we want when the operator passed a specific bind IP.
    // For the unspecified-bind case, enable auto-discovery so the daemon
    // figures out the interface addresses.
    .enable_addr_auto();

    daemon.register(svc).context("ServiceDaemon::register")?;
    info!(
        "mDNS announcing {MDNS_SERVICE_TYPE} on port {} as '{}'",
        addr.port(),
        instance
    );
    Ok(daemon)
}

/// Strip characters mDNS hostnames can't carry (dots especially) and
/// truncate to a reasonable length. Empty input becomes `"pharos"`.
fn sanitize_label(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "pharos".into()
    } else {
        cleaned.chars().take(63).collect()
    }
}
