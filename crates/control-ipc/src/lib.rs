// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Local control + telemetry protocol between **pharos-compositor** (the server)
//! and **pharos-panel** (the client).
//!
//! The wire is **newline-delimited JSON**: the panel sends [`Request`]s, the host
//! replies with [`Event`]s. This crate is a lean leaf — `serde` + `serde_json` +
//! `dirs`, nothing heavy (no tokio, no QUIC, no `host-core`).
//!
//! Two transports share one codec: a **Unix domain socket** for the local Linux
//! compositor (`connect`/`bind`, `#[cfg(unix)]`), and **TCP** for a host reached
//! over the network — the Windows host, or a remote Linux host (`tcp_connect`/
//! `tcp_bind`, cross-platform). The newline-JSON codec (`write_*`/`read_*`) and
//! the render-loop-free [`serve_status`] server are generic and platform-neutral.

use serde::{Deserialize, Serialize};

/// The reconnecting control **client** (panel/tray bridge to a host) — shared so
/// both surfaces use one cross-platform implementation.
pub mod client;

/// Bumped on any breaking change to [`Request`]/[`Event`]. Both ends exchange it
/// in their `Hello` so a mismatched panel/compositor pair can fail loudly.
///
/// v2 added the pairing control surface (`StartPairing`/`CancelPairing`,
/// [`HostStatus::pairing`]) and host [`Request::Relaunch`]. The new
/// `HostStatus` field is `#[serde(default)]`, so a v1 compositor's status still
/// deserializes on a v2 panel (it simply reports no pairing state).
///
/// v3 added [`Request::LaunchApp`] (the app launcher). It is purely additive —
/// the server reads requests leniently, so a v2 host just replies `Error` to a
/// `LaunchApp` it doesn't know, and the bump only makes the mismatch visible in
/// the `Hello` exchange.
///
/// v4 added [`Request::SetControllerAlias`] + the [`HostStatus::controllers`]
/// liveness vector (`#[serde(default)]`, so a v3 host's status still decodes on
/// a v4 panel — it just reports an empty controller list).
///
/// Frame-timings (the panel's Compositor → Performance section) was added
/// without a version bump: [`Request::GetFrameTimings`] is a new request
/// variant that an older host answers with [`Event::Error`] via the lenient
/// reader, and [`Event::FrameTimings`] is a new event variant a panel only
/// receives when it asked. No existing wire shape changed, so the same
/// "additive-only" rule the controller liveness vector follows applies.
///
/// The issue #19 peer-fleet surface ([`Request::ForgetPeer`],
/// [`Request::SetPrimaryPeer`], [`HostStatus::primary_pubkey`]) is additive the
/// same way: old hosts answer the new requests with [`Event::Error`] via the
/// lenient reader, and the new status field is `#[serde(default)]`.
///
/// The issue #22 capability set ([`HostStatus::caps`]) is additive too:
/// `#[serde(default)]` decodes a pre-caps host as [`HostCaps::legacy`]
/// (pairing + peers only), and per-field defaults keep a partially-populated
/// caps object decoding as "absent capability = false". No bump.
pub const PROTOCOL_VERSION: u32 = 4;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Panel → compositor. Tagged on `"kind"` so the JSON reads as
/// `{"kind":"PlayMedia","path":"…","fullscreen":true}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Request {
    /// First message the panel sends; carries [`PROTOCOL_VERSION`] for the check.
    Hello { version: u32 },
    /// Ask for a one-shot [`HostStatus`] snapshot (replied as [`Event::Status`]).
    GetStatus,
    /// Play a media file, optionally pinned to a named output / fullscreen.
    PlayMedia {
        path: String,
        output: Option<String>,
        fullscreen: bool,
        /// Subtitle handling (defaults to [`Subtitles::Auto`] for older panels).
        #[serde(default)]
        subtitles: Subtitles,
        /// Audio-track selection (defaults to [`Audio::Auto`] for older panels).
        #[serde(default)]
        audio: Audio,
    },
    /// Stop whatever media is currently playing.
    StopMedia,
    /// Run the per-stage latency benchmark over `frames` samples.
    RunLatencyBench { frames: u32 },
    /// Switch the virtual-display arrangement.
    SetLayoutMode { mode: LayoutMode },
    /// Re-read the on-disk layout (`multi.toml`) and apply it.
    ReloadLayout,
    /// Turn the live preview on/off. While on, the host writes a small
    /// downscaled JPEG of the streamed output to [`preview_path`] at ~2fps; the
    /// panel displays it. The host **auto-stops after 20s** (a hard cap that
    /// bounds the GPU-readback overhead), so the panel re-sends this to keep it
    /// going. Off (the default) = zero readback overhead.
    SetPreview { enabled: bool },
    /// Open the pairing window: bring up the SPAKE2 pair listener, generate a
    /// PIN, and surface it via [`HostStatus::pairing`]. `addr` overrides the
    /// listen address (default `0.0.0.0:4432`). Idempotent: a second call while
    /// pairing is already open is a no-op `Ack`.
    StartPairing { addr: Option<String> },
    /// Close the pairing window (stop the listener); clears the active PIN.
    CancelPairing,
    /// Launch a saved application (by its `apps.toml` slug) into the virtual
    /// display. The host resolves the slug against `apps.toml`, builds the
    /// environment-aware command, and spawns it on the compositor's Wayland
    /// socket — the same path the `launch-app` CLI verb and a Trireme tile use.
    /// Replied to with an [`Event::Ack`] (`ok=true`, or `ok=false` + reason).
    LaunchApp { slug: String },
    /// Restart the host process in place (tear down + re-exec with the same
    /// args). Used to apply a new on-disk layout/encoder config or to recover a
    /// wedged host. The control socket drops while the host restarts; the panel
    /// reconnects on its own. Replied to with an [`Event::Ack`] *before* the
    /// host actually re-execs (best-effort — the panel may instead just observe
    /// the disconnect/reconnect).
    Relaunch,
    /// Set (or clear) the user-given alias for a controller. `alias = None`
    /// clears the override and falls back to the classifier label. The host
    /// updates the persisted profile, applies it to a live connection, and (on
    /// best effort — see compositor `host_input` / streamer plumbing) re-pushes
    /// the new alias to connected clients. Added in proto v4.
    SetControllerAlias {
        vendor: u16,
        product: u16,
        alias: Option<String>,
    },
    /// Fetch a one-shot per-stage [`FrameTimingsSnapshot`] from the compositor
    /// **and** implicitly subscribe the link to live instrumentation for a
    /// short TTL (the host re-arms it on each call, default 5s). Old hosts
    /// don't know this variant — the lenient reader answers with
    /// [`Event::Error`] and the link stays up. Replied to with
    /// [`Event::FrameTimings`].
    ///
    /// Added without a `PROTOCOL_VERSION` bump (additive, like the v4
    /// controller liveness vector).
    GetFrameTimings,
    /// Subscribe the link to live `HostStatus.controllers` updates for `ttl_ms`
    /// milliseconds. While the subscription is active the host rebuilds the
    /// per-controller liveness vector (~1 Hz, independent of the 4 Hz status
    /// throttle); when no subscriber is active the host sets the field empty
    /// and skips the rebuild (each rebuild walks `evdev::enumerate()` per
    /// controller, which is expensive at ~25 evdev devices × N controllers).
    ///
    /// The panel re-arms this on every render tick while its Controllers tab
    /// is mounted; once it switches away the TTL expires and the host stops
    /// paying for the rebuild. Replied to with [`Event::Ack`].
    ///
    /// Old hosts don't know this variant — the lenient reader answers with
    /// [`Event::Error`] and the link stays up. Added without a
    /// `PROTOCOL_VERSION` bump (additive, like [`Self::GetFrameTimings`]).
    SubscribeControllers { ttl_ms: u32 },
    /// Switch the host's video codec (HEVC/AV1). The host persists the choice
    /// next to `multi.toml` and restarts its encode session(s) to apply it (the
    /// stream briefly drops and the client reconnects — NVENC can't change codec
    /// in place). Replied to with an [`Event::Ack`]. Additive (no
    /// `PROTOCOL_VERSION` bump) — old hosts answer [`Event::Error`] and the link
    /// stays up.
    SetVideoCodec { codec: VideoCodec },
    /// W8: live pacing control. `auto` toggles the adaptive auto-pacer (the host
    /// self-tunes to its sustainable rate); `fps` is the ceiling (auto) or the
    /// fixed target. Applied to the RUNNING pacer with no relaunch. Replied to with
    /// [`Event::Ack`]. Additive (no `PROTOCOL_VERSION` bump). Not persisted —
    /// `multi.toml` `fps`/`fps_auto` are the defaults on restart.
    SetFps { auto: bool, fps: u32 },
    /// Live bitrate control: set the encoder's average-bitrate ceiling (kbit/s).
    /// Applied IN PLACE to the running NVENC session via reconfigure (no relaunch,
    /// no SIGSEGV — unlike codec/resolution which re-init). The network-aware
    /// controller adapts BELOW this ceiling; setting it raises the cap back. The
    /// max (capped-VBR) ceiling is derived as 1.5× by default. Replied to with
    /// [`Event::Ack`]. Additive (no `PROTOCOL_VERSION` bump). Not persisted —
    /// `multi.toml` `bitrate_kbps` is the default on restart.
    SetBitrate { kbps: u32 },
    /// Resolution ladder (Phase 2): set the TARGET coded height ceiling in px
    /// (e.g. `720`, `1080`, `1440`); `0` = Auto (native). Applied IN PLACE to the
    /// running NVENC session via dynamic-resolution reconfigure (no relaunch, no
    /// SIGSEGV — validated on the encoder envelope). The host strives to uphold
    /// this and the network-aware controller may drop to a lower rung under load,
    /// but never exceeds it. Per-output the coded width is derived from the
    /// ceiling preserving the native aspect ratio. Replied to with [`Event::Ack`].
    /// Additive (no `PROTOCOL_VERSION` bump) — old hosts answer [`Event::Error`]
    /// and the link stays up. Not persisted — Auto on restart.
    SetResolution { height: u32 },
    /// Resolution ladder (Phase 2): set the MINIMUM coded height in px the
    /// network controller may auto-downscale to (the floor of the adaptive
    /// `[min, target]` range); `0` = no floor (the lowest ladder rung). Bounded by
    /// the target ceiling and native. Once the ladder reaches this floor it stops
    /// dropping resolution and leans on bitrate/fps instead — so a weak link
    /// degrades to "min resolution + low bitrate/fps" rather than ever-smaller
    /// frames. Does NOT cap the manual [`Request::SetResolution`] target. Replied
    /// to with [`Event::Ack`]. Additive (no `PROTOCOL_VERSION` bump). Not persisted.
    SetResolutionFloor { height: u32 },
    /// Switch the host's stream fan-out mode. `mirror = true` shares one encode
    /// across every connected client (clamped to the weakest — surfaced via
    /// [`HostStatus::stream_mode`] / [`MirrorClamp`]); `false` is the default
    /// single-client (pre-empt) behavior. Replied to with an [`Event::Ack`].
    /// Additive (no `PROTOCOL_VERSION` bump) — old hosts answer [`Event::Error`]
    /// and the link stays up. See `MIRROR_EXTEND_PLAN.md`.
    SetStreamMode { mirror: bool },
    /// Enable/disable INPUT forwarding for one device (by its hex pubkey), live.
    /// The host gates that connection's input at the seat boundary. Default for
    /// any device the panel never toggles is enabled (today's behavior). Replied
    /// to with an [`Event::Ack`]. Additive (no `PROTOCOL_VERSION` bump) — old
    /// hosts answer [`Event::Error`] and the link stays up. Drives the compositor
    /// canvas's per-device input toggle (the green card).
    SetInputEnabled { pubkey: String, enabled: bool },
    /// Request the host engage/disengage HDR (10-bit Main10) output. Orthogonal to
    /// codec. The host still gates on client capability (v4 `wants_hdr`) + an
    /// app-declared HDR surface; this is the panel/operator override. Additive (no
    /// `PROTOCOL_VERSION` bump) — old hosts answer [`Event::Error`].
    SetHdrMode { enabled: bool },
    /// Forget (unpair) a peer by its hex pubkey (issue #19): the host removes it
    /// from `peers.toml` (atomic rewrite via the shared host-core helper), clears
    /// it as primary if it was, and closes any live QUIC session it holds — the
    /// pairing authorizer re-reads `peers.toml` per connection, so future
    /// connects are denied with nothing else to invalidate. Replied to with an
    /// [`Event::Ack`] (`ok=false` + reason for an unknown pubkey). Served by BOTH
    /// hosts (compositor control server + pharos-win `serve_control`). Additive
    /// (no `PROTOCOL_VERSION` bump) — old hosts answer [`Event::Error`] and the
    /// link stays up.
    ForgetPeer { pubkey: String },
    /// Set (or, with `pubkey = None`, clear) the host's PRIMARY client (issue
    /// #19). Persisted in `peers.toml` (`primary_pubkey`) and applied live:
    /// while the primary is connected, a different client's connect is refused
    /// with a distinct QUIC close code instead of pre-empting; non-primary vs
    /// non-primary keeps newest-wins. The pubkey must be a paired peer. Replied
    /// to with an [`Event::Ack`]. Served by BOTH hosts. Additive (no
    /// `PROTOCOL_VERSION` bump) — old hosts answer [`Event::Error`].
    SetPrimaryPeer { pubkey: Option<String> },
}

/// Compositor → panel. Tagged on `"kind"`; the data-bearing variants wrap a
/// single struct so the payload nests cleanly (`{"kind":"Status","Status":{…}}`).
// A protocol enum's size is dominated by its largest message by design; events
// are transient (decoded, handled, dropped), never stored in bulk, so the
// size skew clippy flags here costs nothing. Boxing `HostStatus` would touch
// every construction site across four crates for no wire or memory win.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Event {
    /// Reply to [`Request::Hello`]: the compositor's version + hostname.
    Hello { version: u32, host: String },
    /// A full host status snapshot (reply to [`Request::GetStatus`], or pushed).
    Status(HostStatus),
    /// Result of a [`Request::RunLatencyBench`].
    Latency(LatencyReport),
    /// Media playback began.
    MediaStarted { path: String },
    /// Media playback ended (stopped or finished).
    MediaStopped,
    /// Generic acknowledgement of a command, with an optional human message.
    Ack { ok: bool, msg: Option<String> },
    /// A command failed; `msg` describes why.
    Error { msg: String },
    /// Reply to [`Request::GetFrameTimings`]: a per-stage latency snapshot of
    /// the compositor's render-loop instrumentation. The compositor collects
    /// this lazily — it only does the per-stage `Instant::now()` work while a
    /// panel has recently asked (or the `PHAROS_FRAME_TIMINGS` env var is set),
    /// so when no panel is open the cost stays zero.
    FrameTimings(FrameTimingsSnapshot),
}

/// How the media player should handle subtitles, shared so the panel and the
/// compositor's `mpv` launch agree. `Auto` is mpv's default — it picks up
/// embedded tracks and same-directory sidecar files (`movie.srt`,
/// `movie.en.srt`, …); `Off` disables them; `File` forces a specific external
/// subtitle file (for when it isn't a recognised sidecar).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Subtitles {
    /// Embedded tracks + same-dir sidecars, auto-selected (mpv default).
    #[default]
    Auto,
    /// No subtitles.
    Off,
    /// Select a specific embedded subtitle track by mpv's 1-based id (`--sid`).
    Track(u32),
    /// Load this explicit external subtitle file.
    File(String),
}

/// Which audio track the media player should use. `Auto` is mpv's default
/// (first/default track); `Off` mutes by selecting no track (`--aid=no`);
/// `Track` picks a specific embedded track by mpv's 1-based id (`--aid`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Audio {
    /// Default track, auto-selected (mpv default).
    #[default]
    Auto,
    /// No audio track.
    Off,
    /// Select a specific embedded audio track by mpv's 1-based id (`--aid`).
    Track(u32),
}

/// Which video codec the host encodes with. Shared so the panel's Video tab and
/// the compositor agree on the vocabulary. `Hevc` (H.265) is the default and
/// runs everywhere; `Av1` needs an Ada-or-newer NVIDIA encoder (the 4070 has
/// it) and an AV1-capable decoder on the client; `H264` (AVC) is the
/// compatibility codec — every hardware decoder made this century plays it, at
/// ~+40% bitrate for equal quality; `Vvc` (H.266) is the experimental
/// successor — no consumer GPU encodes it, so the host falls back to SOFTWARE
/// encode via ffmpeg's `libvvenc` (CPU-heavy; realtime only at modest
/// resolutions). Serialised lowercase so it reads cleanly both in the panel's
/// JSON and in `multi.toml` (`video_codec = "av1"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    /// H.265 / HEVC — the default; broad hardware-decode support.
    #[default]
    Hevc,
    /// AV1 — ~30% smaller at equal quality; needs Ada+ encode and an AV1 decoder.
    Av1,
    /// H.264 / AVC — the universal-compatibility codec; ~40% more bitrate than
    /// HEVC for equal quality, and most AVC hardware decoders cap at 1080p60
    /// (or 4K30), so prefer it only when the client can't decode HEVC/AV1.
    #[serde(alias = "avc")]
    H264,
    /// H.266 / VVC — ~40% smaller than HEVC at equal quality, but with NO
    /// hardware encoder on any consumer GPU: the host software-encodes via
    /// ffmpeg `libvvenc`. Experimental; expect realtime only at 1080p-class
    /// resolutions on a strong CPU, and hardware decode only on the newest
    /// client SoCs (software decode elsewhere).
    #[serde(alias = "h266")]
    Vvc,
}

impl VideoCodec {
    /// Short uppercase codec abbreviation for status/display
    /// (`"HEVC"` / `"AV1"` / `"AVC"` / `"VVC"`).
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::Hevc => "HEVC",
            VideoCodec::Av1 => "AV1",
            VideoCodec::H264 => "AVC",
            VideoCodec::Vvc => "VVC",
        }
    }
    /// The ITU-T designation that pairs with the abbreviation
    /// (`"H.265"` / `"—"` / `"H.264"` / `"H.266"`). AV1 is AOM, not ITU-T,
    /// so it has no H.26x name.
    pub fn itu_name(self) -> &'static str {
        match self {
            VideoCodec::Hevc => "H.265",
            VideoCodec::Av1 => "AV1",
            VideoCodec::H264 => "H.264",
            VideoCodec::Vvc => "H.266",
        }
    }
    /// The encoder label that pairs with this codec (`"NVENC HEVC"` …). VVC has
    /// no NVENC path; it is software-encoded via ffmpeg `libvvenc`.
    pub fn encoder_label(self) -> &'static str {
        match self {
            VideoCodec::Hevc => "NVENC HEVC",
            VideoCodec::Av1 => "NVENC AV1",
            VideoCodec::H264 => "NVENC AVC",
            VideoCodec::Vvc => "vvenc VVC (software)",
        }
    }
    /// Whether the Linux host encodes this codec on NVENC (vs the software
    /// ffmpeg/libvvenc fallback path).
    pub fn is_nvenc(self) -> bool {
        !matches!(self, VideoCodec::Vvc)
    }
}

/// Virtual-display arrangement, shared so the panel and compositor agree on the
/// vocabulary (mirror one image, extend across outputs, or a single output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LayoutMode {
    /// All outputs show the same image.
    Mirror,
    /// Outputs are tiled into one larger desktop.
    Extend,
    /// Only one output is active.
    Single,
}

// ---------------------------------------------------------------------------
// Status payloads
// ---------------------------------------------------------------------------

/// A snapshot of the host's streaming state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStatus {
    pub hostname: String,
    pub streaming: bool,
    pub clients: Vec<ClientStatus>,
    pub outputs: Vec<OutputStatus>,
    pub media: Option<MediaStatus>,
    /// Live pairing-window state, if a pair listener is currently open (or has
    /// reported a recent outcome). `None` means no pairing has been started this
    /// host run. `#[serde(default)]` keeps a v1 compositor's status decodable on
    /// a v2 panel — it just reports no pairing.
    #[serde(default)]
    pub pairing: Option<PairingState>,
    /// Per-controller live state (proto v4): the union of every controller in
    /// `controller-profiles.toml` and every controller the host has observed
    /// over the current run. `#[serde(default)]` keeps a v3 compositor's status
    /// decodable on a v4 panel (it reports an empty vec).
    #[serde(default)]
    pub controllers: Vec<ControllerStatus>,
    /// The host's currently-configured video codec label (`"HEVC"` / `"AV1"`),
    /// independent of whether a client is connected — so the Video tab can mark
    /// the active codec/encoder even before a stream starts. `#[serde(default)]`
    /// keeps an older host's status decodable (it reports `None`). Additive.
    #[serde(default)]
    pub video_codec: Option<String>,
    /// True when this host's control plane is **status-only** — it answers
    /// `Hello`/`GetStatus` but `Event::Error`s every mutating request (the
    /// Windows host's render-loop-free [`serve_status`]; the full Linux
    /// compositor is controllable and leaves this `false`). `#[serde(default)]`
    /// → an older host decodes as `false` (controllable). Additive.
    ///
    /// Superseded by the per-verb [`Self::caps`] set (issue #22) — the panel now
    /// gates each control on its specific capability. Kept (and still filled by
    /// hosts) so an older panel's all-or-nothing gate keeps working.
    #[serde(default)]
    pub read_only: bool,
    /// Per-verb capability set for this host's control plane (issue #22). The
    /// panel gates each control on the specific capability it drives, with an
    /// honest disabled reason, instead of the all-or-nothing [`Self::read_only`].
    /// `#[serde(default)]` → a host that doesn't send it decodes as
    /// [`HostCaps::legacy`] (pairing + peers only). Additive, no
    /// `PROTOCOL_VERSION` bump.
    #[serde(default)]
    pub caps: HostCaps,
    /// GPU/encoder health, so the tray + panel can surface a fault without the
    /// user reading logs. Set by the compositor's encode classifier / watchdog
    /// (see `GPU_ERROR_HANDLING_PLAN.md`). `#[serde(default)]` → an older host
    /// decodes as [`GpuHealth::Healthy`]. Additive, no `PROTOCOL_VERSION` bump.
    #[serde(default)]
    pub gpu_health: GpuHealth,
    /// How the host fans the encoded stream to clients (see [`StreamMode`]).
    /// `#[serde(default)]` → an older host decodes as [`StreamMode::Single`].
    /// Additive, no `PROTOCOL_VERSION` bump. See `MIRROR_EXTEND_PLAN.md`.
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Active HDR label when the host is emitting an HDR stream (e.g.
    /// `Some("HDR10")`), else `None`. Set once a client negotiated HDR (v4
    /// `wants_hdr`) and the host engaged a 10-bit Main10 session. `#[serde(default)]`
    /// keeps an older host's status decodable; additive, no `PROTOCOL_VERSION` bump.
    #[serde(default)]
    pub hdr_active: Option<String>,
    /// The hex pubkey of the host's PRIMARY client (issue #19), or `None` when
    /// no primary is set. What the host is *enforcing right now* (its transport
    /// pre-emption policy), so the panel's star toggle reflects live truth
    /// rather than a possibly-stale `peers.toml` read. `#[serde(default)]`
    /// keeps an older host's status decodable; additive, no `PROTOCOL_VERSION`
    /// bump.
    #[serde(default)]
    pub primary_pubkey: Option<String>,
}

/// What this host's control plane can actually DO, advertised per verb so the
/// panel gates each control on the specific capability it drives (issue #22)
/// rather than the all-or-nothing [`HostStatus::read_only`] flag — which, once
/// the Windows host grew pairing, enabled every control against it and left
/// most of them clickable-but-bouncing.
///
/// Additive over the wire, twice over:
///
/// * A status with **no `caps` key at all** (any host predating this struct)
///   decodes via the `#[serde(default)]` on [`HostStatus::caps`] as
///   [`HostCaps::legacy`] — pairing + peers only. That matches what the old
///   Windows host actually serves; a pre-caps *Linux* compositor connecting to
///   a newer panel degrades to the same conservative set (honestly disabled
///   controls) rather than the panel guessing at verbs it can't confirm.
/// * A caps object **missing one field** (a host predating a newly added
///   capability) decodes that field as `false` via the per-field
///   `#[serde(default)]` — a host never gains a capability by omission.
///
/// No `PROTOCOL_VERSION` bump (the same additive pattern as `gpu_health` /
/// `stream_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCaps {
    /// [`Request::StartPairing`] / [`Request::CancelPairing`].
    #[serde(default)]
    pub pairing: bool,
    /// The issue #19 fleet controls: [`Request::ForgetPeer`] /
    /// [`Request::SetPrimaryPeer`].
    #[serde(default)]
    pub peers: bool,
    /// [`Request::LaunchApp`] (the Apps tab's Launch button).
    #[serde(default)]
    pub launch_app: bool,
    /// [`Request::SetStreamMode`] (the mirror fan-out toggle).
    #[serde(default)]
    pub stream_mode: bool,
    /// [`Request::SetInputEnabled`] (the per-device input gate).
    #[serde(default)]
    pub input_toggle: bool,
    /// [`Request::SetPreview`] (the live preview JPEG).
    #[serde(default)]
    pub preview: bool,
    /// [`Request::Relaunch`] (restart the host in place).
    #[serde(default)]
    pub relaunch: bool,
    /// The controller plane: [`Request::SetControllerAlias`] /
    /// [`Request::SubscribeControllers`].
    #[serde(default)]
    pub controllers: bool,
    /// [`Request::GetFrameTimings`] (the Performance section's live
    /// instrumentation).
    #[serde(default)]
    pub frame_timings: bool,
}

impl HostCaps {
    /// What a host that predates the caps field is assumed to serve: pairing +
    /// peers only — the old Windows host's real surface, and the conservative
    /// reading of any other pre-caps host. This is the decode default for a
    /// status with no `caps` key.
    pub fn legacy() -> Self {
        HostCaps {
            pairing: true,
            peers: true,
            ..HostCaps::none()
        }
    }

    /// Every capability — the full Linux compositor.
    pub fn all() -> Self {
        HostCaps {
            pairing: true,
            peers: true,
            launch_app: true,
            stream_mode: true,
            input_toggle: true,
            preview: true,
            relaunch: true,
            controllers: true,
            frame_timings: true,
        }
    }

    /// No capabilities — a pure status publisher ([`serve_status`]).
    pub fn none() -> Self {
        HostCaps {
            pairing: false,
            peers: false,
            launch_app: false,
            stream_mode: false,
            input_toggle: false,
            preview: false,
            relaunch: false,
            controllers: false,
            frame_timings: false,
        }
    }
}

impl Default for HostCaps {
    /// The decode default for a missing `caps` key is [`HostCaps::legacy`] —
    /// see the struct docs for why that is NOT `none()`.
    fn default() -> Self {
        HostCaps::legacy()
    }
}

/// How the host fans the encoded stream out to connected clients.
///
/// `Single` is today's behavior (one client at a time; a newcomer pre-empts)
/// and the validated default — full per-client adaptive bitrate/fps. `Mirror`
/// shares ONE encode across every connected client, which means the stream is
/// clamped to the **lowest-common-denominator** client (resolution + fps +
/// bitrate). The panel surfaces that throttle from [`MirrorClamp`]. `Extend`
/// (N independent per-output streams) is reserved for later.
///
/// Additive over the wire (`#[serde(default)]` on [`HostStatus::stream_mode`]
/// decodes an older host as `Single`). See `MIRROR_EXTEND_PLAN.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StreamMode {
    /// One client at a time, pre-empting newcomers. Full per-client adaptation.
    #[default]
    Single,
    /// One shared encode fanned out to all clients, clamped to the weakest one.
    Mirror {
        /// What the shared encode is currently clamped to, and why. Boxed so the
        /// `Mirror` variant stays small (keeps `Event::Status` under clippy's
        /// `large_enum_variant` bar); serde derefs the `Box`, so the wire format
        /// is identical to an inline `MirrorClamp`.
        clamp: Box<MirrorClamp>,
    },
}

/// What a mirror encode is clamped to, so the panel can show "quality limited"
/// with the real numbers and (when known) the client that set the floor.
///
/// The values are what the host is *actually encoding* (the single shared
/// session). `client_count < 2` means mirror is enabled but no throttle is in
/// effect yet (the clamp only bites once a second, weaker client connects).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorClamp {
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    /// Encoder average-bitrate ceiling, kbit/s.
    pub bitrate_kbps: u32,
    /// Friendly name / addr of the client that forced the clamp (the weakest),
    /// or `None` when nothing is limiting it (e.g. fewer than two clients).
    #[serde(default)]
    pub limited_by: Option<String>,
    /// How many clients currently share this encode.
    pub client_count: u32,
}

/// GPU/encoder health published by the host. Additive over the wire: the
/// `#[serde(default)]` on [`HostStatus::gpu_health`] decodes an older host (or
/// any peer that omits it) as [`GpuHealth::Healthy`], so no `PROTOCOL_VERSION`
/// bump is needed (the same pattern as `video_codec` / `read_only`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum GpuHealth {
    /// Encoding normally.
    #[default]
    Healthy,
    /// Still encoding but suspect (recurring soft faults, or a self-clearing
    /// thermal/power throttle); the host has backed quality down. Surfaced amber.
    Degraded {
        /// Short human-readable cause.
        reason: String,
        /// The throttle reason (thermal/power) when that's the cause, else `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        throttle: Option<String>,
    },
    /// The GPU faulted; the host is restarting or has stopped. Surfaced sticky red.
    Faulted {
        /// Short human-readable cause.
        reason: String,
        /// The kernel Xid that named the fault, if one was captured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_xid: Option<u32>,
    },
}

/// A GPU fault persisted to a small file so the tray can show "faulted & gave up"
/// even after the host process exits and the control plane is gone — distinct
/// from "never started". Best-effort: written on a FATAL fault, cleared on a
/// clean host start. Lives in `$XDG_RUNTIME_DIR` (tmpfs → cleared on reboot,
/// which is the right lifetime for a "cold boot required" marker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuFaultRecord {
    /// Short human-readable cause.
    pub reason: String,
    /// The kernel Xid that named it, if any.
    #[serde(default)]
    pub last_xid: Option<u32>,
    /// True when a restart can't fix it (e.g. Xid 79/62) — "cold boot required".
    #[serde(default)]
    pub terminal: bool,
    /// UNIX-ms when the fault was recorded.
    #[serde(default)]
    pub unix_ms: i64,
}

/// Path to the durable GPU-fault file (`$XDG_RUNTIME_DIR/pharos-gpu-fault.json`);
/// `None` if `$XDG_RUNTIME_DIR` isn't set (e.g. non-Linux).
pub fn gpu_fault_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(std::path::PathBuf::from(dir).join("pharos-gpu-fault.json"))
}

/// Persist a GPU fault. Best-effort — a write failure must NEVER become a new
/// fatal path, so every error is swallowed.
pub fn write_gpu_fault(rec: &GpuFaultRecord) {
    if let Some(path) = gpu_fault_path() {
        if let Ok(json) = serde_json::to_vec(rec) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Read the durable GPU fault, if one is present (best-effort).
pub fn read_gpu_fault() -> Option<GpuFaultRecord> {
    let bytes = std::fs::read(gpu_fault_path()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Clear the durable GPU fault (best-effort) — called on a clean host start.
pub fn clear_gpu_fault() {
    if let Some(path) = gpu_fault_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// The target encode-envelope resolution carried across a host restart (Phase 3c,
/// restart-into-panel-res). When a client connects with a panel larger than the
/// running encode envelope, the compositor records the panel size here and exits
/// for a clean systemd restart; the fresh process reads it at startup and builds
/// the primary output at (at least) this size — a fresh NVENC init is crash-safe
/// where an in-process re-init SIGSEGVs the driver. Lives in `$XDG_RUNTIME_DIR`
/// (tmpfs → cleared on reboot, so a cold boot resets to the configured size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamResRecord {
    pub w: u32,
    pub h: u32,
}

/// Path to the stream-res file (`$XDG_RUNTIME_DIR/pharos-stream-res.json`);
/// `None` if `$XDG_RUNTIME_DIR` isn't set (e.g. non-Linux).
pub fn stream_res_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(std::path::PathBuf::from(dir).join("pharos-stream-res.json"))
}

/// Persist the target encode envelope. Returns `true` only when the size was
/// genuinely written to disk (path resolved AND the write succeeded). The caller
/// MUST gate a restart on this: if the persist is a no-op (e.g. `$XDG_RUNTIME_DIR`
/// unset) or fails, restarting would rebuild at the configured size and the same
/// oversized client would re-trigger the grow forever — an infinite restart loop
/// that trips the systemd StartLimit breaker. On `false`, don't restart; the
/// client just rides the existing (smaller) envelope via the downscale ladder.
#[must_use]
pub fn write_stream_res(w: u32, h: u32) -> bool {
    let Some(path) = stream_res_path() else {
        return false;
    };
    let Ok(json) = serde_json::to_vec(&StreamResRecord { w, h }) else {
        return false;
    };
    std::fs::write(path, json).is_ok()
}

/// Read the target encode envelope, if one was recorded (best-effort).
pub fn read_stream_res() -> Option<(u32, u32)> {
    let bytes = std::fs::read(stream_res_path()?).ok()?;
    let rec: StreamResRecord = serde_json::from_slice(&bytes).ok()?;
    Some((rec.w, rec.h))
}

/// Remove the stream-res file (best-effort) — called once the host has
/// successfully started at the grown size, so the file's lifetime is bounded to
/// the single restart hop it's meant for. Without this, a size grown for one
/// client persists for the whole boot (tmpfs), inflating the envelope even after
/// the user shrinks their configured resolution.
pub fn clear_stream_res() {
    if let Some(path) = stream_res_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ── "Start on boot" autostart entry ─────────────────────────────────────────
// A freedesktop autostart .desktop that runs the host launcher at login. Shared
// state read/written by both the panel (Settings toggle) and the tray (menu
// checkbox), so they always agree.

/// Path to the user autostart entry (`$XDG_CONFIG_HOME/autostart/
/// pharos-host.desktop`, default `~/.config/autostart/...`); `None` if no config
/// dir can be resolved.
pub fn autostart_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("autostart").join("pharos-host.desktop"))
}

/// True if "start Pharos at login" is enabled (the autostart entry exists).
pub fn autostart_enabled() -> bool {
    autostart_path().is_some_and(|p| p.exists())
}

/// Enable/disable "start at login". When enabling, `exec` is the command the
/// autostart entry runs (the host launcher). Returns the error string on failure
/// so the caller can surface it.
pub fn set_autostart(enabled: bool, exec: &str) -> Result<(), String> {
    let path = autostart_path().ok_or("no XDG config dir")?;
    if enabled {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let desktop = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Pharos Host\n\
             Comment=Start the Pharos streaming host at login\n\
             Exec={exec}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(&path, desktop).map_err(|e| e.to_string())
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Per-controller liveness + alias for the panel's Controllers tab (proto v4).
///
/// The panel joins this with the on-disk profiles it already loads — the
/// liveness fields drive the "Connected" dot + the sort, while [`Self::alias`]
/// is the source of truth for the display name (so renaming on the panel
/// reflects in real time on every other panel without a re-load).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerStatus {
    pub vendor: u16,
    pub product: u16,
    /// User-given display name. `None` = fall back to the classifier label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// True while a client is reporting this controller as attached.
    pub connected: bool,
    /// UNIX-ms timestamp of the most recent `connected = true` transition.
    /// `None` if the host has never seen the controller this run (i.e. the
    /// entry comes from the on-disk profile only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_ms: Option<i64>,
    /// Which transport bus the host saw this controller arrive on — used by
    /// the panel to render a USB / BT pill next to the name. Backward-
    /// compatible additive (no protocol-version bump): `#[serde(default)]`
    /// fills [`ControllerBus::Unknown`] on a v4 host's status that doesn't
    /// carry the field yet.
    #[serde(default)]
    pub bus: ControllerBus,
}

/// Transport bus a controller arrived on, as reported by the host. Mirrors
/// the subset of evdev `BusType`s a pad can show up under (`0x03` USB and
/// `0x05` Bluetooth — the only two physical paths a gamepad reaches a Pharos
/// host on); anything else (incl. "no live evdev for this VID:PID") collapses
/// to [`ControllerBus::Unknown`] so the panel can simply not render a pill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerBus {
    /// Couldn't classify (no matching evdev present, or the host build can't
    /// probe). The panel hides the pill on this value.
    #[default]
    Unknown,
    /// `BUS_USB` (`0x03`) — wired or USB/IP-forwarded.
    Usb,
    /// `BUS_BLUETOOTH` (`0x05`) — paired BT pad.
    Bluetooth,
}

/// Live state of the host's pairing window, polled by the panel via
/// [`Request::GetStatus`] (and pushed in the immediate reply to
/// [`Request::StartPairing`]). The PIN is shown for the whole pairing round and
/// rotates after each handshake, so a ~1 Hz poll is plenty to keep the panel's
/// inline pairing card live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingState {
    /// True while the listener is up and accepting devices.
    pub active: bool,
    /// The address the pair listener is bound to (e.g. `0.0.0.0:4432`).
    pub addr: String,
    /// The current six-digit PIN to enter on the Trireme device. `None` only in
    /// the brief window before the first PIN is generated.
    pub pin: Option<String>,
    /// The most recent terminal outcome of a pairing attempt, retained so the
    /// card can show "Paired with X ✓" or a failure reason while the listener
    /// stays open for the next device.
    #[serde(default)]
    pub last: Option<PairOutcome>,
    /// True when this is the host's always-open, host-lifetime pairing listener
    /// (issue #17): it is a permanent host property that a panel Cancel cannot
    /// tear down, so the panel shows an informational "pairing is always on"
    /// state instead of a misleading Cancel button. `#[serde(default)]` keeps
    /// older hosts (which never set it) deserializing as a normal, cancellable
    /// window.
    #[serde(default)]
    pub always_on: bool,
}

/// The outcome of one pairing attempt, surfaced in [`PairingState::last`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PairOutcome {
    /// A device connected and the handshake is in progress.
    Attempt { peer: String },
    /// A device finished pairing; it is now in `peers.toml`.
    Paired {
        pubkey: String,
        alias: Option<String>,
    },
    /// A pairing attempt failed (wrong PIN, protocol mismatch, hang-up, …).
    Failed { reason: String },
}

/// One connected (or known) Trireme client and its live encode stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientStatus {
    pub addr: Option<String>,
    pub pubkey: Option<String>,
    pub connected: bool,
    /// e.g. `"NVENC HEVC"`.
    pub encoder: String,
    /// e.g. `"HEVC"`.
    pub codec: String,
    pub fps: u32,
    pub frames_sent: u64,
    /// Resolution ladder (Phase 2): the CODED resolution currently streamed to
    /// this client. Below the output's native size when the ladder has downscaled
    /// (user-selected ceiling or auto-drop under load). `0,0` if unknown.
    #[serde(default)]
    pub coded_w: u32,
    #[serde(default)]
    pub coded_h: u32,
    /// The selected target ceiling height in px; `0` = Auto (native). With
    /// `coded_h` this distinguishes a user cap (`coded_h == ceiling < native`)
    /// from an auto-drop under load (`coded_h < ceiling`).
    #[serde(default)]
    pub res_ceiling_h: u32,
    /// The selected minimum coded height in px the ladder may auto-drop to; `0` =
    /// no floor (lowest rung). The adaptive range is `[res_floor_h, res_ceiling_h]`.
    #[serde(default)]
    pub res_floor_h: u32,
}

/// One virtual output (display tile) the compositor is driving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputStatus {
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub x: i32,
    pub y: i32,
    pub addr: String,
    pub connected: bool,
}

/// Currently-loaded media and whether it's playing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStatus {
    pub path: String,
    pub playing: bool,
}

/// Per-stage latency + throughput snapshot of the compositor's render loop,
/// produced live (no flush) by [`Request::GetFrameTimings`].
///
/// The stage list mirrors the eight `stage.mark()` checkpoints in the
/// compositor's main loop: `dispatch` (Wayland event-loop pump), `ctrl` (panel
/// control-command drain), `input` (host input fanout), `gyro` (IMU
/// processing), `render` (composite + scanout), `post` (per-output post),
/// `pace` (frame-rate sleep), `status` (HostStatus refresh). When the
/// instrumentation gate is currently off — the env var is unset and nobody is
/// subscribed — every stage reports zero samples and the panel renders empty
/// rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameTimingsSnapshot {
    /// Number of frames in the live (in-progress) histogram window. A value
    /// of zero means instrumentation is off (or has just rolled over after a
    /// flush — the next poll picks up data).
    pub window_frames: u64,
    pub dispatch: StageStats,
    pub ctrl: StageStats,
    pub input: StageStats,
    pub gyro: StageStats,
    pub render: StageStats,
    pub post: StageStats,
    pub pace: StageStats,
    pub status: StageStats,
}

/// Per-stage mean / p95 / max latency in microseconds + sample count. Zero
/// across the board when the gate is off or the stage has not been hit yet
/// in the current window.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct StageStats {
    pub mean_us: u32,
    pub p95_us: u32,
    pub max_us: u32,
    pub samples: u32,
}

/// Per-step latency benchmark result the panel renders (all stages in **ms**).
///
/// Mirrors `core_proto::WireLatencyReport` field-for-field, except the codec is a
/// human string here instead of a four-CC. `compose_ms`/`encode_ms`/`host_send_ms`
/// are the host stages; `nexus_ms` is the pure wire/router time (`rtt_ms / 2`),
/// with `nexus_uncertainty_ms` the RTT-spread half-width; `buffer_ms` is the
/// in-flight/jitter residency at the decoder, `decode_ms` the (minimum) pure
/// decode, and `present_ms` the present stage. `clock_offset_ms` is **diagnostic
/// only** — no stage is derived from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyReport {
    pub codec: String,
    pub samples: u32,
    pub rtt_ms: f32,
    pub clock_offset_ms: f32,
    pub compose_ms: f32,
    pub encode_ms: f32,
    pub host_send_ms: f32,
    pub nexus_ms: f32,
    pub nexus_uncertainty_ms: f32,
    pub buffer_ms: f32,
    pub decode_ms: f32,
    pub present_ms: f32,
    pub total_ms: f32,
}

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// Where the control socket lives.
///
/// Resolution order: `$PHAROS_CONTROL_SOCK`, else `<runtime dir>/pharos/control.sock`
/// (e.g. `$XDG_RUNTIME_DIR/pharos/…`), else `<config dir>/pharos/control.sock`,
/// else `/tmp/pharos/control.sock`.
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PHAROS_CONTROL_SOCK") {
        return std::path::PathBuf::from(p);
    }
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join("pharos").join("control.sock");
    }
    if let Some(dir) = dirs::config_dir() {
        return dir.join("pharos").join("control.sock");
    }
    std::path::PathBuf::from("/tmp/pharos/control.sock")
}

/// Where the live-preview JPEG is written by the host and read by the panel.
/// Sits next to the control socket (same runtime dir), so both ends — which run
/// on the same machine — resolve it identically. `$PHAROS_PREVIEW_PATH`
/// overrides.
pub fn preview_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PHAROS_PREVIEW_PATH") {
        return std::path::PathBuf::from(p);
    }
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join("pharos").join("preview.jpg");
    }
    if let Some(dir) = dirs::config_dir() {
        return dir.join("pharos").join("preview.jpg");
    }
    std::path::PathBuf::from("/tmp/pharos/preview.jpg")
}

// ---------------------------------------------------------------------------
// Transport + I/O helpers (newline-delimited JSON)
// ---------------------------------------------------------------------------
//
// The newline-JSON CODEC (write_*/read_*) is generic over Write/BufRead and
// cross-platform — both the local Unix-domain-socket plane and the TCP plane
// (Windows host / remote Linux host) use it verbatim. Only the UDS transport
// (connect/bind) is `#[cfg(unix)]`; TCP works everywhere.

/// Connect to the **local** control socket at [`socket_path`] (Unix only).
#[cfg(unix)]
pub fn connect() -> std::io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(socket_path())
}

/// Bind the **local** control socket: create the parent dir, remove any stale
/// socket file, then bind a fresh listener at [`socket_path`] (Unix only).
#[cfg(unix)]
pub fn bind() -> std::io::Result<std::os::unix::net::UnixListener> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A leftover socket file from a previous run would make bind() fail with
    // EADDRINUSE; clear it (ignore "not found").
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::os::unix::net::UnixListener::bind(&path)
}

/// Connect to a host's **TCP** control plane (the Windows host, or a remote
/// Linux host). The panel dials this when pointed at a non-local host. The
/// default port is `core_proto::ports::CONTROL` (the caller builds the addr).
pub fn tcp_connect(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpStream> {
    std::net::TcpStream::connect(addr)
}

/// Bind the **TCP** control plane for a host reached over the network. Bind to
/// `127.0.0.1:<CONTROL>` for SSH-tunnel-only access, or `0.0.0.0:<CONTROL>` to
/// accept LAN panels directly.
pub fn tcp_bind(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpListener> {
    std::net::TcpListener::bind(addr)
}

/// Write one [`Request`] as a JSON line (`{…}\n`).
pub fn write_request<W: std::io::Write>(w: &mut W, req: &Request) -> std::io::Result<()> {
    write_json_line(w, req)
}

/// Write one [`Event`] as a JSON line (`{…}\n`).
pub fn write_event<W: std::io::Write>(w: &mut W, ev: &Event) -> std::io::Result<()> {
    write_json_line(w, ev)
}

/// Read one [`Request`] line. `Ok(None)` on EOF (peer closed cleanly).
pub fn read_request<R: std::io::BufRead>(r: &mut R) -> std::io::Result<Option<Request>> {
    read_json_line(r)
}

/// Read one [`Request`] line **leniently**: `Ok(None)` on EOF,
/// `Ok(Some(Ok(req)))` for a valid request, and `Ok(Some(Err(msg)))` for a
/// malformed or unknown-variant line. Only genuine I/O errors return `Err`.
///
/// A server uses this so a *newer* panel's unknown request (an
/// [`Request`] variant added after this build) is answered with an
/// [`Event::Error`] and the link stays up, instead of a deserialize failure
/// tearing the whole connection down — which would otherwise read on the panel
/// as a silent reconnect blip with the action quietly lost.
pub fn read_request_lenient<R: std::io::BufRead>(
    r: &mut R,
) -> std::io::Result<Option<Result<Request, String>>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None); // EOF
    }
    Ok(Some(
        serde_json::from_str(line.trim_end()).map_err(|e| e.to_string()),
    ))
}

/// Read one [`Event`] line. `Ok(None)` on EOF (peer closed cleanly).
pub fn read_event<R: std::io::BufRead>(r: &mut R) -> std::io::Result<Option<Event>> {
    read_json_line(r)
}

fn write_json_line<W: std::io::Write, T: Serialize>(w: &mut W, val: &T) -> std::io::Result<()> {
    let line = serde_json::to_string(val).map_err(std::io::Error::from)?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

fn read_json_line<R: std::io::BufRead, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None); // EOF
    }
    let val = serde_json::from_str(line.trim_end()).map_err(std::io::Error::from)?;
    Ok(Some(val))
}

// ---------------------------------------------------------------------------
// Render-loop-free TCP status server
// ---------------------------------------------------------------------------

/// Max simultaneous control connections [`serve_status`] will serve. Past this,
/// new connections are dropped (the client sees EOF) so a leak can't exhaust
/// threads. Mirrors the compositor's `MAX_CONTROL_CLIENTS`.
const MAX_STATUS_CLIENTS: usize = 8;
/// Per-connection write timeout — reaps a client that stops reading.
const STATUS_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Decrements the active-connection counter on any exit (incl. panic).
struct ConnGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Serve `Hello` + `GetStatus` over a bound TCP listener, replying with the
/// latest snapshot read from `status` on each `GetStatus`. Every other
/// (actionable) request is answered with [`Event::Error`] so a newer panel
/// degrades gracefully instead of tearing the link down. Spawns a detached
/// accept thread — one serve thread per connection, capped at
/// [`MAX_STATUS_CLIENTS`] — and returns immediately.
///
/// This is the render-loop-free control server: a host with no Wayland render
/// loop (the Windows host, or any status-only publisher) keeps `status` fresh
/// from its own state and lets the panel poll it. The Linux compositor uses its
/// own richer server, which also marshals mutations onto the render loop.
pub fn serve_status(
    listener: std::net::TcpListener,
    host_name: String,
    status: std::sync::Arc<std::sync::Mutex<HostStatus>>,
) {
    // No actionable handler: every request past Hello/GetStatus is rejected,
    // preserving the original status-only contract.
    serve_control(listener, host_name, status, |_req| Event::Error {
        msg: "this host's control plane only supports Hello/GetStatus".into(),
    });
}

/// Like [`serve_status`], but requests other than `Hello`/`GetStatus` are
/// dispatched to `handler` instead of rejected. `handler` runs on the
/// per-connection serve thread and returns the [`Event`] to reply with.
///
/// This is how the render-loop-free host drives *actionable* control (pairing,
/// etc.) without giving up the transport-generic accept loop. The handler is
/// injected rather than referenced directly because the real dispatch target —
/// `host_core::pairing::PairingManager` — lives in `host-core`, which depends on
/// this crate; taking a closure keeps `control-ipc` free of that back-edge.
pub fn serve_control<H>(
    listener: std::net::TcpListener,
    host_name: String,
    status: std::sync::Arc<std::sync::Mutex<HostStatus>>,
    handler: H,
) where
    H: Fn(Request) -> Event + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    let _ = std::thread::Builder::new()
        .name("pharos-control".into())
        .spawn(move || status_accept_loop(listener, host_name, status, handler));
}

fn status_accept_loop<H>(
    listener: std::net::TcpListener,
    host_name: String,
    status: std::sync::Arc<std::sync::Mutex<HostStatus>>,
    handler: std::sync::Arc<H>,
) where
    H: Fn(Request) -> Event + Send + Sync + 'static,
{
    use std::sync::atomic::{AtomicUsize, Ordering};
    let active = std::sync::Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.load(Ordering::Relaxed) >= MAX_STATUS_CLIENTS {
            drop(stream); // over cap → EOF to the client; don't pile up threads
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let guard = ConnGuard(active.clone());
        let host_name = host_name.clone();
        let status = status.clone();
        let handler = handler.clone();
        let _ = std::thread::Builder::new()
            .name("pharos-control-conn".into())
            .spawn(move || {
                let _guard = guard;
                let _ = serve_status_conn(stream, &host_name, &status, &*handler);
            });
    }
}

fn serve_status_conn<H>(
    stream: std::net::TcpStream,
    host_name: &str,
    status: &std::sync::Mutex<HostStatus>,
    handler: &H,
) -> std::io::Result<()>
where
    H: Fn(Request) -> Event,
{
    stream.set_write_timeout(Some(STATUS_WRITE_TIMEOUT)).ok();
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let ev = match read_request_lenient(&mut reader)? {
            None => return Ok(()), // EOF — peer closed cleanly
            Some(Ok(req)) => handle_status_request(req, host_name, status, handler),
            Some(Err(msg)) => Event::Error {
                msg: format!("malformed request: {msg}"),
            },
        };
        write_event(&mut writer, &ev)?;
    }
}

fn handle_status_request<H>(
    req: Request,
    host_name: &str,
    status: &std::sync::Mutex<HostStatus>,
    handler: &H,
) -> Event
where
    H: Fn(Request) -> Event,
{
    match req {
        Request::Hello { .. } => Event::Hello {
            version: PROTOCOL_VERSION,
            host: host_name.to_string(),
        },
        // Recover the guard even if a serve thread panicked while holding it —
        // a poisoned status mutex must not take the whole control plane down.
        Request::GetStatus => {
            Event::Status(status.lock().unwrap_or_else(|e| e.into_inner()).clone())
        }
        // Actionable requests go to the injected handler (a no-op rejecter for
        // plain serve_status; the real pairing dispatch on the Windows host).
        other => handler(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request::PlayMedia {
            path: "/srv/clip.mkv".into(),
            output: Some("HDMI-A-1".into()),
            fullscreen: true,
            subtitles: Subtitles::File("/srv/clip.en.srt".into()),
            audio: Audio::Track(2),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        // Round-trips through JSON identically.
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    fn sample_status(hostname: &str) -> HostStatus {
        HostStatus {
            hostname: hostname.into(),
            streaming: true,
            clients: vec![],
            outputs: vec![],
            media: None,
            pairing: None,
            controllers: vec![],
            video_codec: Some("HEVC".into()),
            read_only: false,
            caps: HostCaps::all(),
            gpu_health: GpuHealth::default(),
            stream_mode: StreamMode::default(),
            hdr_active: None,
            primary_pubkey: None,
        }
    }

    #[test]
    fn gpu_health_additive_and_roundtrips() {
        // An older host's status JSON has no `gpu_health` → it must decode as
        // Healthy (the additive guarantee that lets us skip a PROTOCOL_VERSION
        // bump, exactly like `video_codec` / `read_only`).
        let s = sample_status("rig");
        let mut json = serde_json::to_value(&s).unwrap();
        json.as_object_mut().unwrap().remove("gpu_health");
        let decoded: HostStatus = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.gpu_health, GpuHealth::Healthy);

        // A Faulted state round-trips intact through the JSON codec.
        let faulted = GpuHealth::Faulted {
            reason: "device lost".into(),
            last_xid: Some(79),
        };
        let mut s2 = sample_status("rig");
        s2.gpu_health = faulted.clone();
        let back: HostStatus = serde_json::from_str(&serde_json::to_string(&s2).unwrap()).unwrap();
        assert_eq!(back.gpu_health, faulted);
    }

    #[test]
    fn codec_roundtrip_over_buffer() {
        // The newline-JSON codec (now cross-platform, was #[cfg(unix)]) writes
        // requests and reads them back over an in-memory buffer — the exact path
        // the TCP/Windows server uses, proven without any socket.
        let mut buf: Vec<u8> = Vec::new();
        write_request(
            &mut buf,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        write_request(&mut buf, &Request::GetStatus).unwrap();
        let mut r = std::io::BufReader::new(std::io::Cursor::new(buf));
        assert!(matches!(
            read_request(&mut r).unwrap(),
            Some(Request::Hello { version }) if version == PROTOCOL_VERSION
        ));
        assert!(matches!(
            read_request(&mut r).unwrap(),
            Some(Request::GetStatus)
        ));
        assert!(read_request(&mut r).unwrap().is_none()); // EOF
    }

    #[test]
    fn serve_status_answers_hello_getstatus_and_rejects_actionable() {
        use std::io::BufReader;
        use std::sync::{Arc, Mutex};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        serve_status(
            listener,
            "winrig".into(),
            Arc::new(Mutex::new(sample_status("winrig"))),
        );

        let stream = tcp_connect(addr).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        write_request(
            &mut writer,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        match read_event(&mut reader).unwrap() {
            Some(Event::Hello { version, host }) => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert_eq!(host, "winrig");
            }
            other => panic!("expected Hello, got {other:?}"),
        }

        write_request(&mut writer, &Request::GetStatus).unwrap();
        match read_event(&mut reader).unwrap() {
            Some(Event::Status(s)) => {
                assert_eq!(s.hostname, "winrig");
                assert!(s.streaming);
                assert_eq!(s.video_codec.as_deref(), Some("HEVC"));
            }
            other => panic!("expected Status, got {other:?}"),
        }

        // An actionable request a render-loop-free host can't do → clean Error,
        // link stays up (the cross-version-degradation contract).
        write_request(&mut writer, &Request::StopMedia).unwrap();
        assert!(matches!(
            read_event(&mut reader).unwrap(),
            Some(Event::Error { .. })
        ));
    }

    #[test]
    fn serve_control_dispatches_actionable_to_handler() {
        use std::io::BufReader;
        use std::sync::{Arc, Mutex};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Handler that only the Windows host would install: it fields the
        // actionable request (here StartPairing) and never sees Hello/GetStatus,
        // which serve_control answers centrally.
        serve_control(
            listener,
            "winrig".into(),
            Arc::new(Mutex::new(sample_status("winrig"))),
            |req| match req {
                Request::StartPairing { addr } => Event::Ack {
                    ok: true,
                    msg: Some(format!("pairing:{}", addr.unwrap_or_default())),
                },
                other => Event::Error {
                    msg: format!("unhandled {other:?}"),
                },
            },
        );

        let stream = tcp_connect(addr).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        // Hello/GetStatus are still answered centrally, not by the handler.
        write_request(
            &mut writer,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        assert!(matches!(
            read_event(&mut reader).unwrap(),
            Some(Event::Hello { .. })
        ));

        // The actionable request reaches the handler and its Event is returned.
        write_request(
            &mut writer,
            &Request::StartPairing {
                addr: Some("0.0.0.0:4432".into()),
            },
        )
        .unwrap();
        match read_event(&mut reader).unwrap() {
            Some(Event::Ack { ok, msg }) => {
                assert!(ok);
                assert_eq!(msg.as_deref(), Some("pairing:0.0.0.0:4432"));
            }
            other => panic!("expected handler Ack, got {other:?}"),
        }
    }

    #[test]
    fn event_roundtrip() {
        let ev = Event::Status(HostStatus {
            hostname: "pharos".into(),
            streaming: true,
            clients: vec![ClientStatus {
                addr: Some("192.168.1.122:48010".into()),
                pubkey: Some("abcd".into()),
                connected: true,
                encoder: "NVENC HEVC".into(),
                codec: "HEVC".into(),
                fps: 120,
                frames_sent: 4096,
                coded_w: 1920,
                coded_h: 1080,
                res_ceiling_h: 0,
                res_floor_h: 0,
            }],
            outputs: vec![OutputStatus {
                name: "virtual-0".into(),
                w: 1920,
                h: 1080,
                x: 0,
                y: 0,
                addr: "192.168.1.2:0".into(),
                connected: true,
            }],
            media: Some(MediaStatus {
                path: "/srv/clip.mkv".into(),
                playing: true,
            }),
            pairing: Some(PairingState {
                active: true,
                addr: "0.0.0.0:4432".into(),
                pin: Some("004217".into()),
                last: Some(PairOutcome::Paired {
                    pubkey: "abcd".into(),
                    alias: Some("living-room".into()),
                }),
                always_on: false,
            }),
            controllers: vec![ControllerStatus {
                vendor: 0x057e,
                product: 0x2009,
                alias: Some("couch pro".into()),
                connected: true,
                last_connected_ms: Some(1_700_000_000_000),
                bus: ControllerBus::Usb,
            }],
            video_codec: Some("HEVC".into()),
            read_only: false,
            // Exercise a real capability set over the wire (not the default).
            caps: HostCaps {
                pairing: true,
                peers: true,
                launch_app: true,
                ..HostCaps::none()
            },
            gpu_health: GpuHealth::Degraded {
                reason: "thermal throttle".into(),
                throttle: Some("HwThermalSlowdown".into()),
            },
            // Exercise the Mirror variant + its clamp over the wire.
            stream_mode: StreamMode::Mirror {
                clamp: Box::new(MirrorClamp {
                    w: 1920,
                    h: 1080,
                    fps: 60,
                    bitrate_kbps: 20_000,
                    limited_by: Some("living-room".into()),
                    client_count: 2,
                }),
            },
            hdr_active: Some("HEVC Main10 BT.2020/PQ".into()),
            primary_pubkey: Some("abcd".into()),
        });
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn pairing_requests_roundtrip() {
        for req in [
            Request::StartPairing {
                addr: Some("0.0.0.0:4432".into()),
            },
            Request::StartPairing { addr: None },
            Request::CancelPairing,
            Request::Relaunch,
            Request::LaunchApp {
                slug: "elden-ring".into(),
            },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }

    /// A v1 status (no `pairing` key) must still deserialize on a v2 panel —
    /// `#[serde(default)]` fills it with `None` rather than erroring.
    #[test]
    fn v1_status_without_pairing_still_decodes() {
        let v1 =
            r#"{"hostname":"pharos","streaming":false,"clients":[],"outputs":[],"media":null}"#;
        let s: HostStatus = serde_json::from_str(v1).unwrap();
        assert!(s.pairing.is_none());
        assert!(s.controllers.is_empty());
    }

    /// A v3 status (no `controllers` key) must still deserialize on a v4 panel —
    /// the new field is `#[serde(default)]` and falls back to an empty vec.
    #[test]
    fn v3_status_without_controllers_still_decodes() {
        let v3 = r#"{"hostname":"pharos","streaming":false,"clients":[],"outputs":[],"media":null,"pairing":null}"#;
        let s: HostStatus = serde_json::from_str(v3).unwrap();
        assert!(s.controllers.is_empty());
    }

    /// A pre-bus v4 status (controllers entry without `bus`) must still decode
    /// on a build that knows about [`ControllerBus`] — `#[serde(default)]`
    /// fills [`ControllerBus::Unknown`] instead of failing the parse.
    /// Guards the "no protocol-version bump" claim.
    #[test]
    fn controller_status_without_bus_still_decodes() {
        let json = r#"{"vendor":1,"product":2,"connected":false}"#;
        let s: ControllerStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.bus, ControllerBus::Unknown);
    }

    /// A `GetFrameTimings` request and a `FrameTimings` event both round-trip
    /// through JSON. Guards the additive-only promise — adding the new
    /// variants didn't break the wire shape for older requests/events.
    #[test]
    fn frame_timings_roundtrip() {
        let req = Request::GetFrameTimings;
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);

        let ev = Event::FrameTimings(FrameTimingsSnapshot {
            window_frames: 12,
            dispatch: StageStats {
                mean_us: 100,
                p95_us: 250,
                max_us: 800,
                samples: 12,
            },
            ..Default::default()
        });
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    /// The issue #19 peer-fleet requests round-trip through JSON, including the
    /// `SetPrimaryPeer { pubkey: None }` clear form. Guards the additive-only
    /// promise for the new variants.
    #[test]
    fn peer_fleet_requests_roundtrip() {
        for req in [
            Request::ForgetPeer {
                pubkey: "ab".repeat(32),
            },
            Request::SetPrimaryPeer {
                pubkey: Some("cd".repeat(32)),
            },
            Request::SetPrimaryPeer { pubkey: None },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }

    /// The issue #22 additive guarantee, part 1: a status with NO `caps` key
    /// (any pre-caps host) decodes as [`HostCaps::legacy`] — pairing + peers
    /// only, so the panel enables exactly what such a host really serves.
    #[test]
    fn status_without_caps_decodes_as_legacy() {
        let old =
            r#"{"hostname":"pharos","streaming":false,"clients":[],"outputs":[],"media":null}"#;
        let s: HostStatus = serde_json::from_str(old).unwrap();
        assert_eq!(s.caps, HostCaps::legacy());
        assert!(s.caps.pairing && s.caps.peers);
        assert!(!s.caps.launch_app && !s.caps.relaunch && !s.caps.preview);
    }

    /// The issue #22 additive guarantee, part 2: a caps object missing a field
    /// (a host predating a newly added capability) decodes that field as
    /// `false` — a host never gains a capability by omission. Also proves a
    /// full set round-trips intact.
    #[test]
    fn caps_partial_and_roundtrip() {
        let partial: HostCaps = serde_json::from_str(r#"{"pairing":true,"peers":true}"#).unwrap();
        assert!(partial.pairing && partial.peers);
        assert!(!partial.launch_app && !partial.stream_mode && !partial.input_toggle);
        assert!(!partial.preview && !partial.relaunch);
        assert!(!partial.controllers && !partial.frame_timings);

        let mut s = sample_status("rig");
        s.caps = HostCaps {
            pairing: true,
            peers: true,
            launch_app: true,
            ..HostCaps::none()
        };
        let back: HostStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.caps, s.caps);
    }

    /// A status without `primary_pubkey` (an older host) must still decode —
    /// `#[serde(default)]` fills `None`. Guards the no-version-bump claim.
    #[test]
    fn status_without_primary_pubkey_still_decodes() {
        let old =
            r#"{"hostname":"pharos","streaming":false,"clients":[],"outputs":[],"media":null}"#;
        let s: HostStatus = serde_json::from_str(old).unwrap();
        assert!(s.primary_pubkey.is_none());
    }

    /// A v4 `SetControllerAlias` request round-trips through JSON (and the
    /// `alias` field is `Option` so a `null` is preserved on the wire).
    #[test]
    fn set_controller_alias_roundtrips() {
        for req in [
            Request::SetControllerAlias {
                vendor: 0x057e,
                product: 0x2009,
                alias: Some("couch pro".into()),
            },
            Request::SetControllerAlias {
                vendor: 0x057e,
                product: 0x2009,
                alias: None,
            },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }
}
