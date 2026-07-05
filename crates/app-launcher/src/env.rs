// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Environment detection for *easy mode*: where Steam lives and how to phrase a
//! launch command so it works on this machine's distro/packaging.
//!
//! The same `steam://rungameid/<id>` URI launches a game everywhere; what differs
//! is the wrapper. Native Linux Steam wants `setsid -f steam …` (detach from the
//! host's process group); a Flatpak Steam wants `flatpak run com.valvesoftware.Steam …`;
//! Snap wants `snap run steam …`; Windows hands the URI to the shell.

use std::path::PathBuf;

use crate::config::{LaunchKind, LaunchTarget};

/// The kind of session a [`LaunchTarget`] resolves to — the explicit "where",
/// decoupled from the target enum so the launch path can reason about *safety*
/// (virtual = fine to cold-start; physical = footgun) without re-matching the
/// target variants everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetEnv {
    /// The Pharos virtual display (a headless Wayland output). Safe to launch
    /// into — it's the streamed surface, isolated from the host's real session.
    Virtual,
    /// The host's **physical** desktop (extend/duplicate). Launching a fresh GPU
    /// app here lands on the real session *and* risks the documented NVENC
    /// SIGSEGV, so the launcher refuses to cold-start into it.
    Physical,
}

impl TargetEnv {
    /// True for the physical (real-desktop) environment — the cold-start footgun.
    pub fn is_physical(self) -> bool {
        matches!(self, TargetEnv::Physical)
    }
}

/// Classify a [`LaunchTarget`] into its [`TargetEnv`]. The one place the launch
/// path decides "virtual or physical?" — testable in isolation and the mirror of
/// [`LaunchTarget::is_physical_target`](crate::config::LaunchTarget::is_physical_target).
pub fn classify_target(target: &LaunchTarget) -> TargetEnv {
    if target.is_physical_target() {
        TargetEnv::Physical
    } else {
        TargetEnv::Virtual
    }
}

/// How Steam is installed on this host (the first one detected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamInstall {
    /// A native package; `root` is the Steam data dir (contains `steamapps/`).
    Native { root: PathBuf },
    /// Flatpak (`com.valvesoftware.Steam`); `root` is its sandboxed data dir.
    Flatpak { root: PathBuf },
    /// Snap; `root` is its data dir.
    Snap { root: PathBuf },
    /// Steam installed on Windows; `root` is its install dir.
    Windows { root: PathBuf },
    /// Steam not found.
    None,
}

impl SteamInstall {
    /// The Steam data root (the dir holding `steamapps/`, `appcache/`, …), if any.
    pub fn root(&self) -> Option<&PathBuf> {
        match self {
            SteamInstall::Native { root }
            | SteamInstall::Flatpak { root }
            | SteamInstall::Snap { root }
            | SteamInstall::Windows { root } => Some(root),
            SteamInstall::None => None,
        }
    }

    /// Phrase a `steam://…` URI launch for this install. The returned string is a
    /// ready-to-store [`AppEntry::command`](crate::config::AppEntry::command).
    ///
    /// `shell` reports whether the command needs the platform shell to run (true
    /// only on Windows, where the URI is handed to the shell's handler).
    pub fn launch_uri(&self, uri: &str) -> (String, bool) {
        match self {
            SteamInstall::Native { .. } => (format!("setsid -f steam {uri}"), false),
            SteamInstall::Flatpak { .. } => (
                format!("setsid -f flatpak run com.valvesoftware.Steam {uri}"),
                false,
            ),
            SteamInstall::Snap { .. } => (format!("setsid -f snap run steam {uri}"), false),
            SteamInstall::Windows { .. } => (format!("start \"\" {uri}"), true),
            // No install detected: still emit the most likely native form so the
            // user gets an editable starting point rather than nothing.
            SteamInstall::None => {
                if cfg!(windows) {
                    (format!("start \"\" {uri}"), true)
                } else {
                    (format!("setsid -f steam {uri}"), false)
                }
            }
        }
    }

    /// Build the concrete `(command_line, needs_shell)` for a [`LaunchKind`] on
    /// *this* install. The single place structured launch data becomes a command,
    /// so the launcher never depends on a hand-authored string for the common
    /// kinds:
    ///
    /// * [`LaunchKind::SteamGame`] → the distro-correct `steam://rungameid/<id>`
    ///   wrapper for this install (re-phrased here, not baked into the catalog).
    /// * [`LaunchKind::DesktopFile`] → the stored `Exec=` line, run directly.
    /// * [`LaunchKind::RawCommand`] → the line verbatim (the legacy / fallback
    ///   path; honors the entry's own `shell` flag, so this reports `false`).
    pub fn launch_command(&self, kind: &LaunchKind) -> (String, bool) {
        match kind {
            LaunchKind::SteamGame { appid } => {
                self.launch_uri(&format!("steam://rungameid/{appid}"))
            }
            LaunchKind::DesktopFile { exec } => (exec.clone(), false),
            LaunchKind::RawCommand { line } => (line.clone(), false),
        }
    }
}

/// Detect the host's Steam install (first match wins, native preferred).
pub fn detect_steam() -> SteamInstall {
    #[cfg(windows)]
    {
        for root in windows_steam_candidates() {
            if root.join("steamapps").is_dir() {
                return SteamInstall::Windows { root };
            }
        }
        SteamInstall::None
    }
    #[cfg(not(windows))]
    {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return SteamInstall::None,
        };
        // Native first (the common, fastest path).
        for rel in [
            ".local/share/Steam",
            ".steam/steam",
            ".steam/root",
            ".steam/debian-installation",
        ] {
            let root = home.join(rel);
            if root.join("steamapps").is_dir() {
                return SteamInstall::Native { root };
            }
        }
        let flatpak = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
        if flatpak.join("steamapps").is_dir() {
            return SteamInstall::Flatpak { root: flatpak };
        }
        let snap = home.join("snap/steam/common/.local/share/Steam");
        if snap.join("steamapps").is_dir() {
            return SteamInstall::Snap { root: snap };
        }
        SteamInstall::None
    }
}

#[cfg(windows)]
fn windows_steam_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(base) = std::env::var(var) {
            v.push(PathBuf::from(base).join("Steam"));
        }
    }
    v.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_splits_virtual_and_physical() {
        assert_eq!(
            classify_target(&LaunchTarget::VirtualDisplay),
            TargetEnv::Virtual
        );
        assert_eq!(
            classify_target(&LaunchTarget::VirtualOutput("left".into())),
            TargetEnv::Virtual
        );
        assert_eq!(
            classify_target(&LaunchTarget::ExtendPhysical),
            TargetEnv::Physical
        );
        assert_eq!(
            classify_target(&LaunchTarget::DuplicatePhysical),
            TargetEnv::Physical
        );
        assert!(classify_target(&LaunchTarget::ExtendPhysical).is_physical());
        assert!(!classify_target(&LaunchTarget::VirtualDisplay).is_physical());
    }

    #[test]
    fn native_steam_game_builds_setsid_rungameid() {
        let install = SteamInstall::Native {
            root: PathBuf::from("/x"),
        };
        let (line, shell) = install.launch_command(&LaunchKind::SteamGame { appid: 42 });
        assert_eq!(line, "setsid -f steam steam://rungameid/42");
        assert!(!shell);
    }

    #[test]
    fn flatpak_steam_game_uses_flatpak_run() {
        let install = SteamInstall::Flatpak {
            root: PathBuf::from("/x"),
        };
        let (line, _) = install.launch_command(&LaunchKind::SteamGame { appid: 7 });
        assert!(line.contains("flatpak run com.valvesoftware.Steam"));
        assert!(line.contains("steam://rungameid/7"));
    }

    #[test]
    fn windows_steam_game_needs_shell() {
        let install = SteamInstall::Windows {
            root: PathBuf::from(r"C:\Steam"),
        };
        let (line, shell) = install.launch_command(&LaunchKind::SteamGame { appid: 9 });
        assert!(shell, "Windows URI hand-off needs the shell");
        assert!(line.contains("steam://rungameid/9"));
    }

    #[test]
    fn desktop_and_raw_kinds_pass_through() {
        let install = SteamInstall::None;
        let (d, ds) = install.launch_command(&LaunchKind::DesktopFile {
            exec: "firefox".into(),
        });
        assert_eq!((d.as_str(), ds), ("firefox", false));
        let (r, rs) = install.launch_command(&LaunchKind::RawCommand {
            line: "konsole -e htop".into(),
        });
        assert_eq!((r.as_str(), rs), ("konsole -e htop", false));
    }
}
