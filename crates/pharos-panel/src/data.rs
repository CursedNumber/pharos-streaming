// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Disk + probe access for the panel. Every loader is **non-panicking**: a
//! missing/unparseable file yields safe defaults, and savers do an atomic
//! tmp-write + rename. The on-disk shapes are mirrored here with serde structs
//! (rather than reusing `host-core`, whose `HostConfig` is `Deserialize`-only),
//! so the panel fully owns its read *and* write formats.
//!
//! File layout under [`config_dir`] (default `~/.config/pharos`):
//!   * `peers.toml`               — paired clients (shared with the host).
//!   * `config.toml`              — `[mqtt]` + `[network]` (shared with the host).
//!   * `controller-profiles.toml` — per-controller feel (shared with the host).
//!   * `multi.toml`               — compositor virtual-display layout.
//!   * `panel.toml`               — panel-only runtime tuning + input mirror.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use core_proto::ControllerProfile;
use media_decode::Kind;

use crate::model::{
    CapabilityUi, CompositorLayout, ControllerProfilesUi, HostConfigUi, HostInfo, InputCfgUi,
    OutputTile, PeerInfo, PipelineStage, RuntimeSettings,
};

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `$PHAROS_CONFIG_DIR`, else `<platform config>/pharos`. Matches the host's
/// `host-core::pair::config_dir` so the panel reads/writes the same files.
pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PHAROS_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    match dirs::config_dir() {
        Some(base) => base.join("pharos"),
        // Last-ditch fallback so we never panic on an exotic platform.
        None => PathBuf::from(".pharos"),
    }
}

fn path(name: &str) -> PathBuf {
    config_dir().join(name)
}

/// Atomic write: serialize to `<path>.tmp`, then rename over the target. When
/// `mode` is set (Unix only), the temp file is chmod'd *before* the rename, so
/// the final file never exists with looser permissions even momentarily — used
/// for `config.toml`, which can hold the plaintext MQTT password at rest.
fn atomic_write(target: &std::path::Path, text: &str, mode: Option<u32>) -> Result<()> {
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = target.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))
            .with_context(|| format!("chmod {}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, target).with_context(|| format!("rename into {}", target.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Host identity
// ---------------------------------------------------------------------------

/// Hostname + advertised NIC MACs via the `host-info` leaf crate (cross-platform).
pub fn host_info() -> HostInfo {
    HostInfo {
        hostname: host_info::hostname(),
        macs: host_info::advertised_macs(),
    }
}

// ---------------------------------------------------------------------------
// Peers  (peers.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct PeerRegistryToml {
    /// Hex pubkey of the host's PRIMARY client (issue #19), if one is set.
    /// Written by the host (`SetPrimaryPeer`); the panel only reads it.
    #[serde(default)]
    primary_pubkey: Option<String>,
    #[serde(default, rename = "peer")]
    peers: Vec<PeerToml>,
}

#[derive(Debug, Deserialize)]
struct PeerToml {
    pubkey: String,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    paired_at: Option<String>,
}

/// Read `peers.toml`. Missing/garbled file → empty list (never panics). Live
/// telemetry fields default to `None`/`Unknown`; the per-frame live join
/// (`tabs::overview::apply_live_liveness`) fills them from `HostStatus` while
/// a host is up. `is_primary` reflects the persisted `primary_pubkey`; the
/// live join overrides it with the host's enforced value when connected.
pub fn load_peers() -> Vec<PeerInfo> {
    let p = path("peers.toml");
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let reg: PeerRegistryToml = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("peers.toml at {} unparseable: {e}", p.display());
            return Vec::new();
        }
    };
    let primary = reg.primary_pubkey;
    reg.peers
        .into_iter()
        .map(|p| PeerInfo {
            is_primary: primary
                .as_deref()
                .is_some_and(|pk| pk.eq_ignore_ascii_case(&p.pubkey)),
            alias: p.alias,
            pubkey: p.pubkey,
            paired_at: p.paired_at,
            liveness: crate::model::Liveness::Unknown,
            addr: None,
            encoder: None,
            codec: None,
            resolution: None,
            fps: None,
            last_connected: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Host config  (config.toml: [mqtt] + [network])
// ---------------------------------------------------------------------------
//
// Serde mirror of host-core's HostConfig, but Serialize+Deserialize so we can
// round-trip it. We only write [mqtt] + [network]; any other sections the host
// adds later are preserved by being re-read first… (kept simple: we rewrite the
// two sections we own, which is the documented contract).

#[derive(Debug, Default, Deserialize)]
struct HostConfigToml {
    #[serde(default)]
    mqtt: MqttToml,
    #[serde(default)]
    network: NetworkToml,
    #[serde(default)]
    compositor: CompositorToml,
}

#[derive(Debug, Deserialize)]
struct CompositorToml {
    #[serde(default = "default_true")]
    enable_virtual_displays: bool,
}

impl Default for CompositorToml {
    fn default() -> Self {
        Self {
            enable_virtual_displays: default_true(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct MqttToml {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_mqtt_host")]
    host: String,
    #[serde(default = "default_mqtt_port")]
    port: u16,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default = "default_discovery_prefix")]
    discovery_prefix: String,
    #[serde(default = "default_interval")]
    publish_interval_secs: u64,
}

impl Default for MqttToml {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_mqtt_host(),
            port: default_mqtt_port(),
            username: None,
            password: None,
            node_id: None,
            discovery_prefix: default_discovery_prefix(),
            publish_interval_secs: default_interval(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct NetworkToml {
    #[serde(default)]
    bind_mac: Option<String>,
    #[serde(default)]
    ipv6: bool,
}

fn default_mqtt_host() -> String {
    "127.0.0.1".into()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_discovery_prefix() -> String {
    "homeassistant".into()
}
fn default_interval() -> u64 {
    10
}

/// Read the configured encode pixel-throughput budget (`[encode]
/// max_pixels_per_sec` in `config.toml`). Mirrors host-core's `EncodeConfig`
/// default — `497_664_000` (3840×2160×60), a placeholder until the real ceiling
/// is queried from the GPU. Missing/garbled file → the default (never panics).
pub fn load_encode_budget() -> u64 {
    #[derive(Deserialize)]
    struct Root {
        #[serde(default)]
        encode: EncodeToml,
    }
    #[derive(Deserialize)]
    struct EncodeToml {
        #[serde(default = "default_budget")]
        max_pixels_per_sec: u64,
    }
    impl Default for EncodeToml {
        fn default() -> Self {
            Self {
                max_pixels_per_sec: default_budget(),
            }
        }
    }
    fn default_budget() -> u64 {
        497_664_000
    }
    let p = path("config.toml");
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| toml::from_str::<Root>(&t).ok())
        .map(|r| r.encode.max_pixels_per_sec)
        .filter(|b| *b > 0)
        .unwrap_or_else(default_budget)
}

pub fn load_host_config() -> HostConfigUi {
    let p = path("config.toml");
    let cfg: HostConfigToml = match std::fs::read_to_string(&p) {
        Ok(t) => match toml::from_str(&t) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config.toml at {} unparseable: {e}", p.display());
                HostConfigToml::default()
            }
        },
        Err(_) => HostConfigToml::default(),
    };
    HostConfigUi {
        mqtt_enabled: cfg.mqtt.enabled,
        mqtt_host: cfg.mqtt.host,
        mqtt_port: cfg.mqtt.port,
        mqtt_username: cfg.mqtt.username.unwrap_or_default(),
        mqtt_password: cfg.mqtt.password.unwrap_or_default(),
        mqtt_node_id: cfg.mqtt.node_id.unwrap_or_default(),
        mqtt_discovery_prefix: cfg.mqtt.discovery_prefix,
        mqtt_publish_interval_secs: cfg.mqtt.publish_interval_secs,
        bind_mac: cfg.network.bind_mac.unwrap_or_default(),
        ipv6: cfg.network.ipv6,
        enable_virtual_displays: cfg.compositor.enable_virtual_displays,
    }
}

/// Write `config.toml`'s `[mqtt]` + `[network]` + `[compositor]` tables,
/// **preserving any other section a host may have authored** (`config.toml`
/// is shared with the host). Read-merge-write: parse the on-disk file
/// generically, replace only the tables we own, re-serialize. Empty Strings
/// serialize as absent (`None`), matching the host's "missing == unset"
/// convention.
pub fn save_host_config(c: &HostConfigUi) -> Result<()> {
    let p = path("config.toml");
    let existing = std::fs::read_to_string(&p).unwrap_or_default();
    // 0600: config.toml may carry the MQTT password in plaintext at rest.
    atomic_write(&p, &merge_host_config_toml(&existing, c)?, Some(0o600))
}

/// Merge the panel-owned `[mqtt]`/`[network]`/`[compositor]` tables into
/// `existing` (parsed as a generic table so sections the panel doesn't model
/// survive). Pure + filesystem-free so it can be unit-tested directly.
fn merge_host_config_toml(existing: &str, c: &HostConfigUi) -> Result<String> {
    fn opt(s: &str) -> Option<&str> {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    }

    // A parse failure can't preserve unknown sections anyway, so fall back to a
    // fresh table — matching the old rewrite-everything behaviour for that case.
    let mut root: toml::Table = toml::from_str(existing).unwrap_or_default();

    let mut mqtt = toml::Table::new();
    mqtt.insert("enabled".into(), c.mqtt_enabled.into());
    mqtt.insert("host".into(), c.mqtt_host.trim().to_string().into());
    mqtt.insert("port".into(), i64::from(c.mqtt_port).into());
    if let Some(u) = opt(&c.mqtt_username) {
        mqtt.insert("username".into(), u.to_string().into());
    }
    if let Some(pw) = opt(&c.mqtt_password) {
        mqtt.insert("password".into(), pw.to_string().into());
    }
    if let Some(n) = opt(&c.mqtt_node_id) {
        mqtt.insert("node_id".into(), n.to_string().into());
    }
    mqtt.insert(
        "discovery_prefix".into(),
        c.mqtt_discovery_prefix.trim().to_string().into(),
    );
    mqtt.insert(
        "publish_interval_secs".into(),
        i64::try_from(c.mqtt_publish_interval_secs)
            .unwrap_or(i64::MAX)
            .into(),
    );
    root.insert("mqtt".into(), mqtt.into());

    let mut network = toml::Table::new();
    if let Some(m) = opt(&c.bind_mac) {
        network.insert("bind_mac".into(), m.to_string().into());
    }
    // Off is the default (host reads a missing key as false); only write it when on
    // so a default config keeps an empty [network] table.
    if c.ipv6 {
        network.insert("ipv6".into(), true.into());
    }
    root.insert("network".into(), network.into());

    let mut compositor = toml::Table::new();
    // On is the default (host reads a missing key as true); only write it
    // when off, same "omit the default" convention as `[network] ipv6` above.
    if !c.enable_virtual_displays {
        compositor.insert("enable_virtual_displays".into(), false.into());
    }
    root.insert("compositor".into(), compositor.into());

    toml::to_string_pretty(&root).context("serialize config.toml")
}

// ---------------------------------------------------------------------------
// Runtime settings + input cfg  (panel.toml — both live here)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct PanelToml {
    #[serde(default)]
    runtime: Option<RuntimeSettings>,
    #[serde(default)]
    input: Option<InputCfgUi>,
    /// Last device-reported stream geometry per peer pubkey (panel-owned
    /// telemetry; see [`load_device_geoms`]). `#[serde(default)]` keeps older
    /// `panel.toml` files loadable.
    #[serde(default)]
    devices: std::collections::BTreeMap<String, crate::model::DeviceGeom>,
}

fn load_panel_toml() -> PanelToml {
    let p = path("panel.toml");
    match std::fs::read_to_string(&p) {
        Ok(t) => match toml::from_str(&t) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("panel.toml at {} unparseable: {e}", p.display());
                PanelToml::default()
            }
        },
        Err(_) => PanelToml::default(),
    }
}

fn save_panel_toml(t: &PanelToml) -> Result<()> {
    let text = toml::to_string_pretty(t).context("serialize panel.toml")?;
    atomic_write(&path("panel.toml"), &text, None)
}

pub fn load_runtime_settings() -> RuntimeSettings {
    load_panel_toml().runtime.unwrap_or_default()
}

/// Persist runtime settings into `panel.toml`, preserving the `[input]` section.
pub fn save_runtime_settings(r: &RuntimeSettings) -> Result<()> {
    let mut t = load_panel_toml();
    t.runtime = Some(r.clone());
    save_panel_toml(&t)
}

pub fn load_input_cfg() -> InputCfgUi {
    load_panel_toml().input.unwrap_or_default()
}

/// Persist the input config into `panel.toml`, preserving `[runtime]`.
pub fn save_input_cfg(i: &InputCfgUi) -> Result<()> {
    let mut t = load_panel_toml();
    t.input = Some(i.clone());
    save_panel_toml(&t)
}

/// Last device-reported stream geometry per peer pubkey, from `panel.toml`'s
/// `[devices]` table. The panel records this from the live control socket the
/// first time it sees a device connected (resolution + measured fps), so the
/// Compositor tab can size a device's tile and the panel can show its last
/// resolution/Hz even while the device is offline.
pub fn load_device_geoms() -> BTreeMap<String, crate::model::DeviceGeom> {
    load_panel_toml().devices
}

/// Persist the device-geometry map into `panel.toml`, preserving `[runtime]` +
/// `[input]`. Called only when the map actually changed (a device connected at
/// a new resolution/fps), so we don't rewrite the file every frame.
pub fn save_device_geoms(devices: &BTreeMap<String, crate::model::DeviceGeom>) -> Result<()> {
    let mut t = load_panel_toml();
    t.devices = devices.clone();
    save_panel_toml(&t)
}

// ---------------------------------------------------------------------------
// Controller profiles  (controller-profiles.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct ControllerProfilesToml {
    #[serde(default)]
    controllers: BTreeMap<String, ControllerProfile>,
    #[serde(default)]
    presets: BTreeMap<String, ControllerProfile>,
}

pub fn load_controller_profiles() -> ControllerProfilesUi {
    let p = path("controller-profiles.toml");
    let parsed: ControllerProfilesToml = match std::fs::read_to_string(&p) {
        Ok(t) => match toml::from_str(&t) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "controller-profiles.toml at {} unparseable: {e}",
                    p.display()
                );
                ControllerProfilesToml::default()
            }
        },
        Err(_) => ControllerProfilesToml::default(),
    };
    ControllerProfilesUi {
        controllers: parsed.controllers,
        presets: parsed.presets,
        alias_draft: std::collections::HashMap::new(),
    }
}

pub fn save_controller_profiles(c: &ControllerProfilesUi) -> Result<()> {
    let out = ControllerProfilesToml {
        controllers: c.controllers.clone(),
        presets: c.presets.clone(),
    };
    let text = toml::to_string_pretty(&out).context("serialize controller profiles")?;
    atomic_write(&path("controller-profiles.toml"), &text, None)
}

// ---------------------------------------------------------------------------
// Compositor presets  (compositor-presets.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct CompositorPresetsToml {
    #[serde(default)]
    presets: BTreeMap<String, crate::model::CompositorLayout>,
}

/// Load the named compositor presets (each a full layout: display setup +
/// positions + per-device input flags). Missing/garbled file ⇒ empty.
pub fn load_compositor_presets() -> crate::model::CompositorPresetsUi {
    let p = path("compositor-presets.toml");
    let parsed: CompositorPresetsToml = match std::fs::read_to_string(&p) {
        Ok(t) => match toml::from_str(&t) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "compositor-presets.toml at {} unparseable: {e}",
                    p.display()
                );
                CompositorPresetsToml::default()
            }
        },
        Err(_) => CompositorPresetsToml::default(),
    };
    crate::model::CompositorPresetsUi {
        presets: parsed.presets,
        draft_name: String::new(),
        selected: String::new(),
    }
}

pub fn save_compositor_presets(p: &crate::model::CompositorPresetsUi) -> Result<()> {
    let out = CompositorPresetsToml {
        presets: p.presets.clone(),
    };
    let text = toml::to_string_pretty(&out).context("serialize compositor presets")?;
    atomic_write(&path("compositor-presets.toml"), &text, None)
}

// ---------------------------------------------------------------------------
// Compositor layout  (multi.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MultiCfgToml {
    fps: u32,
    #[serde(default, rename = "output")]
    outputs: Vec<OutputCfgToml>,
}

#[derive(Debug, Deserialize)]
struct OutputCfgToml {
    name: String,
    w: u32,
    h: u32,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    addr: String,
    /// Panel-only: the paired peer (pubkey) this output is pinned to, so the
    /// device↔output link (name sync + device-governed size) survives a
    /// save+reload. The compositor ignores this key. Absent for blank/manual
    /// outputs.
    #[serde(default)]
    peer: Option<String>,
    /// Panel-only: whether this device's INPUT forwarding is enabled (the canvas
    /// green toggle). Absent ⇒ enabled (today's default). The compositor ignores
    /// this key — the live gate is driven by the `SetInputEnabled` control verb.
    #[serde(default)]
    input_enabled: Option<bool>,
}

/// Read `multi.toml` into a layout. Missing/garbled file → empty layout at
/// 60 fps. The compositor TOML carries no panel ids/refresh, so we synthesize
/// ids and derive `refresh_mhz` from `fps`.
pub fn load_compositor_layout() -> CompositorLayout {
    let p = path("multi.toml");
    let cfg: MultiCfgToml = match std::fs::read_to_string(&p) {
        Ok(t) => match toml::from_str(&t) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("multi.toml at {} unparseable: {e}", p.display());
                return CompositorLayout::default();
            }
        },
        Err(_) => return CompositorLayout::default(),
    };
    let fps = cfg.fps.max(1);
    let mut next_id = 1u64;
    let outputs = cfg
        .outputs
        .into_iter()
        .map(|o| {
            let id = next_id;
            next_id += 1;
            OutputTile {
                id,
                name: o.name,
                addr: o.addr,
                peer_pubkey: o.peer.filter(|s| !s.trim().is_empty()),
                x: o.x,
                y: o.y,
                w: o.w,
                h: o.h,
                refresh_mhz: fps * 1000,
                input_enabled: o.input_enabled.unwrap_or(true),
            }
        })
        .collect();
    CompositorLayout {
        fps,
        outputs,
        next_id,
    }
}

/// Write the layout to `multi.toml`, **preserving host-authored top-level keys
/// the panel doesn't model** (`frames`, `launch`, `mode`, …). Read-merge-write
/// that overwrites only `fps` + the `[[output]]` array (the shape `to_multi_toml`
/// owns); `multi.toml` is shared with the compositor.
///
/// **Safety guard**: refuses to write an EMPTY layout over an on-disk
/// `multi.toml` that still has output(s) configured. A host with zero
/// outputs can't stream to anyone, so there's no legitimate reason to
/// persist that silently — and an empty in-memory `CompositorLayout` is far
/// more often a load race (the panel started before the host wrote
/// `multi.toml`, or read it mid-update) than a deliberate "delete every
/// output" action. This took the host down once already (`multi.toml` lost
/// its whole `[[output]]` array on a routine Save-all); see
/// `layout_wipe_is_refused_when_disk_has_outputs` below.
pub fn save_compositor_layout(l: &CompositorLayout) -> Result<()> {
    let p = path("multi.toml");
    let existing = std::fs::read_to_string(&p).unwrap_or_default();
    if would_wipe_configured_outputs(&existing, l) {
        anyhow::bail!(
            "refusing to save: the compositor layout in the panel has NO outputs, but \
             multi.toml on disk still has one or more configured — saving would delete \
             them and the host would fail to start. Drag your device(s) back onto the \
             Compositor canvas before saving, or edit multi.toml by hand if you really \
             want zero outputs."
        );
    }
    atomic_write(&p, &merge_multi_toml(&existing, l)?, None)
}

/// True when persisting `l` over `existing` would silently delete a real,
/// on-disk output configuration (see [`save_compositor_layout`]'s doc for
/// why this is refused rather than allowed). Pure + filesystem-free so it's
/// unit-testable without touching `$PHAROS_CONFIG_DIR`.
fn would_wipe_configured_outputs(existing: &str, l: &CompositorLayout) -> bool {
    if !l.outputs.is_empty() {
        return false;
    }
    toml::from_str::<toml::Table>(existing)
        .ok()
        .and_then(|t| t.get("output").cloned())
        .and_then(|v| v.as_array().map(|a| !a.is_empty()))
        .unwrap_or(false)
}

/// Flush every dirty section to disk, clearing each `dirty_*` flag as its write
/// succeeds. Stops at the first error (returning it) leaving the unsaved sections
/// dirty, so the user can see what didn't persist. Returns how many sections were
/// written. Used by the global Save button and save-on-quit.
pub fn save_all_dirty(state: &mut crate::model::PanelState) -> Result<usize> {
    let mut saved = 0;
    if state.dirty_host {
        save_host_config(&state.host_config)?;
        state.dirty_host = false;
        saved += 1;
    }
    if state.dirty_runtime {
        save_runtime_settings(&state.runtime)?;
        state.dirty_runtime = false;
        saved += 1;
    }
    if state.dirty_input {
        save_input_cfg(&state.input_cfg)?;
        state.dirty_input = false;
        saved += 1;
    }
    if state.dirty_controllers {
        save_controller_profiles(&state.controllers)?;
        state.dirty_controllers = false;
        saved += 1;
    }
    if state.dirty_layout {
        save_compositor_layout(&state.layout)?;
        state.dirty_layout = false;
        saved += 1;
    }
    if state.dirty_presets {
        save_compositor_presets(&state.presets)?;
        state.dirty_presets = false;
        saved += 1;
    }
    if state.dirty_apps {
        // The catalog round-trips through the shared `app-launcher` crate, so the
        // host reads back exactly what the panel writes (single source of truth).
        app_launcher::config::save(&state.apps)?;
        state.dirty_apps = false;
        saved += 1;
    }
    Ok(saved)
}

/// Splat the panel-owned `fps` + `output` array (rendered by
/// [`CompositorLayout::to_multi_toml`]) over whatever else is already in
/// `existing`. An empty layout clears any stale `[[output]]`. Pure + filesystem-
/// free for testing.
fn merge_multi_toml(existing: &str, l: &CompositorLayout) -> Result<String> {
    let mut root: toml::Table = toml::from_str(existing).unwrap_or_default();
    let owned: toml::Table = toml::from_str(&l.to_multi_toml()).context("render layout toml")?;
    for key in ["fps", "output"] {
        match owned.get(key) {
            Some(v) => {
                root.insert(key.to_string(), v.clone());
            }
            None => {
                root.remove(key);
            }
        }
    }
    toml::to_string_pretty(&root).context("serialize multi.toml")
}

// ---------------------------------------------------------------------------
// Decode capabilities  (media-decode probe)
// ---------------------------------------------------------------------------

/// Probe whether the host can actually inject input right now, beyond the
/// compile-time backend identity. On Linux the panel runs on the host, so we test
/// `/dev/uinput` writability directly — the most accurate check, since it honours
/// the udev/logind ACLs that plain group membership wouldn't reveal. Opening the
/// device (then dropping the handle) is side-effect-free; a virtual device only
/// exists after a `UI_DEV_CREATE` ioctl, which we never issue. Windows reports
/// [`InjectorHealth::Ready`] (SendInput, always usable in an interactive
/// session); other platforms return [`InjectorHealth::Unknown`] (not probed).
pub fn injector_health() -> crate::model::InjectorHealth {
    use crate::model::InjectorHealth;
    #[cfg(target_os = "linux")]
    {
        match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
            Ok(_) => InjectorHealth::Ready,
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound => {
                    InjectorHealth::Blocked("input.backend.health.no_device")
                }
                std::io::ErrorKind::PermissionDenied => {
                    InjectorHealth::Blocked("input.backend.health.no_perm")
                }
                _ => InjectorHealth::Blocked("input.backend.health.error"),
            },
        }
    }
    // Windows injects keyboard/mouse via Win32 SendInput, which is always usable
    // from an interactive desktop session (no device node to gate on), so report
    // Ready. (Gamepad injection there needs ViGEm and isn't wired yet — a caveat
    // surfaced elsewhere, not a backend-health failure.)
    #[cfg(windows)]
    {
        InjectorHealth::Ready
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        InjectorHealth::Unknown
    }
}

/// Map the `media-decode` probe into Decode-stage rows. The first call runs the
/// (cached) test-decode probe, which can take up to ~2 s; callers should treat
/// it as lazy/slow-once. `available = usable || software`.
pub fn decode_capabilities() -> Vec<CapabilityUi> {
    map_decode_caps(media_decode::capabilities())
}

/// Like [`decode_capabilities`] but **bypasses** the process-cached probe
/// (`media_decode::reprobe`), so the Video tab's Re-probe reflects a GPU
/// plugged/unplugged since startup instead of re-pulling the frozen result.
pub fn reprobe_decode() -> Vec<CapabilityUi> {
    map_decode_caps(&media_decode::reprobe())
}

/// Map a `media-decode` probe into the Video tab's capability rows. Shared by the
/// cached [`decode_capabilities`] and the uncached [`reprobe_decode`].
fn map_decode_caps(caps: &media_decode::Capabilities) -> Vec<CapabilityUi> {
    let mut out: Vec<CapabilityUi> = caps
        .methods
        .iter()
        .map(|m| {
            let available = m.usable || m.kind == Kind::Software;
            // Hardware we've validated end-to-end on real boxes (see MEMORY).
            let (validated, validated_on) = match m.id.as_str() {
                "cuvid" => (true, Some("RTX 4070 (dev rig)".to_string())),
                "qsv" => (true, Some("ThinkPad T14 (Quick Sync)".to_string())),
                "rkmpp" => (true, Some("RK3588".to_string())),
                "drm" => (true, Some("Raspberry Pi (DRM/KMS)".to_string())),
                "v4l2m2m" => (true, Some("Raspberry Pi 4".to_string())),
                "software" => (true, Some("any CPU".to_string())),
                _ => (false, None),
            };
            CapabilityUi {
                stage: PipelineStage::Decode,
                id: m.id.clone(),
                label: m.label.clone(),
                vendor: decoder_vendor(&m.id),
                codec: "HEVC".into(),
                token: decoder_token(m),
                available,
                validated: validated && available,
                validated_on,
                perf_rank: decode_perf_rank(&m.id),
                side: "client".into(),
                detail: m.detail.clone(),
            }
        })
        .collect();
    out.extend(av1_decode_display_rows());
    out
}

/// AV1 / AVC / VVC client decoders surfaced on the Video tab dashboard. Marked
/// available (the capability exists in a modern ffmpeg / on capable clients)
/// but not yet `validated` — proving each one end-to-end is the client-decode
/// slice. The HEVC rows above are the genuinely-probed ones; these are
/// reference rows so every host codec has a visible decode counterpart.
fn av1_decode_display_rows() -> Vec<CapabilityUi> {
    let mk =
        |id: &str, label: &str, vendor: &str, token: &str, codec: &str, rank: u8| CapabilityUi {
            stage: PipelineStage::Decode,
            id: id.into(),
            label: label.into(),
            vendor: vendor.into(),
            codec: codec.into(),
            token: token.into(),
            available: true,
            validated: false,
            validated_on: None,
            perf_rank: rank,
            side: "client".into(),
            detail: format!("{codec} decode via {token} (pending end-to-end validation)"),
        };
    vec![
        mk("cuvid-av1", "NVIDIA NVDEC", "NVIDIA", "av1_cuvid", "AV1", 1),
        mk("qsv-av1", "Quick Sync AV1", "Intel", "av1_qsv", "AV1", 2),
        mk(
            "dav1d",
            "dav1d (software)",
            "VideoLAN",
            "libdav1d",
            "AV1",
            5,
        ),
        // AVC: hardware H.264 decode is the one truly universal capability.
        mk(
            "cuvid-h264",
            "NVIDIA NVDEC",
            "NVIDIA",
            "h264_cuvid",
            "AVC",
            1,
        ),
        mk("qsv-h264", "Quick Sync AVC", "Intel", "h264_qsv", "AVC", 2),
        mk("software-h264", "Software (CPU)", "CPU", "h264", "AVC", 4),
        // VVC: hardware decode barely exists (Intel Lunar Lake); nearly every
        // client lands on ffmpeg's native software decoder — CPU-heavy.
        mk("qsv-vvc", "Quick Sync VVC", "Intel", "vvc_qsv", "VVC", 2),
        mk(
            "software-vvc",
            "Software (native VVC)",
            "CPU",
            "vvc",
            "VVC",
            6,
        ),
    ]
}

fn decoder_vendor(id: &str) -> String {
    match id {
        "cuvid" => "NVIDIA",
        "qsv" | "vaapi" => "Intel",
        "amf" => "AMD",
        "rkmpp" => "Rockchip",
        "drm" | "v4l2m2m" => "V4L2",
        "software" => "CPU",
        _ => "—",
    }
    .into()
}

fn decoder_token(m: &media_decode::Capability) -> String {
    // The probe stores input_args like ["-c:v","hevc_cuvid"] / ["-hwaccel","drm"].
    m.input_args.last().cloned().unwrap_or_else(|| "—".into())
}

fn decode_perf_rank(id: &str) -> u8 {
    match id {
        "cuvid" => 1,
        "qsv" | "rkmpp" => 2,
        "vaapi" | "amf" => 3,
        "drm" => 3,
        "v4l2m2m" => 4,
        "software" => 6,
        _ => 5,
    }
}

// ---------------------------------------------------------------------------
// Encode capabilities  (ffmpeg -encoders listing)
// ---------------------------------------------------------------------------

/// A static encoder catalogue entry:
/// `(id, label, vendor, token, codec, validated, validated_on, perf_rank)`.
type EncoderRow = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    bool,
    Option<&'static str>,
    u8,
);

/// Probe `ffmpeg -encoders` for the HEVC / AV1 / AVC / VVC encoders the host can
/// use, plus software. `available` = the encoder is listed by the local ffmpeg. Validated
/// flags reflect the rigs we've confirmed each on (NVENC=RTX4070, qsv=T14,
/// amf=RX570). Never panics: if ffmpeg can't be run, everything but software is
/// marked unavailable.
pub fn encode_capabilities() -> Vec<CapabilityUi> {
    let listed = ffmpeg_encoder_set();
    let has = |tok: &str| listed.iter().any(|w| w == tok);
    let gpus = gpu_vendors_present();

    // (id, label, vendor, token, codec, validated, validated_on, perf_rank)
    let rows: &[EncoderRow] = &[
        (
            "nvenc",
            "NVENC HEVC",
            "NVIDIA",
            "hevc_nvenc",
            "HEVC",
            true,
            Some("RTX 4070 (dev rig)"),
            1,
        ),
        (
            "nvenc-av1",
            "NVENC AV1",
            "NVIDIA",
            "av1_nvenc",
            "AV1",
            true,
            Some("RTX 4070 (Ada)"),
            1,
        ),
        (
            "qsv",
            "Quick Sync HEVC",
            "Intel",
            "hevc_qsv",
            "HEVC",
            true,
            Some("ThinkPad T14"),
            2,
        ),
        (
            "qsv-av1",
            "Quick Sync AV1",
            "Intel",
            "av1_qsv",
            "AV1",
            false,
            None,
            2,
        ),
        (
            "amf",
            "AMD AMF HEVC",
            "AMD",
            "hevc_amf",
            "HEVC",
            true,
            Some("RX 570 (.201)"),
            2,
        ),
        (
            "nvenc-h264",
            "NVENC AVC",
            "NVIDIA",
            "h264_nvenc",
            "AVC",
            false,
            None,
            1,
        ),
        (
            "qsv-h264",
            "Quick Sync AVC",
            "Intel",
            "h264_qsv",
            "AVC",
            false,
            None,
            2,
        ),
        (
            "amf-h264",
            "AMD AMF AVC",
            "AMD",
            "h264_amf",
            "AVC",
            false,
            None,
            2,
        ),
        (
            "software",
            "Software (libx265)",
            "CPU",
            "libx265",
            "HEVC",
            true,
            Some("any CPU"),
            6,
        ),
        (
            "software-av1",
            "Software (SVT-AV1)",
            "CPU",
            "libsvtav1",
            "AV1",
            false,
            None,
            6,
        ),
        (
            "software-h264",
            "Software (libx264)",
            "CPU",
            "libx264",
            "AVC",
            false,
            None,
            5,
        ),
        // VVC's ONLY encoder anywhere: no consumer GPU encodes H.266, so the
        // host software-encodes via ffmpeg's libvvenc. Experimental.
        (
            "software-vvc",
            "vvenc VVC (software)",
            "Fraunhofer",
            "libvvenc",
            "VVC",
            false,
            None,
            5,
        ),
    ];

    rows.iter()
        .map(|(id, label, vendor, token, codec, validated, on, rank)| {
            // Software encoders are "available" if their library OR a built-in
            // fallback is present; in practice one always is for HEVC.
            // Hardware encoders need BOTH the ffmpeg encoder AND the vendor's GPU
            // present (a distro ffmpeg lists qsv/amf even with no Intel/AMD GPU).
            let hw_missing_gpu = has(token) && !gpus.has_vendor(vendor);
            let available = match *id {
                "software" => has("libx265") || has("hevc"),
                "software-av1" => has("libsvtav1") || has("libaom-av1") || has("av1"),
                "software-h264" => has("libx264") || has("h264"),
                // No built-in fallback exists for VVC — libvvenc or nothing.
                "software-vvc" => has("libvvenc"),
                _ => has(token) && gpus.has_vendor(vendor),
            };
            let detail = if available {
                format!("listed by ffmpeg (-encoders): {token}")
            } else if hw_missing_gpu {
                format!("built into ffmpeg ({token}) but no {vendor} GPU present")
            } else {
                format!("not built into this ffmpeg ({token})")
            };
            CapabilityUi {
                stage: PipelineStage::Encode,
                id: (*id).to_string(),
                label: (*label).to_string(),
                vendor: (*vendor).to_string(),
                codec: (*codec).to_string(),
                token: (*token).to_string(),
                available,
                validated: *validated && available,
                validated_on: on.map(str::to_string),
                perf_rank: *rank,
                side: "host".into(),
                detail,
            }
        })
        .collect()
}

/// GPU vendors with a render-capable device present on THIS machine. Used to gate
/// hardware-encoder availability: `ffmpeg -encoders` lists every encoder ffmpeg
/// was BUILT with (a distro ffmpeg advertises `hevc_qsv` and `hevc_amf` even on a
/// box with no Intel/AMD GPU), which is a BUILD capability, not a hardware one. So
/// a hardware encoder is "available" only if ffmpeg lists it AND its vendor's GPU
/// is actually present.
#[derive(Clone, Copy, Default)]
struct GpuVendors {
    nvidia: bool,
    intel: bool,
    amd: bool,
}

impl GpuVendors {
    /// True if the given row vendor's silicon is present (non-GPU vendors —
    /// software / Fraunhofer — are always "present": they need no device).
    fn has_vendor(&self, vendor: &str) -> bool {
        match vendor {
            "NVIDIA" => self.nvidia,
            "Intel" => self.intel,
            "AMD" => self.amd,
            _ => true,
        }
    }
}

/// Detect present GPU vendors by PCI vendor id under `/sys/class/drm/renderD*`
/// (render nodes = GPUs that can actually encode; `card*` would double-count and
/// include non-render outputs). Linux-only; other platforms return all-true so the
/// pure ffmpeg-list gate is unchanged there (no cheap portable probe, and we must
/// not regress the Windows host's detection).
fn gpu_vendors_present() -> GpuVendors {
    #[cfg(target_os = "linux")]
    {
        let mut v = GpuVendors::default();
        if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
            for e in rd.flatten() {
                if !e.file_name().to_string_lossy().starts_with("renderD") {
                    continue;
                }
                if let Ok(s) = std::fs::read_to_string(e.path().join("device/vendor")) {
                    match s.trim().to_ascii_lowercase().as_str() {
                        "0x10de" => v.nvidia = true,
                        "0x8086" => v.intel = true,
                        "0x1002" => v.amd = true,
                        _ => {}
                    }
                }
            }
        }
        v
    }
    #[cfg(not(target_os = "linux"))]
    {
        GpuVendors {
            nvidia: true,
            intel: true,
            amd: true,
        }
    }
}

/// Run `ffmpeg -hide_banner -encoders` and collect its stdout words. Reuses
/// `media-decode`'s ffmpeg resolver (handles the Windows WinGet alias path).
fn ffmpeg_encoder_set() -> Vec<String> {
    let prog = media_decode::ffmpeg_program();
    let out = Command::new(&prog)
        .args(["-hide_banner", "-encoders"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
        Err(e) => {
            tracing::warn!("ffmpeg -encoders probe failed ({e}); assuming software only");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Embedded media tracks (ffprobe) — for the Play-media audio/subtitle pickers
// ---------------------------------------------------------------------------

/// One selectable embedded track of a media file, as ffprobe reports it. `id` is
/// mpv's **1-based per-type** track id (i.e. the value for `--aid` / `--sid`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaTrack {
    pub id: u32,
    pub lang: Option<String>,
    pub title: Option<String>,
    pub codec: Option<String>,
}

/// Probe a media file's embedded tracks via a single `ffprobe -of json` call,
/// partitioned into `(audio, subtitle)` in file order so each list's index+1 is
/// mpv's `--aid`/`--sid`. Empty on any failure (no ffprobe, unreadable file) —
/// the caller then just offers Auto/Off. Never panics.
pub fn probe_media_tracks(path: &str) -> (Vec<MediaTrack>, Vec<MediaTrack>) {
    let out = Command::new(media_decode::ffprobe_program())
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("stream=codec_type,codec_name:stream_tags=language,title")
        .arg("-of")
        .arg("json")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return (Vec::new(), Vec::new()),
    };
    let json: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let streams = json
        .get("streams")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let (mut audio, mut subs) = (Vec::new(), Vec::new());
    for s in &streams {
        let field = |k: &str| s.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let tag = |k: &str| {
            s.pointer(&format!("/tags/{k}"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let (lang, title, codec) = (tag("language"), tag("title"), field("codec_name"));
        match s.get("codec_type").and_then(|v| v.as_str()) {
            Some("audio") => audio.push(MediaTrack {
                id: audio.len() as u32 + 1,
                lang,
                title,
                codec,
            }),
            Some("subtitle") => subs.push(MediaTrack {
                id: subs.len() as u32 + 1,
                lang,
                title,
                codec,
            }),
            _ => {}
        }
    }
    (audio, subs)
}

#[cfg(test)]
mod tests {
    //! Read-merge-write tests for the shared-file savers: the panel must overwrite
    //! only the tables it owns and preserve every other section a host authored.
    //! The merge helpers are pure (no filesystem / env), so these are race-free
    //! under parallel `cargo test`.
    use super::*;
    use crate::model::{CompositorLayout, HostConfigUi, OutputTile};

    fn table(s: &str) -> toml::Table {
        toml::from_str(s).expect("valid toml")
    }

    #[test]
    fn gpu_vendor_gate_maps_vendors() {
        let only_nvidia = GpuVendors {
            nvidia: true,
            intel: false,
            amd: false,
        };
        assert!(only_nvidia.has_vendor("NVIDIA"));
        assert!(
            !only_nvidia.has_vendor("Intel"),
            "no iGPU ⇒ QSV unavailable"
        );
        assert!(!only_nvidia.has_vendor("AMD"));
        // Non-GPU vendors need no device and are always present.
        assert!(only_nvidia.has_vendor("Fraunhofer"));
        assert!(only_nvidia.has_vendor(""));
    }

    #[test]
    fn hardware_encoders_require_their_gpu() {
        // With ffmpeg listing qsv/amf/nvenc but only an NVIDIA GPU present, only
        // NVENC (+ software) is available; QSV/AMF are gated off.
        let caps = encode_capabilities();
        let avail = |id: &str| caps.iter().find(|c| c.id == id).map(|c| c.available);
        // This dev box has an NVIDIA render node and no Intel/AMD GPU.
        if gpu_vendors_present().nvidia && !gpu_vendors_present().intel {
            assert_eq!(avail("qsv"), Some(false), "no Intel GPU ⇒ QSV off");
        }
        if !gpu_vendors_present().amd {
            assert_eq!(avail("amf"), Some(false), "no AMD GPU ⇒ AMF off");
        }
    }

    #[test]
    fn host_config_preserves_unmodelled_sections() {
        let existing = "\
[mqtt]
enabled = false
host = \"old\"
port = 1

[network]
bind_mac = \"aa:bb:cc:dd:ee:ff\"

[host_only]
secret = \"keep me\"
answer = 42
";
        let c = HostConfigUi {
            mqtt_enabled: true,
            mqtt_host: "newhost".into(),
            mqtt_port: 1883,
            ..HostConfigUi::default()
        };
        let parsed = table(&merge_host_config_toml(existing, &c).unwrap());
        // Panel-owned table updated...
        let mqtt = parsed["mqtt"].as_table().unwrap();
        assert_eq!(mqtt["host"].as_str(), Some("newhost"));
        assert_eq!(mqtt["enabled"].as_bool(), Some(true));
        assert_eq!(mqtt["port"].as_integer(), Some(1883));
        // ...and the section the panel doesn't model survives intact.
        let host_only = parsed["host_only"].as_table().unwrap();
        assert_eq!(host_only["secret"].as_str(), Some("keep me"));
        assert_eq!(host_only["answer"].as_integer(), Some(42));
    }

    #[test]
    fn host_config_omits_empty_optionals() {
        // Empty username/password/node_id/bind_mac → absent, not "" keys.
        let parsed = table(&merge_host_config_toml("", &HostConfigUi::default()).unwrap());
        let mqtt = parsed["mqtt"].as_table().unwrap();
        assert!(!mqtt.contains_key("username"));
        assert!(!mqtt.contains_key("password"));
        assert!(!mqtt.contains_key("node_id"));
        assert!(!parsed["network"]
            .as_table()
            .unwrap()
            .contains_key("bind_mac"));
    }

    #[test]
    fn multi_layout_preserves_frames_launch_mode() {
        let existing = "\
fps = 30
frames = 600
launch = \"trireme play quic://x\"
mode = \"extend\"

[[output]]
name = \"stale\"
w = 100
h = 100
addr = \"quic://0.0.0.0:9000\"
";
        let layout = CompositorLayout {
            fps: 120,
            outputs: vec![OutputTile {
                id: 1,
                name: "virtual-0".into(),
                addr: "quic://0.0.0.0:7000".into(),
                peer_pubkey: None,
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                refresh_mhz: 120_000,
                input_enabled: true,
            }],
            ..CompositorLayout::default()
        };
        let parsed = table(&merge_multi_toml(existing, &layout).unwrap());
        // Host-authored top-level keys survive the panel's save.
        assert_eq!(parsed["frames"].as_integer(), Some(600));
        assert_eq!(parsed["launch"].as_str(), Some("trireme play quic://x"));
        assert_eq!(parsed["mode"].as_str(), Some("extend"));
        // Panel-owned fps + outputs are replaced (the stale output is gone).
        assert_eq!(parsed["fps"].as_integer(), Some(120));
        let outputs = parsed["output"].as_array().unwrap();
        assert_eq!(outputs.len(), 1);
        let o0 = outputs[0].as_table().unwrap();
        assert_eq!(o0["name"].as_str(), Some("virtual-0"));
        assert_eq!(o0["w"].as_integer(), Some(1920));
        assert_eq!(o0["addr"].as_str(), Some("quic://0.0.0.0:7000"));
    }

    #[test]
    fn multi_layout_clears_outputs_when_empty() {
        let existing = "fps = 30\n\n[[output]]\nname = \"x\"\nw = 1\nh = 1\naddr = \"a\"\n";
        let layout = CompositorLayout {
            fps: 60,
            ..CompositorLayout::default()
        };
        let parsed = table(&merge_multi_toml(existing, &layout).unwrap());
        assert_eq!(parsed["fps"].as_integer(), Some(60));
        assert!(
            !parsed.contains_key("output"),
            "an empty layout must drop the [[output]] array"
        );
    }

    /// The bug that took the host down: a panel session whose in-memory
    /// layout never got populated (or was cleared) must NOT be allowed to
    /// silently overwrite a `multi.toml` that still has real outputs —
    /// `save_compositor_layout` guards this via `would_wipe_configured_outputs`.
    #[test]
    fn layout_save_is_refused_when_it_would_wipe_configured_outputs() {
        let existing = "fps = 30\n\n[[output]]\nname = \"x\"\nw = 1\nh = 1\naddr = \"a\"\n";
        let empty_layout = CompositorLayout::default();
        assert!(would_wipe_configured_outputs(existing, &empty_layout));

        // A layout that still has an output is always safe to save.
        let non_empty_layout = CompositorLayout {
            outputs: vec![OutputTile {
                id: 1,
                name: "virtual-0".into(),
                addr: "quic://0.0.0.0:7000".into(),
                peer_pubkey: None,
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                refresh_mhz: 60_000,
                input_enabled: true,
            }],
            ..CompositorLayout::default()
        };
        assert!(!would_wipe_configured_outputs(existing, &non_empty_layout));

        // An empty layout over an ALREADY-empty (or missing/garbled) file is
        // fine — there's nothing real to lose.
        assert!(!would_wipe_configured_outputs("fps = 30\n", &empty_layout));
        assert!(!would_wipe_configured_outputs("", &empty_layout));
        assert!(!would_wipe_configured_outputs(
            "not valid toml {{{",
            &empty_layout
        ));
    }

    #[test]
    fn compositor_presets_round_trip() {
        // A preset whose layout has a peer-pinned, input-DISABLED tile at a
        // non-default position — the panel-only bits (input flag + positions)
        // must survive the derive-based serialize that the presets file uses
        // (distinct from multi.toml's hand-rolled writer).
        let mut layout = CompositorLayout {
            fps: 90,
            next_id: 7,
            ..CompositorLayout::default()
        };
        layout.outputs.push(OutputTile {
            id: 3,
            name: "couch".into(),
            addr: "quic://0.0.0.0:15777".into(),
            peer_pubkey: Some("ab".repeat(32)),
            x: 1920,
            y: 200,
            w: 2712,
            h: 1220,
            refresh_mhz: 90_000,
            input_enabled: false,
        });
        let mut presets = BTreeMap::new();
        presets.insert("couch co-op".to_string(), layout);
        let text = toml::to_string_pretty(&CompositorPresetsToml { presets }).unwrap();
        let back: CompositorPresetsToml = toml::from_str(&text).unwrap();
        let p = back.presets.get("couch co-op").expect("preset survives");
        assert_eq!(p.fps, 90);
        let o = &p.outputs[0];
        assert!(!o.input_enabled, "the disabled input flag must round-trip");
        assert_eq!((o.x, o.y), (1920, 200), "positions must round-trip");
        assert_eq!(o.peer_pubkey.as_deref(), Some("ab".repeat(32).as_str()));
    }
}
