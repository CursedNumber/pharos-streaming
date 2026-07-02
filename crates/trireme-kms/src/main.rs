//! `trireme-kms` — headless atomic DRM/KMS scanout client.
//!
//! Runs on a display-less box that owns an HDMI output (e.g. a headless GPU box
//! with the GTX 1080 driving the Bravia). Receives the Pharos HEVC stream over QUIC,
//! decodes it with ffmpeg (HW decoder probed at runtime — NVDEC `hevc_cuvid` on
//! the GTX 1080 rig, `hevc_rkmpp`/V4L2 on ARM SBCs), and presents each frame to the
//! connector via the **atomic** DRM API — vblank-locked page-flips. There is no
//! Wayland/X display server here, so `trireme play` (winit/wgpu) is the wrong
//! tool; this binary talks to KMS directly.
//!
//! Usage:
//!   trireme-kms gl-test [card] [WxH@R] [frames]         # prove the 120 Hz path
//!   trireme-kms test-pattern [card] [WxH@R] [frames]    # dumb-buffer path (~60 Hz)
//!   trireme-kms play <host:port> [card] [WxH@R]         # live stream → screen
//!
//! `card` defaults to /dev/dri/card1; mode defaults to the highest-refresh
//! 1920x1080 the connector offers (1080p120 on the Bravia). `play` also reads an
//! optional `trireme-kms.toml` (see trireme-kms.toml.example) — e.g.
//! `idle_badge = false` for a plain black idle screen.

#[cfg(target_os = "linux")]
mod badge;
#[cfg(target_os = "linux")]
mod gl_present;
#[cfg(target_os = "linux")]
mod kms;
#[cfg(target_os = "linux")]
mod pipeline;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;

    init_tracing();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gl-test") => {
            let card = args.get(2).map(String::as_str).unwrap_or("/dev/dri/card1");
            let pref = parse_mode(args.get(3))?;
            let frames = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1200);
            pipeline::run_gl_test(card, pref, frames)
        }
        Some("test-pattern") => {
            let card = args.get(2).map(String::as_str).unwrap_or("/dev/dri/card1");
            let pref = parse_mode(args.get(3))?;
            let frames = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1200);
            pipeline::run_test_pattern(card, pref, frames)
        }
        Some("play") => {
            let addr: std::net::SocketAddr = match args.get(2) {
                Some(a) => a.parse().context("parse <host:port>")?,
                None => (std::net::Ipv4Addr::UNSPECIFIED, core_proto::ports::STREAM).into(),
            };
            let card = args.get(3).map(String::as_str).unwrap_or("/dev/dri/card1");
            let pref = parse_mode(args.get(4))?;
            pipeline::run(addr, card, pref)
        }
        Some("blank") => {
            let card = args.get(2).map(String::as_str).unwrap_or("/dev/dri/card1");
            let pref = parse_mode(args.get(3))?;
            pipeline::run_blank(card, pref)
        }
        // `trireme-kms badge-dump <out.raw> [logo_px]` — render the idle badge to a
        // raw RGBA file (prints WxH) for previewing the artwork off-device.
        Some("badge-dump") => {
            let out = args.get(2).map(String::as_str).unwrap_or("/tmp/badge.raw");
            let px = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(280);
            let b = badge::build(px);
            std::fs::write(out, &b.rgba).context("write badge")?;
            println!("{}x{} -> {out}", b.w, b.h);
            Ok(())
        }
        _ => {
            eprintln!("usage: trireme-kms <gl-test|test-pattern|play|blank> [args..]");
            eprintln!("  trireme-kms gl-test [card] [WxH@R] [frames]");
            eprintln!("  trireme-kms test-pattern [card] [WxH@R] [frames]");
            eprintln!("  trireme-kms play <host:port> [card] [WxH@R]");
            eprintln!("  trireme-kms blank [card] [WxH@R]");
            std::process::exit(2);
        }
    }
}

#[cfg(target_os = "linux")]
fn parse_mode(arg: Option<&String>) -> anyhow::Result<Option<kms::ModePref>> {
    match arg {
        Some(s) => Ok(Some(kms::ModePref::parse(s)?)),
        None => Ok(None),
    }
}

#[cfg(target_os = "linux")]
fn init_tracing() {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .init();
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("trireme-kms is Linux-only (DRM/KMS).");
    std::process::exit(1);
}
