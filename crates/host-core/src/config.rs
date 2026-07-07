// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Optional host config (`config.toml` in the Pharos config dir).
//!
//! Everything here is **opt-in**: a missing or partial file yields safe defaults
//! with every feature **off**, so the host has zero extra overhead (no MQTT
//! thread, no broker connection) unless the operator explicitly enables it. No
//! assumptions are made about the user's network — the broker address must be
//! filled in by them.

use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

/// Top-level host config. Add new opt-in feature sections here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub encode: EncodeConfig,
}

/// Encode-envelope limits. Today just the **pixel-throughput budget**: the
/// compositor's layout editor keeps `Σ(output_w · output_h) · fps` at or under
/// this ceiling, so you can't lay out more pixels-per-second than the encoder
/// can sustain.
///
/// The default is a **placeholder**, not a measured cap: `497_664_000` =
/// 3840 × 2160 × 60 (one 4K UHD stream at 60 Hz). It is deliberately arbitrary —
/// the intent is to derive the real ceiling from the GPU's NVENC capability
/// later (a hardware query), at which point this default goes away. Until then
/// it gives the panel a single, configurable number to budget against.
#[derive(Debug, Clone, Deserialize)]
pub struct EncodeConfig {
    /// Maximum sustained encoded pixels per second across all outputs.
    #[serde(default = "default_max_pixels_per_sec")]
    pub max_pixels_per_sec: u64,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            max_pixels_per_sec: default_max_pixels_per_sec(),
        }
    }
}

/// 3840 × 2160 × 60 — one 4K UHD stream at 60 Hz. Placeholder until we query the
/// GPU's real NVENC throughput (see [`EncodeConfig`]).
fn default_max_pixels_per_sec() -> u64 {
    497_664_000
}

/// Network binding overrides. All optional — default behaviour is unchanged
/// (bind exactly the address passed on the CLI, usually the wildcard).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetworkConfig {
    /// Pin the QUIC listener to the NIC with this MAC (`AA:BB:CC:DD:EE:FF`).
    /// Useful on multi-homed hosts where the wildcard bind picks the wrong path
    /// (the dual-NIC `.201` problem). The MAC is resolved to that interface's
    /// IPv4 at startup; if it can't be resolved, the host falls back to the
    /// CLI bind and warns. `None` = no MAC pinning. No LAN assumptions.
    #[serde(default)]
    pub bind_mac: Option<String>,

    /// Bind the QUIC listener **dual-stack** (`[::]` with `IPV6_V6ONLY` off) so it
    /// accepts BOTH IPv4 and IPv6 clients on one socket. Default `false` = the
    /// existing IPv4 wildcard bind (unchanged). Turn on to reach the host over IPv6
    /// (e.g. an IPv6-only network, or to sidestep NAT). Ignored when `bind_mac`
    /// pins a specific IPv4 NIC (that pin wins). Clients dial whichever family the
    /// host advertises; the Trireme client is already dual-stack-capable.
    #[serde(default)]
    pub ipv6: bool,
}

/// Home Assistant / MQTT integration — **disabled by default**. Many users won't
/// want it (and may prefer zero extra network chatter), so nothing connects
/// unless `enabled = true`.
#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    /// Master switch. Off by default — the host publishes nothing and starts no
    /// MQTT thread unless this is true.
    #[serde(default)]
    pub enabled: bool,
    /// Broker address. No default that assumes the user's LAN — they set it.
    #[serde(default = "localhost")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Stable node id (unique_id base + device identifier). Defaults to hostname.
    #[serde(default)]
    pub node_id: Option<String>,
    /// HA discovery prefix; HA's default is `homeassistant`.
    #[serde(default = "discovery_prefix")]
    pub discovery_prefix: String,
    /// Telemetry publish cadence (seconds).
    #[serde(default = "default_interval")]
    pub publish_interval_secs: u64,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: localhost(),
            port: default_port(),
            username: None,
            password: None,
            node_id: None,
            discovery_prefix: discovery_prefix(),
            publish_interval_secs: default_interval(),
        }
    }
}

fn localhost() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    1883
}
fn discovery_prefix() -> String {
    "homeassistant".into()
}
fn default_interval() -> u64 {
    10
}

pub fn config_path() -> Result<PathBuf> {
    Ok(crate::pair::config_dir()?.join("config.toml"))
}

/// Load the host config. A missing file or any parse error is **non-fatal** and
/// returns defaults (all features off) — config problems must never stop the
/// host from streaming. Parse errors are logged.
pub fn load() -> HostConfig {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return HostConfig::default(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return HostConfig::default(), // no file → defaults (off)
    };
    match toml::from_str::<HostConfig>(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "config.toml at {} unparseable: {e}; using defaults",
                path.display()
            );
            HostConfig::default()
        }
    }
}
