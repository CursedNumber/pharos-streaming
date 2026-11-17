// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Per-client app config: keep a separate copy of chosen INI sections of an
//! app's own config file for each paired Trireme client.
//!
//! The motivating case is an emulator (Lichii, Eglantine, any Eden/yuzu
//! derivative) whose controller mapping lives in the `[Controls]` section of
//! `qt-config.ini`, next to everything else (game folders, graphics, keys). The
//! phone, the TV, and the Xbox each bring different pads, so one shared mapping
//! is wrong for all but one of them. An [`AppEntry`] lists the sections to keep
//! per client in [`AppEntry::per_client_config`]; then:
//!
//! - **Before launch** ([`restore`]): if this client has a saved snapshot, its
//!   sections are spliced into the live file, replacing the shared ones. Every
//!   other section is untouched. A client with no snapshot yet starts from
//!   whatever the file holds (the last client's mapping), which is a better
//!   first guess than defaults.
//! - **After exit** ([`save`]): the sections are read back out of the live file
//!   (the app writes its config on exit) and stored as this client's snapshot.
//!
//! Snapshots live under `<config_dir>/per-client/<client-pubkey-hex>/<slug>/`,
//! keyed by the stable pairing pubkey (not a per-connection id), so a client's
//! mapping survives reconnects and host restarts. Anonymous/unpaired launches
//! and launches from the host itself have no key and skip all of this.
//!
//! Everything here is best-effort: a failure is logged and the launch goes on
//! with whatever the file already holds. Losing a mapping is an annoyance; an
//! app that refuses to start is a broken evening.

use std::path::{Path, PathBuf};

use crate::config::{AppEntry, PerClientFile};

/// Restore `client`'s saved sections into each of `entry`'s per-client files.
/// No-op when the entry has none, or the client has no snapshot yet.
pub fn restore(entry: &AppEntry, client: &str) {
    for (i, spec) in entry.per_client_config.iter().enumerate() {
        if let Err(e) = restore_one(&entry.slug, client, i, spec) {
            tracing::warn!(
                "app `{}`: restoring per-client config `{}` failed: {e:#}",
                entry.slug,
                spec.file
            );
        }
    }
}

/// Save the live per-client sections of each of `entry`'s files as `client`'s
/// snapshot. Call after the app has exited (it writes its config on exit).
pub fn save(entry: &AppEntry, client: &str) {
    for (i, spec) in entry.per_client_config.iter().enumerate() {
        if let Err(e) = save_one(&entry.slug, client, i, spec) {
            tracing::warn!(
                "app `{}`: saving per-client config `{}` failed: {e:#}",
                entry.slug,
                spec.file
            );
        }
    }
}

fn restore_one(slug: &str, client: &str, i: usize, spec: &PerClientFile) -> anyhow::Result<()> {
    let snap = snapshot_path(slug, client, i, spec)?;
    let saved = match std::fs::read_to_string(&snap) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                "app `{slug}`: no per-client `{}` for client {} yet; using the current file",
                spec.file,
                short(client)
            );
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let live_path = expand_home(&spec.file);
    let live = match std::fs::read_to_string(&live_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let merged = splice_sections(&live, &saved);
    if merged != live {
        write_atomic(&live_path, &merged)?;
    }
    tracing::info!(
        "app `{slug}`: restored {:?} of `{}` for client {}",
        spec.sections,
        spec.file,
        short(client)
    );
    Ok(())
}

fn save_one(slug: &str, client: &str, i: usize, spec: &PerClientFile) -> anyhow::Result<()> {
    let live_path = expand_home(&spec.file);
    let live = std::fs::read_to_string(&live_path)?;
    let extracted = extract_sections(&live, &spec.sections);
    if extracted.is_empty() {
        // Nothing to keep (the app never wrote those sections). Don't clobber
        // an older good snapshot with an empty one.
        return Ok(());
    }
    let snap = snapshot_path(slug, client, i, spec)?;
    if let Some(dir) = snap.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(&snap, &extracted)?;
    tracing::info!(
        "app `{slug}`: saved {:?} of `{}` for client {}",
        spec.sections,
        spec.file,
        short(client)
    );
    Ok(())
}

/// `<config_dir>/per-client/<client>/<slug>/<i>-<file name>`. The index keeps
/// two specs with the same file name (different dirs) apart.
fn snapshot_path(
    slug: &str,
    client: &str,
    i: usize,
    spec: &PerClientFile,
) -> anyhow::Result<PathBuf> {
    // Both come from trusted places (the pairing key is hex, the slug is
    // validated), but never let either climb out of the snapshot dir.
    anyhow::ensure!(
        !client.is_empty() && client.chars().all(|c| c.is_ascii_alphanumeric()),
        "bad client key"
    );
    anyhow::ensure!(
        !slug.is_empty() && !slug.contains(['/', '\\']) && slug != "." && slug != "..",
        "bad slug"
    );
    let name = Path::new(&spec.file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    Ok(crate::config_dir()?
        .join("per-client")
        .join(client)
        .join(slug)
        .join(format!("{i}-{name}")))
}

/// Expand a leading `~/` to the home directory.
fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// Write via a sibling temp file + rename so a crash mid-write never leaves the
/// app with a half-written config.
fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".pharos-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn short(client: &str) -> &str {
    client.get(..8).unwrap_or(client)
}

/// The section name of a header line (`[Controls]` → `Controls`), if it is one.
fn header(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']')
}

/// Split INI text into a preamble (lines before the first header) and
/// `(name, text)` sections, where each section's text includes its header line.
/// Lines keep their own endings, so joining the pieces reproduces the input.
fn split_sections(text: &str) -> (String, Vec<(String, String)>) {
    let mut pre = String::new();
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        if let Some(name) = header(line) {
            sections.push((name.to_string(), line.to_string()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push_str(line);
        } else {
            pre.push_str(line);
        }
    }
    (pre, sections)
}

/// The listed sections of `text`, in file order, as standalone INI text.
pub fn extract_sections(text: &str, names: &[String]) -> String {
    let (_, sections) = split_sections(text);
    let mut out = String::new();
    for (name, body) in sections {
        if names.contains(&name) {
            push_section(&mut out, &body);
        }
    }
    out
}

/// `live` with every section present in `saved` replaced by `saved`'s copy, in
/// place. A saved section the live file lacks is appended. All other sections
/// and the preamble are kept byte-for-byte.
pub fn splice_sections(live: &str, saved: &str) -> String {
    let (_, saved_sections) = split_sections(saved);
    let (pre, live_sections) = split_sections(live);
    let mut out = pre;
    let mut used = vec![false; saved_sections.len()];
    for (name, body) in &live_sections {
        match saved_sections.iter().position(|(n, _)| n == name) {
            Some(j) => {
                used[j] = true;
                push_section(&mut out, &saved_sections[j].1);
            }
            None => out.push_str(body),
        }
    }
    for (j, (_, body)) in saved_sections.iter().enumerate() {
        if !used[j] {
            push_section(&mut out, body);
        }
    }
    out
}

/// Append a section, making sure the text before it ends in a newline so the
/// header lands on its own line.
fn push_section(out: &mut String, body: &str) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "[Renderer]\nbackend=1\n\n[Controls]\nplayer_0_button_a=\"pad:A\"\nplayer_0_connected=true\n\n[UI]\ntheme=dark\n";

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn split_roundtrips_bytes() {
        let (pre, s) = split_sections(&format!("; comment\n{LIVE}"));
        let joined: String = std::iter::once(pre)
            .chain(s.into_iter().map(|(_, b)| b))
            .collect();
        assert_eq!(joined, format!("; comment\n{LIVE}"));
    }

    #[test]
    fn extract_takes_only_named_sections() {
        let got = extract_sections(LIVE, &names(&["Controls"]));
        assert_eq!(
            got,
            "[Controls]\nplayer_0_button_a=\"pad:A\"\nplayer_0_connected=true\n\n"
        );
        assert_eq!(extract_sections(LIVE, &names(&["Missing"])), "");
    }

    #[test]
    fn splice_replaces_in_place_and_keeps_the_rest() {
        let saved = "[Controls]\nplayer_0_button_a=\"pad:B\"\n";
        let got = splice_sections(LIVE, saved);
        assert_eq!(
            got,
            "[Renderer]\nbackend=1\n\n[Controls]\nplayer_0_button_a=\"pad:B\"\n[UI]\ntheme=dark\n"
        );
    }

    #[test]
    fn splice_appends_missing_section() {
        let got = splice_sections("[UI]\ntheme=dark", "[Controls]\nx=1\n");
        assert_eq!(got, "[UI]\ntheme=dark\n[Controls]\nx=1\n");
        assert_eq!(
            splice_sections("", "[Controls]\nx=1\n"),
            "[Controls]\nx=1\n"
        );
    }

    #[test]
    fn save_then_restore_roundtrip() {
        let saved = extract_sections(LIVE, &names(&["Controls"]));
        let other = LIVE.replace("pad:A", "pad:Z").replace("dark", "light");
        let restored = splice_sections(&other, &saved);
        // Controls came back; the UI change made in between survived.
        assert!(restored.contains("pad:A") && !restored.contains("pad:Z"));
        assert!(restored.contains("theme=light"));
    }

    #[test]
    fn snapshot_path_rejects_escapes() {
        let spec = PerClientFile {
            file: "~/.config/x/qt-config.ini".into(),
            sections: names(&["Controls"]),
        };
        assert!(snapshot_path("lichii", "../etc", 0, &spec).is_err());
        assert!(snapshot_path("..", "abcd", 0, &spec).is_err());
        assert!(snapshot_path("a/b", "abcd", 0, &spec).is_err());
    }
}
