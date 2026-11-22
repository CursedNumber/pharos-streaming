// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! The Pharos control panel's data model — the **frozen shared contract**.
//!
//! Every tab the panel renders reads and mutates a single [`PanelState`]. The
//! types here mirror the host's on-disk + wire formats (see the `crate::data`
//! loaders/savers) but are panel-local "UI mirrors": they add liveness, dirty
//! flags, and display-friendly shapes (`String` instead of `Option<String>`
//! for text fields, `BTreeMap` for stable controller order, …) that the raw
//! host structs don't carry.
//!
//! Tab agents must treat the public field names here as fixed: they bind UI
//! widgets directly to these fields and call `crate::data` to persist.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use core_proto::ControllerProfile;

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

/// The top-level sections of the control panel (left sidebar). All flat — none
/// are nested, so every section is one arrow-key step away (accessibility).
///
/// Note: the `Overview` variant is displayed as "Status"; the internal name is
/// kept (module `tabs::overview`, locale namespace `overview.*`) to avoid churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Tab {
    #[default]
    Overview,
    Compositor,
    Video,
    Audio,
    Input,
    Controllers,
    Apps,
    Settings,
}

impl Tab {
    /// Every tab, in logical order (Settings last).
    pub fn all() -> [Tab; 8] {
        [
            Tab::Overview,
            Tab::Compositor,
            Tab::Video,
            Tab::Audio,
            Tab::Input,
            Tab::Controllers,
            Tab::Apps,
            Tab::Settings,
        ]
    }

    /// The main navigation group rendered at the TOP of the sidebar, in order.
    /// `Settings` is deliberately excluded — it is pinned to the bottom.
    pub fn main_nav() -> [Tab; 7] {
        [
            Tab::Overview,
            Tab::Compositor,
            Tab::Video,
            Tab::Audio,
            Tab::Input,
            Tab::Controllers,
            Tab::Apps,
        ]
    }

    /// i18n key for this tab's sidebar label.
    fn label_key(self) -> &'static str {
        match self {
            Tab::Overview => "tab.status",
            Tab::Compositor => "tab.compositor",
            Tab::Video => "tab.video",
            Tab::Audio => "tab.audio",
            Tab::Input => "tab.input",
            Tab::Controllers => "tab.controller",
            Tab::Apps => "tab.apps",
            Tab::Settings => "tab.settings",
        }
    }

    /// Localised sidebar label (falls back to English).
    pub fn label(self) -> String {
        crate::i18n::tr(self.label_key())
    }

    /// Parse a tab from a case-insensitive name (used by the `PHAROS_PANEL_TAB`
    /// env var to deep-link the panel to a tab on launch). Accepts both the
    /// display name and the historical internal name where they differ.
    pub fn from_name(s: &str) -> Option<Tab> {
        match s.trim().to_ascii_lowercase().as_str() {
            "status" | "overview" => Some(Tab::Overview),
            "compositor" => Some(Tab::Compositor),
            "video" => Some(Tab::Video),
            "audio" => Some(Tab::Audio),
            "input" | "hid" => Some(Tab::Input),
            "controller" | "controllers" | "gamepad" => Some(Tab::Controllers),
            "apps" | "applications" | "launcher" => Some(Tab::Apps),
            "settings" => Some(Tab::Settings),
            _ => None,
        }
    }

    /// A bundled Phosphor icon glyph for the sidebar (see `crate::fonts`).
    pub fn icon(self) -> &'static str {
        use egui_phosphor::regular as ph;
        match self {
            Tab::Overview => ph::PULSE,          // Status
            Tab::Compositor => ph::SQUARES_FOUR, // display layout
            Tab::Video => ph::FILM_STRIP,
            Tab::Audio => ph::SPEAKER_HIGH,
            Tab::Input => ph::KEYBOARD,
            Tab::Controllers => ph::GAME_CONTROLLER,
            Tab::Apps => ph::ROCKET_LAUNCH,
            Tab::Settings => ph::GEAR_SIX,
        }
    }
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// A peer's (or the host's) current activity state, used for the status badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Liveness {
    /// Reachable / connected but idle.
    Online,
    /// Actively streaming right now.
    Streaming,
    /// Known but not currently reachable.
    Offline,
    /// We haven't probed it yet (no live telemetry available).
    #[default]
    Unknown,
}

impl Liveness {
    /// Badge colour — reuses the Trireme client's status palette.
    pub fn color(self) -> egui::Color32 {
        match self {
            Liveness::Online => crate::theme::online(),
            Liveness::Streaming => crate::theme::streaming(),
            Liveness::Offline => crate::theme::error(),
            Liveness::Unknown => crate::theme::info(),
        }
    }

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            Liveness::Online => "liveness.online",
            Liveness::Streaming => "liveness.streaming",
            Liveness::Offline => "liveness.offline",
            Liveness::Unknown => "liveness.unknown",
        })
    }
}

/// Result of *probing* whether the host can actually inject input right now —
/// distinct from the compile-time backend identity ([`crate::tabs`]). The panel
/// runs on the host, so on Linux it can test `/dev/uinput` writability directly;
/// other platforms aren't probed. Produced by [`crate::data::injector_health`].
#[derive(Debug, Clone, Copy)]
pub enum InjectorHealth {
    /// Probed and the injector can run (e.g. `/dev/uinput` is writable).
    Ready,
    /// Probed and it can't; the payload is the i18n key explaining why.
    Blocked(&'static str),
    /// Not probed on this platform — the compile-time backend is assumed.
    Unknown,
}

impl InjectorHealth {
    /// Badge colour: green ready, amber blocked, neutral unknown.
    pub fn color(self) -> egui::Color32 {
        match self {
            InjectorHealth::Ready => crate::theme::online(),
            InjectorHealth::Blocked(_) => crate::theme::warn(),
            InjectorHealth::Unknown => crate::theme::info(),
        }
    }
}

// ---------------------------------------------------------------------------
// Peers
// ---------------------------------------------------------------------------

/// A paired Trireme client, as the panel displays it. The persisted core is
/// `{alias, pubkey, paired_at}` (from `peers.toml`); the rest is live telemetry
/// that backends may fill in later (today they default to `None`/`Unknown`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Friendly name supplied by the client during pairing.
    pub alias: Option<String>,
    /// 32-byte Ed25519 pubkey, hex-encoded.
    pub pubkey: String,
    /// ISO-8601 hint of when this peer first paired.
    pub paired_at: Option<String>,
    /// Current activity (telemetry; `Unknown` until a backend probes it).
    pub liveness: Liveness,
    /// Last-seen network address (telemetry).
    pub addr: Option<String>,
    /// Encoder the host is using for this peer's stream (telemetry).
    pub encoder: Option<String>,
    /// Video codec being streamed — what this client is decoding, e.g. "HEVC"
    /// / "AV1" (telemetry; the closest honest proxy for the client's decoder,
    /// which the host can't see directly).
    pub codec: Option<String>,
    /// Stream resolution the host is encoding for this client, e.g. "2712×1220"
    /// (telemetry; taken from the primary output while connected).
    pub resolution: Option<String>,
    /// Negotiated frame rate (telemetry).
    pub fps: Option<u32>,
    /// ISO-8601 hint of the last successful connection (telemetry).
    pub last_connected: Option<String>,
    /// True when this peer is the host's PRIMARY client (issue #19): while it
    /// is connected, other clients are refused instead of pre-empting it.
    /// Seeded from `peers.toml` (`primary_pubkey`); overridden by the live
    /// `HostStatus.primary_pubkey` while a host is connected.
    #[serde(default)]
    pub is_primary: bool,
}

impl PeerInfo {
    /// The name to show: alias if present, else a short pubkey fingerprint.
    pub fn display_name(&self) -> String {
        if let Some(a) = &self.alias {
            if !a.trim().is_empty() {
                return a.clone();
            }
        }
        fingerprint(&self.pubkey)
    }
}

/// Short fingerprint of a hex key — first 8 + last 8 chars (matches menu.rs).
///
/// Counts by `char` (not byte) so a non-ASCII input can never panic on a
/// UTF-8 boundary; hex pubkeys are ASCII so this is identical for the real
/// inputs, but the function is part of the frozen public contract and may be
/// handed arbitrary strings by a tab.
pub fn fingerprint(hex: &str) -> String {
    let n = hex.chars().count();
    if n <= 20 {
        return hex.to_string();
    }
    let head: String = hex.chars().take(8).collect();
    let tail: String = hex.chars().skip(n - 8).collect();
    format!("{head}…{tail}")
}

// ---------------------------------------------------------------------------
// Pipeline capabilities (Video tab)
// ---------------------------------------------------------------------------

/// Which stage of the media pipeline a capability belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineStage {
    Encode,
    Transport,
    Decode,
    Audio,
}

impl PipelineStage {
    pub fn label(self) -> &'static str {
        match self {
            PipelineStage::Encode => "Encode",
            PipelineStage::Transport => "Transport",
            PipelineStage::Decode => "Decode",
            PipelineStage::Audio => "Audio",
        }
    }
}

/// One pipeline capability row, normalised for the Video tab's tables. Built by
/// `crate::data::decode_capabilities` / `encode_capabilities` from the
/// `media-decode` probe + an `ffmpeg -encoders` listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityUi {
    pub stage: PipelineStage,
    /// Short stable id (`cuvid`, `qsv`, `nvenc`, `software`, …).
    pub id: String,
    /// Human label ("NVIDIA NVENC", "Intel Quick Sync", …).
    pub label: String,
    /// Vendor/family ("NVIDIA", "Intel", "AMD", "Rockchip", "CPU", …).
    pub vendor: String,
    /// Codec this row covers ("HEVC", "Opus", "QUIC", …).
    pub codec: String,
    /// The ffmpeg token / selector (e.g. `hevc_nvenc`, `hevc_cuvid`, `drm`).
    pub token: String,
    /// Present + working on this box (probe `usable`, or ffmpeg lists it).
    pub available: bool,
    /// Confirmed working on our hardware (test-decode passed / known-good rig).
    pub validated: bool,
    /// Where it was validated, for the tooltip (e.g. "RTX 4070", "T14").
    pub validated_on: Option<String>,
    /// 1 = excellent … 6 = very poor (Sunshine-style ramp; see `theme::perf_color`).
    pub perf_rank: u8,
    /// "host" or "client" — which end of the link runs this.
    pub side: String,
    /// Probe detail / tooltip ("test-decode ok", "no working device", …).
    pub detail: String,
}

// ---------------------------------------------------------------------------
// Compositor layout (Compositor tab)
// ---------------------------------------------------------------------------

/// How the host fits the source desktop(s) onto a set of differently-sized
/// client displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ResolutionPolicy {
    /// Each virtual display runs at its own native resolution (1:1 per client).
    #[default]
    NativePerDisplay,
    /// All displays share the lowest-common resolution (classic LCD wall).
    UniformLcd,
    /// Stretch the source to fill each display, ignoring aspect ratio.
    StretchFill,
    /// Fit the source with letterbox/pillarbox bars, preserving aspect.
    Letterbox,
}

impl ResolutionPolicy {
    pub fn all() -> [ResolutionPolicy; 4] {
        [
            ResolutionPolicy::NativePerDisplay,
            ResolutionPolicy::UniformLcd,
            ResolutionPolicy::StretchFill,
            ResolutionPolicy::Letterbox,
        ]
    }

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            ResolutionPolicy::NativePerDisplay => "respol.native",
            ResolutionPolicy::UniformLcd => "respol.uniform",
            ResolutionPolicy::StretchFill => "respol.stretch",
            ResolutionPolicy::Letterbox => "respol.letterbox",
        })
    }

    pub fn describe(self) -> String {
        crate::i18n::tr(match self {
            ResolutionPolicy::NativePerDisplay => "respol.native_desc",
            ResolutionPolicy::UniformLcd => "respol.uniform_desc",
            ResolutionPolicy::StretchFill => "respol.stretch_desc",
            ResolutionPolicy::Letterbox => "respol.letterbox_desc",
        })
    }
}

/// How refresh rates are reconciled across displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RefreshPolicy {
    /// Drive every display at the lowest common refresh (one shared pacer).
    #[default]
    LowestCommon,
    /// Each display runs at its own refresh (independent pacing).
    PerDisplay,
}

impl RefreshPolicy {
    pub fn all() -> [RefreshPolicy; 2] {
        [RefreshPolicy::LowestCommon, RefreshPolicy::PerDisplay]
    }

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            RefreshPolicy::LowestCommon => "refpol.lowest",
            RefreshPolicy::PerDisplay => "refpol.per_display",
        })
    }

    pub fn describe(self) -> String {
        crate::i18n::tr(match self {
            RefreshPolicy::LowestCommon => "refpol.lowest_desc",
            RefreshPolicy::PerDisplay => "refpol.per_display_desc",
        })
    }
}

/// One virtual display in the compositor layout — maps to a `[[output]]` block
/// in the compositor's `multi.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputTile {
    /// Panel-local stable id (for egui widget ids + removal). Not persisted to
    /// the compositor TOML.
    pub id: u64,
    /// Output name (e.g. "HDMI-A-1", "couch", …).
    pub name: String,
    /// `quic://host:port` sink (or a file path for raw HEVC dumps).
    pub addr: String,
    /// The peer this output streams to, if pinned (telemetry / convenience).
    pub peer_pubkey: Option<String>,
    /// Virtual desktop placement (top-left), in pixels.
    pub x: i32,
    pub y: i32,
    /// Mode, in pixels.
    pub w: u32,
    pub h: u32,
    /// Refresh in millihertz (e.g. 120_000 = 120 Hz). Display only — the
    /// compositor derives the per-output refresh from the shared `fps`.
    pub refresh_mhz: u32,
    /// Whether this device's INPUT is forwarded to the host seat (the canvas
    /// green toggle). Defaults to `true` (today's behavior — all input flows);
    /// the user clicks the card to disable a viewer's input. Pushed live to the
    /// host via `SetInputEnabled`; also persisted so the choice survives a reload.
    pub input_enabled: bool,
}

/// The whole compositor layout — the set of virtual displays + the shared FPS.
/// Serializes to the compositor's `multi.toml` via [`to_multi_toml`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositorLayout {
    /// Shared frame rate across all outputs (the compositor uses a single pacer).
    pub fps: u32,
    pub outputs: Vec<OutputTile>,
    /// Next id to hand out from [`add_output`]. Persisted so ids stay stable.
    pub next_id: u64,
}

impl Default for CompositorLayout {
    fn default() -> Self {
        Self {
            fps: 60,
            outputs: Vec::new(),
            next_id: 1,
        }
    }
}

impl CompositorLayout {
    /// Default QUIC port for the first output; later outputs increment from here.
    /// The single deliberate Pharos connect port (mirrors `transport::PHAROS_STREAM_PORT`
    /// = 15777). Mid-range + clear of pairing (4432) / USB-import (4434), so the
    /// per-output increment no longer walks into a sibling service.
    pub const BASE_PORT: u16 = 15777;

    /// Add a fresh output, placed to the right of the rightmost existing one,
    /// with an auto-incrementing `quic://0.0.0.0:<port>` sink. Returns its id.
    pub fn add_output(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        // Place to the right of everything so tiles don't overlap by default.
        let x = self
            .outputs
            .iter()
            .map(|o| o.x + o.w as i32)
            .max()
            .unwrap_or(0);
        let port = Self::BASE_PORT + self.outputs.len() as u16;
        self.outputs.push(OutputTile {
            id,
            name: format!("output-{id}"),
            addr: format!("quic://0.0.0.0:{port}"),
            peer_pubkey: None,
            x,
            y: 0,
            w: 1920,
            h: 1080,
            refresh_mhz: self.fps * 1000,
            input_enabled: true,
        });
        id
    }

    /// Remove the output with `id`. Returns true if one was removed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.outputs.len();
        self.outputs.retain(|o| o.id != id);
        self.outputs.len() != before
    }

    /// Emit the exact `MultiCfg`/`OutputCfg` TOML the compositor reads:
    /// a top-level `fps` plus one `[[output]]` block per tile with
    /// `name`/`w`/`h`/`x`/`y`/`addr`. The panel-only `id`/`peer`/`refresh`
    /// fields are intentionally omitted (the compositor ignores unknown keys
    /// anyway, but we keep the file clean).
    pub fn to_multi_toml(&self) -> String {
        let mut s = String::new();
        s.push_str("# Generated by pharos-panel — compositor multi-output layout.\n");
        s.push_str(&format!("fps = {}\n", self.fps));
        for o in &self.outputs {
            s.push('\n');
            s.push_str("[[output]]\n");
            s.push_str(&format!("name = {}\n", toml_str(&o.name)));
            s.push_str(&format!("w = {}\n", o.w));
            s.push_str(&format!("h = {}\n", o.h));
            s.push_str(&format!("x = {}\n", o.x));
            s.push_str(&format!("y = {}\n", o.y));
            s.push_str(&format!("addr = {}\n", toml_str(&o.addr)));
            // Panel-only: persist the peer link so the device↔output binding
            // (name sync + device-governed size) survives a save+reload. The
            // compositor ignores unknown keys, so this is invisible to it.
            if let Some(pk) = o.peer_pubkey.as_deref().filter(|s| !s.trim().is_empty()) {
                s.push_str(&format!("peer = {}\n", toml_str(pk)));
            }
            // Panel-only: persist a disabled input toggle so it survives a reload
            // (enabled is the default, so only write the off case — keeps the
            // file clean; the compositor ignores this key either way).
            if !o.input_enabled {
                s.push_str("input_enabled = false\n");
            }
        }
        s
    }
}

/// Minimal TOML string escaper for the values we emit (names + quic URLs).
fn toml_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// Host config mirror (Settings tab) — mirrors host-core HostConfig
// ---------------------------------------------------------------------------

/// UI mirror of the host's `config.toml` (`[mqtt]` + `[network]`). Text fields
/// are plain `String`s; an empty string means "unset" (serialized as absent /
/// `None`) when [`crate::data::save_host_config`] writes it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfigUi {
    pub mqtt_enabled: bool,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_username: String,
    pub mqtt_password: String,
    pub mqtt_node_id: String,
    pub mqtt_discovery_prefix: String,
    pub mqtt_publish_interval_secs: u64,
    /// `[network] bind_mac` — empty = no MAC pinning.
    pub bind_mac: String,
    /// `[network] ipv6` — bind the QUIC listener dual-stack (IPv6 + IPv4). Off by default.
    pub ipv6: bool,
    /// `[compositor] enable_virtual_displays` (issue #6 desktop mirror) — the
    /// master switch. On by default (today's behavior unchanged); off routes
    /// every stream through the real-desktop mirror instead of a headless
    /// virtual output.
    pub enable_virtual_displays: bool,
    /// `[dsu] enabled`: the DSU motion server that relays client gyro to
    /// emulators. Off by default; the host picks it up live.
    pub dsu_enabled: bool,
    /// `[gyro] mouse_enabled`: global gyro→mouse switch. Off by default.
    pub gyro_mouse_enabled: bool,
}

impl Default for HostConfigUi {
    fn default() -> Self {
        // Matches host-core's MqttConfig/CompositorConfig defaults (MQTT off,
        // HA prefix; virtual displays ON).
        Self {
            mqtt_enabled: false,
            mqtt_host: "127.0.0.1".into(),
            mqtt_port: 1883,
            mqtt_username: String::new(),
            mqtt_password: String::new(),
            mqtt_node_id: String::new(),
            mqtt_discovery_prefix: "homeassistant".into(),
            mqtt_publish_interval_secs: 10,
            bind_mac: String::new(),
            ipv6: false,
            enable_virtual_displays: true,
            dsu_enabled: false,
            gyro_mouse_enabled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime / streaming tuning (Settings + Video) — panel.toml
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Appearance (Settings → Appearance): theme + font size
// ---------------------------------------------------------------------------

/// Visual theme for the panel. Drives the egui `Visuals` (background + text
/// contrast) and the status/accent palette. Chosen in Settings → Appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum Theme {
    /// Comfortable grey with bright, high-legibility text (default).
    #[default]
    Comfort = 0,
    /// Near-black background for dim rooms; bright text.
    Midnight = 1,
    /// Black text on a white background, for bright rooms.
    Daylight = 2,
    /// Maximum contrast with a colourblind-safe (Okabe–Ito) accent palette.
    HighContrast = 3,
}

impl Theme {
    pub fn all() -> [Theme; 4] {
        [
            Theme::Comfort,
            Theme::Midnight,
            Theme::Daylight,
            Theme::HighContrast,
        ]
    }

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            Theme::Comfort => "theme.comfort",
            Theme::Midnight => "theme.midnight",
            Theme::Daylight => "theme.daylight",
            Theme::HighContrast => "theme.high_contrast",
        })
    }

    pub fn describe(self) -> String {
        crate::i18n::tr(match self {
            Theme::Comfort => "theme.comfort_desc",
            Theme::Midnight => "theme.midnight_desc",
            Theme::Daylight => "theme.daylight_desc",
            Theme::HighContrast => "theme.high_contrast_desc",
        })
    }

    /// Parse a case-insensitive name (used by the `PHAROS_PANEL_THEME` env var).
    pub fn from_name(s: &str) -> Option<Theme> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_', ' '], "")
            .as_str()
        {
            "comfort" | "comfortgrey" | "grey" | "gray" => Some(Theme::Comfort),
            "midnight" | "dark" => Some(Theme::Midnight),
            "daylight" | "light" | "white" => Some(Theme::Daylight),
            "highcontrast" | "hc" | "contrast" => Some(Theme::HighContrast),
            _ => None,
        }
    }

    /// Reconstruct from the atomic byte published in `theme.rs`.
    pub fn from_u8(v: u8) -> Theme {
        match v {
            1 => Theme::Midnight,
            2 => Theme::Daylight,
            3 => Theme::HighContrast,
            _ => Theme::Comfort,
        }
    }

    /// True for themes that use a light background (affects accent choices).
    pub fn is_light(self) -> bool {
        matches!(self, Theme::Daylight)
    }
}

/// Overall UI scale, applied via egui's zoom factor. "Small" is the original
/// (pre-theming) size; the panel defaults to Medium for comfort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FontScale {
    Small,
    #[default]
    Medium,
    Large,
}

impl FontScale {
    pub fn all() -> [FontScale; 3] {
        [FontScale::Small, FontScale::Medium, FontScale::Large]
    }

    pub fn label(self) -> String {
        crate::i18n::tr(match self {
            FontScale::Small => "fontscale.small",
            FontScale::Medium => "fontscale.medium",
            FontScale::Large => "fontscale.large",
        })
    }

    /// Parse a case-insensitive name (used by the `PHAROS_PANEL_FONT` env var).
    pub fn from_name(s: &str) -> Option<FontScale> {
        match s.trim().to_ascii_lowercase().as_str() {
            "small" | "s" => Some(FontScale::Small),
            "medium" | "med" | "m" => Some(FontScale::Medium),
            "large" | "l" | "big" => Some(FontScale::Large),
            _ => None,
        }
    }

    /// egui zoom factor (multiplies the native pixels-per-point).
    pub fn factor(self) -> f32 {
        match self {
            FontScale::Small => 1.0,
            FontScale::Medium => 1.18,
            FontScale::Large => 1.4,
        }
    }
}

/// Streaming + behaviour knobs the panel owns (persisted to `panel.toml`).
/// These map onto the host's `PHAROS_*` env-var tuning + policy choices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    /// Target encoder bitrate (kbit/s) — `PHAROS_BITRATE_KBPS`.
    pub bitrate_kbps: u32,
    /// QUIC flow-control window (KiB) — the latency-critical knob
    /// (`PHAROS_STREAM_WINDOW_KB`).
    pub stream_window_kb: u32,
    /// Encoder queue depth in frames — `PHAROS_QUEUE_FRAMES`.
    pub queue_frames: u32,
    /// How long the host waits for a client before giving up (seconds).
    pub client_wait_secs: u64,
    /// Swap A/B face buttons globally on the virtual pad.
    pub gamepad_swap_face: bool,
    /// Accept clients that have not paired (`PHAROS_ALLOW_UNPAIRED`).
    pub allow_unpaired: bool,
    pub resolution_policy: ResolutionPolicy,
    pub refresh_policy: RefreshPolicy,
    /// Panel appearance (defaulted so older `panel.toml` files still load).
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub font_scale: FontScale,
    /// Interface language (locale code, e.g. "en"). Default + fallback: English.
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_language() -> String {
    "en".to_string()
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            bitrate_kbps: 20_000,
            stream_window_kb: 256,
            queue_frames: 8,
            client_wait_secs: 120,
            gamepad_swap_face: false,
            allow_unpaired: false,
            resolution_policy: ResolutionPolicy::default(),
            refresh_policy: RefreshPolicy::default(),
            theme: Theme::default(),
            font_scale: FontScale::default(),
            language: default_language(),
        }
    }
}

// ---------------------------------------------------------------------------
// Input config mirror (Input tab) — mirrors trireme InputCfg
// ---------------------------------------------------------------------------

/// UI mirror of the input-forwarding config (`[input]` in the host's sources).
/// `mode` is one of `focus | toggle | always | off`. Defaults mirror the
/// Trireme client's `InputCfg::default()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCfgUi {
    pub mode: String,
    pub forward_keyboard: bool,
    pub forward_mouse: bool,
    pub forward_scroll: bool,
    /// Chord strings that stay local (never forwarded).
    pub local_escapes: Vec<String>,
    pub toggle_capture: String,
    pub toggle_fullscreen: String,
    pub disconnect: String,
}

impl Default for InputCfgUi {
    fn default() -> Self {
        Self {
            mode: "focus".into(),
            forward_keyboard: true,
            forward_mouse: true,
            forward_scroll: true,
            local_escapes: vec![
                "Ctrl+Alt+Delete".into(),
                "Ctrl+Shift+Escape".into(),
                "Win+L".into(),
                "F11".into(),
                "Alt+Tab".into(),
                "Alt+Shift+Tab".into(),
                "Win+Tab".into(),
            ],
            toggle_capture: "Ctrl+Alt+End".into(),
            toggle_fullscreen: "F10".into(),
            disconnect: "Ctrl+Alt+Backspace".into(),
        }
    }
}

impl InputCfgUi {
    /// The four valid capture modes, for a combo box.
    pub const MODES: [&'static str; 4] = ["focus", "toggle", "always", "off"];
}

// ---------------------------------------------------------------------------
// Controller profiles (Controllers tab) — controller-profiles.toml
// ---------------------------------------------------------------------------

/// UI mirror of `controller-profiles.toml`. `BTreeMap` (not the on-disk
/// `HashMap`) so the controller list + preset list render in a stable order.
/// Keys are the canonical `"vvvv:pppp"` controller ids (lowercase hex).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControllerProfilesUi {
    /// Saved feel per controller, keyed `"vvvv:pppp"`.
    pub controllers: BTreeMap<String, ControllerProfile>,
    /// Named, reusable presets the user can apply to any controller.
    pub presets: BTreeMap<String, ControllerProfile>,
    /// Transient inline-editor buffer per controller key, keyed `"vvvv:pppp"`.
    /// Presence = the name is currently in edit mode (with `draft` as the
    /// in-progress string); absence = display the static label. Not persisted,
    /// not serialised — purely UI state.
    #[serde(skip)]
    pub alias_draft: std::collections::HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Host identity (top bar + Overview)
// ---------------------------------------------------------------------------

/// This host's identity for the top bar + Overview tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostInfo {
    pub hostname: String,
    /// Advertised NIC MACs (lowercase `aa:bb:cc:dd:ee:ff`), for WoL + binding.
    pub macs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Per-device reported geometry (Compositor tab)
// ---------------------------------------------------------------------------

/// Last device-reported stream geometry for one peer, persisted in `panel.toml`'s
/// `[devices]` table (keyed by pubkey). The panel records it from the live
/// control socket the first time it sees the device connected, so the Compositor
/// can size that device's tile and show its last resolution/Hz even when it's
/// offline. "Device-reported" in the practical sense: it's the negotiated stream
/// geometry + measured rate (devices don't report their native panel mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceGeom {
    pub w: u32,
    pub h: u32,
    /// Measured stream frame rate at capture time — stands in for refresh "Hz".
    pub fps: u32,
}

// ---------------------------------------------------------------------------
// The whole panel state
// ---------------------------------------------------------------------------

/// Everything the panel renders + edits. Loaded once via [`PanelState::load`];
/// tabs mutate fields in place and set the matching `dirty_*` flag, which the
/// app's Save flow consumes.
#[derive(Debug, Clone)]
pub struct PanelState {
    pub tab: Tab,
    pub peers: Vec<PeerInfo>,
    pub host: HostInfo,
    pub host_config: HostConfigUi,
    pub runtime: RuntimeSettings,
    pub decode_caps: Vec<CapabilityUi>,
    pub encode_caps: Vec<CapabilityUi>,
    pub controllers: ControllerProfilesUi,
    pub input_cfg: InputCfgUi,
    pub layout: CompositorLayout,
    /// Named compositor presets (display setup + positions + per-device input).
    pub presets: CompositorPresetsUi,
    /// Configured encode pixel-throughput budget (`[encode] max_pixels_per_sec`
    /// in `config.toml`). The Compositor keeps `Σ(w·h)·fps` under this.
    pub encode_budget: u64,
    /// Last device-reported stream geometry per peer pubkey (panel.toml
    /// `[devices]`). Drives Compositor tile sizing + the per-device res/Hz shown.
    pub device_geoms: std::collections::BTreeMap<String, DeviceGeom>,
    /// The saved application catalog (`apps.toml`), edited by the Apps tab.
    pub apps: app_launcher::Catalog,
    /// Transient state for the Apps tab's create/edit editor (not persisted).
    /// `None` when no editor is open.
    pub apps_editor: Option<AppEditor>,
    /// Last hard error to surface in the status bar (cleared by the user / a
    /// successful action).
    pub last_error: Option<String>,
    /// Transient success / progress note for the status bar.
    pub status: Option<String>,
    pub dirty_host: bool,
    pub dirty_runtime: bool,
    pub dirty_controllers: bool,
    pub dirty_input: bool,
    pub dirty_layout: bool,
    pub dirty_apps: bool,
    pub dirty_presets: bool,
}

/// Named compositor layout presets, managed from the Compositor tab. Each preset
/// is a full [`CompositorLayout`] snapshot — display setup, tile positions, AND
/// the per-device `input_enabled` flags. `draft_name`/`selected` are transient UI
/// buffers (not persisted). Mirrors the controller-profiles preset shape.
#[derive(Debug, Clone, Default)]
pub struct CompositorPresetsUi {
    pub presets: std::collections::BTreeMap<String, CompositorLayout>,
    /// "Save as" text buffer.
    pub draft_name: String,
    /// Currently-selected preset name in the combo box.
    pub selected: String,
}

/// Transient editor state for the Apps tab's "Create / edit application" form.
/// Runtime-only (never persisted): holds the draft entry, which existing app it
/// edits (if any), and the lazily-scanned easy-mode picker results.
#[derive(Debug, Clone)]
pub struct AppEditor {
    /// Index into [`PanelState::apps`]`.apps` being edited, or `None` for a new
    /// entry to append on save.
    pub editing: Option<usize>,
    /// The entry under edit.
    pub draft: app_launcher::AppEntry,
    /// Cached Steam scan (easy mode), `None` until the user asks to scan.
    pub steam: Option<Vec<app_launcher::SteamGame>>,
    /// Cached installed-app (.desktop) scan, `None` until asked.
    pub desktop: Option<Vec<app_launcher::DesktopApp>>,
    /// Filter text for the easy-mode picker list.
    pub picker_filter: String,
    /// Vulkan implicit layers installed on this box, scanned once when the editor
    /// opens (reading manifests every frame would hit the disk at 60Hz).
    pub vk_layers: Vec<app_launcher::LayerFamily>,
}

impl AppEditor {
    /// A fresh editor for a brand-new app.
    pub fn new() -> Self {
        AppEditor {
            editing: None,
            draft: app_launcher::AppEntry::new(),
            steam: None,
            desktop: None,
            picker_filter: String::new(),
            vk_layers: app_launcher::vklayer::scan(),
        }
    }

    /// An editor seeded to edit the entry at `index`.
    pub fn edit(index: usize, entry: app_launcher::AppEntry) -> Self {
        AppEditor {
            editing: Some(index),
            draft: entry,
            steam: None,
            desktop: None,
            picker_filter: String::new(),
            vk_layers: app_launcher::vklayer::scan(),
        }
    }
}

impl Default for AppEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl PanelState {
    /// Load everything from disk + probes. Never panics: each loader maps its
    /// own errors to defaults (and the first probe can be slow but is cached).
    pub fn load() -> Self {
        // Env overrides (handy for testing/screenshots; persisted settings win
        // otherwise): PHAROS_PANEL_TAB / _THEME / _FONT / _LANG.
        let mut runtime = crate::data::load_runtime_settings();
        if let Some(t) = std::env::var("PHAROS_PANEL_THEME")
            .ok()
            .and_then(|s| Theme::from_name(&s))
        {
            runtime.theme = t;
        }
        if let Some(f) = std::env::var("PHAROS_PANEL_FONT")
            .ok()
            .and_then(|s| FontScale::from_name(&s))
        {
            runtime.font_scale = f;
        }
        if let Ok(lang) = std::env::var("PHAROS_PANEL_LANG") {
            let lang = lang.trim();
            if !lang.is_empty() {
                runtime.language = lang.to_string();
            }
        }
        Self {
            tab: std::env::var("PHAROS_PANEL_TAB")
                .ok()
                .and_then(|s| Tab::from_name(&s))
                .unwrap_or_default(),
            peers: crate::data::load_peers(),
            host: crate::data::host_info(),
            host_config: crate::data::load_host_config(),
            runtime,
            decode_caps: crate::data::decode_capabilities(),
            encode_caps: crate::data::encode_capabilities(),
            controllers: crate::data::load_controller_profiles(),
            input_cfg: crate::data::load_input_cfg(),
            layout: crate::data::load_compositor_layout(),
            presets: crate::data::load_compositor_presets(),
            encode_budget: crate::data::load_encode_budget(),
            device_geoms: crate::data::load_device_geoms(),
            apps: app_launcher::config::load(),
            apps_editor: None,
            last_error: None,
            status: None,
            dirty_host: false,
            dirty_runtime: false,
            dirty_controllers: false,
            dirty_input: false,
            dirty_layout: false,
            dirty_apps: false,
            dirty_presets: false,
        }
    }

    /// True if any section has unsaved edits (drives the Save indicator).
    pub fn any_dirty(&self) -> bool {
        self.dirty_host
            || self.dirty_runtime
            || self.dirty_controllers
            || self.dirty_input
            || self.dirty_layout
            || self.dirty_apps
    }
}
