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
mod encode;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod wgc;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::net::SocketAddr;

    init_tracing();
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
        // `pharos-host stream [bind] [output_idx] [fps] [qsv|nvenc|amf] [dda|wgc]`
        Some("stream") => {
            let bind = match args.get(2) {
                Some(s) => parse_bind(s)?,
                None => (
                    std::net::Ipv4Addr::UNSPECIFIED,
                    core_proto::ports::WIN_STREAM,
                )
                    .into(),
            };
            let output_idx: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            let fps: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
            let encoder = match args.get(5).map(String::as_str) {
                Some("nvenc") => encode::Encoder::Nvenc,
                Some("amf") => encode::Encoder::Amf,
                _ => encode::Encoder::Qsv,
            };
            // Capture backend: ddagrab (DXGI Desktop Duplication, self-pacing) by
            // default; `wgc` uses Windows.Graphics.Capture (captures IddCx virtual
            // displays + by HMONITOR regardless of adapter — needed on the headless
            // AMD rig where ddagrab can't reach the virtual display's adapter).
            let capture = match args.get(6).map(String::as_str) {
                Some("wgc") => CaptureBackend::Wgc,
                _ => CaptureBackend::Dda,
            };
            run_stream(bind, output_idx, fps, encoder, capture)
        }
        // `pharos-host wgc-test [monitor_idx] [frames] [out.bgra]` — capture via
        // Windows.Graphics.Capture and dump the last frame (validates WGC alone).
        Some("wgc-test") => {
            let idx: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let frames: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120);
            let out = args.get(4).map(String::as_str).unwrap_or("wgc-frame.bgra");
            run_wgc_test(idx, frames, out)
        }
        _ => {
            eprintln!(
                "Pharos host (Windows)\n\
                 usage:\n  \
                 pharos-host pair     [addr=0.0.0.0:4432]\n  \
                 pharos-host stream   [bind=0.0.0.0:4435] [output_idx=0] [fps=60] [qsv|nvenc|amf] [dda|wgc]\n  \
                 pharos-host wgc-test [monitor_idx=0] [frames=120] [out.bgra]"
            );
            Ok(())
        }
    }
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
}

/// Accept either a bare `host:port` or a `quic://host:port` bind string (the
/// Linux `capture-zc` uses the latter form), returning the bind `SocketAddr`.
#[cfg(windows)]
fn parse_bind(s: &str) -> anyhow::Result<std::net::SocketAddr> {
    let s = s.strip_prefix("quic://").unwrap_or(s);
    s.parse()
        .map_err(|e| anyhow::anyhow!("bad bind address {s:?}: {e}"))
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
            // Resolution ladder is Linux/NVENC-only; the Windows host doesn't
            // expose a coded size here yet (0,0 = unknown to the panel).
            coded_w: 0,
            coded_h: 0,
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
        // serve_status answers only Hello/GetStatus; every mutating control
        // request gets Event::Error. Tell the panel so it disables them.
        read_only: true,
        // P6: the run_stream supervisor loop drives this (Healthy while streaming,
        // Degraded during a respawn/backoff, Faulted when the breaker trips).
        // throttle + last_xid are always None on Windows (no NVML/Xid plane).
        gpu_health,
        // Single-client only — no mirror fan-out on the Windows path.
        stream_mode: control_ipc::StreamMode::Single,
        // Windows host HDR capture+encode is the deferred game-HDR path; until
        // it lands the Windows host always streams SDR.
        hdr_active: None,
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

    // Enforce pairing by default, exactly like the Linux compositor: only
    // clients whose verified Ed25519 pubkey is in `peers.toml` may stream
    // (`PHAROS_ALLOW_UNPAIRED=1` opts out). This is the shared cross-platform
    // authorizer — the one place the M2 auth work wasn't yet applied on Windows.
    let streamer = Arc::new(transport::QuicStreamer::start_with_authorizer(
        bind,
        identity,
        host_core::pair::client_authorizer(),
        // The Windows host is single-client only — a never-set mirror flag.
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?);
    info!("QUIC video host on {bind}; waiting for Trireme to connect…");

    // Optional MQTT / Home-Assistant telemetry — off unless enabled in config.toml.
    let mqtt = start_mqtt(encoder, capture, fps);

    // Input back-channel → SendInput, on its own thread. `try_recv` works
    // outside a tokio runtime; sleep briefly when the queue is empty.
    if let Some(mut rx) = streamer.take_input_rx() {
        std::thread::Builder::new()
            .name("input-inject".into())
            .spawn(move || {
                let injector = input::Injector::new();
                loop {
                    match rx.try_recv() {
                        Ok(ev) => injector.dispatch(&ev),
                        Err(_) => std::thread::sleep(Duration::from_millis(1)),
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
        control_ipc::GpuHealth::Healthy,
    )));
    {
        // Refresh the published status ~4 Hz from the live streamer + frame
        // counter (the Windows analogue of the Linux render loop's status push).
        let (status_r, streamer_r, frames_r, hostname_r, health_r) = (
            status.clone(),
            streamer.clone(),
            frames.clone(),
            hostname.clone(),
            gpu_health.clone(),
        );
        std::thread::Builder::new()
            .name("control-status".into())
            .spawn(move || loop {
                let s = build_status(
                    &hostname_r,
                    encoder_label,
                    codec_label,
                    fps,
                    streamer_r.is_connected(),
                    streamer_r.connected_addr(),
                    streamer_r.connected_pubkey(),
                    frames_r.load(Ordering::Relaxed),
                    health_r.lock().map(|g| g.clone()).unwrap_or_default(),
                );
                if let Ok(mut g) = status_r.lock() {
                    *g = s;
                }
                std::thread::sleep(Duration::from_millis(250));
            })?;
    }
    let ctrl_addr = std::net::SocketAddr::from(([0, 0, 0, 0], core_proto::ports::CONTROL));
    match control_ipc::tcp_bind(ctrl_addr) {
        Ok(listener) => {
            info!("control plane on tcp {ctrl_addr} — panel can poll status");
            control_ipc::serve_status(listener, hostname.clone(), status.clone());
        }
        Err(e) => warn!("control plane bind {ctrl_addr} failed: {e} (status unavailable to panel)"),
    }

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

    'supervisor: loop {
        let pacer_stop = Arc::new(AtomicBool::new(false));
        let (enc, stdout, stderr, pacer) =
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
                    if let Some(f) = cap.recv_timeout(next - now) {
                        last = Some(f.bgra);
                        while let Some(f2) = cap.try_recv() {
                            last = Some(f2.bgra);
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
}

#[cfg(windows)]
impl FaultReason {
    fn describe(&self) -> String {
        match self {
            FaultReason::Eof => "ffmpeg exited (stdout EOF)".to_string(),
            FaultReason::DeviceLoss(r) => format!("device loss: {r}"),
            FaultReason::FaultInjected(r) => r.clone(),
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

/// Build one capture+encode pipeline generation. The SINGLE path used by both the
/// first start and every respawn — its `Err` feeds the supervisor's breaker (no
/// separate first-start bail). Returns the encoder, its stdout/stderr, and (WGC
/// only) the pacer JoinHandle so the supervisor can stop+join it on teardown.
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
)> {
    match capture {
        CaptureBackend::Dda => {
            let mut e = encode::CaptureEncoder::spawn(output_idx, fps, encoder)?;
            let stdout = e
                .take_stdout()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stdout pipe"))?;
            let stderr = e
                .take_stderr()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg produced no stderr pipe"))?;
            Ok((e, stdout, stderr, None))
        }
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
            Ok((e, stdout, stderr, Some(pacer)))
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
            Some(f) => {
                n += 1;
                // Keep the latest frame on disk, so even a single (change-driven)
                // frame yields an inspectable image.
                std::fs::write(out, &f.bgra)?;
                wrote = true;
                if n.is_multiple_of(30) {
                    info!("  {n} frames…");
                }
            }
            None => info!("  no new frame within 1s (WGC is change-driven; idle desktop)"),
        }
    }
    let secs = t0.elapsed().as_secs_f64().max(1e-6);
    info!(
        "captured {n} frames in {secs:.2}s ({:.1} fps); wrote last → {out} (wrote={wrote})",
        n as f64 / secs
    );
    Ok(())
}

#[cfg(windows)]
fn init_tracing() {
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
    use super::{build_status, parse_win_fault_inject, RespawnBreaker};

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
            control_ipc::GpuHealth::Healthy,
        );
        assert!(!s.streaming);
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
