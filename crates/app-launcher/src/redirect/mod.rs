// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

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

// The VDF editors are only used by the unix injectors; gating them with `unix`
// keeps the Windows build free of dead-code warnings until a native Windows
// provider lands.
#[cfg(unix)]
mod vdf;

#[cfg(unix)]
mod proton;

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

/// Is the Proton auto-pin enabled? A *separate* opt-in
/// (`PHAROS_STEAM_PIN_PROTON`) layered on top of the redirect, since it edits a
/// second file (`config.vdf`) and changes which Proton a game runs under — a
/// bigger commitment than wrapping Launch Options. Only ever consulted for a game
/// Pharos is already redirecting.
pub fn pin_proton_enabled() -> bool {
    std::env::var_os("PHAROS_STEAM_PIN_PROTON").is_some_and(|v| v != "0")
}

/// Outcome of an idempotent `CompatToolMapping` (Proton) pin.
#[derive(Debug, PartialEq, Eq)]
pub enum PinOutcome {
    /// The named tool was pinned for the appid (inserted or overrode a Valve
    /// default).
    Pinned(String),
    /// The appid already maps to our target tool — nothing to do.
    AlreadyPinned,
    /// Steam is running; a `config.vdf` write would be clobbered on exit.
    /// Recorded as pending — it arms on the next Steam start.
    Deferred,
    /// Left untouched: the user forced a specific custom Proton, or the config
    /// couldn't be navigated. Carries a reason.
    Refused(String),
    /// No winewayland-capable Proton is installed to pin.
    NoSuitableProton,
    /// Steam isn't installed / no data root found.
    NoSteam,
}

/// Platform-specific redirect provider (Launch-Options shim + marker).
pub trait RedirectProvider {
    /// Idempotently ensure the shim exists on disk (content-diff; `0755` on unix).
    fn ensure_shim_installed(&self) -> anyhow::Result<()>;
    /// The "ours" token — the shim basename — for empty-or-ours detection.
    fn ours_token(&self) -> &str;
    /// Idempotently inject `<shim> %command%` into this game's Launch Options.
    fn inject(&self, install: &SteamInstall, id: RedirectId) -> anyhow::Result<InjectOutcome>;
    /// Idempotently pin a known-good, winewayland-capable Proton for this game in
    /// `config.vdf`'s `CompatToolMapping` (see [`pin_proton_enabled`]). Only fills
    /// in an absent mapping or overrides a Wayland-breaking Valve default; never
    /// touches a Proton the user explicitly forced. Same Steam-closed clobber
    /// guard + backup as [`RedirectProvider::inject`].
    fn pin_proton(&self, install: &SteamInstall, id: RedirectId) -> anyhow::Result<PinOutcome>;
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
    fn pin_proton(&self, _install: &SteamInstall, _id: RedirectId) -> anyhow::Result<PinOutcome> {
        Ok(PinOutcome::NoSteam)
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
    write_atomic(&marker_path(), &m.render())
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

// ── Per-app env: `apps.toml` `env` for a game a running Steam forks ──────────
//
// The Launch-Options gap: env set on the `steam://rungameid/<id>` handoff command
// dies with that command. A running Steam forks the game from the CLIENT's
// environment, and a Steam that Pharos pre-warms inherits the compositor's — so
// an entry's `env` never reached a Steam title, silently. The shim is already in
// the game's Launch Options for a redirected game, and it already re-execs the
// game with an env it sources at exec time; so the env rides there, as one
// sourceable fragment per appid written next to the marker.
//
// Consequences worth knowing: it applies only on a LIVE redirect (the shim
// sources it after the staleness guard), so desktop play is never touched, and
// it needs the redirect opt-in like everything else that edits Launch Options.

/// `$XDG_RUNTIME_DIR/pharos/app-env` — one sourceable fragment per Steam appid,
/// plus the `pending` fallback pointer. Same tmpfs as the marker, so nothing here
/// outlives a logout.
pub fn app_env_dir() -> PathBuf {
    marker_path()
        .parent()
        .map(|d| d.join("app-env"))
        .unwrap_or_else(|| PathBuf::from("app-env"))
}

/// The fragment for one appid.
pub fn app_env_path(appid: u32) -> PathBuf {
    app_env_dir().join(appid.to_string())
}

/// Fallback pointer for the (unexpected) case where Steam doesn't put
/// `SteamAppId`/`SteamGameId` in the wrapper's environment: names the appid of
/// the handoff currently in flight. Written only when there IS env to apply and
/// removed by the launcher once the launch window closes, so its freshness is
/// enforced by the writer rather than by a non-portable mtime test in the shim.
pub fn app_env_pending_path() -> PathBuf {
    app_env_dir().join("pending")
}

/// The fragment is `.`-sourced by /bin/sh, so a key that isn't a usable variable
/// name can't be expressed as an assignment and is dropped rather than quietly
/// mangled. Same predicate the editor validates against and the direct-spawn path
/// enforces — one definition, in the schema.
pub use crate::config::valid_env_key;

/// Render the sourceable fragment for `env`, returning it alongside the keys that
/// were dropped as unusable. Values are single-quoted with embedded `'` escaped,
/// exactly like [`MarkerEnv::render`].
pub fn render_app_env(env: &[(String, String)]) -> (String, Vec<String>) {
    let mut s = String::new();
    let mut skipped = Vec::new();
    for (k, v) in env {
        let k = k.trim();
        if !valid_env_key(k) {
            skipped.push(k.to_string());
            continue;
        }
        s.push_str(&format!("export {k}='{}'\n", v.replace('\'', "'\\''")));
    }
    (s, skipped)
}

/// Atomically write the per-appid fragment. Returns how many variables it
/// carries; an entry whose keys are all unusable writes nothing and clears any
/// fragment left by a previous launch, so a fixed-then-broken edit can't keep
/// applying the old values.
pub fn write_app_env(appid: u32, env: &[(String, String)]) -> std::io::Result<usize> {
    let (body, skipped) = render_app_env(env);
    for k in &skipped {
        tracing::warn!(
            "launch-redirect: dropping app env var `{k}` for appid {appid} — not a usable shell \
             variable name ([A-Za-z_][A-Za-z0-9_]*)"
        );
    }
    let count = body.lines().count();
    if count == 0 {
        remove_app_env(appid);
        return Ok(0);
    }
    write_atomic(&app_env_path(appid), &body)?;
    Ok(count)
}

/// Point the shim at `appid` for the launch in flight (see
/// [`app_env_pending_path`]).
pub fn write_app_env_pending(appid: u32) -> std::io::Result<()> {
    write_atomic(
        &app_env_pending_path(),
        &kv("PHAROS_APP_ENV_APPID", &appid.to_string()),
    )
}

/// Drop the fallback pointer once the launch window has closed.
pub fn clear_app_env_pending() {
    let _ = std::fs::remove_file(app_env_pending_path());
}

/// Remove one appid's fragment (ignore if already gone).
pub fn remove_app_env(appid: u32) {
    let _ = std::fs::remove_file(app_env_path(appid));
}

/// Clear every fragment at compositor startup — the sibling of
/// [`sweep_stale_marker`], for the same reason: a crashed prior run must not
/// leave env armed for a game we're no longer launching.
pub fn sweep_app_env() {
    let _ = std::fs::remove_dir_all(app_env_dir());
}

/// Temp + rename write, creating the parent dir. Shared by the marker and the
/// per-app fragments so both land atomically for a shim that may source them at
/// any moment.
fn write_atomic(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
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
    fn app_env_renders_exports_and_escapes() {
        let (body, skipped) = render_app_env(&[
            ("VK_LOADER_LAYERS_ENABLE".into(), "VK_LAYER_reshade".into()),
            ("QUOTED".into(), "it's".into()),
        ]);
        assert!(skipped.is_empty());
        assert!(body.contains("export VK_LOADER_LAYERS_ENABLE='VK_LAYER_reshade'\n"));
        assert!(body.contains("export QUOTED='it'\\''s'\n"));
    }

    #[test]
    fn app_env_drops_unusable_keys() {
        // A key that isn't a shell name can't be expressed as an assignment; it is
        // dropped (and reported) rather than corrupting the whole fragment, which
        // `.`-sourcing a malformed line would do.
        let (body, skipped) = render_app_env(&[
            ("2BAD".into(), "x".into()),
            ("has space".into(), "x".into()),
            ("A=B".into(), "x".into()),
            ("".into(), "x".into()),
            ("GOOD_1".into(), "y".into()),
        ]);
        assert_eq!(skipped, vec!["2BAD", "has space", "A=B", ""]);
        assert_eq!(body, "export GOOD_1='y'\n");
    }

    #[test]
    fn app_env_key_shape() {
        assert!(valid_env_key("A"));
        assert!(valid_env_key("_x9"));
        assert!(!valid_env_key(""));
        assert!(!valid_env_key("9a"));
        assert!(!valid_env_key("a-b"));
    }

    #[test]
    fn app_env_paths_sit_beside_the_marker() {
        let dir = app_env_dir();
        assert_eq!(dir.parent(), marker_path().parent());
        assert!(app_env_path(70).ends_with("app-env/70"));
        assert!(app_env_pending_path().ends_with("app-env/pending"));
    }

    #[test]
    fn enabled_reflects_env() {
        // Not asserting the global env; just that the helper reads the var name.
        // (Cargo runs tests without PHAROS_STEAM_REDIRECT.)
        assert!(!enabled() || std::env::var_os("PHAROS_STEAM_REDIRECT").is_some());
    }
}
