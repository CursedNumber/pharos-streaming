// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! `pharos-tray` — a three-state lighthouse system-tray daemon for the Pharos
//! host:
//!
//! - **Idle** — the host's control plane isn't reachable. Plain silhouette.
//! - **Connected** — host is up but no client is pulling frames. Amber.
//! - **Streaming** — at least one client is actively reading frames. Green.
//!
//! Right-click → "Open Panel" (only when a `pharos-panel` binary is resolvable)
//! and "Exit Pharos" (stops the host).
//!
//! Cross-platform: this file is the shared core — the state source (the shared
//! [`control_ipc::client`]), the SVG→RGBA icon rasterizer, and the menu actions.
//! Only the tray DISPLAY backend is per-OS: freedesktop StatusNotifierItem
//! (`ksni`/D-Bus) on Linux ([`linux`]), the Win32 notification area (`tray-icon`
//! + a `tao` message pump) on Windows ([`windows`]).

use std::path::{Path, PathBuf};

use anyhow::Result;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pharos_tray=info,info".into()),
        )
        .init();

    #[cfg(target_os = "linux")]
    {
        linux::run()
    }
    #[cfg(windows)]
    {
        windows::run()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        eprintln!("pharos-tray: no system-tray backend for this platform.");
        Ok(())
    }
}

// The lighthouse SVGs are embedded (no hicolor install needed), now from the
// shared `pharos-assets` crate — one source for the tray + the compositor's idle
// screen, rasterized at runtime. See [`State::svg`].

/// Poll cadence. Generous: a tray only needs to flicker on state changes, so 2s
/// keeps CPU near zero.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Host (compositor) not running — grey, unlit, no beams.
    Offline,
    /// Host up, no client streaming — green lantern, grey beams. (Panel: "Idle".)
    Idle,
    /// Host up, a client is pulling frames — green lantern, green beams.
    Streaming,
    /// GPU degraded — still serving, GPU strained. Green lantern, red beams.
    Degraded,
    /// GPU faulted — host restarting or stopped. Sticky red.
    Faulted,
}

impl State {
    fn tooltip(self) -> &'static str {
        match self {
            State::Offline => "Pharos: host not running",
            State::Idle => "Pharos: idle",
            State::Streaming => "Pharos: streaming",
            State::Degraded => "Pharos: GPU degraded",
            State::Faulted => "Pharos: GPU fault",
        }
    }
    fn svg(self) -> &'static [u8] {
        match self {
            State::Offline => pharos_assets::PHAROS_OFFLINE,
            State::Idle => pharos_assets::PHAROS_IDLE,
            State::Streaming => pharos_assets::PHAROS_STREAMING,
            State::Degraded => pharos_assets::PHAROS_DEGRADED,
            State::Faulted => pharos_assets::PHAROS_FAULTED,
        }
    }

    /// Pure mapping from a control-link snapshot to a tray state — factored out of
    /// [`detect_state`] so it's unit-testable without a live control plane. GPU
    /// health takes priority over streaming/idle so a fault is never hidden
    /// behind a "Streaming" icon.
    fn from_status(
        connected: bool,
        status: Option<&control_ipc::HostStatus>,
        faulted_offline: bool,
    ) -> State {
        if !connected {
            // A durable fault file while the control plane is down = the host
            // faulted and exited (or the breaker tripped) → red, distinct from a
            // host that was simply never started.
            return if faulted_offline {
                State::Faulted
            } else {
                State::Offline
            };
        }
        match status {
            Some(s) => match &s.gpu_health {
                control_ipc::GpuHealth::Faulted { .. } => State::Faulted,
                control_ipc::GpuHealth::Degraded { .. } => State::Degraded,
                control_ipc::GpuHealth::Healthy if s.streaming => State::Streaming,
                // Host up, no client streaming — matches the panel's "Idle".
                control_ipc::GpuHealth::Healthy => State::Idle,
            },
            // Connected but no status snapshot yet (handshake in flight) → Idle.
            None => State::Idle,
        }
    }
}

/// The host state, read from the shared control client's cached link — `Idle`
/// when the control plane isn't reachable, `Faulted`/`Degraded` when the latest
/// `HostStatus.gpu_health` says so, `Streaming` when it's streaming, else
/// `Connected`. The client maintains the connection + ~1 Hz poll on its own
/// thread (UDS on Linux, loopback TCP on Windows), so this is a cheap,
/// non-blocking, cross-platform cache read.
fn detect_state() -> State {
    let h = control_ipc::client::handle();
    let connected = h.connected();
    let status = h.status();
    // Only consult the durable fault file when the control plane is unreachable
    // (a live host reports its health over the socket).
    let faulted_offline = !connected && control_ipc::read_gpu_fault().is_some();
    State::from_status(connected, status.as_ref(), faulted_offline)
}

/// One rasterized icon as straight RGBA (premultiplied, per tiny-skia). The
/// Linux backend byte-swaps this into SNI's ARGB; the Windows backend feeds it
/// to `tray_icon::Icon::from_rgba` directly.
struct RgbaIcon {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Render an SVG to `size`×`size` RGBA via the shared [`pharos_assets`] rasterizer.
fn rasterize_rgba(svg: &[u8], size: u32) -> Result<RgbaIcon> {
    let img = pharos_assets::rasterize_square(svg, size)?;
    Ok(RgbaIcon {
        width: img.width,
        height: img.height,
        rgba: img.rgba,
    })
}

/// Find the `pharos-panel` binary so "Open Panel" only shows when it would do
/// something. Prefers a sibling of this exe (a real install — appends `.exe` on
/// Windows), then the in-repo dev `target/{release,debug}` paths.
fn find_panel() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "pharos-panel.exe"
    } else {
        "pharos-panel"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(exe_name);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    // In-repo dev fallback: ../../target/{release,debug}/pharos-panel[.exe].
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    for sub in ["target/release", "target/debug"] {
        let p = repo.join(sub).join(exe_name);
        if p.is_file() {
            return Some(p);
        }
    }
    // Linux dev: the launcher script does a freshest-by-mtime pick.
    #[cfg(target_os = "linux")]
    {
        let launcher = repo.join("rig/launch-panel.sh");
        if launcher.is_file() {
            return Some(launcher);
        }
    }
    None
}

/// Spawn the resolved panel binary.
///
/// The spawned `Child` is handed to a detached waiter thread that `wait()`s it, so
/// pharos-tray reaps the panel when the user closes it instead of leaking a
/// `<defunct>` zombie on every open/close cycle. A thread-per-open is cheap (the
/// panel is opened occasionally, and the thread blocks in `wait` then exits) and
/// needs no shared state; it's cross-platform (`wait` is a harmless no-op re:
/// zombies on Windows, which has no zombie processes).
fn open_panel(path: &Path) {
    tracing::info!("opening pharos-panel: {}", path.display());
    match std::process::Command::new(path).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("failed to launch pharos-panel: {e}"),
    }
}

/// The host launcher (`<repo>/rig/launch-host.sh`), used as the autostart `Exec`
/// for "Start on boot". Derived from the build-time repo root (same approach as
/// [`find_panel`]'s in-repo fallback) and canonicalised when present.
#[cfg(target_os = "linux")]
fn host_launcher() -> String {
    let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("rig")
        .join("launch-host.sh");
    raw.canonicalize()
        .unwrap_or(raw)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod state_tests {
    use super::State;
    use control_ipc::{GpuHealth, HostStatus};

    fn status(streaming: bool, gpu: GpuHealth) -> HostStatus {
        HostStatus {
            hostname: "rig".into(),
            streaming,
            clients: vec![],
            outputs: vec![],
            media: None,
            pairing: None,
            controllers: vec![],
            video_codec: None,
            read_only: false,
            gpu_health: gpu,
            stream_mode: control_ipc::StreamMode::Single,
            hdr_active: None,
            primary_pubkey: None,
        }
    }

    #[test]
    fn disconnected_with_no_fault_is_offline() {
        assert_eq!(State::from_status(false, None, false), State::Offline);
        // Even a streaming snapshot is Offline if the control link is down.
        let s = status(true, GpuHealth::Healthy);
        assert_eq!(State::from_status(false, Some(&s), false), State::Offline);
    }

    #[test]
    fn disconnected_with_a_fault_file_is_faulted() {
        // Host down + a durable fault marker = "faulted & gave up", not Offline.
        assert_eq!(State::from_status(false, None, true), State::Faulted);
    }

    #[test]
    fn healthy_maps_streaming_and_idle() {
        assert_eq!(
            State::from_status(true, Some(&status(true, GpuHealth::Healthy)), false),
            State::Streaming
        );
        assert_eq!(
            State::from_status(true, Some(&status(false, GpuHealth::Healthy)), false),
            State::Idle
        );
        // Connected but no snapshot yet → Idle (handshake in flight).
        assert_eq!(State::from_status(true, None, false), State::Idle);
    }

    #[test]
    fn gpu_health_takes_priority_over_streaming() {
        let degraded = status(
            true,
            GpuHealth::Degraded {
                reason: "encode choke".into(),
                throttle: Some("thermal".into()),
            },
        );
        assert_eq!(
            State::from_status(true, Some(&degraded), false),
            State::Degraded
        );
        let faulted = status(
            true,
            GpuHealth::Faulted {
                reason: "device lost".into(),
                last_xid: Some(79),
            },
        );
        assert_eq!(
            State::from_status(true, Some(&faulted), false),
            State::Faulted
        );
    }
}
