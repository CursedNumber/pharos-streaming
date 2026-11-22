// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! The persisted application catalog (`apps.toml`).
//!
//! Shape on disk (an array of tables so it reads cleanly and merges by hand):
//!
//! ```toml
//! [[app]]
//! slug = "elden-ring"
//! name = "Elden Ring"
//! kind = "game"
//! command = "setsid -f steam steam://rungameid/1245620"
//! box_art = "/home/you/.local/share/Steam/appcache/librarycache/1245620_library_600x900.jpg"
//! target = "virtual-display"
//! ```
//!
//! Everything but `slug`/`name`/`command` is optional and `#[serde(default)]`,
//! so a hand-written entry stays minimal and a future field never breaks an old
//! file.

use serde::{Deserialize, Serialize};

/// What a catalog entry represents — drives the default icon/placeholder in the
/// panel and the Trireme grid, and (later) sensible launch defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AppKind {
    /// A generic application.
    #[default]
    App,
    /// A game (Steam or native).
    Game,
    /// The plain virtual desktop — the built-in `desktop` entry.
    Desktop,
    /// A media file / player.
    Media,
}

/// *Where* an app should be launched.
///
/// This is the seam that kept "duplicate / extend the **physical** desktop" a
/// drop-in later rather than a rewrite: every layer (config, control-IPC, the
/// client relay, the panel editor) carries this opaquely, and only [`launch`]
/// matches on it. **`DuplicatePhysical` is wired** (issue #6, "desktop mirror
/// mode"): the Linux compositor mirrors the real desktop session via
/// xdg-desktop-portal + PipeWire (video) and libei (keyboard/pointer input);
/// on Windows every target already means this (WGC/DDA capture is physical
/// by nature — see [`launch`]'s guard). `ExtendPhysical` remains reserved —
/// accepted, stored, and shown, but [`launch`] still returns a clear "not
/// wired yet" error for it instead of silently doing the wrong thing.
///
/// [`launch`]: crate::launch::launch
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchTarget {
    /// The Pharos virtual display (headless Wayland output). The default for
    /// every entry except the built-in `desktop` (see [`AppEntry::desktop_builtin`]).
    #[default]
    VirtualDisplay,
    /// A specific named virtual output (a `multi.toml` output name). Advisory in
    /// v1 — window placement isn't per-output yet — but stored so it's honored
    /// the moment it is.
    VirtualOutput(String),
    /// Reserved: extend the host's physical desktop with the stream.
    ExtendPhysical,
    /// Duplicate/mirror the host's physical desktop (issue #6). The default
    /// for the built-in `desktop` entry.
    DuplicatePhysical,
}

/// How the host should treat gamepad gyro samples while *this* app is the
/// active foreground app. Per-app, so a shooter can demand raw motion while a
/// browser maps the same pad's gyro onto the mouse cursor and an emulator
/// turns it off entirely.
///
/// The compositor's IMU reader thread always runs (it's cheap, and the
/// client's gyro-calibration visualiser still wants samples), so the mode
/// only gates *dispatch*:
///  * [`Enabled`](GyroMode::Enabled) — raw `GamepadMotion` reaches the game
///    via the virtual pad; gyro→mouse aim is OFF. Default — the most useful
///    setting for a real gyro-aware game / Steam Input.
///  * [`Mouse`](GyroMode::Mouse) — host applies gyro→mouse aim instead of
///    surfacing raw gyro. Raw samples still flow to the client calibration UI.
///  * [`Disabled`](GyroMode::Disabled) — neither aim nor raw forwarding;
///    the client's visualiser also goes quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GyroMode {
    /// Raw gyro reaches the game (default).
    #[default]
    Enabled,
    /// Host turns gyro into mouse motion (aim assist).
    Mouse,
    /// Drop gyro entirely while this app is active.
    Disabled,
}

/// How this app's framerate is capped while it runs.
///
/// A streamed game rendering far above the virtual output's refresh just burns
/// GPU/heat for frames that are never encoded — the headless output is a *fixed*
/// mode with no VRR, so there is no adaptive-sync window to fill and matching the
/// output rate is a free efficiency win. Realised by injecting a MangoHud
/// `fps_limit` into the child's environment at launch
/// ([`launch`](crate::launch::launch)).
///
///  * [`Off`](FpsCap::Off) — no cap (default; the app renders unbounded).
///  * [`Auto`](FpsCap::Auto) — cap at the live virtual output's refresh, read
///    from the [`LaunchContext`](crate::launch::LaunchContext). Falls back to no
///    cap if the host didn't supply a refresh (e.g. a bare CLI/test launch).
///  * [`Fixed`](FpsCap::Fixed) — cap at an explicit FPS.
///
/// Only the **direct-spawn** path applies this. A Steam title is re-spawned by
/// the Steam client from *its* own environment, so a cap for a Steam game must
/// live in that title's Steam launch options (`mangohud %command%`), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FpsCap {
    /// No frame cap (default).
    #[default]
    Off,
    /// Cap at the live virtual output's refresh.
    Auto,
    /// Cap at an explicit FPS.
    Fixed(u32),
}

impl FpsCap {
    /// The concrete FPS limit to apply, given the live virtual output's refresh
    /// (`output_fps`, whole FPS, if the host knows it). `None` means "no cap".
    ///
    /// `Auto` resolves to the output refresh exactly: the streamed output is a
    /// fixed mode, so there is no VRR ceiling to stay below — matching the rate
    /// is correct. (On a *physical* VRR panel you'd cap a few FPS under max
    /// refresh to keep adaptive-sync engaged; that's local play, not this path.)
    /// A `Fixed(0)` is treated as "no cap" so a stray zero can't freeze the game.
    pub fn resolve(self, output_fps: Option<u32>) -> Option<u32> {
        match self {
            FpsCap::Off => None,
            FpsCap::Auto => output_fps.filter(|f| *f > 0),
            FpsCap::Fixed(0) => None,
            FpsCap::Fixed(n) => Some(n),
        }
    }
}

impl LaunchTarget {
    /// True for the targets [`launch`](crate::launch::launch) can currently
    /// spawn into: the virtual-display family, plus `DuplicatePhysical` now
    /// that the desktop mirror is wired (issue #6). `ExtendPhysical` remains
    /// the one reserved/unsupported seam.
    pub fn is_supported(&self) -> bool {
        matches!(
            self,
            LaunchTarget::VirtualDisplay
                | LaunchTarget::VirtualOutput(_)
                | LaunchTarget::DuplicatePhysical
        )
    }

    /// True for the targets that paint onto the host's **real** session — extend
    /// or duplicate the physical desktop. Purely descriptive (used by the panel
    /// to label a target "real desktop"); it does NOT imply "unsupported" —
    /// see [`is_supported`](Self::is_supported), which now allows
    /// `DuplicatePhysical` through. [`launch`](crate::launch::launch)'s guard
    /// bails on `ExtendPhysical` specifically, not on this predicate.
    ///
    /// Centralized here so every layer agrees on what "physical" means.
    pub fn is_physical_target(&self) -> bool {
        matches!(
            self,
            LaunchTarget::ExtendPhysical | LaunchTarget::DuplicatePhysical
        )
    }
}

/// *How* an entry is launched, as structured data instead of a hand-authored
/// command string.
///
/// Easy-mode picks (a Steam game, a `.desktop` app) carry the few fields the
/// launcher needs (`appid`, the desktop-file `exec`) so [`launch`] can build the
/// correct, env-aware command *itself* — the user never has to paste a path to a
/// wrapper script. A free-form entry stays a [`LaunchKind::RawCommand`], the
/// only kind a hand-written `apps.toml` produces.
///
/// On disk this is a tagged table, e.g.
/// ```toml
/// [app.launch]
/// kind = "steam-game"
/// appid = 1245620
/// ```
/// It is optional: an entry with no `[app.launch]` table falls back to its
/// [`AppEntry::command`] string (the v1 shape, still fully supported). See
/// [`AppEntry::resolved_kind`].
///
/// [`launch`]: crate::launch::launch
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LaunchKind {
    /// A Steam game launched by `steam://rungameid/<appid>`. The launcher phrases
    /// the URI for the detected [`SteamInstall`](crate::env::SteamInstall) at
    /// launch time, so a catalog authored on one machine launches correctly on
    /// another (native vs Flatpak vs Snap vs Windows).
    SteamGame { appid: u32 },
    /// A freedesktop `.desktop` app, launched by its (field-code-stripped)
    /// `Exec=` line. `exec` is the structured command the scanner extracted.
    DesktopFile { exec: String },
    /// A free-form command line (the v1 behaviour). `line` is tokenized on
    /// whitespace (respecting double quotes) unless [`AppEntry::shell`] is set.
    RawCommand { line: String },
    /// A gamescope-hosted Steam session (issue #38): the whole Steam client —
    /// Big Picture UI, overlay, Steam Input, launch/shader dialogs — runs inside
    /// one `gamescope … -- steam -gamepadui` nested session on the virtual
    /// display, the way the Steam Deck hosts it. This is what makes Steam Input
    /// API games (which read controllers only via Steam client IPC, routed by
    /// the window focus *Steam* sees) playable through the stream, and puts
    /// every Steam-owned window in-stream instead of lost on the host desktop.
    /// Linux-only; sized from the live output mode at launch (see
    /// [`launch`](crate::launch::launch)).
    SteamSession,
}

/// Which display server hosts this app's windows on the virtual output.
///
/// The built-in Smithay compositor speaks Wayland only — an X11-only app
/// launched onto it simply never maps a window, and a Steam-Input-API game
/// needs the whole Steam client focused in ITS display to get controller
/// configs (issue #38). This knob lets an entry opt into a nested host that
/// fits the app, per app, from the editor:
///
/// - `builtin` (default): spawn straight onto the compositor's Wayland socket —
///   native Wayland clients, the existing behaviour.
/// - `xwayland`: spawn a rootful fullscreen Xwayland onto the virtual display
///   and point the app at it (`DISPLAY=:N`, Wayland scrubbed) — X11-only apps.
///   No window manager runs inside, so it fits fullscreen games/emulators, not
///   multi-window desktop apps.
/// - `gamescope`: wrap the app in a nested gamescope sized to the stream. For a
///   Steam launch this boots the whole client Deck-style *directly into the
///   game* (`gamescope -e … -- steam -gamepadui steam://rungameid/<appid>`) —
///   Steam Input works and no Big Picture navigation is needed (issue #38's
///   session, per game).
///
/// Linux-only semantics; non-`builtin` values fail the launch on other hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AppCompositor {
    /// The built-in compositor's Wayland socket (default).
    #[default]
    Builtin,
    /// A per-app rootful Xwayland on the virtual display (X11-only apps).
    Xwayland,
    /// A nested gamescope session; Steam launches boot straight into the game.
    Gamescope,
}

/// One launchable application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppEntry {
    /// Short, stable, lowercase id — the CLI key (`pharos-compositor launch-app
    /// <slug>`) and the relay id. Unique within the catalog.
    pub slug: String,
    /// Human display name shown on the card / tile.
    pub name: String,
    /// What this is (icon/placeholder + defaults).
    #[serde(default)]
    pub kind: AppKind,
    /// The launch command line. Tokenized on whitespace (respecting double
    /// quotes) unless [`AppEntry::shell`] is set. An **empty** command is a
    /// deliberate no-op (the built-in Desktop "launches" nothing).
    #[serde(default)]
    pub command: String,
    /// Structured launch description, if this entry was created by easy mode (a
    /// Steam game / desktop app) or upgraded. When present it takes precedence
    /// over [`command`](AppEntry::command), which is kept as a human-readable
    /// hint and as the fallback for hand-written entries that have no
    /// `[app.launch]` table. Resolve with [`AppEntry::resolved_kind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchKind>,
    /// Run `command` through the platform shell (`/bin/sh -c` / `cmd /c`) so
    /// pipes, `&&`, env-expansion and `steam://` URI handlers work. Default off
    /// (a direct spawn, which is what games/most apps want).
    #[serde(default)]
    pub shell: bool,
    /// Working directory for the child (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra environment for the child, as `[["KEY","VALUE"], …]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Vulkan implicit layers to arm for this app, as family keys from
    /// [`crate::vklayer::scan`] (a layer name with its architecture suffix
    /// stripped, so a selection survives a 32-bit build being added later).
    /// Realised as env at spawn — the layer's own `enable_environment` switch
    /// plus the loader's `VK_LOADER_LAYERS_ENABLE` list — injected BEFORE
    /// [`env`](AppEntry::env) so a hand-typed variable still wins. MangoHud is
    /// deliberately not offered here; it is the
    /// [`perf_overlay`](AppEntry::perf_overlay) checkbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vk_layers: Vec<String>,
    /// Local path to a box-art / icon image (PNG or JPEG). Shown in the panel and
    /// relayed (as bytes) to Trireme clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_art: Option<String>,
    /// Where to launch. Defaults to the virtual display.
    #[serde(default)]
    pub target: LaunchTarget,
    /// How the host treats gamepad gyro while *this* app is the active foreground
    /// app — raw to the game (default), gyro→mouse, or disabled entirely. The
    /// compositor's IMU reader keeps running regardless; this only gates dispatch.
    /// `#[serde(default)]` so an old `apps.toml` without this field still loads.
    #[serde(default)]
    pub gyro_mode: GyroMode,
    /// Which display server hosts this app on the virtual output — the built-in
    /// Wayland compositor (default), a per-app rootful Xwayland, or a nested
    /// gamescope session (see [`AppCompositor`]). `#[serde(default)]` so an old
    /// `apps.toml` without this field still loads.
    #[serde(default)]
    pub compositor: AppCompositor,
    /// Show MangoHud's perf overlay (fps/frametime/GPU/CPU) burned into this app's
    /// stream. Off by default — the [`fps_cap`](AppEntry::fps_cap) limiter normally
    /// runs MangoHud *invisibly* (`no_display=1`); turning this on flips it to
    /// `no_display=0` so the HUD is visible on the client, and activates MangoHud
    /// even without a cap. Needs MangoHud installed on the host (the panel gates the
    /// control on that); like the cap, it rides the direct-spawn env path, so a
    /// Steam-URL title still needs `mangohud %command%` in its Steam launch options.
    /// Declared BEFORE `fps_cap` because that field's `Fixed` variant serialises as a
    /// `[app.fps_cap]` table and TOML emits scalars-before-tables (see `fps_cap`).
    #[serde(default)]
    pub perf_overlay: bool,
    /// How this app's framerate is capped while it runs — off (default), auto
    /// (match the virtual output's refresh), or a fixed FPS. Realised as a
    /// MangoHud `fps_limit` injected into the child env on the direct-spawn path
    /// (Steam titles must set it in their Steam launch options — see [`FpsCap`]).
    /// `#[serde(default)]` so an old `apps.toml` without this field still loads.
    /// Kept last (like `target`) because the `Fixed` variant serializes as a
    /// standalone `[app.fps_cap]` sub-table, and TOML's "tables come last" rule
    /// means any *scalar* field declared after it would be emitted after that
    /// table and fail to round-trip. (Both `off`/`auto` strings and the
    /// `{ fixed = N }` inline form still parse on read — only the emitted shape
    /// is constrained.)
    #[serde(default)]
    pub fps_cap: FpsCap,
    /// Files of the app's own config to keep a separate copy of per paired
    /// client (by section), e.g. an emulator's `[Controls]` in `qt-config.ini`
    /// so the phone, the TV, and the Xbox each keep their own pad mapping. See
    /// [`crate::per_client`]. Emitted as `[[app.per_client_config]]` tables, so
    /// it sits after every scalar field (TOML's tables-come-last rule).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub per_client_config: Vec<PerClientFile>,
}

/// One per-client config file: a path (a leading `~/` means the host user's
/// home) and the INI section names in it to keep per client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerClientFile {
    /// The app's config file, e.g. `~/.config/lichii/qt-config.ini`.
    pub file: String,
    /// Section names without brackets, e.g. `["Controls"]`.
    pub sections: Vec<String>,
}

impl AppEntry {
    /// A blank entry ready for the editor.
    pub fn new() -> Self {
        AppEntry {
            slug: String::new(),
            name: String::new(),
            kind: AppKind::App,
            command: String::new(),
            launch: None,
            shell: false,
            cwd: None,
            env: Vec::new(),
            vk_layers: Vec::new(),
            box_art: None,
            target: LaunchTarget::default(),
            gyro_mode: GyroMode::default(),
            compositor: AppCompositor::default(),
            perf_overlay: false,
            fps_cap: FpsCap::default(),
            per_client_config: Vec::new(),
        }
    }

    /// The built-in "Desktop" entry — always present in the catalog the panel,
    /// CLI, and clients see, so there is always a tile to return to the plain
    /// virtual desktop. Its empty command makes [`launch`](crate::launch::launch)
    /// a no-op (the desktop is the virtual display).
    pub fn desktop_builtin() -> Self {
        AppEntry {
            slug: "desktop".into(),
            name: "Desktop".into(),
            kind: AppKind::Desktop,
            command: String::new(),
            launch: None,
            shell: false,
            cwd: None,
            env: Vec::new(),
            vk_layers: Vec::new(),
            box_art: None,
            // Desktop mirror mode (issue #6) is the default for the built-in
            // Desktop entry: streaming "your desktop" now means the REAL
            // host desktop, not an isolated virtual one. Per-app entries
            // still default to `VirtualDisplay` (an app's isolated stream
            // display); the panel's target picker lets either be overridden.
            target: LaunchTarget::DuplicatePhysical,
            gyro_mode: GyroMode::default(),
            compositor: AppCompositor::default(),
            perf_overlay: false,
            fps_cap: FpsCap::default(),
            per_client_config: Vec::new(),
        }
    }

    /// The built-in "Steam Big Picture" entry — a gamescope-hosted Steam session
    /// on the virtual display (issue #38). Only surfaced by [`Catalog::with_builtins`]
    /// when the host can actually run it (Linux + gamescope + a Steam install —
    /// see [`steam_session_supported`](crate::launch::steam_session_supported)),
    /// so the tile never appears where it would only error.
    pub fn steam_session_builtin() -> Self {
        AppEntry {
            slug: "steam-session".into(),
            name: "Steam Big Picture".into(),
            kind: AppKind::Game,
            command: String::new(),
            launch: Some(LaunchKind::SteamSession),
            shell: false,
            cwd: None,
            env: Vec::new(),
            vk_layers: Vec::new(),
            box_art: None,
            target: LaunchTarget::VirtualDisplay,
            gyro_mode: GyroMode::default(),
            compositor: AppCompositor::default(),
            perf_overlay: false,
            fps_cap: FpsCap::default(),
            per_client_config: Vec::new(),
        }
    }

    /// True for entries the user authored (i.e. not a synthetic built-in like
    /// `desktop`), so the editor only lets you delete/edit real ones.
    pub fn is_builtin(&self) -> bool {
        self.slug == "desktop" || self.slug == "steam-session"
    }

    /// The effective [`LaunchKind`] for this entry: the explicit structured
    /// `launch` table if present, otherwise a [`LaunchKind::RawCommand`]
    /// synthesized from the back-compat [`command`](AppEntry::command) string.
    ///
    /// This is the single place `launch` consults so structured and legacy
    /// entries flow through one code path. A blank command yields a `RawCommand`
    /// with an empty line, which `launch` treats as the no-op (Desktop) case.
    pub fn resolved_kind(&self) -> LaunchKind {
        match &self.launch {
            Some(k) => k.clone(),
            None => LaunchKind::RawCommand {
                line: self.command.clone(),
            },
        }
    }
}

impl Default for AppEntry {
    fn default() -> Self {
        Self::new()
    }
}

/// The whole catalog. `#[serde(rename = "app")]` makes the on-disk array
/// `[[app]]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default, rename = "app")]
    pub apps: Vec<AppEntry>,
}

impl Catalog {
    /// Find an entry by slug (case-insensitive on the slug).
    pub fn find(&self, slug: &str) -> Option<&AppEntry> {
        let s = slug.trim();
        self.apps.iter().find(|a| a.slug.eq_ignore_ascii_case(s))
    }

    /// Return a copy with built-in entries prepended if they aren't already
    /// present: `desktop` always, `steam-session` (issue #38) only where the
    /// host can run it. Use this for anything user-facing — the panel grid, the
    /// CLI, the client relay — so Desktop always shows. A user entry with the
    /// same slug wins over the built-in (same override rule as `desktop`).
    pub fn with_builtins(&self) -> Catalog {
        let mut out = Catalog::default();
        if self.find("desktop").is_none() {
            out.apps.push(AppEntry::desktop_builtin());
        }
        if self.find("steam-session").is_none() && crate::launch::steam_session_supported() {
            out.apps.push(AppEntry::steam_session_builtin());
        }
        out.apps.extend(self.apps.iter().cloned());
        out
    }

    /// Validate slug uniqueness/shape before save, returning a human error.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for a in &self.apps {
            let slug = a.slug.trim();
            if slug.is_empty() {
                return Err(format!("`{}` has an empty slug", a.name));
            }
            if a.name.trim().is_empty() {
                return Err(format!("`{slug}` has an empty name"));
            }
            if !seen.insert(slug.to_ascii_lowercase()) {
                return Err(format!("duplicate slug `{slug}`"));
            }
        }
        Ok(())
    }
}

// ── Environment rows ────────────────────────────────────────────────────────
//
// An entry's `env` is the escape hatch that carries Vulkan-layer / wrapper
// variables (ReShade, vkBasalt, the DLSS layers) into a game, so it is worth
// checking rather than passing through: every consumer spells a variable as a
// shell-ish `KEY=VALUE`, and a key that isn't a usable name is either dropped
// (the redirect fragment, which `.`-sources it) or turned into a malformed
// `environ` entry (a direct spawn). One definition here; the panel validates
// against it before saving and the launcher enforces it before spawning.

/// Why one row of an entry's `env` can't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRowIssue {
    /// A value with no key to put it under.
    EmptyKey,
    /// Not a POSIX-portable variable name.
    BadKey,
    /// An earlier row already sets this key; this one would silently win.
    Duplicate,
}

/// Is `k` a POSIX-portable environment-variable name (`[A-Za-z_][A-Za-z0-9_]*`)?
pub fn valid_env_key(k: &str) -> bool {
    let mut cs = k.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Classify every row of `env`, index-aligned with it. A wholly blank row is
/// `None` (an editor row nobody filled in yet, dropped by [`sanitize_env`]), not
/// an error to shout about while the user is still typing.
pub fn env_row_issues(env: &[(String, String)]) -> Vec<Option<EnvRowIssue>> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(env.len());
    for (k, v) in env {
        let k = k.trim();
        out.push(if k.is_empty() {
            // A wholly blank row is pending, not broken.
            if v.trim().is_empty() {
                None
            } else {
                Some(EnvRowIssue::EmptyKey)
            }
        } else if !valid_env_key(k) {
            Some(EnvRowIssue::BadKey)
        } else if !seen.insert(k) {
            Some(EnvRowIssue::Duplicate)
        } else {
            None
        });
    }
    out
}

/// Trim keys and drop wholly blank rows. What an editor should do before it
/// validates, so an unfilled "Add variable" click never reaches `apps.toml`.
pub fn sanitize_env(env: &mut Vec<(String, String)>) {
    env.retain(|(k, v)| !(k.trim().is_empty() && v.trim().is_empty()));
    for (k, _) in env.iter_mut() {
        let trimmed = k.trim();
        if trimmed.len() != k.len() {
            *k = trimmed.to_string();
        }
    }
}

/// Load the persisted catalog from `apps.toml`. A missing file is an empty
/// catalog; an unparseable one logs and returns empty (never panics — the host
/// must keep streaming regardless).
pub fn load() -> Catalog {
    let path = match crate::apps_path() {
        Ok(p) => p,
        Err(_) => return Catalog::default(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Catalog::default(),
    };
    match toml::from_str::<Catalog>(&text) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("apps.toml at {} unparseable: {e}; ignoring", path.display());
            Catalog::default()
        }
    }
}

/// Load the catalog including built-ins (Desktop). The host/CLI/relay entry
/// point.
pub fn load_with_builtins() -> Catalog {
    load().with_builtins()
}

/// Persist the catalog atomically (write `<path>.tmp`, then rename). Validates
/// first so a duplicate/blank slug is rejected before it overwrites a good file.
pub fn save(catalog: &Catalog) -> anyhow::Result<()> {
    catalog.validate().map_err(|e| anyhow::anyhow!(e))?;
    let path = crate::apps_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(catalog)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_toml() {
        let cat = Catalog {
            apps: vec![
                AppEntry {
                    slug: "elden-ring".into(),
                    name: "Elden Ring".into(),
                    kind: AppKind::Game,
                    command: "setsid -f steam steam://rungameid/1245620".into(),
                    ..AppEntry::new()
                },
                AppEntry {
                    slug: "files".into(),
                    name: "File Manager".into(),
                    command: "nautilus".into(),
                    env: vec![("GTK_THEME".into(), "Adwaita".into())],
                    target: LaunchTarget::VirtualOutput("left".into()),
                    ..AppEntry::new()
                },
            ],
        };
        let text = toml::to_string_pretty(&cat).unwrap();
        let back: Catalog = toml::from_str(&text).unwrap();
        assert_eq!(cat, back);
    }

    #[test]
    fn builtins_prepend_desktop() {
        // Desktop is always first. The steam-session built-in is machine-gated
        // (Linux + gamescope + Steam), so only assert that ANY extra built-in is
        // exactly that one — the test must pass both on CI (absent) and on a rig
        // with gamescope installed (present).
        let cat = Catalog::default().with_builtins();
        assert_eq!(cat.apps[0].slug, "desktop");
        assert!(cat.apps[0].is_builtin());
        for extra in &cat.apps[1..] {
            assert_eq!(extra.slug, "steam-session");
            assert!(extra.is_builtin());
            assert_eq!(extra.resolved_kind(), LaunchKind::SteamSession);
        }
    }

    #[test]
    fn steam_session_kind_roundtrips_through_toml() {
        // A hand-written `[app.launch] kind = "steam-session"` table must parse,
        // and the built-in entry must survive a save/load cycle.
        let cat = Catalog {
            apps: vec![AppEntry {
                slug: "bp".into(),
                name: "Big Picture".into(),
                launch: Some(LaunchKind::SteamSession),
                ..AppEntry::new()
            }],
        };
        let text = toml::to_string_pretty(&cat).unwrap();
        assert!(text.contains("kind = \"steam-session\""), "{text}");
        let back: Catalog = toml::from_str(&text).unwrap();
        assert_eq!(back.apps[0].resolved_kind(), LaunchKind::SteamSession);
    }

    #[test]
    fn legacy_entry_without_launch_table_parses_and_resolves_to_raw() {
        // Back-compat: a v1 apps.toml entry (no `[app.launch]`) must still parse
        // and resolve to a RawCommand built from `command`.
        let toml = r#"
            [[app]]
            slug = "firefox"
            name = "Firefox"
            command = "firefox"
            target = "virtual-display"
        "#;
        let cat: Catalog = toml::from_str(toml).unwrap();
        let e = cat.find("firefox").unwrap();
        assert!(e.launch.is_none());
        assert_eq!(
            e.resolved_kind(),
            LaunchKind::RawCommand {
                line: "firefox".into()
            }
        );
    }

    #[test]
    fn structured_launch_roundtrips_and_takes_precedence() {
        let cat = Catalog {
            apps: vec![AppEntry {
                slug: "elden-ring".into(),
                name: "Elden Ring".into(),
                kind: AppKind::Game,
                command: "setsid -f steam steam://rungameid/1245620".into(),
                launch: Some(LaunchKind::SteamGame { appid: 1245620 }),
                ..AppEntry::new()
            }],
        };
        let text = toml::to_string_pretty(&cat).unwrap();
        let back: Catalog = toml::from_str(&text).unwrap();
        assert_eq!(cat, back);
        // resolved_kind prefers the structured table over the command string.
        assert_eq!(
            back.apps[0].resolved_kind(),
            LaunchKind::SteamGame { appid: 1245620 }
        );
    }

    #[test]
    fn physical_target_helper_agrees_with_support() {
        assert!(LaunchTarget::ExtendPhysical.is_physical_target());
        assert!(LaunchTarget::DuplicatePhysical.is_physical_target());
        assert!(!LaunchTarget::VirtualDisplay.is_physical_target());
        // ExtendPhysical stays reserved/unsupported; DuplicatePhysical is
        // wired (issue #6 desktop mirror) — `is_physical_target` describes
        // "targets the real desktop", NOT "unsupported".
        assert!(!LaunchTarget::ExtendPhysical.is_supported());
        assert!(LaunchTarget::DuplicatePhysical.is_supported());
        assert!(LaunchTarget::VirtualDisplay.is_supported());
    }

    #[test]
    fn fps_cap_resolves_against_output_refresh() {
        // Off / Fixed(0) never cap; Fixed(n) is literal; Auto follows the output.
        assert_eq!(FpsCap::Off.resolve(Some(120)), None);
        assert_eq!(FpsCap::Fixed(0).resolve(Some(120)), None);
        assert_eq!(FpsCap::Fixed(60).resolve(Some(120)), Some(60));
        assert_eq!(FpsCap::Auto.resolve(Some(120)), Some(120));
        // Auto with no known refresh (CLI/test) falls back to uncapped, not a guess.
        assert_eq!(FpsCap::Auto.resolve(None), None);
        assert_eq!(FpsCap::Auto.resolve(Some(0)), None);
    }

    #[test]
    fn fps_cap_field_roundtrips_through_toml() {
        // The Fixed variant serializes as a standalone `[app.fps_cap]` sub-table;
        // confirm it survives a write/read cycle as the trailing AppEntry field
        // (a scalar field after it would land after the table and break this),
        // and that a legacy file with no fps_cap deserialises to the Off default.
        let cat = Catalog {
            apps: vec![AppEntry {
                slug: "capped".into(),
                name: "Capped".into(),
                fps_cap: FpsCap::Fixed(142),
                target: LaunchTarget::VirtualOutput("left".into()),
                ..AppEntry::new()
            }],
        };
        let text = toml::to_string_pretty(&cat).unwrap();
        let back: Catalog = toml::from_str(&text).unwrap();
        assert_eq!(cat, back);
        assert_eq!(back.apps[0].fps_cap, FpsCap::Fixed(142));

        let legacy = r#"
            [[app]]
            slug = "old"
            name = "Old"
            command = "firefox"
        "#;
        let c: Catalog = toml::from_str(legacy).unwrap();
        assert_eq!(c.find("old").unwrap().fps_cap, FpsCap::Off);
    }

    #[test]
    fn per_client_config_roundtrips_after_fixed_fps_cap() {
        // Both are tables in TOML; the hand-written shape must parse, and a
        // save (with a `[app.fps_cap]` sub-table before it) must come back.
        let hand = r#"
            [[app]]
            slug = "lichii"
            name = "Lichii"
            command = "lichii"
            fps_cap = { fixed = 60 }

            [[app.per_client_config]]
            file = "~/.config/lichii/qt-config.ini"
            sections = ["Controls"]
        "#;
        let cat: Catalog = toml::from_str(hand).unwrap();
        let e = cat.find("lichii").unwrap();
        assert_eq!(
            e.per_client_config,
            vec![PerClientFile {
                file: "~/.config/lichii/qt-config.ini".into(),
                sections: vec!["Controls".into()],
            }]
        );
        let text = toml::to_string_pretty(&cat).unwrap();
        let back: Catalog = toml::from_str(&text).unwrap();
        assert_eq!(cat, back);
        // Empty stays out of the file entirely.
        let plain = toml::to_string_pretty(&Catalog {
            apps: vec![AppEntry {
                slug: "x".into(),
                name: "X".into(),
                ..AppEntry::new()
            }],
        })
        .unwrap();
        assert!(!plain.contains("per_client_config"));
    }

    #[test]
    fn vk_layers_roundtrip_through_toml() {
        let cat = Catalog {
            apps: vec![AppEntry {
                slug: "nms".into(),
                name: "No Man's Sky".into(),
                vk_layers: vec!["VK_LAYER_reshade".into()],
                ..AppEntry::new()
            }],
        };
        let text = toml::to_string_pretty(&cat).unwrap();
        let back: Catalog = toml::from_str(&text).unwrap();
        assert_eq!(back.apps[0].vk_layers, vec!["VK_LAYER_reshade".to_string()]);
        // Absent for an entry with no layers, and an old apps.toml still loads.
        let plain = toml::to_string_pretty(&Catalog {
            apps: vec![AppEntry::new()],
        })
        .unwrap();
        assert!(!plain.contains("vk_layers"), "{plain}");
        let legacy: Catalog =
            toml::from_str("[[app]]\nslug = \"a\"\nname = \"A\"\ncommand = \"x\"\n").unwrap();
        assert!(legacy.apps[0].vk_layers.is_empty());
    }

    #[test]
    fn env_key_shape() {
        assert!(valid_env_key("A"));
        assert!(valid_env_key("_x9"));
        assert!(valid_env_key("VK_LOADER_LAYERS_ENABLE"));
        assert!(!valid_env_key(""));
        assert!(!valid_env_key("9a"));
        assert!(!valid_env_key("a-b"));
        assert!(!valid_env_key("A=B"));
        assert!(!valid_env_key("has space"));
    }

    #[test]
    fn env_rows_are_classified() {
        let env = vec![
            ("VK_LOADER_LAYERS_ENABLE".into(), "VK_LAYER_reshade".into()),
            ("".into(), "".into()),          // pending row, not an error
            ("".into(), "orphan".into()),    // value with no name
            ("2BAD".into(), "x".into()),     // not a variable name
            (" GOOD ".into(), "y".into()),   // trimmed before checking
            ("GOOD".into(), "again".into()), // duplicate of the row above
        ];
        assert_eq!(
            env_row_issues(&env),
            vec![
                None,
                None,
                Some(EnvRowIssue::EmptyKey),
                Some(EnvRowIssue::BadKey),
                None,
                Some(EnvRowIssue::Duplicate),
            ]
        );
    }

    #[test]
    fn sanitize_env_drops_blanks_and_trims_keys() {
        let mut env = vec![
            ("".into(), "".into()),
            ("  SPACED  ".into(), " kept ".into()),
            ("".into(), "orphan".into()),
        ];
        sanitize_env(&mut env);
        // The blank row is gone; the key is trimmed and the VALUE is left alone
        // (trailing space in a value can be meaningful); the orphan value stays so
        // validation can report it instead of silently discarding what was typed.
        assert_eq!(
            env,
            vec![
                ("SPACED".to_string(), " kept ".to_string()),
                (String::new(), "orphan".to_string()),
            ]
        );
    }

    #[test]
    fn validate_rejects_duplicate_slugs() {
        let cat = Catalog {
            apps: vec![
                AppEntry {
                    slug: "a".into(),
                    name: "A".into(),
                    ..AppEntry::new()
                },
                AppEntry {
                    slug: "A".into(),
                    name: "A2".into(),
                    ..AppEntry::new()
                },
            ],
        };
        assert!(cat.validate().is_err());
    }
}
