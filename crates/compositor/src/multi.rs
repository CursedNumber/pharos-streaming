// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Multi-output capture — N virtual monitors driven by one compositor.
//!
//! Vestigial open door: one event loop, one Wayland socket, one shared GLES
//! renderer + CUDA context, and N independent encode/transport pipelines (one
//! Smithay `Output` + render-target texture + NVENC `Session` + `QuicStreamer`
//! per entry in the config). Apps live in one `Space`, so a future "drag a
//! window from monitor A to monitor B" works without re-architecting.
//!
//! For the v1 demo, configure outputs to overlap at (0,0) and you'll get the
//! same content streaming to N clients; configure them side-by-side and you'll
//! get distinct regions per stream (`XdgShellHandler::new_toplevel` places the
//! Nth toplevel on the Nth output in extend layouts, mapping at each output's
//! origin; only the cross-monitor window-drag remains future work, not
//! load-bearing for the open door).
//!
//! ## Why a self-referential `Pipeline` (and why the transmute is sound)
//!
//! `Encoder::start_session(self, …) -> Session` consumes the encoder, so a
//! `Session` is fully owned. But the per-frame state we need over the
//! session's lifetime — `Bitstream<'a>` and `RegisteredResource<'a, T>` —
//! borrow from the session via `&Session`. To keep one self-contained
//! `Pipeline` per output, we Box the session (stable heap address) and
//! transmute the references it hands out to `'static`. The references are
//! actually `'self` for the Pipeline — sound as long as: (a) the session
//! never moves (Box ensures this), and (b) `bitstreams` and `reg` drop
//! before `_session`. Struct fields drop in declaration order, so we
//! declare them in that order below.

use std::ffi::c_void;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8};
use std::sync::Arc;

use anyhow::Result;
use control_ipc::VideoCodec;
use core_crypto::Identity;
use cudarc::driver::result as curesult;
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT::{NV_ENC_BUFFER_FORMAT_ABGR, NV_ENC_BUFFER_FORMAT_ABGR10},
    NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID, NV_ENC_CODEC_HEVC_GUID,
    NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_PARAMS_HEVC, NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_P4_GUID, NV_ENC_SEI_PAYLOAD, NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{
    Bitstream, CodecPictureParams, EncodePictureParams, Encoder, EncoderInitParams, ErrorKind,
    RegisteredResource, Session,
};
use serde::Deserialize;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::KeyState;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{
    Bind, Blit, Color32F, ExportMem, Frame, ImportDma, Offscreen, Renderer, TextureFilter,
};
use smithay::desktop::space::{space_render_elements, SpaceRenderElements};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size};
use smithay::utils::{Transform, SERIAL_COUNTER};
use tracing::info;

use crate::cuegl::CudaGlBridge;
use crate::{unix_nanos_now, CalloopData, State};

/// Top-level config for `pharos-compositor capture-zc-multi <config.toml>`.
#[derive(Debug, Clone, Deserialize)]
pub struct MultiCfg {
    /// Frame rate, shared across all outputs (single pacer). Always the encode
    /// framerate + the auto-pace ceiling; see [`fps_auto`](Self::fps_auto).
    pub fps: u32,
    /// W8: when true, the pace target is ADAPTIVE — the host measures its raw
    /// per-frame work cost and paces to the sustainable rate (between a ~24fps floor
    /// and `fps` as the ceiling), self-tuning to hardware/resolution/scene instead of
    /// chasing a hardcoded number. When false (default), `fps` is a fixed cap
    /// (today's behaviour). NVENC is always configured for `fps` either way.
    #[serde(default)]
    pub fps_auto: bool,
    /// Frame budget. `0` runs until interrupted.
    #[serde(default)]
    pub frames: u32,
    /// Optional client to launch into the shared compositor socket.
    #[serde(default)]
    pub launch: Option<String>,
    /// Informational layout tag the panel round-trips: `"mirror"` / `"extend"` /
    /// `"single"`. The loop is driven entirely by each output's `x`/`y`, so this
    /// is documentation/round-trip only unless those are absent.
    #[serde(default)]
    pub mode: Option<String>,
    /// Video codec for every output's encode session (HEVC default; AV1 on
    /// Ada+; AVC/H.264 for legacy clients; VVC/H.266 experimental — software
    /// encode via ffmpeg/libvvenc, since no GPU encodes it). The panel switches
    /// this via a sidecar override applied in [`load_multi_config`], so the
    /// choice survives the relaunch that applies it.
    #[serde(default)]
    pub video_codec: VideoCodec,
    /// W3b: opt-in target encoder bitrate (kbit/s) for every output's NVENC session.
    /// `None` keeps the preset's heuristic rate control (today's behavior — no 1080p
    /// regression). Set this (or `PHAROS_BITRATE_KBPS`) to CAP the bitrate so a
    /// high-res/fps stream can't outrun the bounded QUIC window (the ~2s-backlog
    /// risk). See [`resolve_rc_bitrate`].
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    /// W3b: capped-VBR ceiling (kbit/s). When set above `bitrate_kbps` the rate
    /// control is VBR capped at this max; otherwise CBR at the average.
    /// `PHAROS_MAX_BITRATE_KBPS` overrides.
    #[serde(default)]
    pub max_bitrate_kbps: Option<u32>,
    /// Ceiling (kbit/s) for CLIENT-requested bitrate caps (the Trireme slider,
    /// Stage 3). Deliberately a separate knob from `bitrate_kbps` — that is the
    /// host's own operating point, which a client cap is designed to override.
    /// Unset = a built-in 500 Mbps sanity cap. `PHAROS_CLIENT_MAX_BITRATE_KBPS`
    /// overrides.
    #[serde(default)]
    pub client_max_bitrate_kbps: Option<u32>,
    #[serde(rename = "output")]
    pub outputs: Vec<OutputCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputCfg {
    pub name: String,
    pub w: u32,
    pub h: u32,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    /// `quic://host:port` to stream, otherwise a file path for raw HEVC.
    pub addr: String,
    /// The paired peer (hex pubkey) pinned to this output, if any. Used to apply
    /// the per-device input gate at startup (and by the panel to bind a tile).
    #[serde(default)]
    pub peer: Option<String>,
    /// Whether this peer's input reaches the seat. Applied to the transport gate at
    /// startup so the *config* is honored — not only the panel's live toggle. (The
    /// panel can still flip it at runtime via `SetInputEnabled`.)
    #[serde(default = "default_input_enabled")]
    pub input_enabled: bool,
}

/// `input_enabled` defaults to on when the field is absent — the safe default is a
/// working controller, not a silently dead one.
fn default_input_enabled() -> bool {
    true
}

impl MultiCfg {
    /// Build a one-output config — the shape `pharos-compositor capture-zc`
    /// used to drive directly before the single-output path was folded into
    /// the multi backend. `frames = 0` means run-until-interrupted (matches
    /// the run-host-stream.sh default of 86400000 = ~9 days).
    pub fn single_output(
        width: u32,
        height: u32,
        fps: u32,
        stream_addr: &str,
        launch_cmd: Option<&str>,
    ) -> Self {
        Self {
            fps,
            frames: 0,
            launch: launch_cmd.map(|s| s.to_string()),
            mode: Some("single".to_string()),
            video_codec: VideoCodec::default(),
            fps_auto: false,
            bitrate_kbps: None,
            max_bitrate_kbps: None,
            client_max_bitrate_kbps: None,
            outputs: vec![OutputCfg {
                name: "HEADLESS-1".to_string(),
                w: width,
                h: height,
                x: 0,
                y: 0,
                addr: stream_addr.to_string(),
                peer: None,
                input_enabled: true,
            }],
        }
    }
}

/// The codec-override sidecar that sits next to a given `multi.toml`. The panel
/// writes it (via [`write_codec_override`]) and triggers a relaunch; the fresh
/// process reads it here, so a codec switch survives the restart that applies it
/// without rewriting (and reformatting) the user's `multi.toml`.
fn codec_sidecar(config_path: &Path) -> std::path::PathBuf {
    config_path.with_file_name("video-codec")
}

/// Read the panel-written codec override next to `config_path`, if present.
fn read_codec_override(config_path: &Path) -> Option<VideoCodec> {
    let s = std::fs::read_to_string(codec_sidecar(config_path)).ok()?;
    match s.trim().to_ascii_lowercase().as_str() {
        "av1" => Some(VideoCodec::Av1),
        "hevc" | "h265" => Some(VideoCodec::Hevc),
        "avc" | "h264" => Some(VideoCodec::H264),
        "vvc" | "h266" => Some(VideoCodec::Vvc),
        _ => None,
    }
}

/// Persist the panel's codec choice as the sidecar next to `config_path`. Called
/// from the render loop before it relaunches to apply the change.
pub fn write_codec_override(config_path: &Path, codec: VideoCodec) -> std::io::Result<()> {
    std::fs::write(
        codec_sidecar(config_path),
        codec.label().to_ascii_lowercase(),
    )
}

pub fn load_multi_config(path: &Path) -> Result<MultiCfg> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut cfg: MultiCfg =
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    // A panel-written codec override wins over the TOML (absent = keep TOML).
    if let Some(codec) = read_codec_override(path) {
        cfg.video_codec = codec;
    }
    anyhow::ensure!(!cfg.outputs.is_empty(), "no outputs configured");
    anyhow::ensure!(cfg.fps > 0, "fps must be > 0");
    // No hard upper bound, but consumer GPUs cap NVENC concurrent sessions at
    // 3-ish; warn loudly so the user knows what to expect on a 4070.
    if cfg.outputs.len() > 3 {
        tracing::warn!(
            "{} outputs requested; consumer NVIDIA GPUs cap NVENC concurrent sessions at ~3",
            cfg.outputs.len()
        );
    }
    Ok(cfg)
}

/// RAII for the CUDA device buffer. Drops AFTER the registered resource and
/// session (declaration order in `Pipeline`), so the unregister + session drop
/// happen while the buffer is still mapped.
struct CudaBuf(u64);
impl Drop for CudaBuf {
    fn drop(&mut self) {
        unsafe { curesult::free_sync(self.0) }.ok();
    }
}

#[allow(dead_code)] // the Nvenc backend's `_session` is held to keep its borrowers alive.
struct Pipeline {
    // Drop order is the field-declaration order: the backend (whose NVENC
    // variant holds the self-referential session chain) drops before cuda_buf,
    // so the unregister (in the NVENC `reg`'s drop) happens while the device
    // pointer is still valid.
    backend: EncodeBackend,
    cuda_buf: CudaBuf,

    // Per-output rendering state.
    pitch: u32,
    width: u32,
    height: u32,
    output: Output,
    target: GlesTexture,
    damage: OutputDamageTracker,
    name: String,
    addr_label: String,

    // --- Resolution ladder (Phase 2) ---
    /// Current coded size NVENC encodes — `<= (width, height)`. Equals native
    /// until the actuator downscales. `encode_picture` is fed a frame of exactly
    /// this size; the client learns it via `DG_STREAM_GEOMETRY`.
    coded_w: u32,
    coded_h: u32,
    /// The selected ceiling coded height (the client/panel "target resolution").
    /// `0` = Auto (native). The controller upholds this and may drop below it
    /// under load, but never exceeds it.
    res_ceiling_h: u32,
    /// The selected minimum coded height the auto-downscale ladder may drop to.
    /// `0` = no floor (the lowest ladder rung). Bounded by the ceiling/native at
    /// use sites. Once the ladder reaches this, it stops dropping resolution.
    res_floor_h: u32,
    /// Cached offscreen downscale target `(texture, w, h)`, sized to the current
    /// coded size. `None` while coded == native (the zero-cost default path: no
    /// blit, byte-identical to pre-ladder behaviour).
    scale: Option<(GlesTexture, u32, u32)>,
    /// HDR only: fp16 scratch the normal composite renders into (preserving scRGB
    /// values >1.0), then converted scRGB→PQ into `target` by the post-pass.
    /// Lazily (re)created at native size.
    hdr_scratch: Option<GlesTexture>,

    // Sink — either a QUIC streamer or a raw-HEVC file.
    streamer: Option<transport::QuicStreamer>,
    file: Option<std::fs::File>,

    /// Pre-baked Pharos "no app running" idle screen, drawn ONLY when no window
    /// overlaps THIS output (per-output empty element list). `None` if it couldn't
    /// be built. Output-sized, built once — zero cost while an app is on screen.
    idle: Option<crate::idle::IdleScreen>,

    /// Desktop-mirror (issue #6): whether this pipeline composites the usual
    /// Wayland app space (`Virtual`, today's only behavior) or streams the
    /// host's REAL desktop session instead (`PhysicalMirror`). Only pipeline
    /// 0 (the primary/single-output case) is ever flipped — see
    /// `ControlCmd::LaunchApp` and the `enable_virtual_displays` master
    /// switch in `run_capture_zc_multi`.
    #[cfg(feature = "portal-mirror")]
    source: CaptureSource,
}

/// The per-output encoder a [`Pipeline`] feeds. HEVC/AV1/AVC run on NVENC
/// (zero-copy from the CUDA linear buffer); VVC — which no GPU encodes — runs
/// on the software `swenc` path (device→host readback into ffmpeg/libvvenc).
enum EncodeBackend {
    Nvenc {
        // Drop order is the field-declaration order. Keep these three in this
        // exact order: bitstreams reference session; reg references session;
        // _session owns the encoder.
        bitstreams: Vec<Bitstream<'static>>,
        reg: RegisteredResource<'static, ()>,
        _session: Box<Session>,
        num_bufs: usize,
    },
    Soft(crate::swenc::SoftEncoder),
}

impl Pipeline {
    fn is_streaming_target_ready(&self) -> bool {
        match &self.streamer {
            Some(s) => s.is_connected(),
            None => true, // file mode is always "ready"
        }
    }

    /// The live libei input sink, if this pipeline is currently mirroring the
    /// real desktop AND its input bridge has come up. `None` routes input to
    /// the ordinary smithay seat (today's behavior).
    #[cfg(feature = "portal-mirror")]
    fn mirror_input(&self) -> Option<&crate::mirror_input::MirrorInput> {
        match &self.source {
            CaptureSource::PhysicalMirror(m) => m.input.as_ref(),
            CaptureSource::Virtual | CaptureSource::MirrorPending(_) => None,
        }
    }
}

/// Where a pipeline's rendered frame comes from — see `Pipeline::source`.
#[cfg(feature = "portal-mirror")]
enum CaptureSource {
    /// Composite Wayland app windows into the headless virtual output. The
    /// only behavior before issue #6; still the default for every output
    /// except the one flipped to a physical-mirror target.
    Virtual,
    /// The portal handshake is running on a dedicated worker thread and hasn't
    /// finished yet. That handshake BLOCKS on the user clicking "Share" in the
    /// KDE permission dialog — which can take seconds — so it must never run on
    /// the render loop (a >1.5s stall trips the P4 heartbeat watchdog and kills
    /// the host before the dialog can even be read). The render loop polls this
    /// channel each tick and flips to `PhysicalMirror` on success / back to
    /// `Virtual` on failure (see `poll_mirror_pending`). Until then the pipeline
    /// renders as `Virtual` (idle screen / app windows), stream stays live.
    MirrorPending(std::sync::mpsc::Receiver<std::result::Result<MirrorReady, String>>),
    /// Stream the REAL host desktop session instead. Holds the frame channel,
    /// the persistent GL importer, the libei input sink, and the teardown
    /// handle for the keeper thread that owns the portal session + PipeWire
    /// stream + tokio runtime (dropping `MirrorState` drops that handle, which
    /// signals the keeper to tear everything down — see `MirrorReady`).
    PhysicalMirror(Box<MirrorState>),
}

/// The Send-safe ingredients the portal keeper thread hands back to the render
/// loop once negotiation succeeds. Everything that is NOT `Send` or must stay
/// pinned to the runtime that created it (the `ashpd` session + the tokio
/// runtime driving its zbus connection) stays on the keeper thread; only these
/// cross the channel.
#[cfg(feature = "portal-mirror")]
pub struct MirrorReady {
    frame_rx: std::sync::mpsc::Receiver<crate::portal_mirror::MirrorFrame>,
    /// The libei socket from `ConnectToEIS`, spawned into a `MirrorInput` on the
    /// render thread (spawn is cheap + non-blocking; the fd is `Send`, the input
    /// thread is not required to be). `None` = view-only (portal too old / EIS
    /// failed).
    input_fd: Option<std::os::fd::OwnedFd>,
    /// Dropping the matching `MirrorState._shutdown` resolves the keeper's
    /// `await` on the receiver half (with `Err(RecvError)`), which tears the
    /// session down. A tokio oneshot (not std mpsc) so the keeper can await it
    /// INSIDE `block_on`, keeping the current-thread runtime driving zbus the
    /// whole time it's parked.
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

/// Live state for a pipeline currently mirroring the real desktop.
#[cfg(feature = "portal-mirror")]
struct MirrorState {
    /// Teardown signal to the keeper thread: dropping this `Sender` (when the
    /// pipeline flips back to `Virtual`, or the whole `MirrorState` is dropped)
    /// resolves the keeper's awaited receiver, which then drops the portal
    /// session, the PipeWire stream, and the tokio runtime — closing the mirror
    /// cleanly.
    _shutdown: tokio::sync::oneshot::Sender<()>,
    frame_rx: std::sync::mpsc::Receiver<crate::portal_mirror::MirrorFrame>,
    importer: crate::portal_mirror::MirrorImporter,
    /// `None` until the libei handshake/thread spawn succeeds (or if it
    /// failed) — video can still flow with a dead input bridge; the mirror
    /// just isn't controllable yet, logged once at spawn time.
    input: Option<crate::mirror_input::MirrorInput>,
    /// Most recent frame pulled off `frame_rx`; re-rendered as-is on ticks
    /// where PipeWire hasn't delivered a fresh one yet (matches the video
    /// pacer, not PipeWire's own rate).
    latest: Option<crate::portal_mirror::MirrorFrame>,
}

/// Where the portal's restore token is persisted (next to `multi.toml`'s
/// sibling files — same directory the codec-override sidecar and the
/// identity live in) so a relaunch skips the permission dialog. Best-effort:
/// a missing/unreadable file just means "ask again," never fatal.
#[cfg(feature = "portal-mirror")]
fn mirror_restore_token_path() -> Option<std::path::PathBuf> {
    crate::pair::config_dir()
        .ok()
        .map(|d| d.join("mirror-restore-token"))
}

#[cfg(feature = "portal-mirror")]
fn read_mirror_restore_token() -> Option<String> {
    let p = mirror_restore_token_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(feature = "portal-mirror")]
fn write_mirror_restore_token(token: &str) {
    let Some(p) = mirror_restore_token_path() else {
        return;
    };
    if let Err(e) = std::fs::write(&p, token) {
        tracing::warn!(
            "mirror: couldn't persist restore token at {}: {e}",
            p.display()
        );
    }
}

/// Begin flipping `pipeline` from `Virtual` toward `PhysicalMirror`: spawn a
/// dedicated keeper thread that negotiates the portal session (reusing a
/// persisted restore token so a repeat launch doesn't re-prompt), starts the
/// PipeWire frame stream, and connects the libei input socket. Video and input
/// degrade independently — a libei failure logs and leaves the mirror view-only
/// rather than aborting the whole activation.
///
/// **NON-blocking by design.** The portal handshake blocks until the user clicks
/// "Share" in the KDE permission dialog (seconds), so it CANNOT run on the render
/// loop — a >1.5s stall trips the P4 heartbeat watchdog and kills the host before
/// the dialog is even readable (the original synchronous version did exactly
/// this). Instead the handshake runs on a keeper thread that also OWNS the tokio
/// runtime for the mirror's whole lifetime: `ashpd` speaks D-Bus over zbus's
/// tokio integration, so the session stays alive only while a runtime keeps
/// driving its background reader — the old code's throwaway current-thread
/// runtime (dropped when activation returned) would have silently killed the
/// session. This function returns immediately with the pipeline in
/// `MirrorPending`; the render loop's `poll_mirror_pending` installs the live
/// `PhysicalMirror` (or reverts to `Virtual`) once the keeper reports back.
/// How long the portal keeper waits for the user to answer the "Share your
/// screen?" dialog before giving up and reverting to `Virtual`. Generous enough
/// to walk to the host and click, short enough that an abandoned dialog doesn't
/// wedge desktop-mirror until a host restart.
#[cfg(feature = "portal-mirror")]
const MIRROR_DIALOG_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(feature = "portal-mirror")]
fn activate_physical_mirror(pipeline: &mut Pipeline) -> Result<()> {
    use crate::portal_mirror::MirrorSession;

    let (ready_tx, ready_rx) =
        std::sync::mpsc::channel::<std::result::Result<MirrorReady, String>>();

    std::thread::Builder::new()
        .name("pharos-mirror-portal".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("build tokio runtime: {e}")));
                    return;
                }
            };

            // The ENTIRE session lifetime runs inside one `block_on`: negotiate,
            // hand back the Send ingredients, then await teardown. Staying inside
            // `block_on` keeps the current-thread runtime driving zbus for the
            // whole time the mirror is up — a runtime that stopped being driven
            // would let the portal session go unresponsive.
            rt.block_on(async move {
                // Negotiate: open the session (this await blocks on the user's
                // "Share" dialog click), persist any restore token, connect input.
                // Bounded by MIRROR_DIALOG_TIMEOUT so an UNANSWERED dialog (user
                // never clicked Share, or didn't see it) self-clears: the keeper
                // gives up, the render loop reverts to Virtual, and a fresh
                // Desktop tap starts a new attempt — no host restart needed. A
                // dialog the user actively CANCELS returns an Err sooner (no
                // wait for the timeout).
                let opened = tokio::time::timeout(MIRROR_DIALOG_TIMEOUT, async {
                    let restore_token = read_mirror_restore_token();
                    let session = MirrorSession::open(restore_token.as_deref()).await?;
                    if let Some(token) = &session.restore_token {
                        write_mirror_restore_token(token);
                    }
                    let stream_info = session
                        .streams
                        .first()
                        .ok_or_else(|| anyhow::anyhow!("portal granted zero streams"))?
                        .clone();
                    let input_fd = match session.connect_input().await {
                        Ok(fd) => Some(fd),
                        Err(e) => {
                            tracing::warn!("mirror: ConnectToEIS failed: {e:#} (view-only)");
                            None
                        }
                    };
                    Ok::<_, anyhow::Error>((session, stream_info, input_fd))
                })
                .await;
                let (session, stream_info, input_fd) = match opened {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                    Err(_elapsed) => {
                        let _ = ready_tx.send(Err(format!(
                            "no response to the screen-share dialog within {}s (abandoned?) — \
                             reverting; tap Desktop again to retry",
                            MIRROR_DIALOG_TIMEOUT.as_secs()
                        )));
                        return;
                    }
                };

                // PipeWire frame stream (its own MainLoop thread, independent of
                // the runtime). Built here so a failure cancels the activation.
                let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
                let stream =
                    match crate::portal_mirror::spawn_frame_stream(stream_info.node_id, frame_tx) {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("spawn frame stream: {e:#}")));
                            return;
                        }
                    };

                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                info!(
                    "mirror: portal negotiated (stream {}x{:?}, input {})",
                    stream_info.node_id,
                    stream_info.size,
                    if input_fd.is_some() {
                        "live"
                    } else {
                        "unavailable"
                    }
                );
                if ready_tx
                    .send(Ok(MirrorReady {
                        frame_rx,
                        input_fd,
                        shutdown_tx,
                    }))
                    .is_err()
                {
                    // Render loop gave up on us (deactivated before the dialog was
                    // answered). Fall through: `stream` + `session` drop here.
                    return;
                }

                // Park until the render loop drops the matching `MirrorState`
                // (its `_shutdown` sender resolves this await with `Err`). The
                // runtime keeps zbus serviced meanwhile; nothing spins.
                let _ = shutdown_rx.await;
                drop(stream);
                drop(session);
                tracing::info!("mirror: portal session torn down");
            });
        })
        .map_err(|e| anyhow::anyhow!("mirror: spawn portal keeper thread: {e}"))?;

    pipeline.source = CaptureSource::MirrorPending(ready_rx);
    Ok(())
}

/// Render-loop step: if `pipeline` is `MirrorPending`, poll the keeper thread's
/// channel and, on a result, flip to a live `PhysicalMirror` (spawning the
/// libei input thread from the handed-back fd) or revert to `Virtual`. A no-op
/// for any other source, and while the handshake is still in flight (the
/// pipeline keeps rendering `Virtual` — idle screen / app windows — so the
/// stream never freezes waiting on the dialog). Cheap: one `try_recv` per tick.
#[cfg(feature = "portal-mirror")]
fn poll_mirror_pending(pipeline: &mut Pipeline) {
    // Scope the borrow of `pipeline.source` (via `rx`) so it ends before the
    // reassignment below; `next` owns everything it needs.
    let next: CaptureSource = {
        let CaptureSource::MirrorPending(rx) = &pipeline.source else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(ready)) => {
                let input = ready.input_fd.and_then(|fd| {
                    match crate::mirror_input::MirrorInput::spawn(fd) {
                        Ok(mi) => Some(mi),
                        Err(e) => {
                            tracing::warn!(
                                "mirror: libei thread failed to start: {e:#} (view-only)"
                            );
                            None
                        }
                    }
                });
                info!("mirror: activated on output '{}'", pipeline.name);
                CaptureSource::PhysicalMirror(Box::new(MirrorState {
                    _shutdown: ready.shutdown_tx,
                    frame_rx: ready.frame_rx,
                    importer: crate::portal_mirror::MirrorImporter::default(),
                    input,
                    latest: None,
                }))
            }
            Ok(Err(e)) => {
                tracing::warn!("mirror: activation failed: {e}; staying on the virtual display");
                CaptureSource::Virtual
            }
            // Still negotiating (dialog open / restore in flight): keep waiting.
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                tracing::warn!("mirror: portal keeper exited without a result; staying virtual");
                CaptureSource::Virtual
            }
        }
    };
    pipeline.source = next;
}

/// Forward one keyboard/pointer `InputEvent` to the libei bridge instead of
/// the smithay seat — the mirror-mode counterpart of `dispatch_input`
/// (`lib.rs`). Button/keycode conventions match that function's mapping
/// exactly (see `mirror_input`'s module docs for why no xkb `+8` offset is
/// applied here). Gamepad/gyro/tuning variants never reach this closure —
/// `HostInput::dispatch_input_event` consumes those before calling it.
#[cfg(feature = "portal-mirror")]
fn route_to_mirror_input(mi: &crate::mirror_input::MirrorInput, ev: core_proto::InputEvent) {
    match ev {
        core_proto::InputEvent::Key {
            scancode, pressed, ..
        } => mi.key(scancode, pressed),
        core_proto::InputEvent::PointerAbs { x, y } => mi.pointer_abs(x, y),
        core_proto::InputEvent::PointerRel { dx, dy } => mi.pointer_rel(dx, dy),
        core_proto::InputEvent::PointerButton { button, pressed } => {
            // evdev button codes: 0x110=BTN_LEFT .. 0x114=BTN_EXTRA (same
            // mapping as `dispatch_input`'s smithay path in `lib.rs`).
            mi.button(0x110u32 + u32::from(button), pressed);
        }
        core_proto::InputEvent::Scroll { dx, dy } => mi.scroll(dx, dy),
        _ => {}
    }
}

/// Reconcile the primary pipeline's capture source with a just-launched
/// app's [`app_launcher::config::LaunchTarget`] and the `enable_virtual_displays`
/// master switch. Only pipeline 0 is ever touched — see `Pipeline::source`'s
/// docs on why mirroring is a single-output concern.
///
/// Precedence: the master switch OFF wins outright ("never build a virtual
/// output" means every app's stream — even one that still declares
/// `VirtualDisplay` in its saved entry — mirrors the real desktop instead).
/// With the switch ON, the app's own target decides: `DuplicatePhysical`
/// mirrors, anything else (including a missing/unknown slug) uses the
/// ordinary virtual composite.
#[cfg(feature = "portal-mirror")]
fn apply_capture_target(
    pipelines: &mut [Option<Pipeline>],
    target: Option<&app_launcher::config::LaunchTarget>,
) {
    let Some(Some(p0)) = pipelines.first_mut() else {
        return;
    };
    let master_switch_on = host_core::config::load().compositor.enable_virtual_displays;
    let wants_mirror = !master_switch_on
        || matches!(
            target,
            Some(app_launcher::config::LaunchTarget::DuplicatePhysical)
        );
    // `MirrorPending` counts as "already mirroring" — a repeat DuplicatePhysical
    // launch while the dialog is still open must not spawn a second keeper /
    // second permission prompt.
    let is_mirroring = matches!(
        p0.source,
        CaptureSource::PhysicalMirror(_) | CaptureSource::MirrorPending(_)
    );
    match (wants_mirror, is_mirroring) {
        (true, false) => {
            if let Err(e) = activate_physical_mirror(p0) {
                tracing::warn!("mirror: activation failed: {e:#}; staying on the virtual display");
            }
        }
        (false, true) => deactivate_physical_mirror(p0),
        _ => {} // already in the wanted state
    }
}

/// Flip `pipeline` back to `Virtual`. Dropping the outgoing source tears
/// everything down: for a live `PhysicalMirror`, dropping `MirrorState`'s
/// `_shutdown` sender wakes the keeper thread, which drops the portal session
/// (ending ScreenCast + RemoteDesktop), the PipeWire stream, the tokio runtime,
/// and the libei thread (`MirrorInput::drop`); for a `MirrorPending`, dropping
/// the receiver makes the keeper's `ready_tx.send` fail and it tears itself down
/// (cancelling an unanswered dialog).
#[cfg(feature = "portal-mirror")]
fn deactivate_physical_mirror(pipeline: &mut Pipeline) {
    match pipeline.source {
        CaptureSource::PhysicalMirror(_) => {
            info!("mirror: deactivated on output '{}'", pipeline.name)
        }
        CaptureSource::MirrorPending(_) => {
            info!("mirror: activation cancelled on output '{}'", pipeline.name)
        }
        CaptureSource::Virtual => {}
    }
    pipeline.source = CaptureSource::Virtual;
}

/// Whether the primary output (pipeline 0) is currently streaming the REAL
/// desktop rather than a virtual composite. The Steam-redirect marker keys off
/// this: in mirror mode the marker must be absent so a game the user launches
/// from the mirrored desktop stays on wayland-0 and lands in the mirror. Always
/// `false` when the `portal-mirror` feature is compiled out (no mirror source
/// can ever exist).
#[cfg(feature = "portal-mirror")]
fn primary_is_mirroring(pipelines: &[Option<Pipeline>]) -> bool {
    // A pending activation counts as mirroring: the user intends the real
    // desktop, so suppress the redirect marker now rather than briefly writing
    // it for the seconds the dialog is open.
    matches!(
        pipelines.first(),
        Some(Some(p)) if matches!(
            p.source,
            CaptureSource::PhysicalMirror(_) | CaptureSource::MirrorPending(_)
        )
    )
}
#[cfg(not(feature = "portal-mirror"))]
fn primary_is_mirroring(_pipelines: &[Option<Pipeline>]) -> bool {
    false
}

/// Slugs of apps whose launch target mirrors the real desktop
/// (`DuplicatePhysical`, which the built-in Desktop tile defaults to).
/// Parses apps.toml, so call it only on a state change, not per frame.
fn mirror_target_slugs() -> Vec<String> {
    app_launcher::config::load_with_builtins()
        .apps
        .into_iter()
        .filter(|a| a.target == app_launcher::config::LaunchTarget::DuplicatePhysical)
        .map(|a| a.slug)
        .collect()
}

fn mirror_target_slug(slug: &str) -> bool {
    app_launcher::config::load_with_builtins()
        .find(slug)
        .is_some_and(|a| a.target == app_launcher::config::LaunchTarget::DuplicatePhysical)
}

/// The primary output's native mode, for sizing launches (a steam-session's
/// gamescope output — issue #38). Native `width`/`height`, NOT the DRC-scaled
/// coded size: the session renders at the streamed surface's full mode and the
/// resolution ladder keeps downscaling the encode underneath it.
/// Lowercase-hex pairing pubkey of the live client, if one is connected and
/// paired. The host serves one client at a time (a new one pre-empts), so this
/// is the client an app launch is for; in mirror mode it is the first live
/// pipeline's. Keys per-client app config (`app_launcher::per_client`).
fn connected_client(pipelines: &[Option<Pipeline>]) -> Option<String> {
    pipelines
        .iter()
        .flatten()
        .filter_map(|p| p.streamer.as_ref())
        .find_map(|s| s.connected_pubkey())
}

fn primary_output_size(pipelines: &[Option<Pipeline>]) -> Option<(u32, u32)> {
    pipelines
        .first()
        .and_then(|p| p.as_ref())
        .map(|p| (p.width, p.height))
}

/// Reconcile the seamless Steam-redirect marker with the live session. The
/// marker must be present on disk iff the redirect feature is enabled, a client
/// is actively streaming, the primary output is NOT mirroring the real
/// desktop, AND no gamescope Steam session is live. The mirror clause is the
/// desktop-mirror fix (issue #6): while the user is looking at the mirrored
/// real desktop, a game they start there must run on wayland-0 (so it appears
/// in the mirror), which means the shim must see NO marker and pass through.
/// Writing the marker in mirror mode would instead shove the game onto the
/// unwatched virtual display. The steam-session clause is the same idea for
/// issue #38: the session Steam already launches games inside gamescope, and a
/// marker would make the shim re-exec them onto the bare compositor socket,
/// out of the session (and out of Steam Input's sight).
///
/// Idempotent and edge-tracked via [`REDIRECT_MARKER_ON`] — safe to call on
/// every status tick; it only touches the filesystem on an actual transition.
/// Runs entirely on the single render/control-loop thread, so reads of the
/// pipelines' capture source never race the launch handlers that flip them.
fn reconcile_redirect_marker(pipelines: &[Option<Pipeline>], socket_name: &std::ffi::OsStr) {
    if !app_launcher::redirect::enabled() {
        return;
    }
    let any_connected = pipelines
        .iter()
        .flatten()
        .any(|p| p.streamer.as_ref().is_some_and(|s| s.is_connected()));
    let mirroring = primary_is_mirroring(pipelines);
    let steam_session = app_launcher::launch::steam_session_active();
    let want = any_connected && !mirroring && !steam_session;
    let was = REDIRECT_MARKER_ON.swap(want, std::sync::atomic::Ordering::Relaxed);
    if want == was {
        return;
    }
    if want {
        let marker = app_launcher::redirect::MarkerEnv::for_current(
            socket_name.to_string_lossy().into_owned(),
            app_launcher::session_audio_sink(),
        );
        if let Err(e) = app_launcher::redirect::write_marker(&marker) {
            // Roll the edge back so the next status tick retries the write
            // instead of leaving the marker absent for the whole session.
            tracing::warn!("launch-redirect: write marker failed, will retry: {e:#}");
            REDIRECT_MARKER_ON.store(was, std::sync::atomic::Ordering::Relaxed);
        } else {
            tracing::info!(
                "launch-redirect: stream active on the virtual display → games route to the stream"
            );
        }
    } else {
        app_launcher::redirect::remove_marker();
        if mirroring {
            tracing::info!(
                "launch-redirect: primary output mirroring the real desktop → games stay on the \
                 desktop (in the mirror)"
            );
        } else if steam_session {
            tracing::info!(
                "launch-redirect: gamescope Steam session live → the session launches games \
                 itself; shim passes through"
            );
        } else {
            tracing::info!("launch-redirect: stream idle → games stay on the desktop");
        }
    }
}

/// Build a fresh NVENC session + reg + bitstreams for an existing Pipeline.
/// The CUDA device buffer (cuda_buf) and Smithay output/target/damage are
/// kept; only the NVENC chain is rebuilt — that's what gives the next
/// `encode_picture` call a clean picture-0 IDR.
///
/// Classic left-pointing arrow cursor (X11 `left_ptr`), hotspot at the top-left
/// cell (0,0). `'o'` = black outline, `'X'` = white fill, `' '` = transparent.
/// Rasterised + scaled by [`cursor_overlay_rects`] and painted as a per-frame
/// overlay so a streamed client (a TV mouse / controller-as-cursor) can see
/// where it's pointing when a windowed dialog is up.
const CURSOR_BITMAP: &[&str] = &[
    "o           ",
    "oo          ",
    "oXo         ",
    "oXXo        ",
    "oXXXo       ",
    "oXXXXo      ",
    "oXXXXXo     ",
    "oXXXXXXo    ",
    "oXXXXXXXo   ",
    "oXXXXXXXXo  ",
    "oXXXXXoooo  ",
    "oXXoXXo     ",
    "oXo oXXo    ",
    "oo  oXXo    ",
    "o    oXXo   ",
    "      oo    ",
];

// A render element that is EITHER a normal space (window) element OR one of the
// solid-colour rectangles that make up the overlay cursor. Wrapping both into a
// single type lets the cursor be composited INSIDE `render_output`'s one
// damage-tracked pass (same GPU pass + fence as the app frame) instead of a
// second draw the encoder could capture mid-way — which flickered.
smithay::backend::renderer::element::render_elements! {
    OverlayRenderElements<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor = SolidColorRenderElement,
}

/// One opaque cursor rectangle as a render element. A fresh `Id` per frame is
/// fine: the damage tracker only retains the previous frame's element set, and
/// the render loop always drives `render_output` with `age = 0` (full redraw),
/// so there's no stale-damage or unbounded-state concern.
fn cursor_solid(geo: Rectangle<i32, Physical>, color: [f32; 4]) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        Id::new(),
        geo,
        CommitCounter::default(),
        Color32F::from(color),
        Kind::Unspecified,
    )
}

/// The physical rectangles making up one colour layer of the overlay cursor.
type CursorRects = Vec<Rectangle<i32, Physical>>;

/// Physical scale for the overlay cursor. Proportional to resolution so the
/// apparent size is stable across outputs, tuned smaller than a 2x desktop
/// cursor (users found 4px/cell too big at 4K). `PHAROS_CURSOR_SCALE` overrides.
/// Clamped to a minimum of 2 so it's never sub-pixel.
fn cursor_scale(height: u32) -> i32 {
    if let Some(s) = std::env::var("PHAROS_CURSOR_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
    {
        return s.clamp(1, 16);
    }
    ((height as f32 / 800.0).round() as i32).max(2)
}

/// Rasterise [`CURSOR_BITMAP`] at physical `loc`, scaled by `scale`, into two
/// rectangle lists `(outline, fill)` — outline drawn first, fill on top — so the
/// arrow reads on any background. Consecutive same-kind cells merge into one
/// rect per run to keep the draw-call count low. Pure (unit-tested).
fn cursor_overlay_rects(loc: (f64, f64), scale: i32) -> (CursorRects, CursorRects) {
    let ox = loc.0.round() as i32;
    let oy = loc.1.round() as i32;
    let mut outline = Vec::new();
    let mut fill = Vec::new();
    for (row, line) in CURSOR_BITMAP.iter().enumerate() {
        let y = oy + row as i32 * scale;
        let bytes = line.as_bytes();
        let mut col = 0usize;
        while col < bytes.len() {
            let c = bytes[col];
            if c == b' ' {
                col += 1;
                continue;
            }
            // Extend the run over identical cells.
            let start = col;
            while col < bytes.len() && bytes[col] == c {
                col += 1;
            }
            let run = (col - start) as i32;
            let rect = Rectangle::<i32, Physical>::new(
                (ox + start as i32 * scale, y).into(),
                Size::from((run * scale, scale)),
            );
            match c {
                b'o' => outline.push(rect),
                _ => fill.push(rect),
            }
        }
    }
    (outline, fill)
}

/// NVENC encode GUID for a [`VideoCodec`]. Only call for NVENC codecs
/// (`codec.is_nvenc()`): VVC has no NVENC GUID — the pipeline builder routes it
/// to the software backend (`swenc`) before ever asking for one.
fn codec_encode_guid(codec: VideoCodec) -> nvidia_video_codec_sdk::sys::nvEncodeAPI::GUID {
    match codec {
        VideoCodec::Av1 => NV_ENC_CODEC_AV1_GUID,
        VideoCodec::Hevc => NV_ENC_CODEC_HEVC_GUID,
        VideoCodec::H264 => NV_ENC_CODEC_H264_GUID,
        VideoCodec::Vvc => unreachable!("VVC is not an NVENC codec (software swenc path)"),
    }
}

/// Periodic IDR cadence in frames. Used to be hardcoded to `fps` (1s) so a
/// mid-stream joiner resynced within a second; that requirement no longer
/// holds, so it's env-tunable via `PHAROS_IDR_PERIOD_SEC` (default 4s) —
/// spacing IDRs out means the encoder eats the IDR-sized rate-control spike
/// less often.
fn idr_period_frames(fps: u32) -> u32 {
    let secs = std::env::var("PHAROS_IDR_PERIOD_SEC")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4);
    fps.max(1).saturating_mul(secs)
}

/// VBV buffer depth, in frames of average bitrate. Used to be hardcoded to 1
/// frame, which an IDR (2-4x a P-frame) overflows — the rate controller then
/// clamps QP hard to stay in budget, visible as a blocky pulse synced to the
/// IDR cadence. Env-tunable via `PHAROS_VBV_FRAMES` (default 4) now that the
/// buffer no longer needs to double as the resync-latency bound.
fn vbv_buffer_frames() -> u32 {
    std::env::var("PHAROS_VBV_FRAMES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4)
}

/// Apply the codec-specific low-latency knobs onto a fresh preset config: a
/// periodic IDR (see `idr_period_frames`) plus per-keyframe parameter-set
/// repetition. HEVC repeats VPS/SPS/PPS (`repeatSPSPPS`); H.264 repeats SPS/PPS
/// (same flag name in its config struct); AV1 repeats the sequence-header OBU
/// (`repeatSeqHdr`). `cfg` must be the preset returned by `get_preset_config`
/// for this codec's GUID, so the active union variant matches the codec.
fn apply_codec_lowlatency(
    cfg: &mut nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_CONFIG,
    codec: VideoCodec,
    fps: u32,
) {
    let idr = idr_period_frames(fps);
    match codec {
        VideoCodec::Hevc => {
            cfg.encodeCodecConfig.hevcConfig.idrPeriod = idr;
            // SAFETY: the union is the HEVC variant (NV_ENC_CODEC_HEVC_GUID); the
            // bitfield setter needs `&mut` to the union field, hence `unsafe`.
            unsafe {
                cfg.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1);
            }
        }
        VideoCodec::H264 => {
            cfg.encodeCodecConfig.h264Config.idrPeriod = idr;
            // SAFETY: the union is the H.264 variant (NV_ENC_CODEC_H264_GUID).
            unsafe {
                cfg.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
            }
        }
        VideoCodec::Av1 => {
            cfg.encodeCodecConfig.av1Config.idrPeriod = idr;
            // SAFETY: the union is the AV1 variant (NV_ENC_CODEC_AV1_GUID).
            unsafe {
                cfg.encodeCodecConfig.av1Config.set_repeatSeqHdr(1);
            }
        }
        // VVC never reaches NVENC config: the software path sets its own
        // periodic IDR on the ffmpeg command line (`-g`).
        VideoCodec::Vvc => {}
    }
}

/// Static HDR10 encode config: HEVC **Main10** profile + 10-bit depth + BT.2020
/// primaries / PQ (ST-2084) transfer / BT.2020-NCL matrix in the VUI. HEVC-only
/// (AV1 HDR is a separate profile); a no-op for AV1. The Mastering-Display (SEI
/// 137) + Content-Light-Level (SEI 144) are per-picture and attached at the
/// encode call, not here. `cfg` must be the HEVC preset (union = HEVC variant).
fn apply_hdr10(
    cfg: &mut nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_CONFIG,
    codec: VideoCodec,
) {
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
        NV_ENC_HEVC_PROFILE_MAIN10_GUID, NV_ENC_VUI_COLOR_PRIMARIES, NV_ENC_VUI_MATRIX_COEFFS,
        NV_ENC_VUI_TRANSFER_CHARACTERISTIC,
    };
    match codec {
        VideoCodec::Hevc => {
            cfg.profileGUID = NV_ENC_HEVC_PROFILE_MAIN10_GUID;
            // SAFETY: the union is the HEVC variant (gated by the match); the
            // bitfield setter + VUI writes need `&mut` to the union field.
            unsafe {
                let hevc = &mut cfg.encodeCodecConfig.hevcConfig;
                hevc.set_pixelBitDepthMinus8(2); // 10-bit encoded output
                let vui = &mut hevc.hevcVUIParameters;
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = 0; // limited range — standard HDR10 signal
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
                vui.transferCharacteristics =
                    NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
                vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
            }
        }
        // AV1 10-bit HDR is a separate profile/signalling path — not wired yet.
        // H.264 has no practical HDR10 profile (High-10 has no HW decode), and
        // the software VVC path is 8-bit RGBA readback — both stay SDR (the
        // startup gate warns and clears `hdr` before this is ever called).
        VideoCodec::Av1 | VideoCodec::H264 | VideoCodec::Vvc => {}
    }
}

/// HDR post-composite shader: scRGB (BT.709 linear light, 1.0 = 80 nits — what
/// winewayland/DXVK submits) → PQ (ST-2084) / BT.2020, written into the 10-bit
/// target. Applied as a full-screen pass over an fp16 scratch that the normal
/// composite rendered into (so scRGB values >1.0 survive). It samples a plain 2D
/// texture, so the `EXTERNAL` variant is compiled but never exercised. PQ
/// constants + the BT.709→BT.2020 matrix match the standard (as in gamescope's
/// colorimetry). `//_DEFINES_` is replaced by the renderer per shader variant.
const SCRGB_TO_PQ_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

const float PQ_M1 = 0.1593017578125;
const float PQ_M2 = 78.84375;
const float PQ_C1 = 0.8359375;
const float PQ_C2 = 18.8515625;
const float PQ_C3 = 18.6875;

vec3 nits_to_pq(vec3 nits) {
    vec3 y = clamp(nits / 10000.0, 0.0, 1.0);
    vec3 ym1 = pow(y, vec3(PQ_M1));
    vec3 num = PQ_C1 + PQ_C2 * ym1;
    vec3 den = 1.0 + PQ_C3 * ym1;
    return pow(num / den, vec3(PQ_M2));
}

// BT.709 -> BT.2020 linear RGB, column-major.
const mat3 BT709_TO_BT2020 = mat3(
    0.627403896, 0.069097289, 0.016391439,
    0.329283038, 0.919540395, 0.088013308,
    0.043313066, 0.011362316, 0.895595253
);

void main() {
    vec4 c = texture2D(tex, v_coords);
    vec3 lin709 = max(c.rgb, 0.0);              // scRGB: linear light, BT.709
    vec3 nits = BT709_TO_BT2020 * (lin709 * 80.0);
    vec3 pq = nits_to_pq(nits);
    gl_FragColor = vec4(pq, alpha);
}
"#;

/// SDR (sRGB-encoded, BT.709) → PQ / BT.2020 for surfaces in a 10-bit HDR session
/// that DON'T declare HDR (a normal SDR game, the desktop, the idle screen). Same
/// as the scRGB shader but: sRGB EOTF decode (the buffer is gamma-encoded, not
/// linear) and SDR white → 203 nits (BT.2408 HDR reference white) — so SDR shows
/// at a comfortable brightness on the HDR display instead of being read as raw PQ
/// (mid-tones crushed dark, whites blown to 10000 nits).
const SRGB_TO_PQ_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

const float PQ_M1 = 0.1593017578125;
const float PQ_M2 = 78.84375;
const float PQ_C1 = 0.8359375;
const float PQ_C2 = 18.8515625;
const float PQ_C3 = 18.6875;

vec3 nits_to_pq(vec3 nits) {
    vec3 y = clamp(nits / 10000.0, 0.0, 1.0);
    vec3 ym1 = pow(y, vec3(PQ_M1));
    vec3 num = PQ_C1 + PQ_C2 * ym1;
    vec3 den = 1.0 + PQ_C3 * ym1;
    return pow(num / den, vec3(PQ_M2));
}

vec3 srgb_to_linear(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(lo, hi, step(vec3(0.04045), c));
}

const mat3 BT709_TO_BT2020 = mat3(
    0.627403896, 0.069097289, 0.016391439,
    0.329283038, 0.919540395, 0.088013308,
    0.043313066, 0.011362316, 0.895595253
);

void main() {
    vec4 c = texture2D(tex, v_coords);
    vec3 lin709 = srgb_to_linear(clamp(c.rgb, 0.0, 1.0)); // SDR sRGB → linear
    vec3 nits = BT709_TO_BT2020 * (lin709 * 203.0);       // SDR white → 203 nits
    vec3 pq = nits_to_pq(nits);
    gl_FragColor = vec4(pq, alpha);
}
"#;

/// Lowest the network controller will throttle the average bitrate to (kbit/s).
const NET_FLOOR_KBPS: u32 = 2000;

/// W3b: resolve the opt-in encoder bitrate `(avg_kbps, max_kbps)`, or `None` when
/// nothing is configured (leaves the NVENC preset's own heuristic RC untouched, so
/// the working 1080p path doesn't regress). Precedence: `PHAROS_BITRATE_KBPS` env
/// > [`MultiCfg::bitrate_kbps`]. The ceiling is `PHAROS_MAX_BITRATE_KBPS` >
/// > [`MultiCfg::max_bitrate_kbps`] > the average (i.e. CBR when no ceiling is given).
fn resolve_rc_bitrate(cfg: &MultiCfg) -> Option<(u32, u32)> {
    let env_u32 = |k: &str| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
    };
    let avg = env_u32("PHAROS_BITRATE_KBPS").or(cfg.bitrate_kbps)?.max(1);
    let max = env_u32("PHAROS_MAX_BITRATE_KBPS")
        .or(cfg.max_bitrate_kbps)
        .unwrap_or(avg)
        .max(avg);
    Some((avg, max))
}

/// Sanity ceiling for client-requested caps when neither the env override nor
/// `client_max_bitrate_kbps` is set. Above the top Trireme slider stop (300 Mbps)
/// so the shipped ladder is never clamped by default, below silly (a fat-fingered
/// future numeric input can't drive NVENC to Gbps rates).
const CLIENT_CAP_HARD_MAX_KBPS: u32 = 500_000;

/// The ceiling applied to a CLIENT-requested bitrate cap (Stage 3) before it
/// drives RC. Precedence: `PHAROS_CLIENT_MAX_BITRATE_KBPS` env, then
/// [`MultiCfg::client_max_bitrate_kbps`], then [`CLIENT_CAP_HARD_MAX_KBPS`].
/// Deliberately NOT derived from `bitrate_kbps`: that is the host's operating
/// point, which the client cap is designed to override (clamping to it would
/// undo the Phase 3 lever).
fn client_bitrate_ceiling(cfg: &MultiCfg) -> u32 {
    std::env::var("PHAROS_CLIENT_MAX_BITRATE_KBPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .or(cfg.client_max_bitrate_kbps)
        .unwrap_or(CLIENT_CAP_HARD_MAX_KBPS)
        .max(NET_FLOOR_KBPS)
}

/// Live-reconfigure the RC bitrate on every streaming pipeline's NVENC session
/// (in place via `NvEncReconfigureEncoder` — proven crash-free, unlike a re-init).
/// `avg_kbps` is the new average; the configured avg→max headroom ratio is
/// preserved; VBV is sized via `vbv_buffer_frames`. Used by the network-aware
/// controller and the reconnect-restore path. Errors are logged, not fatal.
fn reconfigure_all_bitrate(
    pipelines: &[Option<Pipeline>],
    avg_kbps: u32,
    avg_cap: u32,
    max_cap: u32,
    fps: u32,
    force_idr: bool,
) {
    let avg_bps = avg_kbps.saturating_mul(1000);
    let max_kbps = ((u64::from(avg_kbps) * u64::from(max_cap)) / u64::from(avg_cap.max(1))) as u32;
    let max_bps = max_kbps.saturating_mul(1000);
    let vbv = (avg_bps / fps.max(1)).saturating_mul(vbv_buffer_frames());
    for slot in pipelines.iter() {
        if let Some(p) = slot.as_ref() {
            if p.streamer.is_some() {
                match &p.backend {
                    EncodeBackend::Nvenc { _session, .. } => {
                        if let Err(e) = _session.reconfigure_bitrate(
                            NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR,
                            avg_bps,
                            max_bps,
                            vbv,
                            force_idr,
                        ) {
                            tracing::warn!("reconfigure bitrate: {e:?}");
                        }
                    }
                    EncodeBackend::Soft(_) => {
                        // An ffmpeg pipe has no live-reconfigure channel; the
                        // software (VVC) session keeps its spawn-time bitrate.
                        tracing::debug!(
                            "reconfigure bitrate skipped on '{}' (software encoder)",
                            p.name
                        );
                    }
                }
            }
        }
    }
}

/// Resolution ladder, by coded HEIGHT in px. The client/panel selects a ceiling
/// height; the network controller may step DOWN this ladder under sustained link
/// pressure and back UP as it clears. Each per-output native height is the
/// implicit top rung — only ladder rungs strictly below native are reachable for
/// that output. Descending order.
const RES_LADDER_H: &[u32] = &[1440, 1080, 720, 540];

/// Even-align down. NVENC + 4:2:0 chroma want even coded dimensions.
fn align_even(v: u32) -> u32 {
    v & !1
}

/// Derive the coded `(width, height)` for an output of native size
/// `nat_w`×`nat_h` at a target coded height `h`, preserving the native aspect
/// ratio and clamping to native. `h == 0` or `h >= nat_h` ⇒ native (no
/// downscale). Both dimensions are even-aligned.
fn coded_for_height(nat_w: u32, nat_h: u32, h: u32) -> (u32, u32) {
    if h == 0 || h >= nat_h {
        return (nat_w, nat_h);
    }
    let ch = align_even(h.max(2));
    let cw = align_even(((u64::from(nat_w) * u64::from(ch)) / u64::from(nat_h.max(1))) as u32)
        .clamp(2, nat_w);
    (cw, ch)
}

/// Convert a SHORT-SIDE resolution target (the client/panel "1080p" sense — lines
/// on the narrow dimension, matching the dial's short-side envelope cap) into the
/// height-equivalent the height-based ladder uses. Identity for a landscape output
/// (`out_w >= out_h`, where the short side IS the height); for a PORTRAIT output the
/// short side is the WIDTH, so scale by long/short. `0` (Auto) passes through; the
/// result is even-aligned. Without this, a 1080 target on a 1080×2400 portrait
/// output would cap HEIGHT to 1080 and shrink the whole render to 486×1080.
fn short_side_target_to_height(out_w: u32, out_h: u32, short_target: u32) -> u32 {
    if short_target == 0 || out_h <= out_w {
        short_target
    } else {
        ((u64::from(short_target) * u64::from(out_h) / u64::from(out_w.max(1))) as u32) & !1
    }
}

/// The next ladder height strictly BELOW `cur_h` for an output of native height
/// `nat_h` (only rungs `< nat_h` are usable), not dropping below `floor_h` (the
/// user-selected minimum; `0` = no floor → the lowest rung). `None` if already at
/// the floor / lowest reachable rung.
fn rung_below(nat_h: u32, cur_h: u32, floor_h: u32) -> Option<u32> {
    RES_LADDER_H
        .iter()
        .copied()
        .find(|&r| r < cur_h && r < nat_h && r >= floor_h)
}

/// The next ladder height strictly ABOVE `cur_h` but not exceeding `ceiling_h`
/// (the user/client selection; `0` = native), for an output of native height
/// `nat_h`. Returns `None` if already at the ceiling. `native` is a valid rung.
fn rung_above(nat_h: u32, cur_h: u32, ceiling_h: u32) -> Option<u32> {
    let cap = if ceiling_h == 0 || ceiling_h >= nat_h {
        nat_h
    } else {
        ceiling_h
    };
    // Candidate rungs above cur_h, plus native AND the cap itself, capped at the
    // ceiling. Including `cap` lets the climb reach a non-rung ceiling exactly
    // (e.g. a 900 ceiling that a non-GUI control or the future client ModeChange
    // could request) — otherwise the ladder would strand one rung below it.
    RES_LADDER_H
        .iter()
        .copied()
        .chain([nat_h, cap])
        .filter(|&r| r > cur_h && r <= cap)
        .min()
}

/// The effective auto-downscale floor: the user-selected minimum (`floor_h`)
/// clamped so it can never exceed the target ceiling (or native). `0` = no floor
/// (the ladder may drop to its lowest rung). Keeps `[floor, ceiling]` consistent
/// even if a caller sets a floor above the ceiling.
fn effective_floor(nat_h: u32, ceiling_h: u32, floor_h: u32) -> u32 {
    if floor_h == 0 {
        return 0;
    }
    let cap = if ceiling_h == 0 || ceiling_h >= nat_h {
        nat_h
    } else {
        ceiling_h
    };
    floor_h.min(cap)
}

/// `(exhausted, recovered)` state of the lever BELOW the resolution ladder
/// (bitrate when explicit RC is configured, else fps). The ladder only drops a
/// rung once the lower lever is *exhausted* and only recovers once it's back at
/// its cap. "Exhausted" requires the floor to be STRICTLY below the cap — a
/// degenerate config (bitrate cap ≤ the 2 Mbps floor, or fps pinned at the
/// floor) must not read as exhausted, or the ladder would latch to the bottom
/// rung on a perfectly healthy link.
fn lever_state(
    rc_caps: Option<(u32, u32)>,
    net_allowed_kbps: u32,
    floor_kbps: u32,
    net_allowed_fps: f32,
    fps_floor: f32,
    fps_cap: f32,
) -> (bool, bool) {
    match rc_caps {
        Some((avg_cap, _)) => {
            let floor = floor_kbps.min(avg_cap);
            (
                floor < avg_cap && net_allowed_kbps <= floor,
                net_allowed_kbps >= avg_cap,
            )
        }
        None => (
            fps_floor + 0.5 < fps_cap && net_allowed_fps <= fps_floor + 0.5,
            net_allowed_fps >= fps_cap - 0.5,
        ),
    }
}

/// Apply a coded resolution to a streaming pipeline IN PLACE — live-reconfigure
/// the NVENC session (no re-init; see [`Session::reconfigure_resolution`]),
/// announce the new geometry to the client (the existing `DG_STREAM_GEOMETRY`
/// path both clients already honour live), and record the coded size so the
/// render loop downscales frames to match. No-op when unchanged. Errors are
/// logged, not fatal — the rung simply doesn't move.
fn apply_coded_resolution(p: &mut Pipeline, cw: u32, ch: u32) {
    if (cw, ch) == (p.coded_w, p.coded_h) {
        return;
    }
    let EncodeBackend::Nvenc { _session, .. } = &p.backend else {
        // The software (VVC) encoder is spawned at a fixed size — an ffmpeg
        // rawvideo pipe can't change frame geometry mid-stream. The ladder
        // rung simply doesn't move; bandwidth pressure falls to bitrate RC.
        tracing::debug!(
            target: "compositor::res",
            "output={} coded-resolution change skipped (software encoder)",
            p.name
        );
        return;
    };
    match _session.reconfigure_resolution(cw, ch) {
        Ok(()) => {
            p.coded_w = cw;
            p.coded_h = ch;
            if let Some(s) = &p.streamer {
                s.set_geometry(cw, ch);
            }
            tracing::info!(
                target: "compositor::res",
                "output={} coded → {}x{} (native {}x{})",
                p.name, cw, ch, p.width, p.height
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "compositor::res",
                "output={} reconfigure_resolution {cw}x{ch} failed: {e:?}",
                p.name
            );
        }
    }
}

// Kept for reference only — DO NOT call on reconnect: re-initialising NVENC in a
// long-running process crashes the NVIDIA driver (see the reconnect handler above
// and lesson-nvenc-driver-crash). A safe rebuild would need a fresh process.
#[allow(dead_code)]
fn rebuild_pipeline_session(
    mut p: Pipeline,
    cuda: &std::sync::Arc<CudaContext>,
    fps: u32,
    codec: VideoCodec,
) -> Result<Pipeline> {
    use std::ffi::c_void;
    // NVENC-only (the VVC software backend has no session to rebuild). The old
    // chain drops in the correct order — bitstreams first (each borrows from
    // _session), then reg, then _session itself: the EncodeBackend::Nvenc
    // field order — when `p.backend` is overwritten at the end. To build the
    // new bits we need the same dptr, which we do still own. We work by:
    //   1. building the new session into a Box,
    //   2. transmuting a 'static ref out of it,
    //   3. building new reg + bitstreams against the new session,
    //   4. THEN dropping the old reg + old session.
    // (1)–(3) all use Option, so we can sequence the destructors.
    let encoder = Encoder::initialize_with_cuda(cuda.clone())
        .map_err(|e| anyhow::anyhow!("nvenc init [{}]: {e:?}", p.name))?;
    let encode_guid = codec_encode_guid(codec);
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| anyhow::anyhow!("preset [{}]: {e:?}", p.name))?;
    preset_config.presetCfg.gopLength = idr_period_frames(fps);
    preset_config.presetCfg.frameIntervalP = 1;
    // Codec-specific periodic IDR + parameter-set repetition, so a client
    // reconnecting onto this freshly-rebuilt session resyncs at the next IDR
    // (HEVC repeats VPS/SPS/PPS; AV1 repeats the sequence-header OBU).
    apply_codec_lowlatency(&mut preset_config.presetCfg, codec, fps);
    let mut init = EncoderInitParams::new(encode_guid, p.width, p.height);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);
    let new_session = Box::new(
        encoder
            .start_session(NV_ENC_BUFFER_FORMAT_ABGR, init)
            .map_err(|e| anyhow::anyhow!("session [{}]: {e:?}", p.name))?,
    );
    let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
        + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
        .max(1) as usize;
    // SAFETY: same self-ref pattern as Pipeline::new — Box gives a stable
    // address, and we ensure the new bitstreams + reg are stored back into
    // Pipeline in the same field order (bitstreams, reg, _session, cuda_buf)
    // so they drop in the right sequence.
    let session_ref: &'static Session =
        unsafe { std::mem::transmute::<&Session, &'static Session>(&*new_session) };
    let new_bitstreams: Vec<Bitstream<'static>> = (0..num_bufs)
        .map(|_| session_ref.create_output_bitstream())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("bitstream [{}]: {e:?}", p.name))?;
    let new_reg: RegisteredResource<'static, ()> = session_ref
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            p.cuda_buf.0 as *mut c_void,
            p.pitch,
        )
        .map_err(|e| anyhow::anyhow!("register [{}]: {e:?}", p.name))?;
    // Move the new chain in; the old bitstreams/reg/session drop here, in
    // EncodeBackend::Nvenc field-declaration order (bitstreams → reg → session).
    p.backend = EncodeBackend::Nvenc {
        bitstreams: new_bitstreams,
        reg: new_reg,
        _session: new_session,
        num_bufs,
    };
    Ok(p)
}

/// Severity of an NVENC encode/lock error — decides whether the render loop
/// survives the frame or the host must restart. See `GPU_ERROR_HANDLING_PLAN.md`.
#[allow(dead_code)] // `Degraded` is produced by the cadence path in a later phase (P3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    /// NVIDIA-documented retryable: skip this output's frame, keep streaming.
    Transient,
    /// GPU suspect but still encoding (cadence-derived; wired in P3).
    Degraded,
    /// Session poisoned or device gone: terminate so a FRESH process rebuilds NVENC.
    Fatal,
}

/// Classify an NVENC `ErrorKind`. Only the three NVIDIA "retry in a few ms / not
/// fatal" codes are Transient (the whole point of P1 — these must not crash the
/// host); everything else is conservatively Fatal. `classify` never returns
/// `Degraded` — that comes from the cadence watchdog in a later phase.
fn classify(kind: ErrorKind) -> Severity {
    match kind {
        ErrorKind::NeedMoreInput | ErrorKind::EncoderBusy | ErrorKind::LockBusy => {
            Severity::Transient
        }
        // Conservatively fatal: session poisoned or device gone — NoEncodeDevice,
        // Invalid*Device, DeviceNotExist, OutOfMemory, EncoderNotInitialized,
        // Generic, and any unknown kind. Recovery is a fresh process, never an
        // in-process NVENC re-init (that SIGSEGVs libnvcuvid).
        _ => Severity::Fatal,
    }
}

/// UNIX-ms now (for the durable fault record's timestamp).
fn now_unix_ms() -> i64 {
    (unix_nanos_now() / 1_000_000) as i64
}

/// Build the error returned on a FATAL encode fault, AND record it: flip the
/// shared health to sticky Faulted and persist a durable best-effort record so
/// the tray shows "faulted & gave up" after the process exits. The returned `Err`
/// propagates out of the render loop → `main` exits non-zero → systemd
/// `Restart=on-failure` respawns a fresh NVENC process (bounded by the StartLimit
/// breaker); reconnecting clients resync off the existing ~1s IDR.
fn fatal_err(output: &str, stage: &str, kind: ErrorKind) -> anyhow::Error {
    let reason = format!("nvenc {stage} FATAL [{output}] kind={kind:?}");
    set_gpu_health(HEALTH_FAULTED, &reason);
    control_ipc::write_gpu_fault(&control_ipc::GpuFaultRecord {
        reason: reason.clone(),
        last_xid: None,
        terminal: false,
        unix_ms: now_unix_ms(),
    });
    anyhow::anyhow!("{reason} — restarting host")
}

/// Software fault injection for the P1 harness (mode B): exercise the
/// classifier on a healthy GPU with no faulty card. Parsed from
/// `PHAROS_FAULT_INJECT="<kind>[:<period>]"` where kind ∈ {busy, need_more,
/// lock_busy, device_lost, oom, generic} and period = inject every Nth frame
/// (default 120 ≈ every 2s @60fps). Unset → `None` → zero runtime cost.
struct FaultInject {
    kind: ErrorKind,
    period: u64,
}

impl FaultInject {
    /// The injected kind if a fault is due on `frame` (never frame 0, so the
    /// opening IDR always goes out cleanly).
    fn due(&self, frame: u32) -> Option<ErrorKind> {
        if frame > 0 && u64::from(frame) % self.period == 0 {
            Some(self.kind)
        } else {
            None
        }
    }
}

fn parse_fault_inject() -> Option<FaultInject> {
    let spec = std::env::var("PHAROS_FAULT_INJECT").ok()?;
    let mut parts = spec.split(':');
    let kind = match parts.next()?.trim() {
        "busy" => ErrorKind::EncoderBusy,
        "need_more" => ErrorKind::NeedMoreInput,
        "lock_busy" => ErrorKind::LockBusy,
        "device_lost" => ErrorKind::DeviceNotExist,
        "oom" => ErrorKind::OutOfMemory,
        "generic" => ErrorKind::Generic,
        other => {
            tracing::warn!(
                target: "compositor::nvenc",
                "PHAROS_FAULT_INJECT: unknown kind '{other}' \
                 (want busy|need_more|lock_busy|device_lost|oom|generic) — ignoring"
            );
            return None;
        }
    };
    let period = parts
        .next()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(120)
        .max(1);
    tracing::warn!(
        target: "compositor::nvenc",
        "⚠ FAULT INJECTION ARMED kind={kind:?} period={period} (PHAROS_FAULT_INJECT) — test build behavior"
    );
    Some(FaultInject { kind, period })
}

// ── GPU health (P3) ─────────────────────────────────────────────────────────
// Render-loop-owned health state, also readable by the P4 watchdog thread (hence
// process-global statics, not locals). 0 = Healthy, 1 = Degraded, 2 = Faulted
// (sticky). See `GPU_ERROR_HANDLING_PLAN.md`.
static GPU_HEALTH: AtomicU8 = AtomicU8::new(HEALTH_HEALTHY);
/// UNIX-nanos stamp of the last completed render-loop iteration. The P4 watchdog
/// reads this to catch a fully-hung blocking `encode_picture` that never returns
/// (the in-loop counters can't fire if the call never comes back). Written each
/// frame; not yet read in P3.
static LAST_FRAME_NS: AtomicU64 = AtomicU64::new(0);
/// Human-readable reason behind the current non-Healthy state (empty when Healthy).
static FAULT_REASON: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// True while ≥1 client is connected (set by the status push). Gates the P4
/// watchdog heartbeat — a hung `encode_picture` can only happen while streaming.
static STREAMING: AtomicBool = AtomicBool::new(false);
/// True while the Steam-redirect marker is currently on disk (edge-tracked by
/// [`reconcile_redirect_marker`]). NOT the same as `STREAMING`: the marker is
/// suppressed while the primary output is mirroring the real desktop, so a game
/// the user starts from the mirrored desktop passes through to wayland-0 and
/// lands in the mirror instead of the unwatched virtual display. See
/// `app_launcher::redirect`.
static REDIRECT_MARKER_ON: AtomicBool = AtomicBool::new(false);
/// Desired stream fan-out mode: `true` = mirror (one shared encode for every
/// connected client, clamped to the weakest); `false` = single-client/pre-empt
/// (the validated default). Set from the control thread via [`set_mirror_enabled`]
/// (a plain flag, no render-loop round-trip — it doesn't touch the NVENC session).
/// Read by the status push to publish `HostStatus.stream_mode` AND injected into
/// the `QuicStreamer` (via [`mirror_flag`]) where the transport's keep-N fan-out
/// loop honors it. An `Arc<AtomicBool>` (not a plain static) so the transport can
/// hold a clone without depending on this compositor module.
static MIRROR_ENABLED: std::sync::LazyLock<std::sync::Arc<AtomicBool>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(AtomicBool::new(false)));

/// Host input backstop (issue #13): release EVERY piece of held input state on
/// the seat + pads when the connected client changes or a stale backlog is
/// purged. A departed client's key-up / button-up / stick-return events can never
/// arrive, so without this a held key, mouse button, or gamepad control sticks in
/// the live host session and fights the physical console user.
///
/// Releases: (1) every currently-pressed KEYBOARD key (from smithay's own
/// `pressed_keys()`, so it catches any held key, not just ones we remember);
/// (2) every POINTER button we injected and still hold (`held_ptr_buttons`,
/// since smithay's public `PointerHandle` exposes no held-button list); (3) all
/// virtual gamepad state via `host_input` (`release_all_pads`). Best-effort and
/// off the hot timing path (only fires on a client change / backlog, not per
/// frame). Idempotent: releasing an already-up control is harmless.
fn release_all_held(
    data: &mut CalloopData,
    host_input: &mut crate::host_input::HostInput,
    held_ptr_buttons: &mut std::collections::HashSet<u8>,
) {
    use smithay::backend::input::ButtonState;
    use smithay::input::pointer::ButtonEvent;

    let time = data.state.start_time.elapsed().as_millis() as u32;

    // (1) Held keyboard keys → Released. smithay tracks these in the keyboard's
    // internal `pressed_keys`; feed each back as a release through the same
    // `input()` path so xkb state + client focus stay consistent.
    if let Some(keyboard) = data.state.seat.get_keyboard() {
        let held = keyboard.pressed_keys();
        if !held.is_empty() {
            tracing::info!("input: releasing {} held keyboard key(s)", held.len());
        }
        for keycode in held {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<(), _>(
                &mut data.state,
                keycode,
                KeyState::Released,
                serial,
                time,
                |_, _, _| FilterResult::Forward,
            );
        }
    }

    // (2) Held pointer buttons → Released. We only ever inject evdev codes
    // 0x110 + button; release exactly the ones we still hold.
    if let Some(pointer) = data.state.seat.get_pointer() {
        if !held_ptr_buttons.is_empty() {
            tracing::info!(
                "input: releasing {} held pointer button(s)",
                held_ptr_buttons.len()
            );
        }
        let buttons: Vec<u8> = held_ptr_buttons.drain().collect();
        for button in buttons {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.button(
                &mut data.state,
                &ButtonEvent {
                    button: 0x110u32 + u32::from(button),
                    state: ButtonState::Released,
                    serial,
                    time,
                },
            );
            pointer.frame(&mut data.state);
        }
    }

    // (3) Gamepad state → neutral on every live virtual pad, plus drop any
    // undrained gyro motion.
    host_input.release_all_pads();
}

/// Set the desired mirror mode (see [`MIRROR_ENABLED`]). Called from the panel
/// control handler; takes effect on the transport's next accept boundary + the
/// next status push.
pub fn set_mirror_enabled(on: bool) {
    MIRROR_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether mirror mode is currently requested (see [`MIRROR_ENABLED`]).
pub fn mirror_enabled() -> bool {
    MIRROR_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A clone of the shared mirror flag to inject into [`transport::QuicStreamer`]
/// at construction, so the streamer's fan-out loop sees panel toggles live.
pub fn mirror_flag() -> std::sync::Arc<AtomicBool> {
    (*MIRROR_ENABLED).clone()
}
/// Set by the watchdog's NVML sampler while the GPU is in a *concerning* throttle
/// (thermal / power-brake — NOT a normal power-cap). An INDEPENDENT amber signal
/// merged into the wire health, so it never fights the render loop's slow-window
/// Degraded state machine.
static THROTTLE: AtomicBool = AtomicBool::new(false);
static THROTTLE_REASON: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
/// The most recent kernel Xid the watchdog scraped (0 = none), surfaced in the
/// Faulted wire state.
static LAST_XID: AtomicU32 = AtomicU32::new(0);

const HEALTH_HEALTHY: u8 = 0;
const HEALTH_DEGRADED: u8 = 1;
const HEALTH_FAULTED: u8 = 2;

/// Sliding-window Degraded trigger: Degraded once >= `SLOW_DEGRADE_K` slow events
/// (an encode > 200ms, or an `EncoderBusy`) fall within the last
/// `SLOW_WINDOW_FRAMES` frames; recovers to Healthy only after a fully-clean
/// window. Windowed — never a single spike (per the design review).
const SLOW_WINDOW_FRAMES: u32 = 240; // ~4s @60fps
const SLOW_DEGRADE_K: usize = 5;

/// Publish a GPU-health transition. `Faulted` is sticky — once set it never
/// downgrades in-process; recovery is a fresh process (P5). Only logs + writes on
/// an actual severity change, so it is cheap to call every frame.
fn set_gpu_health(severity: u8, reason: &str) {
    use std::sync::atomic::Ordering;
    let cur = GPU_HEALTH.load(Ordering::Relaxed);
    if cur == HEALTH_FAULTED && severity < HEALTH_FAULTED {
        return; // sticky: never un-fault in-process
    }
    if severity == cur {
        return;
    }
    GPU_HEALTH.store(severity, Ordering::Relaxed);
    {
        let mut r = FAULT_REASON.lock().unwrap_or_else(|e| e.into_inner());
        *r = reason.to_string();
    }
    match severity {
        HEALTH_HEALTHY => tracing::info!(target: "compositor::nvenc", "gpu health → Healthy"),
        HEALTH_DEGRADED => {
            tracing::warn!(target: "compositor::nvenc", "gpu health → Degraded: {reason}")
        }
        _ => tracing::error!(target: "compositor::nvenc", "gpu health → Faulted: {reason}"),
    }
}

/// Snapshot the current health as a wire [`control_ipc::GpuHealth`] for the
/// status push, merging the render loop's `GPU_HEALTH` state machine with the
/// watchdog's independent `THROTTLE` amber signal and last-seen Xid. Faulted
/// wins; otherwise Degraded if either the state machine or a throttle says so.
fn current_gpu_health() -> control_ipc::GpuHealth {
    use std::sync::atomic::Ordering;
    let health = GPU_HEALTH.load(Ordering::Relaxed);
    if health == HEALTH_FAULTED {
        let xid = LAST_XID.load(Ordering::Relaxed);
        return control_ipc::GpuHealth::Faulted {
            reason: FAULT_REASON
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            last_xid: (xid != 0).then_some(xid),
        };
    }
    let throttling = THROTTLE.load(Ordering::Relaxed);
    if health == HEALTH_DEGRADED || throttling {
        let reason = if health == HEALTH_DEGRADED {
            FAULT_REASON
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        } else {
            THROTTLE_REASON
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let throttle = throttling.then(|| {
            THROTTLE_REASON
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        return control_ipc::GpuHealth::Degraded { reason, throttle };
    }
    control_ipc::GpuHealth::Healthy
}

// ── P4: GPU-health watchdog (detached thread) ───────────────────────────────
// Reads the shared statics; NEVER calls an NVENC API (the `Session` is
// !Send/!Sync) — it only sets atomics/health and samples NVML + kmsg. Three
// independent cadences so an NVML/kmsg hang can't blind the heartbeat.

/// Best-effort systemd `sd_notify`. No-op when not run under a `Type=notify` unit
/// (manual runs), so it's safe to call unconditionally.
fn sd_notify(states: &[sd_notify::NotifyState]) {
    let _ = sd_notify::notify(false, states);
}

/// End the process *now* for a systemd restart, skipping C `atexit` handlers.
/// We must not run them: the CUDA/NVENC runtime registers teardown that can block
/// for ~10s trying to sync with the GPU — observed live, where a plain
/// `process::exit` hung long enough for the systemd watchdog to fire instead of a
/// clean exit, turning a ~2s restart into ~12s. The kernel reclaims all GPU
/// resources on process death regardless, so skipping userspace teardown is safe
/// and makes the restart near-instant. Logs are already flushed (tracing writes
/// synchronously) before any caller reaches here.
fn fast_restart_exit(code: i32) -> ! {
    // SAFETY: `_exit` simply ends the process; it's async-signal-safe and never
    // returns. We deliberately skip atexit handlers and stdio flushing.
    unsafe { libc::_exit(code) }
}

/// Set/clear the independent throttle amber signal (writes only on a change). The
/// reason is computed lazily, only when transitioning into a throttle.
fn set_throttle(active: bool, reason: impl FnOnce() -> String) {
    use std::sync::atomic::Ordering;
    if THROTTLE.swap(active, Ordering::Relaxed) == active {
        return; // no change
    }
    if active {
        let r = reason();
        tracing::warn!(target: "compositor::gpu", "{r}");
        *THROTTLE_REASON.lock().unwrap_or_else(|e| e.into_inner()) = r;
    } else {
        tracing::info!(target: "compositor::gpu", "GPU throttle cleared");
        THROTTLE_REASON
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// Is this kernel Xid restart-worthy (device gone / engine dead / uncorrectable
/// memory)? The soft classes (13 GR-exception, 31 MMU-fault, …) are app-induced
/// as often as not, so toggling health on them would just flap — they're logged.
fn xid_is_fatal(xid: u32) -> bool {
    matches!(xid, 79 | 62 | 48 | 45 | 43)
}

/// Short description for the Xids we surface.
fn xid_desc(xid: u32) -> &'static str {
    match xid {
        13 => "GR exception (often app, sometimes HW)",
        31 => "MMU fault",
        43 => "channel/engine reset",
        45 => "robust-channel preemptive cleanup",
        48 => "double-bit ECC error",
        62 => "GSP/PMU halt",
        79 => "GPU has fallen off the bus",
        _ => "see NVIDIA Xid reference",
    }
}

/// Extract the Xid number from a `/dev/kmsg` record, e.g.
/// `…;NVRM: Xid (PCI:0000:01:00.0): 79, pid=…` → `Some(79)`.
fn parse_xid(line: &str) -> Option<u32> {
    let after = line.split_once("NVRM: Xid")?.1;
    // Skip the "(PCI:…): " preamble if present, then read the leading digits.
    let after = after.split_once("): ").map(|(_, r)| r).unwrap_or(after);
    let digits: String = after
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// React to a scraped Xid: record it and, for the fatal classes, flip the shared
/// health to sticky Faulted.
fn handle_xid(xid: u32) {
    use std::sync::atomic::Ordering;
    LAST_XID.store(xid, Ordering::Relaxed);
    if !xid_is_fatal(xid) {
        tracing::warn!(target: "compositor::gpu", "kernel Xid {xid} — {}", xid_desc(xid));
        return;
    }
    // Fatal Xid → record + exit for a fresh process. Terminal-hard classes (79
    // off-the-bus / 62 GSP halt) can't be fixed by a restart; we still exit (the
    // StartLimit breaker bounds the retries to a few), but mark the record
    // `terminal` so the panel/tray say "cold boot required".
    let terminal = matches!(xid, 79 | 62);
    let reason = format!("kernel Xid {xid} — {}", xid_desc(xid));
    set_gpu_health(HEALTH_FAULTED, &reason);
    control_ipc::write_gpu_fault(&control_ipc::GpuFaultRecord {
        reason: reason.clone(),
        last_xid: Some(xid),
        terminal,
        unix_ms: now_unix_ms(),
    });
    tracing::error!(
        target: "compositor::gpu",
        "watchdog: fatal Xid {xid}{} → exiting (1) for a fresh process",
        if terminal {
            " (terminal — cold boot may be required)"
        } else {
            ""
        }
    );
    fast_restart_exit(1);
}

/// NVML telemetry sampler (best-effort). dlopen's libnvidia-ml via nvml-wrapper;
/// `None` if NVML/the driver isn't present. Re-fetches the device per sample so
/// it never holds a borrow of the `Nvml` across the loop.
struct NvmlSampler {
    nvml: nvml_wrapper::Nvml,
}

impl NvmlSampler {
    fn init() -> Option<Self> {
        match nvml_wrapper::Nvml::init() {
            Ok(nvml) => {
                tracing::info!(target: "compositor::gpu", "watchdog: NVML telemetry active");
                Some(Self { nvml })
            }
            Err(e) => {
                tracing::warn!(target: "compositor::gpu", "watchdog: NVML unavailable ({e}); telemetry off");
                None
            }
        }
    }

    fn sample(&mut self) {
        use nvml_wrapper::bitmasks::device::ThrottleReasons;
        use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
        let Ok(dev) = self.nvml.device_by_index(0) else {
            return;
        };
        let temp_c = dev.temperature(TemperatureSensor::Gpu).ok();
        let power_w = dev.power_usage().ok().map(|mw| mw / 1000);
        let gclk = dev.clock_info(Clock::Graphics).ok();
        let throttle = dev.current_throttle_reasons().ok();
        tracing::debug!(
            target: "compositor::gpu",
            "nvml temp={temp_c:?}C power={power_w:?}W gclk={gclk:?}MHz throttle={throttle:?}"
        );
        // Concerning = thermal or power-brake slowdown (NOT a normal SW power cap,
        // which is just the configured limit being enforced).
        if let Some(t) = throttle {
            let concerning = t.intersects(
                ThrottleReasons::HW_THERMAL_SLOWDOWN
                    | ThrottleReasons::SW_THERMAL_SLOWDOWN
                    | ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN
                    | ThrottleReasons::HW_SLOWDOWN,
            );
            set_throttle(concerning, || {
                let mut s = String::from("GPU throttling:");
                if t.contains(ThrottleReasons::HW_THERMAL_SLOWDOWN)
                    || t.contains(ThrottleReasons::SW_THERMAL_SLOWDOWN)
                {
                    s.push_str(" thermal");
                }
                if t.contains(ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN) {
                    s.push_str(" power-brake");
                }
                s
            });
        }
    }
}

/// `/dev/kmsg` reader scraping kernel NVRM Xid lines (best-effort). Seeks to the
/// end on open so it only sees NEW records, and reads non-blocking so a poll never
/// stalls the watchdog. `None` if `/dev/kmsg` isn't readable (the host's user unit
/// may lack permission → NVML-only).
struct KmsgXidReader {
    file: std::fs::File,
}

impl KmsgXidReader {
    fn open() -> Option<Self> {
        use std::io::Seek;
        use std::os::unix::io::AsRawFd;
        match std::fs::OpenOptions::new().read(true).open("/dev/kmsg") {
            Ok(mut f) => {
                let _ = f.seek(std::io::SeekFrom::End(0));
                // O_NONBLOCK so a caught-up read returns EAGAIN instead of blocking.
                unsafe {
                    let fd = f.as_raw_fd();
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                tracing::info!(target: "compositor::gpu", "watchdog: kmsg Xid scraper active");
                Some(Self { file: f })
            }
            Err(e) => {
                tracing::warn!(
                    target: "compositor::gpu",
                    "watchdog: /dev/kmsg unreadable ({e}); Xid scrape off (NVML-only)"
                );
                None
            }
        }
    }

    fn poll(&mut self) {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let line = String::from_utf8_lossy(&buf[..n]);
                    if let Some(xid) = parse_xid(&line) {
                        handle_xid(xid);
                    }
                }
                Err(_) => break, // EAGAIN (caught up) or a transient read error
            }
        }
    }
}

/// The detached `pharos-gpu-watchdog` body. Never returns.
fn gpu_watchdog_loop() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let mut nvml = NvmlSampler::init();
    let mut kmsg = KmsgXidReader::open();
    let mut last_nvml = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(200)); // ~5 Hz base tick

        // (a) Heartbeat — the ONLY catch for a fully-hung blocking encode that
        // never returns. Gated on streaming (an idle host that legitimately
        // paused must not be declared faulted; a hung encode only happens while
        // streaming anyway).
        if STREAMING.load(Ordering::Relaxed) {
            let last = LAST_FRAME_NS.load(Ordering::Relaxed);
            if last != 0 && unix_nanos_now().saturating_sub(last) > 1_500_000_000 {
                let reason = "render loop stalled >1.5s (heartbeat)";
                set_gpu_health(HEALTH_FAULTED, reason);
                control_ipc::write_gpu_fault(&control_ipc::GpuFaultRecord {
                    reason: reason.to_string(),
                    last_xid: None,
                    terminal: false,
                    unix_ms: now_unix_ms(),
                });
                tracing::error!(
                    target: "compositor::gpu",
                    "watchdog: render loop hung → exiting (1) for a fresh process"
                );
                fast_restart_exit(1);
            }
        }

        // (b) NVML telemetry ~2 Hz — best-effort; the seconds-before-fault context
        // that doubles as the 3070 Ti post-mortem.
        if last_nvml.elapsed() >= Duration::from_millis(500) {
            last_nvml = Instant::now();
            if let Some(s) = nvml.as_mut() {
                s.sample();
            }
        }

        // (c) kmsg Xid scrape every tick — authoritative for Xid 79/62 when NVML
        // hangs on a card that's gone.
        if let Some(k) = kmsg.as_mut() {
            k.poll();
        }
    }
}

/// Running-app + apps.toml state, probed on a background thread (never the render
/// loop). The probe bumps `generation` whenever `running` or `mtime` changes; the
/// render loop re-pushes the catalog on a generation change. See the probe thread
/// in [`run_capture_zc_multi`].
struct RunningProbe {
    running: std::collections::HashSet<String>,
    mtime: Option<std::time::SystemTime>,
    generation: u64,
}

/// Diagnostic (HANDOFF #1): log the frontmost toplevel's committed buffer pixel
/// format whenever it changes. During an on-device HDR validation this is the
/// ground truth for whether a game is actually submitting fp16 scRGB
/// (`Abgr16161616f` = true HDR content, the winewayland/DXVK path) versus falling
/// back to an 8-bit SDR buffer — the open question the color-management handshake
/// was blocking. Throttled to log-on-change; only meaningful in a 10-bit session.
/// Opt in with `RUST_LOG=…,compositor::color=debug`.
fn log_top_surface_buffer_format(space: &smithay::desktop::Space<smithay::desktop::Window>) {
    use smithay::backend::allocator::Buffer as _;
    use smithay::wayland::compositor::{with_states, BufferAssignment, SurfaceAttributes};
    thread_local! {
        static LAST: std::cell::Cell<Option<Fourcc>> = const { std::cell::Cell::new(None) };
    }
    let fourcc = space
        .elements()
        .last()
        .and_then(|w| w.toplevel())
        .and_then(|t| {
            with_states(t.wl_surface(), |states| {
                match &states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer
                {
                    Some(BufferAssignment::NewBuffer(buf)) => {
                        smithay::wayland::dmabuf::get_dmabuf(buf)
                            .ok()
                            .map(|d| d.format().code)
                    }
                    _ => None,
                }
            })
        });
    LAST.with(|last| {
        if last.get() != fourcc {
            last.set(fourcc);
            tracing::debug!(target: "compositor::color", "top toplevel committed buffer format = {fourcc:?}");
        }
    });
}

/// Read a Wayland selection pipe (a host app's clipboard) to EOF with a total
/// `timeout`, off the render thread. `read_fd` is consumed (closed on return). The
/// timeout stops a stuck/forked app that keeps the write end open from leaking this
/// thread forever. Returns the valid UTF-8 prefix (an invalid tail — e.g. a
/// multi-byte char split by the 1 MiB cap — is dropped), or `None` on empty/no data.
fn read_selection_text(read_fd: libc::c_int, timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    // Non-blocking so poll(2) drives the deadline instead of a read() that can hang.
    unsafe {
        let flags = libc::fcntl(read_fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(read_fd) }; // closes read_fd on drop
    let deadline = std::time::Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    'outer: loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Cap the poll wait at 1s so the outer deadline is re-checked periodically.
        let tmo = remaining.as_millis().min(1000) as libc::c_int;
        let pr = unsafe { libc::poll(&mut pfd, 1, tmo) };
        if pr < 0 {
            break; // poll error
        }
        if pr == 0 {
            continue; // timeout tick; re-check the deadline
        }
        loop {
            match f.read(&mut chunk) {
                Ok(0) => break 'outer, // EOF: the app closed its write end
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() >= core_proto::MAX_CLIPBOARD_BYTES {
                        buf.truncate(core_proto::MAX_CLIPBOARD_BYTES);
                        break 'outer;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break, // drained; re-poll
                Err(_) => break 'outer,
            }
        }
    }
    if buf.is_empty() {
        return None;
    }
    match std::str::from_utf8(&buf) {
        Ok(s) => Some(s.to_string()),
        // Deliver the valid prefix (handles a codepoint split by the byte cap)
        // rather than dropping the whole clipboard.
        Err(e) => {
            let valid = e.valid_up_to();
            if valid == 0 {
                None
            } else {
                Some(String::from_utf8_lossy(&buf[..valid]).into_owned())
            }
        }
    }
}

pub fn run_capture_zc_multi(cfg: &MultiCfg, inject_keys: &[u32]) -> Result<()> {
    // The NVENC library is loaded at runtime, not linked, so a box without the
    // NVIDIA driver gets here; say so plainly instead of panicking later
    // inside the first encoder session.
    if let Err(e) = nvidia_video_codec_sdk::safe::nvenc_library_available() {
        anyhow::bail!(
            "the Linux host encodes with NVIDIA NVENC, but {} could not be loaded ({e}). \
             Install the NVIDIA proprietary driver (its encode library ships with it), \
             or use the Windows host on Intel/AMD GPUs.",
            nvidia_video_codec_sdk::safe::NVENC_LIBRARY
        );
    }

    // Load (or generate) the host's long-term Ed25519 identity once; share
    // it with every per-output streamer so clients can pin the same key
    // regardless of which output they're watching.
    let id_path = crate::pair::identity_path()?;
    let identity = Arc::new(
        Identity::load_or_create(&id_path)
            .map_err(|e| anyhow::anyhow!("identity at {}: {e}", id_path.display()))?,
    );
    info!("Pharos identity {}", identity.public_hex());

    // Seed the live pre-emption policy from the persisted primary_pubkey
    // (issue #19) before any streamer accepts a client; SetPrimaryPeer /
    // ForgetPeer keep it in sync from here on.
    crate::pair::sync_primary_to_transport();

    // Apply each output's configured input gate up front, reviving the previously
    // dead `[[output]] input_enabled`. This makes the CONFIG authoritative at
    // startup (a controller that should work isn't silently gated off waiting for
    // the panel), while the panel can still flip a device live via SetInputEnabled.
    for oc in &cfg.outputs {
        if let Some(peer_hex) = oc.peer.as_deref().filter(|s| !s.is_empty()) {
            match crate::control::parse_pubkey_hex(peer_hex) {
                Some(pk) => {
                    transport::set_input_enabled(pk, oc.input_enabled);
                    info!(
                        "input gate (config): peer {}… input_enabled={}",
                        &peer_hex[..peer_hex.len().min(8)],
                        oc.input_enabled
                    );
                }
                None => tracing::warn!(
                    "output '{}': peer '{}' is not a 32-byte hex pubkey; input gate not applied",
                    oc.name,
                    peer_hex
                ),
            }
        }
    }

    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();
    let mut state = State::new(&mut event_loop, display);

    // Advertise wp_viewporter. GPU Wayland clients — mpv's gpu/gpu-next VO,
    // winewayland, gamescope — require it to set up their surfaces; without it
    // they tear down the Wayland output and fall back to X11 (which the media
    // launcher blocks by scrubbing DISPLAY), leaving a black captured output.
    // The single-output path creates this too; the multi path had been missing
    // it. Held for the run's lifetime — dropping it withdraws the global.
    let _viewporter = smithay::wayland::viewporter::ViewporterState::new::<State>(&dh);

    // Advertise wp_presentation + zwp_relative_pointer_manager_v1 +
    // zwp_pointer_constraints_v1 — the remaining three globals gamescope's
    // Wayland backend hard-requires (issue #38: its init aborts with
    // "Couldn't create Wayland objects." if ANY of its nine required globals
    // is missing; pharos already had the other six). Presentation feedback is
    // answered in the render loop at the frame-callback cadence with
    // `presentation_clock`; relative motion was already dispatched by the
    // input path; constraints activate via the handler in lib.rs.
    let presentation_clock = smithay::utils::Clock::<smithay::utils::Monotonic>::new();
    let _presentation = smithay::wayland::presentation::PresentationState::new::<State>(
        &dh,
        presentation_clock.id() as u32,
    );
    let _relative_pointer =
        smithay::wayland::relative_pointer::RelativePointerManagerState::new::<State>(&dh);
    let _pointer_constraints =
        smithay::wayland::pointer_constraints::PointerConstraintsState::new::<State>(&dh);

    // Advertise wp_color_manager_v1 so an HDR client (winewayland/DXVK, mpv) can
    // declare a PQ/scRGB surface instead of pre-flattening to 8-bit sRGB. The
    // committed per-surface image description is read at compositing time. Only
    // meaningful when the capture path is 10-bit — register alongside the HDR gate.
    //
    // GATED — OFF by default (opt in with PHAROS_ENABLE_COLOR_MGMT). The
    // wp_color_manager_v1 get_information handshake was completed in `47a6102` (the
    // display description now delivers the full primaries/luminances/target_luminance
    // info set + done, so a v2 client no longer drops to v1 and spin-binds). It stays
    // opt-in only because the end-to-end scRGB→PQ present has not yet been validated
    // on-device with a real Proton game (see HANDOFF.md). SDR content is still
    // tone-mapped to PQ by the compositing shader, so nothing is lost with it off.
    let _color_mgmt = if std::env::var_os("PHAROS_ENABLE_COLOR_MGMT").is_some() {
        info!(target: "compositor::color", "wp_color_manager_v1 ENABLED (PHAROS_ENABLE_COLOR_MGMT) — get_information handshake complete (47a6102); pending on-device game validation");
        Some(crate::color_mgmt::init(&dh))
    } else {
        None
    };

    // Shared GLES renderer + CUDA context. NVIDIA's EGL on `/dev/dri/renderD128`.
    let node = "/dev/dri/renderD128";
    let drm_fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| anyhow::anyhow!("open {node}: {e}"))?;
    let gbm = GbmDevice::new(drm_fd)?;
    let egl_display = unsafe { EGLDisplay::new(gbm)? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };

    // Advertise dmabuf so EGL clients (alacritty etc.) can submit GPU buffers.
    let formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
    let _dmabuf_global = crate::advertise_dmabuf(&mut state.dmabuf_state, &dh, node, formats);

    let cuda = CudaContext::new(0).map_err(|e| anyhow::anyhow!("cuda init: {e:?}"))?;
    cuda.bind_to_thread()
        .map_err(|e| anyhow::anyhow!("bind cuda: {e:?}"))?;
    let bridge = unsafe { CudaGlBridge::new()? };

    // HDR10 (static): dev-gated behind PHAROS_HDR while the wants_hdr wire
    // negotiation is still being built. When on, the whole capture→encode chain
    // widens to 10-bit — Abgr2101010 render target, ABGR10 NVENC input, HEVC
    // Main10 + BT.2020/PQ VUI, MDCV/CLL SEI re-emitted per IDR. HEVC-only; an
    // AV1 config ignores it (logged). SDR is unchanged when the flag is unset.
    let hdr = std::env::var_os("PHAROS_HDR").is_some()
        && match cfg.video_codec {
            VideoCodec::Hevc => true,
            other => {
                tracing::warn!(
                    "PHAROS_HDR set but codec is {} — HDR10 wired for HEVC only; staying SDR",
                    other.label()
                );
                false
            }
        };
    let target_fourcc = if hdr {
        Fourcc::Abgr2101010
    } else {
        Fourcc::Abgr8888
    };
    // Static HDR10 metadata payloads (built once; re-emitted on every IDR).
    let hdr_sei = if hdr {
        Some(crate::nvenc::hdr10_sei_payloads())
    } else {
        None
    };
    if hdr {
        info!("HDR10 ENABLED (dev PHAROS_HDR): HEVC Main10 + BT.2020/PQ + MDCV/CLL SEI");
    }
    // Compile the scRGB→PQ post-composite shader once (HDR only).
    let pq_program: Option<GlesTexProgram> = if hdr {
        Some(
            renderer
                .compile_custom_texture_shader(SCRGB_TO_PQ_SHADER, &[])
                .map_err(|e| anyhow::anyhow!("compile HDR scRGB→PQ shader: {e:?}"))?,
        )
    } else {
        None
    };
    // SDR→PQ for non-HDR-declared surfaces in a 10-bit session (SDR games, idle).
    let srgb_pq_program: Option<GlesTexProgram> = if hdr {
        Some(
            renderer
                .compile_custom_texture_shader(SRGB_TO_PQ_SHADER, &[])
                .map_err(|e| anyhow::anyhow!("compile HDR sRGB→PQ shader: {e:?}"))?,
        )
    } else {
        None
    };

    // Build one Pipeline per configured output. Wrapped in `Option` so we
    // can take ownership for a session-rebuild on reconnect (a fresh NVENC
    // session means the new client's first encoded frame is automatically
    // its session's opening IDR — no garbage frames).
    // `[network] ipv6` from config.toml: bind each QUIC listener dual-stack so it
    // accepts IPv6 + IPv4. Read once for all outputs. Default off (IPv4 wildcard).
    let ipv6 = host_core::config::load().network.ipv6;
    if ipv6 {
        info!("[network] ipv6 = true → binding QUIC listeners dual-stack ([::], IPv4-mapped)");
    }
    let mut pipelines: Vec<Option<Pipeline>> = Vec::with_capacity(cfg.outputs.len());
    for (i, oc) in cfg.outputs.iter().enumerate() {
        // Phase 3c (restart-into-panel-res): a prior run may have grown the primary
        // output to a connecting client's panel-native size and restarted us. A
        // FRESH NVENC init at that larger size is crash-safe (an in-process re-init
        // SIGSEGVs the driver — see lesson-nvenc-driver-crash). Per-dimension max so
        // the envelope only ever grows above config, never below — this is what
        // keeps the connect-time check (below) loop-free. Other outputs (extend
        // mode) keep their configured size; the panel grow is primary-output only.
        let oc = {
            let mut oc = oc.clone();
            // Single-output only: extend mode (N outputs) keeps each output's
            // configured grain — a panel-driven resize there would reshape one
            // client's output and isn't what extend is for.
            //
            // Apply the recorded client envelope VERBATIM (not max-with-config): the
            // connect-time check already computed the exact target — grown for a
            // bigger same-orientation panel, or the client's exact dims for an
            // orientation change (landscape↔portrait), where a per-dimension max
            // would produce a nonsensical mixed-orientation envelope. The file is
            // only ever present between a restart-write and this consume (cleared
            // just below once we've built), so it always holds a valid target.
            if i == 0 && cfg.outputs.len() == 1 {
                if let Some((fw, fh)) = control_ipc::read_stream_res() {
                    if fw > 0 && fh > 0 && (fw != oc.w || fh != oc.h) {
                        info!(
                            "output[0] envelope set to {fw}x{fh} (config {}x{}) for a client's \
                             panel res [stream-res file]",
                            oc.w, oc.h
                        );
                        oc.w = fw;
                        oc.h = fh;
                    }
                }
            }
            oc
        };
        let oc = &oc;
        let output = Output::new(
            oc.name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Pharos".into(),
                model: "Virtual".into(),
            },
        );
        let _global = output.create_global::<State>(&dh);
        let mode = OutputMode {
            size: (oc.w as i32, oc.h as i32).into(),
            refresh: (cfg.fps * 1000) as i32,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        state.space.map_output(&output, (oc.x, oc.y));

        let target: GlesTexture =
            renderer.create_buffer(target_fourcc, (oc.w as i32, oc.h as i32).into())?;
        let damage = OutputDamageTracker::from_output(&output);

        // The per-output linear CUDA buffer the GL blit fills each frame. NVENC
        // encodes straight from it (zero-copy); the software (VVC) path copies
        // it device→host instead. Allocated before the backend branch — both
        // backends need it.
        let pitch = oc.w * 4;
        let dptr = unsafe { curesult::malloc_sync((pitch * oc.h) as usize) }
            .map_err(|e| anyhow::anyhow!("cuMemAlloc [{}]: {e:?}", oc.name))?;

        let backend = if cfg.video_codec.is_nvenc() {
            // NVENC: one Encoder + Session per output. start_session(self) consumes
            // the Encoder, so the Session is fully owned.
            let encoder = Encoder::initialize_with_cuda(cuda.clone())
                .map_err(|e| anyhow::anyhow!("nvenc init [{}]: {e:?}", oc.name))?;
            // Codec is global to the config (HEVC default, AV1 on Ada+, AVC for
            // legacy clients); the GUID picks the union variant
            // `apply_codec_lowlatency` then configures.
            let encode_guid = codec_encode_guid(cfg.video_codec);
            let preset_guid = NV_ENC_PRESET_P4_GUID;
            let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
            let mut preset_config = encoder
                .get_preset_config(encode_guid, preset_guid, tuning)
                .map_err(|e| anyhow::anyhow!("preset [{}]: {e:?}", oc.name))?;
            // Cap the GOP + IDR period (see `idr_period_frames`) so a client that
            // reconnects mid-stream resumes decoding without a session-rebuild
            // dance. The default for ULTRA_LOW_LATENCY is effectively "infinite"
            // — fine for a single fixed connection, wrong for the multi-shot
            // streamer. gopLength governs intra-distance; `apply_codec_lowlatency`
            // sets the codec's IDR period + parameter-set repetition (HEVC
            // VPS/SPS/PPS, H.264 SPS/PPS, AV1 sequence-header OBU) so the
            // reconnecting decoder gets the headers an IDR/keyframe alone lacks.
            preset_config.presetCfg.gopLength = idr_period_frames(cfg.fps);
            preset_config.presetCfg.frameIntervalP = 1;
            apply_codec_lowlatency(&mut preset_config.presetCfg, cfg.video_codec, cfg.fps);
            if hdr {
                apply_hdr10(&mut preset_config.presetCfg, cfg.video_codec);
            }
            // W3b: opt-in explicit rate control. Without a configured target the preset's
            // heuristic RC is kept (today's behavior). With one, cap the bitrate so a high
            // res/fps stream can't outrun the bounded QUIC flow window (PHAROS_STREAM_WINDOW_KB)
            // — the ~2s-backlog recipe. CBR when only an average is set; capped-VBR when a
            // higher ceiling is given. VBV is a few frames deep (`vbv_buffer_frames`) so
            // it can absorb an IDR-sized spike instead of underflowing into blockiness.
            if let Some((avg_kbps, max_kbps)) = resolve_rc_bitrate(cfg) {
                let rc = &mut preset_config.presetCfg.rcParams;
                let capped_vbr = max_kbps > avg_kbps;
                rc.rateControlMode = if capped_vbr {
                    NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR
                } else {
                    NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR
                };
                rc.averageBitRate = avg_kbps.saturating_mul(1000);
                rc.maxBitRate = max_kbps.saturating_mul(1000);
                rc.vbvBufferSize =
                    (rc.averageBitRate / cfg.fps.max(1)).saturating_mul(vbv_buffer_frames());
                rc.vbvInitialDelay = rc.vbvBufferSize;
                info!(
                    "nvenc RC [{}]: {} avg={}kbps max={}kbps vbv={}B",
                    oc.name,
                    if capped_vbr { "capped-VBR" } else { "CBR" },
                    avg_kbps,
                    max_kbps,
                    rc.vbvBufferSize,
                );
            }
            let mut init = EncoderInitParams::new(encode_guid, oc.w, oc.h);
            init.preset_guid(preset_guid)
                .tuning_info(tuning)
                .framerate(cfg.fps, 1)
                .enable_picture_type_decision()
                // Resolution ladder (Phase 2): native is the envelope ceiling so the
                // session can be live-reconfigured DOWN to any rung without a re-init
                // (which would SIGSEGV the driver). The ladder never goes above native.
                .max_encode_resolution(oc.w, oc.h)
                .encode_config(&mut preset_config.presetCfg);
            let buffer_format = if hdr {
                NV_ENC_BUFFER_FORMAT_ABGR10
            } else {
                NV_ENC_BUFFER_FORMAT_ABGR
            };
            let session = Box::new(
                encoder
                    .start_session(buffer_format, init)
                    .map_err(|e| anyhow::anyhow!("session [{}]: {e:?}", oc.name))?,
            );
            let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
                + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
                .max(1) as usize;

            // SAFETY: see module-level "Why a self-referential Pipeline" — Box
            // gives a stable address, and the field order below ensures these
            // 'static borrows drop before the session does.
            let session_ref: &'static Session =
                unsafe { std::mem::transmute::<&Session, &'static Session>(&*session) };

            let bitstreams: Vec<Bitstream<'static>> = (0..num_bufs)
                .map(|_| session_ref.create_output_bitstream())
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| anyhow::anyhow!("bitstream [{}]: {e:?}", oc.name))?;

            let reg: RegisteredResource<'static, ()> = session_ref
                .register_generic_resource(
                    (),
                    NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                    dptr as *mut c_void,
                    pitch,
                )
                .map_err(|e| anyhow::anyhow!("register [{}]: {e:?}", oc.name))?;

            EncodeBackend::Nvenc {
                bitstreams,
                reg,
                _session: session,
                num_bufs,
            }
        } else {
            // VVC (H.266): no consumer GPU encodes it, so this output runs the
            // software ffmpeg/libvvenc path — see swenc.rs for the latency
            // contract and its limitations vs NVENC. Frames leave the GPU via a
            // device→host copy of the same CUDA buffer NVENC would encode from.
            match crate::swenc::SoftEncoder::spawn(
                oc.w,
                oc.h,
                cfg.fps,
                idr_period_frames(cfg.fps),
                resolve_rc_bitrate(cfg),
            ) {
                Ok(enc) => EncodeBackend::Soft(enc),
                Err(e) => {
                    // Self-heal: a persisted VVC override that can't spawn would
                    // crash-loop this process under systemd until the StartLimit
                    // breaker gives up. Clear the sidecar so the NEXT start falls
                    // back to the multi.toml codec (HEVC default), and exit with
                    // the real error in the journal.
                    if let Ok(dir) = crate::pair::config_dir() {
                        let _ = std::fs::remove_file(codec_sidecar(&dir.join("multi.toml")));
                    }
                    return Err(anyhow::anyhow!(
                        "software VVC encoder failed to start [{}]: {e:#}; cleared the codec \
                         override so the next start returns to the configured codec",
                        oc.name
                    ));
                }
            }
        };

        let (streamer, file) = if let Some(addr_str) = oc.addr.strip_prefix("quic://") {
            let a: SocketAddr = addr_str
                .parse()
                .map_err(|e| anyhow::anyhow!("addr {addr_str} [{}]: {e}", oc.name))?;
            (
                Some(transport::QuicStreamer::start_with_authorizer(
                    a,
                    identity.clone(),
                    crate::pair::client_authorizer(),
                    mirror_flag(),
                    ipv6,
                )?),
                None,
            )
        } else {
            (None, Some(std::fs::File::create(&oc.addr)?))
        };

        // Announce the encode geometry so the client sizes its decoder to the real
        // resolution (vs. guessing 1080p and shearing every NV12 frame), and the
        // colorimetry so the client only runs its HDR→SDR tonemap on a real HDR
        // stream (issue #16). HDR is fixed per process here (PHAROS_HDR at startup),
        // so this one announce holds for the session.
        if let Some(s) = &streamer {
            s.set_geometry(oc.w, oc.h);
            s.set_hdr(hdr);
            // TODO(cursor-mode): Linux host should set_cursor_captured from the
            // smithay pointer-constraints protocol / null-cursor-surface state, so the
            // desktop client auto-locks its cursor during gameplay and frees it in
            // menus (same DG_CURSOR_MODE follow the Windows host already drives). This
            // pass wired the Windows host + client only; the compositor stays FREE
            // here (never announces), so a Linux-host client keeps today's manual-grab
            // behaviour until this is done.
        }

        info!(
            "output[{i}] '{}' {}x{} @ ({},{}) → {}",
            oc.name, oc.w, oc.h, oc.x, oc.y, oc.addr
        );

        pipelines.push(Some(Pipeline {
            backend,
            cuda_buf: CudaBuf(dptr),
            pitch,
            width: oc.w,
            height: oc.h,
            output,
            target,
            damage,
            name: oc.name.clone(),
            addr_label: oc.addr.clone(),
            // Resolution ladder starts at native (coded == native, no downscale,
            // Auto ceiling); the actuator moves it once a target is selected.
            coded_w: oc.w,
            coded_h: oc.h,
            res_ceiling_h: 0,
            res_floor_h: 0,
            scale: None,
            hdr_scratch: None,
            streamer,
            file,
            // Built LAZILY on the first idle frame (see the render loop), not
            // here — baking it at startup (PNG decode + composite) delays the
            // host's sd_notify READY and can trip the systemd start-timeout.
            idle: None,
            #[cfg(feature = "portal-mirror")]
            source: CaptureSource::Virtual,
        }));
    }

    // All pipelines built (the primary at its possibly-grown size). The stream-res
    // file has done its one job — carrying the grown size across this restart hop —
    // so clear it. Otherwise a size grown for one client would persist the whole
    // boot (tmpfs) and keep inflating the envelope even after the user shrinks their
    // configured resolution. A genuinely-bigger client just re-records it on connect.
    control_ipc::clear_stream_res();

    // Desktop-mirror master switch (issue #6, `config.toml [compositor]
    // enable_virtual_displays`, default true — unchanged behavior). When
    // OFF, the primary output starts mirroring the REAL desktop instead of
    // an isolated virtual one; `ControlCmd::LaunchApp` can still flip it
    // per-app afterwards. Only the primary output is ever affected — an
    // extend layout's secondary outputs stay virtual regardless (mirroring
    // is a single real-desktop session, not one per output).
    #[cfg(feature = "portal-mirror")]
    if !host_core::config::load().compositor.enable_virtual_displays {
        if let Some(Some(p)) = pipelines.first_mut() {
            if let Err(e) = activate_physical_mirror(p) {
                tracing::warn!(
                    "mirror: enable_virtual_displays=false but mirror activation failed: {e:#}; \
                     staying on the virtual display"
                );
            }
        }
    }

    let socket_name = state.socket_name.clone();

    // Seamless Steam-game display redirect (opt-in): clear any orphan launch-env
    // marker from a crashed prior run, and remove ours on graceful teardown. The
    // marker is (re)written on the streaming edge in the status block below; the
    // shim's pid+start-ticks staleness guard covers process::exit paths (which
    // skip Drop). See app_launcher::redirect.
    struct MarkerGuard;
    impl Drop for MarkerGuard {
        fn drop(&mut self) {
            app_launcher::redirect::remove_marker();
        }
    }
    let _marker_guard = app_launcher::redirect::enabled().then(|| {
        app_launcher::redirect::sweep_stale_marker();
        // Same reasoning for the per-app env fragments the launcher arms for a
        // Steam handoff: a crashed prior run must not leave a game's env armed.
        app_launcher::redirect::sweep_app_env();
        MarkerGuard
    });

    let mut data = CalloopData {
        state,
        display_handle: dh,
    };

    let child = match &cfg.launch {
        Some(cmd) => {
            info!("launching client `{cmd}` on {socket_name:?}");
            let mut parts = cmd.split_whitespace();
            let program = parts.next().unwrap_or(cmd);
            Some(
                std::process::Command::new(program)
                    .args(parts)
                    .env("WAYLAND_DISPLAY", &socket_name)
                    .env_remove("DISPLAY")
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("spawn {cmd}: {e}"))?,
            )
        }
        None => None,
    };

    // Warmup: let the client connect, map a window, draw.
    let warmup = std::time::Instant::now();
    while warmup.elapsed() < Duration::from_millis(2000) {
        event_loop.dispatch(Some(Duration::from_millis(16)), &mut data)?;
        data.state.space.refresh();
        // A game can bind wp_color_manager_v1 and run its whole HDR handshake
        // during this 2s warmup; flush the deferred info done() here too so it
        // isn't starved of the terminator (see the render-loop flush + color_mgmt).
        crate::color_mgmt::flush_pending_info_done();
        let _ = data.display_handle.flush_clients();
    }

    // Per-stage frame timing (gated; default off). Always on when
    // PHAROS_FRAME_TIMINGS=1 — flushes one greppable line per ~5s window with
    // mean/p95/max for each stage of the render loop, tagged
    // `compositor::framing`. The next cadence regression is a single
    // `grep frame_timings_us` over the host log. Constructed here (above the
    // ControlServer spawn) so the server can hold a clone of the
    // subscriber-deadline handle and light instrumentation on demand.
    let mut framing = FrameTimings::from_env();
    if framing.enabled_by_env {
        tracing::info!(
            target: "compositor::framing",
            "frame_timings_us instrumentation ON via PHAROS_FRAME_TIMINGS (flushes every {}s)",
            framing.flush_every.as_secs(),
        );
    }
    let framing_deadline = framing.subscribers_handle();
    // Controllers-tab subscriber gate. The status push otherwise rebuilds the
    // per-controller liveness vector by walking `evdev::enumerate()` once per
    // controller (~25 file opens × N pads → hundreds of ms of `status` stage
    // time on a busy host). The panel's Controllers tab `SubscribeControllers`
    // calls poke this on every render tick; when it's stale (tab unmounted /
    // panel gone) the build is skipped entirely.
    let controllers_deadline = SubscriberDeadline::fresh();

    // --- Control server (pharos-panel) ---------------------------------------
    // Spawn BEFORE the client-wait so an idle host (running, waiting for a
    // Trireme) is already visible to the panel rather than the control socket
    // being absent. Bind failure → a never-yielding command receiver, so
    // streaming is unaffected. `status` is refreshed per render-loop iteration.
    use crate::control::{ControlCmd, ControlServer};
    use std::sync::Mutex;
    let status = Arc::new(Mutex::new(crate::control::default_status()));
    let (_control_server, ctrl_rx) = ControlServer::spawn(
        status.clone(),
        framing_deadline,
        controllers_deadline.clone(),
    );
    // Set by ControlCmd::Relaunch (here in the client-wait, or in the main loop):
    // break, tear down, then re-exec.
    let mut relaunch_requested = false;

    // Wait for at least one streaming target to be ready before the loop runs
    // hot. We still skip individual outputs per-frame until they're ready
    // (see Pipeline::is_streaming_target_ready) so a late-connecting client
    // doesn't get a backlog burst — its session starts at its first encode.
    let any_streamer = pipelines
        .iter()
        .any(|p| p.as_ref().is_some_and(|p| p.streamer.is_some()));

    // --- Audio: capture the session's isolated sink --------------------------
    // Apps launched into the virtual display are routed to a per-session null
    // sink (via `PULSE_SINK`); we capture THAT sink's monitor, not the host's
    // desktop default. This streams the game's audio, keeps the physical
    // desktop's audio (a YouTube tab, notifications) off the wire, and stops the
    // couch game from playing out the host's speakers. The capture thread is
    // gated on a connected client (costs nothing pre-connect, resumes across
    // reconnects); a reconciler pulls stray sink-inputs (Steam games that
    // bypassed `PULSE_SINK`) onto our sink. All non-fatal: no PipeWire ⇒
    // video-only. `_audio_sink` lives for the whole session; its Drop unloads
    // the module at teardown.
    let _audio_sink = if any_streamer {
        match media_audio::NullSink::create("pharos_stream") {
            Ok(sink) => {
                app_launcher::set_session_audio_sink(Some(sink.sink_name().to_string()));
                let monitor = sink.monitor();
                let audio = pipelines
                    .iter()
                    .flatten()
                    .filter_map(|p| p.streamer.as_ref())
                    .next()
                    .map(|s| s.audio_handle());
                if let Some(audio) = audio {
                    std::thread::Builder::new()
                        .name("pharos-audio".into())
                        .spawn(move || {
                            let r = media_audio::capture_opus(
                                Some(&monitor),
                                media_audio::DEFAULT_BITRATE,
                                None,
                                |pkt| {
                                    if audio.is_connected() {
                                        audio.send(pkt);
                                    }
                                },
                            );
                            if let Err(e) = r {
                                tracing::warn!(
                                    "audio capture stopped: {e:#} (streaming video only)"
                                );
                            }
                        })?;
                }
                let sink_name = sink.sink_name().to_string();
                let socket = socket_name.to_string_lossy().into_owned();
                std::thread::Builder::new()
                    .name("pharos-audio-route".into())
                    .spawn(move || loop {
                        media_audio::reconcile_virtual_display_audio(&socket, &sink_name);
                        std::thread::sleep(Duration::from_millis(1500));
                    })?;
                Some(sink)
            }
            Err(e) => {
                tracing::warn!("audio sink setup failed: {e:#} (streaming video only)");
                None
            }
        }
    } else {
        None
    };

    if any_streamer {
        // The host is a persistent daemon: it idles here until a client
        // connects and NEVER self-terminates for lack of one — losing a client
        // (or never getting one) must never tear down the process and any media
        // on the wall with it. The panel can still stop it (Relaunch, drained
        // below). PHAROS_CLIENT_WAIT_SECS is an opt-in hard give-up for headless
        // test harnesses (>0 = bail after N seconds); unset/0 = wait forever.
        // Once a client connects the main loop runs forever and rides out
        // reconnects, so this only ever governs the pre-first-client idle.
        let wait_secs: u64 = std::env::var("PHAROS_CLIENT_WAIT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if wait_secs > 0 {
            info!("waiting up to {wait_secs}s for at least one Trireme to connect…");
        } else {
            info!("idle — waiting for at least one Trireme to connect…");
        }
        // Tell systemd we're up NOW — initialised and listening — even though no
        // client has connected yet. READY must NOT wait for the first client: under
        // Type=notify a client-less host would otherwise never leave "activating",
        // hit the start timeout, get killed, and loop forever (an idle host could
        // never come up at all). The READY below (post-wait, unconditional) is then
        // a harmless duplicate. We also pet the watchdog through this wait so an
        // idle host doesn't trip WatchdogSec before the render loop — which owns the
        // ping — is reached.
        sd_notify(&[sd_notify::NotifyState::Ready]);
        let mut idle_watchdog_ping = std::time::Instant::now();
        let wait_start = std::time::Instant::now();
        while !pipelines
            .iter()
            .any(|p| p.as_ref().is_some_and(Pipeline::is_streaming_target_ready))
        {
            event_loop.dispatch(Some(Duration::from_millis(16)), &mut data)?;
            data.state.space.refresh();
            // Flush deferred wp_image_description_info_v1.done() destructors here
            // too — an app (game) can bind wp_color_manager_v1 and run its HDR
            // handshake while we're still idle waiting for the first Trireme; if
            // done() only fired from the render loop the client would stall on an
            // incomplete get_information and abandon the v2 manager.
            crate::color_mgmt::flush_pending_info_done();
            let _ = data.display_handle.flush_clients();
            if idle_watchdog_ping.elapsed() >= std::time::Duration::from_secs(2) {
                sd_notify(&[sd_notify::NotifyState::Watchdog]);
                idle_watchdog_ping = std::time::Instant::now();
            }
            // Honor a panel Relaunch even before any client connects.
            if crate::control::drain_prestream(&ctrl_rx) {
                relaunch_requested = true;
                break;
            }
            if wait_secs > 0 && wait_start.elapsed() > Duration::from_secs(wait_secs) {
                anyhow::bail!("no client connected within {wait_secs}s");
            }
        }
        if !relaunch_requested {
            info!("at least one Trireme connected — starting stream");
        }
    }

    if !inject_keys.is_empty() {
        let surface = data
            .state
            .space
            .elements()
            .next()
            .and_then(|w| w.toplevel())
            .map(|t| t.wl_surface().clone());
        if let Some(surface) = surface {
            let keyboard = data.state.seat.get_keyboard().unwrap();
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(&mut data.state, Some(surface), serial);
            info!("keyboard focus set; injecting {} keys", inject_keys.len());
        }
    }
    let mut key_idx = 0usize;

    // W8 pacing target. `fps_auto` makes it ADAPTIVE: start at `fps` (the ceiling)
    // and let the 1s control loop settle it to the sustainable rate derived from
    // measured per-frame work; otherwise `fps` is a fixed cap (today). `current_pace`
    // is the live per-frame interval the relative pacer below uses.
    let mut auto_pace = cfg.fps_auto;
    // `cfg_fps_cap` = the config/panel fps ceiling (cfg.fps, adjustable live via
    // SetFps). `fps_cap` = the EFFECTIVE cap = cfg_fps_cap capped by the connected
    // client's panel refresh, recomputed each 1s window — so the host never feeds a
    // 60Hz panel 144fps (the decoder-overload corruption fix).
    let mut cfg_fps_cap: f32 = (cfg.fps as f32).max(1.0);
    let mut fps_cap: f32 = cfg_fps_cap;
    // The adaptation floor (and direct-app launch cap floor). Default 24; a client
    // fps range (DG_CLIENT_FPSPREF) raises the min and lowers cfg_fps_cap to its max.
    let mut fps_floor: f32 = 24.0;
    let mut target_fps: f32 = fps_cap;
    let mut current_pace = Duration::from_secs_f64(1.0 / target_fps as f64);
    // EWMA of raw per-frame WORK time (µs; excludes the pace sleep) — the auto loop
    // derives the sustainable rate from this. Accumulated per 1s wire window.
    let mut work_us_ewma: f32 = 1e6 / fps_cap;
    let mut work_us_in_window: u64 = 0;
    let loop_start = std::time::Instant::now();
    let mut next_deadline = loop_start;
    let unlimited = cfg.frames == 0;

    // (Control server + `status`/`ctrl_rx` were spawned above, before the
    // client-wait, so an idle host is visible to the panel.)
    let mut media_child: Option<std::process::Child> = None;
    let mut media_path: Option<String> = None;
    // Live preview: `Some(deadline)` while on (auto-stops at the 20s deadline);
    // `last_preview` throttles the JPEG write to ~2fps.
    let mut preview_until: Option<std::time::Instant> = None;
    let mut last_preview = std::time::Instant::now();
    // The status snapshot is what the panel polls over control-ipc; the panel
    // polls at ~1 Hz, so rebuilding the HostStatus 60 times per second is pure
    // overhead. The Controllers section in particular calls `evdev::enumerate()`
    // per controller per build to populate ControllerBus, which is ~30 file opens
    // each; at 60 Hz with two controllers, that's ~7k file opens/sec and shows
    // up as visible encode-pacing jitter on a busy host. ~4 Hz is more than
    // enough for panel responsiveness and well under the panel's poll cadence.
    let mut last_status_push = std::time::Instant::now() - std::time::Duration::from_secs(1);
    const STATUS_PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
    // Controllers liveness has its own throttle on top of `STATUS_PUSH_INTERVAL`:
    // the panel polls at ~1 Hz and controllers change on the seconds-to-minutes
    // timescale (plug, repair), so once per second is plenty. Combined with the
    // subscriber gate below, this caps the `evdev::enumerate()` cost at ~1×/s
    // while one panel tab is open and 0 otherwise.
    let mut last_controllers_build = std::time::Instant::now() - std::time::Duration::from_secs(2);
    const CONTROLLERS_BUILD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    // Network input back-channel: drain the primary client's input and dispatch
    // keyboard/mouse to the seat so a focused app (the media player, an app) is
    // controllable from the client. Gamepad/gyro on the multi host is a separate
    // follow-up (dropped here). Pointer coords scale against the primary output.
    let mut input_rx = pipelines
        .iter()
        .flatten()
        .filter_map(|p| p.streamer.as_ref())
        .find_map(|s| s.take_input_rx());
    // Index of the pipeline whose streamer owns `input_rx` (the FIRST with a
    // streamer — `find_map` above takes it from that same one). We poll its
    // `connected_addr()` each frame so the input drain can detect a client CHANGE
    // and purge/release the departed client's stale state (issue #13 host
    // backstop). `None` when no streamer exists (file-sink only).
    let input_streamer_idx = pipelines
        .iter()
        .position(|p| p.as_ref().is_some_and(|p| p.streamer.is_some()));
    // App launcher: push the catalog (with box art) to every client's relay
    // stream so the Trireme grid populates, and drain launch requests (tile taps)
    // from the primary client into the virtual display.
    let initial_running = crate::control::running_app_slugs();
    let app_catalog = crate::control::build_app_catalog(&initial_running);
    for s in pipelines
        .iter()
        .flatten()
        .filter_map(|p| p.streamer.as_ref())
    {
        s.send_app_catalog(app_catalog.clone());
    }
    let mut app_launch_rx = pipelines
        .iter()
        .flatten()
        .filter_map(|p| p.streamer.as_ref())
        .find_map(|s| s.take_app_launch_rx());
    // Steam Big Picture auto close+relaunch: a worker thread that shuts down a
    // running desktop Steam (blocking, off the render loop) hands its result back
    // here. `Ok(slug)` = Steam is down, force-launch the session now; `Err(msg)` =
    // couldn't close it, show the client a failure toast. Drained next to the
    // client app-relay requests each tick.
    let (deferred_launch_tx, deferred_launch_rx) =
        std::sync::mpsc::channel::<Result<String, String>>();
    // Clipboard sync (host ⇄ client). Client→host copies arrive on each streamer's
    // receiver; a host app's copy is read on a DETACHED thread (never the render
    // loop) which posts the text back via `clip_read_rx`; the render loop echo-gates
    // it before relaying. The two hashes break the copy↔paste feedback loop
    // (belt-and-braces to the client's own last-applied guard).
    //
    // Drain EVERY streamer's client→host receiver, not just the first: in extend
    // mode each output is its own streamer with its own unbounded queue, and an
    // undrained one grows without bound (OOM). Host→client goes to the first
    // (primary) streamer only — mirror/extend viewers don't get the host clipboard.
    let mut clipboard_in_rxs: Vec<_> = pipelines
        .iter()
        .flatten()
        .filter_map(|p| p.streamer.as_ref())
        .filter_map(|s| s.take_clipboard_in_rx())
        .collect();
    let clipboard_out = pipelines
        .iter()
        .flatten()
        .filter_map(|p| p.streamer.as_ref())
        .map(|s| s.clipboard_sender())
        .next();
    // Bound the per-frame client→host work: clipboard is last-writer-wins, so we
    // coalesce a burst to the newest text and touch the Wayland selection at most
    // once per frame. Without this an authenticated client flooding tiny messages
    // could stall the render loop into the P4 watchdog kill.
    const CLIPBOARD_MAX_DRAIN_PER_FRAME: usize = 256;
    let (clip_read_tx, clip_read_rx) = std::sync::mpsc::channel::<String>();
    // Last text pushed host→client / applied client→host (content hashes), for
    // echo suppression.
    let mut clip_last_sent: Option<u64> = None;
    let mut clip_last_recv: Option<u64> = None;
    // Text mime flavors: the order we REQUEST when reading a host selection, and
    // (as a set) what we OFFER on a compositor-owned selection from client text.
    const CLIPBOARD_TEXT_MIMES: [&str; 4] = [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "STRING",
    ];
    // Running-state + apps.toml are probed on a BACKGROUND thread, never the render
    // loop: running_app_slugs walks all of /proc and load_with_builtins parses
    // apps.toml — a ~2ms+ hitch (worse on a D-state process) that
    // lesson-render-loop-no-block forbids on the encode thread. The worker bumps a
    // generation when the running set or apps.toml mtime changes; the render loop
    // just reads the generation cheaply and re-pushes the catalog on a bump.
    let running_probe = std::sync::Arc::new(std::sync::Mutex::new(RunningProbe {
        running: initial_running,
        mtime: crate::control::apps_toml_mtime(),
        generation: 0,
    }));
    {
        let probe = running_probe.clone();
        let _ = std::thread::Builder::new()
            .name("pharos-running-probe".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(1500));
                let running = crate::control::running_app_slugs();
                let mtime = crate::control::apps_toml_mtime();
                let mut g = probe.lock().unwrap_or_else(|e| e.into_inner());
                if g.running != running || g.mtime != mtime {
                    g.running = running;
                    g.mtime = mtime;
                    g.generation = g.generation.wrapping_add(1);
                }
            });
    }
    let mut last_apps_check = std::time::Instant::now();
    let mut last_probe_gen: u64 = 0;
    let mut last_mirroring = false;
    let mut focus_surface: Option<
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    > = None;
    let mut last_win_count = 0usize;
    // Issue #13 host backstop state. `last_input_conn` mirrors the input
    // streamer's `connected_addr()` so we can spot a client CHANGE (disconnect or
    // preemption) ~4 Hz and release the departed client's held state; a lost
    // key-up / button-up / stick-return would otherwise stick in the live host
    // session. `held_ptr_buttons` tracks the pointer buttons WE injected (smithay's
    // public PointerHandle exposes no pressed-button list) so we release exactly
    // those. Held KEYBOARD keys come from smithay's own `pressed_keys()`; pads via
    // `host_input`. `INPUT_BACKLOG_BURST` mirrors the Windows stall-replay proxy.
    const INPUT_BACKLOG_BURST: usize = 256;
    let mut last_input_conn: Option<String> = None;
    let mut last_input_conn_check = std::time::Instant::now();
    let mut held_ptr_buttons: std::collections::HashSet<u8> = std::collections::HashSet::new();
    // Pointer clamp / gyro-cursor centering bounds. Use the LIVE primary output
    // size, NOT the config: a client can grow the envelope above config (e.g. a
    // 4K panel over a 1080p `multi.toml`), and the process rebuilds the output at
    // that size. Snapshotting `cfg.outputs[0]` here left the cursor clamped to
    // the 1080p config on a 4K canvas — reachable area was the top-left quadrant.
    // `p.width/p.height` are set once at pipeline build (the envelope) and never
    // mutated by DRC (which only reconfigures the coded/encode size), so this
    // snapshot stays correct for the session's lifetime.
    let (input_w, input_h) =
        primary_output_size(&pipelines).unwrap_or((cfg.outputs[0].w, cfg.outputs[0].h));
    // USB/IP-imported keyboards (vhci) → seat. Reads only vhci devices, so the
    // host's own keyboard is untouched. Always on; idle when none are imported.
    let usbkbd = crate::usbkbd::UsbKbd::spawn();
    // A Flydigi Vader's own gyro (vendor HID interface) → DSU. Idle unless a
    // Vader is attached AND `[dsu] enabled`.
    let flydigi = crate::flydigi::FlydigiGyro::spawn();

    // Gamepad slots + gyro→mouse + per-controller tuning. Lives in the shared
    // `host_input` module (used to be inline in the single-output path before
    // it was deleted). Cursor centers on the primary output's dimensions so a
    // gyro user lands at the middle of the picture on connect.
    let mut host_input = crate::host_input::HostInput::new(input_w, input_h);

    // Latency bench: drive it on the FIRST output that has a streamer. We take
    // that output's latency receiver once (it survives reconnects — the streamer
    // instance is preserved across a session rebuild) and stamp meta on its send.
    let bench_output: Option<usize> = pipelines
        .iter()
        .position(|p| p.as_ref().is_some_and(|p| p.streamer.is_some()));
    let mut latency_rx = bench_output.and_then(|idx| {
        pipelines[idx]
            .as_ref()
            .and_then(|p| p.streamer.as_ref())
            .and_then(|s| s.take_latency_rx())
    });
    // Continuous client QoS (delivered fps) from the same streamer — feeds the
    // network-aware pacing ceiling below.
    let mut qos_rx = bench_output.and_then(|idx| {
        pipelines[idx]
            .as_ref()
            .and_then(|p| p.streamer.as_ref())
            .and_then(|s| s.take_qos_rx())
    });
    let mut bench_remaining: u32 = 0;
    let mut bench_frame_idx: u64 = 0;
    let mut bench_reply: Option<
        std::sync::mpsc::Sender<Result<control_ipc::LatencyReport, String>>,
    > = None;
    let mut bench_deadline: u32 = 0;

    // Per-output trackers: did we ever send a frame; was the streamer ready
    // last iteration (for false→true reconnect detection).
    let mut first_sent = vec![false; pipelines.len()];
    let mut last_ready = vec![false; pipelines.len()];

    // (FrameTimings was constructed earlier — its handle is shared with the
    // control server so the panel can light instrumentation on demand.)

    // Wire ground-truth fps counter — frames pushed to streamer.send() per
    // rolling 1s window, alongside total bytes sent. This is the metric whose
    // absence let "host encode cadence 60fps" hide a real 2.5 fps wire rate.
    // ONE info line per second tagged `compositor::wire`.
    let mut wire_window_start = std::time::Instant::now();
    let mut wire_frames_in_window: u32 = 0;
    let mut wire_bytes_in_window: u64 = 0;
    // EWMA of measured fps (alpha 0.3) used as the status `fps` field instead
    // of the configured cfg.fps. Seeded to cfg.fps so a freshly-spawned host
    // doesn't briefly read 0 fps in the panel.
    let mut measured_fps_ewma: f32 = cfg.fps as f32;
    // Network-aware pacing (W8++): a ceiling on target_fps derived from the
    // client's delivered-rate QoS. Inert on a healthy link (delivered ≈ sent → the
    // ceiling rides at fps_cap); on a lossy/thin path it drops toward the delivered
    // rate so the host stops flooding (the bunker-LTE case: host sent 85fps, client
    // got 14 — fps is a weak BANDWIDTH lever under capped-VBR, but cutting the frame
    // RATE still trims packets/burst and is the only live lever we have). Steps DOWN
    // fast (2 bad ticks), UP slow (after several clear ticks) to avoid oscillation.
    let mut net_allowed_fps: f32 = fps_cap;
    let mut client_recv_fps_ewma: f32 = 0.0;
    let mut net_low_ticks: u32 = 0;
    let mut net_clear_ticks: u32 = 0;
    // Network-aware BITRATE controller — the real bandwidth lever (fps is weak
    // under capped-VBR). Active only when explicit RC is configured; then the fps
    // ceiling stays at cap and bitrate carries the adaptation, applied via the
    // live (crash-free, proven) NVENC reconfigure. Starts at the configured
    // average. INERT on a healthy link: delivered ≈ sent → never changes → never
    // reconfigures.
    let mut rc_caps = resolve_rc_bitrate(cfg); // Some((avg_kbps, max_kbps)); SetBitrate updates it
    let mut net_allowed_kbps: u32 = rc_caps.map(|(avg, _)| avg).unwrap_or(0);
    let mut last_reconfig_kbps: u32 = net_allowed_kbps;
    // Lowest bitrate we've forced a keyframe for this loss episode (so a marginal-
    // loss oscillation doesn't re-IDR every step), and how many ticks the client's
    // QoS has been silent (reverse-path datagrams can drop on a congested WAN —
    // recover toward the cap rather than staying latched low until reconnect).
    let mut net_lowwater_kbps: u32 = net_allowed_kbps;
    let mut qos_stale_ticks: u32 = 0;
    // Stage 3: the adaptation floor in kbps. Defaults to the hard NET_FLOOR_KBPS,
    // but a client bitrate range (DG_CLIENT_BITRATEPREF) raises it to the client's
    // chosen min so auto-recovery never drops below what the user asked for.
    let mut bitrate_floor_kbps: u32 = NET_FLOOR_KBPS;
    // Last client bitrate pref applied, packed (min_kbps<<32)|max_kbps (max=0 =
    // uncapped). Seeded 0 = the inert default the streamer atomic reports until a
    // client sends a real range, so it's a no-op from t=0 and only fires on change.
    let mut last_client_bitratepref: u64 = 0;
    // Connect-time bandwidth probe (issue #40): whether a probe has been kicked
    // for the CURRENT Auto client (reset when its pref clears on disconnect),
    // whether its result was applied, and the transport hint it was bounded by
    // (a mid-session WiFi↔Ethernet flip re-probes). PHAROS_PROBE=0 disables the
    // whole mechanism — Auto then resolves to the configured RC as before.
    let probe_enabled = std::env::var("PHAROS_PROBE").map_or(true, |v| v.trim() != "0");
    let mut probe_kicked = false;
    let mut probe_applied = false;
    let mut probe_transport: u8 = 0;
    // Last client framerate pref applied, packed (min_fps<<32)|max_fps (max=0=Auto).
    // Seeded 0 so it's inert until a client sends a real range; on change it sets
    // cfg_fps_cap (the encode-pace ceiling, ≤ cfg.fps) and fps_floor.
    let mut last_client_fpspref: u64 = 0;
    // Resolution ladder (Phase 2) controller state. The ladder is the rung BELOW
    // the bitrate/fps levers: it only steps once the lower lever is exhausted and
    // the link is still pressured. `res_low_ticks` / `res_clear_ticks` give it
    // asymmetric hysteresis (drop after a few seconds of pressure, recover slowly)
    // so it doesn't oscillate on WAN rain-fade. The current coded size lives
    // per-pipeline (`Pipeline::coded_h`); these only time the steps.
    let mut res_low_ticks: u32 = 0;
    let mut res_clear_ticks: u32 = 0;
    // Stage 2: last client-requested resolution pref applied per pipeline, packed
    // (target_h<<32)|floor_h. Seeded to 0 = the inert Auto/no-floor default the
    // streamer atomic also reports until a client sends a real pref — so a default
    // (0,0) is a no-op from t=0 and the block only fires on a GENUINE client range,
    // never spuriously stomping a panel-set resolution on the first window. The
    // client sends its [floor, target] continuously via DG_CLIENT_RESPREF; we apply
    // it only when it CHANGES, so a constant pref doesn't re-stomp every window.
    let mut last_client_respref = vec![0u64; pipelines.len()];
    // Per-output NVENC encode-slowness tracking. A run of 3 frames each >200ms
    // surfaces as "NVENC LIKELY WEDGED" — the documented libnvcuvid stall.
    let mut nvenc_consecutive_slow = vec![0u32; pipelines.len()];
    // Per-output running count of EncoderBusy transients survived without
    // crashing the host (P1 — surfaced to HostStatus in a later phase).
    let mut nvenc_busy = vec![0u64; pipelines.len()];
    // On-demand IDR rate limit: the streamer asks for a keyframe whenever a
    // reference is lost (a dropped AU, or a client `DG_REQUEST_IDR`). We honour
    // it by forcing the next encode to an IDR, but coalesce requests that land
    // within `min_idr_interval` of the last keyframe — a fresh IDR already gave a
    // clean reference, and this caps the IDR-byte cost under sustained loss (where
    // the pacer should be lowering bitrate/resolution instead). `last_idr_at`
    // tracks the last keyframe per output (periodic OR forced). Tunable via
    // PHAROS_MIN_IDR_INTERVAL_MS.
    let min_idr_interval = std::time::Duration::from_millis(
        std::env::var("PHAROS_MIN_IDR_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150),
    );
    let mut last_idr_at: Vec<Option<std::time::Instant>> = vec![None; pipelines.len()];
    // Software fault injection (mode B), parsed once. `None` unless
    // PHAROS_FAULT_INJECT is set → zero cost on the hot path.
    let fault_inject = parse_fault_inject();
    // P3: GPU-health state machine. `Ordering` is needed for the health atomics
    // below; reset the process-global health (statics persist across calls, but
    // there is only ever one render loop) and track slow encodes in a sliding
    // window so Degraded needs a sustained RATE, not a single spike (per review).
    use std::sync::atomic::Ordering;
    GPU_HEALTH.store(HEALTH_HEALTHY, Ordering::Relaxed);
    *FAULT_REASON.lock().unwrap_or_else(|e| e.into_inner()) = String::new();
    // A clean start means we are NOT faulted: clear any durable fault marker a
    // prior crashed run left behind (the tray reads it when the host is down).
    control_ipc::clear_gpu_fault();
    let mut slow_events: VecDeque<u32> = VecDeque::new();
    // P4: spawn the detached GPU-health watchdog (best-effort — a spawn failure
    // must not take down the host) and tell systemd we're ready (Type=notify).
    let _ = std::thread::Builder::new()
        .name("pharos-gpu-watchdog".into())
        .spawn(gpu_watchdog_loop);
    sd_notify(&[sd_notify::NotifyState::Ready]);
    let mut last_watchdog_ping = std::time::Instant::now();

    // One-shot NVENC config dump so a wedged-driver post-mortem has the
    // driver config captured at start. The session itself doesn't expose a
    // driver-version string from the SDK crate, so we log codec/dims/fps/preset
    // for each output here — driver version still comes from `nvidia-smi`
    // alongside this log.
    for (idx, slot) in pipelines.iter().enumerate() {
        let Some(p) = slot.as_ref() else { continue };
        match &p.backend {
            EncodeBackend::Nvenc { .. } => tracing::info!(
                target: "compositor::nvenc",
                "nvenc init output[{idx}] '{}' codec={} w={} h={} fps={} preset=P4 tuning=ULTRA_LOW_LATENCY",
                p.name, cfg.video_codec.label(), p.width, p.height, cfg.fps,
            ),
            EncodeBackend::Soft(_) => tracing::info!(
                target: "compositor::swenc",
                "software encode init output[{idx}] '{}' codec={} w={} h={} fps={} (ffmpeg/libvvenc)",
                p.name, cfg.video_codec.label(), p.width, p.height, cfg.fps,
            ),
        }
    }

    let mut i: u32 = 0;
    loop {
        // W8: per-iteration work clock — its elapsed at the pacer (below) is this
        // frame's raw work time (excludes the pace sleep), fed to the auto control loop.
        let frame_start = std::time::Instant::now();
        // Refresh the gate once per frame, then snapshot it into the local
        // cursor — so the eight `stage.mark()` calls across this frame all see
        // the same enabled bit. A subscriber poke between stages still takes
        // effect, just on the *next* frame: that way we never time half a
        // frame and report a misleading spike on the boundary.
        framing.refresh_gate();
        // Per-frame stage cursor for the framing histogram. When the gate is
        // off this is a no-op (`Instant::now()` is never called). When on,
        // each stage records the elapsed µs since the previous mark.
        let mut stage = StageCursor::start(framing.enabled);
        if !unlimited && i >= cfg.frames {
            break;
        }
        event_loop.dispatch(Some(Duration::from_millis(1)), &mut data)?;
        // Fire any deferred wp_image_description_info_v1.done() destructors now —
        // outside the dispatch that created them (firing them in-handler crashes
        // wayland-backend). See crate::color_mgmt.
        crate::color_mgmt::flush_pending_info_done();
        // …and PUSH it to the client immediately, in the same iteration as the
        // get_information that produced the info burst — NOT at the end-of-frame
        // flush_clients() (~16ms later, after render+encode). winewayland reads the
        // output's HDR image description synchronously: it waits for the info
        // `done()` terminator within its own handshake roundtrip and, not getting
        // it in time, treats the color manager as unready and re-binds forever
        // (the spin-bind + blank window). Flushing here delivers info+done together
        // within ~1ms so its very first pass completes and it presents.
        let _ = data.display_handle.flush_clients();
        data.state.space.refresh();
        // Drop Switch-To associations whose window is gone (normal app exit, or a
        // winewayland throwaway toplevel) so a later Switch-To can't raise a dead
        // surface. Cheap: no-op unless app_surfaces is non-empty.
        data.state.sweep_app_surfaces();
        // Center any fixed-size dialog that ignored the fullscreen configure (e.g.
        // Cyberpunk's REDprelauncher) instead of leaving it in the output's
        // top-left corner. No-op for a fullscreen app (size == output).
        data.state.center_windows();
        framing.dispatch += stage.mark();

        // Drain panel control commands (non-blocking). Each carries a oneshot
        // reply we answer immediately, except RunBench (delivered later, when
        // the client's WireLatencyReport arrives below).
        while let Ok(cmd) = ctrl_rx.try_recv() {
            match cmd {
                ControlCmd::PlayMedia {
                    path,
                    fullscreen,
                    subtitles,
                    audio,
                    reply,
                } => {
                    if let Some(mut c) = media_child.take() {
                        let _ = c.kill();
                    }
                    match crate::control::spawn_media_player(
                        &path,
                        fullscreen,
                        &subtitles,
                        &audio,
                        &socket_name,
                    ) {
                        Ok(child) => {
                            info!("panel: playing media '{path}' (fullscreen={fullscreen})");
                            media_child = Some(child);
                            media_path = Some(path);
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            tracing::warn!("panel: PlayMedia failed: {e}");
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                ControlCmd::StopMedia { reply } => {
                    if let Some(mut c) = media_child.take() {
                        let _ = c.kill();
                    }
                    media_path = None;
                    info!("panel: media stopped");
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::LaunchApp { slug, reply } => {
                    // Spawn the launched app and (when the entry has a real
                    // command — the built-in `desktop` slug has none) track
                    // the [`std::process::Child`] so per-frame `reap_exited_apps`
                    // can reset the active gyro mode to default when it exits.
                    // Cap the game render to the live effective stream fps (client
                    // panel refresh / chosen fps, ≤ cfg.fps) — NOT the raw cfg.fps —
                    // so a direct-launched app's MangoHud limiter matches the stream
                    // and doesn't render above it (the source of felt stutter).
                    let launch_fps = (fps_cap.round() as u32).max(1);
                    // Tag the launch so toplevels mapping within the claim window
                    // associate this slug → a Trireme client can Switch-To even a
                    // panel-launched app.
                    data.state.pending_app_launch = Some((slug.clone(), std::time::Instant::now()));
                    let out_size = primary_output_size(&pipelines);
                    let client = connected_client(&pipelines);
                    match crate::control::launch_app(
                        &slug,
                        &socket_name,
                        launch_fps,
                        out_size,
                        client.clone(),
                    ) {
                        Ok(child) => {
                            info!("panel: launched app '{slug}'");
                            // Per-app gyro policy: read the saved entry's mode
                            // (default = Enabled if the slug isn't in apps.toml
                            // — e.g. the built-in `desktop` slug, which has no
                            // user file). The host's IMU reader keeps running
                            // either way; this just gates dispatch.
                            let catalog = app_launcher::config::load_with_builtins();
                            let entry = catalog.find(&slug);
                            let mode = entry.map(|e| e.gyro_mode).unwrap_or_default();
                            host_input.set_app_gyro_mode(mode);
                            // Desktop-mirror (issue #6): flip the primary
                            // output's capture source to match this app's
                            // `LaunchTarget` (the built-in `desktop` slug
                            // defaults to `DuplicatePhysical`; a real app
                            // entry's own target may pull it back to
                            // `Virtual`). See `apply_capture_target`.
                            #[cfg(feature = "portal-mirror")]
                            apply_capture_target(&mut pipelines, entry.map(|e| &e.target));
                            if let Some(c) = child {
                                host_input.track_app_child(slug, c, client);
                            }
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            data.state.pending_app_launch = None;
                            tracing::warn!("panel: LaunchApp '{slug}' failed: {e}");
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                ControlCmd::RunBench { frames, reply } => {
                    // Bench needs the chosen output's client connected.
                    let ready = bench_output.is_some_and(|idx| {
                        pipelines[idx]
                            .as_ref()
                            .is_some_and(Pipeline::is_streaming_target_ready)
                    });
                    if bench_output.is_none() {
                        let _ = reply.send(Err("no streaming output (file mode)".into()));
                    } else if !ready {
                        let _ = reply.send(Err("no client connected".into()));
                    } else {
                        info!("panel: latency bench over {frames} frames");
                        bench_remaining = frames.max(1);
                        bench_frame_idx = 0;
                        bench_deadline = 600;
                        bench_reply = Some(reply);
                    }
                }
                ControlCmd::SetMode { mode, reply } => {
                    info!("panel: SetLayoutMode {mode:?} (file-driven; advisory)");
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::ReloadLayout { reply } => {
                    info!("panel: ReloadLayout (best-effort; restart applies a new layout)");
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::SetPreview { enabled, reply } => {
                    preview_until =
                        enabled.then(|| std::time::Instant::now() + Duration::from_secs(20));
                    info!(
                        "panel: live preview {}",
                        if enabled { "on (20s)" } else { "off" }
                    );
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::Relaunch { reply } => {
                    info!("panel: Relaunch — restarting host");
                    let _ = reply.send(Ok(()));
                    relaunch_requested = true;
                    break;
                }
                ControlCmd::SetControllerAlias {
                    vendor,
                    product,
                    alias,
                    reply,
                } => {
                    // Mutate the live profile (so a live-connected pad picks the
                    // new alias up immediately on its next reconnect / tuning
                    // re-push) *and* persist. Empty / whitespace-only aliases
                    // collapse to None to match the panel's clear-alias intent.
                    let trimmed = alias.and_then(|s| {
                        let t = s.trim().to_string();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    });
                    let mut prof = host_input.controller_profiles.get(vendor, product);
                    prof.alias = trimmed;
                    host_input.controller_profiles.set(vendor, product, prof);
                    let res = host_input
                        .controller_profiles
                        .save()
                        .map_err(|e| e.to_string());
                    if let Err(ref e) = res {
                        tracing::warn!(
                            "panel: SetControllerAlias {vendor:04x}:{product:04x} persist failed: {e}"
                        );
                    } else {
                        info!("panel: SetControllerAlias {vendor:04x}:{product:04x} ok");
                    }
                    let _ = reply.send(res);
                    // (No host→client tuning broadcast yet — see open_concerns;
                    // connected clients pick the new alias up on next reconnect.)
                }
                ControlCmd::GetFrameTimings { reply } => {
                    // The subscribe-poke was already applied by the control
                    // thread before the cmd reached us (it owns the
                    // deadline-handle clone), so the gate is already lit for
                    // the next ~5s and the snapshot we return here is from
                    // the live in-progress window.
                    let snap = framing.snapshot();
                    let _ = reply.send(Ok(snap));
                }
                ControlCmd::SetVideoCodec { codec, reply } => {
                    // NVENC can't swap codec in place, so persist the choice as
                    // the sidecar next to the panel's multi.toml and relaunch;
                    // the fresh process reads the override in load_multi_config
                    // and builds its sessions with the new codec.
                    //
                    // VVC first: it has no NVENC path — it software-encodes via
                    // ffmpeg's libvvenc — so preflight that BEFORE persisting +
                    // relaunching. A box without the right ffmpeg gets the error
                    // in the panel instead of a restart into a failed build.
                    let res = if codec == VideoCodec::Vvc {
                        crate::swenc::preflight()
                    } else {
                        Ok(())
                    }
                    .and_then(|()| crate::pair::config_dir().map_err(|e| e.to_string()))
                    .and_then(|dir| {
                        write_codec_override(&dir.join("multi.toml"), codec)
                            .map_err(|e| format!("persist video codec: {e}"))
                    });
                    match res {
                        Ok(()) => {
                            info!(
                                "panel: video codec → {} (relaunching encoder)",
                                codec.label()
                            );
                            let _ = reply.send(Ok(()));
                            relaunch_requested = true;
                        }
                        Err(e) => {
                            tracing::warn!("panel: SetVideoCodec persist failed: {e}");
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                ControlCmd::SetFps { auto, fps, reply } => {
                    // W8 live pacing from the panel — no relaunch. `auto` toggles the
                    // adaptive pacer; `fps` is the ceiling (auto) or fixed target.
                    // Clamped to the encode ceiling cfg.fps (the session can't be paced
                    // faster than it was configured for). Not persisted.
                    auto_pace = auto;
                    cfg_fps_cap = (fps as f32).clamp(1.0, cfg.fps as f32);
                    // Provisional; the next 1s wire window re-applies the client
                    // refresh cap (≤1s convergence).
                    fps_cap = cfg_fps_cap;
                    target_fps = if auto_pace {
                        target_fps.clamp(fps_floor, fps_cap)
                    } else {
                        fps_cap
                    };
                    current_pace = Duration::from_secs_f64(1.0 / target_fps.max(1.0) as f64);
                    info!(
                        "panel: pace → {} cap={:.0} target={:.0} (live)",
                        if auto_pace { "auto" } else { "fixed" },
                        fps_cap,
                        target_fps,
                    );
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::SetBitrate { kbps, reply } => {
                    // Live bitrate ceiling from the panel — in-place NVENC reconfigure,
                    // no relaunch. Sets the average-bitrate cap the network controller
                    // adapts BELOW (max derived 1.5× for capped-VBR). Switches the
                    // session to explicit RC even if it started on the preset heuristic.
                    // Not persisted (`multi.toml bitrate_kbps` is the default).
                    let kbps = kbps.max(1);
                    let max = kbps.saturating_mul(3) / 2;
                    rc_caps = Some((kbps, max));
                    net_allowed_kbps = kbps;
                    last_reconfig_kbps = kbps;
                    net_lowwater_kbps = kbps;
                    reconfigure_all_bitrate(&pipelines, kbps, kbps, max, cfg.fps, true);
                    info!("panel: bitrate ceiling → {kbps} kbps (max {max}, live)");
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::SetResolution { height, reply } => {
                    // Resolution ladder (Phase 2): set the target coded-height
                    // ceiling per streaming pipeline and snap to it now (per-output
                    // aspect-preserving, in-place NVENC reconfigure — no relaunch).
                    // 0 = Auto (native). The network controller may drop BELOW this
                    // later; this is the upper bound. Not persisted. Reset the
                    // ladder hysteresis so the new ceiling takes effect cleanly.
                    res_low_ticks = 0;
                    res_clear_ticks = 0;
                    for slot in pipelines.iter_mut() {
                        if let Some(p) = slot.as_mut() {
                            if p.streamer.is_some() {
                                // Short-side target → height-equivalent (identity in
                                // landscape, scaled in portrait); then clamp to native so
                                // an at/above-native selection is recorded honestly as
                                // Auto (0) — keeps the coded_h-vs-ceiling auto-drop
                                // discriminator (and the panel indicator) accurate.
                                let h_eq = short_side_target_to_height(p.width, p.height, height);
                                p.res_ceiling_h = if h_eq == 0 || h_eq >= p.height {
                                    0
                                } else {
                                    h_eq
                                };
                                let (cw, ch) = coded_for_height(p.width, p.height, h_eq);
                                apply_coded_resolution(p, cw, ch);
                            }
                        }
                    }
                    info!(
                        "panel: resolution ceiling → {} (live)",
                        if height == 0 {
                            "auto".to_string()
                        } else {
                            format!("{height}p")
                        }
                    );
                    let _ = reply.send(Ok(()));
                }
                ControlCmd::SetResolutionFloor { height, reply } => {
                    // Resolution ladder (Phase 2): set the MINIMUM coded height the
                    // auto-drop may reach (the floor of the adaptive range). 0 = no
                    // floor (lowest rung); >= native = never auto-drop. If a prior
                    // drop already took coded BELOW the new floor, snap it back up to
                    // the floor now (bounded by the ceiling). Reset hysteresis.
                    res_low_ticks = 0;
                    res_clear_ticks = 0;
                    for slot in pipelines.iter_mut() {
                        if let Some(p) = slot.as_mut() {
                            if p.streamer.is_some() {
                                // Short-side floor → height-equivalent (portrait-aware).
                                let h_eq = short_side_target_to_height(p.width, p.height, height);
                                p.res_floor_h = h_eq.min(p.height);
                                let floor =
                                    effective_floor(p.height, p.res_ceiling_h, p.res_floor_h);
                                if floor > 0 && p.coded_h < floor {
                                    let (cw, ch) = coded_for_height(p.width, p.height, floor);
                                    apply_coded_resolution(p, cw, ch);
                                }
                            }
                        }
                    }
                    info!(
                        "panel: min resolution → {} (live)",
                        if height == 0 {
                            "none".to_string()
                        } else {
                            format!("{height}p")
                        }
                    );
                    let _ = reply.send(Ok(()));
                }
            }
        }
        if relaunch_requested {
            break;
        }
        framing.ctrl += stage.mark();
        // Drain client-tile launch requests (non-blocking) → spawn into the
        // virtual display via the shared launcher path. Same per-app gyro
        // policy + child tracking as the panel-side LaunchApp handler above —
        // a Trireme tile launch must give the user the same gyro reset on
        // app exit, otherwise a gyro→mouse-mode game leaves the cursor
        // jittering on the desktop afterwards.
        {
            // Merge this tick's app actions: deferred Steam relaunches first (their
            // desktop-Steam shutdown already completed, so they're force-launches
            // that skip the intercept below), then live client relay requests.
            let mut app_msgs: Vec<(core_proto::AppMsg, bool)> = Vec::new();
            while let Ok(res) = deferred_launch_rx.try_recv() {
                match res {
                    Ok(slug) => app_msgs.push((core_proto::AppMsg::Launch { id: slug }, true)),
                    Err(message) => {
                        for s in pipelines
                            .iter()
                            .flatten()
                            .filter_map(|p| p.streamer.as_ref())
                        {
                            s.send_app_notice(message.clone());
                        }
                    }
                }
            }
            if let Some(rx) = app_launch_rx.as_mut() {
                while let Ok(msg) = rx.try_recv() {
                    app_msgs.push((msg, false));
                }
            }
            for (msg, force) in app_msgs {
                match msg {
                    core_proto::AppMsg::Launch { id } => {
                        // Steam Big Picture graceful handling: if this is the
                        // steam-session and a desktop Steam is already running, a
                        // second launch would attach THERE instead of in-stream. So
                        // instead of failing, tell the client, and shut Steam down +
                        // relaunch on a worker (steam::shutdown_and_wait BLOCKS —
                        // never on the render loop; lesson-render-loop-no-block).
                        // `force` deferred relaunches (Steam already down) skip this.
                        if !force {
                            let is_steam_session = app_launcher::config::load_with_builtins()
                                .find(&id)
                                .is_some_and(|e| {
                                    matches!(
                                        e.resolved_kind(),
                                        app_launcher::config::LaunchKind::SteamSession
                                    )
                                });
                            if is_steam_session && app_launcher::steam::is_ready() {
                                for s in pipelines
                                    .iter()
                                    .flatten()
                                    .filter_map(|p| p.streamer.as_ref())
                                {
                                    s.send_app_notice(
                                        "Steam was running on the desktop — closing it and \
                                         starting the session in-stream…"
                                            .to_string(),
                                    );
                                }
                                let tx = deferred_launch_tx.clone();
                                let slug = id.clone();
                                let _ = std::thread::Builder::new()
                                    .name("pharos-steam-relaunch".into())
                                    .spawn(move || {
                                        if app_launcher::steam::shutdown_and_wait(
                                            std::time::Duration::from_secs(15),
                                        ) {
                                            let _ = tx.send(Ok(slug));
                                        } else {
                                            let _ = tx.send(Err(
                                                "Couldn't close desktop Steam within 15s — \
                                                 quit it on the host, then launch again."
                                                    .to_string(),
                                            ));
                                        }
                                    });
                                continue; // the worker re-injects once Steam is down
                            }
                        }
                        let launch_fps = (fps_cap.round() as u32).max(1);
                        // Tag the launch (slug + now) so toplevels mapping within the
                        // claim window associate with this slug → Switch-To can raise
                        // it by id (launch-order association, robust to the XWayland
                        // pid indirection; last-one-wins handled in new_toplevel).
                        data.state.pending_app_launch =
                            Some((id.clone(), std::time::Instant::now()));
                        let out_size = primary_output_size(&pipelines);
                        let client = connected_client(&pipelines);
                        match crate::control::launch_app(
                            &id,
                            &socket_name,
                            launch_fps,
                            out_size,
                            client.clone(),
                        ) {
                            Ok(child) => {
                                info!("client: launched app '{id}'");
                                let catalog = app_launcher::config::load_with_builtins();
                                let entry = catalog.find(&id);
                                let mode = entry.map(|e| e.gyro_mode).unwrap_or_default();
                                host_input.set_app_gyro_mode(mode);
                                // Desktop-mirror (issue #6): same reconciliation
                                // the panel-side LaunchApp handler does — a
                                // Trireme tile launch must flip the capture
                                // source too, not just the panel's own verb.
                                #[cfg(feature = "portal-mirror")]
                                apply_capture_target(&mut pipelines, entry.map(|e| &e.target));
                                if let Some(c) = child {
                                    host_input.track_app_child(id, c, client);
                                }
                            }
                            Err(e) => {
                                data.state.pending_app_launch = None;
                                tracing::warn!("client: launch app '{id}' failed: {e}");
                            }
                        }
                    }
                    core_proto::AppMsg::SwitchTo { id } => {
                        // Sync ALL input focus (keyboard inside switch_to_app, and the
                        // render loop's pointer/touch/gyro target here) onto the raised
                        // window — else the cursor keeps driving the old one.
                        match data.state.switch_to_app(&id) {
                            Some(s) => {
                                focus_surface = Some(s);
                                info!("client: switched to app '{id}'");
                            }
                            None => tracing::info!("client: switch-to '{id}': no live window"),
                        }
                    }
                    core_proto::AppMsg::EndProcess { id } if mirror_target_slug(&id) => {
                        // "End" on the app that's mirroring the real desktop (the
                        // built-in Desktop tile) = stop mirroring: there's no
                        // process to kill, the stream just returns to the virtual
                        // display. The running highlight clears on the next
                        // catalog push (mirror state is part of it).
                        #[cfg(feature = "portal-mirror")]
                        if primary_is_mirroring(&pipelines) {
                            if host_core::config::load().compositor.enable_virtual_displays {
                                info!("client: ended desktop mirror via '{id}'");
                                apply_capture_target(&mut pipelines, None);
                            } else {
                                tracing::info!(
                                    "client: end '{id}' ignored: virtual displays are off, \
                                     so every stream mirrors the real desktop"
                                );
                            }
                        }
                    }
                    core_proto::AppMsg::EndProcess { id } => {
                        // Kill the whole process tree OFF the render thread: end_app
                        // SIGTERMs, sleeps 600ms, then SIGKILLs — blocking it here
                        // would freeze the stream (lesson-render-loop-no-block). Drop
                        // the surface association now; the ~1 Hz running re-poll
                        // re-pushes the corrected catalog once the tree actually dies.
                        data.state.app_surfaces.remove(&id);
                        let slug = id.clone();
                        let _ = std::thread::Builder::new()
                            .name("pharos-end-app".into())
                            .spawn(move || match crate::control::end_app(&slug) {
                                Ok(()) => info!("ended app '{slug}'"),
                                Err(e) => tracing::warn!("end app '{slug}' failed: {e}"),
                            });
                    }
                    core_proto::AppMsg::Catalog { .. } | core_proto::AppMsg::Notice { .. } => {} // host→client only
                }
            }
        }

        // --- Clipboard sync (host ⇄ client) ---
        // All Wayland selection calls run HERE on the render/calloop thread (they
        // just post wayland events / update seat state — non-blocking); only the fd
        // reads/writes are moved to detached threads (lesson-render-loop-no-block).
        if data.state.clipboard_enabled {
            // (a) A host app changed the clipboard: new_selection stashed the offered
            // mimes and the selection is now committed, so read it (off-thread) and
            // relay host→client. Empty mimes = the host CLEARED → push empty text.
            if let Some(mimes) = data.state.pending_clipboard_read.take() {
                if mimes.is_empty() {
                    if let Some(tx) = clipboard_out.as_ref() {
                        let _ = tx.send(core_proto::ClipboardMsg::Text(String::new()));
                    }
                    clip_last_sent = Some(crate::clipboard_hash(""));
                } else if let Some(mime) = CLIPBOARD_TEXT_MIMES
                    .iter()
                    .find(|m| mimes.iter().any(|o| o == **m))
                {
                    use std::os::fd::FromRawFd;
                    let mut fds = [0 as libc::c_int; 2];
                    if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
                        let read_fd = fds[0];
                        let write_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
                        match smithay::wayland::selection::data_device::request_data_device_client_selection::<State>(
                            &data.state.seat,
                            mime.to_string(),
                            write_fd,
                        ) {
                            Ok(()) => {
                                let tx = clip_read_tx.clone();
                                let _ = std::thread::Builder::new()
                                    .name("pharos-clip-read".into())
                                    .spawn(move || {
                                        if let Some(s) = read_selection_text(
                                            read_fd,
                                            Duration::from_secs(5),
                                        ) {
                                            let _ = tx.send(s);
                                        }
                                    });
                            }
                            Err(e) => {
                                // Selection went server-side / mime vanished: close
                                // our read end so it doesn't leak.
                                unsafe { libc::close(read_fd) };
                                tracing::debug!("host clipboard read skipped: {e}");
                            }
                        }
                    }
                }
            }
            // (b) A detached reader posted host clipboard text: relay host→client,
            // unless it's an echo of what we just applied from the client.
            while let Ok(text) = clip_read_rx.try_recv() {
                let h = crate::clipboard_hash(&text);
                if clip_last_recv == Some(h) {
                    continue;
                }
                clip_last_sent = Some(h);
                if let Some(tx) = clipboard_out.as_ref() {
                    let _ = tx.send(core_proto::ClipboardMsg::Text(text));
                }
            }
            // (c) The client copied something: become the Wayland selection owner so
            // host apps can paste it. Clipboard is last-writer-wins, so coalesce a
            // burst (across ALL streamers) to the newest text and touch the Wayland
            // selection at most once per frame — a per-frame drain cap keeps a
            // flooding client from stalling the render loop. Skip an echo of our own
            // host→client push.
            let mut latest: Option<String> = None;
            let mut drained = 0usize;
            'drain: for rx in clipboard_in_rxs.iter_mut() {
                loop {
                    if drained >= CLIPBOARD_MAX_DRAIN_PER_FRAME {
                        break 'drain;
                    }
                    match rx.try_recv() {
                        Ok(core_proto::ClipboardMsg::Text(t)) => {
                            latest = Some(t);
                            drained += 1;
                        }
                        Err(_) => break, // this receiver drained; try the next
                    }
                }
            }
            if let Some(text) = latest {
                let h = crate::clipboard_hash(&text);
                if clip_last_sent != Some(h) {
                    clip_last_recv = Some(h);
                    if text.is_empty() {
                        smithay::wayland::selection::data_device::clear_data_device_selection(
                            &data.display_handle,
                            &data.state.seat,
                        );
                    } else {
                        let mimes: Vec<String> =
                            CLIPBOARD_TEXT_MIMES.iter().map(|s| s.to_string()).collect();
                        smithay::wayland::selection::data_device::set_data_device_selection::<State>(
                            &data.display_handle,
                            &data.state.seat,
                            mimes,
                            std::sync::Arc::new(text.into_bytes()),
                        );
                    }
                }
            }
        } else {
            // Disabled: drain + discard (capped per receiver) so queues can't grow.
            for rx in clipboard_in_rxs.iter_mut() {
                let mut n = 0usize;
                while n < CLIPBOARD_MAX_DRAIN_PER_FRAME && rx.try_recv().is_ok() {
                    n += 1;
                }
            }
        }

        // Re-push the catalog to every client when the background probe reports a
        // change (apps.toml edited, or an app started/exited) — so the client's
        // running highlight + Switch-To/End menu track reality live. The render
        // loop only reads a generation + clones the (small) running set; the /proc
        // walk + apps.toml parse happen on the probe thread.
        if last_apps_check.elapsed() >= Duration::from_secs(1) {
            last_apps_check = std::time::Instant::now();
            let snapshot = {
                let g = running_probe.lock().unwrap_or_else(|e| e.into_inner());
                if g.generation != last_probe_gen {
                    last_probe_gen = g.generation;
                    Some(g.running.clone())
                } else {
                    None
                }
            };
            // Mirroring the real desktop counts as the mirror-target apps
            // (the Desktop tile) "running", so the client offers End on them.
            let mirroring = primary_is_mirroring(&pipelines);
            let snapshot = match snapshot {
                Some(r) => Some(r),
                None if mirroring != last_mirroring => Some(
                    running_probe
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .running
                        .clone(),
                ),
                None => None,
            };
            last_mirroring = mirroring;
            if let Some(mut running) = snapshot {
                if mirroring {
                    running.extend(mirror_target_slugs());
                }
                let catalog = crate::control::build_app_catalog(&running);
                for s in pipelines
                    .iter()
                    .flatten()
                    .filter_map(|p| p.streamer.as_ref())
                {
                    s.send_app_catalog(catalog.clone());
                }
            }
        }
        // Expire the preview window (hard 20s cap on the readback overhead).
        if preview_until.is_some_and(|u| std::time::Instant::now() >= u) {
            preview_until = None;
        }
        // Is this frame due to capture a preview? (Throttled to ~2fps.)
        let preview_due =
            preview_until.is_some() && last_preview.elapsed() >= Duration::from_millis(500);
        let mut preview_capture: Option<(Vec<u8>, u32, u32, usize)> = None;

        // Deliver a finished bench report (client → host) to a waiting panel.
        if let Some(rx) = latency_rx.as_mut() {
            if let Ok(report) = rx.try_recv() {
                if let Some(reply) = bench_reply.take() {
                    let _ = reply.send(Ok(crate::control::wire_report_to_control(&report)));
                    bench_deadline = 0;
                }
            }
        }
        if bench_reply.is_some() && bench_remaining == 0 {
            if bench_deadline == 0 {
                if let Some(reply) = bench_reply.take() {
                    let _ = reply.send(Err("no latency report from client".into()));
                }
            } else {
                bench_deadline -= 1;
            }
        }

        // Detect reconnects (false → true on a pipeline that's already sent
        // frames). We deliberately do NOT rebuild the NVENC session here:
        // re-initialising NVENC inside a long-running process SIGSEGVs the NVIDIA
        // driver (libnvcuvid, the open-module reconnect-reinit crash — see
        // lesson-nvenc-driver-crash + the host-side fix that motivated it). The
        // running encoder keeps going and the reconnecting client resyncs within
        // ≤1s off the existing 1s-IDR + repeated VPS/SPS/PPS cadence
        // (apply_codec_lowlatency) — the same resync path a fresh client uses.
        // We trade ~1s of undecodable P-frames on the newcomer's decoder for
        // never crashing the host mid-session. (`enablePTD` makes a clean
        // per-frame forced-IDR unreachable via the safe encode API; the periodic
        // IDR is the supported low-risk resync.)
        for idx in 0..pipelines.len() {
            let ready_now = pipelines[idx]
                .as_ref()
                .is_some_and(Pipeline::is_streaming_target_ready);
            // Phase 3c (restart-into-panel-res): on the primary output, reshape the
            // encode envelope to the connecting client's panel — we cannot resize a
            // live NVENC session (in-process re-init SIGSEGVs the driver), so record
            // the target and exit for a clean systemd restart that rebuilds at it via
            // the startup override above. Fires on the false→true edge (before
            // last_ready[idx] updates). Two cases, each at most one restart per change:
            //   • orientation flip (landscape↔portrait): match the client EXACTLY, so
            //     a portrait phone gets a portrait host render — a per-dimension max
            //     would fuse the two orientations into a nonsensical envelope;
            //   • same orientation: grow-only — a smaller client rides the downscale
            //     ladder, a same-size client never trips it.
            // A v2 client (no StreamMode w/h) reports 0×0 and is ignored.
            if idx == 0 && pipelines.len() == 1 && ready_now && !last_ready[idx] {
                if let Some((pw, ph)) = pipelines[idx].as_ref().and_then(|p| {
                    p.streamer.as_ref().map(|s| {
                        let (w, h) = s.client_panel_res();
                        (w & !1, h & !1) // NVENC requires even dimensions
                    })
                }) {
                    if let Some((ew, eh)) = pipelines[idx].as_ref().map(|p| (p.width, p.height)) {
                        if pw > 0 && ph > 0 {
                            let reorient = (pw >= ph) != (ew >= eh);
                            let (tw, th, change) = if reorient {
                                (pw, ph, pw != ew || ph != eh)
                            } else if pw > ew || ph > eh {
                                (ew.max(pw), eh.max(ph), true)
                            } else {
                                (ew, eh, false)
                            };
                            if change {
                                // ONLY restart if the target was genuinely persisted —
                                // else the fresh process rebuilds at config, the same
                                // client re-triggers this, and we loop until systemd's
                                // StartLimit breaker kills the host. On a persist failure
                                // (e.g. $XDG_RUNTIME_DIR unset / disk full), fall through:
                                // the client streams at the current envelope via the
                                // downscale ladder (degraded, not native, but live).
                                if control_ipc::write_stream_res(tw, th) {
                                    info!(
                                        "output[0] client panel {pw}x{ph} vs envelope {ew}x{eh} \
                                         ({}); recorded {tw}x{th} + restarting to rebuild at it",
                                        if reorient { "reorient" } else { "grow" }
                                    );
                                    // fast_restart_exit (not process::exit) so CUDA atexit
                                    // teardown can't hang the restart ~10s; systemd
                                    // Restart=on-failure respawns and reads the file.
                                    fast_restart_exit(3);
                                }
                                tracing::warn!(
                                    "output[0] client panel {pw}x{ph} wants envelope {tw}x{th} but \
                                     could not persist it; streaming at {ew}x{eh} (no restart) to \
                                     avoid a restart loop"
                                );
                            }
                        }
                    }
                }
            }
            if ready_now && !last_ready[idx] && first_sent[idx] {
                info!(
                    "output[{idx}] reconnect detected; resyncing via next IDR (no NVENC rebuild)"
                );
                // Drop the previous client's QoS-derived pacing ceiling — otherwise a
                // new client on a healthy link inherits a stale low net_allowed_fps
                // from a prior WAN client and is capped until the slow ramp recovers.
                net_allowed_fps = fps_cap;
                client_recv_fps_ewma = 0.0;
                net_low_ticks = 0;
                net_clear_ticks = 0;
                qos_stale_ticks = 0;
                // Restore full bitrate for the new client: a prior WAN client may have
                // left the encoder reconfigured down. Reset + reconfigure to the cap
                // immediately (don't make the newcomer wait out the slow up-ramp).
                if let Some((avg_cap, max_cap)) = rc_caps {
                    net_lowwater_kbps = avg_cap;
                    if last_reconfig_kbps != avg_cap {
                        net_allowed_kbps = avg_cap;
                        last_reconfig_kbps = avg_cap;
                        reconfigure_all_bitrate(
                            &pipelines, avg_cap, avg_cap, max_cap, cfg.fps, true,
                        );
                    }
                }
                // Restore the resolution ladder too: a prior degraded client may
                // have driven this pipeline's coded size down a rung. Reset the
                // ladder hysteresis and snap back to the selected ceiling (native
                // when Auto) so the newcomer doesn't inherit a stale-low resolution
                // and wait out the slow per-rung up-ramp. No-op (and zero-cost) in
                // the common case where coded is already at the ceiling.
                res_low_ticks = 0;
                res_clear_ticks = 0;
                if let Some(p) = pipelines[idx].as_mut() {
                    if p.streamer.is_some() {
                        let (cw, ch) = coded_for_height(p.width, p.height, p.res_ceiling_h);
                        apply_coded_resolution(p, cw, ch);
                    }
                }
            }
            last_ready[idx] = ready_now;
        }

        if i >= 5 && key_idx < inject_keys.len() {
            let code = inject_keys[key_idx];
            let time = data.state.start_time.elapsed().as_millis() as u32;
            let keyboard = data.state.seat.get_keyboard().unwrap();
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<(), _>(
                &mut data.state,
                Keycode::new(code + 8),
                KeyState::Pressed,
                serial,
                time,
                |_, _, _| FilterResult::Forward,
            );
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<(), _>(
                &mut data.state,
                Keycode::new(code + 8),
                KeyState::Released,
                serial,
                time,
                |_, _, _| FilterResult::Forward,
            );
            key_idx += 1;
        }

        // Host input backstop (issue #13): if the connected client CHANGED
        // (disconnect / preemption), the departed client's key-ups, button-ups,
        // and stick-returns can never arrive; a held key/button/pad would stick in
        // the live host session and interfere with the physical console user. So
        // ~4 Hz we compare the input streamer's `connected_addr()` and, on a
        // change, PURGE its still-queued events (no stale replay) and RELEASE all
        // held state. Same purge+release also trips below on a large backlog burst.
        // This is the Linux parity to the Windows injector's client-change path.
        if last_input_conn_check.elapsed() >= std::time::Duration::from_millis(250) {
            last_input_conn_check = std::time::Instant::now();
            let conn = input_streamer_idx
                .and_then(|idx| pipelines[idx].as_ref())
                .and_then(|p| p.streamer.as_ref())
                .and_then(|s| s.connected_addr());
            if conn != last_input_conn {
                last_input_conn = conn;
                let mut purged = 0u64;
                if let Some(rx) = input_rx.as_mut() {
                    while rx.try_recv().is_ok() {
                        purged += 1;
                    }
                }
                release_all_held(&mut data, &mut host_input, &mut held_ptr_buttons);
                if purged > 0 {
                    tracing::info!(
                        "input: purged {purged} queued events on client change (issue #13)"
                    );
                }
            }
        }

        // Drain network input → keyboard/mouse to the seat. Focus the newest
        // toplevel first, so an app launched after startup (e.g. the media
        // player) receives keyboard input.
        // Desktop-mirror (issue #6): input is single-focus (one shared
        // `input_rx`, matching the single-client-serial host — see
        // `lesson-host-single-client-serial`), so it's enough to ask the
        // PRIMARY pipeline whether it's currently mirroring the real
        // desktop. When it is, keyboard/pointer events skip the smithay
        // seat entirely and go to libei instead (gamepad still rides the
        // uinput pad either way — see `route_to_mirror_input`).
        #[cfg(feature = "portal-mirror")]
        let mirror_input_active = pipelines
            .first()
            .and_then(|p| p.as_ref())
            .and_then(Pipeline::mirror_input);
        if let Some(rx) = input_rx.as_mut() {
            let win_count = data.state.space.elements().count();
            if win_count != last_win_count {
                last_win_count = win_count;
                let surface = data
                    .state
                    .space
                    .elements()
                    .last()
                    .and_then(|w| w.toplevel())
                    .map(|t| t.wl_surface().clone());
                if let Some(s) = surface {
                    let keyboard = data.state.seat.get_keyboard().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(&mut data.state, Some(s.clone()), serial);
                    focus_surface = Some(s);
                }
            }
            // Drain in a bounded burst so a stall-replay dump (a departed client's
            // queue arriving en masse) is caught: a human tops out at a few events
            // per frame, so a pass past `INPUT_BACKLOG_BURST` is a queued dump.
            // On that we stop injecting, purge the remainder, and release held
            // state (issue #13), mirroring the Windows backlog backstop.
            let mut this_pass = 0usize;
            let mut backlog = false;
            while let Ok(ev) = rx.try_recv() {
                // Track the pointer buttons WE inject so the backstop can release
                // exactly those (smithay's PointerHandle exposes no held list).
                if let core_proto::InputEvent::PointerButton { button, pressed } = &ev {
                    if *pressed {
                        held_ptr_buttons.insert(*button);
                    } else {
                        held_ptr_buttons.remove(button);
                    }
                }
                // Gamepad/gyro/controller-tuning variants are owned by
                // `HostInput`; every other variant falls through to the
                // seat-dispatch closure (the keyboard/pointer path).
                host_input.dispatch_input_event(
                    &mut data,
                    input_w,
                    input_h,
                    focus_surface.as_ref(),
                    ev,
                    |data, other| {
                        #[cfg(feature = "portal-mirror")]
                        if let Some(mi) = mirror_input_active {
                            route_to_mirror_input(mi, other);
                            return;
                        }
                        crate::dispatch_input(
                            data,
                            input_w,
                            input_h,
                            focus_surface.as_ref(),
                            other,
                        );
                    },
                );
                this_pass += 1;
                if this_pass >= INPUT_BACKLOG_BURST {
                    backlog = true;
                    break;
                }
            }
            if backlog {
                let mut purged = 0u64;
                while rx.try_recv().is_ok() {
                    purged += 1;
                }
                release_all_held(&mut data, &mut host_input, &mut held_ptr_buttons);
                tracing::warn!(
                    "input: backlog burst ({this_pass} injected, {purged} purged); \
                     released all held state (issue #13)"
                );
            }
        }

        // Per-frame: detect launched-app exits. When the last tracked child
        // dies, `reap_exited_apps` resets `active_app_gyro_mode` to default,
        // so an app that overrode gyro→mouse no longer affects the next
        // foreground window. Cheap (a couple of `waitpid(WNOHANG)` calls).
        host_input.reap_exited_apps();
        framing.input += stage.mark();

        // Master gyro gate: a client opts in at the v2 auth handshake via
        // `wants_gyro`. If NO connected streamer's client opted in, skip the
        // per-frame drain AND skip raw-sample relay; also flip the IMU
        // reader's gate off so its thread idles in 500ms sleeps (no evdev
        // open, no integration). Cheap to recompute each tick (a handful of
        // atomic loads).
        let any_wants_gyro = pipelines
            .iter()
            .flatten()
            .filter_map(|p| p.streamer.as_ref())
            .any(|s| s.wants_gyro());
        host_input.set_gyro_gate(any_wants_gyro);

        if any_wants_gyro {
            // Gyro from a USB/IP-imported controller's IMU → mouse aim, drained
            // each frame (idle when no IMU device is present). Aimed at the
            // primary output's dimensions, same as the gyro→mouse motion that
            // comes in from the network input drain above.
            host_input.drain_gyro_per_frame(&mut data, input_w, input_h, focus_surface.as_ref());

            // Stream raw IMU samples to the FIRST connected client's calibration
            // visualizer (only on a fresh sample, so we don't spam stale/zero
            // packets when idle). Picking the first streamer matches single-output
            // semantics — most setups only have one client wired to the cal UI.
            if let Some(s) = pipelines
                .iter()
                .flatten()
                .filter_map(|p| p.streamer.as_ref())
                .next()
            {
                host_input.stream_raw_gyro(|pkt| s.send_gyro(pkt));
            }
        }

        for msg in flydigi.drain() {
            host_input.flydigi_motion(msg);
        }
        // The Flydigi's gyro also feeds the first client's calibration screen.
        if let Some(s) = pipelines
            .iter()
            .flatten()
            .filter_map(|p| p.streamer.as_ref())
            .next()
        {
            host_input.stream_flydigi_gyro(|pkt| s.send_gyro(pkt));
        }

        // USB/IP-imported keyboards and mice → the focused surface. Keys go
        // straight to the seat (the host adds the xkb +8 to the evdev code);
        // mouse events take the same seat dispatch as a client's mouse, so
        // focus, pointer locks, and the overlay cursor behave identically.
        // Independent of the network back-channel.
        for ev in usbkbd.drain() {
            let (code, pressed) = match ev {
                crate::usbkbd::UsbEvent::Key(code, pressed) => (code, pressed),
                crate::usbkbd::UsbEvent::Pointer(pev) => {
                    crate::dispatch_input(&mut data, input_w, input_h, focus_surface.as_ref(), pev);
                    continue;
                }
            };
            if let Some(keyboard) = data.state.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                let time = data.state.start_time.elapsed().as_millis() as u32;
                let state = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                keyboard.input::<(), _>(
                    &mut data.state,
                    Keycode::new(code as u32 + 8),
                    state,
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
        }
        framing.gyro += stage.mark();

        // Render → bridge → encode → send for each ready output. Skipping a
        // not-yet-ready output keeps its session at picture 0, so the first
        // sent frame is its session's opening IDR.
        //
        // Per-stage µs split (used by the framing histogram AND the
        // encode-watchdog): the inner `gl_blit` covers smithay render +
        // GL→CUDA bridge; `encode` covers `encode_picture` + bitstream
        // `lock` (the NVENC critical path that wedges on libnvcuvid stalls);
        // `wire_enqueue` covers the streamer.send() try_send (µs unless the
        // mpsc is full, in which case the drop-newest will show).
        let mut frame_gl_blit_us: u64 = 0;
        let mut frame_encode_us: u64 = 0;
        let mut frame_wire_us: u64 = 0;
        for (idx, slot) in pipelines.iter_mut().enumerate() {
            let Some(p) = slot.as_mut() else { continue };
            // Desktop-mirror (issue #6): install a finished portal handshake (or
            // revert a failed one) before rendering. Off-thread activation means
            // this may flip `MirrorPending` → `PhysicalMirror`/`Virtual` on any
            // tick. Runs even for a not-yet-streaming pipeline so a mirror armed
            // at startup (enable_virtual_displays=false) settles before a client
            // connects.
            #[cfg(feature = "portal-mirror")]
            poll_mirror_pending(p);
            if !p.is_streaming_target_ready() {
                continue;
            }
            // Latency stamp: only the chosen bench output, only while a bench
            // window is open. capture time = render start.
            let bench_this = bench_remaining > 0 && bench_output == Some(idx);
            let capture_ns = if bench_this { unix_nanos_now() } else { 0 };
            let gl_blit_t0 = if framing.enabled {
                Some(std::time::Instant::now())
            } else {
                None
            };
            // No app window mapped → paint the Pharos idle screen instead of a
            // flat dark fill, so a streamed black screen unambiguously means "the
            // app went black", not "nothing is running". Built once + cached; this
            // whole branch is skipped while any app is up, so it's zero-cost
            // in-game. The element is built BEFORE bind (it needs `&mut renderer`;
            // the framebuffer doesn't borrow the renderer).
            let elements =
                space_render_elements(&mut renderer, [&data.state.space], &p.output, 1.0)
                    .map_err(|_| anyhow::anyhow!("output[{}] no mode", p.name))?;
            // PER-OUTPUT idle decision: an empty element list means no window
            // overlaps THIS output's region. (`space_render_elements` already
            // filters by the output's geometry, and this headless compositor maps
            // only fullscreen Windows — no layer-shell — so empty ⇒ no app here.)
            // A global "any window in the space" check would wrongly leave an empty
            // secondary output in an EXTEND layout on the flat fill.
            let no_app = elements.is_empty();
            // Desktop-mirror (issue #6): this pipeline streams the REAL host
            // desktop instead of the app-window composite above — takes
            // precedence over both `elements` (nothing is ever mapped onto a
            // mirror output) and the idle screen (a mirrored desktop is
            // never "no app running"; the desktop itself IS the content).
            // Drop-newest: keep only the freshest frame PipeWire delivered
            // since our last tick, matching the channel sender's own policy.
            #[cfg(feature = "portal-mirror")]
            let mirror_el = match &mut p.source {
                CaptureSource::PhysicalMirror(m) => {
                    while let Ok(f) = m.frame_rx.try_recv() {
                        m.latest = Some(f);
                    }
                    // Best-fit into THIS output's logical canvas so a wide host
                    // desktop letterboxes instead of cropping (issue #6).
                    let dst = smithay::utils::Size::from((p.width as i32, p.height as i32));
                    m.latest.as_ref().and_then(|f| {
                        m.importer
                            .element(&mut renderer, f, dst)
                            .map_err(|e| tracing::debug!("output[{}] mirror import: {e:#}", p.name))
                            .ok()
                    })
                }
                // Virtual, or a portal handshake still in flight: render the
                // ordinary app/idle composite this tick (the mirror installs
                // via `poll_mirror_pending` once the user answers the dialog).
                CaptureSource::Virtual | CaptureSource::MirrorPending(_) => None,
            };
            #[cfg(not(feature = "portal-mirror"))]
            let mirror_el: Option<
                smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<
                    GlesRenderer,
                >,
            > = None;
            let idle_el = if mirror_el.is_none() && no_app {
                // Lazy first-idle bake (decode logo + compose), then cached — one
                // brief hitch at app→idle instead of delaying host startup. Never
                // runs while an app is mapped, so it's still zero-cost in-game.
                if p.idle.is_none() {
                    p.idle = crate::idle::IdleScreen::build(p.width, p.height)
                        .map_err(|e| tracing::warn!("output[{}] idle screen: {e:#}", p.name))
                        .ok();
                }
                let alpha = crate::idle::pulse(loop_start.elapsed().as_secs_f32());
                p.idle.as_ref().and_then(|s| {
                    s.element(&mut renderer, alpha)
                        .map_err(|e| tracing::debug!("idle element: {e:#}"))
                        .ok()
                })
            } else {
                None
            };
            // Overlay host cursor: over a windowed dialog / launcher, or over a
            // fullscreen app that shows a pointer while the client is moving it
            // (`host_cursor_wanted`: never under a pointer lock or a hidden
            // cursor, so it stays off in-game), and only when painting the app composite — never over a desktop mirror
            // (that stream already carries the real host cursor) or the idle
            // screen. Gives a streamed TV mouse / controller-cursor a visible
            // pointer to aim with. Rects are physical (session scale 1.0).
            let cursor_overlay =
                if mirror_el.is_none() && idle_el.is_none() && data.state.host_cursor_wanted() {
                    data.state
                        .pointer_location()
                        .map(|loc| cursor_overlay_rects(loc, cursor_scale(p.height)))
                } else {
                    None
                };
            // Wrap the space elements, with the cursor rectangles stacked ON TOP,
            // into ONE list. render_output composites the whole list in a single
            // damage-tracked pass, so the cursor shares the app frame's GPU pass
            // and fence — the encoder never grabs a frame with a half-drawn
            // overlay (a separate post-render draw pass flickered). Element order
            // is topmost-first: cursor FILL above OUTLINE above the app windows,
            // so the black border shows only where the white fill doesn't cover.
            let app_elements: Vec<OverlayRenderElements> = {
                let mut v: Vec<OverlayRenderElements> = Vec::with_capacity(elements.len() + 48);
                if let Some((outline, fill)) = &cursor_overlay {
                    for r in fill {
                        v.push(OverlayRenderElements::Cursor(cursor_solid(
                            *r,
                            [1.0, 1.0, 1.0, 1.0],
                        )));
                    }
                    for r in outline {
                        v.push(OverlayRenderElements::Cursor(cursor_solid(
                            *r,
                            [0.0, 0.0, 0.0, 1.0],
                        )));
                    }
                }
                v.extend(elements.into_iter().map(OverlayRenderElements::Space));
                v
            };
            // Stage C: does the mapped app actually declare HDR (via
            // wp_color_manager_v1, stored per-surface by crate::color_mgmt)? Only
            // then run the scRGB→PQ conversion — the idle screen / an SDR desktop
            // must stay on the direct path, not be mis-treated as scRGB. Gated on
            // the session being 10-bit (`hdr`), since the NVENC format is fixed at
            // session create; the session-level auto-switch composes with the
            // client `wants_hdr` negotiation (a 10-bit session an SDR-only client
            // can't decode must not engage without it).
            let surface_hdr = hdr
                && !no_app
                && data.state.space.elements().any(|w| {
                    w.toplevel()
                        .map(|t| {
                            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                                states
                                    .cached_state
                                    .get::<crate::color_mgmt::ColorSurfaceCachedState>()
                                    .current()
                                    .image_desc
                                    .map(|d| d.is_hdr())
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                });
            // Diagnostic: in a 10-bit session, log the frontmost surface's buffer
            // format on change so an on-device HDR run can confirm fp16 (true HDR)
            // vs an 8-bit SDR fallback. Cheap; debug-gated (compositor::color).
            if hdr && !no_app {
                log_top_surface_buffer_format(&data.state.space);
            }
            {
                if hdr {
                    // 10-bit session: composite into an fp16 scratch (so scRGB
                    // values >1.0 survive; the default shader is a value-passthrough
                    // that would clamp into the 10-bit UNORM target), then a
                    // colour-space post-pass writes the 10-bit target the
                    // ladder/cuegl consume. The shader depends on what the surface
                    // declared: scRGB→PQ for a real HDR surface (surface_hdr),
                    // sRGB→PQ (SDR white → 203 nits) otherwise — so an SDR game / the
                    // idle screen shows correctly on the HDR display instead of
                    // being read as raw PQ (dark mid-tones, blown-out whites).
                    if p.hdr_scratch.is_none() {
                        p.hdr_scratch = Some(renderer.create_buffer(
                            Fourcc::Abgr16161616f,
                            (p.width as i32, p.height as i32).into(),
                        )?);
                    }
                    {
                        let scratch = p.hdr_scratch.as_mut().unwrap();
                        let mut sfb = renderer.bind(scratch)?;
                        if let Some(el) = &mirror_el {
                            // Real desktop content — SDR, so it takes the
                            // sRGB→PQ post-pass below (`surface_hdr` is
                            // false whenever `no_app` skipped the HDR-surface
                            // scan, which mirroring always does).
                            p.damage.render_output(
                                &mut renderer,
                                &mut sfb,
                                0,
                                std::slice::from_ref(el),
                                crate::idle::BG,
                            )?;
                        } else if let Some(el) = &idle_el {
                            p.damage.render_output(
                                &mut renderer,
                                &mut sfb,
                                0,
                                std::slice::from_ref(el),
                                crate::idle::BG,
                            )?;
                        } else {
                            p.damage.render_output(
                                &mut renderer,
                                &mut sfb,
                                0,
                                &app_elements,
                                [0.05, 0.05, 0.08, 1.0],
                            )?;
                        }
                    }
                    // Colour-space post-pass: fp16 scratch → 10-bit target. Pick
                    // scRGB→PQ for a declared-HDR surface, sRGB→PQ for SDR content.
                    let post_prog = if surface_hdr {
                        pq_program.as_ref()
                    } else {
                        srgb_pq_program.as_ref()
                    };
                    let scratch = p.hdr_scratch.as_ref().unwrap();
                    let src = Rectangle::<f64, BufferCoord>::new(
                        (0.0, 0.0).into(),
                        (p.width as f64, p.height as f64).into(),
                    );
                    let dst = Rectangle::<i32, Physical>::new(
                        (0, 0).into(),
                        (p.width as i32, p.height as i32).into(),
                    );
                    let full = [dst];
                    let mut fb = renderer.bind(&mut p.target)?;
                    let mut frame = renderer.render(
                        &mut fb,
                        (p.width as i32, p.height as i32).into(),
                        Transform::Normal,
                    )?;
                    frame.render_texture_from_to(
                        scratch,
                        src,
                        dst,
                        &full,
                        &full,
                        Transform::Normal,
                        1.0,
                        post_prog,
                        &[],
                    )?;
                    let _ = frame.finish()?;
                } else {
                    let mut fb = renderer.bind(&mut p.target)?;
                    if let Some(el) = &mirror_el {
                        p.damage.render_output(
                            &mut renderer,
                            &mut fb,
                            0,
                            std::slice::from_ref(el),
                            crate::idle::BG,
                        )?;
                    } else if let Some(el) = &idle_el {
                        p.damage.render_output(
                            &mut renderer,
                            &mut fb,
                            0,
                            std::slice::from_ref(el),
                            crate::idle::BG,
                        )?;
                    } else {
                        p.damage.render_output(
                            &mut renderer,
                            &mut fb,
                            0,
                            &app_elements,
                            [0.05, 0.05, 0.08, 1.0],
                        )?;
                    }
                    // Live-preview readback: only the first ready output, only when
                    // the throttle is due, only while the 20s window is open. SDR
                    // only — a 10-bit fb read as Abgr8888 would be wrong (this
                    // branch also runs for a 10-bit session with no HDR surface).
                    if preview_due && preview_capture.is_none() && !hdr {
                        let region = Rectangle::new(
                            (0, 0).into(),
                            Size::from((p.width as i32, p.height as i32)),
                        );
                        if let Ok(mapping) =
                            renderer.copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                        {
                            if let Ok(bytes) = renderer.map_texture(&mapping) {
                                let stride = bytes.len() / p.height.max(1) as usize;
                                preview_capture = Some((bytes.to_vec(), p.width, p.height, stride));
                            }
                        }
                    }
                }
            }

            // Resolution ladder: when the coded size is below native, downscale
            // the freshly-rendered native frame into a cached offscreen texture
            // (bilinear `glBlitFramebuffer`) and feed THAT to NVENC; otherwise
            // feed the native target directly. coded == native is the zero-cost
            // default — no blit, byte-identical to the pre-ladder path. The CUDA
            // copy always uses the native pitch (the registered NVENC resource is
            // registered at native), writing the coded-sized top-left region.
            let (src_tex_id, copy_w, copy_h) = if (p.coded_w, p.coded_h) != (p.width, p.height) {
                let (cw, ch) = (p.coded_w, p.coded_h);
                if p.scale.as_ref().map(|(_, w, h)| (*w, *h)) != Some((cw, ch)) {
                    // Match the native target's bit depth or a downscaled HDR
                    // frame silently truncates to 8-bit.
                    let tex =
                        renderer.create_buffer(target_fourcc, (cw as i32, ch as i32).into())?;
                    p.scale = Some((tex, cw, ch));
                }
                let src_rect = Rectangle::<i32, Physical>::new(
                    (0, 0).into(),
                    (p.width as i32, p.height as i32).into(),
                );
                let dst_rect =
                    Rectangle::<i32, Physical>::new((0, 0).into(), (cw as i32, ch as i32).into());
                {
                    let src_fb = renderer.bind(&mut p.target)?;
                    let mut dst_fb = renderer.bind(&mut p.scale.as_mut().unwrap().0)?;
                    renderer.blit(
                        &src_fb,
                        &mut dst_fb,
                        src_rect,
                        dst_rect,
                        TextureFilter::Linear,
                    )?;
                }
                (p.scale.as_ref().unwrap().0.tex_id(), cw, ch)
            } else {
                // Back at native — release the offscreen so we don't pin VRAM.
                p.scale = None;
                (p.target.tex_id(), p.width, p.height)
            };
            let egl = renderer.egl_context();
            unsafe { egl.make_current() }.map_err(|e| anyhow::anyhow!("make_current: {e}"))?;
            let raw_dpy = egl.display().get_display_handle().handle as *mut c_void;
            let raw_ctx = egl.get_context_handle() as *mut c_void;
            unsafe {
                bridge.copy_gl_texture_to_linear(
                    raw_dpy,
                    raw_ctx,
                    src_tex_id,
                    p.cuda_buf.0,
                    p.pitch as usize,
                    copy_w,
                    copy_h,
                )?;
            }
            if let Some(t0) = gl_blit_t0 {
                frame_gl_blit_us += t0.elapsed().as_micros() as u64;
            }

            // On-demand IDR: if this output's streamer is asking for a keyframe
            // (it dropped an AU, or the client sent DG_REQUEST_IDR) and we haven't
            // just emitted one, force THIS frame to be an IDR so the client resyncs
            // in ~1 frame instead of waiting for the next periodic IDR (see
            // `idr_period_frames`). Rate-limited via last_idr_at so sustained
            // loss can't trigger an IDR storm. Only NVENC can honour it mid-GOP;
            // the software (VVC) backend resyncs on its periodic IDR instead.
            let force_idr = p.streamer.as_ref().is_some_and(|s| {
                s.take_idr_request()
                    && last_idr_at[idx].is_none_or(|t| t.elapsed() >= min_idr_interval)
            });
            // Latency stamp: encode start = just before the encode hand-off.
            let encode_start_ns = if bench_this { unix_nanos_now() } else { 0 };
            // Unconditional wall-clock timing of the encode critical path: on
            // NVENC this is the documented libnvcuvid wedge fingerprint, so we
            // always measure it (one Instant::now() pair per output per frame;
            // ~200ns cost). The framing histogram sees µs; the watchdog sees ms.
            let encode_t0 = std::time::Instant::now();

            match &mut p.backend {
                EncodeBackend::Soft(enc) => {
                    // Software (VVC) backend: offer the freshly-blitted frame (the
                    // device→host copy happens only when the encoder is ready for one)
                    // and forward whatever AUs it finished since last tick. Never
                    // blocks the render loop. Bench-window stage stamps aren't
                    // meaningful here (AUs are asynchronous to render frames), so a
                    // bench against a software session times out on the client side.
                    if force_idr {
                        tracing::debug!(
                            target: "compositor::swenc",
                            "IDR request on '{}' waits for the periodic IDR (no live control channel)",
                            p.name
                        );
                    }
                    // SAFETY: cuda_buf is this output's live linear allocation of
                    // pitch*height bytes on the context bound to this render thread.
                    unsafe { enc.submit_from_cuda(p.cuda_buf.0) };
                    frame_encode_us += encode_t0.elapsed().as_micros() as u64;
                    for au in enc.drain_aus() {
                        if au.keyframe {
                            last_idr_at[idx] = Some(std::time::Instant::now());
                        }
                        let au_bytes = au.data.len() as u64;
                        let wire_t0 = if framing.enabled {
                            Some(std::time::Instant::now())
                        } else {
                            None
                        };
                        if let Some(s) = &p.streamer {
                            s.send_frame(au.data, au.keyframe);
                            wire_frames_in_window = wire_frames_in_window.saturating_add(1);
                            wire_bytes_in_window = wire_bytes_in_window.saturating_add(au_bytes);
                        } else if let Some(f) = &mut p.file {
                            f.write_all(&au.data)?;
                        }
                        if let Some(t0) = wire_t0 {
                            frame_wire_us += t0.elapsed().as_micros() as u64;
                        }
                        if !first_sent[idx] {
                            first_sent[idx] = true;
                            info!(
                                "output[{idx}] '{}' first frame sent → {}",
                                p.name, p.addr_label
                            );
                        }
                    }
                }
                EncodeBackend::Nvenc {
                    bitstreams,
                    reg,
                    _session,
                    num_bufs,
                } => {
                    let ob_idx = (i as usize) % *num_bufs;

                    // Software fault injection (P1 harness, mode B): exercise classify()
                    // on a healthy GPU with no faulty card. When a fault is due this
                    // frame we branch into the SAME handler a real NVENC error hits,
                    // WITHOUT calling encode_picture — so the bitstream buffer state is
                    // untouched, mirroring a real EncoderBusy (which produces no output).
                    // Unset PHAROS_FAULT_INJECT → `fault_inject` is None → no-op.
                    if let Some(kind) = fault_inject.as_ref().and_then(|f| f.due(i)) {
                        match classify(kind) {
                            Severity::Transient => {
                                if kind == ErrorKind::EncoderBusy {
                                    nvenc_busy[idx] = nvenc_busy[idx].saturating_add(1);
                                    slow_events.push_back(i); // feeds the windowed Degraded trigger
                                }
                                tracing::warn!(
                                    target: "compositor::nvenc",
                                    "INJECTED transient output={} kind={:?} frame={} busy_total={} — skipping emit (host must survive)",
                                    p.name, kind, i, nvenc_busy[idx]
                                );
                                continue;
                            }
                            Severity::Degraded | Severity::Fatal => {
                                tracing::error!(
                                    target: "compositor::nvenc",
                                    "INJECTED fatal output={} kind={:?} frame={}", p.name, kind, i
                                );
                                return Err(fatal_err(&p.name, "encode(injected)", kind));
                            }
                        }
                    }

                    let ob = &mut bitstreams[ob_idx];
                    // Severity-aware encode + lock. Pre-P1 a bare `?` here meant a single
                    // transient (EncoderBusy/NeedMoreInput/LockBusy) terminated the whole
                    // host; now a retryable kind skips just this output's frame (the
                    // client resyncs off the next periodic IDR) and only a fatal kind
                    // returns Err → the process exits + systemd respawns a FRESH NVENC
                    // process (never an in-process re-init, which SIGSEGVs libnvcuvid).
                    // HDR10 static metadata: re-emit MDCV(137)+CLL(144) on every IDR
                    // (idrPeriod == idr_period_frames, plus any on-demand force_idr) so
                    // a mid-stream joiner gets the mastering/CLL info alongside
                    // repeatSPSPPS at the next periodic IDR.
                    // The payload array must outlive the encode call; NVENC copies it.
                    let mut hdr_sei_arr;
                    let codec_params = match hdr_sei.as_ref() {
                        Some((mdcv, cll))
                            if force_idr || i.is_multiple_of(idr_period_frames(cfg.fps)) =>
                        {
                            hdr_sei_arr = [
                                NV_ENC_SEI_PAYLOAD {
                                    payloadSize: mdcv.len() as u32,
                                    payloadType: 137,
                                    payload: mdcv.as_ptr() as *mut u8,
                                },
                                NV_ENC_SEI_PAYLOAD {
                                    payloadSize: cll.len() as u32,
                                    payloadType: 144,
                                    payload: cll.as_ptr() as *mut u8,
                                },
                            ];
                            // SAFETY: NV_ENC_PIC_PARAMS_HEVC is POD; zeroed is a valid
                            // "no extra params" base and NVENC copies the SEI in-call.
                            let mut pic: NV_ENC_PIC_PARAMS_HEVC = unsafe { std::mem::zeroed() };
                            pic.seiPayloadArrayCnt = hdr_sei_arr.len() as u32;
                            pic.seiPayloadArray = hdr_sei_arr.as_mut_ptr();
                            Some(CodecPictureParams::Hevc(pic))
                        }
                        _ => None,
                    };
                    if let Err(e) = _session.encode_picture(
                        reg,
                        ob,
                        EncodePictureParams {
                            force_idr,
                            codec_params,
                            ..Default::default()
                        },
                    ) {
                        match classify(e.kind()) {
                            Severity::Transient => {
                                if e.kind() == ErrorKind::EncoderBusy {
                                    nvenc_busy[idx] = nvenc_busy[idx].saturating_add(1);
                                    slow_events.push_back(i); // feeds the windowed Degraded trigger
                                }
                                tracing::warn!(
                                    target: "compositor::nvenc",
                                    "nvenc encode transient output={} kind={:?} busy_total={} — skipping frame",
                                    p.name, e.kind(), nvenc_busy[idx]
                                );
                                continue;
                            }
                            Severity::Degraded | Severity::Fatal => {
                                return Err(fatal_err(&p.name, "encode", e.kind()));
                            }
                        }
                    }
                    let lock = match ob.lock() {
                        Ok(l) => l,
                        Err(e) => match classify(e.kind()) {
                            Severity::Transient => {
                                tracing::warn!(
                                    target: "compositor::nvenc",
                                    "nvenc lock transient output={} kind={:?} — skipping frame",
                                    p.name, e.kind()
                                );
                                continue;
                            }
                            Severity::Degraded | Severity::Fatal => {
                                return Err(fatal_err(&p.name, "lock", e.kind()));
                            }
                        },
                    };
                    let encode_elapsed = encode_t0.elapsed();
                    let encode_ms = encode_elapsed.as_millis() as u64;
                    frame_encode_us += encode_elapsed.as_micros() as u64;
                    // Watchdog: a healthy 1080p60 HEVC encode on a 4070 finishes in
                    // ~3-6ms; >50ms is a real stall worth a warn; 3 consecutive >200ms
                    // is the libnvcuvid-wedge fingerprint (memory `lesson-nvenc…`).
                    if encode_ms > 50 {
                        tracing::warn!(
                            target: "compositor::nvenc",
                            "nvenc encode_picture slow output={} ms={}",
                            p.name, encode_ms
                        );
                    }
                    if encode_ms > 200 {
                        // Per-output consecutive-slow run = the libnvcuvid wedge
                        // fingerprint → Faulted (sticky). Also a windowed slow event that
                        // feeds the Degraded rate trigger evaluated after the output loop.
                        nvenc_consecutive_slow[idx] = nvenc_consecutive_slow[idx].saturating_add(1);
                        slow_events.push_back(i);
                        if nvenc_consecutive_slow[idx] >= 3 {
                            tracing::error!(
                                target: "compositor::nvenc",
                                "NVENC LIKELY WEDGED (output={} consecutive_slow={} last_ms={})",
                                p.name, nvenc_consecutive_slow[idx], encode_ms
                            );
                            set_gpu_health(
                                HEALTH_FAULTED,
                                &format!(
                                    "NVENC wedge: 3×>200ms on '{}' (last {encode_ms}ms)",
                                    p.name
                                ),
                            );
                            // Faulted is sticky in the shared GPU_HEALTH (cleared only by a
                            // fresh process, P5), so reset the per-output run here to keep
                            // the log cadence sane while the wedge persists.
                            nvenc_consecutive_slow[idx] = 0;
                        }
                    } else {
                        nvenc_consecutive_slow[idx] = 0;
                    }
                    // Latency stamp: encode done = after the NVENC bitstream lock.
                    let encode_done_ns = if bench_this { unix_nanos_now() } else { 0 };
                    let au_bytes = lock.data().len() as u64;
                    let wire_t0 = if framing.enabled {
                        Some(std::time::Instant::now())
                    } else {
                        None
                    };
                    if let Some(s) = &p.streamer {
                        // Keyframe (IDR/I) AUs must never be dropped by the bounded send
                        // queue in favour of a P-frame backlog — losing one strands the
                        // decoder for a whole GOP (~1s of propagating "pink block"
                        // corruption), whereas a dropped P-frame is resynced by the next
                        // IDR. NVENC reports the picture type on the lock (PTD is enabled),
                        // so flag it for the queue's keyframe reserve.
                        let keyframe = matches!(
                            lock.picture_type(),
                            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR
                                | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
                        );
                        // Track the last keyframe (periodic or on-demand) for the IDR
                        // rate limiter that gates `force_idr` above.
                        if keyframe {
                            last_idr_at[idx] = Some(std::time::Instant::now());
                        }
                        // During a bench window, send the AU + its host-stage stamps in one
                        // call; the transport fills wire_send_ns at write_all (leave it 0).
                        // The first meta is itself the bench-window start signal.
                        if bench_this {
                            let meta = core_proto::FrameStageMeta {
                                frame_idx: bench_frame_idx,
                                capture_ns,
                                encode_start_ns,
                                encode_done_ns,
                                wire_send_ns: 0,
                            };
                            s.send_with_meta(lock.data().to_vec(), keyframe, meta);
                            bench_frame_idx += 1;
                            bench_remaining -= 1;
                        } else {
                            s.send_frame(lock.data().to_vec(), keyframe);
                        }
                        // Ground-truth wire counter: every AU pushed to streamer.send()
                        // counts as one frame on the wire, regardless of bench mode.
                        wire_frames_in_window = wire_frames_in_window.saturating_add(1);
                        wire_bytes_in_window = wire_bytes_in_window.saturating_add(au_bytes);
                    } else if let Some(f) = &mut p.file {
                        f.write_all(lock.data())?;
                    }
                    if let Some(t0) = wire_t0 {
                        frame_wire_us += t0.elapsed().as_micros() as u64;
                    }
                    drop(lock);
                    if !first_sent[idx] {
                        first_sent[idx] = true;
                        info!(
                            "output[{idx}] '{}' first frame sent → {}",
                            p.name, p.addr_label
                        );
                    }
                }
            }
        }
        // P3: per-frame GPU-health bookkeeping, after every output encoded this
        // frame. (1) Age slow events out of the sliding window. (2) Degrade on a
        // sustained slow RATE (never a single spike); clear back to Healthy only
        // after a fully-clean window (hysteresis). Faulted is sticky (set in the
        // wedge branch / by the P4 watchdog) and untouched here. (3) Stamp the
        // heartbeat so the P4 watchdog can catch an encode that never returns.
        while let Some(&front) = slow_events.front() {
            if front + SLOW_WINDOW_FRAMES < i {
                slow_events.pop_front();
            } else {
                break;
            }
        }
        if GPU_HEALTH.load(Ordering::Relaxed) != HEALTH_FAULTED {
            if slow_events.len() >= SLOW_DEGRADE_K {
                set_gpu_health(
                    HEALTH_DEGRADED,
                    &format!(
                        "{} slow encodes in the last {} frames",
                        slow_events.len(),
                        SLOW_WINDOW_FRAMES
                    ),
                );
            } else if slow_events.is_empty() {
                set_gpu_health(HEALTH_HEALTHY, "");
            }
        }
        LAST_FRAME_NS.store(unix_nanos_now(), Ordering::Relaxed);
        // P4: pet the systemd watchdog (~every 2s, fps-independent). If the loop
        // hangs, pings stop and systemd (WatchdogSec) force-restarts us — the
        // external backstop for a hang that wedges every in-process thread.
        if last_watchdog_ping.elapsed() >= std::time::Duration::from_secs(2) {
            sd_notify(&[sd_notify::NotifyState::Watchdog]);
            last_watchdog_ping = std::time::Instant::now();
        }

        // `render` in the existing framing-snapshot wire shape stays the SUM
        // of the new sub-stages — the control-ipc snapshot type is unchanged
        // (panel still reads one `render` bucket), but the log line below
        // splits it so a single grep tells you WHICH sub-stage was slow.
        let render_total = stage.mark();
        framing.render += render_total;
        framing.gl_blit += frame_gl_blit_us;
        framing.encode += frame_encode_us;
        framing.wire_enqueue += frame_wire_us;

        // Wire ground-truth rate — info every 1s. Without this, "host encode
        // cadence 60fps" was a believable lie (the configured target). This
        // line is the actual frames pushed to streamer.send() per second.
        let wire_elapsed = wire_window_start.elapsed();
        if wire_elapsed >= std::time::Duration::from_secs(1) {
            let secs = wire_elapsed.as_secs_f32().max(1e-3);
            // Stage 3: client-driven framerate range (DG_CLIENT_FPSPREF). The bench
            // client owns its [min, max] fps; max=0 = Auto (panel refresh). On CHANGE,
            // set cfg_fps_cap (the encode-pace ceiling, never above the session's
            // cfg.fps) and the adaptation floor; the refresh-cap recompute just below
            // then folds in the panel refresh. Idempotent + seeded inert at (0,0), so
            // a default is a no-op and the client's continuous re-sends don't re-stomp.
            // Reset on disconnect (atomic→0) so a new/reconnecting client re-applies.
            if let Some((min_fps, max_fps)) = bench_output
                .and_then(|idx| pipelines[idx].as_ref())
                .and_then(|p| p.streamer.as_ref())
                .map(|s| s.client_fps_pref())
            {
                let packed = (u64::from(min_fps) << 32) | u64::from(max_fps);
                if packed != last_client_fpspref {
                    last_client_fpspref = packed;
                    cfg_fps_cap = if max_fps == 0 {
                        cfg.fps as f32
                    } else {
                        (max_fps as f32).clamp(1.0, cfg.fps as f32)
                    };
                    fps_floor = if min_fps == 0 {
                        24.0
                    } else {
                        (min_fps as f32).clamp(24.0, cfg_fps_cap)
                    };
                    tracing::info!(
                        target: "compositor::wire",
                        "client fps range → floor={:.0} cap={:.0} (auto={})",
                        fps_floor, cfg_fps_cap, max_fps == 0,
                    );
                }
            }
            // Per-client refresh cap: never pace faster than the panel can display
            // (feeding a 60Hz panel 144fps overloads its decoder → dropped frames →
            // reference corruption). 0 = no client / a v2 client that didn't report →
            // fall back to the config ceiling. `.max(fps_floor)` keeps the downstream
            // `clamp(fps_floor, fps_cap)` valid for any (real ≥24Hz) panel.
            let client_refresh = bench_output
                .and_then(|idx| pipelines[idx].as_ref())
                .and_then(|p| p.streamer.as_ref())
                .map(|s| s.client_refresh_hz())
                .unwrap_or(0);
            fps_cap = if client_refresh > 0 {
                cfg_fps_cap.min(client_refresh as f32)
            } else {
                cfg_fps_cap
            }
            .max(fps_floor);
            let measured_fps = wire_frames_in_window as f32 / secs;
            // EWMA so the panel status field smooths out the 1s boundaries
            // (alpha 0.3 = ~3s effective window — fast enough to surface a
            // regression, slow enough to not flicker).
            measured_fps_ewma = 0.3 * measured_fps + 0.7 * measured_fps_ewma;

            // Drain the client's delivered-rate QoS (reported ~1×/s) into an EWMA.
            let mut got_qos = false;
            if let Some(rx) = qos_rx.as_mut() {
                while let Ok(q) = rx.try_recv() {
                    client_recv_fps_ewma = if client_recv_fps_ewma <= 0.0 {
                        q.recv_fps
                    } else {
                        0.5 * q.recv_fps + 0.5 * client_recv_fps_ewma
                    };
                    got_qos = true;
                }
            }
            // QoS staleness: reverse-path datagrams can drop on a congested WAN while
            // video keeps flowing, freezing client_recv_fps_ewma. Count the silence so
            // we can recover rather than stay latched low until reconnect.
            if got_qos {
                qos_stale_ticks = 0;
            } else {
                qos_stale_ticks = qos_stale_ticks.saturating_add(1);
            }
            // Per-output normalisation: measured_fps / wire_bytes are summed across ALL
            // streaming pipelines, but client QoS (recv_fps) is from ONE client
            // (bench_output). Divide by the live-stream count so the deficit compares
            // like with like — else 2 healthy quic clients read as ~1/N loss.
            let n_streaming = pipelines
                .iter()
                .flatten()
                .filter(|p| p.is_streaming_target_ready())
                .count()
                .max(1) as f32;
            let per_out_fps = (measured_fps / n_streaming).max(1.0);
            // Host-side forward-path loss (permille) — works even when the client sends
            // no QoS (e.g. an older APK). The fallback signal when client QoS is silent.
            let host_loss_permille = bench_output
                .and_then(|idx| pipelines[idx].as_ref())
                .and_then(|p| p.streamer.as_ref())
                .map(|s| s.loss_permille())
                .unwrap_or(0);

            // Stage 3: client-driven bitrate range (DG_CLIENT_BITRATEPREF) — the
            // PRIMARY adaptation lever. The connected (bench) client owns its
            // [min, max] kbps; max=0 means "uncapped, host owns the top". Apply the
            // moment it CHANGES: turn explicit RC ON bounded to the client cap (so
            // the bitrate lever is active even if the host preset left RC off), raise
            // the adaptation floor to the client min, and restart from the cap.
            // Idempotent — a constant pref applies once then no-ops, so the client's
            // continuous re-sends don't re-stomp, and a (0,0) default is inert from
            // t=0. A disconnect resets the streamer atomic to 0, so a reconnecting or
            // new client re-applies fresh.
            if let Some((min_kbps, max_kbps)) = bench_output
                .and_then(|idx| pipelines[idx].as_ref())
                .and_then(|p| p.streamer.as_ref())
                .map(|s| s.client_bitrate_pref())
            {
                let packed = (u64::from(min_kbps) << 32) | u64::from(max_kbps);
                if packed != last_client_bitratepref {
                    last_client_bitratepref = packed;
                    if min_kbps != 0 || max_kbps != 0 {
                        // Cap = the client max, or — uncapped — the host's configured
                        // cap (80 Mbps fallback when the host preset left RC unset).
                        // Either way clamped to the host's client-cap ceiling: the
                        // host knows its encoder and uplink, so a client can't
                        // command a rate past what the host allows.
                        const UNCAPPED_DEFAULT_KBPS: u32 = 80_000;
                        const HARD_MIN_FLOOR_KBPS: u32 = 500;
                        let ceiling = client_bitrate_ceiling(cfg);
                        let requested = if max_kbps != 0 {
                            max_kbps
                        } else {
                            resolve_rc_bitrate(cfg)
                                .map(|(avg, _)| avg)
                                .unwrap_or(UNCAPPED_DEFAULT_KBPS)
                        };
                        let avg_cap = requested.min(ceiling);
                        if avg_cap < requested {
                            tracing::info!(
                                target: "compositor::wire",
                                "client bitrate cap {requested}kbps clamped to host ceiling {ceiling}kbps",
                            );
                        }
                        let max_cap = avg_cap.saturating_mul(3) / 2;
                        bitrate_floor_kbps = if min_kbps == 0 {
                            NET_FLOOR_KBPS
                        } else {
                            min_kbps.clamp(HARD_MIN_FLOOR_KBPS, avg_cap)
                        };
                        rc_caps = Some((avg_cap, max_cap));
                        net_allowed_kbps = avg_cap;
                        last_reconfig_kbps = avg_cap;
                        net_lowwater_kbps = avg_cap;
                        net_low_ticks = 0;
                        net_clear_ticks = 0;
                        reconfigure_all_bitrate(
                            &pipelines, avg_cap, avg_cap, max_cap, cfg.fps, true,
                        );
                        tracing::info!(
                            target: "compositor::wire",
                            "client bitrate range → floor={}kbps cap={}kbps (uncapped={})",
                            bitrate_floor_kbps, avg_cap, max_kbps == 0,
                        );
                    } else {
                        // Client cleared its pref → restore the host's configured floor
                        // and disarm the bandwidth probe: the pref only reads (0,0)
                        // when the client disconnected, so the next client (or the
                        // same one reconnecting) probes fresh.
                        bitrate_floor_kbps = NET_FLOOR_KBPS;
                        probe_kicked = false;
                        probe_applied = false;
                        probe_transport = 0;
                    }
                }
            }

            // Issue #40: measured Auto bitrate. For a client that chose Auto
            // (max=0, uncapped) AND proves it can report delivered goodput
            // (v2 QoS with recv_kbps — an old APK never does, and then none of
            // this runs), run ONE connect-time bandwidth probe and resolve the
            // Auto cap from the MEASUREMENT instead of the configured guess.
            // The stream runs at the conservative configured cap in parallel,
            // so first light is never delayed; the deficit actuator keeps
            // adapting BELOW the measured cap exactly as before. The client's
            // transport hint only BOUNDS the probe's offered load (Ethernet
            // probes higher than WiFi so a fat pipe is found, WiFi stays
            // polite to the RF, unknown stays conservative) — it never sets
            // the cap: QUIC datagrams are congestion-controlled, so even a too
            // -high ceiling cannot push more than BBR allows onto the wire.
            if probe_enabled {
                if let Some(s) = bench_output
                    .and_then(|idx| pipelines[idx].as_ref())
                    .and_then(|p| p.streamer.as_ref())
                {
                    let (min_kbps, max_kbps) = s.client_bitrate_pref();
                    let uncapped_auto = (min_kbps != 0 || max_kbps != 0) && max_kbps == 0;
                    if uncapped_auto && s.client_reports_goodput() {
                        let transport = s.client_transport();
                        let retransport =
                            probe_kicked && transport != probe_transport && transport != 0;
                        if !probe_kicked || retransport {
                            // Probe ceiling by transport hint (offered load only).
                            let ceiling_kbps = match transport {
                                core_proto::latency::TRANSPORT_ETHERNET => 400_000,
                                core_proto::latency::TRANSPORT_WIFI => 250_000,
                                _ => 120_000,
                            };
                            s.start_probe(ceiling_kbps, 2_000);
                            probe_kicked = true;
                            probe_applied = false;
                            probe_transport = transport;
                            tracing::info!(
                                target: "compositor::wire",
                                "probe: Auto bitrate — probing up to {ceiling_kbps}kbps                                  (transport hint {transport}{})",
                                if retransport { ", transport changed" } else { "" },
                            );
                        }
                        if !probe_applied {
                            if let Some(measured) = s.probe_result() {
                                probe_applied = true;
                                // 80% of peak delivered goodput, bounded by the
                                // client floor and the host's client-cap ceiling.
                                let ceiling = client_bitrate_ceiling(cfg);
                                let avg_cap = (measured.saturating_mul(4) / 5)
                                    .max(bitrate_floor_kbps)
                                    .min(ceiling);
                                let max_cap = avg_cap.saturating_mul(3) / 2;
                                rc_caps = Some((avg_cap, max_cap));
                                net_allowed_kbps = avg_cap;
                                last_reconfig_kbps = avg_cap;
                                net_lowwater_kbps = avg_cap;
                                net_low_ticks = 0;
                                net_clear_ticks = 0;
                                reconfigure_all_bitrate(
                                    &pipelines, avg_cap, avg_cap, max_cap, cfg.fps, true,
                                );
                                tracing::info!(
                                    target: "compositor::wire",
                                    "probe: Auto bitrate cap → {avg_cap}kbps                                      (measured {measured}kbps delivered, ceiling {ceiling}kbps)",
                                );
                            }
                        }
                    }
                }
            }

            if qos_stale_ticks >= 3 {
                // No fresh client QoS (frozen recv_fps): either an old client that never
                // reports QoS, or reverse-path datagrams dropped on a congested WAN. Fall
                // back to the host's OWN forward-path loss — back off when it's high,
                // recover toward the cap when it's low. (Dead on a healthy LAN: got_qos is
                // true every tick, so qos_stale_ticks never reaches 3.)
                net_allowed_fps = fps_cap;
                if let Some((avg_cap, max_cap)) = rc_caps {
                    const LOSSY_PERMILLE: u32 = 20; // ≥2% forward loss = congested
                    let floor = bitrate_floor_kbps.min(avg_cap);
                    if host_loss_permille > LOSSY_PERMILLE {
                        net_clear_ticks = 0;
                        net_low_ticks = net_low_ticks.saturating_add(1);
                        if net_low_ticks >= 2 {
                            let sent_kbps =
                                (wire_bytes_in_window as f32 * 8.0 / 1000.0 / secs / n_streaming)
                                    .max(1.0);
                            let keep = (1.0 - host_loss_permille as f32 / 1000.0).clamp(0.3, 1.0);
                            let target = (sent_kbps * keep).max(floor as f32);
                            net_allowed_kbps = (target as u32).clamp(floor, avg_cap);
                        }
                    } else if net_allowed_kbps < avg_cap {
                        net_low_ticks = 0;
                        net_clear_ticks = net_clear_ticks.saturating_add(1);
                        if net_clear_ticks >= 3 {
                            net_allowed_kbps =
                                ((net_allowed_kbps as f32 * 1.2) as u32).min(avg_cap);
                        }
                    }
                    if net_allowed_kbps >= avg_cap {
                        net_lowwater_kbps = avg_cap;
                    }
                    if net_allowed_kbps.abs_diff(last_reconfig_kbps) * 100
                        > last_reconfig_kbps.max(1) * 8
                    {
                        let force_idr = net_allowed_kbps < net_lowwater_kbps;
                        if force_idr {
                            net_lowwater_kbps = net_allowed_kbps;
                        }
                        reconfigure_all_bitrate(
                            &pipelines,
                            net_allowed_kbps,
                            avg_cap,
                            max_cap,
                            cfg.fps,
                            force_idr,
                        );
                        tracing::info!(
                            target: "compositor::wire",
                            "net-bitrate (host-loss {}permille): {}→{}kbps",
                            host_loss_permille, last_reconfig_kbps, net_allowed_kbps,
                        );
                        last_reconfig_kbps = net_allowed_kbps;
                    }
                }
            } else if got_qos && client_recv_fps_ewma > 2.0 {
                // Deficit = delivered / actually-SENT (per client). NOT delivered/target,
                // which conflates a GPU/compute-bound undershoot (host sent fewer, client
                // got them all → ratio ≈ 1) with genuine last-mile loss.
                let deficit = client_recv_fps_ewma / per_out_fps;
                let lossy = deficit < 0.85;
                if lossy {
                    net_clear_ticks = 0;
                    net_low_ticks = net_low_ticks.saturating_add(1);
                } else {
                    net_low_ticks = 0;
                    net_clear_ticks = net_clear_ticks.saturating_add(1);
                }
                if let Some((avg_cap, max_cap)) = rc_caps {
                    // BITRATE adaptation (the real bandwidth lever); fps stays at cap.
                    // Estimate per-client path capacity from what got through:
                    // sent_kbps * delivered_fraction. Down fast (2 ticks), up slow (+20%
                    // after 3 clear ticks). Floor = client min (default 2 Mbps), capped.
                    let floor = bitrate_floor_kbps.min(avg_cap);
                    let sent_kbps =
                        (wire_bytes_in_window as f32 * 8.0 / 1000.0 / secs / n_streaming).max(1.0);
                    if lossy && net_low_ticks >= 2 {
                        let target = (sent_kbps * deficit).max(floor as f32);
                        net_allowed_kbps = (target as u32).clamp(floor, avg_cap);
                    } else if !lossy && net_clear_ticks >= 3 && net_allowed_kbps < avg_cap {
                        net_allowed_kbps = ((net_allowed_kbps as f32 * 1.2) as u32).min(avg_cap);
                    }
                    net_allowed_fps = fps_cap; // bitrate carries adaptation; fps inert
                                               // Reconfigure only on a meaningful (>8%) change — so a healthy link
                                               // (net_allowed_kbps pinned at avg_cap) NEVER reconfigures (inert).
                    if net_allowed_kbps.abs_diff(last_reconfig_kbps) * 100
                        > last_reconfig_kbps.max(1) * 8
                    {
                        // Force a keyframe only on a NEW low (not every down-step) so a
                        // marginal-loss oscillation doesn't inject ~1 extra IDR/s.
                        let force_idr = net_allowed_kbps < net_lowwater_kbps;
                        if force_idr {
                            net_lowwater_kbps = net_allowed_kbps;
                        }
                        reconfigure_all_bitrate(
                            &pipelines,
                            net_allowed_kbps,
                            avg_cap,
                            max_cap,
                            cfg.fps,
                            force_idr,
                        );
                        tracing::info!(
                            target: "compositor::wire",
                            "net-bitrate: {}→{}kbps (cap {}, deficit={:.2}, sent={:.0}kbps, idr={})",
                            last_reconfig_kbps, net_allowed_kbps, avg_cap, deficit, sent_kbps, force_idr,
                        );
                        last_reconfig_kbps = net_allowed_kbps;
                    }
                    if net_allowed_kbps >= avg_cap {
                        net_lowwater_kbps = avg_cap; // episode over; allow a future IDR
                    }
                } else {
                    // No explicit RC (preset heuristic RC): fall back to the fps ceiling.
                    if lossy && net_low_ticks >= 2 {
                        net_allowed_fps = (client_recv_fps_ewma * 1.15).clamp(fps_floor, fps_cap);
                    } else if !lossy && net_clear_ticks >= 3 && net_allowed_fps < fps_cap {
                        net_allowed_fps = (net_allowed_fps + 6.0).min(fps_cap);
                    }
                }
            } else if got_qos {
                net_allowed_fps = fps_cap;
                net_low_ticks = 0;
            }

            // Stage 2: apply a client-driven resolution range (DG_CLIENT_RESPREF).
            // The connected client owns its own [floor, target]; apply it the moment
            // it CHANGES (per-output, aspect-preserving), then let the ladder adapt
            // within it. Idempotent — a constant pref applies once then no-ops, so a
            // panel SetResolution isn't stomped by the client's continuous re-sends.
            for (idx, slot) in pipelines.iter_mut().enumerate() {
                let Some(p) = slot.as_mut() else { continue };
                let Some((target_h, floor_h)) = p.streamer.as_ref().map(|s| s.client_res_pref())
                else {
                    continue;
                };
                let packed = (u64::from(target_h) << 32) | u64::from(floor_h);
                if packed == last_client_respref[idx] {
                    continue;
                }
                last_client_respref[idx] = packed;
                // The client's target/floor are SHORT-SIDE resolutions; the ladder is
                // height-based, so convert for a portrait output (identity in landscape)
                // — otherwise a 1080 target on a 1080x2400 portrait render is capped to
                // 486x1080. See short_side_target_to_height.
                let target_eq = short_side_target_to_height(p.width, p.height, target_h);
                let floor_eq = short_side_target_to_height(p.width, p.height, floor_h);
                p.res_ceiling_h = if target_eq == 0 || target_eq >= p.height {
                    0
                } else {
                    target_eq
                };
                p.res_floor_h = floor_eq.min(p.height);
                // Give the new range a fresh adaptation start — but only when this
                // is the output whose QoS actually drives the (global) ladder, so a
                // slider change on one extend-mode output doesn't zero the ladder
                // credit another output is accumulating.
                if bench_output == Some(idx) {
                    res_low_ticks = 0;
                    res_clear_ticks = 0;
                }
                let (cw, ch) = coded_for_height(p.width, p.height, target_eq);
                apply_coded_resolution(p, cw, ch);
                tracing::info!(
                    target: "compositor::res",
                    "output={} client requested target={}p floor={}p (coded ceiling height={})",
                    p.name, target_h, floor_h, p.res_ceiling_h,
                );
            }

            // RESOLUTION ladder (Phase 2): the rung BELOW the bitrate/fps levers.
            // It only steps once the lever is EXHAUSTED (bitrate pinned at
            // the floor, or — on preset RC — fps pinned at the floor) and the link
            // is still pressured: then drop one rung. It recovers one rung only
            // after the lower lever is back at its cap for a sustained spell (slow,
            // to avoid res oscillation). On a healthy link the lower lever sits at
            // cap and `rung_above`/`rung_below` no-op, so this is inert — and fully
            // inert at native with no ceiling selected. Applies to every streaming
            // pipeline (per-output, aspect-preserving).
            let (lever_exhausted, lever_recovered) = lever_state(
                rc_caps,
                net_allowed_kbps,
                bitrate_floor_kbps,
                net_allowed_fps,
                fps_floor,
                fps_cap,
            );
            if lever_exhausted {
                res_clear_ticks = 0;
                res_low_ticks = res_low_ticks.saturating_add(1);
            } else if lever_recovered {
                res_low_ticks = 0;
                res_clear_ticks = res_clear_ticks.saturating_add(1);
            } else {
                // Mid-band: bleed both toward zero so a flapping link doesn't bank
                // credit toward either a drop or a raise.
                res_low_ticks = res_low_ticks.saturating_sub(1);
                res_clear_ticks = res_clear_ticks.saturating_sub(1);
            }
            // ~3s of exhausted pressure before dropping; ~8s fully clear before
            // raising (drop fast, recover slow).
            const RES_DROP_AFTER_TICKS: u32 = 3;
            const RES_RAISE_AFTER_TICKS: u32 = 8;
            if res_low_ticks >= RES_DROP_AFTER_TICKS {
                res_low_ticks = 0;
                for slot in pipelines.iter_mut() {
                    if let Some(p) = slot.as_mut() {
                        if p.streamer.is_some() {
                            let floor = effective_floor(p.height, p.res_ceiling_h, p.res_floor_h);
                            if let Some(h) = rung_below(p.height, p.coded_h, floor) {
                                let (cw, ch) = coded_for_height(p.width, p.height, h);
                                apply_coded_resolution(p, cw, ch);
                                tracing::info!(
                                    target: "compositor::res",
                                    "ladder: output={} dropped to {}p under sustained link pressure",
                                    p.name, ch,
                                );
                            }
                        }
                    }
                }
            } else if res_clear_ticks >= RES_RAISE_AFTER_TICKS {
                res_clear_ticks = 0;
                for slot in pipelines.iter_mut() {
                    if let Some(p) = slot.as_mut() {
                        if p.streamer.is_some() {
                            if let Some(h) = rung_above(p.height, p.coded_h, p.res_ceiling_h) {
                                let (cw, ch) = coded_for_height(p.width, p.height, h);
                                apply_coded_resolution(p, cw, ch);
                                tracing::info!(
                                    target: "compositor::res",
                                    "ladder: output={} recovered to {}p (link clear)",
                                    p.name, ch,
                                );
                            }
                        }
                    }
                }
            }

            // W8 auto-pace control loop: derive the sustainable rate from measured raw
            // per-frame work and pace there, with ~18% headroom so the pacer always has
            // slack to sleep (stable cadence, absorbs frame-time variance). Self-tunes
            // per scene/resolution; bounded by [fps_floor, fps_cap] and now also by the
            // network ceiling (pace at the lower of compute- and network-sustainable).
            if auto_pace {
                let frames = wire_frames_in_window.max(1) as f32;
                let avg_work_us = work_us_in_window as f32 / frames;
                work_us_ewma = 0.4 * avg_work_us + 0.6 * work_us_ewma;
                let sustainable = 1e6 / (work_us_ewma.max(1.0) * 1.18);
                target_fps = sustainable.min(net_allowed_fps).clamp(fps_floor, fps_cap);
                current_pace = Duration::from_secs_f64(1.0 / target_fps as f64);
                tracing::info!(
                    target: "compositor::wire",
                    "auto-pace: work={:.1}ms sustainable={:.0} net_fps={:.0} recv={:.0} target={:.0} cap={:.0}",
                    work_us_ewma / 1000.0, sustainable, net_allowed_fps, client_recv_fps_ewma, target_fps, fps_cap,
                );
            } else {
                // Fixed-pace mode (the default, fps_auto=false) still honours the
                // network ceiling so a thin link isn't flooded at the user-selected
                // fps — otherwise the whole QoS machinery would be dead weight in the
                // default config. Re-clamps to fps_cap each tick, so the rate climbs
                // back to the user's selection once net_allowed_fps recovers.
                let fixed = fps_cap.min(net_allowed_fps).clamp(fps_floor, fps_cap);
                if (fixed - target_fps).abs() > 0.5 {
                    target_fps = fixed;
                    current_pace = Duration::from_secs_f64(1.0 / target_fps.max(1.0) as f64);
                    tracing::info!(
                        target: "compositor::wire",
                        "net-pace (fixed): net_fps={:.0} recv={:.0} target={:.0} cap={:.0}",
                        net_allowed_fps, client_recv_fps_ewma, target_fps, fps_cap,
                    );
                }
            }
            tracing::info!(
                target: "compositor::wire",
                "host_send_fps measured={:.1} target={:.0} bytes_sent_1s={}",
                measured_fps, target_fps, wire_bytes_in_window,
            );
            wire_window_start = std::time::Instant::now();
            wire_frames_in_window = 0;
            wire_bytes_in_window = 0;
            work_us_in_window = 0;
        }

        // Write the throttled preview JPEG (the downscale + encode happens off
        // the GL critical path, here on the CPU).
        if let Some((rgba, w, h, stride)) = preview_capture.take() {
            crate::preview::write_preview(&rgba, w, h, stride);
            last_preview = std::time::Instant::now();
        }

        // Frame callbacks: send for every output so windows on either can
        // schedule their next frame. Smithay's Window::send_frame only fires
        // for surfaces actually on that output, so this isn't wasteful.
        let now = data.state.start_time.elapsed();
        for slot in pipelines.iter() {
            let Some(p) = slot.as_ref() else { continue };
            for w in data.state.space.elements() {
                w.send_frame(&p.output, now, Some(Duration::ZERO), |_, _| {
                    Some(p.output.clone())
                });
            }
        }
        // wp_presentation feedback, answered at the same cadence: the headless
        // encode has no vblank, so the pace tick IS the presentation time.
        // gamescope's Wayland backend times its present-wait off these events
        // (issue #38); a client that never gets an answer would stall its pacing.
        // The refresh we report is the output's NOMINAL mode refresh — NOT the
        // live auto-pace interval. gamescope treats every advertised-refresh
        // change as a mode switch and re-creates its full set of output images;
        // under VRAM pressure that reallocation fails and its
        // vulkan_remake_output_images() assert SIGABRTs the whole nested session
        // (gamescope rendervulkan.cpp:3376, seen twice with NMS@4K under
        // fps_auto pacer wobble, 2026-07-10/11). Timestamps stay real; only the
        // advertised refresh is pinned to the mode.
        // Feedbacks are MOVED out of surface state, so a surface spanning two
        // outputs is answered once (by the first pipeline that takes it).
        let pres_time = presentation_clock.now();
        for slot in pipelines.iter() {
            let Some(p) = slot.as_ref() else { continue };
            let mut ofb = smithay::desktop::utils::OutputPresentationFeedback::new(&p.output);
            for w in data.state.space.elements() {
                w.take_presentation_feedback(
                    &mut ofb,
                    |_, _| Some(p.output.clone()),
                    |_, _| wp_presentation_feedback::Kind::empty(),
                );
            }
            // Mode refresh is in mHz; interval = 1000/mHz seconds. Falls back to
            // the pace interval only if the output somehow has no current mode.
            let nominal = p
                .output
                .current_mode()
                .filter(|m| m.refresh > 0)
                .map(|m| Duration::from_secs_f64(1000.0 / m.refresh as f64))
                .unwrap_or(current_pace);
            ofb.presented(
                pres_time,
                smithay::wayland::presentation::Refresh::fixed(nominal),
                u64::from(i) + 1,
                wp_presentation_feedback::Kind::Vsync,
            );
        }
        let _ = data.display_handle.flush_clients();
        framing.post += stage.mark();

        // W8 adaptive single pacer across all outputs. `current_pace` is the live
        // per-frame interval (fixed = 1/fps, or auto-tuned above). Relative
        // next_deadline so a changing pace applies cleanly; falling behind (work >
        // pace) resets to now so we never accumulate sleep debt — the host then runs
        // at its raw rate. Record this frame's raw work time first (for the auto loop).
        work_us_in_window =
            work_us_in_window.saturating_add(frame_start.elapsed().as_micros() as u64);
        next_deadline += current_pace;
        let now = std::time::Instant::now();
        if next_deadline > now {
            std::thread::sleep(next_deadline - now);
        } else {
            next_deadline = now;
        }
        framing.pace += stage.mark();

        // Refresh the shared status the control thread serves to the panel: one
        // OutputStatus per pipeline, one ClientStatus per connected output.
        // Throttled to STATUS_PUSH_INTERVAL — see the const above for why.
        let frames_sent = u64::from(i) + 1;
        let should_push_status = last_status_push.elapsed() >= STATUS_PUSH_INTERVAL;
        if should_push_status {
            last_status_push = std::time::Instant::now();
        }
        if should_push_status {
            if let Ok(mut st) = status.lock() {
                let mut outputs = Vec::with_capacity(pipelines.len());
                let mut clients = Vec::new();
                let mut any_connected = false;
                for slot in pipelines.iter() {
                    let Some(p) = slot.as_ref() else { continue };
                    let connected = p.streamer.as_ref().is_some_and(|s| s.is_connected());
                    any_connected |= connected;
                    outputs.push(control_ipc::OutputStatus {
                        name: p.name.clone(),
                        w: p.width,
                        h: p.height,
                        x: p.output.current_location().x,
                        y: p.output.current_location().y,
                        addr: p.addr_label.clone(),
                        connected,
                    });
                    if connected {
                        if mirror_enabled() {
                            // MIRROR: one ClientStatus PER mirrored client (this
                            // streamer fans one encode to N), with each client's
                            // self-measured wire fps + frame count from the
                            // transport registry. `connected_clients()` yields the
                            // live set; the panel renders one client card each.
                            if let Some(s) = p.streamer.as_ref() {
                                for snap in s.connected_clients() {
                                    clients.push(control_ipc::ClientStatus {
                                        addr: snap.addr,
                                        pubkey: snap.pubkey,
                                        connected: true,
                                        encoder: cfg.video_codec.encoder_label().into(),
                                        codec: cfg.video_codec.label().into(),
                                        fps: snap.fps,
                                        frames_sent: snap.frames_sent,
                                        coded_w: p.coded_w,
                                        coded_h: p.coded_h,
                                        res_ceiling_h: p.res_ceiling_h,
                                        res_floor_h: p.res_floor_h,
                                    });
                                }
                            }
                        } else {
                            clients.push(control_ipc::ClientStatus {
                                // The remote addr the live client dialed from (the panel
                                // shows it next to the peer); None for an idle pipeline.
                                addr: p.streamer.as_ref().and_then(|s| s.connected_addr()),
                                // The connected client's verified pubkey (hex), so the
                                // panel can show *which* paired peer is live, not just a
                                // count. None for an anonymous (unpaired) client.
                                pubkey: p.streamer.as_ref().and_then(|s| s.connected_pubkey()),
                                connected: true,
                                encoder: cfg.video_codec.encoder_label().into(),
                                codec: cfg.video_codec.label().into(),
                                // MEASURED wire rate, NOT the configured target. The
                                // status panel's `fps` field is now ground-truth: if
                                // the host is dropping to 2.5 fps under a wedge, this
                                // reads 2.5, not 60. Rounded to nearest integer (the
                                // u32 wire shape doesn't carry sub-fps precision).
                                fps: measured_fps_ewma.round().max(0.0) as u32,
                                frames_sent,
                                coded_w: p.coded_w,
                                coded_h: p.coded_h,
                                res_ceiling_h: p.res_ceiling_h,
                                res_floor_h: p.res_floor_h,
                            });
                        }
                    }
                }
                st.streaming = any_connected;
                STREAMING.store(any_connected, Ordering::Relaxed);
                // Seamless redirect marker (opt-in): reconcile it against BOTH the
                // streaming state and the primary output's capture source. The
                // marker is suppressed in desktop-mirror mode so a game the user
                // starts from the mirrored real desktop lands in the mirror
                // (wayland-0) rather than the unwatched virtual display. The
                // reconcile is idempotent + edge-tracked internally, so calling it
                // every status tick only touches disk on an actual transition.
                reconcile_redirect_marker(&pipelines, &socket_name);
                st.outputs = outputs;
                st.clients = clients;
                st.video_codec = Some(cfg.video_codec.label().into());
                // The live primary-client policy (issue #19), so the panel's
                // star reflects what the host is actually enforcing. Read from
                // the transport global (set at startup + on SetPrimaryPeer) —
                // no file I/O on the status path.
                st.primary_pubkey = transport::primary_peer_hex();
                // HDR label when the session is emitting a 10-bit Main10 stream.
                // The panel/tray surface this; None on an SDR session.
                st.hdr_active = if hdr { Some("HDR10".into()) } else { None };
                // Publish the stream fan-out mode. In mirror, report what the
                // single shared encode is currently producing (geometry of the
                // primary output, measured fps, live bitrate ceiling) so the
                // panel can show the throttle with real numbers. `limited_by`
                // stays None until per-client caps are wired (the transport
                // fan-out stage) — `client_count` already tells the panel whether
                // a clamp is actually biting (≥2 clients) or just armed.
                st.stream_mode = if mirror_enabled() {
                    let (w, h) = st.outputs.first().map_or((0, 0), |o| (o.w, o.h));
                    control_ipc::StreamMode::Mirror {
                        clamp: Box::new(control_ipc::MirrorClamp {
                            w,
                            h,
                            fps: measured_fps_ewma.round().max(0.0) as u32,
                            bitrate_kbps: rc_caps.map(|(avg, _)| avg).unwrap_or(0),
                            limited_by: None,
                            client_count: st.clients.len() as u32,
                        }),
                    }
                } else {
                    control_ipc::StreamMode::Single
                };
                // P3: publish the render loop's GPU-health state → tray/panel.
                st.gpu_health = current_gpu_health();
                st.media = media_path.as_ref().map(|p| control_ipc::MediaStatus {
                    path: p.clone(),
                    playing: media_child.is_some(),
                });
                // Per-controller live state for the panel's Controllers tab. Two
                // independent gates make this almost-free when the tab is closed:
                //
                //  (a) Subscriber gate — only build if a panel has poked the
                //      controllers deadline recently (its Controllers tab is
                //      mounted). Otherwise the field is empty (clearer than
                //      stale): the only consumer is the panel itself, and a
                //      panel that isn't asking won't render it.
                //
                //  (b) ~1 Hz throttle on top of `STATUS_PUSH_INTERVAL` (~4 Hz).
                //      Each rebuild walks `evdev::enumerate()` per controller —
                //      ~25 file opens × N pads — and the panel polls at 1 Hz
                //      anyway, so a faster rebuild is pure overhead. Between
                //      rebuilds we keep the last-built vector so the panel still
                //      sees fresh-enough data on its 1 Hz poll.
                if controllers_deadline.is_active() {
                    if last_controllers_build.elapsed() >= CONTROLLERS_BUILD_INTERVAL {
                        let snap = host_input.liveness_snapshot();
                        st.controllers = crate::control::build_controllers_status(
                            &host_input.controller_profiles,
                            &snap,
                        );
                        last_controllers_build = std::time::Instant::now();
                    }
                    // else: leave st.controllers as-is (last built ≤1s ago).
                } else {
                    // No subscriber → clear rather than keep stale: a panel
                    // re-opening the tab will re-arm the subscription and the
                    // next 1 Hz rebuild fills the vector again.
                    st.controllers = Vec::new();
                }
            }
        }
        framing.status += stage.mark();
        framing.commit_frame_and_maybe_flush();

        i = i.saturating_add(1);
    }

    if let Some(mut c) = media_child.take() {
        let _ = c.kill();
    }
    if let Some(mut c) = child {
        let _ = c.kill();
    }
    // Pipelines drop here: per pipeline, bitstreams → reg (unregister) →
    // _session (NVENC encoder release) → cuda_buf (cuMemFree). Give the
    // QUIC streamers a moment to drain before the streamers' threads end.
    drop(pipelines);
    std::thread::sleep(Duration::from_millis(1500));
    if relaunch_requested {
        // Pipelines dropped above (their QUIC UDP ports are free): re-exec so the
        // new image rebinds cleanly and picks up the on-disk layout/config.
        let e = crate::control::reexec_self();
        anyhow::bail!("relaunch re-exec failed: {e}");
    }
    info!(
        "multi-output capture complete: {} outputs, {} frames",
        cfg.outputs.len(),
        i
    );
    Ok(())
}

// =====================================================================
// Per-stage frame timing instrumentation (gated by PHAROS_FRAME_TIMINGS=1)
// =====================================================================
//
// Records, for each render-loop iteration, the wall-clock microseconds spent
// in eight discrete stages and flushes one greppable summary line to the host
// log every flush window (default 5s). When the env var is unset the gate is
// cheap: a single bool check at each `stage.mark()` and no `Instant::now()`
// call. When set, the cost is ~9 `Instant::now()` calls per frame (~100ns
// each on x86_64) and a 5s flush — negligible against the render budget.
//
// Stage cursor — advances through a frame's stages.
struct StageCursor {
    enabled: bool,
    last: Option<std::time::Instant>,
}

impl StageCursor {
    fn start(enabled: bool) -> Self {
        Self {
            enabled,
            last: if enabled {
                Some(std::time::Instant::now())
            } else {
                None
            },
        }
    }

    /// Microseconds since the previous mark (or 0 when the gate is off).
    /// Advances the cursor so the next call measures the next stage.
    fn mark(&mut self) -> u64 {
        if !self.enabled {
            return 0;
        }
        let now = std::time::Instant::now();
        let us = self
            .last
            .map(|t| now.duration_since(t).as_micros() as u64)
            .unwrap_or(0);
        self.last = Some(now);
        us
    }
}

// One stage's running stats over the current flush window.
#[derive(Default)]
struct StageStats {
    samples: Vec<u64>, // microseconds, one per frame
}

impl StageStats {
    fn add(&mut self, us: u64) {
        self.samples.push(us);
    }

    /// (mean, p95, max) in microseconds, or (0, 0, 0) for an empty window.
    fn summary(&self) -> (u64, u64, u64) {
        if self.samples.is_empty() {
            return (0, 0, 0);
        }
        let n = self.samples.len();
        let sum: u128 = self.samples.iter().map(|&x| x as u128).sum();
        let mean = (sum / n as u128) as u64;
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let p95_idx = ((n as f64) * 0.95) as usize;
        let p95 = sorted[p95_idx.min(n - 1)];
        let max = *sorted.last().unwrap();
        (mean, p95, max)
    }

    fn reset(&mut self) {
        self.samples.clear();
    }

    /// Wire-shape snapshot of the histogram (clamped to `u32` since panel
    /// rows display µs and one stage measured in seconds is already a bug we
    /// want to see). An empty histogram returns all zeros.
    fn to_snapshot(&self) -> control_ipc::StageStats {
        let (mean, p95, max) = self.summary();
        control_ipc::StageStats {
            mean_us: mean.min(u32::MAX as u64) as u32,
            p95_us: p95.min(u32::MAX as u64) as u32,
            max_us: max.min(u32::MAX as u64) as u32,
            samples: self.samples.len().min(u32::MAX as usize) as u32,
        }
    }
}

/// Cloneable handle to the live-subscriber deadline behind a [`FrameTimings`].
/// The control thread holds one and pushes the deadline forward with
/// [`Self::poke`] every time a panel asks for a snapshot; the render loop
/// reads it from inside [`FrameTimings::refresh_gate`].
///
/// The handle is independent of the [`FrameTimings`] itself (only the inner
/// atomic + origin instant) so the control server can keep extending the
/// subscription window without touching the render-loop-owned struct.
#[derive(Clone)]
pub struct SubscriberDeadline {
    until_ns: Arc<std::sync::atomic::AtomicU64>,
    start_mono: std::time::Instant,
}

/// Standalone deadline handle used only by the `control` module tests, which
/// drive the accept loop without a real render loop / `FrameTimings`. Returns
/// a fresh handle backed by its own atomic; pokes against it have no effect
/// anywhere else.
#[cfg(test)]
pub fn test_only_subscriber_deadline() -> SubscriberDeadline {
    SubscriberDeadline::fresh()
}

impl SubscriberDeadline {
    /// Build a fresh, standalone subscriber deadline backed by its own atomic.
    /// Used to create the controllers-tab gate alongside the frame-timings one:
    /// they're independent (a panel can have only the Controllers view open and
    /// the framing gate stays cold, or vice versa), so each gets its own
    /// instance of the same generic type.
    pub fn fresh() -> Self {
        SubscriberDeadline {
            until_ns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            start_mono: std::time::Instant::now(),
        }
    }

    /// True iff the current monotonic clock is still inside the window pushed
    /// by the most recent [`Self::poke`]. The render loop uses this to skip
    /// expensive work (e.g. rebuilding the controllers status) when no panel
    /// has asked for it recently.
    pub fn is_active(&self) -> bool {
        use std::sync::atomic::Ordering;
        let now_ns = self.start_mono.elapsed().as_nanos() as u64;
        let deadline_ns = self.until_ns.load(Ordering::Acquire);
        now_ns < deadline_ns
    }

    /// Bump the deadline so instrumentation stays hot for at least `ttl`
    /// longer. If a previous subscriber already pushed further out we keep
    /// the later deadline (never shorten — that would let a fast-clicking
    /// panel race itself into a closed window).
    pub fn poke(&self, ttl: Duration) {
        use std::sync::atomic::Ordering;
        let now_ns = self.start_mono.elapsed().as_nanos() as u64;
        let new_until = now_ns.saturating_add(ttl.as_nanos() as u64);
        // Compare-and-extend: only widen the window.
        let mut cur = self.until_ns.load(Ordering::Acquire);
        while new_until > cur {
            match self.until_ns.compare_exchange_weak(
                cur,
                new_until,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// Whole-frame timing accumulator. Each field is a `u64` µs counter populated
/// by `stage.mark()` calls inside the main loop, then committed to per-stage
/// histograms once per frame by [`commit_frame_and_maybe_flush`]. The flush
/// emits one INFO line tagged `compositor::framing` with `mean/p95/max` per
/// stage and resets, ~5s by default.
///
/// The instrumentation gate is the union of two sources: the
/// `PHAROS_FRAME_TIMINGS` env var (sticky-on for ad-hoc host-side debugging)
/// and a live subscription deadline ([`Self::subscribers_until_ns`]) pushed
/// forward by every `GetFrameTimings` request. With neither active the cost
/// stays zero — `stage.mark()` short-circuits before any `Instant::now()`.
struct FrameTimings {
    enabled_by_env: bool,
    /// Cached union of `enabled_by_env || now < subscribers_until_ns`,
    /// recomputed once per frame so the per-stage `stage.mark()` hot path
    /// reads a plain bool. The deadline behind it is bumped by
    /// [`Self::poke_subscriber`] (called from the control thread).
    enabled: bool,
    /// Monotonic nanoseconds (since process start) up to which a live
    /// subscriber is keeping instrumentation hot. `0` = no subscriber. Lives
    /// behind an `Arc<AtomicU64>` so the control thread can push the deadline
    /// forward from another thread without locking the render loop.
    subscribers_until_ns: Arc<std::sync::atomic::AtomicU64>,
    /// Monotonic origin: nanoseconds elapsed since `start_mono` is what the
    /// gate compares against. Lets us store the deadline as a single `u64`
    /// rather than serialising `Instant`.
    start_mono: std::time::Instant,
    flush_every: Duration,
    last_flush: std::time::Instant,
    frames: u64,
    // Per-frame stage accumulators (reset each frame in commit).
    dispatch: u64,
    ctrl: u64,
    input: u64,
    gyro: u64,
    render: u64,
    // Sub-stages of `render` — the encode-send block is the only window in
    // the frame that can hide hundreds of ms, so we split it. `render` stays
    // their sum so the existing control-ipc snapshot wire shape (one `render`
    // bucket) is unchanged; the framing log line below shows all three.
    gl_blit: u64,
    encode: u64,
    wire_enqueue: u64,
    post: u64,
    pace: u64,
    status: u64,
    // Per-flush-window stage histograms.
    h_dispatch: StageStats,
    h_ctrl: StageStats,
    h_input: StageStats,
    h_gyro: StageStats,
    h_render: StageStats,
    h_gl_blit: StageStats,
    h_encode: StageStats,
    h_wire_enqueue: StageStats,
    h_post: StageStats,
    h_pace: StageStats,
    h_status: StageStats,
}

impl FrameTimings {
    fn from_env() -> Self {
        let enabled_by_env = std::env::var_os("PHAROS_FRAME_TIMINGS").is_some_and(|v| v != "0");
        let flush_secs = std::env::var("PHAROS_FRAME_TIMINGS_FLUSH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(5);
        Self {
            enabled_by_env,
            enabled: enabled_by_env, // refreshed each frame from the gate union
            subscribers_until_ns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            start_mono: std::time::Instant::now(),
            flush_every: Duration::from_secs(flush_secs),
            last_flush: std::time::Instant::now(),
            frames: 0,
            dispatch: 0,
            ctrl: 0,
            input: 0,
            gyro: 0,
            render: 0,
            gl_blit: 0,
            encode: 0,
            wire_enqueue: 0,
            post: 0,
            pace: 0,
            status: 0,
            h_dispatch: StageStats::default(),
            h_ctrl: StageStats::default(),
            h_input: StageStats::default(),
            h_gyro: StageStats::default(),
            h_render: StageStats::default(),
            h_gl_blit: StageStats::default(),
            h_encode: StageStats::default(),
            h_wire_enqueue: StageStats::default(),
            h_post: StageStats::default(),
            h_pace: StageStats::default(),
            h_status: StageStats::default(),
        }
    }

    /// Refresh [`Self::enabled`] from the gate union (env var OR live
    /// subscriber). Called once per frame at the top of the loop so the eight
    /// `stage.mark()` calls in between read a stable cached bool — they never
    /// see the gate flip mid-frame (which would leave one stage timed and the
    /// next not, producing a misleading huge spike on the boundary).
    fn refresh_gate(&mut self) {
        if self.enabled_by_env {
            self.enabled = true;
            return;
        }
        let now_ns = self.start_mono.elapsed().as_nanos() as u64;
        let deadline_ns = self
            .subscribers_until_ns
            .load(std::sync::atomic::Ordering::Acquire);
        self.enabled = now_ns < deadline_ns;
    }

    /// A clone of the [`Self::subscribers_until_ns`] handle so the control
    /// server can push the deadline forward from another thread. The render
    /// loop owns the [`FrameTimings`] itself; only the atomic deadline is
    /// shared.
    fn subscribers_handle(&self) -> SubscriberDeadline {
        SubscriberDeadline {
            until_ns: self.subscribers_until_ns.clone(),
            start_mono: self.start_mono,
        }
    }

    /// Live snapshot of the current (in-progress) per-stage histograms — does
    /// **not** reset them, so the existing 5s flush keeps its window intact
    /// and the panel sees fresh data as it accumulates. Cheap (a sort over
    /// each stage's samples Vec).
    fn snapshot(&self) -> control_ipc::FrameTimingsSnapshot {
        control_ipc::FrameTimingsSnapshot {
            window_frames: self.h_dispatch.samples.len() as u64,
            dispatch: self.h_dispatch.to_snapshot(),
            ctrl: self.h_ctrl.to_snapshot(),
            input: self.h_input.to_snapshot(),
            gyro: self.h_gyro.to_snapshot(),
            render: self.h_render.to_snapshot(),
            post: self.h_post.to_snapshot(),
            pace: self.h_pace.to_snapshot(),
            status: self.h_status.to_snapshot(),
        }
    }

    /// End-of-frame: roll the per-frame counters into the histograms, reset
    /// them, and if a flush window has elapsed, emit the summary line.
    fn commit_frame_and_maybe_flush(&mut self) {
        if !self.enabled {
            return;
        }
        self.h_dispatch.add(self.dispatch);
        self.h_ctrl.add(self.ctrl);
        self.h_input.add(self.input);
        self.h_gyro.add(self.gyro);
        self.h_render.add(self.render);
        self.h_gl_blit.add(self.gl_blit);
        self.h_encode.add(self.encode);
        self.h_wire_enqueue.add(self.wire_enqueue);
        self.h_post.add(self.post);
        self.h_pace.add(self.pace);
        self.h_status.add(self.status);
        // Reset per-frame accumulators.
        self.dispatch = 0;
        self.ctrl = 0;
        self.input = 0;
        self.gyro = 0;
        self.render = 0;
        self.gl_blit = 0;
        self.encode = 0;
        self.wire_enqueue = 0;
        self.post = 0;
        self.pace = 0;
        self.status = 0;
        self.frames += 1;

        if self.last_flush.elapsed() >= self.flush_every {
            self.flush();
            self.last_flush = std::time::Instant::now();
        }
    }

    fn flush(&mut self) {
        // Format `stage=mean/p95/max` so all three numbers grep + plot from
        // one line per window; max in particular is the spike detector.
        let fmt = |name: &str, s: &StageStats| -> String {
            let (m, p, mx) = s.summary();
            format!("{name}={m}/{p}/{mx}")
        };
        let frames = self.h_dispatch.samples.len();
        tracing::info!(
            target: "compositor::framing",
            "frame_timings_us window_frames={frames} \
             {} {} {} {} {} {} {} {} {} {} {}",
            fmt("dispatch", &self.h_dispatch),
            fmt("ctrl", &self.h_ctrl),
            fmt("input", &self.h_input),
            fmt("gyro", &self.h_gyro),
            fmt("render", &self.h_render),
            fmt("gl_blit", &self.h_gl_blit),
            fmt("encode", &self.h_encode),
            fmt("wire_enqueue", &self.h_wire_enqueue),
            fmt("post", &self.h_post),
            fmt("pace", &self.h_pace),
            fmt("status", &self.h_status),
        );
        self.h_dispatch.reset();
        self.h_ctrl.reset();
        self.h_input.reset();
        self.h_gyro.reset();
        self.h_render.reset();
        self.h_gl_blit.reset();
        self.h_encode.reset();
        self.h_wire_enqueue.reset();
        self.h_post.reset();
        self.h_pace.reset();
        self.h_status.reset();
    }
}

#[cfg(test)]
mod cursor_overlay_tests {
    use super::{cursor_overlay_rects, cursor_scale, CURSOR_BITMAP};

    #[test]
    fn scale_tracks_resolution() {
        // Env override must be absent for the proportional formula to apply.
        assert!(std::env::var("PHAROS_CURSOR_SCALE").is_err());
        assert_eq!(cursor_scale(1080), 2);
        assert_eq!(cursor_scale(2160), 3);
        // Never sub-pixel, even on a tiny output.
        assert_eq!(cursor_scale(240), 2);
    }

    #[test]
    fn hotspot_is_top_left_and_rects_scale() {
        let scale = 3;
        let (outline, fill) = cursor_overlay_rects((100.0, 50.0), scale);
        // The very first cell (row 0, col 0) is an outline pixel at the hotspot.
        let tip = outline
            .iter()
            .find(|r| r.loc.x == 100 && r.loc.y == 50)
            .expect("outline rect at the hotspot");
        assert_eq!(tip.size.h, scale, "one bitmap row tall");
        assert!(tip.size.w >= scale, "at least one cell wide");
        // Both colours are present, and every rect is a positive-area multiple
        // of the scale (runs merge whole cells).
        assert!(!fill.is_empty() && !outline.is_empty());
        for r in outline.iter().chain(fill.iter()) {
            assert!(r.size.w > 0 && r.size.h == scale);
            assert_eq!(r.size.w % scale, 0);
        }
    }

    #[test]
    fn total_cells_match_bitmap() {
        // Sum of rect widths (in cells) equals the count of non-space glyphs —
        // the rasteriser drops nothing and invents nothing.
        let (outline, fill) = cursor_overlay_rects((0.0, 0.0), 1);
        let drawn: i32 = outline.iter().chain(fill.iter()).map(|r| r.size.w).sum();
        let glyphs = CURSOR_BITMAP
            .iter()
            .map(|l| l.bytes().filter(|&b| b != b' ').count() as i32)
            .sum::<i32>();
        assert_eq!(drawn, glyphs);
    }
}

#[cfg(test)]
mod p1_gpu_error_tests {
    use super::{classify, FaultInject, Severity};
    use nvidia_video_codec_sdk::ErrorKind;

    #[test]
    fn retryable_kinds_are_transient() {
        // The whole point of P1: these three NVIDIA "retry in a few ms / not
        // fatal" codes must NOT crash the host. Pre-P1 a single one terminated
        // the daemon via the blanket `?`.
        for k in [
            ErrorKind::NeedMoreInput,
            ErrorKind::EncoderBusy,
            ErrorKind::LockBusy,
        ] {
            assert_eq!(classify(k), Severity::Transient, "{k:?} must be transient");
        }
    }

    #[test]
    fn device_gone_and_unknown_kinds_are_fatal() {
        for k in [
            ErrorKind::DeviceNotExist,
            ErrorKind::NoEncodeDevice,
            ErrorKind::InvalidDevice,
            ErrorKind::InvalidEncoderDevice,
            ErrorKind::OutOfMemory,
            ErrorKind::EncoderNotInitialized,
            ErrorKind::Generic,
            // Not explicitly listed → conservatively fatal (the safe default).
            ErrorKind::InvalidParam,
            ErrorKind::MapFailed,
        ] {
            assert_eq!(classify(k), Severity::Fatal, "{k:?} must be fatal");
        }
    }

    #[test]
    fn injection_skips_frame_zero_and_fires_on_period() {
        let f = FaultInject {
            kind: ErrorKind::EncoderBusy,
            period: 100,
        };
        assert_eq!(
            f.due(0),
            None,
            "frame 0 stays clean so the opening IDR ships"
        );
        assert_eq!(f.due(50), None);
        assert_eq!(f.due(100), Some(ErrorKind::EncoderBusy));
        assert_eq!(f.due(101), None);
        assert_eq!(f.due(200), Some(ErrorKind::EncoderBusy));
    }

    #[test]
    fn gpu_health_set_and_sticky() {
        use super::{
            current_gpu_health, set_gpu_health, set_throttle, GPU_HEALTH, HEALTH_DEGRADED,
            HEALTH_FAULTED, HEALTH_HEALTHY, THROTTLE,
        };
        use std::sync::atomic::Ordering;
        // The only test that touches the process-global health statics — runs
        // alone on them, so no race with the others.
        GPU_HEALTH.store(HEALTH_HEALTHY, Ordering::Relaxed);
        THROTTLE.store(false, Ordering::Relaxed);
        assert!(matches!(
            current_gpu_health(),
            control_ipc::GpuHealth::Healthy
        ));

        // Throttle is an INDEPENDENT amber signal merged into the wire health.
        set_throttle(true, || "GPU throttling: thermal".to_string());
        match current_gpu_health() {
            control_ipc::GpuHealth::Degraded { throttle, .. } => {
                assert_eq!(throttle.as_deref(), Some("GPU throttling: thermal"))
            }
            other => panic!("expected Degraded(throttle), got {other:?}"),
        }
        set_throttle(false, String::new);
        assert!(matches!(
            current_gpu_health(),
            control_ipc::GpuHealth::Healthy
        ));

        set_gpu_health(HEALTH_DEGRADED, "slow encodes");
        assert!(matches!(
            current_gpu_health(),
            control_ipc::GpuHealth::Degraded { .. }
        ));

        set_gpu_health(HEALTH_FAULTED, "wedge");
        match current_gpu_health() {
            control_ipc::GpuHealth::Faulted { reason, .. } => assert_eq!(reason, "wedge"),
            other => panic!("expected Faulted, got {other:?}"),
        }

        // Faulted is sticky: a later Healthy/Degraded/throttle must NOT downgrade it.
        set_gpu_health(HEALTH_HEALTHY, "");
        assert!(matches!(
            current_gpu_health(),
            control_ipc::GpuHealth::Faulted { .. }
        ));

        GPU_HEALTH.store(HEALTH_HEALTHY, Ordering::Relaxed); // reset for cleanliness
    }

    #[test]
    fn xid_parsing_and_severity() {
        use super::{parse_xid, xid_is_fatal};
        assert_eq!(
            parse_xid("6,1,2,-;NVRM: Xid (PCI:0000:01:00.0): 79, pid=0, ..."),
            Some(79)
        );
        assert_eq!(parse_xid("NVRM: Xid (PCI:0000:0a:00): 13, foo"), Some(13));
        assert_eq!(parse_xid("unrelated kernel message"), None);
        assert!(xid_is_fatal(79) && xid_is_fatal(62) && xid_is_fatal(48) && xid_is_fatal(43));
        assert!(!xid_is_fatal(13) && !xid_is_fatal(31) && !xid_is_fatal(1));
    }
}

#[cfg(test)]
mod resolution_ladder_tests {
    use super::{
        coded_for_height, effective_floor, lever_state, rung_above, rung_below, NET_FLOOR_KBPS,
        RES_LADDER_H,
    };

    #[test]
    fn coded_preserves_aspect_and_clamps_to_native() {
        // 16:9 native, drop to 720p → 1280x720, both even.
        assert_eq!(coded_for_height(1920, 1080, 720), (1280, 720));
        // 0 or >= native ⇒ native (no downscale).
        assert_eq!(coded_for_height(1920, 1080, 0), (1920, 1080));
        assert_eq!(coded_for_height(1920, 1080, 1080), (1920, 1080));
        assert_eq!(coded_for_height(1920, 1080, 4000), (1920, 1080));
        // Portrait native (e.g. the 14T Pro panel) preserves aspect + stays even.
        let (w, h) = coded_for_height(1220, 2712, 1080);
        assert_eq!(h, 1080);
        assert_eq!(w % 2, 0);
        assert!(w < 1220 && (w as i64 - (1220 * 1080 / 2712) as i64).abs() <= 2);
        // Odd target height is even-aligned down.
        assert_eq!(coded_for_height(1280, 721, 541).1 % 2, 0);
    }

    #[test]
    fn rung_below_descends_within_native() {
        // No floor (0): from native 1080p the next rung down is 720p (1440 is not
        // < native).
        assert_eq!(rung_below(1080, 1080, 0), Some(720));
        assert_eq!(rung_below(1080, 720, 0), Some(540));
        // Already at the lowest rung.
        assert_eq!(rung_below(1080, 540, 0), None);
        // A 1440p-native output can use the 1080 rung.
        assert_eq!(rung_below(1440, 1440, 0), Some(1080));
    }

    #[test]
    fn rung_below_respects_the_floor() {
        // Floor 720: from 1080 we may drop to 720, then stop (no 540).
        assert_eq!(rung_below(1080, 1080, 720), Some(720));
        assert_eq!(rung_below(1080, 720, 720), None);
        // A non-rung floor stops at the lowest rung >= floor.
        assert_eq!(rung_below(1080, 1080, 700), Some(720));
        assert_eq!(rung_below(1080, 720, 700), None); // 540 < 700 → blocked
                                                      // Floor at/above native ⇒ never drop.
        assert_eq!(rung_below(1080, 1080, 1080), None);
        // effective_floor clamps the selected min to the ceiling and passes 0 through.
        assert_eq!(effective_floor(1080, 0, 0), 0); // no floor
        assert_eq!(effective_floor(1080, 0, 720), 720); // floor under native
        assert_eq!(effective_floor(1080, 720, 1080), 720); // floor clamped to ceiling
        assert_eq!(effective_floor(1080, 0, 4000), 1080); // floor clamped to native
    }

    #[test]
    fn rung_above_recovers_toward_ceiling_never_past_it() {
        // No ceiling (0) ⇒ recover up to native.
        assert_eq!(rung_above(1080, 540, 0), Some(720));
        assert_eq!(rung_above(1080, 720, 0), Some(1080)); // native is a valid rung
        assert_eq!(rung_above(1080, 1080, 0), None); // already native
                                                     // Ceiling caps the climb: from 540 with a 720 ceiling, stop at 720.
        assert_eq!(rung_above(1080, 540, 720), Some(720));
        assert_eq!(rung_above(1080, 720, 720), None); // at the ceiling
                                                      // A NON-rung ceiling is reachable exactly (the cap is a climb candidate),
                                                      // so the ladder can't strand one rung below it. From 720 → 900 directly;
                                                      // from 540 it still steps one rung at a time (→720).
        assert_eq!(rung_above(1080, 720, 900), Some(900));
        assert_eq!(rung_above(1080, 540, 900), Some(720));
        assert_eq!(rung_above(1080, 900, 900), None); // at the non-rung ceiling
    }

    #[test]
    fn lever_state_requires_real_headroom() {
        // Degenerate explicit-RC config: cap at/below the 2 Mbps floor must NOT
        // read as exhausted (else the ladder latches low on a healthy link).
        assert_eq!(
            lever_state(Some((1500, 2000)), 1500, NET_FLOOR_KBPS, 60.0, 24.0, 60.0),
            (false, true)
        );
        assert_eq!(
            lever_state(Some((2000, 3000)), 2000, NET_FLOOR_KBPS, 60.0, 24.0, 60.0),
            (false, true)
        );
        // Normal cap with headroom: at the floor ⇒ exhausted; at the cap ⇒ recovered.
        assert_eq!(
            lever_state(Some((20000, 30000)), 2000, NET_FLOOR_KBPS, 60.0, 24.0, 60.0),
            (true, false)
        );
        assert_eq!(
            lever_state(
                Some((20000, 30000)),
                20000,
                NET_FLOOR_KBPS,
                60.0,
                24.0,
                60.0
            ),
            (false, true)
        );
        // A client-raised floor lifts the exhausted threshold: at 8 Mbps with a
        // 10 Mbps client floor under a 20 Mbps cap ⇒ exhausted (already at the floor).
        assert_eq!(
            lever_state(Some((20000, 30000)), 8000, 10000, 60.0, 24.0, 60.0),
            (true, false)
        );
        // Preset RC (no caps): fps lever. Pinned at the floor with headroom ⇒ exhausted.
        assert_eq!(
            lever_state(None, 0, NET_FLOOR_KBPS, 24.0, 24.0, 60.0),
            (true, false)
        );
        // Degenerate fps (cap == floor): never exhausted.
        assert_eq!(
            lever_state(None, 0, NET_FLOOR_KBPS, 24.0, 24.0, 24.0),
            (false, true)
        );
    }

    #[test]
    fn ladder_is_descending() {
        assert!(RES_LADDER_H.windows(2).all(|w| w[0] > w[1]));
    }
}
