//! Pharos application launcher.
//!
//! One launchable application is an [`AppEntry`]: a short `slug`, a display
//! `name`, a `command`, optional box art, and a [`LaunchTarget`] saying *where*
//! it should appear. Entries live in `apps.toml` in the Pharos config dir (next
//! to `peers.toml` / `multi.toml`), edited visually by `pharos-panel` and read
//! back by the host.
//!
//! The crate does three things, all in pure Rust:
//!
//! 1. **Config** ([`config`]) — load/save the [`Catalog`] of [`AppEntry`]s.
//! 2. **Launch** ([`launch`]) — spawn an entry's process *environment-aware*,
//!    pointing it at the compositor's virtual display (Wayland socket) and
//!    scrubbing `DISPLAY` so it can't escape to the host's real X11/Wayland.
//! 3. **Easy mode** ([`scan`]) — discover installed Steam games and `.desktop`
//!    apps and turn a pick into a ready-to-edit [`AppEntry`] with a generated,
//!    distro-correct launch command (`setsid -f steam steam://rungameid/…`,
//!    `flatpak run …`, `steam://rungameid/…` on Windows) and offline box art.
//!
//! The host (compositor) is the only process that knows the live virtual-display
//! socket, so the CLI and Trireme clients never launch directly — they ask the
//! host to [`launch_slug`] by name and the host fills in the [`LaunchContext`].

use std::path::PathBuf;

pub mod config;
pub mod env;
pub mod launch;
pub mod redirect;
pub mod scan;
mod steam;

pub use config::{AppEntry, AppKind, Catalog, FpsCap, GyroMode, LaunchKind, LaunchTarget};
pub use launch::{launch, session_audio_sink, set_session_audio_sink, LaunchContext};
pub use scan::{DesktopApp, SteamGame};

/// `~/.config/pharos/` on Linux / `%APPDATA%\pharos\` on Windows, or
/// `$PHAROS_CONFIG_DIR` if overridden.
///
/// Resolved identically to `host_core::pair::config_dir` and the panel's
/// `data::config_dir` (intentional small duplication so this stays a lean leaf
/// with no `host-core` dependency).
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("PHAROS_CONFIG_DIR") {
        return Ok(PathBuf::from(p));
    }
    let base = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no platform config dir; set PHAROS_CONFIG_DIR"))?;
    Ok(base.join("pharos"))
}

/// Full path to `apps.toml`.
pub fn apps_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("apps.toml"))
}

/// Resolve `slug` against the on-disk catalog (plus built-ins like `desktop`)
/// and [`launch`] it with the given context. The host's one-stop entry point for
/// the CLI verb and the client relay: load → find → launch.
///
/// Returns the spawned child (or `None` for a no-op entry such as the bare
/// Desktop, whose empty command means "the virtual display is already showing").
pub fn launch_slug(slug: &str, ctx: &LaunchContext) -> anyhow::Result<Option<std::process::Child>> {
    let catalog = config::load_with_builtins();
    let entry = catalog
        .find(slug)
        .ok_or_else(|| anyhow::anyhow!("no app with slug `{slug}` in apps.toml"))?;
    launch::launch(entry, ctx)
}
