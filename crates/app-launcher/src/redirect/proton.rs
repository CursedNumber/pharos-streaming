// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Surgical editor for Steam's `config.vdf` `CompatToolMapping` — pins a
//! known-good, winewayland-capable Proton for a redirected game so the seamless
//! display redirect actually gets a Wayland surface.
//!
//! The redirect shim ([`super::linux`]) can put a game's env on the virtual
//! display, but only a Proton whose `winewayland.drv` works will render there;
//! stock "Proton - Experimental" falls back to XWayland and hangs at 0% with no
//! surface (validated 2026-07-09, No Man's Sky vs Crimson Desert on `.100`). The
//! Proton version can't be set from Launch Options (`%command%`) because Steam
//! resolves the compat tool *before* Launch Options apply — the only lever is the
//! per-app force-override that lives in
//! `config.vdf > InstallConfigStore > Software > Valve > Steam > CompatToolMapping`.
//!
//! Same discipline as the [`super::vdf`] `LaunchOptions` editor: navigate by
//! brace-depth (path keys case-insensitive; the appid matched as a **whole
//! field** so `10` never hits `1091500`), edit or insert only the one appid's
//! mapping, and preserve every other byte. We never parse-and-reserialise.
//!
//! Policy — only ever *fill in*, never fight the user:
//! * appid absent from `CompatToolMapping` (the common case: Steam is using its
//!   server-side default, which for a whitelisted title is a stock Proton that
//!   breaks winewayland) → insert our pin.
//! * appid present and already our target tool → [`CompatEdit::AlreadyOurs`].
//! * appid present and mapped to a *Valve built-in* default (empty, or
//!   `proton_experimental` / `proton_<digits>` / `proton_hotfix` / `proton_next`)
//!   → override, since that's exactly the default that breaks Wayland.
//! * appid present and mapped to any *other* installed custom tool (the user
//!   deliberately forced a specific Proton) → [`CompatEdit::Refused`], untouched.

use std::collections::BTreeMap;

/// Steam's per-app force-override priority for a user selection. Steam writes
/// `250` when you pick a Proton in a game's Compatibility settings; matching it
/// makes our pin win over the global `"0"` default the same way a hand pick does.
pub(crate) const FORCE_PRIORITY: u32 = 250;

/// Outcome of a surgical `CompatToolMapping` edit on the in-memory file text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompatEdit {
    /// The text changed; carries the new full-file content.
    Changed(String),
    /// The appid is already mapped to our target tool — no write needed.
    AlreadyOurs,
    /// The appid is mapped to a different, user-chosen custom Proton — untouched.
    Refused,
    /// No `…>Steam` container in this file (not a recognisable `config.vdf`).
    NoSteamBlock,
}

/// The container path down to the `Steam` block, lowercased for case-insensitive
/// matching. `CompatToolMapping` is a direct child of this.
const STEAM_PATH: [&str; 4] = ["installconfigstore", "software", "valve", "steam"];
const CTM_KEY: &str = "compattoolmapping";

/// Is `name` one of Valve's *built-in* default Proton identifiers — the class of
/// mapping we're allowed to override because it breaks winewayland? Covers the
/// empty (server-default) mapping, `proton_experimental`, `proton_next`,
/// `proton_hotfix`, and versioned `proton_<digits…>` (e.g. `proton_9`,
/// `proton_9_0_4`, `proton_63`). Anything else (a custom tool name) is a user
/// choice we must not clobber.
pub(crate) fn is_valve_default_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() || n == "proton_experimental" || n == "proton_next" || n == "proton_hotfix" {
        return true;
    }
    if let Some(rest) = n.strip_prefix("proton_") {
        // A versioned built-in: the remainder is only digits and underscores
        // (`9`, `9_0_4`, `513`). A custom tool like `proton_ge` or
        // `proton_cachyos` has letters here, so it's correctly excluded.
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'_');
    }
    false
}

/// Pin `tool_name` as the compat tool for `appid`, per the module policy: insert
/// when the appid is unmapped, refresh when already ours, override a Valve
/// built-in default ([`is_valve_default_name`]), and refuse a user-forced custom
/// Proton. Every byte outside the one edited/inserted mapping is preserved.
pub(crate) fn set_compat_tool(text: &str, appid: u32, tool_name: &str) -> CompatEdit {
    let appid_s = appid.to_string();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    let mut stack: Vec<String> = Vec::new();
    let mut pending_key: Option<String> = None;

    // Landmarks discovered during the single walk.
    let mut steam_open_idx: Option<usize> = None;
    let mut steam_inner_indent: Option<String> = None; // indent of a Steam child
    let mut ctm_open_idx: Option<usize> = None;
    let mut ctm_inner_indent: Option<String> = None; // indent of an appid key line
    let mut in_app = false;
    let mut app_open_idx: Option<usize> = None;
    let mut app_close_idx: Option<usize> = None;
    let mut app_inner_indent: Option<String> = None; // indent of a leaf in the app
    let mut existing_name: Option<String> = None;

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line == "{" {
            let key = pending_key.take().unwrap_or_default();
            stack.push(key);
            let depth = stack.len();
            // …>Steam (depth 4, path match).
            if depth == STEAM_PATH.len()
                && steam_open_idx.is_none()
                && stack
                    .iter()
                    .zip(STEAM_PATH.iter())
                    .all(|(s, p)| s.eq_ignore_ascii_case(p))
            {
                steam_open_idx = Some(i);
            }
            // …>Steam>CompatToolMapping (depth 5).
            if depth == STEAM_PATH.len() + 1
                && steam_open_idx.is_some()
                && ctm_open_idx.is_none()
                && stack[STEAM_PATH.len()].eq_ignore_ascii_case(CTM_KEY)
            {
                ctm_open_idx = Some(i);
            }
            // …>CompatToolMapping>"<appid>" (depth 6, whole-field appid match).
            // `app_close_idx.is_none()` so a *duplicate* appid block (malformed
            // hand-edit) only ever binds the FIRST — mirrors the localconfig walk.
            if depth == STEAM_PATH.len() + 2
                && ctm_open_idx.is_some()
                && !in_app
                && app_close_idx.is_none()
                && stack[STEAM_PATH.len()].eq_ignore_ascii_case(CTM_KEY)
                && stack[STEAM_PATH.len() + 1] == appid_s
            {
                in_app = true;
                app_open_idx = Some(i);
            }
        } else if line == "}" {
            let depth = stack.len();
            // Closing the target app block: bind its `}` and STOP tracking, so the
            // `name`/close of a *later* sibling block can't overwrite our capture.
            if in_app && depth == STEAM_PATH.len() + 2 {
                app_close_idx = Some(i);
                in_app = false;
            }
            stack.pop();
            // Closing the Steam block (the only depth-4 block we ever enter) ends
            // the walk — nothing relevant lives past it.
            if depth == STEAM_PATH.len() && steam_open_idx.is_some() {
                break;
            }
        } else {
            let depth = stack.len();
            // Indent capture works for ANY child line (a 1-token subtable key or a
            // 2-token leaf) — CTM's children are appid *key* lines (1 token), so a
            // leaf-only capture would miss them and mis-indent an inserted block.
            if depth == STEAM_PATH.len() && steam_open_idx.is_some() && steam_inner_indent.is_none()
            {
                steam_inner_indent = Some(super::vdf::leading_ws(raw));
            }
            if depth == STEAM_PATH.len() + 1 && ctm_open_idx.is_some() && ctm_inner_indent.is_none()
            {
                ctm_inner_indent = Some(super::vdf::leading_ws(raw));
            }
            let toks = super::vdf::quoted_tokens(line);
            match toks.len() {
                1 => pending_key = Some(toks.into_iter().next().unwrap()),
                2 => {
                    let mut it = toks.into_iter();
                    let key = it.next().unwrap();
                    let val = it.next().unwrap();
                    // Inside the target app block: capture the `name` leaf + indent.
                    if in_app && depth == STEAM_PATH.len() + 2 {
                        if app_inner_indent.is_none() {
                            app_inner_indent = Some(super::vdf::leading_ws(raw));
                        }
                        if key.eq_ignore_ascii_case("name") {
                            existing_name = Some(val);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ── Existing appid mapping: decide keep / override / refuse ──────────────
    if let (Some(open), Some(close)) = (app_open_idx, app_close_idx) {
        let name = existing_name.unwrap_or_default();
        if name == tool_name {
            return CompatEdit::AlreadyOurs;
        }
        if !is_valve_default_name(&name) {
            // A deliberately-forced custom Proton — respect it.
            return CompatEdit::Refused;
        }
        // Valve built-in default that breaks Wayland: replace the block body with
        // our canonical three leaves. Braces (lines `open` and `close`) are never
        // touched, so the file stays structurally valid.
        let key_indent = ctm_inner_indent
            .clone()
            .unwrap_or_else(|| super::vdf::leading_ws(lines[open]));
        let leaf_indent = app_inner_indent
            .clone()
            .unwrap_or_else(|| format!("{key_indent}\t"));
        let eol = super::vdf::line_ending(lines[open]);
        let body = canonical_leaves(&leaf_indent, eol, tool_name);
        let mut out = String::with_capacity(text.len() + body.len());
        for (i, l) in lines.iter().enumerate() {
            if i <= open || i >= close {
                out.push_str(l); // the `{` line, the `}` line, and everything else
            } else if i == open + 1 {
                out.push_str(&body); // replace the whole body in one shot
            }
            // lines strictly inside (open, close) other than the anchor are dropped
        }
        return CompatEdit::Changed(out);
    }

    // ── No appid mapping: insert a fresh block ──────────────────────────────
    if let Some(ctm_open) = ctm_open_idx {
        // CompatToolMapping exists → insert the appid block right after its `{`.
        let key_indent = ctm_inner_indent
            .unwrap_or_else(|| format!("{}\t", super::vdf::leading_ws(lines[ctm_open])));
        let eol = super::vdf::line_ending(lines[ctm_open]);
        let block = appid_block(&key_indent, eol, &appid_s, tool_name);
        return CompatEdit::Changed(insert_after(&lines, ctm_open, &block, text.len()));
    }

    if let Some(steam_open) = steam_open_idx {
        // No CompatToolMapping at all → create it (with our one mapping) as a
        // direct child of the Steam block.
        let ctm_indent = steam_inner_indent
            .unwrap_or_else(|| format!("{}\t", super::vdf::leading_ws(lines[steam_open])));
        let eol = super::vdf::line_ending(lines[steam_open]);
        let appid_indent = format!("{ctm_indent}\t");
        let inner = appid_block(&appid_indent, eol, &appid_s, tool_name);
        let block = format!(
            "{ctm_indent}\"CompatToolMapping\"{eol}{ctm_indent}{{{eol}{inner}{ctm_indent}}}{eol}"
        );
        return CompatEdit::Changed(insert_after(&lines, steam_open, &block, text.len()));
    }

    CompatEdit::NoSteamBlock
}

/// The three canonical leaf lines (`name` / `config` / `priority`) at `indent`.
fn canonical_leaves(indent: &str, eol: &str, tool_name: &str) -> String {
    let name = super::vdf::escape_vdf(tool_name);
    format!(
        "{indent}\"name\"\t\t\"{name}\"{eol}\
         {indent}\"config\"\t\t\"\"{eol}\
         {indent}\"priority\"\t\t\"{FORCE_PRIORITY}\"{eol}"
    )
}

/// A full `"<appid>" { name config priority }` block, key + braces at
/// `key_indent`, leaves one tab deeper.
fn appid_block(key_indent: &str, eol: &str, appid: &str, tool_name: &str) -> String {
    let leaves = canonical_leaves(&format!("{key_indent}\t"), eol, tool_name);
    format!("{key_indent}\"{appid}\"{eol}{key_indent}{{{eol}{leaves}{key_indent}}}{eol}")
}

/// Rebuild the file with `block` inserted immediately after line `idx`.
fn insert_after(lines: &[&str], idx: usize, block: &str, cap_hint: usize) -> String {
    let mut out = String::with_capacity(cap_hint + block.len());
    for (i, l) in lines.iter().enumerate() {
        out.push_str(l);
        if i == idx {
            out.push_str(block);
        }
    }
    out
}

// ── compatibilitytool.vdf parsing (installed-tool discovery) ─────────────────

/// One custom compat tool installed under a `compatibilitytools.d`. `internal` is
/// the key Steam stores in `CompatToolMapping` (`name`); `display` is its
/// human label (used for ranked matching).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompatTool {
    pub internal: String,
    pub display: String,
}

/// Parse a `compatibilitytool.vdf` body, returning the tools it registers.
/// Structure: `compatibilitytools > compat_tools > "<internal>" { display_name … }`.
/// A file may register more than one tool. Robust to indentation and casing.
pub(crate) fn parse_compat_tools(text: &str) -> Vec<CompatTool> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut stack: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    // While inside a tool block, its internal name so we can attach display_name.
    let mut cur_tool: Option<String> = None;
    let mut cur_depth = 0usize;

    for raw in text.lines() {
        let line = raw.trim();
        if line == "{" {
            let key = pending.take().unwrap_or_default();
            stack.push(key);
            // depth 3 == compatibilitytools>compat_tools><internal>
            if stack.len() == 3
                && stack[0].eq_ignore_ascii_case("compatibilitytools")
                && stack[1].eq_ignore_ascii_case("compat_tools")
            {
                let internal = stack[2].clone();
                out.entry(internal.clone()).or_default();
                cur_tool = Some(internal);
                cur_depth = stack.len();
            }
        } else if line == "}" {
            if cur_tool.is_some() && stack.len() == cur_depth {
                cur_tool = None;
            }
            stack.pop();
        } else {
            let toks = super::vdf::quoted_tokens(line);
            match toks.as_slice() {
                [k] => pending = Some(k.clone()),
                [k, v] => {
                    if let Some(t) = &cur_tool {
                        if k.eq_ignore_ascii_case("display_name") {
                            out.insert(t.clone(), v.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    out.into_iter()
        .map(|(internal, display)| {
            let display = if display.is_empty() {
                internal.clone()
            } else {
                display
            };
            CompatTool { internal, display }
        })
        .collect()
}

/// Rank a tool by how likely its `winewayland.drv` works, highest first. `0`
/// means "no positive signal" — never auto-pin those. Matches on both the
/// internal name and the display label, case-insensitively.
///
/// Order reflects ground truth on the rig: `proton-cachyos*` (native Wayland)
/// is the validated winner; GE-Proton exposes winewayland in recent builds; a
/// bare "native"/"wayland" marker is a weaker but positive hint.
pub(crate) fn wayland_rank(t: &CompatTool) -> u32 {
    let hay = format!("{} {}", t.internal, t.display).to_ascii_lowercase();
    let has = |needle: &str| hay.contains(needle);
    if has("cachyos") && has("native") {
        return 5;
    }
    if has("cachyos") {
        return 4;
    }
    if has("ge-proton") || has("proton-ge") || has("ge_proton") || has("glorious") {
        return 3;
    }
    if has("native") {
        return 2;
    }
    if has("wayland") {
        return 1;
    }
    0
}

/// Choose the best installed tool to pin: the highest [`wayland_rank`] with a
/// positive score. Ties break by name for determinism. `None` when nothing
/// installed looks Wayland-capable (caller then declines to pin).
pub(crate) fn choose_tool(tools: &[CompatTool]) -> Option<&CompatTool> {
    tools.iter().filter(|t| wayland_rank(t) > 0).max_by(|a, b| {
        wayland_rank(a)
            .cmp(&wayland_rank(b))
            .then_with(|| b.internal.cmp(&a.internal)) // stable: earlier name wins
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic config.vdf: appid 0 is the global default (proton_experimental),
    // 1091500 is unmapped (absent), 3321460 forced to a custom GE build, 570 to a
    // versioned Valve built-in.
    fn fixture() -> String {
        [
            "\"InstallConfigStore\"",
            "{",
            "\t\"Software\"",
            "\t{",
            "\t\t\"Valve\"",
            "\t\t{",
            "\t\t\t\"Steam\"",
            "\t\t\t{",
            "\t\t\t\t\"CompatToolMapping\"",
            "\t\t\t\t{",
            "\t\t\t\t\t\"0\"",
            "\t\t\t\t\t{",
            "\t\t\t\t\t\t\"name\"\t\t\"proton_experimental\"",
            "\t\t\t\t\t\t\"config\"\t\t\"\"",
            "\t\t\t\t\t\t\"priority\"\t\t\"75\"",
            "\t\t\t\t\t}",
            "\t\t\t\t\t\"3321460\"",
            "\t\t\t\t\t{",
            "\t\t\t\t\t\t\"name\"\t\t\"GE-Proton9-20\"",
            "\t\t\t\t\t\t\"config\"\t\t\"\"",
            "\t\t\t\t\t\t\"priority\"\t\t\"250\"",
            "\t\t\t\t\t}",
            "\t\t\t\t\t\"570\"",
            "\t\t\t\t\t{",
            "\t\t\t\t\t\t\"name\"\t\t\"proton_9\"",
            "\t\t\t\t\t\t\"config\"\t\t\"\"",
            "\t\t\t\t\t\t\"priority\"\t\t\"250\"",
            "\t\t\t\t\t}",
            "\t\t\t\t}",
            "\t\t\t}",
            "\t\t}",
            "\t}",
            "}",
            "",
        ]
        .join("\n")
    }

    const TOOL: &str = "proton_cachyos";

    fn changed(r: CompatEdit) -> String {
        match r {
            CompatEdit::Changed(s) => s,
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn inserts_when_appid_absent() {
        let f = fixture();
        let out = changed(set_compat_tool(&f, 1091500, TOOL));
        // Exactly one new balanced pair (the inserted appid block).
        assert_eq!(out.matches('{').count(), f.matches('{').count() + 1);
        assert_eq!(out.matches('}').count(), f.matches('}').count() + 1);
        assert!(out.contains("\"1091500\""));
        assert!(out.contains(&format!("\"name\"\t\t\"{TOOL}\"")));
        assert!(out.contains("\"priority\"\t\t\"250\""));
    }

    #[test]
    fn overrides_valve_versioned_default() {
        // 570 forced to proton_9 (a Valve built-in) → override to our tool.
        let out = changed(set_compat_tool(&fixture(), 570, TOOL));
        // The 570 block now names our tool; proton_9 is gone from it.
        let seg = &out[out.find("\"570\"").unwrap()..];
        let seg = &seg[..seg.find('}').unwrap()];
        assert!(seg.contains(&format!("\"name\"\t\t\"{TOOL}\"")));
        assert!(!seg.contains("proton_9"));
    }

    #[test]
    fn overrides_experimental_default() {
        // Pin the global "0" default itself (it's proton_experimental).
        let out = changed(set_compat_tool(&fixture(), 0, TOOL));
        let seg = &out[out.find("\"0\"").unwrap()..];
        let seg = &seg[..seg.find('}').unwrap()];
        assert!(seg.contains(&format!("\"name\"\t\t\"{TOOL}\"")));
        assert!(!seg.contains("proton_experimental"));
    }

    #[test]
    fn refuses_user_forced_custom_tool() {
        // 3321460 forced to GE-Proton (a custom, non-Valve tool) → untouched.
        assert_eq!(
            set_compat_tool(&fixture(), 3321460, TOOL),
            CompatEdit::Refused
        );
    }

    #[test]
    fn already_ours_is_idempotent() {
        let once = changed(set_compat_tool(&fixture(), 1091500, TOOL));
        assert_eq!(
            set_compat_tool(&once, 1091500, TOOL),
            CompatEdit::AlreadyOurs
        );
    }

    #[test]
    fn override_is_idempotent() {
        let once = changed(set_compat_tool(&fixture(), 570, TOOL));
        assert_eq!(set_compat_tool(&once, 570, TOOL), CompatEdit::AlreadyOurs);
    }

    #[test]
    fn creates_mapping_when_absent() {
        // A config.vdf with a Steam block but NO CompatToolMapping.
        let f = fixture().replace(
            "\t\t\t\t\"CompatToolMapping\"\n\t\t\t\t{\n\t\t\t\t\t\"0\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton_experimental\"\n\t\t\t\t\t\t\"config\"\t\t\"\"\n\t\t\t\t\t\t\"priority\"\t\t\"75\"\n\t\t\t\t\t}\n\t\t\t\t\t\"3321460\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"GE-Proton9-20\"\n\t\t\t\t\t\t\"config\"\t\t\"\"\n\t\t\t\t\t\t\"priority\"\t\t\"250\"\n\t\t\t\t\t}\n\t\t\t\t\t\"570\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton_9\"\n\t\t\t\t\t\t\"config\"\t\t\"\"\n\t\t\t\t\t\t\"priority\"\t\t\"250\"\n\t\t\t\t\t}\n\t\t\t\t}\n",
            "",
        );
        assert!(!f.contains("CompatToolMapping"));
        let out = changed(set_compat_tool(&f, 1091500, TOOL));
        assert!(out.contains("\"CompatToolMapping\""));
        assert!(out.contains("\"1091500\""));
        assert!(out.contains(&format!("\"name\"\t\t\"{TOOL}\"")));
        // Two new balanced pairs: the CompatToolMapping block + the appid block.
        assert_eq!(out.matches('{').count(), f.matches('{').count() + 2);
        assert_eq!(out.matches('}').count(), f.matches('}').count() + 2);
    }

    #[test]
    fn no_steam_block() {
        assert_eq!(
            set_compat_tool("\"Foo\"\n{\n}\n", 10, TOOL),
            CompatEdit::NoSteamBlock
        );
    }

    #[test]
    fn appid_whole_field_no_overmatch() {
        // Pinning 10 must not touch 1091500 (substring) — 10 is absent → insert.
        let f = fixture().replace(
            "\t\t\t\t\t\"570\"",
            "\t\t\t\t\t\"1091500\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton_experimental\"\n\t\t\t\t\t}\n\t\t\t\t\t\"570\"",
        );
        let out = changed(set_compat_tool(&f, 10, TOOL));
        // 1091500 kept its experimental mapping (we inserted a separate "10").
        assert!(out.contains("\"1091500\""));
        assert!(out.contains("\"10\""));
    }

    #[test]
    fn preserves_other_bytes_on_insert() {
        let f = fixture();
        let out = changed(set_compat_tool(&f, 1091500, TOOL));
        // Removing exactly the inserted block yields the original.
        let block = "\t\t\t\t\t\"1091500\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"name\"\t\t\"proton_cachyos\"\n\t\t\t\t\t\t\"config\"\t\t\"\"\n\t\t\t\t\t\t\"priority\"\t\t\"250\"\n\t\t\t\t\t}\n";
        assert_eq!(out.replacen(block, "", 1), f);
    }

    // Opt-in real-file check, mirroring vdf::real_localconfig:
    // `PHAROS_TEST_CONFIGVDF=<copy> PHAROS_TEST_APPID=<id> cargo test real_config_vdf`.
    // Verifies the surgical edit on a genuine config.vdf keeps braces balanced and
    // is idempotent. Silent no-op when the env isn't set (CI-safe).
    #[test]
    fn real_config_vdf() {
        let (Some(path), Some(appid)) = (
            std::env::var_os("PHAROS_TEST_CONFIGVDF"),
            std::env::var("PHAROS_TEST_APPID")
                .ok()
                .and_then(|s| s.parse::<u32>().ok()),
        ) else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap();
        let tool = std::env::var("PHAROS_TEST_PROTON").unwrap_or_else(|_| TOOL.to_string());
        match set_compat_tool(&text, appid, &tool) {
            CompatEdit::Changed(out) => {
                assert_eq!(
                    out.matches('{').count(),
                    out.matches('}').count(),
                    "braces balanced"
                );
                // We only ever add whole appid/CTM blocks or rewrite one block's
                // body — never drop a brace from the original.
                assert!(
                    out.matches('{').count() >= text.matches('{').count(),
                    "no braces lost"
                );
                assert_eq!(
                    set_compat_tool(&out, appid, &tool),
                    CompatEdit::AlreadyOurs,
                    "idempotent"
                );
                eprintln!("real_config_vdf: appid {appid} → Changed (pinned {tool}), idempotent ✓");
            }
            other => eprintln!("real_config_vdf: appid {appid} → {other:?}"),
        }
    }

    // ── policy + detection unit tests ───────────────────────────────────────

    #[test]
    fn valve_default_classification() {
        for y in [
            "",
            "proton_experimental",
            "proton_next",
            "proton_hotfix",
            "proton_9",
            "proton_9_0_4",
            "proton_63",
            "PROTON_9", // case-insensitive
        ] {
            assert!(is_valve_default_name(y), "{y:?} should be a Valve default");
        }
        for n in [
            "proton_cachyos",
            "GE-Proton9-20",
            "proton_ge",
            "proton_tkg",
            "luxtorpeda",
        ] {
            assert!(!is_valve_default_name(n), "{n:?} should NOT be");
        }
    }

    #[test]
    fn parse_compat_tools_extracts_internal_and_display() {
        let vdf = "\"compatibilitytools\"\n{\n\t\"compat_tools\"\n\t{\n\t\t\"proton_cachyos\"\n\t\t{\n\t\t\t\"install_path\"\t\".\"\n\t\t\t\"display_name\"\t\"Proton-CachyOS\"\n\t\t\t\"from_oslist\"\t\"windows\"\n\t\t}\n\t}\n}\n";
        let tools = parse_compat_tools(vdf);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].internal, "proton_cachyos");
        assert_eq!(tools[0].display, "Proton-CachyOS");
    }

    #[test]
    fn parse_compat_tools_defaults_display_to_internal() {
        let vdf = "\"compatibilitytools\"\n{\n\t\"compat_tools\"\n\t{\n\t\t\"weird_tool\"\n\t\t{\n\t\t\t\"install_path\"\t\".\"\n\t\t}\n\t}\n}\n";
        let tools = parse_compat_tools(vdf);
        assert_eq!(tools[0].display, "weird_tool");
    }

    #[test]
    fn choose_prefers_cachyos_then_ge() {
        let tools = vec![
            CompatTool {
                internal: "GE-Proton9-20".into(),
                display: "GE-Proton9-20".into(),
            },
            CompatTool {
                internal: "proton_cachyos".into(),
                display: "Proton-CachyOS".into(),
            },
            CompatTool {
                internal: "luxtorpeda".into(),
                display: "Luxtorpeda".into(),
            },
        ];
        assert_eq!(choose_tool(&tools).unwrap().internal, "proton_cachyos");
    }

    #[test]
    fn choose_none_when_nothing_wayland_capable() {
        let tools = vec![CompatTool {
            internal: "luxtorpeda".into(),
            display: "Luxtorpeda".into(),
        }];
        assert!(choose_tool(&tools).is_none());
    }
}
