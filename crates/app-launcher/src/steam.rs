// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Steam readiness probe + pre-warm.
//!
//! "Ready" means the Steam client is past startup and will actually honour a
//! `steam://` URI handoff — checking that a process exists isn't enough, because
//! a cold-starting Steam will silently drop URIs until its IPC pipe is up. The
//! launcher uses [`is_ready`] + [`prewarm`] + [`wait_ready`] to block a Steam
//! launch just long enough to dodge the documented cold-start NVENC SIGSEGV
//! without permanently coupling the render loop to Steam startup.

use std::time::{Duration, Instant};

/// Is Steam ready to accept a `steam://` URL handoff right now?
pub fn is_ready() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::is_ready()
    }
    #[cfg(target_os = "windows")]
    {
        windows::is_ready()
    }
    #[cfg(target_os = "macos")]
    {
        macos::is_ready()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        true
    }
}

/// Start Steam in the background ("pre-warm"). Non-blocking — pair with [`wait_ready`].
pub fn prewarm() {
    #[cfg(target_os = "linux")]
    {
        linux::prewarm();
    }
    #[cfg(target_os = "windows")]
    {
        windows::prewarm();
    }
    #[cfg(target_os = "macos")]
    {
        macos::prewarm();
    }
}

/// Block (synchronously) until [`is_ready`] returns true, or `timeout` elapses.
/// Returns `true` if Steam became ready, `false` on timeout.
pub fn wait_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let poll = Duration::from_millis(250);
    loop {
        if is_ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// Move a spawned `Child` onto a tiny reaper thread that wait()s on it once.
/// Without this, a pre-warm spawn that exits quickly (Linux `setsid -f`, macOS
/// `open`) becomes a zombie until the host exits; on Windows the reaper just
/// lives as long as Steam itself, which is fine. Best-effort — if the thread
/// itself can't spawn, we accept the (rare) zombie rather than blocking.
#[allow(dead_code)] // referenced from per-OS modules; cfg gating hides it on others
fn reap_in_background(child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("steam-prewarm-reaper".to_string())
        .spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::{Command, Stdio};

    pub(super) fn is_ready() -> bool {
        let Some(home) = dirs::home_dir() else {
            return false;
        };
        let steam_dir = home.join(".steam");
        // steam.pipe is a FIFO; Path::exists() still resolves it.
        if !steam_dir.join("steam.pipe").exists() {
            return false;
        }
        let pid_path = steam_dir.join("steam.pid");
        let Ok(raw) = std::fs::read_to_string(&pid_path) else {
            return false;
        };
        let Ok(pid) = raw.trim().parse::<u32>() else {
            return false;
        };
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return false;
        }
        // PID-recycle belt-and-braces: confirm /proc/<pid>/comm actually names
        // the Steam client. Without this, an unrelated process that happens to
        // reuse the recorded pid would look "ready" even though Steam is gone.
        let comm_path = format!("/proc/{pid}/comm");
        match std::fs::read_to_string(&comm_path) {
            Ok(comm) => comm.trim() == "steam",
            Err(_) => false,
        }
    }

    pub(super) fn prewarm() {
        let mut cmd = Command::new("setsid");
        cmd.args(["-f", "steam", "-silent", "-nofriendsui", "-noconsole"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Route Steam (and therefore every game it launches) into the session's
        // isolated sink, so game audio is captured for the stream. We start
        // Steam here, so this is the reliable path; the compositor's reconciler
        // is the fallback for a Steam already running outside our control.
        if let Some(sink) = crate::launch::session_audio_sink() {
            cmd.env("PULSE_SINK", sink);
        }
        let spawn = cmd.spawn();
        match spawn {
            Ok(child) => super::reap_in_background(child),
            Err(e) => tracing::warn!("steam::prewarm: failed to spawn setsid steam: {e}"),
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::process::{Command, Stdio};
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    pub(super) fn is_ready() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(key) = hkcu.open_subkey("Software\\Valve\\Steam\\ActiveProcess") else {
            return false;
        };
        let pid: u32 = match key.get_value("pid") {
            Ok(v) => v,
            Err(_) => return false,
        };
        pid != 0
    }

    /// Resolve a Steam.exe to spawn, trying (in order):
    ///   1. HKCU\Software\Valve\Steam\SteamPath
    ///   2. HKLM\Software\WOW6432Node\Valve\Steam\InstallPath
    ///   3. %ProgramFiles(x86)%\Steam\Steam.exe
    ///   4. %ProgramFiles%\Steam\Steam.exe
    ///   5. bare "Steam.exe" (rely on %PATH%)
    ///
    /// The debug log records which path won so a wrong guess is recoverable.
    fn resolve_steam_exe() -> (String, &'static str) {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Some(p) = hkcu
            .open_subkey("Software\\Valve\\Steam")
            .ok()
            .and_then(|k| k.get_value::<String, _>("SteamPath").ok())
        {
            return (format!("{p}\\Steam.exe"), "HKCU SteamPath");
        }
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Some(p) = hklm
            .open_subkey("Software\\WOW6432Node\\Valve\\Steam")
            .ok()
            .and_then(|k| k.get_value::<String, _>("InstallPath").ok())
        {
            return (format!("{p}\\Steam.exe"), "HKLM InstallPath");
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            let candidate = format!("{pf86}\\Steam\\Steam.exe");
            if std::path::Path::new(&candidate).exists() {
                return (candidate, "%ProgramFiles(x86)%");
            }
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            let candidate = format!("{pf}\\Steam\\Steam.exe");
            if std::path::Path::new(&candidate).exists() {
                return (candidate, "%ProgramFiles%");
            }
        }
        ("Steam.exe".to_string(), "PATH fallback")
    }

    pub(super) fn prewarm() {
        let (exe, source) = resolve_steam_exe();
        tracing::debug!("steam::prewarm: resolved Steam.exe via {source}: {exe}");
        let spawn = Command::new(&exe)
            .args(["-silent", "-nofriendsui", "-noconsole"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawn {
            Ok(child) => super::reap_in_background(child),
            Err(e) => tracing::warn!("steam::prewarm: failed to spawn {exe}: {e}"),
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::process::{Command, Stdio};

    pub(super) fn is_ready() -> bool {
        // Preferred probe: the historical macOS binary name.
        let strict = Command::new("pgrep")
            .args(["-x", "steam_osx"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if strict {
            tracing::debug!("steam::is_ready(macos): matched via `pgrep -x steam_osx`");
            return true;
        }
        // Fallback: case-insensitive match catches "Steam", "steam_arm64", etc.
        let lax = Command::new("pgrep")
            .args(["-ix", "steam"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if lax {
            tracing::debug!("steam::is_ready(macos): matched via `pgrep -ix steam` fallback");
        }
        lax
    }

    pub(super) fn prewarm() {
        let spawn = Command::new("open")
            .args(["-ga", "Steam", "--args", "-silent", "-nofriendsui"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawn {
            Ok(child) => super::reap_in_background(child),
            Err(e) => tracing::warn!("steam::prewarm: failed to open Steam: {e}"),
        }
    }
}
