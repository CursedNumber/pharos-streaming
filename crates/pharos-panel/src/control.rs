// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Live control client — the panel's bridge to a running host.
//!
//! The reconnecting client (the worker, the UDS/TCP transport, the cached
//! [`LinkState`]) now lives in the shared [`control_ipc::client`] so the panel
//! and the tray use ONE cross-platform implementation. This module is a thin
//! **panel facade**: it re-exports that client and keeps only the genuinely
//! panel-specific bits — launching a *local* host process, and the i18n string
//! shown when there's no live link.
//!
//! ## Transport (local-first)
//! [`control_ipc::client`] reaches a **local** host by default — a Unix socket
//! on Linux, loopback TCP (`127.0.0.1:15778`) on Windows (no UDS there). Set
//! `PHAROS_CONTROL_ADDR=<host[:port]>` to drive a **remote** host instead (the
//! advanced opt-in). The handle works on Linux *and* Windows now; only truly
//! exotic targets fall back to the no-op handle.

pub use control_ipc::client::{handle, host_supports_v2};
use control_ipc::Request;

/// True when the connected host's control plane is **status-only** — the Windows
/// host's `serve_status`, which answers Hello/GetStatus but `Event::Error`s every
/// mutating request. Tabs gray out their action controls when this is true (so a
/// click can't just bounce); the verb wrappers below also no-op as a backstop.
/// False when no host is connected or the host is fully controllable (the Linux
/// compositor).
pub fn host_is_read_only() -> bool {
    handle().status().map(|s| s.read_only).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Host control verbs — thin wrappers: UI -> shared client -> host. Each no-ops
// against a read-only host so a stray call can't draw an Error reply.
// ---------------------------------------------------------------------------

/// Open the host's pairing window (mint a PIN, start the SPAKE2 listener).
/// `addr` overrides the listen address; `None` uses the host default. No-op if
/// no host is connected (the request is dropped) — the UI reflects that via
/// [`Handle::connected`].
pub fn start_pairing(addr: Option<String>) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::StartPairing { addr });
}

/// Close the host's pairing window.
pub fn cancel_pairing() {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::CancelPairing);
}

/// Ask the running host to restart itself in place (re-exec). The socket drops
/// and the worker reconnects on its own once the host is back.
pub fn relaunch_host() {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::Relaunch);
}

/// Enable/disable the host's live preview (it writes a JPEG ~2fps, auto-stopping
/// after 20s, so the panel re-sends `true` to keep it going).
pub fn set_preview(enabled: bool) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::SetPreview { enabled });
}

/// Enable/disable INPUT forwarding for one device (by hex pubkey), live on the
/// running host (the compositor canvas's green per-device toggle). No-op on a
/// read-only host or for a device with no pinned pubkey (nothing to address).
pub fn set_input_enabled(pubkey: &str, enabled: bool) {
    if host_is_read_only() || pubkey.is_empty() {
        return;
    }
    handle().send_request(Request::SetInputEnabled {
        pubkey: pubkey.to_string(),
        enabled,
    });
}

/// Switch the host's stream fan-out mode. `mirror = true` shares one encode
/// across all clients (clamped to the weakest); `false` is single-client. The
/// host echoes the resulting mode via `HostStatus.stream_mode` on its next poll.
/// No-op on a read-only (status-only) host.
pub fn set_stream_mode(mirror: bool) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::SetStreamMode { mirror });
}

/// Ask the host to forget (unpair) a peer by hex pubkey (issue #19): it
/// rewrites `peers.toml`, closes the peer's live session, and denies future
/// connects (the authorizer re-reads the registry per connection). The
/// Overview's per-card overflow menu drives this after a confirm step. No-op
/// on a read-only host or with no live link (the card's menu is gated on both).
pub fn forget_peer(pubkey: &str) {
    if host_is_read_only() || pubkey.is_empty() {
        return;
    }
    handle().send_request(Request::ForgetPeer {
        pubkey: pubkey.to_string(),
    });
}

/// Ask the host to set (or, with `None`, clear) its PRIMARY client (issue
/// #19). Persisted host-side in `peers.toml` and applied live to the
/// pre-emption policy; the panel sees the result on its next status poll via
/// `HostStatus.primary_pubkey`. No-op on a read-only host.
pub fn set_primary_peer(pubkey: Option<String>) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::SetPrimaryPeer { pubkey });
}

/// Ask the running host to launch a saved app (by `apps.toml` slug) into the
/// virtual display. No-op if no host is connected (the request is dropped); the
/// Apps tab disables the Launch buttons in that case.
pub fn launch_app(slug: impl Into<String>) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::LaunchApp { slug: slug.into() });
}

/// Ask the host to set (or, with `alias = None`, clear) a controller's
/// user-given display name. The host persists it in `controller-profiles.toml`;
/// every panel sees the update on its next `GetStatus` poll (~1 Hz). No-op if no
/// host is connected (the request is dropped).
pub fn set_controller_alias(vendor: u16, product: u16, alias: Option<String>) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::SetControllerAlias {
        vendor,
        product,
        alias,
    });
}

/// Ask the host for a fresh per-stage frame-timings snapshot. The request also
/// **subscribes** the link for ~5s of live instrumentation; while a view keeps
/// calling this on its render tick (~1 Hz) the subscription stays armed. The
/// reply arrives asynchronously as [`control_ipc::Event::FrameTimings`] and is
/// mirrored into [`Handle::frame_timings`] / [`Handle::frame_history`]. No-op if
/// no host is connected.
pub fn request_frame_timings() {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::GetFrameTimings);
}

/// Subscribe the link to live `HostStatus.controllers` updates for `ttl_ms`.
/// While at least one panel keeps re-poking the gate (the Controllers tab calls
/// this on every render tick), the host rebuilds the per-controller liveness
/// vector ~1 Hz; once the gate goes stale the host skips the (expensive) rebuild
/// entirely. No-op if no host is connected.
pub fn subscribe_controllers(ttl_ms: u32) {
    if host_is_read_only() {
        return;
    }
    handle().send_request(Request::SubscribeControllers { ttl_ms });
}

// ---------------------------------------------------------------------------
// Launch a *local* host (the panel runs ON the host machine)
// ---------------------------------------------------------------------------

/// Launch a host process when none is running (the panel runs *on* the host).
///
/// `$PHAROS_HOST_CMD` (whitespace-split, no shell quoting) overrides the command
/// on either platform. Otherwise: on Linux, `pharos-compositor capture-zc-multi`
/// (detached into its own process group) plus the USB/IP import bridge; on
/// Windows, `pharos-host stream` (a sibling `pharos-host.exe`). Returns a human
/// error string on failure.
#[cfg(unix)]
pub fn launch_host() -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    let mut cmd = match host_cmd_override() {
        Some(c) => c,
        None => {
            let mut c = std::process::Command::new(resolve_sibling_bin("pharos-compositor"));
            c.arg("capture-zc-multi");
            c
        }
    };
    // Detach into a fresh process group so a panel exit doesn't SIGHUP the host.
    cmd.process_group(0);
    cmd.spawn().map_err(|e| format!("launch host: {e}"))?;
    // The compositor alone can't import USB devices — bring up the USB/IP import
    // bridge too, so controllers work when launched from the panel.
    start_usbimport_best_effort();
    Ok(())
}

/// Stop the local host: the systemd-run unit (rig launcher) AND any bare
/// compositor the panel spawned directly, plus the USB/IP bridge. Best-effort.
#[cfg(unix)]
pub fn stop_host() {
    use std::process::Command;
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "pharos-host.service"])
        .status();
    let _ = Command::new("pkill")
        .args(["-TERM", "-f", "target/(debug|release)/pharos-compositor"])
        .status();
    let _ = Command::new("sudo")
        .args(["-n", "systemctl", "stop", "pharos-usbimport.service"])
        .status();
}

/// True if a host is up — the control link is reachable, or a compositor process
/// exists (the panel may have spawned one that hasn't bound the socket yet).
#[cfg(unix)]
pub fn host_is_running() -> bool {
    if handle().connected() {
        return true;
    }
    std::process::Command::new("pgrep")
        .args(["-f", "target/(debug|release)/pharos-compositor"])
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start the host if none is running — idempotent, never restarts a live one.
/// Called when the panel loads so the control center brings the host up.
#[cfg(unix)]
pub fn ensure_host_running() {
    if !host_is_running() {
        let _ = launch_host();
    }
}

/// Start the system-tray daemon if it isn't already running, as a transient user
/// unit (mirrors rig/launch-host.sh). Idempotent — called when the panel loads so
/// the tray is part of the GUI suite.
#[cfg(unix)]
pub fn ensure_tray_running() {
    use std::process::{Command, Stdio};
    let running = Command::new("pgrep")
        .args(["-f", "target/(debug|release)/pharos-tray"])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if running {
        return;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed", "pharos-tray.service"])
        .status();
    let _ = Command::new("systemd-run")
        .args([
            "--user",
            "--unit=pharos-tray",
            "-p",
            "Restart=on-failure",
            "-p",
            "RestartSec=2",
        ])
        .arg(resolve_sibling_bin("pharos-tray"))
        .status();
}

/// The host launcher (`<repo>/rig/launch-host.sh`) for the "Start on boot"
/// autostart entry, derived from the build-time repo root.
#[cfg(unix)]
pub fn host_launcher() -> String {
    let raw = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("rig")
        .join("launch-host.sh");
    raw.canonicalize()
        .unwrap_or(raw)
        .to_string_lossy()
        .into_owned()
}

/// Windows: stop the local host process (best-effort), and the USB/IP import
/// bridge with it.
#[cfg(windows)]
pub fn stop_host() {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "pharos-host.exe"])
        .status();
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "pharos-usbimport.exe"])
        .status();
}

/// Windows: spawn the sibling `pharos-host.exe` (the pharos-win binary). The
/// child is independent of the panel by default on Windows, so no process-group
/// detach is needed.
#[cfg(windows)]
pub fn launch_host() -> Result<(), String> {
    let mut cmd = match host_cmd_override() {
        Some(c) => c,
        None => {
            let mut c = std::process::Command::new(resolve_sibling_bin("pharos-host"));
            c.arg("stream");
            c
        }
    };
    cmd.spawn().map_err(|e| format!("launch host: {e}"))?;
    // The host alone can't import USB devices — bring up the USB/IP import
    // bridge too (mirrors the Unix path). It needs no elevation on Windows;
    // usbip-win2 (its driver + usbip.exe) is its own install, which the bridge
    // detects and explains in its log if missing.
    start_usbimport_best_effort();
    Ok(())
}

/// Windows: spawn the sibling `pharos-usbimport.exe`, fire-and-forget. If one
/// is already running the new instance exits on its own (the USB relay port is
/// taken), so this stays idempotent without process scanning. Failures are
/// swallowed — USB import is an optional capability.
#[cfg(windows)]
fn start_usbimport_best_effort() {
    let _ = std::process::Command::new(resolve_sibling_bin("pharos-usbimport")).spawn();
}

/// Fallback for exotic targets: there's no host to launch.
#[cfg(not(any(unix, windows)))]
pub fn launch_host() -> Result<(), String> {
    Err("launching a host is not supported on this platform".into())
}

/// A `Command` from `$PHAROS_HOST_CMD` if it's set + non-empty, else `None`.
#[cfg(any(unix, windows))]
fn host_cmd_override() -> Option<std::process::Command> {
    let line = std::env::var("PHAROS_HOST_CMD").ok()?;
    let line = line.trim();
    let mut parts = line.split_whitespace();
    let prog = parts.next()?;
    let mut c = std::process::Command::new(prog);
    c.args(parts);
    Some(c)
}

/// Best path to a sibling Pharos binary: next to the panel's own exe if present
/// (the normal `target/<profile>/` layout), else the bare name for a `PATH`
/// lookup. Appends `.exe` on Windows.
#[cfg(any(unix, windows))]
fn resolve_sibling_bin(name: &str) -> std::path::PathBuf {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(&exe_name);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(exe_name)
}

/// Bring up `pharos-usbimport` — the USB/IP import bridge that forwards a
/// client's controllers / USB devices into the host — as a transient system
/// unit, exactly the way `rig/launch-host.sh` does. Without it the compositor
/// runs but imported gamepads flap (nothing serves the import port).
///
/// Best-effort: a `--system` unit needs passwordless sudo, and starting an
/// already-running unit is a harmless no-op. Failures are swallowed.
#[cfg(unix)]
fn start_usbimport_best_effort() {
    let bin = resolve_sibling_bin("pharos-usbimport");
    let _ = std::process::Command::new("sudo")
        .args([
            "-n",
            "systemd-run",
            "--system",
            "--unit=pharos-usbimport",
            "-p",
            "Restart=on-failure",
            "-p",
            "RestartSec=2",
            "--setenv=RUST_LOG=info",
        ])
        .arg(&bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// A localised one-liner for the "no live link" condition, shared by tabs that
/// need to explain why live controls are inert. On a real host platform that's
/// "host not running"; on an exotic target with no client, it's "unavailable".
pub fn unavailable_reason() -> String {
    #[cfg(any(unix, windows))]
    {
        crate::i18n::tr("control.not_running")
    }
    #[cfg(not(any(unix, windows)))]
    {
        crate::i18n::tr("control.unavailable")
    }
}
