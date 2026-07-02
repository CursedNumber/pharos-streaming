//! Seamless redirect of Pharos-launched Steam games into the virtual display.
//!
//! The problem: `steam://rungameid/<id>` is delivered to whatever Steam is already
//! running. When that's a *desktop* Steam, it spawns the game in the desktop's
//! environment (`WAYLAND_DISPLAY=wayland-0`), ignoring the socket Pharos set on the
//! handoff command — so a streamed game lands on the host desktop, blank on the
//! stream. Env passed via the URI is a dead end; only Steam's per-game
//! **Launch Options** env reaches a game a running Steam forks.
//!
//! The fix, matching the pattern users already apply by hand
//! (`env WAYLAND_DISPLAY=… %command%`): wrap each game's Launch Options **once** in
//! a conditional shim — `pharos-redirect %command%`. The shim reads a *marker* the
//! compositor writes only while a client is actively streaming, and either
//! re-execs the game into the live stream env or (no/stale marker) passes straight
//! through to the desktop. So the baked Launch Options never pin a socket and never
//! preclude desktop play; only the marker toggles behaviour.
//!
//! Editing `localconfig.vdf` is opt-in (`PHAROS_STEAM_REDIRECT`) and safe:
//! idempotent, empty-or-ours-only, backed up, atomic, and **never written while
//! Steam is running** (Steam holds the config in memory and would clobber the file
//! on exit — see [`linux`]).
//!
//! Cross-platform seam: [`RedirectProvider`] is platform-dispatched. Linux ships
//! the Launch-Options shim + `$XDG_RUNTIME_DIR` marker; a future Windows host
//! (pharos-win) plugs a `.cmd` shim + `%LOCALAPPDATA%` marker into the same
//! contract without touching the injection logic.

use std::path::PathBuf;

// The VDF editor is only used by the unix launch-options injector; gating it with
// `linux` keeps the Windows build free of dead-code warnings until a native
// Windows provider lands.
#[cfg(unix)]
mod vdf;

#[cfg(unix)]
mod linux;

use crate::env::SteamInstall;

/// The Steam game to redirect. An enum later if non-Steam kinds gain redirect.
pub struct RedirectId {
    /// Steam appid.
    pub appid: u32,
}

/// Result of an idempotent Launch-Options injection.
#[derive(Debug, PartialEq, Eq)]
pub enum InjectOutcome {
    /// The shim was written into the game's (empty) Launch Options.
    Injected,
    /// Already ours — nothing to do.
    AlreadyOurs,
    /// Steam is running; writing now would be clobbered on exit. Recorded as
    /// pending — it arms on the next Steam start (which Pharos itself does when it
    /// cold-launches Steam into the stream).
    Deferred,
    /// Left untouched: a user-authored Launch Options value, an ambiguous
    /// multi-user config, or the appid absent from localconfig.
    Refused(String),
    /// Steam isn't installed / no data root found.
    NoSteam,
}

/// Is the seamless-redirect feature enabled? Opt-in via `PHAROS_STEAM_REDIRECT`,
/// mirroring the `PHAROS_HDR` / `PHAROS_ENABLE_COLOR_MGMT` dev gates — editing a
/// user's Steam config should never happen without explicit opt-in.
pub fn enabled() -> bool {
    std::env::var_os("PHAROS_STEAM_REDIRECT").is_some_and(|v| v != "0")
}

/// Platform-specific redirect provider (Launch-Options shim + marker).
pub trait RedirectProvider {
    /// Idempotently ensure the shim exists on disk (content-diff; `0755` on unix).
    fn ensure_shim_installed(&self) -> anyhow::Result<()>;
    /// The "ours" token — the shim basename — for empty-or-ours detection.
    fn ours_token(&self) -> &str;
    /// Idempotently inject `<shim> %command%` into this game's Launch Options.
    fn inject(&self, install: &SteamInstall, id: RedirectId) -> anyhow::Result<InjectOutcome>;
}

/// The active platform provider.
pub fn provider() -> Box<dyn RedirectProvider> {
    #[cfg(unix)]
    {
        Box::new(linux::LinuxShimProvider)
    }
    #[cfg(not(unix))]
    {
        Box::new(NoopProvider)
    }
}

/// Windows/other placeholder until a native provider lands — the seam compiles
/// everywhere so the compositor call sites are platform-agnostic.
#[cfg(not(unix))]
struct NoopProvider;
#[cfg(not(unix))]
impl RedirectProvider for NoopProvider {
    fn ensure_shim_installed(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn ours_token(&self) -> &str {
        "pharos-redirect"
    }
    fn inject(&self, _install: &SteamInstall, _id: RedirectId) -> anyhow::Result<InjectOutcome> {
        Ok(InjectOutcome::NoSteam)
    }
}

// ── Marker: written by the compositor while a stream is active ───────────────

/// The environment the shim exports into a redirected game. Rendered by the
/// compositor into the marker and re-sourced by the shim — one definition so the
/// two can never drift.
#[derive(Debug, Clone)]
pub struct MarkerEnv {
    /// Compositor pid (staleness guard: the shim checks `/proc/<pid>`).
    pub pid: u32,
    /// Compositor `/proc/self/stat` start-ticks (PID-reuse guard).
    pub start_ticks: u64,
    /// The live Wayland socket basename, e.g. `wayland-1` — the shim verifies the
    /// socket file still exists before redirecting.
    pub socket: String,
    /// `WAYLAND_DISPLAY` to export (usually == `socket`).
    pub wayland_display: String,
    /// The session audio sink, exported as `PULSE_SINK` (if any).
    pub pulse_sink: Option<String>,
    /// Whether to force `PROTON_ENABLE_WAYLAND=1` for the redirected game.
    pub proton_enable_wayland: bool,
}

impl MarkerEnv {
    /// A marker for the *current* process (the compositor): fills `pid` +
    /// `start_ticks` from `/proc/self`.
    pub fn for_current(socket: impl Into<String>, pulse_sink: Option<String>) -> Self {
        let socket = socket.into();
        MarkerEnv {
            pid: std::process::id(),
            start_ticks: self_start_ticks(),
            wayland_display: socket.clone(),
            socket,
            pulse_sink,
            proton_enable_wayland: true,
        }
    }

    /// Render the sourceable POSIX-sh marker fragment. Values single-quoted with
    /// embedded `'` escaped (`'\''`), so any socket/sink name is safe.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&kv("PHAROS_MARK_PID", &self.pid.to_string()));
        s.push_str(&kv("PHAROS_MARK_START", &self.start_ticks.to_string()));
        s.push_str(&kv("PHAROS_MARK_SOCKET", &self.socket));
        s.push_str(&kv("PHAROS_WAYLAND_DISPLAY", &self.wayland_display));
        if let Some(sink) = &self.pulse_sink {
            s.push_str(&kv("PHAROS_PULSE_SINK", sink));
        }
        s.push_str(&kv(
            "PHAROS_PROTON_ENABLE_WAYLAND",
            if self.proton_enable_wayland { "1" } else { "0" },
        ));
        s
    }
}

fn kv(k: &str, v: &str) -> String {
    format!("{k}='{}'\n", v.replace('\'', "'\\''"))
}

/// `$XDG_RUNTIME_DIR/pharos/launch-env` — per-user, `0700`, tmpfs (auto-cleared on
/// logout, so a stale marker can't outlive a session).
pub fn marker_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("pharos").join("launch-env")
}

/// Atomically write the marker (temp + rename), creating the `pharos/` dir.
pub fn write_marker(m: &MarkerEnv) -> std::io::Result<()> {
    let path = marker_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, m.render())?;
    std::fs::rename(&tmp, &path)
}

/// Remove the marker (ignore if already gone). Desktop launches then pass through.
pub fn remove_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// Clear any orphan marker at compositor startup (a crashed prior run). Belt to
/// the shim's own pid+start-ticks staleness braces.
pub fn sweep_stale_marker() {
    remove_marker();
}

/// This process's start-ticks (`/proc/self/stat` field 22), for the marker's
/// PID-reuse guard. `0` off-Linux (no `/proc`) — the shim's socket-exists check
/// still gates redirects there.
fn self_start_ticks() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| {
                let rparen = s.rfind(')')?;
                s[rparen + 1..]
                    .split_whitespace()
                    .nth(19)
                    .and_then(|t| t.parse::<u64>().ok())
            })
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_renders_sourceable_fragment() {
        let m = MarkerEnv {
            pid: 42,
            start_ticks: 9000,
            socket: "wayland-1".into(),
            wayland_display: "wayland-1".into(),
            pulse_sink: Some("pharos_stream".into()),
            proton_enable_wayland: true,
        };
        let r = m.render();
        assert!(r.contains("PHAROS_MARK_PID='42'\n"));
        assert!(r.contains("PHAROS_WAYLAND_DISPLAY='wayland-1'\n"));
        assert!(r.contains("PHAROS_PULSE_SINK='pharos_stream'\n"));
        assert!(r.contains("PHAROS_PROTON_ENABLE_WAYLAND='1'\n"));
    }

    #[test]
    fn marker_single_quote_escaped() {
        let m = MarkerEnv {
            pid: 1,
            start_ticks: 0,
            socket: "s".into(),
            wayland_display: "w".into(),
            pulse_sink: Some("wei'rd".into()),
            proton_enable_wayland: false,
        };
        assert!(m.render().contains("PHAROS_PULSE_SINK='wei'\\''rd'\n"));
    }

    #[test]
    fn enabled_reflects_env() {
        // Not asserting the global env; just that the helper reads the var name.
        // (Cargo runs tests without PHAROS_STEAM_REDIRECT.)
        assert!(!enabled() || std::env::var_os("PHAROS_STEAM_REDIRECT").is_some());
    }
}
