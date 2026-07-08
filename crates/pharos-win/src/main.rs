// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! `pharos-host` — the Pharos streaming host for Windows (BATTLE_PLAN.md M3).
//!
//! Pipeline: capture a display → HEVC (Quick Sync / NVENC via an ffmpeg
//! subprocess) → `transport::QuicStreamer` (the same QUIC server the Linux
//! `compositor` uses) → Trireme. The input back-channel (`take_input_rx`) is
//! drained into Win32 `SendInput`. Pairing + identity are the shared
//! `host_core::pair` code, so an unmodified Trireme connects to either host OS.
//!
//! Bring-up (Phase 1) captures an existing monitor by `output_idx`; the headless
//! IddCx virtual display (Phase 2) just changes which monitor we point at.

// Pure byte logic — also compiled under `test` so its unit tests run on Linux.
#[cfg(any(windows, test))]
mod au;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod encode;
#[cfg(windows)]
mod input;
// Pure XUSB report mapping — also compiled under `test` so its unit tests run
// on the Linux dev box (the ViGEm plumbing in `input` stays Windows-only).
#[cfg(any(windows, test))]
mod pads;
#[cfg(windows)]
mod wgc;
#[cfg(windows)]
mod zerocopy;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::net::SocketAddr;

    init_tracing();
    // Physical pixels everywhere (issue #8): without this the process is
    // DPI-virtualized and every GDI/user32 monitor query lies by the scale
    // factor (a 3840x2160 desktop at 300% reads 1280x720), poisoning the WGC
    // monitor list and the pre-connect geometry announce. WGC's item.Size()
    // and Desktop Duplication frames were always physical; this makes the
    // rest of the process agree with them. Input is unaffected (the absolute
    // path maps normalized 0..1 to SendInput's 0..65535, no pixel metrics).
    // Must run before any monitor API; a failure (unlikely on Win10 1703+)
    // just leaves the old virtualized behavior, so log and continue.
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            tracing::warn!(
                "SetProcessDpiAwarenessContext failed: {e} (monitor sizes may be DPI-scaled)"
            );
        }
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // `pharos-host pair [addr]` — run the pairing listener (shared crate).
        Some("pair") => {
            let addr: SocketAddr = match args.get(2) {
                Some(a) => a.parse()?,
                None => (std::net::Ipv4Addr::UNSPECIFIED, core_proto::ports::PAIR).into(),
            };
            host_core::pair::run(addr)
        }
        // `pharos-host stream [bind] [output_idx] [fps] [qsv|nvenc|amf] [dda|wgc|wgc-zc]`
        // Encoder defaults to GPU-vendor auto-detection when not given. Bare
        // `pharos-host` (no subcommand) also lands here with all defaults (below).
        Some("stream") => run_stream_args(&args),
        // No subcommand at all → run the streaming host with defaults, exactly
        // like `pharos-host stream` with no args (issue #17). This makes a
        // double-clicked / bare-invoked host a live capture+stream that surfaces
        // its PIN, rather than a usage dump. An UNRECOGNIZED subcommand still
        // prints usage (the `_ =>` arm below).
        None => run_stream_args(&args),
        // `pharos-host wgc-test [monitor_idx] [frames] [out.bgra]` — capture via
        // Windows.Graphics.Capture and dump the last frame (validates WGC alone).
        Some("wgc-test") => {
            let idx: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let frames: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120);
            let out = args.get(4).map(String::as_str).unwrap_or("wgc-frame.bgra");
            run_wgc_test(idx, frames, out)
        }
        // `pharos-host pad-test [seconds]` — plug a ViGEm virtual Xbox 360 pad
        // and drive a visible pattern (validates issue #2 with no client).
        Some("pad-test") => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            run_pad_test(secs)
        }
        // `pharos-host nvenc-test [w] [h] [frames] [out.hevc]` — encode a test
        // pattern through the in-process D3D11 NVENC session (issue #12
        // bring-up: driver loading, session open, texture registration).
        Some("nvenc-test") => {
            let w: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1920);
            let h: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1080);
            let frames: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(120);
            let out = args.get(5).map(String::as_str).unwrap_or("nvenc-test.hevc");
            zerocopy::encode_test_pattern(w, h, 60, frames, out)
        }
        // `pharos-host nvenc-test-hdr [w] [h] [frames] [out.hevc]` — the full
        // HDR leg with no HDR display: fp16 scRGB ramp → PQ shader → Main10 +
        // SEI (issue #4 bring-up; the compositor's nvenc-test-hdr sibling).
        Some("nvenc-test-hdr") => {
            let w: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1920);
            let h: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1080);
            let frames: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(120);
            let out = args
                .get(5)
                .map(String::as_str)
                .unwrap_or("nvenc-test-hdr.hevc");
            zerocopy::encode_test_pattern_hdr(w, h, 60, frames, out)
        }
        _ => {
            eprintln!(
                "Pharos host (Windows)\n\
                 usage:\n  \
                 pharos-host          (no subcommand: run `stream` with defaults)\n  \
                 pharos-host pair     [addr=0.0.0.0:4432]\n  \
                 pharos-host stream   [bind=0.0.0.0:15777] [output_idx=0] [fps=60] [qsv|nvenc|amf] [dda|wgc|wgc-zc]\n  \
                 pharos-host wgc-test [monitor_idx=0] [frames=120] [out.bgra]\n  \
                 pharos-host pad-test [seconds=10]\n  \
                 pharos-host nvenc-test [w=1920] [h=1080] [frames=120] [out.hevc]\n  \
                 pharos-host nvenc-test-hdr [w=1920] [h=1080] [frames=120] [out.hevc]"
            );
            Ok(())
        }
    }
}

/// Parse the `stream` argument vector and run the host. Shared by the explicit
/// `pharos-host stream …` subcommand and the bare `pharos-host` default run mode
/// (issue #17), so both honour the same defaults: the unified stream port, the
/// first output, 60 fps, GPU-vendor-auto encoder, and ddagrab capture. `args` is
/// the whole `std::env::args()` vector; positional args start at index 2 (index
/// 1 is the subcommand or absent).
#[cfg(windows)]
fn run_stream_args(args: &[String]) -> anyhow::Result<()> {
    let bind = match args.get(2) {
        Some(s) => parse_bind(s)?,
        // Unified canonical stream port (issue #17): was WIN_STREAM (4435).
        None => (std::net::Ipv4Addr::UNSPECIFIED, core_proto::ports::STREAM).into(),
    };
    let output_idx: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let fps: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let encoder = match args.get(5).map(String::as_str) {
        Some("nvenc") => encode::Encoder::Nvenc,
        Some("amf") => encode::Encoder::Amf,
        Some("qsv") => encode::Encoder::Qsv,
        _ => detect_default_encoder(),
    };
    // Capture backend: ddagrab (DXGI Desktop Duplication, self-pacing) by
    // default; `wgc` uses Windows.Graphics.Capture (captures IddCx virtual
    // displays + by HMONITOR regardless of adapter — needed on the headless
    // AMD rig where ddagrab can't reach the virtual display's adapter).
    let capture = match args.get(6).map(String::as_str) {
        Some("wgc") => CaptureBackend::Wgc,
        Some("wgc-zc") => CaptureBackend::WgcZc,
        _ => CaptureBackend::Dda,
    };
    // The zero-copy path IS the NVENC SDK; the qsv/amf ffmpeg encoders
    // have no in-process equivalent here.
    if matches!(capture, CaptureBackend::WgcZc) && !matches!(encoder, encode::Encoder::Nvenc) {
        anyhow::bail!("wgc-zc requires the nvenc encoder");
    }
    run_stream(bind, output_idx, fps, encoder, capture)
}

/// Which capture source feeds the encoder.
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
enum CaptureBackend {
    /// ffmpeg `ddagrab` (DXGI Desktop Duplication) — self-pacing, captures the
    /// default adapter's outputs.
    Dda,
    /// Windows.Graphics.Capture in-process — captures by `HMONITOR` (any adapter,
    /// incl. IddCx virtual displays); change-driven, so the host paces it.
    Wgc,
    /// WGC capture + in-process NVENC on the same D3D11 device (issue #12):
    /// no ffmpeg subprocess, no CPU frame copies. nvenc-only.
    WgcZc,
}

/// Accept either a bare `host:port` or a `quic://host:port` bind string (the
/// Linux `capture-zc` uses the latter form), returning the bind `SocketAddr`.
#[cfg(windows)]
fn parse_bind(s: &str) -> anyhow::Result<std::net::SocketAddr> {
    let s = s.strip_prefix("quic://").unwrap_or(s);
    s.parse()
        .map_err(|e| anyhow::anyhow!("bad bind address {s:?}: {e}"))
}

/// Encoder default when the CLI doesn't name one: match the machine's GPU
/// vendor instead of assuming Quick Sync. The old hard Qsv default
/// black-screened NVIDIA-only rigs — no MFX means the encoder faults five
/// times and parks, while audio/input keep working — and the panel launches
/// `pharos-host stream` bare, so it hit exactly that. Explicit args still win.
#[cfg(windows)]
fn detect_default_encoder() -> encode::Encoder {
    let (enc, why) = match display::preferred_encoder_vendor() {
        Some(display::VENDOR_NVIDIA) => (encode::Encoder::Nvenc, "NVIDIA adapter"),
        Some(display::VENDOR_AMD) => (encode::Encoder::Amf, "AMD adapter"),
        Some(display::VENDOR_INTEL) => (encode::Encoder::Qsv, "Intel adapter"),
        _ => (encode::Encoder::Qsv, "no known GPU vendor; falling back"),
    };
    tracing::info!("encoder auto-detect: {enc:?} ({why})");
    enc
}

/// Start the optional MQTT/Home-Assistant integration **only if enabled in
/// `config.toml`** (off by default — no broker connection, no thread, no network
/// chatter otherwise). Publishes availability + a Phase-1 telemetry set via HA
/// discovery and returns the handle (kept alive for the session). Any failure is
/// non-fatal — streaming continues without it.
#[cfg(windows)]
fn start_mqtt(
    encoder: encode::Encoder,
    capture: CaptureBackend,
    fps: u32,
) -> Option<integration_mqtt::Mqtt> {
    use integration_mqtt::{host, Config, Device, EntitySpec, Mqtt};

    let cfg = host_core::config::load().mqtt;
    if !cfg.enabled {
        return None; // opt-in; default is fully off
    }
    let node = cfg.node_id.clone().unwrap_or_else(host::hostname);
    let hostname = host::hostname();
    let device = Device {
        identifier: node.clone(),
        name: format!("Pharos ({hostname})"),
        manufacturer: "Pharos".into(),
        model: "Pharos host".into(),
        sw_version: env!("CARGO_PKG_VERSION").into(),
    };
    let mqtt = match Mqtt::connect(
        Config {
            host: cfg.host.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            client_id: format!("pharos-{node}"),
            discovery_prefix: cfg.discovery_prefix.clone(),
            base_topic: format!("pharos/{node}"),
            node_id: node.clone(),
        },
        device,
    ) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("MQTT disabled: connect failed: {e:#}");
            return None;
        }
    };

    let enc_s = match encoder {
        encode::Encoder::Qsv => "hevc_qsv",
        encode::Encoder::Nvenc => "hevc_nvenc",
        encode::Encoder::Amf => "hevc_amf",
    };
    let cap_s = match capture {
        CaptureBackend::Dda => "ddagrab",
        CaptureBackend::Wgc => "wgc",
        CaptureBackend::WgcZc => "wgc-zc",
    };
    let announce = |s: EntitySpec| {
        if let Err(e) = mqtt.announce(&s) {
            tracing::warn!("MQTT announce {}: {e}", s.object_id);
        }
    };
    announce(EntitySpec::sensor("state", "State").with_icon("mdi:monitor-share"));
    announce(
        EntitySpec::binary_sensor("client_connected", "Client connected")
            .with_device_class("connectivity"),
    );
    announce(EntitySpec::sensor("encoder", "Encoder").diagnostic());
    announce(EntitySpec::sensor("capture", "Capture backend").diagnostic());
    announce(
        EntitySpec::sensor("target_fps", "Target FPS")
            .with_unit("fps")
            .diagnostic(),
    );
    announce(EntitySpec::sensor("hostname", "Hostname").diagnostic());

    mqtt.publish_state("state", "idle");
    mqtt.publish_state("client_connected", "OFF");
    mqtt.publish_state("encoder", enc_s);
    mqtt.publish_state("capture", cap_s);
    mqtt.publish_state("target_fps", fps.to_string());
    mqtt.publish_state("hostname", hostname);
    // Advertise every NIC's MAC (mac, mac_2, …). A multi-NIC host exposes all of
    // them so HA WoL or a MAC-pinned bind can target any interface, not just the
    // first one.
    for (i, mac) in host::advertised_macs().into_iter().enumerate() {
        let obj = if i == 0 {
            "mac".to_string()
        } else {
            format!("mac_{}", i + 1)
        };
        let name = if i == 0 {
            "MAC address".to_string()
        } else {
            format!("MAC address {}", i + 1)
        };
        announce(
            EntitySpec::sensor(obj.clone(), name)
                .diagnostic()
                .with_icon("mdi:network"),
        );
        mqtt.publish_state(&obj, mac);
    }
    tracing::info!(
        "MQTT integration enabled (broker {}:{})",
        cfg.host,
        cfg.port
    );
    Some(mqtt)
}

/// The host streaming loop: start the QUIC server, gate on a connected client,
/// then pump ffmpeg's HEVC output (split into access units) onto the stream
/// while a side thread injects the input back-channel.
/// Build the [`control_ipc::HostStatus`] this Windows host publishes to the
/// panel. Pure (no I/O) so the field mapping type-checks and unit-tests on the
/// Linux dev box even though [`run_stream`] is Windows-only — only the thin
/// wiring that feeds it needs Windows. The single-streamer Windows model fills
/// `clients` with at most one entry and leaves the multi-output `outputs` empty.
/// Pack/unpack a `(w, h)` pixel size into one `u64`, `(w<<32)|h`; the same
/// packing `transport::QuicStreamer` uses for its latched geometry. Lets the
/// ~4 Hz status thread share the capture size with the supervisor loop through
/// a single `AtomicU64`.
#[cfg(any(windows, test))]
fn pack_wh(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

#[cfg(any(windows, test))]
fn unpack_wh(g: u64) -> (u32, u32) {
    ((g >> 32) as u32, g as u32)
}

/// One evaluation step of the bufferbloat-aware adaptive-bitrate controller (issue
/// #30), factored pure for unit tests. AIMD with a TCP-style **ssthresh brake** so the
/// up-ramp stops blindly chasing the ceiling — the flaw that made the first cut
/// sawtooth ~7 Mbps over a WiFi link's capacity. `congested` folds three signals the
/// caller computes: delivered-fps deficit, packet loss, and — critically — an RTT
/// bufferbloat spike (latency growth the other two miss). On congestion: mark
/// `ssthresh` at the current rate and multiplicatively decrease (harder when the
/// deficit is worse). When clean for ≥3 windows: fast-ramp back toward `ssthresh`
/// (never overshooting it), then probe ABOVE it additively, raising the mark as it
/// safely climbs. Returns `(allowed_kbps, ssthresh_kbps)`; the caller applies the >8%
/// actuator debounce before pushing `allowed` to the encoder.
#[cfg(any(windows, test))]
#[allow(clippy::too_many_arguments)]
fn adapt_bitrate_step(
    allowed_kbps: u32,
    ssthresh_kbps: u32,
    floor_kbps: u32,
    ceiling_kbps: u32,
    congested: bool,
    deficit: f32,
    clear_ticks: u32,
    probe_step_kbps: u32,
) -> (u32, u32) {
    let floor = floor_kbps.min(ceiling_kbps);
    if congested {
        // React on the FIRST congested window (latency protection needs speed). Brake at
        // ~85% of where we congested and drop at least that far (harder when the deficit
        // is worse), so recovery hovers BELOW capacity instead of sawtoothing above it —
        // the fix for the ceiling-chasing oscillation.
        let beta = deficit.clamp(0.5, 0.85);
        let ssthresh = ((allowed_kbps as f32 * 0.85) as u32).max(floor);
        let allowed = ((allowed_kbps as f32 * beta) as u32).max(floor);
        (allowed, ssthresh.max(allowed))
    } else if clear_ticks >= 3 {
        if allowed_kbps < ssthresh_kbps {
            // Fast recovery toward the last-known-good rate — never overshoot the mark.
            let allowed = ((allowed_kbps as f32 * 1.2) as u32)
                .min(ssthresh_kbps)
                .clamp(floor, ceiling_kbps);
            (allowed, ssthresh_kbps)
        } else {
            // At/above the mark: probe up slowly (additive), raising the mark as we go.
            let allowed = allowed_kbps
                .saturating_add(probe_step_kbps)
                .clamp(floor, ceiling_kbps);
            (allowed, allowed.max(ssthresh_kbps))
        }
    } else {
        (allowed_kbps.clamp(floor, ceiling_kbps), ssthresh_kbps)
    }
}

#[cfg(any(windows, test))]
#[allow(clippy::too_many_arguments)]
fn build_status(
    hostname: &str,
    encoder_label: &str,
    codec_label: &str,
    fps: u32,
    connected: bool,
    client_addr: Option<String>,
    client_pubkey: Option<String>,
    frames_sent: u64,
    coded: (u32, u32),
    hdr_active: Option<String>,
    gpu_health: control_ipc::GpuHealth,
) -> control_ipc::HostStatus {
    let clients = if connected {
        vec![control_ipc::ClientStatus {
            addr: client_addr,
            pubkey: client_pubkey,
            connected: true,
            encoder: encoder_label.to_string(),
            codec: codec_label.to_string(),
            fps,
            frames_sent,
            // The captured monitor's pixel size: the same value announced to
            // the client as DG_STREAM_GEOMETRY, mirrored here for the panel;
            // (0, 0) = unknown. The resolution LADDER stays Linux/NVENC-only,
            // so ceiling/floor remain 0.
            coded_w: coded.0,
            coded_h: coded.1,
            res_ceiling_h: 0,
            res_floor_h: 0,
        }]
    } else {
        Vec::new()
    };
    control_ipc::HostStatus {
        hostname: hostname.to_string(),
        streaming: connected,
        clients,
        outputs: Vec::new(),
        media: None,
        pairing: None,
        controllers: Vec::new(),
        video_codec: Some(codec_label.to_string()),
        // The Windows host serves pairing via control_ipc::serve_control, so it
        // is not read-only; kept false for panels that predate the caps set.
        read_only: false,
        // The per-verb capability set (issue #22): exactly what this host's
        // serve_control closure handles — pairing (#17), the peer-fleet verbs
        // (#19), and app launch (#22). Everything else (preview, relaunch,
        // stream-mode, input gating, controller/frame-timings planes) still
        // replies Event::Error, and the panel now disables those controls with
        // an honest reason instead of offering a click that bounces.
        caps: control_ipc::HostCaps {
            pairing: true,
            peers: true,
            launch_app: true,
            ..control_ipc::HostCaps::none()
        },
        // P6: the run_stream supervisor loop drives this (Healthy while streaming,
        // Degraded during a respawn/backoff, Faulted when the breaker trips).
        // throttle + last_xid are always None on Windows (no NVML/Xid plane).
        gpu_health,
        // Single-client only — no mirror fan-out on the Windows path.
        stream_mode: control_ipc::StreamMode::Single,
        // Some("HEVC Main10 BT.2020/PQ") when the wgc-zc pipeline runs its
        // HDR10 session (issue #4); the ffmpeg paths stream SDR and pass None.
        hdr_active,
        // The live primary-client policy (issue #19): read from the transport
        // global (seeded at startup, updated by SetPrimaryPeer/ForgetPeer) so
        // the panel's star reflects what this host is actually enforcing.
        primary_pubkey: transport::primary_peer_hex(),
    }
}

/// `(encoder label, codec label)` for the published status. pharos-win streams
/// HEVC on all three encoders.
#[cfg(windows)]
fn encoder_labels(e: encode::Encoder) -> (&'static str, &'static str) {
    match e {
        encode::Encoder::Qsv => ("Quick Sync HEVC", "HEVC"),
        encode::Encoder::Nvenc => ("NVENC HEVC", "HEVC"),
        encode::Encoder::Amf => ("AMF HEVC", "HEVC"),
    }
}

#[cfg(windows)]
fn run_stream(
    bind: std::net::SocketAddr,
    output_idx: u32,
    fps: u32,
    encoder: encode::Encoder,
    capture: CaptureBackend,
) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tracing::{info, warn};

    // MAC-pinned bind (multi-NIC hosts): if config names a `bind_mac`, listen on
    // that NIC's IP instead of whatever the CLI passed (usually the wildcard) —
    // deterministic path selection on a dual/triple/quad-homed host. Falls back
    // to the CLI bind (and warns) if the MAC isn't found.
    let mut bind = bind;
    if let Some(mac) = host_core::config::load().network.bind_mac {
        match integration_mqtt::host::ip_for_mac(&mac) {
            Some(ip) => {
                info!("bind_mac {mac} → {ip}; pinning listener to that NIC");
                bind = std::net::SocketAddr::new(ip, bind.port());
            }
            None => warn!("bind_mac {mac} matches no NIC; binding {bind} as given"),
        }
    }

    // Long-term identity — same file/location the Linux host uses, so a client
    // that paired with this box verifies the video-auth signature.
    let id_path = host_core::pair::identity_path()?;
    let identity = Arc::new(
        core_crypto::Identity::load_or_create(&id_path)
            .map_err(|e| anyhow::anyhow!("identity at {}: {e}", id_path.display()))?,
    );
    info!(
        "Pharos identity {} (saved at {})",
        identity.public_hex(),
        id_path.display()
    );

    // Seed the live pre-emption policy from the persisted primary_pubkey
    // (issue #19) before the streamer accepts a client; the serve_control
    // handlers (SetPrimaryPeer / ForgetPeer) keep it in sync from here on.
    host_core::pair::sync_primary_to_transport();

    // Enforce pairing by default, exactly like the Linux compositor: only
    // clients whose verified Ed25519 pubkey is in `peers.toml` may stream
    // (`PHAROS_ALLOW_UNPAIRED=1` opts out). This is the shared cross-platform
    // authorizer — the one place the M2 auth work wasn't yet applied on Windows.
    let streamer = Arc::new(transport::QuicStreamer::start_with_authorizer(
        bind,
        // Clone the Arc: `identity` is reused below for the mDNS advertise.
        identity.clone(),
        host_core::pair::client_authorizer(),
        // The Windows host is single-client only — a never-set mirror flag.
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        // `[network] ipv6` — bind dual-stack when enabled (shared cross-platform config).
        host_core::config::load().network.ipv6,
    )?);
    info!("QUIC video host on {bind}; waiting for Trireme to connect…");

    // The client sizes its NV12 frame reader from the DG_STREAM_GEOMETRY
    // announce; without one, trireme assumes 1920x1080 and mis-renders any
    // other size as stride corruption (issue #1). Latch the captured monitor's
    // size as soon as the streamer exists so the connect-time announce already
    // carries the truth; the supervisor loop re-latches on every pipeline
    // (re)build. `coded` mirrors the value into HostStatus for the panel.
    let coded = Arc::new(AtomicU64::new(0));
    // Some(label) while a generation streams HDR10 (wgc-zc only); mirrored
    // into HostStatus so the panel's badge follows the live session.
    let hdr_label: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    if let Some((w, h)) = capture_size(capture, output_idx) {
        streamer.set_geometry(w, h);
        coded.store(pack_wh(w, h), Ordering::Relaxed);
        info!("capture geometry {w}x{h} (output_idx {output_idx}); announcing to clients");
    }
    // Colorimetry announce (issues #16, #28): leave it UNDETERMINED until the first
    // pipeline build determines the real value (wgc-zc samples the monitor's HDR state
    // at build; the ffmpeg path announces SDR at build). The transport now holds a
    // tri-state and sends no DG_STREAM_HDR until then, so a client that connects before
    // the first pipeline plain-decodes for the sub-second gap instead of being told a
    // premature SDR it must later correct — the connect-time gray wash. (Was an eager
    // `streamer.set_hdr(false)` here, which caused exactly that flash.)

    // Cursor-mode poll: the foreground game's own cursor-capture state, announced to
    // the client as DG_CURSOR_MODE so the desktop cursor mirrors it (auto-locks to
    // relative mouselook during gameplay, frees to absolute in menus; no manual
    // toggle). A windowed FPS that grabs the mouse HIDES the system cursor, so a
    // CURSORINFO whose flags lack CURSOR_SHOWING (flags.0 == 0) means CAPTURED; the
    // cursor showing means FREE. GetCursorInfo is process-global + cheap, so a small
    // dedicated ~60 Hz thread is fine (folding it into the capture loop would gate it
    // on the encode cadence). Initialised to FREE; on a GetCursorInfo failure we leave
    // the last state (don't thrash). Only changes matter — the announce layer bursts
    // on change and steadies ~1×/s.
    streamer.set_cursor_captured(false);
    {
        let streamer_cursor = streamer.clone();
        std::thread::Builder::new()
            .name("cursor-mode-poll".into())
            .spawn(move || {
                use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO};
                loop {
                    let mut ci = CURSORINFO {
                        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                        ..Default::default()
                    };
                    // SAFETY: `ci` is a live, correctly-sized CURSORINFO; GetCursorInfo
                    // only writes into it. On failure we skip the store (leave the last
                    // announced state) so a transient error can't flap the client grab.
                    if unsafe { GetCursorInfo(&mut ci) }.is_ok() {
                        // `flags` lacking CURSOR_SHOWING (i.e. flags.0 == 0, the cursor
                        // is hidden) means the game captured/hid the mouse => CAPTURED
                        // / relative. CURSOR_SHOWING set (flags.0 != 0) => FREE /
                        // absolute. CURSORINFO_FLAGS is a newtype with no BitAnd/PartialEq
                        // in this `windows` build, so test the raw bit directly.
                        let captured = ci.flags.0 == 0;
                        streamer_cursor.set_cursor_captured(captured);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(16));
                }
            })?;
    }

    // Optional MQTT / Home-Assistant telemetry — off unless enabled in config.toml.
    let mqtt = start_mqtt(encoder, capture, fps);

    // Input back-channel → SendInput, on its own thread. `try_recv` works
    // outside a tokio runtime; sleep briefly when the queue is empty.
    // Telemetry: the first few events verbatim plus a periodic rate line, so a
    // silent input path (client not sending vs host not injecting) is
    // diagnosable from the host log alone.
    if let Some(mut rx) = streamer.take_input_rx() {
        let streamer_input = streamer.clone();
        std::thread::Builder::new()
            .name("input-inject".into())
            .spawn(move || {
                let mut injector = input::Injector::new();
                let mut seen: u64 = 0;
                let mut window: u64 = 0;
                let mut last_rate = Instant::now();
                let mut last_conn_check = Instant::now();
                let mut last_conn: Option<String> = None;
                // Backstop for a DEPARTED client whose stale input replays in bulk
                // (issue #13): the client fix bounds a same-client stall, but a
                // client that queued and then dropped can still dump a batch. If a
                // single drain pass yields more than this many events it's not a
                // human's input rate; treat it as a stall-replay and purge the rest
                // + release_all, matching the client-change intent.
                const INPUT_BACKLOG_BURST: u64 = 256;
                loop {
                    // Drain in a burst so we can spot a stall-replay: a healthy
                    // human tops out at a few events per millisecond, so a pass
                    // that keeps yielding past the burst cap is a queued dump.
                    let mut this_pass: u64 = 0;
                    while let Ok(ev) = rx.try_recv() {
                        seen += 1;
                        window += 1;
                        this_pass += 1;
                        if seen <= 20 {
                            tracing::info!("input[{seen}]: {ev:?}");
                        }
                        injector.dispatch(&ev);
                        if this_pass >= INPUT_BACKLOG_BURST {
                            // Sustained backlog: stop injecting the dump, purge the
                            // remainder, and release held state so no stale
                            // key/button/pad is left down.
                            let mut purged = 0u64;
                            while rx.try_recv().is_ok() {
                                purged += 1;
                            }
                            injector.release_all();
                            tracing::warn!(
                                "input: backlog burst ({this_pass} injected, {purged} \
                                 purged); released all held state (issue #13)"
                            );
                            break;
                        }
                    }
                    if this_pass == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Client change (disconnect, or preemption by a new one),
                    // checked ~4 Hz by peer addr: purge whatever the departed
                    // client still has queued (stale input must not replay into
                    // the next session, issue #13) and release everything it
                    // held down (its key-ups can never arrive now; a lost
                    // release is a stuck modifier in the host session).
                    if last_conn_check.elapsed() >= Duration::from_millis(250) {
                        last_conn_check = Instant::now();
                        let conn = streamer_input.connected_addr();
                        if conn != last_conn {
                            let mut purged = 0u64;
                            while rx.try_recv().is_ok() {
                                purged += 1;
                            }
                            if purged > 0 {
                                tracing::info!(
                                    "input: purged {purged} queued events on client change"
                                );
                            }
                            injector.release_all();
                            last_conn = conn;
                        }
                    }
                    if last_rate.elapsed() >= Duration::from_secs(5) {
                        if window > 0 {
                            tracing::info!("input: {window} events in last 5s ({seen} total)");
                        }
                        last_rate = Instant::now();
                        window = 0;
                    }
                }
            })?;
    }

    // Audio: WASAPI loopback → Opus → QUIC datagrams, on its own thread, gated on
    // a connected client (costs nothing pre-connect, resumes across reconnects).
    // Non-fatal — video keeps streaming if audio capture fails (same as the Linux
    // compositor path).
    {
        let audio = streamer.audio_handle();
        std::thread::Builder::new()
            .name("pharos-audio".into())
            .spawn(move || {
                let mut sent = 0u64;
                let r =
                    media_audio::capture_opus(None, media_audio::DEFAULT_BITRATE, None, |pkt| {
                        if audio.is_connected() {
                            audio.send(pkt);
                            sent += 1;
                            if sent.is_multiple_of(100) {
                                info!("audio: {sent} Opus packets sent (~{}s)", sent / 50);
                            }
                        }
                    });
                if let Err(e) = r {
                    warn!("audio capture stopped: {e:#} (streaming video only)");
                }
            })?;
    }

    // ── Control plane ──────────────────────────────────────────────────────
    // Publish a HostStatus over TCP so the panel (on the Linux box) can see this
    // Windows host's live state — connected client, encoder, fps, frames sent.
    // Render-loop-free (control_ipc::serve_status); brought up BEFORE the
    // connect-wait so an idle host is already panel-visible. Non-fatal: a bind
    // failure logs and streaming continues.
    let hostname = host_info::hostname();
    let (encoder_label, codec_label) = encoder_labels(encoder);
    let frames = Arc::new(AtomicU64::new(0));
    // P6 GPU-health plane: the supervisor loop below is the SOLE writer; the ~4Hz
    // status thread reads it into HostStatus.gpu_health.
    let gpu_health = Arc::new(Mutex::new(control_ipc::GpuHealth::Healthy));
    // Software fault-injection seam (the Windows analogue of the Linux compositor's
    // PHAROS_FAULT_INJECT): `device_lost` = one respawn cycle in generation 0 then
    // recover; `device_lost:P` = a synthetic fault every P frames in every
    // generation → trips the circuit breaker → sticky Faulted.
    let inject = std::env::var("PHAROS_WIN_FAULT_INJECT")
        .ok()
        .and_then(|s| parse_win_fault_inject(&s));
    if let Some(spec) = &inject {
        warn!("⚠ FAULT INJECTION ARMED {spec:?} (PHAROS_WIN_FAULT_INJECT) — test build behavior");
    }
    let status = Arc::new(Mutex::new(build_status(
        &hostname,
        encoder_label,
        codec_label,
        fps,
        streamer.is_connected(),
        streamer.connected_addr(),
        streamer.connected_pubkey(),
        0,
        unpack_wh(coded.load(Ordering::Relaxed)),
        None,
        control_ipc::GpuHealth::Healthy,
    )));
    {
        // Refresh the published status ~4 Hz from the live streamer + frame
        // counter (the Windows analogue of the Linux render loop's status push).
        let (status_r, streamer_r, frames_r, hostname_r, health_r, coded_r, hdr_r) = (
            status.clone(),
            streamer.clone(),
            frames.clone(),
            hostname.clone(),
            gpu_health.clone(),
            coded.clone(),
            hdr_label.clone(),
        );
        std::thread::Builder::new()
            .name("control-status".into())
            .spawn(move || loop {
                let mut s = build_status(
                    &hostname_r,
                    encoder_label,
                    codec_label,
                    fps,
                    streamer_r.is_connected(),
                    streamer_r.connected_addr(),
                    streamer_r.connected_pubkey(),
                    frames_r.load(Ordering::Relaxed),
                    unpack_wh(coded_r.load(Ordering::Relaxed)),
                    hdr_r.lock().ok().and_then(|g| g.clone()),
                    health_r.lock().map(|g| g.clone()).unwrap_or_default(),
                );
                if let Ok(mut g) = status_r.lock() {
                    // Preserve the pairing field: the PairingManager is its sole
                    // writer, and this ~4 Hz rebuild (build_status sets it None)
                    // would otherwise clobber the live PIN every 250ms.
                    s.pairing = g.pairing.take();
                    *g = s;
                }
                std::thread::sleep(Duration::from_millis(250));
            })?;
    }
    // Pairing driven from the panel (issue #17): the same shared PairingManager
    // the Linux compositor uses, mirroring the live PIN/outcome into `status`
    // (the ~4 Hz refresh above preserves that field). This is what makes the
    // panel's Pair button work on Windows instead of the raw `pharos-host pair`
    // CLI. The panel StartPairing path still opens on-demand windows off any
    // render loop; the always-open listener below covers the headless operator.
    // announce_mdns = false: this host advertises process-wide below (`_mdns`);
    // a second registration would conflict-rename to "… (2)" on clients.
    let pairing = host_core::pairing::PairingManager::new(status.clone(), false);
    // Always-open, host-lifetime pairing (issue #17): bring up a PIN-bearing
    // listener at startup so a fresh Trireme can pair with no panel and no
    // separate `pharos-host pair` terminal. It keeps accepting devices (rotating
    // the PIN) until the process exits or the brute-force lockout trips.
    match pairing.start_host_lifetime(None) {
        Ok(()) => {
            info!("always-open pairing listener up (issue #17)");
            // Surface the PIN + discovery facts to a headless operator who has
            // only this console (the panel/tray read it from HostStatus.pairing).
            // The first PIN is minted synchronously by start_host_lifetime, so
            // it's already in `status` here; the listener rotates it after each
            // device (see the panel/tray for the live value).
            let pin = status
                .lock()
                .ok()
                .and_then(|s| s.pairing.as_ref().and_then(|p| p.pin.clone()));
            match pin {
                Some(pin) => info!(
                    "pairing PIN: {pin} — enter on Trireme; discoverable on {} as this host; stream port {}",
                    host_core::pair::MDNS_SERVICE_TYPE,
                    bind.port()
                ),
                None => info!(
                    "pairing open; discoverable on {}; stream port {}",
                    host_core::pair::MDNS_SERVICE_TYPE,
                    bind.port()
                ),
            }
        }
        Err(e) => warn!("always-open pairing listener not started: {e}"),
    }
    let ctrl_addr = std::net::SocketAddr::from(([0, 0, 0, 0], core_proto::ports::CONTROL));
    match control_ipc::tcp_bind(ctrl_addr) {
        Ok(listener) => {
            info!("control plane on tcp {ctrl_addr} — panel can poll status + pair");
            let status_ctrl = status.clone();
            control_ipc::serve_control(listener, hostname.clone(), status.clone(), move |req| {
                match req {
                    control_ipc::Request::StartPairing { addr } => match pairing.start(addr) {
                        // Reply with a fresh snapshot so the panel's pairing card
                        // shows the PIN at once (mirrors the Linux host's reply).
                        Ok(()) => {
                            let snap = status_ctrl
                                .lock()
                                .map(|s| s.clone())
                                .unwrap_or_else(|e| e.into_inner().clone());
                            control_ipc::Event::Status(snap)
                        }
                        Err(e) => control_ipc::Event::Error { msg: e },
                    },
                    control_ipc::Request::CancelPairing => {
                        pairing.cancel();
                        control_ipc::Event::Ack {
                            ok: true,
                            msg: None,
                        }
                    }
                    // Issue #19 peer-fleet controls — the same shared host-core
                    // helpers the Linux compositor dispatches to (the issue's
                    // parity note: every new Request handled on BOTH hosts).
                    // File + transport globals only, so they run fine on this
                    // serve thread with no render loop.
                    control_ipc::Request::ForgetPeer { pubkey } => {
                        match host_core::pair::forget_peer(&pubkey) {
                            Ok(out) if out.removed => {
                                info!(
                                    "panel: forgot peer {}… (primary={}, live sessions closed={})",
                                    &pubkey[..pubkey.len().min(8)],
                                    out.was_primary,
                                    out.sessions_closed
                                );
                                control_ipc::Event::Ack {
                                    ok: true,
                                    msg: Some(if out.sessions_closed > 0 {
                                        "peer forgotten; its live session was closed".into()
                                    } else {
                                        "peer forgotten".into()
                                    }),
                                }
                            }
                            Ok(_) => control_ipc::Event::Ack {
                                ok: false,
                                msg: Some("no such paired peer".into()),
                            },
                            Err(e) => control_ipc::Event::Ack {
                                ok: false,
                                msg: Some(format!("forget peer: {e}")),
                            },
                        }
                    }
                    control_ipc::Request::SetPrimaryPeer { pubkey } => {
                        match host_core::pair::set_primary_peer(pubkey.as_deref()) {
                            Ok(()) => control_ipc::Event::Ack {
                                ok: true,
                                msg: Some(match pubkey.as_deref() {
                                    Some(pk) => format!(
                                        "primary client → {}… (others no longer preempt it)",
                                        &pk[..pk.len().min(8)]
                                    ),
                                    None => "primary client cleared (newest-wins preempt)".into(),
                                }),
                            },
                            Err(e) => control_ipc::Event::Ack {
                                ok: false,
                                msg: Some(format!("set primary: {e}")),
                            },
                        }
                    }
                    // Panel/client app launch (issue #22): the same catalog +
                    // launcher path the Linux compositor dispatches to. The
                    // Windows context has no compositor socket — the captured
                    // desktop IS the streamed output, so a plain spawn lands on
                    // it — and carries the stream fps so an entry's
                    // FpsCap::Auto resolves to the stream rate. Runs on the
                    // serve thread: the Steam path detaches to its own worker
                    // inside launch(), and a direct spawn is quick, so the
                    // capture/encode loop is never blocked. The Child handle is
                    // dropped (no zombie semantics on Windows); Steam handoffs
                    // return no child anyway.
                    control_ipc::Request::LaunchApp { slug } => {
                        let ctx = app_launcher::LaunchContext::default().with_output_fps(fps);
                        match app_launcher::launch_slug(&slug, &ctx) {
                            Ok(child) => {
                                info!(
                                    "panel: launched app `{slug}`{}",
                                    match &child {
                                        Some(c) => format!(" (pid {})", c.id()),
                                        None => " (handed off / already running)".into(),
                                    }
                                );
                                control_ipc::Event::Ack {
                                    ok: true,
                                    msg: None,
                                }
                            }
                            Err(e) => control_ipc::Event::Ack {
                                ok: false,
                                msg: Some(format!("{e:#}")),
                            },
                        }
                    }
                    other => control_ipc::Event::Error {
                        msg: format!("Windows host does not support {other:?}"),
                    },
                }
            });
        }
        Err(e) => warn!("control plane bind {ctrl_addr} failed: {e} (status unavailable to panel)"),
    }

    // Advertise over mDNS so a fresh Trireme lists this host with no typed IP
    // (issue #17). The service is the pairing port; the `stream` TXT carries this
    // host's *actual* stream port (`bind`, the unified STREAM 15777 by default,
    // issue #17) so the client self-configures. Held for the process lifetime —
    // dropping `_mdns` withdraws the broadcast.
    let _mdns = {
        let pair_addr = std::net::SocketAddr::from(([0, 0, 0, 0], core_proto::ports::PAIR));
        match host_core::pair::advertise(pair_addr, &identity, bind.port()) {
            Ok(d) => {
                info!("mDNS advertising this host (stream port {})", bind.port());
                Some(d)
            }
            Err(e) => {
                warn!("mDNS advertise failed: {e:#} (LAN discovery off; add host by IP)");
                None
            }
        }
    };

    // Gate the encoder on a connected client so the first access unit is the
    // session's opening IDR — a clean live start with no backlog (same rationale
    // as the Linux capture-zc path).
    let wait_start = Instant::now();
    while !streamer.is_connected() {
        std::thread::sleep(Duration::from_millis(20));
        if wait_start.elapsed() > Duration::from_secs(120) {
            anyhow::bail!("no client connected within 120s");
        }
    }
    info!(
        "Trireme connected — capture+encode (output_idx={output_idx}, {fps}fps, {encoder:?}, {capture:?})"
    );
    if let Some(m) = &mqtt {
        m.publish_state("state", "streaming");
        m.publish_state("client_connected", "ON");
    }

    // ── P6 recovery ladder (thinner than the Linux compositor's — no NVML/Xid/
    // in-place reconfigure/systemd). The supervisor builds the capture+encode
    // pipeline, pumps access units, and on ANY fault (ffmpeg exit, a device-loss
    // string on ffmpeg stderr, or an injected fault) tears the pipeline down and
    // rebuilds it, with exponential backoff and a sliding-window circuit breaker.
    // On a breaker trip we PARK (Windows has no systemd to respawn us) keeping the
    // control plane alive so the panel/tray see a sticky-red Faulted.
    let mut breaker = RespawnBreaker::new(5, Duration::from_secs(60));
    let mut backoff = Duration::from_millis(500);
    let backoff_cap = Duration::from_secs(4);
    let mut generation: u64 = 0;

    // ── Adaptive bitrate controller (issue #30) ──────────────────────────────
    // wgc-zc only: it's the one path with an in-process NVENC session that can
    // `reconfigure_bitrate` in place. A ~1s AIMD loop rides the CBR target between the
    // MIN_ADAPTIVE_KBPS floor and the build ceiling. Congestion is ANY of: a
    // delivered-fps deficit (client QoS), forward packet loss, or an RTT bufferbloat
    // spike — the last of which is what fps/loss miss (frames still arrive, just late,
    // which is the "crisp but laggy" failure of the first cut). A TCP-style ssthresh
    // brake keeps recovery below capacity instead of chasing the ceiling. Only
    // publishes into `bitrate_ctl`; the encode thread applies the reconfigure. Survives
    // respawns (shared cell); snaps back to the ceiling on a new client.
    let bitrate_ctl = zerocopy::BitrateCtl::default();
    if matches!(capture, CaptureBackend::WgcZc) {
        if let Some(mut qos_rx) = streamer.take_qos_rx() {
            let (streamer_b, frames_b, ctl_b) =
                (streamer.clone(), frames.clone(), bitrate_ctl.clone());
            std::thread::Builder::new()
                .name("wgc-bitrate-ctl".into())
                .spawn(move || {
                    // ≥2% forward loss counts as congestion.
                    const LOSSY_PERMILLE: u32 = 20;
                    let mut recv_fps_ewma = 0.0f32;
                    let (mut clear_ticks, mut stale_ticks) = (0u32, 0u32);
                    // `allowed` = the loop's current target; `pushed` = last value sent to
                    // the encoder (8% debounce); `ssthresh` = the AIMD brake; `base_rtt_us`
                    // = the learned uncongested path RTT (min-filter with a slow leak).
                    let (mut allowed, mut pushed, mut ssthresh) = (0u32, 0u32, 0u32);
                    let mut base_rtt_us = 0u32;
                    let mut last_conn: Option<String> = None;
                    let mut last_frames = frames_b.load(Ordering::Relaxed);
                    let mut win = Instant::now();
                    loop {
                        std::thread::sleep(Duration::from_secs(1));
                        let ceiling = ctl_b.ceiling_kbps.load(Ordering::Relaxed);
                        let conn = streamer_b.connected_addr();
                        // New client (or disconnect): snap the target + brake back to the
                        // ceiling so a fresh session never inherits a prior client's rate.
                        if conn != last_conn {
                            last_conn = conn.clone();
                            recv_fps_ewma = 0.0;
                            clear_ticks = 0;
                            stale_ticks = 0;
                            base_rtt_us = 0;
                            allowed = ceiling;
                            pushed = ceiling;
                            ssthresh = ceiling;
                            if ceiling > 0 {
                                ctl_b.target_kbps.store(ceiling, Ordering::Relaxed);
                            }
                            while qos_rx.try_recv().is_ok() {}
                            last_frames = frames_b.load(Ordering::Relaxed);
                            win = Instant::now();
                            continue;
                        }
                        // No client, or the pipeline hasn't published a ceiling yet.
                        if conn.is_none() || ceiling == 0 {
                            while qos_rx.try_recv().is_ok() {}
                            last_frames = frames_b.load(Ordering::Relaxed);
                            win = Instant::now();
                            continue;
                        }
                        if allowed == 0 {
                            allowed = ceiling;
                            pushed = ceiling;
                            ssthresh = ceiling;
                        }
                        let floor = zerocopy::MIN_ADAPTIVE_KBPS.min(ceiling);
                        let secs = win.elapsed().as_secs_f32().max(1e-3);
                        win = Instant::now();
                        // Host send-rate this window (the client's delivered fps is
                        // measured against it), from the shared AU counter.
                        let f = frames_b.load(Ordering::Relaxed);
                        let send_fps = f.saturating_sub(last_frames) as f32 / secs;
                        last_frames = f;
                        // Client-reported delivered fps, EWMA α=0.5.
                        let mut got_qos = false;
                        while let Ok(q) = qos_rx.try_recv() {
                            recv_fps_ewma = if recv_fps_ewma <= 0.0 {
                                q.recv_fps
                            } else {
                                0.5 * q.recv_fps + 0.5 * recv_fps_ewma
                            };
                            got_qos = true;
                        }
                        stale_ticks = if got_qos { 0 } else { stale_ticks.saturating_add(1) };
                        // Bufferbloat: learn the baseline RTT (min-filter with a slow
                        // upward leak so a genuinely higher baseline is tracked), and flag
                        // congestion when the live RTT inflates past base + max(15ms,
                        // base/2) — queue buildup that loss and the fps deficit both miss.
                        let rtt = streamer_b.rtt_us();
                        if rtt > 0 {
                            base_rtt_us = if base_rtt_us == 0 {
                                rtt
                            } else {
                                rtt.min(base_rtt_us.saturating_add((base_rtt_us / 32).max(200)))
                            };
                        }
                        let bloat = rtt > 0
                            && base_rtt_us > 0
                            && rtt > base_rtt_us.saturating_add((base_rtt_us / 2).max(15_000));
                        // Delivered-fps deficit while QoS is fresh; forward loss always.
                        let loss = streamer_b.loss_permille();
                        let deficit = if stale_ticks < 3 && recv_fps_ewma > 2.0 && send_fps > 2.0 {
                            recv_fps_ewma / send_fps
                        } else {
                            1.0
                        };
                        let congested = deficit < 0.85 || loss >= LOSSY_PERMILLE || bloat;
                        clear_ticks = if congested { 0 } else { clear_ticks.saturating_add(1) };
                        // Additive probe step above the brake: ~4% of the ceiling.
                        let probe = (ceiling / 25).max(1000);
                        let (next_allowed, next_ssthresh) = adapt_bitrate_step(
                            allowed, ssthresh, floor, ceiling, congested, deficit, clear_ticks,
                            probe,
                        );
                        allowed = next_allowed;
                        ssthresh = next_ssthresh;
                        // Actuator debounce: only reconfigure on a >8% move, so a healthy
                        // link pinned at the ceiling never churns the encoder.
                        if allowed != pushed
                            && (allowed.abs_diff(pushed) as u64) * 100 > (pushed as u64) * 8
                        {
                            ctl_b.target_kbps.store(allowed, Ordering::Relaxed);
                            info!(
                                "adaptive bitrate: {pushed} -> {allowed} kbps (recv_fps~{recv_fps_ewma:.0} \
                                 send_fps~{send_fps:.0} loss={loss}permille rtt={}ms base={}ms bloat={bloat} \
                                 ssthresh={ssthresh} ceiling={ceiling})",
                                rtt / 1000,
                                base_rtt_us / 1000
                            );
                            pushed = allowed;
                        }
                    }
                })?;
        }
    }

    'supervisor: loop {
        // ── In-process WGC→NVENC generation (issue #12) ───────────────────
        // Same ladder as the ffmpeg path below, minus the subprocess: build,
        // pump typed AUs, ordered teardown, breaker/backoff bookkeeping. Kept
        // as its own branch so the ffmpeg path stays byte-identical.
        if matches!(capture, CaptureBackend::WgcZc) {
            // HDR is a session-level property (NVENC format is fixed at
            // create): sampled from the connected client's wants_hdr at each
            // generation build, so a preference change applies on the next
            // respawn (the Linux host's HDR-flip-is-a-relaunch rule).
            let pipeline = match zerocopy::ZcPipeline::start(
                output_idx as usize,
                fps,
                streamer.wants_hdr(),
                bitrate_ctl.clone(),
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("pipeline build failed (gen {generation}): {e:#}");
                    if breaker.record_and_check() {
                        set_health(
                            &gpu_health,
                            control_ipc::GpuHealth::Faulted {
                                reason: format!(
                                    "encoder build failed after {} tries: {e}",
                                    breaker.max
                                ),
                                last_xid: None,
                            },
                        );
                        break 'supervisor;
                    }
                    set_health(
                        &gpu_health,
                        control_ipc::GpuHealth::Degraded {
                            reason: format!("encoder restarting: {e}"),
                            throttle: None,
                        },
                    );
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(backoff_cap);
                    generation += 1;
                    continue 'supervisor;
                }
            };
            let (w, h) = pipeline.size;
            streamer.set_geometry(w, h);
            // Colorimetry: announce THIS generation's real HDR-ness so the client
            // conditions its tonemap correctly, and a mid-session HDR↔SDR flip at a
            // respawn boundary re-announces here (issue #16).
            streamer.set_hdr(pipeline.hdr);
            coded.store(pack_wh(w, h), Ordering::Relaxed);
            if let Ok(mut g) = hdr_label.lock() {
                *g = pipeline.hdr.then(|| "HEVC Main10 BT.2020/PQ".to_string());
            }

            let gen_start = Instant::now();
            set_health(&gpu_health, control_ipc::GpuHealth::Healthy);
            if let Some(m) = &mqtt {
                m.publish_state("state", "streaming");
            }

            let reason = inner_pump_zc(&pipeline, &frames, &streamer, inject.as_ref(), generation);
            pipeline.stop();

            if gen_start.elapsed() >= Duration::from_secs(3) {
                backoff = Duration::from_millis(500); // clean stream window → reset
            }
            let rmsg = reason.describe();
            // A deliberate HDR-flip reconfigure (issue #29) is not a fault: respawn
            // immediately at the new colorimetry — no breaker tick, no backoff — so the
            // client's reconfigure gap (its green/black flash) is as short as possible.
            if matches!(reason, FaultReason::Reconfigure(_)) {
                warn!("encoder pipeline reconfiguring (gen {generation}): {rmsg}");
                set_health(&gpu_health, control_ipc::GpuHealth::Healthy);
                generation += 1;
                continue 'supervisor;
            }
            warn!("encoder pipeline ended (gen {generation}): {rmsg} — recovering");
            if breaker.record_and_check() {
                set_health(
                    &gpu_health,
                    control_ipc::GpuHealth::Faulted {
                        reason: format!(
                            "GPU/encoder unrecoverable after {} restarts: {rmsg}",
                            breaker.max
                        ),
                        last_xid: None,
                    },
                );
                break 'supervisor;
            }
            set_health(
                &gpu_health,
                control_ipc::GpuHealth::Degraded {
                    reason: format!("encoder restart: {rmsg}"),
                    throttle: None,
                },
            );
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(backoff_cap);
            generation += 1;
            continue 'supervisor;
        }

        let pacer_stop = Arc::new(AtomicBool::new(false));
        let (enc, stdout, stderr, pacer, size) =
            match build_pipeline(capture, output_idx, fps, encoder, &pacer_stop) {
                Ok(t) => t,
                Err(e) => {
                    // The FIRST build error feeds the breaker too — a transient
                    // WGC/ffmpeg init failure at t=0 must not kill the host.
                    warn!("pipeline build failed (gen {generation}): {e:#}");
                    if breaker.record_and_check() {
                        set_health(
                            &gpu_health,
                            control_ipc::GpuHealth::Faulted {
                                reason: format!(
                                    "encoder build failed after {} tries: {e}",
                                    breaker.max
                                ),
                                last_xid: None,
                            },
                        );
                        break 'supervisor;
                    }
                    set_health(
                        &gpu_health,
                        control_ipc::GpuHealth::Degraded {
                            reason: format!("encoder restarting: {e}"),
                            throttle: None,
                        },
                    );
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(backoff_cap);
                    generation += 1;
                    continue 'supervisor;
                }
            };

        // Re-latch geometry each generation: a display-mode change kills the
        // pipeline (DDA access-lost, WGC device gone), so the respawn is where
        // a new size becomes visible; transport only re-announces on change,
        // so a same-size respawn sends nothing extra.
        if let Some((w, h)) = size {
            streamer.set_geometry(w, h);
            coded.store(pack_wh(w, h), Ordering::Relaxed);
        }
        // The ffmpeg capture path (this branch) is always SDR — only the wgc-zc
        // pipeline builds a 10-bit Main10 encode. Announce SDR so a client that had
        // seen an HDR generation from a prior wgc-zc build drops its tonemap (issue
        // #16); transport only re-announces on change, so a steady SDR run is quiet.
        streamer.set_hdr(false);

        let sig = FaultSignal::new(generation);
        let scanner = spawn_stderr_scanner(stderr, sig.clone());
        let (stdout_rx, reader) = spawn_stdout_reader(stdout);
        let mut splitter = au::AnnexBSplitter::new();
        let gen_start = Instant::now();
        set_health(&gpu_health, control_ipc::GpuHealth::Healthy);
        if let Some(m) = &mqtt {
            m.publish_state("state", "streaming");
        }

        let reason = inner_pump(
            stdout_rx,
            &sig,
            &mut splitter,
            &frames,
            &streamer,
            inject.as_ref(),
            generation,
        );

        // Teardown in strict order so no D3D11 device / capture thread leaks:
        // signal the pacer to stop, kill ffmpeg (unblocks a write_all-stuck pacer
        // AND EOFs the stderr scanner + stdout reader), then join the helpers.
        pacer_stop.store(true, Ordering::Relaxed);
        drop(enc);
        if let Some(h) = pacer {
            let _ = h.join();
        }
        let _ = reader.join();
        let _ = scanner.join();
        // NB: do NOT flush()+send the splitter tail on a respawn — a truncated
        // partial AU is undecodable; the fresh ffmpeg emits a new VPS/SPS/PPS+IDR
        // within ~1 GOP and the client resyncs.

        if gen_start.elapsed() >= Duration::from_secs(3) {
            backoff = Duration::from_millis(500); // clean stream window → reset
        }
        let rmsg = reason.describe();
        warn!("encoder pipeline ended (gen {generation}): {rmsg} — recovering");
        if breaker.record_and_check() {
            set_health(
                &gpu_health,
                control_ipc::GpuHealth::Faulted {
                    reason: format!(
                        "GPU/encoder unrecoverable after {} restarts: {rmsg}",
                        breaker.max
                    ),
                    last_xid: None,
                },
            );
            break 'supervisor;
        }
        set_health(
            &gpu_health,
            control_ipc::GpuHealth::Degraded {
                reason: format!("encoder restart: {rmsg}"),
                throttle: None,
            },
        );
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(backoff_cap);
        generation += 1;
    }

    // Breaker tripped. Windows has no systemd Restart=on-failure and no durable
    // fault file, so the live TCP control plane is the ONLY fault channel to the
    // panel: PARK here (returning would propagate to main() returning, which exits
    // the process and kills the detached control-plane thread) and keep serving
    // the sticky-red Faulted status indefinitely.
    warn!("encoder gave up after repeated faults — parking; panel shows Faulted");
    if let Some(m) = &mqtt {
        m.publish_state("state", "faulted");
        m.publish_state("client_connected", "OFF");
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Feed the WGC capturer's BGRA frames to ffmpeg's rawvideo stdin at a steady
/// `fps`, on a dedicated thread. WGC is **change-driven** (a static screen emits
/// no new frames), so the pacer holds the last frame and re-emits it each tick —
/// turning a bursty source into the continuous stream the encoder expects. Frame
/// back-pressure comes for free: if ffmpeg is slow, `write_all` blocks.
#[cfg(windows)]
fn spawn_pacer(
    cap: wgc::WgcCapturer,
    mut stdin: std::process::ChildStdin,
    fps: u32,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let handle = std::thread::Builder::new()
        .name("wgc-pacer".into())
        .spawn(move || {
            let mut last: Option<Vec<u8>> = None;
            let mut next = Instant::now() + interval;
            loop {
                // Supervisor asked us to stop (respawn teardown): exit so the
                // WgcCapturer we own is dropped and its D3D11 device released.
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // Until the next tick, wait for a fresh frame; then drain any
                // further queued frames so we always emit the newest one.
                let now = Instant::now();
                if now < next {
                    match cap.recv_timeout(next - now) {
                        wgc::Recv::Frame(f) => {
                            last = Some(f.bgra);
                            while let wgc::Recv::Frame(f2) = cap.try_recv() {
                                last = Some(f2.bgra);
                            }
                        }
                        wgc::Recv::Empty => {}
                        // Capture thread died (device/session fault). Stop the
                        // pacer: dropping ffmpeg's stdin EOFs the encoder, the
                        // pump reports the fault, and the supervisor respawns
                        // with a fresh WGC session + D3D11 device. Without this
                        // the held frame streams on as a silent freeze-frame.
                        wgc::Recv::Dead => {
                            tracing::warn!(
                                "wgc-pacer: capture thread gone; stopping so the supervisor respawns"
                            );
                            break;
                        }
                    }
                }
                let now = Instant::now();
                if now < next {
                    std::thread::sleep(next - now);
                }
                next += interval;

                // Re-emit the held frame; skip until the first one arrives. If
                // ffmpeg died (stdin closed) write_all errors and we stop — and the
                // supervisor's drop(enc) closes the pipe even if we're blocked here.
                if let Some(ref buf) = last {
                    if stdin.write_all(buf).is_err() {
                        tracing::warn!("wgc-pacer: ffmpeg stdin closed; stopping");
                        break;
                    }
                }
            }
        })?;
    Ok(handle)
}

// ───────────────────────────── P6 recovery ladder ─────────────────────────────

/// Why a fault-test host restarted its encode pipeline.
#[cfg(windows)]
enum FaultReason {
    /// ffmpeg exited on its own (stdout EOF).
    Eof,
    /// A device-loss/TDR string appeared on ffmpeg stderr.
    DeviceLoss(String),
    /// The PHAROS_WIN_FAULT_INJECT seam fired a synthetic fault.
    FaultInjected(String),
    /// A DELIBERATE reconfigure (wgc-zc mid-session HDR flip, issue #29) — respawn
    /// immediately at the new colorimetry, without the fault backoff or breaker tick.
    Reconfigure(String),
}

#[cfg(windows)]
impl FaultReason {
    fn describe(&self) -> String {
        match self {
            FaultReason::Eof => "ffmpeg exited (stdout EOF)".to_string(),
            FaultReason::DeviceLoss(r) => format!("device loss: {r}"),
            FaultReason::FaultInjected(r) => r.clone(),
            FaultReason::Reconfigure(r) => r.clone(),
        }
    }
}

/// Per-generation cross-thread signal: the stderr scanner publishes a device-loss
/// event (Release) that the pump polls (Acquire). Minted fresh each generation so
/// a stale `true` from a killed child can't instantly re-trip the new stream.
#[cfg(windows)]
#[derive(Clone)]
struct FaultSignal {
    device_loss: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    #[allow(dead_code)]
    generation: u64,
}

#[cfg(windows)]
impl FaultSignal {
    fn new(generation: u64) -> Self {
        Self {
            device_loss: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reason: std::sync::Arc::new(std::sync::Mutex::new(None)),
            generation,
        }
    }
}

/// Best-guess (lowercased) substrings that indicate a real GPU device-loss/TDR on
/// the NVENC/DXGI/D3D11VA path. BEST-GUESS until confirmed against a real TDR on
/// the rig — a `const &[&str]` so a newly-observed string is a one-line add. Tier-2
/// ambiguous strings (bare "cuda_error", "operation not permitted") are excluded
/// to avoid false Faulted; the plain ffmpeg-exit respawn path already covers
/// "ffmpeg died for any reason".
#[cfg(windows)]
const DEVICE_LOSS_PATTERNS: &[&str] = &[
    "dxgi_error_device_removed",
    "dxgi_error_device_reset",
    "device removed",
    "device reset",
    "device lost",
    "no nvenc capable devices",
    "cannot load nvcuvid",
    "openencodesessionex failed",
    "cuda_error_no_device",
    "cuda_error_invalid_device",
    "hardware accelerator device removed",
];

/// Scan ffmpeg stderr line-by-line: echo every line (replacing the old
/// `.stderr(inherit())` so diagnostics stay visible) and flag a device-loss on a
/// pattern match. Ends on EOF (the supervisor's drop(enc) closes the pipe).
#[cfg(windows)]
fn spawn_stderr_scanner(
    stderr: std::process::ChildStderr,
    sig: FaultSignal,
) -> std::thread::JoinHandle<()> {
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::Ordering;
    std::thread::Builder::new()
        .name("ffmpeg-stderr".into())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                tracing::info!(target: "pharos::ffmpeg", "{line}");
                if !sig.device_loss.load(Ordering::Relaxed) {
                    let low = line.to_ascii_lowercase();
                    if DEVICE_LOSS_PATTERNS.iter().any(|p| low.contains(p)) {
                        if let Ok(mut g) = sig.reason.lock() {
                            if g.is_none() {
                                *g = Some(line.clone());
                            }
                        }
                        // Release so the reason String is visible before the flag.
                        sig.device_loss.store(true, Ordering::Release);
                    }
                }
                // Keep draining to EOF so remaining lines echo + the pipe closes.
            }
        })
        .expect("spawn ffmpeg-stderr scanner")
}

/// Read ffmpeg stdout on its own thread and forward chunks over a bounded channel,
/// so the pump can poll the device-loss flag on a `recv_timeout` even when a hung
/// ffmpeg emits neither bytes nor EOF (the true-TDR hang case). Exits on EOF or
/// when the pump drops the receiver.
#[cfg(windows)]
fn spawn_stdout_reader(
    mut stdout: std::process::ChildStdout,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::thread::JoinHandle<()>,
) {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let handle = std::thread::Builder::new()
        .name("ffmpeg-stdout".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break, // ffmpeg exited
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // pump dropped the receiver
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn ffmpeg-stdout reader");
    (rx, handle)
}

/// Best-effort pixel size of the monitor `output_idx` selects under `capture`.
/// Dda asks DXGI (ffmpeg's ddagrab enumerates DXGI outputs on adapter 0); Wgc
/// asks `EnumDisplayMonitors`, the same list `WgcCapturer::start` indexes.
/// `None` means unknown: streaming proceeds without a geometry announce (the
/// pre-fix behavior) and the client falls back to its 1080p default.
#[cfg(windows)]
fn capture_size(capture: CaptureBackend, output_idx: u32) -> Option<(u32, u32)> {
    match capture {
        CaptureBackend::Dda => match display::dda_output_size(output_idx) {
            Ok(wh) => Some(wh),
            Err(e) => {
                tracing::warn!("capture geometry unknown: {e:#} (client will use its default)");
                None
            }
        },
        CaptureBackend::Wgc | CaptureBackend::WgcZc => wgc::list_monitors()
            .get(output_idx as usize)
            .copied()
            .filter(|&(w, h)| w > 0 && h > 0),
    }
}

/// Build one capture+encode pipeline generation. The SINGLE path used by both the
/// first start and every respawn — its `Err` feeds the supervisor's breaker (no
/// separate first-start bail). Returns the encoder, its stdout/stderr, (WGC only)
/// the pacer JoinHandle so the supervisor can stop+join it on teardown, and the
/// capture pixel size for the geometry announce (`None` = unknown, no announce).
#[cfg(windows)]
#[allow(clippy::type_complexity)]
fn build_pipeline(
    capture: CaptureBackend,
    output_idx: u32,
    fps: u32,
    encoder: encode::Encoder,
    pacer_stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<(
    encode::CaptureEncoder,
    std::process::ChildStdout,
    std::process::ChildStderr,
    Option<std::thread::JoinHandle<()>>,
    Option<(u32, u32)>,
)> {
    match capture {
        CaptureBackend::Dda => {
            // Resolve BEFORE spawning ffmpeg so each respawn re-reads the
            // (possibly changed) mode the fresh ddagrab will capture.
            let size = capture_size(capture, output_idx);
            let mut e = encode::CaptureEncoder::spawn(output_idx, fps, encoder)?;
            let stdout = e
                .take_stdout()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stdout pipe"))?;
            let stderr = e
                .take_stderr()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stderr pipe"))?;
            Ok((e, stdout, stderr, None, size))
        }
        // The zero-copy path never reaches build_pipeline: the supervisor
        // branches to ZcPipeline::start before calling it.
        CaptureBackend::WgcZc => unreachable!("wgc-zc has its own supervisor branch"),
        CaptureBackend::Wgc => {
            // Fresh WgcCapturer per generation: re-reads the monitor size and gets
            // a fresh D3D11 device (a TDR likely killed the old one).
            let (w, h, cap) = wgc::WgcCapturer::start(output_idx as usize)?;
            tracing::info!("WGC capturing monitor {output_idx} at {w}x{h}; pacing to {fps}fps");
            let mut e = encode::CaptureEncoder::spawn_wgc(w, h, fps, encoder)?;
            let stdin = e
                .take_stdin()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stdin pipe"))?;
            let stdout = e
                .take_stdout()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stdout pipe"))?;
            let stderr = e
                .take_stderr()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stderr pipe"))?;
            let pacer = spawn_pacer(cap, stdin, fps, pacer_stop.clone())?;
            // WGC already knows the exact frame size it started with; announce
            // that rather than re-enumerating monitors.
            Ok((e, stdout, stderr, Some(pacer), Some((w, h))))
        }
    }
}

/// Pump ffmpeg's AUs onto the stream until a fault. Polls the device-loss flag
/// each iteration (so a hung ffmpeg still breaks out via the recv timeout) and
/// checks the fault-inject seam per frame. Owns the stdout receiver, so returning
/// drops it → the reader thread exits.
#[cfg(windows)]
fn inner_pump(
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    sig: &FaultSignal,
    splitter: &mut au::AnnexBSplitter,
    frames: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    streamer: &std::sync::Arc<transport::QuicStreamer>,
    inject: Option<&InjectSpec>,
    generation: u64,
) -> FaultReason {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};
    use tracing::{info, warn};

    let mut gen_frames: u64 = 0;
    let mut last_log = Instant::now();
    loop {
        if sig.device_loss.load(Ordering::Acquire) {
            let reason = sig
                .reason
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| "ffmpeg device loss".to_string());
            return FaultReason::DeviceLoss(reason);
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                for au in splitter.push(&chunk) {
                    frames.fetch_add(1, Ordering::Relaxed);
                    gen_frames += 1;
                    streamer.send(au);
                    if let Some(spec) = inject {
                        if spec.should_fire(generation, gen_frames) {
                            return FaultReason::FaultInjected(spec.label());
                        }
                    }
                }
                if last_log.elapsed() >= Duration::from_secs(2) {
                    info!(
                        "encode: {} access units sent",
                        frames.load(Ordering::Relaxed)
                    );
                    last_log = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                warn!("ffmpeg stdout closed — encoder exited");
                return FaultReason::Eof;
            }
        }
    }
}

/// Pump the in-process pipeline's AUs onto the stream until a fault: the
/// zero-copy analogue of [`inner_pump`]. No splitter (one NVENC lock = one
/// AU), typed fault reasons instead of stderr scraping, and keyframe flags
/// via `send_frame` so IDRs get the transport queue's keyframe reserve. The
/// fault-inject seam counts encoded AUs exactly like the ffmpeg path, so the
/// P6 test timings hold.
#[cfg(windows)]
fn inner_pump_zc(
    pipeline: &zerocopy::ZcPipeline,
    frames: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    streamer: &std::sync::Arc<transport::QuicStreamer>,
    inject: Option<&InjectSpec>,
    generation: u64,
) -> FaultReason {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use tracing::info;

    let mut gen_frames: u64 = 0;
    let mut last_log = Instant::now();
    loop {
        match pipeline.recv_timeout(Duration::from_millis(200)) {
            zerocopy::ZcRecv::Au(au) => {
                frames.fetch_add(1, Ordering::Relaxed);
                gen_frames += 1;
                streamer.send_frame(au.bytes, au.keyframe);
                if let Some(spec) = inject {
                    if spec.should_fire(generation, gen_frames) {
                        return FaultReason::FaultInjected(spec.label());
                    }
                }
                if last_log.elapsed() >= Duration::from_secs(2) {
                    info!(
                        "encode: {} access units sent",
                        frames.load(Ordering::Relaxed)
                    );
                    last_log = Instant::now();
                }
            }
            zerocopy::ZcRecv::Empty => continue,
            zerocopy::ZcRecv::Dead => {
                let reason = pipeline
                    .fault_reason()
                    .unwrap_or_else(|| "encoder thread exited".to_string());
                // A deliberate HDR-flip respawn (issue #29) is a reconfigure, not a
                // fault: classify it so the supervisor skips the backoff + breaker.
                if reason.contains("issue #29") {
                    return FaultReason::Reconfigure(reason);
                }
                return FaultReason::DeviceLoss(reason);
            }
        }
    }
}

/// Set the shared GPU-health cell (single writer = the supervisor).
#[cfg(windows)]
fn set_health(
    cell: &std::sync::Arc<std::sync::Mutex<control_ipc::GpuHealth>>,
    health: control_ipc::GpuHealth,
) {
    if let Ok(mut g) = cell.lock() {
        *g = health;
    }
}

/// Sliding-window circuit breaker: trips when `max` restarts fall within `window`.
/// Single-owner on the run_stream thread — no synchronization.
#[cfg(any(windows, test))]
struct RespawnBreaker {
    stamps: std::collections::VecDeque<std::time::Instant>,
    max: usize,
    window: std::time::Duration,
}

#[cfg(any(windows, test))]
impl RespawnBreaker {
    fn new(max: usize, window: std::time::Duration) -> Self {
        Self {
            stamps: std::collections::VecDeque::new(),
            max,
            window,
        }
    }

    /// Record a restart; return true once `max` restarts fall inside `window`.
    fn record_and_check(&mut self) -> bool {
        let now = std::time::Instant::now();
        self.stamps.push_back(now);
        while self
            .stamps
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            self.stamps.pop_front();
        }
        self.stamps.len() >= self.max
    }
}

/// Parsed `PHAROS_WIN_FAULT_INJECT`. `device_lost` → one-shot in generation 0;
/// `device_lost:P` → a synthetic fault every P frames in every generation.
#[cfg(any(windows, test))]
#[derive(Debug, Clone)]
struct InjectSpec {
    periodic: Option<u64>,
    oneshot_after: u64,
}

#[cfg(any(windows, test))]
impl InjectSpec {
    fn should_fire(&self, generation: u64, gen_frames: u64) -> bool {
        match self.periodic {
            Some(p) => gen_frames > 0 && gen_frames.is_multiple_of(p),
            None => generation == 0 && gen_frames >= self.oneshot_after,
        }
    }

    fn label(&self) -> String {
        match self.periodic {
            Some(p) => format!("injected device_lost (every {p} frames)"),
            None => "injected device_lost (one-shot)".to_string(),
        }
    }
}

/// Parse the `PHAROS_WIN_FAULT_INJECT` value. Only `device_lost[:P]` is recognized;
/// anything else arms nothing.
#[cfg(any(windows, test))]
fn parse_win_fault_inject(s: &str) -> Option<InjectSpec> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (kind, period) = match s.split_once(':') {
        Some((k, p)) => (k, p.trim().parse::<u64>().ok()),
        None => (s, None),
    };
    if !kind.eq_ignore_ascii_case("device_lost") {
        return None;
    }
    Some(match period {
        Some(p) if p > 0 => InjectSpec {
            periodic: Some(p),
            oneshot_after: 0,
        },
        _ => InjectSpec {
            periodic: None,
            oneshot_after: 120,
        },
    })
}

/// Capture N frames from a monitor via WGC and dump the last as raw BGRA —
/// proves the WGC path in isolation before wiring it into the encode loop.
#[cfg(windows)]
fn run_wgc_test(monitor_idx: usize, frames: u32, out: &str) -> anyhow::Result<()> {
    use std::time::Instant;
    use tracing::info;

    let mons = wgc::list_monitors();
    info!("monitors present ({}):", mons.len());
    for (i, (w, h)) in mons.iter().enumerate() {
        info!("  [{i}] {w}x{h}");
    }

    let (w, h, cap) = wgc::WgcCapturer::start(monitor_idx)?;
    info!("capturing monitor {monitor_idx} at {w}x{h} for up to 5s / {frames} frames…");

    let t0 = Instant::now();
    let mut n = 0u32;
    let mut wrote = false;
    while n < frames && t0.elapsed() < std::time::Duration::from_secs(5) {
        match cap.recv_timeout(std::time::Duration::from_millis(1000)) {
            wgc::Recv::Frame(f) => {
                n += 1;
                // Keep the latest frame on disk, so even a single (change-driven)
                // frame yields an inspectable image.
                std::fs::write(out, &f.bgra)?;
                wrote = true;
                if n.is_multiple_of(30) {
                    info!("  {n} frames…");
                }
            }
            wgc::Recv::Empty => {
                info!("  no new frame within 1s (WGC is change-driven; idle desktop)")
            }
            wgc::Recv::Dead => anyhow::bail!("WGC capture thread died mid-test"),
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(1e-6);
    info!(
        "captured {n} frames in {secs:.2}s ({:.1} fps); wrote last → {out} (wrote={wrote})",
        n as f64 / secs
    );
    Ok(())
}

/// Plug a virtual Xbox 360 pad via ViGEmBus and drive a visible pattern (left
/// stick circles, right trigger pulses, A toggles once a second) so joy.cpl or
/// Device Manager shows the pad working with no streaming client attached —
/// the issue #2 bring-up check, sibling to `wgc-test`.
#[cfg(windows)]
fn run_pad_test(secs: u64) -> anyhow::Result<()> {
    use core_proto::gamepad as g;
    use core_proto::InputEvent;
    use std::time::{Duration, Instant};
    use tracing::info;

    let mut inj = input::Injector::new();
    inj.dispatch(&InputEvent::GamepadConnect {
        slot: 0,
        vendor: 0x045e,
        product: 0x028e,
    });
    info!("driving the pad for {secs}s: check joy.cpl → \"Xbox 360 Controller\"…");
    let t0 = Instant::now();
    let mut t = 0f32;
    while t0.elapsed() < Duration::from_secs(secs) {
        t += 0.016;
        inj.dispatch(&InputEvent::GamepadAxis {
            slot: 0,
            axis: g::AXIS_LEFT_X,
            value: t.sin(),
        });
        inj.dispatch(&InputEvent::GamepadAxis {
            slot: 0,
            axis: g::AXIS_LEFT_Y,
            value: t.cos(),
        });
        inj.dispatch(&InputEvent::GamepadAxis {
            slot: 0,
            axis: g::AXIS_RTRIGGER,
            value: t.sin() * 0.5 + 0.5,
        });
        inj.dispatch(&InputEvent::GamepadButton {
            slot: 0,
            button: g::BTN_A,
            pressed: (t as u64).is_multiple_of(2),
        });
        std::thread::sleep(Duration::from_millis(16));
    }
    inj.dispatch(&InputEvent::GamepadDisconnect { slot: 0 });
    info!("pad-test done; pad unplugged");
    Ok(())
}

#[cfg(windows)]
fn init_tracing() {
    // Enable the Windows console's virtual-terminal processing so the ANSI colour
    // codes tracing emits render as colours, not literal `<ESC>[2m` garbage in the
    // pharos-host PowerShell/cmd window. Old conhost defaults VT off; Win10 1511+
    // supports enabling it. Best-effort: on failure we just keep the raw output.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
        };
        if let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) {
            let mut mode = CONSOLE_MODE(0);
            if GetConsoleMode(h, &mut mode).is_ok() {
                let _ = SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .init();
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "pharos-host is Windows-only (IddCx virtual display / ddagrab / SendInput). \
         Build and run it on the Windows host; the Linux host is `pharos-compositor`."
    );
}

#[cfg(test)]
mod tests {
    use super::{
        adapt_bitrate_step, build_status, pack_wh, parse_win_fault_inject, unpack_wh,
        RespawnBreaker,
    };

    #[test]
    fn adaptive_congestion_backs_off_below_the_congestion_point() {
        // deficit 0.7 → drop to 70%; ssthresh braked to 85% of where we congested
        // (60000×0.7=42000, ssthresh=60000×0.85=51000).
        assert_eq!(
            adapt_bitrate_step(60000, 60000, 2000, 60000, true, 0.7, 0, 2400),
            (42000, 51000)
        );
    }

    #[test]
    fn adaptive_fast_recovery_stops_at_ssthresh() {
        // Clean ≥3 windows, below the brake → +20%, but never past ssthresh.
        assert_eq!(
            adapt_bitrate_step(42000, 51000, 2000, 60000, false, 1.0, 3, 2400),
            (50400, 51000)
        );
        // Next +20% would be 60480 but caps at the brake.
        assert_eq!(
            adapt_bitrate_step(50400, 51000, 2000, 60000, false, 1.0, 3, 2400),
            (51000, 51000)
        );
    }

    #[test]
    fn adaptive_probes_additively_above_the_brake() {
        // At/above ssthresh + clean → additive probe, raising the brake with it.
        assert_eq!(
            adapt_bitrate_step(51000, 51000, 2000, 60000, false, 1.0, 3, 2400),
            (53400, 53400)
        );
    }

    #[test]
    fn adaptive_holds_until_three_clean_windows() {
        assert_eq!(
            adapt_bitrate_step(42000, 51000, 2000, 60000, false, 1.0, 2, 2400),
            (42000, 51000)
        );
    }

    #[test]
    fn adaptive_inert_pinned_at_ceiling_on_a_clean_link() {
        // allowed == ceiling == ssthresh, clean → no change (probe clamps at ceiling).
        assert_eq!(
            adapt_bitrate_step(60000, 60000, 2000, 60000, false, 1.0, 3, 2400),
            (60000, 60000)
        );
    }

    #[test]
    fn adaptive_congestion_clamps_to_floor() {
        // Hard deficit clamps allowed to the floor; ssthresh stays >= floor
        // (2500×0.5=1250 → floor 2000; ssthresh=2500×0.85=2125).
        assert_eq!(
            adapt_bitrate_step(2500, 2200, 2000, 60000, true, 0.1, 0, 2400),
            (2000, 2125)
        );
    }

    #[test]
    fn status_reflects_connected_client() {
        let s = build_status(
            "winrig",
            "AMF HEVC",
            "HEVC",
            60,
            true,
            Some("192.0.2.5:51234".into()),
            Some("ab12cd".into()),
            4096,
            (3840, 2160),
            Some("HEVC Main10 BT.2020/PQ".into()),
            control_ipc::GpuHealth::Healthy,
        );
        assert_eq!(s.hostname, "winrig");
        assert!(s.streaming);
        assert_eq!(s.video_codec.as_deref(), Some("HEVC"));
        assert_eq!(s.clients.len(), 1);
        let c = &s.clients[0];
        assert_eq!(c.encoder, "AMF HEVC");
        assert_eq!(c.codec, "HEVC");
        assert_eq!(c.fps, 60);
        assert_eq!(c.frames_sent, 4096);
        assert_eq!(c.addr.as_deref(), Some("192.0.2.5:51234"));
        assert_eq!(c.pubkey.as_deref(), Some("ab12cd"));
        // Issue #1: the captured monitor's size reaches the panel.
        assert_eq!((c.coded_w, c.coded_h), (3840, 2160));
        // Issue #4: the HDR badge follows the live session label.
        assert_eq!(s.hdr_active.as_deref(), Some("HEVC Main10 BT.2020/PQ"));
    }

    /// Issue #22: the published status advertises exactly the verbs this host's
    /// serve_control closure handles — pairing, peers, launch_app — and nothing
    /// it would bounce with Event::Error. The panel's per-control gating rides
    /// on this being truthful.
    #[test]
    fn status_advertises_exactly_the_served_caps() {
        let s = build_status(
            "winrig",
            "NVENC HEVC",
            "HEVC",
            60,
            false,
            None,
            None,
            0,
            (0, 0),
            None,
            control_ipc::GpuHealth::Healthy,
        );
        assert!(!s.read_only, "pairing works, so not status-only");
        assert!(s.caps.pairing && s.caps.peers && s.caps.launch_app);
        assert!(
            !s.caps.relaunch
                && !s.caps.preview
                && !s.caps.stream_mode
                && !s.caps.input_toggle
                && !s.caps.controllers
                && !s.caps.frame_timings,
            "unserved verbs must not be advertised: {:?}",
            s.caps
        );
    }

    #[test]
    fn wh_packing_roundtrips() {
        assert_eq!(unpack_wh(pack_wh(3840, 2160)), (3840, 2160));
        assert_eq!(unpack_wh(pack_wh(0, 0)), (0, 0));
        assert_eq!(unpack_wh(0), (0, 0)); // unset atomic = unknown
    }

    #[test]
    fn status_idle_has_no_clients() {
        let s = build_status(
            "winrig",
            "AMF HEVC",
            "HEVC",
            60,
            false,
            None,
            None,
            0,
            (0, 0),
            None,
            control_ipc::GpuHealth::Healthy,
        );
        assert!(!s.streaming);
        assert!(s.hdr_active.is_none());
        assert!(s.clients.is_empty());
        // Even idle, the host advertises its codec so the panel's Video tab can
        // light up the active encoder before a client connects.
        assert_eq!(s.video_codec.as_deref(), Some("HEVC"));
    }

    #[test]
    fn gpu_health_flows_into_status() {
        let s = build_status(
            "winrig",
            "NVENC HEVC",
            "HEVC",
            60,
            true,
            None,
            None,
            0,
            (1920, 1080),
            None,
            control_ipc::GpuHealth::Faulted {
                reason: "device removed".into(),
                last_xid: None,
            },
        );
        match s.gpu_health {
            control_ipc::GpuHealth::Faulted { reason, last_xid } => {
                assert_eq!(reason, "device removed");
                assert!(last_xid.is_none());
            }
            other => panic!("expected Faulted, got {other:?}"),
        }
    }

    #[test]
    fn breaker_trips_on_burst() {
        let mut b = RespawnBreaker::new(3, std::time::Duration::from_secs(10));
        assert!(!b.record_and_check()); // 1
        assert!(!b.record_and_check()); // 2
        assert!(b.record_and_check()); // 3 → trip
    }

    #[test]
    fn breaker_evicts_outside_window() {
        let mut b = RespawnBreaker::new(2, std::time::Duration::from_millis(40));
        assert!(!b.record_and_check()); // 1 in window
        std::thread::sleep(std::time::Duration::from_millis(60));
        // The first stamp is now outside the 40ms window → evicted; only this one
        // is inside → no trip.
        assert!(!b.record_and_check());
        // A second within the window → 2 inside → trip.
        assert!(b.record_and_check());
    }

    #[test]
    fn fault_inject_parse_and_fire() {
        // one-shot: device_lost
        let s = parse_win_fault_inject("device_lost").unwrap();
        assert_eq!(s.periodic, None);
        assert_eq!(s.oneshot_after, 120);
        assert!(s.should_fire(0, 120));
        assert!(!s.should_fire(0, 119));
        assert!(!s.should_fire(1, 500)); // one-shot fires in generation 0 only
                                         // periodic: device_lost:30
        let p = parse_win_fault_inject("device_lost:30").unwrap();
        assert_eq!(p.periodic, Some(30));
        assert!(p.should_fire(0, 30));
        assert!(p.should_fire(2, 60)); // every generation
        assert!(!p.should_fire(0, 31));
        assert_eq!(s.label(), "injected device_lost (one-shot)");
        assert_eq!(p.label(), "injected device_lost (every 30 frames)");
        // unknown / empty / :0 → one-shot fallback
        assert!(parse_win_fault_inject("garbage").is_none());
        assert!(parse_win_fault_inject("").is_none());
        assert!(parse_win_fault_inject("device_lost:0")
            .unwrap()
            .periodic
            .is_none());
    }
}
