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

use crate::config::AppEntry;
use crate::env::{classify_target, detect_steam};

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
        }
    }

    /// Record the live virtual output's refresh (whole FPS), used to resolve an
    /// [`FpsCap::Auto`](crate::config::FpsCap::Auto) at launch.
    pub fn with_output_fps(mut self, fps: u32) -> Self {
        self.output_fps = Some(fps);
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
    let install = detect_steam();
    let (line_owned, kind_shell) = install.launch_command(&kind);
    let line = line_owned.trim();

    if line.is_empty() {
        // The Desktop built-in (and any intentionally-blank entry): the virtual
        // display already *is* the desktop, so there's nothing to spawn.
        tracing::info!("app `{}`: no command (desktop / no-op)", entry.slug);
        return Ok(None);
    }

    // Physical-desktop footgun guard. Extend/duplicate the host's *real* session
    // is the dangerous target: a cold-started GPU app lands on the live desktop
    // AND can SIGSEGV NVENC. Refuse explicitly — never silently spawn onto the
    // physical session. This is the spawn-side half of `is_supported()` gating,
    // with the *reason* spelled out. (When the physical seam is finally wired,
    // this stays the cold-start refusal; a future warm/attach path would carve
    // out its own branch rather than dropping the guard.)
    if classify_target(&entry.target).is_physical() {
        anyhow::bail!(
            "app `{}` targets {:?} (the host's PHYSICAL desktop), which the \
             launcher refuses to cold-start into: a fresh GPU app would appear on \
             the real session being captured AND can crash the NVENC encoder \
             (the documented libnvcuvid SIGSEGV). Extend/duplicate the physical \
             desktop as a stream source is a planned feature; until it's wired to \
             attach to an already-running session, launching into it is gated off. \
             Use a virtual-display target instead.",
            entry.slug,
            entry.target
        );
    }

    // Defense in depth: any other not-yet-supported target (should be subsumed by
    // the physical guard above, but keep the explicit gate so a new LaunchTarget
    // variant can't silently slip through to a spawn).
    if !entry.target.is_supported() {
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
    for (k, v) in &entry.env {
        cmd.env(k, v);
    }

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
            .map(|paths| {
                std::env::split_paths(&paths).any(|dir| dir.join("mangohud").is_file())
            })
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
    // physical target must never be redirected) with the feature opted in.
    let redirect_appid = if ctx.wayland_socket.is_some() && crate::redirect::enabled() {
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
        env: entry
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        redirect_appid,
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
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
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
            let mut c = Command::new("cmd");
            c.arg("/C").arg(line);
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
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
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
fn resolved_line(entry: &AppEntry) -> (String, bool) {
    let kind = entry.resolved_kind();
    let install = detect_steam();
    install.launch_command(&kind)
}

/// The Steam appid this entry launches, if any (it hands off to Steam by URI).
/// `appid 0` is rejected — `rungameid/0` is a Steam sentinel, not a real game, and
/// an over-broad `AppId=0` marker would sweep unrelated processes.
fn entry_steam_appid(entry: &AppEntry) -> Option<u32> {
    let (line, _) = resolved_line(entry);
    appid_for_line(line.trim())
}

/// The Steam appid a resolved launch line targets (0 / non-Steam → None). Split
/// from [`entry_steam_appid`] so callers that already resolved the line (and the
/// Steam install) don't re-probe the filesystem per entry.
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
        let roots: Vec<u32> = if let Some(appid) = entry_steam_appid(entry) {
            // Every process whose cmdline references the appid (the reaper + the
            // proton wrapper); tree-walking each catches the game .exe, wine
            // services, AND the crash reporter (all reaper descendants).
            steam_appid_roots(appid)
        } else if let Some(pid) = live_registered_pid(&entry.slug) {
            vec![pid]
        } else {
            Vec::new()
        };
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
        assert!(cfg.contains("fps_limit=90") && cfg.contains("no_display=0"), "{cfg}");
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

    #[test]
    fn physical_target_is_refused_with_clear_reason() {
        for target in [
            LaunchTarget::ExtendPhysical,
            LaunchTarget::DuplicatePhysical,
        ] {
            let e = AppEntry {
                slug: "x".into(),
                name: "X".into(),
                command: "true".into(),
                target,
                ..AppEntry::new()
            };
            let err = launch(&e, &LaunchContext::default())
                .unwrap_err()
                .to_string();
            assert!(err.contains("PHYSICAL"), "reason must be explicit: {err}");
            assert!(err.contains("NVENC"), "reason must name the footgun: {err}");
        }
    }

    #[test]
    fn idempotent_second_launch_of_live_pid_is_noop() {
        // First launch spawns; record the slug→pid in the registry. A long-lived
        // child (`sleep`) keeps /proc/<pid> alive across the second call.
        let slug = format!("idem-test-{}", std::process::id());
        let e = AppEntry {
            slug: slug.clone(),
            name: "Idem".into(),
            command: "sleep 30".into(),
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
