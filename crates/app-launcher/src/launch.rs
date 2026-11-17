// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Spawning an [`AppEntry`] into the right place, environment-aware.
//!
//! The host fills in a [`LaunchContext`] (the live virtual-display socket) and
//! calls [`launch`]. On Linux a virtual-display launch sets `WAYLAND_DISPLAY` to
//! the compositor's socket and scrubs `DISPLAY` — exactly the pattern the
//! compositor already uses for its media player and startup client — so the app
//! renders into the streamed output and can't leak onto the host's real session.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::config::{AppCompositor, AppEntry, LaunchTarget};
use crate::env::detect_steam;

/// Steam's URI handler can race with pipe creation: a `steam://` handoff fired
/// the instant `~/.steam/steam.pipe` first appears is occasionally dropped on
/// the floor while the client is still wiring up its IPC listener. A brief
/// grace once the pipe is up sidesteps that without coupling us to Steam's
/// startup more than necessary.
const POST_PIPE_GRACE: Duration = Duration::from_millis(500);

/// Upper bound on how long the Steam-launch worker waits for the IPC pipe to
/// come up after a pre-warm. The worker lives off the render thread, so this
/// timeout no longer blocks the stream.
const STEAM_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// What the host knows about the live target that an [`AppEntry`] doesn't: the
/// virtual display's Wayland socket (and an X11 `DISPLAY`, if the host ever runs
/// XWayland for the virtual output).
#[derive(Debug, Clone, Default)]
pub struct LaunchContext {
    /// `WAYLAND_DISPLAY` for the compositor's virtual output (e.g. `wayland-1`).
    pub wayland_socket: Option<OsString>,
    /// `DISPLAY` for the virtual output, if applicable (usually `None`).
    pub x11_display: Option<String>,
    /// `PULSE_SINK` for the app: the session's isolated null sink, so its audio
    /// is captured for the stream instead of going to the host's default sink.
    /// Defaults from [`session_audio_sink`] (set by the compositor) in
    /// [`LaunchContext::wayland`].
    pub audio_sink: Option<String>,
    /// The live virtual output's refresh in whole FPS, if the host knows it.
    /// Used to resolve an [`FpsCap::Auto`](crate::config::FpsCap::Auto) into a
    /// concrete MangoHud `fps_limit`. `None` (the CLI/tests, which have no
    /// compositor context) leaves an `Auto` cap *uncapped* rather than guessing.
    pub output_fps: Option<u32>,
    /// The live virtual output's native mode in pixels, if the host knows it.
    /// Sizes a [`LaunchKind::SteamSession`]'s gamescope output (`-W`/`-H`) to
    /// the streamed surface; `None` (CLI/tests) falls back to 1920×1080.
    pub output_size: Option<(u32, u32)>,
}

impl LaunchContext {
    /// Convenience: a context that targets the given compositor Wayland socket,
    /// with `audio_sink` defaulted to the live session sink (if any). The output
    /// refresh is unknown here — set it with [`with_output_fps`] when the caller
    /// knows the virtual output's rate (the compositor does).
    ///
    /// [`with_output_fps`]: LaunchContext::with_output_fps
    pub fn wayland(socket: impl Into<OsString>) -> Self {
        LaunchContext {
            wayland_socket: Some(socket.into()),
            x11_display: None,
            audio_sink: session_audio_sink(),
            output_fps: None,
            output_size: None,
        }
    }

    /// Record the live virtual output's refresh (whole FPS), used to resolve an
    /// [`FpsCap::Auto`](crate::config::FpsCap::Auto) at launch.
    pub fn with_output_fps(mut self, fps: u32) -> Self {
        self.output_fps = Some(fps);
        self
    }

    /// Record the live virtual output's native mode, used to size a Steam
    /// session's gamescope output.
    pub fn with_output_size(mut self, w: u32, h: u32) -> Self {
        self.output_size = Some((w, h));
        self
    }
}

/// Process-global name of the session's audio sink (a `module-null-sink`).
/// The compositor sets this once it has created the sink; launches then route
/// the app's audio there via `PULSE_SINK`. `None` = no isolation (apps land on
/// the host default sink, the legacy behaviour).
fn session_audio_sink_cell() -> &'static Mutex<Option<String>> {
    static SINK: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

/// Set (or clear with `None`) the session audio sink that virtual-display apps
/// should play into. Called by the compositor after it creates the per-session
/// null sink.
pub fn set_session_audio_sink(name: Option<String>) {
    *session_audio_sink_cell().lock().unwrap() = name;
}

/// The current session audio sink, if the compositor has set one.
pub fn session_audio_sink() -> Option<String> {
    session_audio_sink_cell().lock().unwrap().clone()
}

/// Launch `entry` into the place named by its [`LaunchTarget`], using `ctx` for
/// the live socket. Returns the spawned child, or `Ok(None)` for a no-op (an
/// empty command, e.g. the built-in Desktop, *or* an already-running app — see
/// the idempotency note below).
///
/// Errors (a bad command, a missing binary, an unsupported/physical target) are
/// returned, never panicked — a launch failing must fail the *request*, not the
/// host.
///
/// **Idempotency.** Every successful direct-exec spawn is recorded in a
/// process-wide registry keyed by slug. If the slug's previous child is still
/// alive, a second `launch` is a no-op returning `Ok(None)` ("already running")
/// rather than a duplicate process. `steam://rungameid/…` launches are already
/// deduped by Steam itself (it focuses the running game), so the registry mostly
/// protects the direct-exec entries (Firefox, a bare game exe). Best-effort and
/// non-blocking: a dead/absent pid simply falls through to a fresh spawn.
pub fn launch(entry: &AppEntry, ctx: &LaunchContext) -> anyhow::Result<Option<Child>> {
    // Resolve the structured kind once (explicit `[app.launch]` table, else a
    // RawCommand synthesized from the legacy `command` string). The detected
    // Steam install turns a SteamGame/RawCommand into the concrete command line,
    // so the launcher — not a hand-authored string — owns the distro wrapper.
    let kind = entry.resolved_kind();

    // A Steam session (issue #38) is not a command line: gamescope is sized from
    // the live output mode in `ctx`, so it gets its own build path before any
    // line resolution. Its own guards mirror the generic ones below.
    if matches!(kind, crate::config::LaunchKind::SteamSession) {
        return launch_steam_session(entry, ctx, None);
    }

    let install = detect_steam();
    let (line_owned, kind_shell) = install.launch_command(&kind);
    let line = line_owned.trim();

    if line.is_empty() {
        // The Desktop built-in (and any intentionally-blank entry): the virtual
        // display already *is* the desktop, so there's nothing to spawn.
        tracing::info!("app `{}`: no command (desktop / no-op)", entry.slug);
        return Ok(None);
    }

    // Extend-physical footgun guard — LINUX ONLY. `DuplicatePhysical` is wired
    // (issue #6: the compositor mirrors the real desktop via xdg-desktop-portal
    // + PipeWire/libei instead of cold-spawning INTO it — nothing new lands on
    // the live session, so the NVENC libnvcuvid cold-start SIGSEGV this guard
    // exists for doesn't apply). `ExtendPhysical` remains unwired and dangerous
    // in the same way `DuplicatePhysical` used to be: refuse explicitly rather
    // than silently doing the wrong thing.
    //
    // On WINDOWS (issue #22) the virtual/physical split this guard encodes does
    // not exist: the streamed surface IS a real desktop output (DDA/WGC capture
    // of `output_idx`), so every target — VirtualDisplay and the physical family
    // alike — means "spawn normally onto the captured desktop". The guard's
    // NVENC caveat doesn't carry over either: the libnvcuvid SIGSEGV lives in
    // ffmpeg's CUVID loader on the Linux path, and the Windows wgc-zc pipeline
    // drives the NVENC SDK in-process without ever loading it — verified live on
    // the rig (2026-07-07): a cold Steam start + GPU game launch during a
    // running in-process NVENC session encodes on undisturbed. So Windows skips
    // both this guard and the not-yet-supported gate below.
    if cfg!(not(windows)) && matches!(entry.target, LaunchTarget::ExtendPhysical) {
        anyhow::bail!(
            "app `{}` targets {:?} (extending the host's PHYSICAL desktop), which \
             the launcher refuses to cold-start into: a fresh GPU app would appear \
             on the real session being captured AND can crash the NVENC encoder \
             (the documented libnvcuvid SIGSEGV). Duplicate/mirror the physical \
             desktop is wired (issue #6); extending it is still a planned feature. \
             Use DuplicatePhysical or a virtual-display target instead.",
            entry.slug,
            entry.target
        );
    }

    // Defense in depth (Linux only, see above): any other not-yet-supported
    // target (should be subsumed by the guard above, but keep the explicit
    // gate so a new LaunchTarget variant can't silently slip through to a spawn).
    if cfg!(not(windows)) && !entry.target.is_supported() {
        anyhow::bail!(
            "app `{}` targets {:?}, which isn't wired into the compositor yet",
            entry.slug,
            entry.target
        );
    }

    // Idempotency: if we already launched this slug and that child is still alive,
    // don't spawn a duplicate — treat it as a focus/no-op. (Steam URIs self-dedupe,
    // but direct-exec entries like Firefox would otherwise stack up.)
    if let Some(pid) = live_registered_pid(&entry.slug) {
        tracing::info!(
            "app `{}`: already running (pid {pid}); focus/no-op, not relaunching",
            entry.slug
        );
        return Ok(None);
    }

    // Per-app compositor routing (see [`AppCompositor`]): a non-builtin host
    // wraps or re-targets the spawn. Gamescope owns the whole build path — for
    // a Steam launch it boots the client INSIDE the session straight into the
    // game (the issue-#38 session, per game); Xwayland brings up a rootful X
    // server on the virtual display first. Both are Linux-only and bail
    // actionably elsewhere.
    match entry.compositor {
        AppCompositor::Builtin => {}
        AppCompositor::Gamescope => return launch_in_gamescope(entry, ctx, line, kind_shell),
        AppCompositor::Xwayland => return launch_in_xwayland(entry, ctx, line, kind_shell),
    }

    // Perf overlay on Windows = RTSS. It hooks new processes system-wide once
    // its service is running, so starting it BEFORE the app is the whole
    // integration — and unlike the env-scoped MangoHud path below, it also
    // covers Steam handoffs (Steam re-spawns the game, but RTSS hooks it all
    // the same). What the OSD shows is configured in RTSS itself.
    #[cfg(windows)]
    if entry.perf_overlay {
        ensure_rtss_running();
    }

    // Warm-Steam guard: a launch that hands off to the Steam client (a `steam://`
    // URI or the `steam` binary) must NOT cold-start Steam concurrently with the
    // live NVENC encode — that triggers the documented `libnvcuvid` SIGSEGV that
    // kills the host. Move the whole Steam path onto a detached worker thread so
    // the compositor's render loop never blocks on Steam startup (up to ~20s).
    // The URI-delivery process exits immediately after handoff, so launch()'s
    // own `Ok(None)` is the correct return — the actual game is owned by Steam,
    // not by us. running_apps tracking already handles Ok(None) gracefully.
    if launches_via_steam(line) {
        // Steam-game idempotency the pid-registry can't provide: a `steam://`
        // handoff pid dies immediately, so `live_registered_pid` never catches a
        // running Steam game. If the game is already running, a second
        // `rungameid` handoff is at best redundant and at worst races a
        // mid-teardown instance into the D3D `Failed to create device` crash we hit
        // on reconnect — so detect the live game by its appid and no-op (Steam
        // would focus it anyway; we just skip the dangerous redundant handoff).
        if let Some(appid) = parse_steam_appid(line) {
            if steam_game_running(appid) {
                tracing::info!(
                    "app `{}`: Steam game {appid} already running; focus/no-op, not relaunching",
                    entry.slug
                );
                return Ok(None);
            }
        }
        spawn_steam_handoff_worker(entry, ctx, line, kind_shell)?;
        return Ok(None);
    }

    let mut cmd = build_command(entry, line, kind_shell)?;

    // Point the child at the virtual display. On Linux that means the
    // compositor's Wayland socket, with DISPLAY scrubbed so a Wayland-capable app
    // can't fall back to the host's real X11/Wayland session. Reached only for the
    // virtual-display family (physical targets bailed above).
    if let Some(sock) = &ctx.wayland_socket {
        cmd.env("WAYLAND_DISPLAY", sock);
    }
    match &ctx.x11_display {
        Some(d) => {
            cmd.env("DISPLAY", d);
        }
        None => {
            cmd.env_remove("DISPLAY");
        }
    }
    // Route the app's audio into the session's isolated sink so it's captured
    // for the stream (and kept off the host desktop's mix). `PULSE_SINK` is
    // honoured by PulseAudio / pipewire-pulse clients — SDL, Wine/Proton, mpv,
    // Firefox, etc. — which is everything we launch here.
    if let Some(sink) = &ctx.audio_sink {
        cmd.env("PULSE_SINK", sink);
    }

    // FPS cap → MangoHud limiter env. Resolved against the live virtual-output
    // refresh (`ctx.output_fps`) so an `Auto` cap matches the stream rate.
    // Injected *before* per-entry env so an explicit `MANGOHUD_CONFIG` the user
    // typed into the entry still wins. Direct-spawn only: a Steam handoff
    // re-spawns the game from Steam's own env, so a Steam title's cap must live
    // in its Steam launch options instead (see [`FpsCap`]).
    for (k, v) in mangohud_env(entry.fps_cap.resolve(ctx.output_fps), entry.perf_overlay) {
        cmd.env(k, v);
    }

    // Per-entry working dir + extra env, applied last so an entry can override.
    if let Some(dir) = &entry.cwd {
        if !dir.trim().is_empty() {
            cmd.current_dir(dir);
        }
    }
    apply_entry_env(&mut cmd, &entry.slug, &effective_env(entry));

    tracing::info!(
        "launching app `{}` ({}) on {:?}",
        entry.slug,
        line,
        ctx.wayland_socket
    );
    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn `{}` for app `{}`: {e}", line, entry.slug))?;
    register_pid(&entry.slug, child.id());
    Ok(Some(child))
}

// ── Steam session (issue #38): gamescope-hosted Big Picture in the stream ───
//
// Two architectural walls, one root cause: the Steam client lives on the real
// desktop while a Pharos-launched game lives on the compositor's nested Wayland
// display. Steam Input routes controllers by the window focus *Steam* sees, so
// a Steam-Input-API game in the stream never gets its action set activated
// (lesson-steam-input-nested-wayland); and Steam's launch/shader/onboarding
// windows map on the desktop, invisible in-stream. Hosting the WHOLE Steam
// client Deck-style — `gamescope … -- steam -gamepadui` — inside the virtual
// display solves both: Steam owns the session it is focused in, and every
// Steam window renders in the stream.

/// Is gamescope on `PATH`? Gates the built-in `steam-session` tile and the
/// spawn. Cached — presence doesn't change within a run. Always false
/// off-Linux (nested gamescope is a Linux Wayland client).
pub fn gamescope_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        cfg!(target_os = "linux")
            && std::env::var_os("PATH")
                .map(|paths| {
                    std::env::split_paths(&paths).any(|dir| dir.join("gamescope").is_file())
                })
                .unwrap_or(false)
    })
}

/// Can THIS host run a gamescope-hosted Steam session? Linux with gamescope on
/// `PATH` and a detected Steam install. Consulted by
/// [`Catalog::with_builtins`](crate::config::Catalog::with_builtins) so the
/// built-in tile only appears where launching it can work.
pub fn steam_session_supported() -> bool {
    gamescope_available() && detect_steam().root().is_some()
}

/// The live Steam session's identity: `(pid, start_ticks)`, recorded at spawn.
/// Start ticks make the liveness probe immune to pid recycling (same guard as
/// [`end_app`]'s SIGKILL pass). Linux-only, like the spawn path that fills it.
#[cfg(target_os = "linux")]
type SessionPid = Option<(u32, Option<u64>)>;

#[cfg(target_os = "linux")]
fn steam_session_pid_cell() -> &'static Mutex<SessionPid> {
    static PID: OnceLock<Mutex<SessionPid>> = OnceLock::new();
    PID.get_or_init(|| Mutex::new(None))
}

/// Is a Pharos-spawned gamescope Steam session currently alive? Cheap (one
/// /proc probe) and self-clearing when the session exits. The compositor's
/// redirect-marker reconciler keys off this: inside the session Steam already
/// owns the right display, so the `pharos-redirect` shim must see NO marker
/// and pass through — a marker would yank a session-launched game out of
/// gamescope onto the bare compositor socket.
pub fn steam_session_active() -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut cell = steam_session_pid_cell()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match *cell {
            Some((pid, start)) if pid_alive(pid) && proc_starttime(pid) == start => true,
            Some(_) => {
                *cell = None;
                false
            }
            None => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// gamescope argv for a Steam session: `-e` (Steam integration), `-f`
/// (fullscreen), output sized to the live virtual mode, nested refresh matched
/// to the stream pace, then any `extra` flags (`PHAROS_GAMESCOPE_ARGS`), `--`,
/// and the install-correct Steam client argv in Big Picture mode. An
/// `autolaunch` URI (`steam://rungameid/<appid>`) appended after `-gamepadui`
/// makes the client boot straight into that game — the SteamOS boot-to-game
/// shape — so a per-game session never shows Big Picture navigation. Any
/// `inner_env` is applied to the nested command with an `env` prefix, so it
/// reaches Steam and its games without touching gamescope itself.
#[cfg(target_os = "linux")]
fn steam_session_args(
    w: u32,
    h: u32,
    fps: Option<u32>,
    extra: &[String],
    client: &[String],
    autolaunch: Option<&str>,
    inner_env: &[(String, String)],
) -> Vec<String> {
    let mut a: Vec<String> = vec!["-e".into(), "-f".into()];
    a.push("-W".into());
    a.push(w.to_string());
    a.push("-H".into());
    a.push(h.to_string());
    if let Some(f) = fps.filter(|f| *f > 0) {
        a.push("-r".into());
        a.push(f.to_string());
    }
    a.extend(extra.iter().cloned());
    a.push("--".into());
    // Anything in `inner_env` is for Steam and the games it forks, NOT for
    // gamescope: an `env K=V` prefix on the nested command keeps a Vulkan layer
    // off gamescope's own renderer (see [`env_prefix_argv`]).
    a.extend(env_prefix_argv(inner_env));
    a.extend(client.iter().cloned());
    a.push("-gamepadui".into());
    if let Some(uri) = autolaunch {
        a.push(uri.into());
    }
    a
}

/// Spawn a gamescope-hosted Steam session: gamescope hosting the full Steam
/// client on the virtual display. Two callers: a
/// [`LaunchKind::SteamSession`](crate::config::LaunchKind::SteamSession) entry
/// (`autolaunch = None`, plain Big Picture — issue #38) and a Steam-launching
/// entry whose [`AppCompositor`] is `Gamescope` (`autolaunch =
/// Some("steam://rungameid/<appid>")`, the client boots straight into that
/// game). Mirrors [`launch`]'s generic guards (physical-target refusal, slug
/// idempotency) and adds the session-specific ones: gamescope + Steam present,
/// and no desktop Steam already running — `-gamepadui` handed to a running
/// Steam just flips *its* desktop window to Big Picture, so nothing would
/// appear in the stream.
///
/// The per-app `fps_cap`/`perf_overlay` knobs deliberately do NOT apply here:
/// `MANGOHUD=1` on gamescope would hook gamescope's own Vulkan compositor, and
/// every game in the session inherits the env — cap per-title in Steam launch
/// options instead. gamescope's `-r` already paces the session to the stream.
fn launch_steam_session(
    entry: &AppEntry,
    ctx: &LaunchContext,
    autolaunch: Option<&str>,
) -> anyhow::Result<Option<Child>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ctx, autolaunch);
        anyhow::bail!(
            "app `{}` is a steam-session (gamescope-hosted Steam), which is Linux-only",
            entry.slug
        );
    }
    #[cfg(target_os = "linux")]
    {
        if entry.target.is_physical_target() {
            anyhow::bail!(
                "app `{}` is a steam-session targeting {:?} (the host's PHYSICAL desktop); \
                 the session exists to own the virtual display — use a virtual-display target",
                entry.slug,
                entry.target
            );
        }
        if let Some(pid) = live_registered_pid(&entry.slug) {
            tracing::info!(
                "app `{}`: Steam session already running (pid {pid}); focus/no-op",
                entry.slug
            );
            return Ok(None);
        }
        if !gamescope_available() {
            anyhow::bail!(
                "steam-session needs gamescope on PATH — install your distro's `gamescope` package"
            );
        }
        let install = detect_steam();
        if install.root().is_none() {
            anyhow::bail!("steam-session: no Steam install detected on this host");
        }
        if crate::steam::is_ready() {
            anyhow::bail!(
                "steam-session: Steam is already running on the desktop, and a second launch \
                 would attach to it there instead of starting inside the stream. Fully quit \
                 Steam (Steam → Exit, or `steam -shutdown`) and launch the session again"
            );
        }
        let (w, h) = ctx.output_size.unwrap_or((1920, 1080));
        let extra = std::env::var("PHAROS_GAMESCOPE_ARGS")
            .map(|s| tokenize(&s))
            .unwrap_or_default();
        let args = steam_session_args(
            w,
            h,
            ctx.output_fps,
            &extra,
            &install.session_client_argv(),
            autolaunch,
            &crate::vklayer::enable_env(&entry.vk_layers, &crate::vklayer::scan()),
        );
        let mut cmd = Command::new("gamescope");
        cmd.args(&args);
        if let Some(sock) = &ctx.wayland_socket {
            cmd.env("WAYLAND_DISPLAY", sock);
        }
        // gamescope brings its own XWayland for the games; scrub the host's
        // DISPLAY so nothing inside can escape to the real X11 session.
        match &ctx.x11_display {
            Some(d) => {
                cmd.env("DISPLAY", d);
            }
            None => {
                cmd.env_remove("DISPLAY");
            }
        }
        // Steam and every game it launches inherit the session sink, so the
        // whole session's audio lands in the stream capture.
        if let Some(sink) = &ctx.audio_sink {
            cmd.env("PULSE_SINK", sink);
        }
        if let Some(dir) = &entry.cwd {
            if !dir.trim().is_empty() {
                cmd.current_dir(dir);
            }
        }
        // The entry's own `env` goes on gamescope (and is inherited by everything
        // inside it), as it always has; the layer toggles do NOT — they ride the
        // inner argv instead, so they never attach to gamescope's own renderer.
        apply_entry_env(&mut cmd, &entry.slug, &entry.env);
        tracing::info!(
            "launching Steam session `{}`: gamescope {}",
            entry.slug,
            args.join(" ")
        );
        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("spawn gamescope for steam-session `{}`: {e}", entry.slug)
        })?;
        let pid = child.id();
        register_pid(&entry.slug, pid);
        *steam_session_pid_cell()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((pid, proc_starttime(pid)));
        Ok(Some(child))
    }
}

/// Launch an entry whose [`AppCompositor`] is `Gamescope`: wrap the resolved
/// command in a nested gamescope sized to the live virtual mode.
///
/// A Steam launch line gets the full session treatment instead of a plain wrap
/// — the Steam client itself must live inside the gamescope for Steam Input to
/// route controllers (issue #38), so the whole client boots there Deck-style
/// with the game auto-launched (no Big Picture navigation). If a gamescope
/// Steam session is ALREADY live (the built-in tile, or another per-game
/// session), Steam is single-instance, so the plain URI handoff opens the game
/// inside it — reuse, not a second session.
fn launch_in_gamescope(
    entry: &AppEntry,
    ctx: &LaunchContext,
    line: &str,
    kind_shell: bool,
) -> anyhow::Result<Option<Child>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ctx, line, kind_shell);
        anyhow::bail!(
            "app `{}` selects the gamescope compositor, which is Linux-only",
            entry.slug
        );
    }
    #[cfg(target_os = "linux")]
    {
        if entry.target.is_physical_target() {
            anyhow::bail!(
                "app `{}` selects gamescope but targets {:?} (the host's PHYSICAL desktop); \
                 gamescope wraps apps on the virtual display — use a virtual-display target",
                entry.slug,
                entry.target
            );
        }
        if !gamescope_available() {
            anyhow::bail!(
                "app `{}` selects gamescope, but gamescope isn't on PATH — install your \
                 distro's `gamescope` package",
                entry.slug
            );
        }

        if launches_via_steam(line) {
            let appid = parse_steam_appid(line);
            if let Some(id) = appid {
                if steam_game_running(id) {
                    tracing::info!(
                        "app `{}`: Steam game {id} already running; focus/no-op, not relaunching",
                        entry.slug
                    );
                    return Ok(None);
                }
            }
            if steam_session_active() {
                // Steam already owns a gamescope session in the stream; the URI
                // handoff opens the game inside it.
                tracing::info!(
                    "app `{}`: live gamescope Steam session found — handing the launch off to it",
                    entry.slug
                );
                spawn_steam_handoff_worker(entry, ctx, line, kind_shell)?;
                return Ok(None);
            }
            let uri = appid.map(|id| format!("steam://rungameid/{id}"));
            return launch_steam_session(entry, ctx, uri.as_deref());
        }

        // Non-Steam command in a plain nested gamescope (no `-e` — that flag is
        // Steam-session integration). MangoHud cap/overlay are deliberately NOT
        // injected: MANGOHUD=1 in this env would hook gamescope's own Vulkan
        // renderer as well as the app (same rationale as the Steam session).
        if entry.fps_cap.resolve(ctx.output_fps).is_some() || entry.perf_overlay {
            tracing::info!(
                "app `{}`: fps_cap/perf_overlay are skipped under gamescope — put \
                 MANGOHUD_CONFIG in the entry's env (it applies inside the wrap) instead",
                entry.slug
            );
        }
        let (w, h) = ctx.output_size.unwrap_or((1920, 1080));
        let extra = std::env::var("PHAROS_GAMESCOPE_ARGS")
            .map(|s| tokenize(&s))
            .unwrap_or_default();
        let mut args: Vec<String> = vec![
            "-f".into(),
            "-W".into(),
            w.to_string(),
            "-H".into(),
            h.to_string(),
        ];
        if let Some(f) = ctx.output_fps.filter(|f| *f > 0) {
            args.push("-r".into());
            args.push(f.to_string());
        }
        args.extend(extra);
        args.push("--".into());
        // Layer toggles arm the app inside the wrap, never gamescope itself.
        args.extend(env_prefix_argv(&crate::vklayer::enable_env(
            &entry.vk_layers,
            &crate::vklayer::scan(),
        )));
        if kind_shell || entry.shell {
            args.push("/bin/sh".into());
            args.push("-c".into());
            args.push(line.to_string());
        } else {
            args.extend(tokenize(line));
        }
        let mut cmd = Command::new("gamescope");
        cmd.args(&args);
        if let Some(sock) = &ctx.wayland_socket {
            cmd.env("WAYLAND_DISPLAY", sock);
        }
        // gamescope brings its own XWayland for the app; scrub the host's
        // DISPLAY so nothing inside can escape to the real X11 session.
        cmd.env_remove("DISPLAY");
        if let Some(sink) = &ctx.audio_sink {
            cmd.env("PULSE_SINK", sink);
        }
        if let Some(dir) = &entry.cwd {
            if !dir.trim().is_empty() {
                cmd.current_dir(dir);
            }
        }
        // The entry's own `env` goes on gamescope (and is inherited by everything
        // inside it), as it always has; the layer toggles do NOT — they ride the
        // inner argv instead, so they never attach to gamescope's own renderer.
        apply_entry_env(&mut cmd, &entry.slug, &entry.env);
        tracing::info!(
            "launching app `{}` in gamescope: gamescope {}",
            entry.slug,
            args.join(" ")
        );
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn gamescope for app `{}`: {e}", entry.slug))?;
        register_pid(&entry.slug, child.id());
        Ok(Some(child))
    }
}

/// Launch an entry whose [`AppCompositor`] is `Xwayland`: bring up a rootful,
/// fullscreen Xwayland on the virtual display, wait for its socket, then run
/// the app against it (`DISPLAY=:N`, `WAYLAND_DISPLAY` scrubbed so an
/// X11-capable toolkit can't silently prefer Wayland). A minimal built-in
/// window manager ([`crate::xwm`]) fills the screen with each main window and
/// centres dialogs. `-terminate 10` makes the server exit on its own
/// 10s after its last client disconnects. The grace delay is load-bearing: a
/// bare `-terminate` exits on the FIRST disconnect, so an app that probes the
/// display before opening its window (Eden/Lichii does) killed the server under
/// itself. Until a client connects (or on a failed app spawn) we kill it
/// ourselves, and [`end_app`] reaps it as a registered companion of the slug.
fn launch_in_xwayland(
    entry: &AppEntry,
    ctx: &LaunchContext,
    line: &str,
    kind_shell: bool,
) -> anyhow::Result<Option<Child>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ctx, line, kind_shell);
        anyhow::bail!(
            "app `{}` selects the xwayland compositor, which is Linux-only",
            entry.slug
        );
    }
    #[cfg(target_os = "linux")]
    {
        if entry.target.is_physical_target() {
            anyhow::bail!(
                "app `{}` selects xwayland but targets {:?} (the host's PHYSICAL desktop); \
                 the per-app Xwayland lives on the virtual display — use a virtual-display target",
                entry.slug,
                entry.target
            );
        }
        if launches_via_steam(line) {
            anyhow::bail!(
                "app `{}` selects xwayland, but it launches via the Steam client, whose spawn \
                 environment Pharos doesn't control — use `gamescope` (Steam boots inside the \
                 session) or `builtin` (the display-redirect shim) instead",
                entry.slug
            );
        }

        let (w, h) = ctx.output_size.unwrap_or((1920, 1080));
        let xdpy = free_x_display()?;
        let mut xcmd = Command::new("Xwayland");
        xcmd.arg(format!(":{xdpy}"))
            .arg("-fullscreen")
            .arg("-geometry")
            .arg(format!("{w}x{h}"))
            .arg("-terminate")
            .arg("10");
        if let Some(sock) = &ctx.wayland_socket {
            xcmd.env("WAYLAND_DISPLAY", sock);
        }
        xcmd.env_remove("DISPLAY");
        let mut xchild = xcmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "spawn Xwayland for app `{}`: {e} (is xorg-xwayland installed?)",
                entry.slug
            )
        })?;

        // Readiness: poll for the X socket. A server that dies during startup
        // (bad flag, no Wayland socket) fails the launch with its exit status
        // instead of a timeout.
        let sock_path = format!("/tmp/.X11-unix/X{xdpy}");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::path::Path::new(&sock_path).exists() {
                break;
            }
            if let Ok(Some(status)) = xchild.try_wait() {
                anyhow::bail!(
                    "Xwayland for app `{}` exited during startup ({status})",
                    entry.slug
                );
            }
            if std::time::Instant::now() >= deadline {
                let _ = xchild.kill();
                anyhow::bail!("Xwayland for app `{}` didn't come up within 5s", entry.slug);
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        crate::xwm::spawn(xdpy);

        let mut cmd = build_command(entry, line, kind_shell)?;
        cmd.env("DISPLAY", format!(":{xdpy}"));
        cmd.env_remove("WAYLAND_DISPLAY");
        // The host session is Wayland, and toolkits (and Eden-based emulators'
        // "Wayland is experimental" check) read the session type, not just
        // WAYLAND_DISPLAY. Inside this server there is only X11; say so. Entry
        // env is applied after this and can still override.
        cmd.env("XDG_SESSION_TYPE", "x11");
        cmd.env("QT_QPA_PLATFORM", "xcb");
        cmd.env("SDL_VIDEODRIVER", "x11");
        if let Some(sink) = &ctx.audio_sink {
            cmd.env("PULSE_SINK", sink);
        }
        // Direct spawn: the MangoHud cap/overlay env works exactly as on the
        // builtin path (it hooks the app, not a wrapper).
        for (k, v) in mangohud_env(entry.fps_cap.resolve(ctx.output_fps), entry.perf_overlay) {
            cmd.env(k, v);
        }
        if let Some(dir) = &entry.cwd {
            if !dir.trim().is_empty() {
                cmd.current_dir(dir);
            }
        }
        apply_entry_env(&mut cmd, &entry.slug, &effective_env(entry));
        tracing::info!(
            "launching app `{}` ({}) on Xwayland :{}",
            entry.slug,
            line,
            xdpy
        );
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = xchild.kill();
                return Err(anyhow::anyhow!(
                    "spawn `{}` for app `{}`: {e}",
                    line,
                    entry.slug
                ));
            }
        };
        register_pid(&entry.slug, child.id());
        register_companion(&entry.slug, xchild.id());
        Ok(Some(child))
    }
}

/// Pick a free X display number by probing the lock file + socket. Racy in
/// principle (TOCTOU against another X server starting concurrently), but the
/// range starts well above desktop displays, and a collision surfaces as a
/// failed Xwayland startup — which fails this launch cleanly, not the host.
#[cfg(target_os = "linux")]
fn free_x_display() -> anyhow::Result<u32> {
    for n in 50..100u32 {
        let lock = format!("/tmp/.X{n}-lock");
        let sock = format!("/tmp/.X11-unix/X{n}");
        if !std::path::Path::new(&lock).exists() && !std::path::Path::new(&sock).exists() {
            return Ok(n);
        }
    }
    anyhow::bail!("no free X display number in :50..:100")
}

/// Translate a resolved FPS limit into the MangoHud environment that turns it
/// into a frame cap. `None` (no cap) yields no env at all — the app is untouched.
///
/// Env-only activation (rather than prefixing the command with `mangohud`) is
/// the *safe* choice: `MANGOHUD=1` enables MangoHud's implicit **Vulkan** layer —
/// which covers Proton/DXVK/VKD3D and native Vulkan, i.e. essentially every
/// modern game — and if MangoHud isn't installed the var is simply ignored and
/// the app still launches (just uncapped). Force-prefixing `mangohud` would
/// instead make the game fail to start outright when the wrapper is missing.
/// The gap is OpenGL-*only* titles, which need the `mangohud` wrapper (LD_PRELOAD);
/// for those, prefix it in the entry's command (or run in a shell).
///
/// `fps_limit_method=early` caps at frame *start* (RTSS-style, lowest input lag —
/// what this streaming host wants). `no_display` is driven by `overlay`: the cap
/// alone runs MangoHud *invisibly* (`no_display=1`) so it never burns a HUD into
/// the encoded stream, while `overlay=true` (the per-app `perf_overlay` opt-in)
/// flips it to `no_display=0` so the overlay IS visible on the client — and turns
/// MangoHud on even with no cap. Either one present ⇒ `MANGOHUD=1`; neither ⇒ no
/// env at all. A user who wants more HUD detail can still layer `MANGOHUD_CONFIG`
/// in the entry's env (applied after this, so it overrides).
fn mangohud_env(limit: Option<u32>, overlay: bool) -> Vec<(String, String)> {
    if limit.is_none() && !overlay {
        return Vec::new();
    }
    let mut cfg: Vec<String> = Vec::new();
    if let Some(n) = limit {
        cfg.push(format!("fps_limit={n}"));
        cfg.push("fps_limit_method=early".to_string());
    }
    cfg.push(format!("no_display={}", u8::from(!overlay)));
    vec![
        ("MANGOHUD".to_string(), "1".to_string()),
        ("MANGOHUD_CONFIG".to_string(), cfg.join(",")),
    ]
}

/// Whether MangoHud is available on THIS system — gates the panel's perf-overlay /
/// FPS-cap controls (both ride MangoHud) so we don't offer a knob that no-ops.
/// Cached: presence doesn't change within a run. Linux only — checks for the
/// `mangohud` wrapper on `PATH` or the Vulkan implicit-layer manifest the env-only
/// activation actually needs. Other platforms (the Windows host's RTSS/Afterburner
/// equivalent isn't wired yet) report `false`.
pub fn mangohud_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return false;
        }
        let on_path = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("mangohud").is_file()))
            .unwrap_or(false);
        // The env-only path (MANGOHUD=1) needs the Vulkan implicit layer, which may
        // be present even when the `mangohud` wrapper binary isn't.
        let layer = [
            "/usr/share/vulkan/implicit_layer.d/MangoHud.x86_64.json",
            "/usr/share/vulkan/implicit_layer.d/MangoHud.json",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).is_file());
        on_path || layer
    })
}

/// Whether a perf-overlay backend exists on THIS system — MangoHud on Linux,
/// RTSS (RivaTuner Statistics Server) on Windows. Gates the panel's
/// perf-overlay control so it isn't offered where it would no-op; the panel
/// picks its per-OS wording off `cfg!(windows)` to match.
pub fn perf_overlay_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        mangohud_available()
    }
    #[cfg(windows)]
    {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| rtss_exe().is_some())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        false
    }
}

/// Resolve RTSS.exe: the Unwinder registry key first (`InstallPath` is the
/// full exe path, `InstallDir` its folder — both observed live on RTSS 7.3.7),
/// then the Program Files defaults. `None` when not installed.
#[cfg(windows)]
fn rtss_exe() -> Option<std::path::PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    if let Ok(key) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey("SOFTWARE\\WOW6432Node\\Unwinder\\RTSS")
    {
        if let Ok(p) = key.get_value::<String, _>("InstallPath") {
            let p = std::path::PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Ok(d) = key.get_value::<String, _>("InstallDir") {
            let p = std::path::PathBuf::from(d).join("RTSS.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(var) {
            let p = std::path::PathBuf::from(base)
                .join("RivaTuner Statistics Server")
                .join("RTSS.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Make sure RTSS is running so it hooks the app we're about to launch and
/// draws its OSD into the frames the stream captures. Best-effort and
/// idempotent: no-op when RTSS isn't installed or already runs, and a spawn
/// failure only logs — the app launch itself must never be blocked on an
/// overlay. (No reaper needed: Windows has no zombie semantics, dropping the
/// child handle is fine.)
#[cfg(windows)]
fn ensure_rtss_running() {
    let Some(exe) = rtss_exe() else {
        tracing::debug!("perf_overlay set but RTSS not installed; launching without overlay");
        return;
    };
    let running = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq RTSS.exe", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("RTSS.exe"))
        .unwrap_or(false);
    if running {
        return;
    }
    match std::process::Command::new(&exe).spawn() {
        Ok(_) => tracing::info!("started RTSS for the perf overlay ({})", exe.display()),
        Err(e) => tracing::warn!("start RTSS ({}): {e}", exe.display()),
    }
}

/// An entry's effective environment: the typed Vulkan-layer toggles realised as
/// env, then the entry's own `env` on top, so a hand-typed variable still
/// overrides what a toggle generated. The one place the two are combined, so
/// every spawn path — direct, Xwayland, gamescope, Steam handoff — arms layers
/// the same way.
fn effective_env(entry: &AppEntry) -> Vec<(String, String)> {
    if entry.vk_layers.is_empty() {
        return entry.env.clone();
    }
    let installed = crate::vklayer::scan();
    let layer_env = crate::vklayer::enable_env(&entry.vk_layers, &installed);
    if !layer_env.is_empty() {
        tracing::info!(
            "app `{}`: arming Vulkan layer(s) {:?}",
            entry.slug,
            entry.vk_layers
        );
    }
    merge_env(layer_env, &entry.env)
}

/// Generated env first, hand-typed env second — `Command::env` and the shim's
/// sourced fragment both let the later write win, so this ordering is what makes
/// a typed variable able to override (or undo) what a toggle generated.
fn merge_env(
    mut layer_env: Vec<(String, String)>,
    entry_env: &[(String, String)],
) -> Vec<(String, String)> {
    layer_env.extend(entry_env.iter().cloned());
    layer_env
}

/// `env`-prefix tokens that carry `env` into a nested command, for the paths that
/// must NOT apply it to the wrapper itself. gamescope is a Vulkan client of the
/// Pharos compositor, so an implicit layer armed in *its* environment would
/// post-process the whole session as well as the game — the same trap the
/// MangoHud knobs already sidestep by declining to run under gamescope at all.
/// Prefixing the inner argv arms the layer for the app alone.
fn env_prefix_argv(env: &[(String, String)]) -> Vec<String> {
    if env.is_empty() {
        return Vec::new();
    }
    let mut argv = vec!["env".to_string()];
    argv.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    argv
}

/// Apply an entry's `env` to a child, skipping rows that aren't usable variable
/// names. `apps.toml` is hand-editable and the panel is not the only writer, so
/// the spawn paths enforce the same rule the editor validates against instead of
/// handing the kernel a malformed `environ` entry that no one can see afterwards.
/// Applied last by every caller, so an entry still overrides what we set above it.
fn apply_entry_env(cmd: &mut Command, slug: &str, env: &[(String, String)]) {
    for (k, v) in env {
        let key = k.trim();
        if !crate::config::valid_env_key(key) {
            tracing::warn!(
                "app `{slug}`: skipping env var `{k}` — not a usable variable name \
                 ([A-Za-z_][A-Za-z0-9_]*)"
            );
            continue;
        }
        cmd.env(key, v);
    }
}

/// Owned snapshot of everything `spawn_steam_url` needs to build and spawn the
/// URI-handoff `Command`. Captured on the caller's thread so the worker holds
/// no borrow on AppEntry / LaunchContext / compositor state — it must outlive
/// the request that spawned it.
struct SteamHandoffSpec {
    slug: String,
    line: String,
    kind_shell: bool,
    shell: bool,
    wayland_socket: Option<OsString>,
    x11_display: Option<String>,
    audio_sink: Option<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    /// Steam appid, when the launch line is a `rungameid` handoff and the seamless
    /// display-redirect feature is on — the worker idempotently arms the game's
    /// Launch Options so it renders into the stream (see [`crate::redirect`]).
    redirect_appid: Option<u32>,
    /// The appid of the launch line, parsed regardless of the redirect gate. Only
    /// used to phrase the per-app-env diagnostics: `env` on a Steam entry rides
    /// the redirect shim, so when the redirect is off we must say so instead of
    /// dropping the variables silently (which is what this path used to do).
    appid: Option<u32>,
}

/// Spawn a detached, named worker thread that pre-warms Steam (if needed),
/// waits for the IPC pipe, and then fires the URI-handoff command. The worker
/// owns its data outright — no borrow on caller-frame state — so its lifetime
/// is independent of the compositor's render loop.
fn spawn_steam_handoff_worker(
    entry: &AppEntry,
    ctx: &LaunchContext,
    line: &str,
    kind_shell: bool,
) -> anyhow::Result<()> {
    // Arm the seamless display-redirect only for a virtual-display launch (a
    // physical target must never be redirected) with the feature opted in — and
    // never while a gamescope Steam session is live: the handoff then goes to
    // the SESSION Steam, which already launches the game inside the stream, so
    // arming (and its "goes to the DESKTOP" deferral warning) would be wrong.
    let redirect_appid =
        if ctx.wayland_socket.is_some() && crate::redirect::enabled() && !steam_session_active() {
            parse_steam_appid(line)
        } else {
            None
        };
    let spec = SteamHandoffSpec {
        slug: entry.slug.clone(),
        line: line.to_string(),
        kind_shell,
        shell: entry.shell,
        wayland_socket: ctx.wayland_socket.clone(),
        x11_display: ctx.x11_display.clone(),
        audio_sink: ctx.audio_sink.clone(),
        cwd: entry.cwd.clone(),
        // Layer toggles + the entry's own env, resolved here on the caller's
        // thread: the worker outlives this frame and must own its data, and the
        // fragment it writes for the shim is the only route into a Steam-forked
        // game (see the app-env block in `run_steam_handoff`).
        env: effective_env(entry),
        redirect_appid,
        appid: parse_steam_appid(line),
    };
    let thread_name = format!("steam-launch-{}", spec.slug);
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || run_steam_handoff(spec))
        .map_err(|e| anyhow::anyhow!("failed to spawn steam-launch worker: {e}"))?;
    Ok(())
}

/// Worker body: wait for Steam to be ready (pre-warming if not), then fire the
/// URI-handoff command. Logs success/failure via tracing; never panics.
/// How long the app-env `pending` pointer stays valid after a handoff — long
/// enough to cover a cold Proton/shader-cache start, short enough that it is gone
/// before the user is plausibly launching a different game by hand.
const APP_ENV_PENDING_TTL: Duration = Duration::from_secs(180);

fn run_steam_handoff(spec: SteamHandoffSpec) {
    // Seamless display redirect: idempotently wrap the game's Steam Launch Options
    // in the `pharos-redirect` shim so a game a desktop Steam forks still renders
    // into the stream. Done here (before pre-warm) so a COLD Steam — which the
    // pre-warm below starts — loads the armed value at client start. A running
    // Steam can't be safely written (it clobbers localconfig on exit), so that
    // case defers and arms on the next Steam start. Best-effort: never fail the
    // launch on a redirect hiccup.
    if let Some(appid) = spec.redirect_appid {
        let provider = crate::redirect::provider();
        // Pin a winewayland-capable Proton for this game (opt-in, separate gate).
        // Only redirected games reach here, so we never touch a game the user
        // plays on the desktop. Same Steam-closed guard as the shim inject; best-
        // effort, never fails the launch.
        if crate::redirect::pin_proton_enabled() {
            match provider.pin_proton(&detect_steam(), crate::redirect::RedirectId { appid }) {
                Ok(crate::redirect::PinOutcome::Deferred) => tracing::warn!(
                    "steam-launch[{}]: Proton pin for appid {appid} DEFERRED — Steam is already \
                     running; config.vdf can't be armed now. Fully quit Steam and relaunch from \
                     Pharos and the pin arms automatically.",
                    spec.slug
                ),
                Ok(crate::redirect::PinOutcome::NoSuitableProton) => tracing::warn!(
                    "steam-launch[{}]: no winewayland-capable Proton installed to pin for appid \
                     {appid} — install proton-cachyos (or set PHAROS_STEAM_PROTON) or the stream \
                     may stay blank. Continuing.",
                    spec.slug
                ),
                Ok(outcome) => tracing::info!(
                    "steam-launch[{}]: Proton pin for appid {appid}: {outcome:?}",
                    spec.slug
                ),
                Err(e) => tracing::warn!(
                    "steam-launch[{}]: Proton pin for appid {appid} failed (non-fatal): {e:#}",
                    spec.slug
                ),
            }
        }
        match provider
            .ensure_shim_installed()
            .and_then(|()| provider.inject(&detect_steam(), crate::redirect::RedirectId { appid }))
        {
            // Steam already running → its in-memory config can't be safely armed
            // now (a file edit is invisible to it and clobbered on exit). Say so
            // loudly + actionably rather than silently sending the game to the
            // desktop; the injection self-arms the next time a launch finds Steam
            // closed (Pharos cold-starts it).
            Ok(crate::redirect::InjectOutcome::Deferred) => tracing::warn!(
                "steam-launch[{}]: display redirect for appid {appid} DEFERRED — Steam is \
                 already running, so this launch goes to the DESKTOP. Fully quit Steam and \
                 relaunch from Pharos (it will start Steam into the stream), and the redirect \
                 arms automatically.",
                spec.slug
            ),
            Ok(outcome) => tracing::info!(
                "steam-launch[{}]: display redirect for appid {appid}: {outcome:?}",
                spec.slug
            ),
            Err(e) => tracing::warn!(
                "steam-launch[{}]: display redirect for appid {appid} failed (non-fatal): {e:#}",
                spec.slug
            ),
        }
    }

    // Per-entry `env` for a game the Steam CLIENT forks. It cannot ride the
    // handoff command below: that process delivers a URI and exits, and Steam
    // spawns the game from ITS environment (a Steam Pharos pre-warms inherits the
    // compositor's, not the entry's). So hand the variables to the redirect shim,
    // which is already wrapping this game's Launch Options and already re-execs it
    // with a sourced env. Applies on a live redirect only, so desktop play is
    // untouched; without the redirect opt-in there is no shim, and the honest
    // answer is a warning rather than the silent drop this path used to do.
    let mut wrote_pending = false;
    if !spec.env.is_empty() {
        match spec.redirect_appid {
            Some(appid) => match crate::redirect::write_app_env(appid, &spec.env) {
                Ok(0) => tracing::warn!(
                    "steam-launch[{}]: none of this app's env vars are usable shell variable \
                     names — nothing will be applied to appid {appid}",
                    spec.slug
                ),
                Ok(n) => {
                    tracing::info!(
                        "steam-launch[{}]: armed {n} env var(s) for appid {appid} via the \
                         redirect shim (applied while the stream is live)",
                        spec.slug
                    );
                    match crate::redirect::write_app_env_pending(appid) {
                        Ok(()) => wrote_pending = true,
                        Err(e) => tracing::warn!(
                            "steam-launch[{}]: app-env fallback pointer not written \
                             (non-fatal): {e}",
                            spec.slug
                        ),
                    }
                }
                Err(e) => tracing::warn!(
                    "steam-launch[{}]: writing app env for appid {appid} failed — the game will \
                     launch WITHOUT those vars: {e}",
                    spec.slug
                ),
            },
            None => {
                let why = if spec.appid.is_none() {
                    "this launch line isn't a `rungameid` handoff, so there's no appid to arm — \
                     put the vars in the game's Steam launch options as `VAR=value %command%`"
                } else if steam_session_active() {
                    "a gamescope Steam session owns this launch, and it inherits the env of the \
                     SESSION entry — put the vars there instead"
                } else if !crate::redirect::enabled() {
                    "the seamless redirect is off, and it owns the only path that reaches a \
                     Steam-forked game — set PHAROS_STEAM_REDIRECT=1, or put the vars in the \
                     game's Steam launch options as `VAR=value %command%`"
                } else {
                    "this app isn't launching into the virtual display, so the redirect (and \
                     its env) doesn't apply"
                };
                tracing::warn!(
                    "steam-launch[{}]: {} env var(s) set on this app will NOT reach the game: {}",
                    spec.slug,
                    spec.env.len(),
                    why
                );
            }
        }
    }

    if !crate::steam::is_ready() {
        tracing::info!(
            "steam-launch[{}]: Steam not ready — pre-warming and waiting up to {}s",
            spec.slug,
            STEAM_READY_TIMEOUT.as_secs()
        );
        crate::steam::prewarm();
        if !crate::steam::wait_ready(STEAM_READY_TIMEOUT) {
            tracing::warn!(
                "steam-launch[{}]: Steam did not become ready within {}s — \
                 dropping handoff; start Steam manually or retry",
                spec.slug,
                STEAM_READY_TIMEOUT.as_secs()
            );
            return;
        }
        std::thread::sleep(POST_PIPE_GRACE);
        tracing::info!(
            "steam-launch[{}]: Steam ready, proceeding with handoff",
            spec.slug
        );
    }
    match spawn_steam_url(&spec) {
        Ok(mut child) => {
            tracing::info!(
                "steam-launch[{}]: handoff spawned (pid {}, line `{}`)",
                spec.slug,
                child.id(),
                spec.line
            );
            // The URI-handoff process exits almost immediately after delivering
            // the URL to Steam; reap it here so we don't leak a zombie.
            let _ = child.wait();
        }
        Err(e) => {
            tracing::warn!(
                "steam-launch[{}]: failed to spawn handoff `{}`: {e}",
                spec.slug,
                spec.line
            );
        }
    }

    // Retire the app-env fallback pointer once the launch window has closed. It
    // only matters for a Steam that doesn't export `SteamAppId`, and it names one
    // appid, so it must not outlive this handoff and mislabel the next game the
    // user starts by hand. Freshness enforced here rather than by an mtime test in
    // the shim, where the portable spellings run out. The per-appid fragment stays
    // (it is addressed by appid, so it can't be applied to the wrong game) until
    // the next compositor start sweeps it.
    if wrote_pending {
        std::thread::sleep(APP_ENV_PENDING_TTL);
        crate::redirect::clear_app_env_pending();
    }
}

/// Build + spawn the Steam URI-handoff `Command` for an owned `SteamHandoffSpec`.
/// Mirrors the env/cwd setup the inline path used to do — virtual-display socket,
/// scrubbed `DISPLAY`, per-entry cwd, per-entry env overrides.
fn spawn_steam_url(spec: &SteamHandoffSpec) -> std::io::Result<Child> {
    let mut cmd = build_command_from_parts(&spec.slug, &spec.line, spec.shell, spec.kind_shell)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    if let Some(sock) = &spec.wayland_socket {
        cmd.env("WAYLAND_DISPLAY", sock);
    }
    match &spec.x11_display {
        Some(d) => {
            cmd.env("DISPLAY", d);
        }
        None => {
            cmd.env_remove("DISPLAY");
        }
    }
    // Best-effort for Steam titles: set `PULSE_SINK` on the handoff so a Steam
    // *pre-warmed by us* (see [`steam::prewarm`]) and its games inherit it. A
    // Steam already running outside our launch won't — the compositor's
    // sink-input reconciler covers that case.
    if let Some(sink) = &spec.audio_sink {
        cmd.env("PULSE_SINK", sink);
    }
    if let Some(dir) = &spec.cwd {
        if !dir.trim().is_empty() {
            cmd.current_dir(PathBuf::from(dir));
        }
    }
    apply_entry_env(&mut cmd, &spec.slug, &spec.env);
    cmd.spawn()
}

/// Shared command construction used by both the inline (non-Steam) path via
/// [`build_command`] and the detached Steam worker. Splits the line, applies
/// the shell wrapper if requested, and returns a base `Command`.
fn build_command_from_parts(
    slug: &str,
    line: &str,
    shell: bool,
    kind_shell: bool,
) -> anyhow::Result<Command> {
    let line = line.trim();
    if shell || kind_shell {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // `cmd.exe /C <line>` must be built with raw_arg, NOT arg(). cmd does
            // not parse its tail with CommandLineToArgvW rules, so std's default
            // arg-escaping — which backslash-escapes embedded quotes — corrupts a
            // line like `start "" steam://rungameid/<id>`: cmd/`start` see `\"\"`
            // and try to launch `\\`, which pops a "Windows cannot find '\\'"
            // dialog and drops the URI on the floor (the Steam handoff never
            // reaches Steam — observed live on the rig, issue #22). raw_arg passes
            // the `/C <line>` verbatim so the quotes we wrote survive to cmd. The
            // first token after `/C` is a bareword (`start`/an exe), never a
            // quote, so cmd's outer-quote-stripping rule doesn't fire.
            let mut c = Command::new("cmd");
            c.raw_arg(format!("/C {line}"));
            return Ok(c);
        }
        #[cfg(not(windows))]
        {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(line);
            return Ok(c);
        }
    }
    let parts = tokenize(line);
    let mut it = parts.into_iter();
    let program = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("app `{}` has an empty command", slug))?;
    let mut c = Command::new(program);
    c.args(it);
    Ok(c)
}

/// Process-wide registry of the last child PID we spawned per slug, for
/// idempotency. Best-effort: a `setsid -f`-detached child (Steam) exits almost
/// immediately so its pid won't stay live — which is fine, Steam dedupes itself;
/// the registry's real value is the long-lived direct-exec entries.
fn registry() -> &'static Mutex<HashMap<String, u32>> {
    static LAUNCHED: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    LAUNCHED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Extract the Steam appid from a `…steam://rungameid/<id>…` launch line.
fn parse_steam_appid(line: &str) -> Option<u32> {
    let after = line.split("rungameid/").nth(1)?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Is a Steam game with `appid` already running? Scans `/proc/*/cmdline` for a
/// process referencing this appid — the `reaper … AppId=<id>` wrapper or a
/// `rungameid/<id>` handoff still in flight. This is the running-game check the
/// pid-registry can't do for Steam (the handoff pid dies instantly). Linux-only;
/// elsewhere `false` so the dedup degrades to "let Steam decide". Cheap enough at
/// user-launch frequency (a one-shot /proc walk).
#[cfg(target_os = "linux")]
fn steam_game_running(appid: u32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        // cmdline is NUL-separated; flatten to spaces, then match the appid as a
        // WHOLE field (not a substring — else appid 10 matches a running 1091500).
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline_has_appid(&cmd, appid) {
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn steam_game_running(_appid: u32) -> bool {
    false
}

/// Record a successful spawn. A poisoned lock is recovered (we only ever hold it
/// briefly and never panic under it) so a launch elsewhere can't wedge launches.
fn register_pid(slug: &str, pid: u32) {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(slug.to_string(), pid);
}

/// Companion processes a launch spawned ALONGSIDE the app (today: the per-app
/// rootful Xwayland), keyed by slug. Not the app's own tree — [`end_app`] adds
/// live companions to its kill roots and drops the slot either way.
#[cfg(target_os = "linux")]
fn companion_registry() -> &'static Mutex<HashMap<String, Vec<u32>>> {
    static COMPANIONS: OnceLock<Mutex<HashMap<String, Vec<u32>>>> = OnceLock::new();
    COMPANIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a companion process for `slug` (see [`companion_registry`]).
#[cfg(target_os = "linux")]
fn register_companion(slug: &str, pid: u32) {
    companion_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(slug.to_string())
        .or_default()
        .push(pid);
}

/// Remove and return `slug`'s still-alive companions (dead ones are dropped).
#[cfg(target_os = "linux")]
fn take_live_companions(slug: &str) -> Vec<u32> {
    companion_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(slug)
        .unwrap_or_default()
        .into_iter()
        .filter(|&p| pid_alive(p))
        .collect()
}

/// The registered pid for `slug` *if it is still alive*. A stale (dead) entry is
/// pruned and treated as absent so the next launch spawns fresh.
fn live_registered_pid(slug: &str) -> Option<u32> {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let pid = *map.get(slug)?;
    if pid_alive(pid) {
        Some(pid)
    } else {
        map.remove(slug);
        None
    }
}

/// Is `pid` a live process? Linux: the existence of `/proc/<pid>` (cheap, no
/// signal, no extra deps). Other platforms: conservatively report *not* alive so
/// idempotency degrades to "always relaunch" rather than wrongly suppressing a
/// launch.
fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // A ZOMBIE (defunct, state 'Z') process still has a /proc/<pid> entry, so a
        // bare `.exists()` reported it "alive". That latched `steam_session_active`
        // on a gamescope that had EXITED but whose Child was never reaped —
        // permanently suppressing the game-redirect marker so every game silently
        // fell through to the real desktop instead of the stream. Read the process
        // state and count a zombie/dead process as gone. Unparseable-but-present is
        // conservatively treated as alive (don't falsely reap a real process).
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat_state_char(&stat).is_none_or(|c| c != 'Z' && c != 'X' && c != 'x'),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// The process state char (field 3 of `/proc/<pid>/stat`): the first token after
/// the `(comm)` field. `comm` can itself contain spaces AND parens, so anchor on
/// the LAST `')'` (same trick as [`proc_starttime`]). `None` if malformed.
/// `'Z'` = zombie, `'X'`/`'x'` = dead; `'R'`/`'S'`/`'D'`/`'T'` are live.
#[cfg(any(target_os = "linux", test))]
fn stat_state_char(stat: &str) -> Option<char> {
    let rparen = stat.rfind(')')?;
    stat[rparen + 1..].split_whitespace().next()?.chars().next()
}

// ── Running-state + End-process (the Trireme app-selector feature) ──────────
//
// Two launch shapes, two truths:
//   • direct apps (Firefox, native games) — we own the `Child`; its pid roots the
//     tree, and `register_pid` already tracks it.
//   • Steam games — `steam://rungameid/N` is handed to the ALREADY-running Steam
//     client, which spawns the game under ITS own tree (a `reaper SteamLaunch
//     AppId=N` root), NOT under any process we control. So a per-launch systemd
//     scope would be empty; the only handle is the appid, found by scanning /proc.
//
// "Is it running" reuses the existing /proc scan (Steam) + pid registry (direct).
// "End process" kills the FULL tree from the right root — for Steam that's the
// reaper, whose descendants include stragglers like CrashReporter.exe.

/// Resolve an entry's concrete launch line (+ shell flag), mirroring [`launch`]'s
/// front matter, so running/kill can detect the Steam handoff + appid.
/// Linux-only like its callers (the /proc-based running/kill machinery).
#[cfg(target_os = "linux")]
fn resolved_line(entry: &AppEntry) -> (String, bool) {
    let kind = entry.resolved_kind();
    let install = detect_steam();
    install.launch_command(&kind)
}

/// The Steam appid this entry launches, if any (it hands off to Steam by URI).
/// `appid 0` is rejected — `rungameid/0` is a Steam sentinel, not a real game, and
/// an over-broad `AppId=0` marker would sweep unrelated processes.
#[cfg(target_os = "linux")]
fn entry_steam_appid(entry: &AppEntry) -> Option<u32> {
    let (line, _) = resolved_line(entry);
    appid_for_line(line.trim())
}

/// The Steam appid a resolved launch line targets (0 / non-Steam → None). Split
/// from [`entry_steam_appid`] so callers that already resolved the line (and the
/// Steam install) don't re-probe the filesystem per entry.
#[cfg(target_os = "linux")]
fn appid_for_line(line: &str) -> Option<u32> {
    if launches_via_steam(line) {
        parse_steam_appid(line).filter(|&a| a != 0)
    } else {
        None
    }
}

/// Pull every Steam appid referenced in a cmdline — each whole `AppId=<n>` and
/// `rungameid/<n>` field (the digit run right after the marker), parsed to u32.
/// WHOLE-FIELD, not substring: this is what makes appid 10 (Counter-Strike) NOT
/// match a running 1091500 (Cyberpunk) — the bug that would otherwise SIGKILL the
/// wrong game. `0` is dropped (Steam sentinel).
#[cfg(target_os = "linux")]
fn extract_appids(cmd: &str, out: &mut std::collections::HashSet<u32>) {
    for marker in ["AppId=", "rungameid/"] {
        let mut rest = cmd;
        while let Some(pos) = rest.find(marker) {
            let after = &rest[pos + marker.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u32>() {
                if n != 0 {
                    out.insert(n);
                }
            }
            rest = after;
        }
    }
}

/// Does this cmdline reference `appid` as a whole field?
#[cfg(target_os = "linux")]
fn cmdline_has_appid(cmd: &str, appid: u32) -> bool {
    let mut set = std::collections::HashSet::new();
    extract_appids(cmd, &mut set);
    set.contains(&appid)
}

/// The set of slugs in `entries` that are currently running, computed in ONE
/// /proc pass (so the catalog poll doesn't walk /proc per app). Steam install is
/// detected ONCE (it never moves at runtime); running Steam appids are extracted
/// from the snapshot into a set for O(1) per-entry membership. Used to set
/// [`core_proto::AppInfo`]-style running flags and to detect running changes.
pub fn running_slugs(entries: &[AppEntry]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    #[cfg(target_os = "linux")]
    {
        // One Steam-install probe + one /proc snapshot for the whole set.
        let install = detect_steam();
        let cmdlines = collect_cmdlines();
        let mut running_ids = std::collections::HashSet::new();
        for c in &cmdlines {
            extract_appids(c, &mut running_ids);
        }
        for e in entries {
            // Direct app: cheap registry + /proc/<pid> existence check.
            if live_registered_pid(&e.slug).is_some() {
                out.insert(e.slug.clone());
                continue;
            }
            // Steam game: its appid present in the running-appid set.
            let (line, _) = install.launch_command(&e.resolved_kind());
            if let Some(appid) = appid_for_line(line.trim()) {
                if running_ids.contains(&appid) {
                    out.insert(e.slug.clone());
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        for e in entries {
            if live_registered_pid(&e.slug).is_some() {
                out.insert(e.slug.clone());
            }
        }
    }
    out
}

/// End `entry`'s app — kill its whole process tree, including stragglers. For a
/// Steam game that's the reaper tree (found by appid); for a direct app it's the
/// registered pid's tree. SIGTERM, brief grace, then SIGKILL survivors. No-op
/// (Ok) if nothing matching is running.
pub fn end_app(entry: &AppEntry) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut roots: Vec<u32> = if let Some(appid) = entry_steam_appid(entry) {
            // Every process whose cmdline references the appid (the reaper + the
            // proton wrapper); tree-walking each catches the game .exe, wine
            // services, AND the crash reporter (all reaper descendants).
            steam_appid_roots(appid)
        } else if let Some(pid) = live_registered_pid(&entry.slug) {
            vec![pid]
        } else {
            Vec::new()
        };
        // Companions (a per-app Xwayland) die with the app. `-terminate` usually
        // reaps them on its own once the last X client is gone; this covers a
        // wedged client and the pre-first-client window.
        roots.extend(take_live_companions(&entry.slug));
        if roots.is_empty() {
            tracing::info!("end_app `{}`: nothing running", entry.slug);
            return Ok(());
        }
        // Union of every root's process tree (dedup), kill children-first.
        let mut all = std::collections::BTreeSet::new();
        for r in roots {
            for p in process_tree(r) {
                all.insert(p);
            }
        }
        let pids: Vec<u32> = all.into_iter().collect();
        tracing::info!(
            "end_app `{}`: killing {} pids {:?}",
            entry.slug,
            pids.len(),
            pids
        );
        // Snapshot each pid's start time so the SIGKILL pass can't hit a RECYCLED
        // pid: across the 600ms grace a pid can exit and its number be reused by an
        // unrelated process. We only SIGKILL survivors whose start time is
        // unchanged (provably the same process we SIGTERM'd).
        let starts: Vec<(u32, Option<u64>)> =
            pids.iter().map(|&p| (p, proc_starttime(p))).collect();
        kill_pids(&pids, "TERM");
        std::thread::sleep(Duration::from_millis(600));
        let survivors: Vec<u32> = starts
            .into_iter()
            .filter(|&(p, st)| pid_alive(p) && proc_starttime(p) == st)
            .map(|(p, _)| p)
            .collect();
        if !survivors.is_empty() {
            kill_pids(&survivors, "KILL");
        }
        // Drop any stale registry entry so a relaunch spawns fresh.
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&entry.slug);
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = entry;
        anyhow::bail!("end_app is only implemented on Linux");
    }
}

/// All pids whose cmdline references this Steam appid — the tree roots to kill.
#[cfg(target_os = "linux")]
fn steam_appid_roots(appid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline_has_appid(&cmd, appid) {
            out.push(pid);
        }
    }
    out
}

/// A process's start time (jiffies since boot — `/proc/<pid>/stat` field 22),
/// used to detect pid recycling. The comm field (field 2) is parenthesized and
/// may contain spaces/parens, so we split AFTER the final ')'; field 22 is then
/// index 19 of the remaining tokens (fields 3..). `None` if the process is gone.
#[cfg(target_os = "linux")]
fn proc_starttime(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = s.rfind(')')?;
    let after: Vec<&str> = s[rparen + 1..].split_whitespace().collect();
    after.get(19).and_then(|t| t.parse::<u64>().ok())
}

/// Snapshot every process's cmdline (NUL→space) in one /proc pass.
#[cfg(target_os = "linux")]
fn collect_cmdlines() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(raw) = std::fs::read(e.path().join("cmdline")) {
            out.push(String::from_utf8_lossy(&raw).replace('\0', " "));
        }
    }
    out
}

/// A pid and all its descendants (BFS over `/proc/<pid>/task/<tid>/children`).
/// Children-first ordering is achieved by the caller killing the returned set;
/// since we SIGTERM the whole set at once then SIGKILL survivors, exact order is
/// not required — the cgroup of a reaper keeps its tree intact for the walk.
#[cfg(target_os = "linux")]
fn process_tree(root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        let task_dir = format!("/proc/{pid}/task");
        let Ok(threads) = std::fs::read_dir(&task_dir) else {
            continue;
        };
        for t in threads.flatten() {
            if let Ok(s) = std::fs::read_to_string(t.path().join("children")) {
                for c in s.split_whitespace() {
                    if let Ok(cpid) = c.parse::<u32>() {
                        stack.push(cpid);
                    }
                }
            }
        }
    }
    out
}

/// Send `sig` (TERM/KILL) to `pids` via an ABSOLUTE-path kill (no extra
/// signal-crate dep, and not PATH-dependent — a minimal systemd unit env may lack
/// /usr/bin). Tries /usr/bin/kill then /bin/kill; logs if neither succeeds rather
/// than silently reporting success while killing nothing.
#[cfg(target_os = "linux")]
fn kill_pids(pids: &[u32], sig: &str) {
    if pids.is_empty() {
        return;
    }
    for kill_bin in ["/usr/bin/kill", "/bin/kill"] {
        if !std::path::Path::new(kill_bin).exists() {
            continue;
        }
        let mut c = Command::new(kill_bin);
        c.arg(format!("-{sig}"));
        for p in pids {
            c.arg(p.to_string());
        }
        match c.status() {
            Ok(st) if st.success() => return,
            Ok(st) => tracing::warn!("kill -{sig} exited {st}; pids {pids:?}"),
            Err(e) => tracing::warn!("spawn {kill_bin}: {e}"),
        }
        return;
    }
    tracing::warn!("no kill binary found (/usr/bin/kill, /bin/kill); {pids:?} not signalled");
}

/// True if this command line hands off to the Steam client — a `steam://` URI or the
/// `steam` binary as the program — i.e. the launches that need a warm Steam.
fn launches_via_steam(line: &str) -> bool {
    line.contains("steam://")
        || tokenize(line)
            .iter()
            .any(|t| t.rsplit('/').next().unwrap_or(t) == "steam")
}

/// Build the base [`Command`] from the resolved command `line`, honoring the
/// shell flag. `kind_shell` is set when the *structured* kind needs the platform
/// shell (a Windows `steam://` URI handed to the shell handler); it ORs with the
/// entry's own `shell` so a legacy entry that opted into the shell still gets it.
fn build_command(entry: &AppEntry, line: &str, kind_shell: bool) -> anyhow::Result<Command> {
    build_command_from_parts(&entry.slug, line, entry.shell, kind_shell)
}

/// Split a command line into tokens on whitespace, but keep `"double quoted"`
/// runs together so a path with spaces (`"C:\Program Files\game.exe"`) survives
/// a non-shell launch. Backslashes are preserved verbatim (Windows paths); only
/// the double quote is special.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut has_token = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppKind, LaunchKind, LaunchTarget};

    #[test]
    fn stat_state_char_parses_zombie_and_tricky_comm() {
        // Normal running process.
        assert_eq!(stat_state_char("1234 (gamescope) R 1 1234 ..."), Some('R'));
        // Sleeping.
        assert_eq!(stat_state_char("42 (bash) S 1 42 ..."), Some('S'));
        // ZOMBIE — the case that latched steam_session_active (must read as dead).
        assert_eq!(stat_state_char("999 (gamescope) Z 1 999 ..."), Some('Z'));
        // comm containing spaces AND parens must not fool the parser: anchor on the
        // LAST ')'. State is still the first token after it.
        assert_eq!(
            stat_state_char("7 (weird )(name) Z 1 7 ..."),
            Some('Z'),
            "comm with embedded parens/spaces"
        );
        // Malformed → None (pid_alive then conservatively treats present-as-alive).
        assert_eq!(stat_state_char("garbage-no-paren"), None);
    }

    #[test]
    fn zombie_state_reads_as_not_alive() {
        // The exact predicate pid_alive applies to the parsed state: a zombie/dead
        // process is NOT alive; R/S/D/T are.
        let alive =
            |stat: &str| stat_state_char(stat).is_none_or(|c| c != 'Z' && c != 'X' && c != 'x');
        assert!(!alive("1 (g) Z 0 ..."), "zombie is not alive");
        assert!(!alive("1 (g) X 0 ..."), "dead is not alive");
        assert!(alive("1 (g) R 0 ..."));
        assert!(alive("1 (g) S 0 ..."));
        assert!(
            alive("no-parens-conservatively-alive"),
            "unparseable present ⇒ alive"
        );
    }

    #[test]
    fn tokenize_respects_quotes() {
        assert_eq!(
            tokenize("steam steam://rungameid/10"),
            vec!["steam", "steam://rungameid/10"]
        );
        assert_eq!(
            tokenize("\"/opt/My Game/run.sh\" --fast"),
            vec!["/opt/My Game/run.sh", "--fast"]
        );
        assert_eq!(tokenize("   "), Vec::<String>::new());
    }

    #[test]
    fn mangohud_env_only_when_capped() {
        // No cap, no overlay → no env (the app is untouched).
        assert!(mangohud_env(None, false).is_empty());
        // A cap → MANGOHUD=1 plus an fps_limit carrying the resolved number,
        // the early (low-latency) method, and an invisible (no_display) limiter.
        let env = mangohud_env(Some(60), false);
        assert!(env.iter().any(|(k, v)| k == "MANGOHUD" && v == "1"));
        let cfg = &env.iter().find(|(k, _)| k == "MANGOHUD_CONFIG").unwrap().1;
        assert!(
            cfg.contains("fps_limit=60"),
            "config carries the number: {cfg}"
        );
        assert!(
            cfg.contains("fps_limit_method=early"),
            "low-latency method: {cfg}"
        );
        assert!(cfg.contains("no_display=1"), "pure limiter, no HUD: {cfg}");
    }

    #[test]
    fn mangohud_env_overlay() {
        // Overlay, no cap → MangoHud on with a VISIBLE HUD and no fps_limit.
        let env = mangohud_env(None, true);
        assert!(env.iter().any(|(k, v)| k == "MANGOHUD" && v == "1"));
        let cfg = &env.iter().find(|(k, _)| k == "MANGOHUD_CONFIG").unwrap().1;
        assert!(cfg.contains("no_display=0"), "HUD visible: {cfg}");
        assert!(!cfg.contains("fps_limit"), "no cap requested: {cfg}");
        // Overlay AND cap → visible HUD that also limits.
        let env = mangohud_env(Some(90), true);
        let cfg = &env.iter().find(|(k, _)| k == "MANGOHUD_CONFIG").unwrap().1;
        assert!(
            cfg.contains("fps_limit=90") && cfg.contains("no_display=0"),
            "{cfg}"
        );
    }

    /// Regression for the issue #22 live bug: the Steam handoff line
    /// `start "" <uri>` was corrupted to `\"\"` by std's arg-escaping when handed
    /// to `cmd /C` via `arg()`, so `start` tried to launch `\\` (a "Windows
    /// cannot find '\\'" dialog) and the URI never reached Steam. The `raw_arg`
    /// fix passes `/C <line>` verbatim. We can't read a `Command`'s line back, so
    /// prove it through cmd itself: echo an empty-quote pair and confirm cmd saw
    /// `""`, not the backslash-escaped `\"\"`.
    #[cfg(windows)]
    #[test]
    fn windows_shell_line_quotes_survive_to_cmd() {
        let mut cmd = build_command_from_parts("t", "echo \"\"", false, true).unwrap();
        let out = cmd.output().expect("run cmd echo");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains('\\'),
            "cmd must not receive backslash-escaped quotes: {stdout:?}"
        );
        assert!(
            stdout.contains("\"\""),
            "the empty-quote pair must survive verbatim to cmd: {stdout:?}"
        );
    }

    #[test]
    fn empty_command_is_noop() {
        let e = AppEntry {
            slug: "desktop".into(),
            name: "Desktop".into(),
            kind: AppKind::Desktop,
            ..AppEntry::new()
        };
        let child = launch(&e, &LaunchContext::default()).unwrap();
        assert!(child.is_none());
    }

    /// `ExtendPhysical` is the one target still refused (unwired) on Linux —
    /// `DuplicatePhysical` was wired for issue #6 (desktop mirror mode) and no
    /// longer hits this guard; see `duplicate_physical_target_launches`.
    #[cfg(not(windows))]
    #[test]
    fn extend_physical_target_is_refused_with_clear_reason() {
        let e = AppEntry {
            slug: "x".into(),
            name: "X".into(),
            command: "true".into(),
            target: LaunchTarget::ExtendPhysical,
            ..AppEntry::new()
        };
        let err = launch(&e, &LaunchContext::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("PHYSICAL"), "reason must be explicit: {err}");
        assert!(err.contains("NVENC"), "reason must name the footgun: {err}");
    }

    /// Issue #6: `DuplicatePhysical` is wired (the compositor mirrors the
    /// real desktop instead of cold-spawning into it), so `launch` no longer
    /// refuses it on Linux — it spawns normally, same as a virtual target.
    #[cfg(not(windows))]
    #[test]
    fn duplicate_physical_target_launches() {
        let e = AppEntry {
            slug: format!("phys-dup-{}", std::process::id()),
            name: "X".into(),
            command: "true".into(),
            target: LaunchTarget::DuplicatePhysical,
            ..AppEntry::new()
        };
        let child = launch(&e, &LaunchContext::default())
            .expect("DuplicatePhysical must be wired, not refused");
        if let Some(mut c) = child {
            c.wait().ok();
        }
    }

    /// On Windows the guard is carved out (issue #22): the captured desktop IS a
    /// physical output, so a physical-family target spawns normally instead of
    /// being refused. Uses `ping -n` as the spawnable stand-in (ships in
    /// System32; there is no `true.exe`).
    #[cfg(windows)]
    #[test]
    fn physical_target_launches_on_windows() {
        for (i, target) in [
            LaunchTarget::ExtendPhysical,
            LaunchTarget::DuplicatePhysical,
        ]
        .into_iter()
        .enumerate()
        {
            let e = AppEntry {
                slug: format!("phys-win-{i}-{}", std::process::id()),
                name: "X".into(),
                command: "ping -n 1 127.0.0.1".into(),
                target,
                ..AppEntry::new()
            };
            let child = launch(&e, &LaunchContext::default())
                .expect("physical targets must not be refused on Windows");
            if let Some(mut c) = child {
                c.kill().ok();
                c.wait().ok();
            }
        }
    }

    #[test]
    fn idempotent_second_launch_of_live_pid_is_noop() {
        // First launch spawns; record the slug→pid in the registry. A long-lived
        // child keeps the pid alive across the second call: `sleep` on Unix,
        // `ping -n` on Windows (ships in System32; there is no `sleep.exe`).
        let slug = format!("idem-test-{}", std::process::id());
        let long_lived = if cfg!(windows) {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };
        let e = AppEntry {
            slug: slug.clone(),
            name: "Idem".into(),
            command: long_lived.into(),
            ..AppEntry::new()
        };
        let first = launch(&e, &LaunchContext::default()).expect("first launch");
        // On Linux the registry tracks the (alive) sleep; the second launch must
        // detect it and no-op. Elsewhere pid_alive() is conservatively false, so
        // we only assert the strong property on Linux.
        if cfg!(target_os = "linux") {
            let mut child = first.expect("first launch spawned a child");
            let second = launch(&e, &LaunchContext::default()).expect("second launch");
            assert!(
                second.is_none(),
                "second launch of a live pid must be a no-op"
            );
            child.kill().ok();
            child.wait().ok();
        } else if let Some(mut c) = first {
            c.kill().ok();
            c.wait().ok();
        }
    }

    #[test]
    fn dead_pid_relaunches() {
        // A slug whose recorded pid is dead must NOT be suppressed: register an
        // impossible pid, then a launch should fall through to a fresh spawn.
        let slug = format!("dead-test-{}", std::process::id());
        register_pid(&slug, u32::MAX); // /proc/4294967295 never exists
        assert!(
            live_registered_pid(&slug).is_none(),
            "a dead pid must be pruned and treated as absent"
        );
    }

    /// The gamescope argv is deterministic and ordered: integration+fullscreen,
    /// output size, refresh (only when known), user extras, then `-- <client>
    /// -gamepadui` (+ the boot-to-game URI when given). This is the whole
    /// session command — worth pinning exactly.
    #[cfg(target_os = "linux")]
    #[test]
    fn steam_session_args_shape() {
        let a = steam_session_args(
            2560,
            1440,
            Some(60),
            &["--hdr-enabled".into()],
            &["steam".into()],
            None,
            &[],
        );
        let want: Vec<String> = [
            "-e",
            "-f",
            "-W",
            "2560",
            "-H",
            "1440",
            "-r",
            "60",
            "--hdr-enabled",
            "--",
            "steam",
            "-gamepadui",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(a, want);
        // Unknown refresh → no -r flag at all rather than a guess.
        let a = steam_session_args(1920, 1080, None, &[], &["steam".into()], None, &[]);
        assert!(!a.contains(&"-r".to_string()), "{a:?}");
        // Boot-to-game: the autolaunch URI rides LAST, after -gamepadui, so the
        // client parses all its flags before the game URI (the SteamOS shape).
        let a = steam_session_args(
            1920,
            1080,
            None,
            &[],
            &["steam".into()],
            Some("steam://rungameid/275850"),
            &[],
        );
        assert_eq!(
            a.last().map(String::as_str),
            Some("steam://rungameid/275850")
        );
        assert_eq!(a[a.len() - 2], "-gamepadui");

        // Layer env rides an `env` prefix on the NESTED command, immediately
        // after `--`: gamescope is itself a Vulkan client of the compositor, so a
        // layer armed in its own environment would post-process the session too.
        let a = steam_session_args(
            1920,
            1080,
            None,
            &[],
            &["steam".into()],
            None,
            &[("ENABLE_RESHADE".into(), "1".into())],
        );
        let dashdash = a.iter().position(|s| s == "--").expect("-- present");
        assert_eq!(a[dashdash + 1], "env");
        assert_eq!(a[dashdash + 2], "ENABLE_RESHADE=1");
        assert_eq!(a[dashdash + 3], "steam");
    }

    #[test]
    fn hand_typed_env_wins_over_a_layer_toggle() {
        // The escape hatch has to be able to beat the typed control, including
        // pointing VK_LOADER_LAYERS_ENABLE somewhere else entirely.
        let merged = merge_env(
            vec![
                ("ENABLE_RESHADE".into(), "1".into()),
                ("VK_LOADER_LAYERS_ENABLE".into(), "VK_LAYER_reshade".into()),
            ],
            &[(
                "VK_LOADER_LAYERS_ENABLE".to_string(),
                "VK_LAYER_x".to_string(),
            )],
        );
        assert_eq!(
            merged.last(),
            Some(&(
                "VK_LOADER_LAYERS_ENABLE".to_string(),
                "VK_LAYER_x".to_string()
            ))
        );
        // With no toggles the effective env is exactly the entry's own, and no
        // manifest scan happens at all.
        let entry = AppEntry {
            env: vec![("A".into(), "1".into())],
            ..AppEntry::new()
        };
        assert_eq!(effective_env(&entry), entry.env);
    }

    #[test]
    fn env_prefix_argv_is_empty_when_there_is_nothing_to_carry() {
        // No toggles => no `env` wrapper at all, so the nested command stays
        // byte-identical to what it was before this feature existed.
        assert!(env_prefix_argv(&[]).is_empty());
        assert_eq!(
            env_prefix_argv(&[("A".into(), "1".into()), ("B".into(), "x y".into())]),
            vec!["env".to_string(), "A=1".to_string(), "B=x y".to_string()]
        );
    }

    /// A steam-session pointed at the physical desktop makes no sense (the
    /// session exists to own the virtual display) and must be refused with the
    /// reason, same policy as the generic physical-target guard.
    #[cfg(target_os = "linux")]
    #[test]
    fn steam_session_physical_target_is_refused() {
        let e = AppEntry {
            slug: "bp-phys".into(),
            name: "BP".into(),
            launch: Some(LaunchKind::SteamSession),
            target: LaunchTarget::DuplicatePhysical,
            ..AppEntry::new()
        };
        let err = launch(&e, &LaunchContext::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("PHYSICAL"), "reason must be explicit: {err}");
    }

    #[test]
    fn structured_steam_game_builds_rungameid_command() {
        // A SteamGame kind resolves to a steam://rungameid/<appid> command via the
        // detected install (or the None fallback), independent of `command`.
        let e = AppEntry {
            slug: "elden-ring".into(),
            name: "Elden Ring".into(),
            kind: AppKind::Game,
            command: "stale-or-empty".into(),
            launch: Some(LaunchKind::SteamGame { appid: 1245620 }),
            ..AppEntry::new()
        };
        let install = detect_steam();
        let (line, _shell) = install.launch_command(&e.resolved_kind());
        assert!(
            line.contains("steam://rungameid/1245620"),
            "structured Steam launch must build the rungameid URI, got: {line}"
        );
    }

    #[test]
    fn raw_command_falls_back_to_command_string() {
        // No `launch` table → resolved_kind is a RawCommand from `command`.
        let e = AppEntry {
            slug: "ff".into(),
            name: "Firefox".into(),
            command: "firefox --new-window".into(),
            ..AppEntry::new()
        };
        match e.resolved_kind() {
            LaunchKind::RawCommand { line } => assert_eq!(line, "firefox --new-window"),
            other => panic!("expected RawCommand fallback, got {other:?}"),
        }
    }
}
