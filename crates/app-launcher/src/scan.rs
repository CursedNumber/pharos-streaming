// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! *Easy mode*: discover installed games/apps and turn a pick into a ready
//! [`AppEntry`] with a generated launch command and offline box art.
//!
//! Steam scanning is hand-rolled rather than pulling a (version-fragile) crate:
//! Steam's `libraryfolders.vdf` and `appmanifest_*.acf` are Valve KeyValues
//! text, which a tolerant "give me the value after this key" reader handles in a
//! few lines and across every OS. We only read the two fields we need (`appid`,
//! `name`), so the parser stays trivial and robust to format drift.

use std::path::{Path, PathBuf};

use crate::config::{AppEntry, AppKind, LaunchKind, LaunchTarget};
use crate::env::detect_steam;

/// One installed Steam game discovered by [`scan_steam`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamGame {
    pub appid: u32,
    pub name: String,
    /// Offline box art (portrait grid image) if found in Steam's cache.
    pub box_art: Option<PathBuf>,
}

/// One installed desktop application discovered by [`scan_desktop_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApp {
    pub name: String,
    /// The `Exec=` line with field codes (`%U`, `%f`, …) stripped.
    pub exec: String,
    /// An icon path, if the `Icon=` value resolved to an absolute file.
    pub icon: Option<PathBuf>,
}

/// Scan every Steam library for installed games, newest-listed first. Empty if
/// Steam isn't installed. Never errors — a malformed manifest is skipped.
pub fn scan_steam() -> Vec<SteamGame> {
    let install = detect_steam();
    let root = match install.root() {
        Some(r) => r.clone(),
        None => return Vec::new(),
    };

    let mut games = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for lib in steam_library_dirs(&root) {
        let steamapps = lib.join("steamapps");
        let entries = match std::fs::read_dir(&steamapps) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_manifest = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("appmanifest_") && n.ends_with(".acf"));
            if !is_manifest {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let appid = kv_first(&text, "appid").and_then(|s| s.parse::<u32>().ok());
            let name = kv_first(&text, "name");
            if let (Some(appid), Some(name)) = (appid, name) {
                // Skip Steamworks redistributables / tools that clutter the list.
                if name.is_empty() || !seen.insert(appid) {
                    continue;
                }
                let box_art = find_box_art(&root, appid);
                games.push(SteamGame {
                    appid,
                    name,
                    box_art,
                });
            }
        }
    }
    games.sort_by_key(|a| a.name.to_lowercase());
    games
}

/// All Steam library roots: the default install root plus every `path` listed in
/// `steamapps/libraryfolders.vdf` (games can live on other drives).
fn steam_library_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let vdf = root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(text) = std::fs::read_to_string(&vdf) {
        for p in kv_all(&text, "path") {
            let pb = PathBuf::from(p);
            if !dirs.contains(&pb) {
                dirs.push(pb);
            }
        }
    }
    dirs
}

/// Look for an offline portrait box-art image for `appid`, trying the layouts
/// Steam has used over time. Returns the first that exists. Portrait names in
/// priority order: the dedicated 2:3 grid art (`library_600x900.jpg`) then the
/// store capsule (`library_capsule.jpg`, also 2:3).
fn find_box_art(root: &Path, appid: u32) -> Option<PathBuf> {
    const PORTRAIT: [&str; 2] = ["library_600x900.jpg", "library_capsule.jpg"];
    let cache = root.join("appcache").join("librarycache");

    // Older flat naming.
    for name in [
        format!("{appid}_library_600x900.jpg"),
        format!("{appid}p.jpg"),
    ] {
        let p = cache.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // Current per-appid dir: the portrait sits directly under it, or (2024+) one
    // content-hash subdir down (e.g. `<appid>/<hash>/library_capsule.jpg`).
    let dir = cache.join(appid.to_string());
    for name in PORTRAIT {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    find_named(&dir, &PORTRAIT, 2)
}

/// Bounded-depth search for the first file whose name is in `names` (used for
/// Steam's content-hashed librarycache subdirs).
fn find_named(dir: &Path, names: &[&str], depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_file() {
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.contains(&n))
            {
                return Some(p);
            }
        } else if p.is_dir() {
            subdirs.push(p);
        }
    }
    if depth == 0 {
        return None;
    }
    subdirs
        .into_iter()
        .find_map(|sd| find_named(&sd, names, depth - 1))
}

/// Turn a discovered Steam game into an editable [`AppEntry`] with a
/// distro-correct launch command and (if found) box art.
pub fn suggest_from_steam(game: &SteamGame) -> AppEntry {
    let install = detect_steam();
    // Carry the *structured* kind (the appid) so the launcher re-phrases the
    // `steam://rungameid/<id>` wrapper for whatever install is present at launch
    // time. `command`/`shell` are still filled in as a human-readable hint and
    // the fallback for any consumer that hasn't learned about `launch` yet.
    let kind = LaunchKind::SteamGame { appid: game.appid };
    let (command, shell) = install.launch_command(&kind);
    AppEntry {
        slug: slugify(&game.name),
        name: game.name.clone(),
        kind: AppKind::Game,
        command,
        launch: Some(kind),
        shell,
        box_art: game
            .box_art
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        target: LaunchTarget::VirtualDisplay,
        ..AppEntry::new()
    }
}

/// Turn a picked desktop app into an editable [`AppEntry`].
pub fn suggest_from_desktop(app: &DesktopApp) -> AppEntry {
    // Structured DesktopFile kind carries the (field-code-stripped) Exec line, so
    // the launcher runs it directly without the user pasting a path. `command`
    // mirrors it for display / back-compat.
    AppEntry {
        slug: slugify(&app.name),
        name: app.name.clone(),
        kind: AppKind::App,
        command: app.exec.clone(),
        launch: Some(LaunchKind::DesktopFile {
            exec: app.exec.clone(),
        }),
        box_art: app.icon.as_ref().map(|p| p.to_string_lossy().into_owned()),
        target: LaunchTarget::VirtualDisplay,
        ..AppEntry::new()
    }
}

/// Turn a picked executable path into an editable [`AppEntry`]. The name is the
/// file stem, title-cased; the command quotes the path so spaces survive a
/// non-shell launch.
pub fn suggest_from_executable(path: &Path) -> AppEntry {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app")
        .to_string();
    let name = title_case(&stem);
    let p = path.to_string_lossy();
    let command = if p.contains(char::is_whitespace) {
        format!("\"{p}\"")
    } else {
        p.into_owned()
    };
    AppEntry {
        slug: slugify(&name),
        name,
        kind: AppKind::App,
        command,
        ..AppEntry::new()
    }
}

/// Scan freedesktop `.desktop` files in the standard XDG application dirs
/// (Linux). Empty on other platforms. Hidden / `NoDisplay` entries are skipped.
pub fn scan_desktop_entries() -> Vec<DesktopApp> {
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
    #[cfg(target_os = "linux")]
    {
        let mut dirs = Vec::new();
        if let Some(data) = dirs::data_dir() {
            dirs.push(data.join("applications"));
        }
        dirs.push(PathBuf::from("/usr/local/share/applications"));
        dirs.push(PathBuf::from("/usr/share/applications"));
        if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
            for d in xdg.split(':').filter(|s| !s.is_empty()) {
                dirs.push(PathBuf::from(d).join("applications"));
            }
        }

        let mut apps = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for dir in dirs {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                if let Some(app) = parse_desktop_entry(&path) {
                    if seen.insert(app.name.to_lowercase()) {
                        apps.push(app);
                    }
                }
            }
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        apps
    }
}

/// Parse the `[Desktop Entry]` group of a `.desktop` file for Name/Exec/Icon.
/// Returns `None` for hidden, `NoDisplay`, or non-`Application` entries.
#[cfg(target_os = "linux")]
fn parse_desktop_entry(path: &Path) -> Option<DesktopApp> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_group = false;
    let (mut name, mut exec, mut icon) = (None, None, None);
    let (mut no_display, mut hidden, mut is_app) = (false, false, true);
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" if name.is_none() => name = Some(val.trim().to_string()),
            "Exec" if exec.is_none() => exec = Some(strip_field_codes(val.trim())),
            "Icon" if icon.is_none() => icon = Some(val.trim().to_string()),
            "NoDisplay" => no_display = val.trim().eq_ignore_ascii_case("true"),
            "Hidden" => hidden = val.trim().eq_ignore_ascii_case("true"),
            "Type" => is_app = val.trim() == "Application",
            _ => {}
        }
    }
    if no_display || hidden || !is_app {
        return None;
    }
    let name = name?;
    let exec = exec?;
    if exec.is_empty() {
        return None;
    }
    Some(DesktopApp {
        name,
        exec,
        icon: icon.and_then(|i| resolve_icon(&i)),
    })
}

/// Remove `.desktop` `Exec=` field codes (`%f %F %u %U %i %c %k %d …`).
#[cfg(target_os = "linux")]
fn strip_field_codes(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|tok| !(tok.len() == 2 && tok.starts_with('%')))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve a `.desktop` `Icon=` value to a file: an absolute path is used as-is;
/// a bare theme name is looked up in a few common hicolor/pixmaps locations
/// (best-effort — `None` if not found, and the panel falls back to a placeholder).
#[cfg(target_os = "linux")]
fn resolve_icon(icon: &str) -> Option<PathBuf> {
    let p = Path::new(icon);
    if p.is_absolute() && p.is_file() {
        return Some(p.to_path_buf());
    }
    let mut bases = Vec::new();
    if let Some(data) = dirs::data_dir() {
        bases.push(data.join("icons/hicolor"));
    }
    bases.push(PathBuf::from("/usr/share/icons/hicolor"));
    bases.push(PathBuf::from("/usr/share/pixmaps"));
    for base in bases {
        for sz in ["512x512", "256x256", "128x128", "scalable", ""] {
            for ext in ["png", "svg", "jpg"] {
                let cand = if sz.is_empty() {
                    base.join(format!("{icon}.{ext}"))
                } else {
                    base.join(sz).join("apps").join(format!("{icon}.{ext}"))
                };
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

// --- tiny KeyValues (VDF/ACF) reader ---------------------------------------

/// Return the first `"value"` that follows a `"key"` token in Valve KeyValues
/// text. Tolerant: ignores nesting/whitespace, just scans tokens in order.
fn kv_first(text: &str, key: &str) -> Option<String> {
    kv_all(text, key).into_iter().next()
}

/// Return every `"value"` that follows each `"key"` token, in order.
fn kv_all(text: &str, key: &str) -> Vec<String> {
    let toks = vdf_tokens(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        if toks[i] == key {
            out.push(toks[i + 1].clone());
        }
        i += 1;
    }
    out
}

/// Split KeyValues text into its double-quoted tokens (keys and values are both
/// quoted in VDF/ACF). Braces and unquoted tokens are ignored.
fn vdf_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escape = false;
    for ch in text.chars() {
        if in_quote {
            if escape {
                cur.push(ch);
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                out.push(std::mem::take(&mut cur));
                in_quote = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_quote = true;
        }
    }
    // Recover a token left open by an unterminated quote at EOF (a truncated /
    // malformed manifest) rather than silently dropping it.
    if in_quote && !cur.is_empty() {
        out.push(cur);
    }
    out
}

// --- string helpers --------------------------------------------------------

/// A filesystem/CLI-safe slug: lowercase, alnum runs joined by single dashes.
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = true; // avoids a leading dash
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        "app".into()
    } else {
        s
    }
}

/// Title-case a file stem for a default display name (`my-game` → `My Game`).
fn title_case(stem: &str) -> String {
    stem.split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_is_clean() {
        assert_eq!(slugify("Elden Ring"), "elden-ring");
        assert_eq!(
            slugify("  Half-Life 2: Episode  One "),
            "half-life-2-episode-one"
        );
        assert_eq!(slugify("***"), "app");
    }

    #[test]
    fn title_case_works() {
        assert_eq!(title_case("my-cool_game"), "My Cool Game");
    }

    #[test]
    fn vdf_reads_values() {
        let acf = r#"
            "AppState"
            {
                "appid"     "1245620"
                "name"      "ELDEN RING"
                "installdir"    "ELDEN RING"
            }
        "#;
        assert_eq!(kv_first(acf, "appid").as_deref(), Some("1245620"));
        assert_eq!(kv_first(acf, "name").as_deref(), Some("ELDEN RING"));
    }

    #[test]
    fn vdf_reads_all_library_paths() {
        let vdf = r#"
            "libraryfolders"
            {
                "0" { "path" "/home/u/.local/share/Steam" }
                "1" { "path" "/mnt/games/SteamLibrary" }
            }
        "#;
        assert_eq!(
            kv_all(vdf, "path"),
            vec!["/home/u/.local/share/Steam", "/mnt/games/SteamLibrary"]
        );
    }

    #[test]
    fn find_box_art_handles_hashed_capsule_layout() {
        // Simulate Steam's 2024+ librarycache: <appid>/<hash>/library_capsule.jpg.
        let base = std::env::temp_dir().join(format!("pharos-art-test-{}", std::process::id()));
        let hashed =
            base.join("appcache/librarycache/3321460/0ea942465fd1dff8a7fbc11cc9fca8c476c5e5ae");
        std::fs::create_dir_all(&hashed).unwrap();
        let art = hashed.join("library_capsule.jpg");
        std::fs::write(&art, b"jpegbytes").unwrap();
        let found = find_box_art(&base, 3321460);
        std::fs::remove_dir_all(&base).ok();
        assert_eq!(found.as_deref(), Some(art.as_path()));
    }

    #[test]
    fn suggest_executable_quotes_spaces() {
        let e = suggest_from_executable(Path::new("/opt/My Game/run.sh"));
        assert_eq!(e.command, "\"/opt/My Game/run.sh\"");
        assert_eq!(e.name, "Run");
    }
}
