// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Server registry — what the menu shows as tiles, what `trireme` remembers
//! across runs.
//!
//! Persisted at the platform's config dir (`~/.config/trireme/servers.toml`
//! on Linux, `%APPDATA%\trireme\servers.toml` on Windows,
//! `~/Library/Application Support/trireme/servers.toml` on macOS) via the
//! `dirs` crate. Each entry is a Pharos host the user has remembered.
//!
//! Friendly-name resolution (per the user's spec):
//!   custom alias > cached hostname > IP/host string
//!
//! `hostname_cache` is auto-populated best-effort by [`reverse_lookup`], a PTR
//! lookup via the `dns_lookup` crate, kicked off from the menu's
//! `kick_reverse_lookup`. The cache stays whatever it was if the lookup fails.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One remembered server. `id` is a stable handle used internally; the user
/// edits everything else through the menu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    /// Stable identifier (slug of host:port at first add; never changes).
    pub id: String,
    pub host: String,
    pub port: u16,
    /// User-set friendly name. When `Some(..)` and non-empty, beats every
    /// other naming source.
    #[serde(default)]
    pub alias: Option<String>,
    /// Last-resolved hostname for `host`. Caches a reverse DNS so the tile
    /// can keep showing `workstation.local` after the user goes offline.
    #[serde(default)]
    pub hostname_cache: Option<String>,
    /// Per-server pace (low-latency | balanced | smooth | vrr). Falls back
    /// to LowLatency at the menu level.
    #[serde(default)]
    pub pace: Option<String>,
    /// Pairing state. Set true after a successful SPAKE2 handshake; menu
    /// shows it as a badge.
    #[serde(default)]
    pub paired: bool,
    /// Hex-encoded 32-byte Ed25519 pubkey of the host, pinned at pairing
    /// time. Future video connections check this against the peer cert.
    #[serde(default)]
    pub host_pubkey: Option<String>,
    /// Port the host's pairing listener is on. Defaults to 4432; user can
    /// override when adding the server.
    #[serde(default)]
    pub pair_port: Option<u16>,
    /// Wall-clock timestamp of the last successful connect, ISO 8601.
    /// Purely informational — sorted by recency in the menu.
    #[serde(default)]
    pub last_connected: Option<String>,
    /// Host's NIC MAC for **Wake-on-LAN**, e.g. `AA:BB:CC:DD:EE:FF`. Opt-in and
    /// user-supplied — when set, Trireme can wake this host (`trireme wake`).
    /// `None` = WoL not offered for this server (no assumptions about the LAN).
    #[serde(default)]
    pub mac: Option<String>,
    /// Additional addresses for the *same* host (e.g. a second NIC), each `host`
    /// or `host:port`. The player rotates through `[host:port] + alt_addrs` on
    /// reconnect, so a host-NIC failure is transient. Opt-in; empty = single path.
    #[serde(default)]
    pub alt_addrs: Vec<String>,
    /// Source protocol: `None`/`"pharos"` = a Pharos QUIC/HEVC host (the
    /// default); `"vnc"` = a native VNC server; `"spice"` = a native SPICE
    /// server (the usual QEMU/Proxmox VM console). VNC/SPICE are shown in a
    /// split-screen cell and are interactive (pointer + keyboard). `"spice"`
    /// needs a Trireme build with the `spice` feature. Opt-in; no assumptions.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Auth password for `"vnc"`/`"spice"` servers that require one.
    #[serde(default)]
    pub password: Option<String>,
}

impl ServerEntry {
    pub fn new(host: String, port: u16, alias: Option<String>) -> Self {
        let id = make_id(&host, port);
        Self {
            id,
            host,
            port,
            alias,
            hostname_cache: None,
            pace: None,
            paired: false,
            host_pubkey: None,
            pair_port: None,
            last_connected: None,
            mac: None,
            alt_addrs: Vec::new(),
            protocol: None,
            password: None,
        }
    }

    /// Parse [`alt_addrs`](Self::alt_addrs) into `SocketAddr`s — each entry is
    /// `host:port`, or a bare `host` reusing this server's port. Bad entries are
    /// skipped (warned), never fatal.
    pub fn alt_socket_addrs(&self) -> Vec<SocketAddr> {
        use std::net::ToSocketAddrs;
        let mut out = Vec::new();
        for a in &self.alt_addrs {
            let candidate = if a.contains(':') {
                a.clone()
            } else {
                format!("{a}:{}", self.port)
            };
            match candidate.to_socket_addrs() {
                Ok(mut it) => {
                    if let Some(sa) = it.next() {
                        out.push(sa);
                    }
                }
                Err(e) => tracing::warn!("server {:?}: bad alt_addr {a:?}: {e}", self.host),
            }
        }
        out
    }

    /// Pairing port (defaults to 4432). Uses the registry's override when set.
    pub fn pair_port_or_default(&self) -> u16 {
        self.pair_port.unwrap_or(4432)
    }

    /// Friendly name per the resolution rules. Never empty.
    pub fn friendly_name(&self) -> String {
        if let Some(a) = self.alias.as_deref().filter(|s| !s.trim().is_empty()) {
            return a.to_string();
        }
        if let Some(h) = self.hostname_cache.as_deref().filter(|s| !s.is_empty()) {
            // Don't fall through to a hostname that's literally the same string
            // as the host — that's no information.
            if h != self.host {
                return h.to_string();
            }
        }
        self.host.clone()
    }

    /// `host:port` for the line under the friendly name.
    pub fn address_label(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Parse `host:port` into a `SocketAddr` if possible. We attempt a DNS
    /// lookup at use-time, but stash failures so the menu can surface them.
    pub fn socket_addr(&self) -> Result<SocketAddr> {
        use std::net::ToSocketAddrs;
        (self.host.as_str(), self.port)
            .to_socket_addrs()
            .with_context(|| format!("resolve {}:{}", self.host, self.port))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("no addresses for {}:{}", self.host, self.port))
    }
}

fn make_id(host: &str, port: u16) -> String {
    let mut id = format!("{}_{}", host, port);
    id.retain(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    id
}

/// The on-disk file shape. Wrapped so we can add top-level keys later
/// (sort preference, theme, etc.) without breaking older saves.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "server")]
    pub servers: Vec<ServerEntry>,
}

impl Registry {
    pub fn find(&self, id: &str) -> Option<&ServerEntry> {
        self.servers.iter().find(|s| s.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut ServerEntry> {
        self.servers.iter_mut().find(|s| s.id == id)
    }

    /// Add or replace by (host, port). Returns the new/updated entry's id.
    pub fn upsert(&mut self, host: String, port: u16, alias: Option<String>) -> String {
        let id = make_id(&host, port);
        if let Some(s) = self.find_mut(&id) {
            // Preserve cache + paired state; replace alias.
            s.alias = alias;
            return id;
        }
        let entry = ServerEntry::new(host, port, alias);
        let id = entry.id.clone();
        self.servers.push(entry);
        id
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let len = self.servers.len();
        self.servers.retain(|s| s.id != id);
        self.servers.len() != len
    }
}

/// Platform config directory: `~/.config/trireme/` (Linux),
/// `%APPDATA%\trireme\` (Windows), `~/Library/Application Support/trireme/`
/// (macOS), or `$TRIREME_CONFIG_DIR` if the user overrode it.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("TRIREME_CONFIG_DIR") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no platform config dir; set TRIREME_CONFIG_DIR"))?;
    Ok(base.join("trireme"))
}

pub fn servers_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("servers.toml"))
}

/// Where the long-term Trireme identity (Ed25519 keypair) lives.
pub fn identity_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("identity.toml"))
}

/// Load the registry from disk, or return an empty one if no file is found.
pub fn load() -> Result<Registry> {
    let path = servers_path()?;
    if !path.exists() {
        return Ok(Registry::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let registry: Registry =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(registry)
}

pub fn save(registry: &Registry) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = dir.join("servers.toml");
    let text = toml::to_string_pretty(registry).context("serialize registry")?;
    // Atomic write: tmp + rename so a crash mid-write doesn't truncate.
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename → {}", path.display()))?;
    Ok(())
}

/// Best-effort reverse DNS via libc's `getnameinfo`. Returns `Some(hostname)`
/// only when the lookup yields a result distinct from `host` (so a tile for
/// `192.168.1.100` will show `workstation.local` instead). Returns `None`
/// if `host` is already a name, the lookup fails, or it round-trips to the
/// same IP string.
pub fn reverse_lookup(host: &str) -> Option<String> {
    // Skip names — the cache only fills in the IP-only case.
    let ip: std::net::IpAddr = host.parse().ok()?;
    let resolved = dns_lookup::lookup_addr(&ip).ok()?;
    let resolved = resolved.trim();
    if resolved.is_empty() || resolved == host {
        None
    } else {
        Some(resolved.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(host: &str, port: u16, alias: Option<&str>, cache: Option<&str>) -> ServerEntry {
        let mut s = ServerEntry::new(host.to_string(), port, alias.map(String::from));
        s.hostname_cache = cache.map(String::from);
        s
    }

    #[test]
    fn friendly_name_prefers_alias() {
        let s = entry("192.168.1.10", 4433, Some("Workstation"), Some("ws.local"));
        assert_eq!(s.friendly_name(), "Workstation");
    }

    #[test]
    fn friendly_name_falls_back_to_hostname_cache() {
        let s = entry("192.168.1.10", 4433, None, Some("ws.local"));
        assert_eq!(s.friendly_name(), "ws.local");
    }

    #[test]
    fn friendly_name_falls_back_to_host() {
        let s = entry("192.168.1.10", 4433, None, None);
        assert_eq!(s.friendly_name(), "192.168.1.10");
    }

    #[test]
    fn friendly_name_ignores_blank_alias() {
        let s = entry("192.168.1.10", 4433, Some("   "), Some("ws.local"));
        assert_eq!(s.friendly_name(), "ws.local");
    }

    #[test]
    fn id_is_stable_for_host_port() {
        let a = ServerEntry::new("192.168.1.10".into(), 4433, None);
        let b = ServerEntry::new("192.168.1.10".into(), 4433, Some("alias".into()));
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn registry_upsert_preserves_state() {
        let mut r = Registry::default();
        let id = r.upsert("192.168.1.10".into(), 4433, Some("first".into()));
        r.find_mut(&id).unwrap().paired = true;
        r.upsert("192.168.1.10".into(), 4433, Some("second".into()));
        let updated = r.find(&id).unwrap();
        assert_eq!(updated.alias.as_deref(), Some("second"));
        assert!(updated.paired, "paired flag should survive upsert");
        assert_eq!(r.servers.len(), 1);
    }
}
