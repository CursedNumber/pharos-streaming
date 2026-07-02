//! `pharos-compositor` — runs the headless Wayland compositor standalone for
//! testing. Defaults to 1920x1080@60; CLI/pal::Mode wiring comes later.

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // `pharos-compositor capture [out.mp4] [frames]` — render + encode to HEVC.
        Some("capture") => {
            let out = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("pharos-capture.mp4");
            let frames = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120u32);
            compositor::run_capture(1920, 1080, 60, frames, out)
        }
        // `pharos-compositor capture-gl [out.mp4] [frames] [client|none]` — GLES path.
        // Defaults to launching `alacritty` into the compositor so a real window
        // is composited into the capture; pass `none` to capture an empty output.
        Some("capture-gl") => {
            let out = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("pharos-capture-gl.mp4");
            let frames = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120u32);
            let launch = match args.get(4).map(String::as_str) {
                Some("none") => None,
                Some(c) => Some(c),
                None => Some("alacritty"),
            };
            compositor::run_capture_gl(1920, 1080, 60, frames, out, launch)
        }
        // `pharos-compositor capture-zc [out.hevc] [frames] [client|none] [fps]` — full zero-copy.
        // `out` may be `quic://host:port` to stream instead of writing a file.
        // `fps` (default 60) sets the output mode, NVENC framerate, and real-time pacing.
        // This now builds a one-output `MultiCfg` and runs the multi backend;
        // the dedicated single-output entry point was retired so gamepad/gyro
        // and the panel control plane stay on a single code path.
        Some("capture-zc") => {
            let out = args.get(2).map(String::as_str).unwrap_or("pharos-zc.hevc");
            // `frames` arg ignored — `MultiCfg::single_output` uses 0 (= run
            // until interrupted), which matches the resident-host expectation.
            // Kept in the CLI shape so existing scripts don't break.
            let _frames_arg = args.get(3);
            let launch = match args.get(4).map(String::as_str) {
                Some("none") => None,
                Some(c) => Some(c),
                None => Some("alacritty"),
            };
            let fps = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60u32);
            let cfg = compositor::multi::MultiCfg::single_output(1920, 1080, fps, out, launch);
            compositor::multi::run_capture_zc_multi(&cfg, &[])
        }
        // `pharos-compositor input-demo [out.hevc] [frames]` — inject keystrokes
        // into a terminal (alacritty -e cat) and capture the echoed text. Same
        // one-output `MultiCfg` shape as `capture-zc`, just with `inject_keys`.
        Some("input-demo") => {
            let out = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("pharos-input.hevc");
            let _frames_arg = args.get(3);
            // evdev keycodes for "pharos" (no shift → lowercase).
            const KEYS: &[u32] = &[25, 35, 30, 19, 24, 31];
            let cfg = compositor::multi::MultiCfg::single_output(
                1920,
                1080,
                60,
                out,
                Some("alacritty -e cat"),
            );
            compositor::multi::run_capture_zc_multi(&cfg, KEYS)
        }
        // `pharos-compositor pair [addr]` — pairing listener (separate from video).
        Some("pair") => {
            let addr: std::net::SocketAddr = match args.get(2) {
                Some(a) => a.parse()?,
                None => (std::net::Ipv4Addr::UNSPECIFIED, core_proto::ports::PAIR).into(),
            };
            compositor::pair::run(addr)
        }
        // `pharos-compositor capture-zc-multi <config.toml>` — N virtual outputs,
        // one event loop, one shared Wayland socket; each output is its own
        // NVENC session + QUIC stream. See crates/compositor/multi.example.toml.
        Some("capture-zc-multi") => {
            // Default to the panel's saved layout (~/.config/pharos/multi.toml)
            // so `capture-zc-multi` with no arg runs exactly what pharos-panel
            // wrote. An explicit path arg still overrides.
            let path = match args.get(2) {
                Some(p) => std::path::PathBuf::from(p),
                None => compositor::pair::config_dir()?.join("multi.toml"),
            };
            let cfg = compositor::multi::load_multi_config(&path)?;
            compositor::multi::run_capture_zc_multi(&cfg, &[])
        }
        // `pharos-compositor launch-app <slug>` — ask the *running* host to launch
        // a saved app (apps.toml slug) into the virtual display. A thin control-ipc
        // client; the heavy lifting lives in the host's render loop + app-launcher.
        Some("launch-app") => {
            let slug = match args.get(2) {
                Some(s) => s.clone(),
                None => {
                    eprintln!("usage: pharos-compositor launch-app <slug>");
                    eprintln!("       (list slugs with: pharos-compositor apps)");
                    std::process::exit(2);
                }
            };
            launch_app_cli(&slug)
        }
        // `pharos-compositor apps` — list the saved apps (slug + name) from
        // apps.toml so the CLI/scripts know what `launch-app` accepts. Reads the
        // config directly; no running host required.
        Some("apps") => {
            let catalog = app_launcher::config::load_with_builtins();
            for app in &catalog.apps {
                let kind = format!("{:?}", app.kind).to_lowercase();
                println!("{:<24} {:<8} {}", app.slug, kind, app.name);
            }
            Ok(())
        }
        // `pharos-compositor nvenc-test [out.hevc] [frames]` — direct NVENC-from-CUDA.
        Some("nvenc-test") => {
            let out = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("pharos-nvenc.hevc");
            let frames = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120u32);
            compositor::nvenc::encode_test_pattern(1920, 1080, 60, frames, out)
        }
        // `pharos-compositor nvenc-test-hdr [out.hevc] [frames]` — 10-bit HEVC
        // Main10 (BT.2020/PQ + HDR10 SEI) from a CPU-filled ABGR10 test pattern.
        // Isolates the HDR encoder config from the GL 10-bit interop.
        Some("nvenc-test-hdr") => {
            let out = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("pharos-nvenc-hdr10.hevc");
            let frames = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120u32);
            compositor::nvenc::encode_test_pattern_hdr10(1920, 1080, 60, frames, out)
        }
        _ => compositor::run_headless(1920, 1080, 60_000),
    }
}

/// `launch-app <slug>`: connect to the running host's control socket, ask it to
/// launch the app, print the outcome. Errors if no host is running or the host
/// refuses (bad slug / spawn failure), so scripts get a non-zero exit.
#[cfg(target_os = "linux")]
fn launch_app_cli(slug: &str) -> anyhow::Result<()> {
    use control_ipc::{Event, Request, PROTOCOL_VERSION};
    use std::io::BufReader;

    let stream = control_ipc::connect().map_err(|e| {
        anyhow::anyhow!("can't reach the host control socket ({e}); is pharos-compositor running?")
    })?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // Handshake so the host logs a sane peer; we don't act on the reply.
    control_ipc::write_request(
        &mut writer,
        &Request::Hello {
            version: PROTOCOL_VERSION,
        },
    )?;
    let _ = control_ipc::read_event(&mut reader)?;

    control_ipc::write_request(
        &mut writer,
        &Request::LaunchApp {
            slug: slug.to_string(),
        },
    )?;
    match control_ipc::read_event(&mut reader)? {
        Some(Event::Ack { ok: true, .. }) => {
            println!("launched `{slug}`");
            Ok(())
        }
        Some(Event::Ack { ok: false, msg }) => {
            anyhow::bail!(
                "host refused: {}",
                msg.unwrap_or_else(|| "unknown error".into())
            )
        }
        Some(Event::Error { msg }) => anyhow::bail!("host error: {msg}"),
        other => anyhow::bail!("unexpected reply from host: {other:?}"),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pharos-compositor is Linux-only (Smithay/Wayland).");
}
