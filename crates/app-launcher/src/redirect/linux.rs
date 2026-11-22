// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Linux redirect provider: a POSIX-sh Launch-Options shim + idempotent
//! `localconfig.vdf` injection, with the Steam-running clobber guard.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::Context;

use super::proton::{self, CompatEdit, CompatTool};
use super::vdf::{self, EditResult};
use super::{InjectOutcome, PinOutcome, RedirectId, RedirectProvider};
use crate::env::SteamInstall;

/// The shim basename — also the VDF "ours" token.
const SHIM_BASENAME: &str = "pharos-redirect";

/// The conditional redirect shim. Invoked by Steam as `pharos-redirect <full game
/// command…>` (from `pharos-redirect %command%`). Marker present + live ⇒ re-exec
/// the game into the stream env; otherwise exec unchanged (normal desktop play).
/// Passthrough is the failure mode of every uncertain branch, so a stale marker
/// can never hijack a desktop launch.
const SHIM_SCRIPT: &str = r#"#!/bin/sh
# pharos-redirect — Pharos conditional Steam-game display redirect.
# Installed and owned by Pharos; do not edit. Wrap a game's Steam Launch Options
# as:  pharos-redirect %command%
set -eu

marker="${XDG_RUNTIME_DIR:-/tmp}/pharos/launch-env"

# Per-launch opt-out (honored even mid-stream) or no marker => desktop passthrough.
if [ "${PHAROS_NO_REDIRECT:-}" = "1" ] || [ ! -r "$marker" ]; then
    exec env "$@"
fi

# shellcheck disable=SC1090
. "$marker"

# Staleness guard: the marker must describe a LIVE compositor (pid + start-ticks,
# defeating PID reuse) whose Wayland socket still exists. Any doubt => passthrough.
_live=0
if [ -n "${PHAROS_MARK_PID:-}" ] && [ -d "/proc/${PHAROS_MARK_PID}" ]; then
    _st=$(awk 'END{n=split($0,a,")"); print a[n]}' "/proc/${PHAROS_MARK_PID}/stat" 2>/dev/null | awk '{print $20}')
    if [ -n "${_st:-}" ] && [ "$_st" = "${PHAROS_MARK_START:-x}" ]; then
        if [ -S "${XDG_RUNTIME_DIR:-/tmp}/${PHAROS_MARK_SOCKET:-__none__}" ]; then
            _live=1
        fi
    fi
fi
[ "$_live" = "1" ] || exec env "$@"

# Redirect into the live stream. Scrub DISPLAY so a Wayland game can't fall back
# to the host session, then export the stream env (mirrors launch.rs). `exec env`
# (not bare `exec`) so a leading VAR=val in enveloped user options is honored as an
# assignment instead of being run as a program name.
unset DISPLAY
export WAYLAND_DISPLAY="${PHAROS_WAYLAND_DISPLAY}"
[ -n "${PHAROS_PULSE_SINK:-}" ] && export PULSE_SINK="${PHAROS_PULSE_SINK}"
export PROTON_ENABLE_WAYLAND="${PHAROS_PROTON_ENABLE_WAYLAND:-1}"

# Per-game env from this app's catalog entry (`apps.toml` `env`), written by the
# launcher as one fragment per appid. Sourced LAST so an entry's variable wins
# over the stream defaults above, mirroring launch.rs's "per-entry env applied
# last" rule. Steam exports SteamAppId/SteamGameId into the game environment we
# were exec'd in; if some Steam ever doesn't, fall back to the `pending` pointer
# the launcher wrote for the handoff in flight. No id, no fragment, or an id that
# isn't all digits => nothing sourced, launch unchanged.
_dir="${XDG_RUNTIME_DIR:-/tmp}/pharos/app-env"
_appid="${SteamAppId:-${SteamGameId:-}}"
case "${_appid:-x}" in
    ''|*[!0-9]*) _appid="" ;;
esac
if [ -z "$_appid" ] && [ -r "$_dir/pending" ]; then
    # shellcheck disable=SC1091
    . "$_dir/pending"
    _appid="${PHAROS_APP_ENV_APPID:-}"
    case "${_appid:-x}" in
        ''|*[!0-9]*) _appid="" ;;
    esac
fi
if [ -n "$_appid" ] && [ -r "$_dir/$_appid" ]; then
    # shellcheck disable=SC1090
    . "$_dir/$_appid"
fi

exec env "$@"
"#;

pub(super) struct LinuxShimProvider;

impl RedirectProvider for LinuxShimProvider {
    fn ours_token(&self) -> &str {
        SHIM_BASENAME
    }

    fn ensure_shim_installed(&self) -> anyhow::Result<()> {
        let path = shim_path();
        // Idempotent: only write when content differs (avoids churn + races).
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() != Some(SHIM_SCRIPT) {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create shim dir {}", dir.display()))?;
            }
            let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
            std::fs::write(&tmp, SHIM_SCRIPT)
                .with_context(|| format!("write shim {}", tmp.display()))?;
            set_executable(&tmp)?;
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("install shim {}", path.display()))?;
        } else {
            // Content current — still make sure it's executable (e.g. a prior
            // partial install left it 0644).
            set_executable(&path).ok();
        }
        Ok(())
    }

    fn inject(&self, install: &SteamInstall, id: RedirectId) -> anyhow::Result<InjectOutcome> {
        let Some(root) = install.root() else {
            return Ok(InjectOutcome::NoSteam);
        };
        let localconfig = match localconfig_path(root) {
            Ok(p) => p,
            Err(e) => return Ok(InjectOutcome::Refused(e.to_string())),
        };
        let shim_cmd = shim_path().display().to_string();
        // The value is written unquoted (Steam parses Launch Options shell-style),
        // so a shim path with a space would tokenise wrong. Rather than corrupt the
        // options, refuse — vanishingly rare ($XDG_DATA_HOME with a space).
        if shim_cmd.contains(char::is_whitespace) {
            return Ok(InjectOutcome::Refused(format!(
                "shim path {shim_cmd:?} contains whitespace — unsupported"
            )));
        }

        // Serialize the whole read-modify-write: every game launch fires its own
        // detached handoff worker (all in this one compositor process), so without
        // this two concurrent injections of DIFFERENT games would each read the
        // original file and the last rename would clobber the other's edit — a
        // lost update, or worse a torn write, on the user's LIVE config. Re-read
        // INSIDE the lock so each worker sees the prior worker's result.
        static WRITE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let text = std::fs::read_to_string(&localconfig)
            .with_context(|| format!("read {}", localconfig.display()))?;

        match vdf::set_launch_options(&text, id.appid, &shim_cmd, SHIM_BASENAME) {
            EditResult::AlreadyOurs => Ok(InjectOutcome::AlreadyOurs),
            EditResult::Refused => Ok(InjectOutcome::Refused(
                "Steam Launch Options are unsafe to auto-wrap (shell operators / quotes / \
                 multiple %command%) — left untouched"
                    .into(),
            )),
            EditResult::AppNotFound => Ok(InjectOutcome::Refused(format!(
                "appid {} not present in localconfig.vdf",
                id.appid
            ))),
            EditResult::Changed(new_text) => {
                // Clobber guard: a running Steam holds localconfig in memory and
                // rewrites it on exit — a live edit is both invisible and lost. So
                // only write when Steam is down; else defer (it arms on the next
                // Steam start, which Pharos performs when it cold-launches Steam).
                if steam_is_running() {
                    return Ok(InjectOutcome::Deferred);
                }
                backup_once(&localconfig)?;
                atomic_write(&localconfig, &new_text)?;
                Ok(InjectOutcome::Injected)
            }
        }
    }

    fn pin_proton(&self, install: &SteamInstall, id: RedirectId) -> anyhow::Result<PinOutcome> {
        let Some(root) = install.root() else {
            return Ok(PinOutcome::NoSteam);
        };
        let config = root.join("config").join("config.vdf");
        if !config.is_file() {
            return Ok(PinOutcome::Refused(format!(
                "{} not found — cannot pin Proton",
                config.display()
            )));
        }

        // Which Proton to pin. An explicit `PHAROS_STEAM_PROTON=<internal-name>`
        // wins verbatim (the user knows their install); otherwise auto-pick the
        // best winewayland-capable tool discovered under the compatibilitytools.d
        // dirs. Nothing suitable installed ⇒ decline rather than pin a Proton that
        // would also fall back to XWayland.
        let tools = discover_compat_tools(root);
        let target = match std::env::var("PHAROS_STEAM_PROTON")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(forced) => forced,
            None => match proton::choose_tool(&tools) {
                Some(t) => t.internal.clone(),
                None => return Ok(PinOutcome::NoSuitableProton),
            },
        };

        // Serialize config.vdf read-modify-write for the same lost-update reason as
        // the localconfig injector (each game launch runs its own worker).
        static WRITE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let text = std::fs::read_to_string(&config)
            .with_context(|| format!("read {}", config.display()))?;

        match proton::set_compat_tool(&text, id.appid, &target) {
            CompatEdit::AlreadyOurs => Ok(PinOutcome::AlreadyPinned),
            CompatEdit::Refused => Ok(PinOutcome::Refused(format!(
                "appid {} is force-set to a specific custom Proton — leaving it alone",
                id.appid
            ))),
            CompatEdit::NoSteamBlock => Ok(PinOutcome::Refused(format!(
                "{} has no Software>Valve>Steam block — not editing",
                config.display()
            ))),
            CompatEdit::Changed(new_text) => {
                // Same clobber guard as the launch-options write: a running Steam
                // holds config.vdf in memory and rewrites it on exit.
                if steam_is_running() {
                    return Ok(PinOutcome::Deferred);
                }
                backup_once(&config)?;
                atomic_write(&config, &new_text)?;
                Ok(PinOutcome::Pinned(target))
            }
        }
    }
}

/// Discover the custom compat tools installed under every `compatibilitytools.d`
/// Steam consults: the user dir (both `<root>/compatibilitytools.d` and the
/// canonical `~/.steam/root/compatibilitytools.d`, which may differ from `root`
/// on flatpak/snap) and the system dir `/usr/share/steam/compatibilitytools.d`.
/// Deduped by internal name (first occurrence wins, mirroring Steam's user-over-
/// system precedence).
fn discover_compat_tools(root: &Path) -> Vec<CompatTool> {
    let mut dirs: Vec<PathBuf> = vec![root.join("compatibilitytools.d")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".steam/root/compatibilitytools.d"));
    }
    dirs.push(PathBuf::from("/usr/share/steam/compatibilitytools.d"));

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<CompatTool> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let manifest = entry.path().join("compatibilitytool.vdf");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            for tool in proton::parse_compat_tools(&text) {
                if seen.insert(tool.internal.clone()) {
                    out.push(tool);
                }
            }
        }
    }
    out
}

/// `$XDG_DATA_HOME/pharos/pharos-redirect` (default `~/.local/share/…`).
fn shim_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pharos").join(SHIM_BASENAME)
}

/// Resolve the localconfig.vdf for the account Pharos will launch as. One user →
/// that user; several → the `MostRecent` from `loginusers.vdf`; ambiguous → error.
fn localconfig_path(root: &Path) -> anyhow::Result<PathBuf> {
    let userdata = root.join("userdata");
    let mut users: Vec<PathBuf> = std::fs::read_dir(&userdata)
        .with_context(|| format!("read {}", userdata.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("config/localconfig.vdf").is_file())
        .collect();
    users.sort();

    match users.len() {
        0 => anyhow::bail!(
            "no Steam userdata with localconfig.vdf under {}",
            userdata.display()
        ),
        1 => Ok(users.remove(0).join("config/localconfig.vdf")),
        _ => {
            // Prefer the MostRecent login; map its steamID64 → 32-bit accountid dir.
            let account = most_recent_accountid(root)
                .context("multiple Steam users and no unambiguous MostRecent login")?;
            let want = userdata
                .join(account.to_string())
                .join("config/localconfig.vdf");
            if want.is_file() {
                Ok(want)
            } else {
                anyhow::bail!("MostRecent user {account} has no localconfig.vdf")
            }
        }
    }
}

/// The 32-bit accountid of the `MostRecent "1"` user in `<root>/config/loginusers.vdf`.
fn most_recent_accountid(root: &Path) -> anyhow::Result<u32> {
    const STEAMID64_BASE: u64 = 76_561_197_960_265_728;
    let text = std::fs::read_to_string(root.join("config/loginusers.vdf"))?;
    // Walk the "users" table with brace-depth tracking: each user block's key is a
    // steamID64. `current` is set when entering a numeric-keyed block and cleared
    // only when we leave THAT block — so a nested subtable (`{ … }`) inside a user
    // entry can't wipe the id before its `MostRecent "1"` leaf is seen.
    let mut depth: usize = 0;
    let mut pending: Option<String> = None;
    let mut current: Option<u64> = None;
    let mut current_depth: usize = 0;
    for line in text.lines() {
        let t = line.trim();
        if t == "{" {
            depth += 1;
            if let Some(id64) = pending.take().and_then(|k| k.parse::<u64>().ok()) {
                current = Some(id64);
                current_depth = depth;
            }
        } else if t == "}" {
            depth = depth.saturating_sub(1);
            if current.is_some() && depth < current_depth {
                current = None;
            }
        } else {
            let toks = super::vdf::quoted_tokens(t);
            match toks.as_slice() {
                [k] => pending = Some(k.clone()),
                [k, v] if k.eq_ignore_ascii_case("mostrecent") && v == "1" => {
                    if let Some(id64) = current {
                        return Ok((id64 - STEAMID64_BASE) as u32);
                    }
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("no MostRecent user in loginusers.vdf")
}

/// Is a Steam client currently running? Scans `/proc/*/comm` for the main `steam`
/// process (not `steamwebhelper`/`reaper`). Conservative: if `/proc` is unreadable
/// we report `true` (refuse to write) rather than risk a clobber.
fn steam_is_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(e.path().join("comm")) {
            if comm.trim() == "steam" {
                return true;
            }
        }
    }
    false
}

/// Copy `path` → `path.pharos.bak` once (preserve the user's pristine pre-Pharos
/// config); never overwrite an existing backup.
fn backup_once(path: &Path) -> anyhow::Result<()> {
    let bak = path.with_extension("vdf.pharos.bak");
    if !bak.exists() {
        std::fs::copy(path, &bak).with_context(|| format!("backup {}", bak.display()))?;
    }
    Ok(())
}

/// Atomic same-dir temp + rename: durable (fsync before rename), permission-
/// preserving (the replacement inherits the original's mode, not umask 0644), and
/// uniquely named per write (pid + monotonic seq) so nothing collides even outside
/// the write lock.
fn atomic_write(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::io::Write;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("vdf.pharos.tmp.{}.{seq}", std::process::id()));

    let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);

    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

fn set_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(path)?.permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_from_loginusers() {
        let vdf = "\"users\"\n{\n\t\"76561197972611406\"\n\t{\n\t\t\"AccountName\"\t\t\"player\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t}\n}\n";
        let dir = std::env::temp_dir().join(format!("pharos-lu-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/loginusers.vdf"), vdf).unwrap();
        let id = most_recent_accountid(&dir).unwrap();
        assert_eq!(id, 12_345_678); // 76561197972611406 - base
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn most_recent_survives_nested_subtable() {
        // A nested subtable inside the user block (its own `{ … }`) must NOT reset
        // the tracked steamID64 before MostRecent is seen.
        let vdf = "\"users\"\n{\n\t\"76561197972611406\"\n\t{\n\t\t\"tags\"\n\t\t{\n\t\t\t\"fav\"\t\t\"1\"\n\t\t}\n\t\t\"MostRecent\"\t\t\"1\"\n\t}\n}\n";
        let dir = std::env::temp_dir().join(format!("pharos-lu2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/loginusers.vdf"), vdf).unwrap();
        assert_eq!(most_recent_accountid(&dir).unwrap(), 12_345_678);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shim_carries_passthrough_and_token() {
        // The shim must default to passthrough and be identifiable by basename.
        assert!(SHIM_SCRIPT.contains("exec env \"$@\""));
        assert!(SHIM_SCRIPT.contains("PHAROS_NO_REDIRECT"));
        assert_eq!(SHIM_BASENAME, "pharos-redirect");
    }

    #[test]
    fn shim_reads_per_app_env() {
        // The per-app fragment is sourced from the appid Steam exports, with the
        // launcher's `pending` pointer as the fallback.
        assert!(SHIM_SCRIPT.contains("SteamAppId"));
        assert!(SHIM_SCRIPT.contains("SteamGameId"));
        assert!(SHIM_SCRIPT.contains("PHAROS_APP_ENV_APPID"));
        assert!(SHIM_SCRIPT.contains("pharos/app-env"));
    }

    /// End-to-end on the real `/bin/sh`: the shim is the only thing standing
    /// between an entry's `env` and a Steam-forked game, and its failure mode is
    /// silence, so exercise it as a process rather than trusting the source text.
    /// Builds a live marker (this pid + its start-ticks + a real socket file) in a
    /// private `XDG_RUNTIME_DIR`, then runs `sh <shim> printenv PHAROS_TEST_VAR`.
    #[cfg(target_os = "linux")]
    #[test]
    fn shim_applies_per_app_env_only_on_a_live_redirect() {
        use std::os::unix::net::UnixListener;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("pharos-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pharos/app-env")).unwrap();
        let shim = dir.join("pharos-redirect");
        std::fs::write(&shim, SHIM_SCRIPT).unwrap();

        // The staleness guard wants a socket that exists, owned by a live pid.
        let sock = "wayland-test";
        let _listener = UnixListener::bind(dir.join(sock)).unwrap();
        let marker = crate::redirect::MarkerEnv::for_current(sock, None);
        let (frag, skipped) =
            crate::redirect::render_app_env(&[("PHAROS_TEST_VAR".into(), "it's live".into())]);
        assert!(skipped.is_empty());
        std::fs::write(dir.join("pharos/app-env/70"), &frag).unwrap();

        let run = |appid: Option<&str>, with_marker: bool| -> String {
            if with_marker {
                std::fs::write(dir.join("pharos/launch-env"), marker.render()).unwrap();
            } else {
                let _ = std::fs::remove_file(dir.join("pharos/launch-env"));
            }
            let mut c = Command::new("sh");
            c.arg(&shim)
                .args(["printenv", "PHAROS_TEST_VAR"])
                .env("XDG_RUNTIME_DIR", &dir)
                .env_remove("SteamAppId")
                .env_remove("SteamGameId")
                .env_remove("PHAROS_TEST_VAR");
            if let Some(id) = appid {
                c.env("SteamAppId", id);
            }
            let out = c.output().expect("run shim");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        // Live marker + the game's own appid → the entry's env reaches the game.
        assert_eq!(run(Some("70"), true), "it's live");
        // A different game must not inherit it.
        assert_eq!(run(Some("620"), true), "");
        // No marker (desktop play) → passthrough, env untouched.
        assert_eq!(run(Some("70"), false), "");

        // Fallback: no SteamAppId in the environment, but the launcher left a
        // pointer for the handoff in flight.
        std::fs::write(
            dir.join("pharos/app-env/pending"),
            "PHAROS_APP_ENV_APPID='70'\n",
        )
        .unwrap();
        assert_eq!(run(None, true), "it's live");

        std::fs::remove_dir_all(&dir).ok();
    }
}
