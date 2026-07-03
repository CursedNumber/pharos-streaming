// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Minimal, *surgical* editor for Steam's `localconfig.vdf` — just enough to set
//! one game's `LaunchOptions` idempotently without reflowing the rest of the file.
//!
//! `localconfig.vdf` is tab-indented KeyValues written one token per line, exactly
//! as Steam serialises it: a value entry is `"key"\t\t"value"` and a subtable is a
//! `"key"` line followed by a `{ … }` block. We navigate
//! `UserLocalConfigStore > Software > Valve > Steam > apps > "<appid>"` by
//! brace-depth (path keys matched case-insensitively, the **appid matched as a
//! whole field** so `10` never hits `1091500`), then replace or insert the single
//! `LaunchOptions` leaf. Every other byte of the file is preserved verbatim — we
//! never parse-and-reserialise, so we can't corrupt untouched subtrees.

/// Outcome of a surgical `LaunchOptions` edit on the in-memory file text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditResult {
    /// The text changed; carries the new full-file content.
    Changed(String),
    /// The value is already exactly ours — no write needed.
    AlreadyOurs,
    /// The app already has a non-empty, non-ours value (user config) — untouched.
    Refused,
    /// No `"<appid>"` block under `…>Steam>apps` in this file.
    AppNotFound,
}

/// The container path to the per-app table, lowercased for case-insensitive match.
const APPS_PATH: [&str; 5] = ["userlocalconfigstore", "software", "valve", "steam", "apps"];

/// Route `apps > "<appid>" > LaunchOptions` through the `shim_cmd` redirect.
/// Empty options become `shim_cmd %command%`; existing user options are
/// ENVELOPED (`shim_cmd <existing>`, or `shim_cmd %command% <existing>` when they
/// carry no `%command%`) so wrappers / env-prefixes / trailing args survive;
/// already-ours options refresh the shim token in place (idempotent). Options
/// unsafe to wrap (shell operators, quotes, multiple `%command%`) are left
/// untouched (`Refused`). Whole-file bytes outside the one edited/inserted line
/// are preserved.
pub(crate) fn set_launch_options(
    text: &str,
    appid: u32,
    shim_cmd: &str,
    ours_token: &str,
) -> EditResult {
    let appid_s = appid.to_string();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    let mut stack: Vec<String> = Vec::new();
    let mut pending_key: Option<String> = None;

    let mut in_app = false;
    let mut app_open_idx: Option<usize> = None; // index of the appid's `{` line
    let mut inner_indent: Option<String> = None; // leading ws of an inner line
    let mut launchopts_idx: Option<usize> = None;
    let mut launchopts_val: Option<String> = None;
    let mut app_close_idx: Option<usize> = None;

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line == "{" {
            let key = pending_key.take().unwrap_or_default();
            stack.push(key);
            if !in_app && stack.len() == APPS_PATH.len() + 1 {
                let path_ok = stack[..APPS_PATH.len()]
                    .iter()
                    .zip(APPS_PATH.iter())
                    .all(|(s, p)| s.eq_ignore_ascii_case(p));
                if path_ok && stack[APPS_PATH.len()] == appid_s {
                    in_app = true;
                    app_open_idx = Some(i);
                }
            }
        } else if line == "}" {
            if in_app && stack.len() == APPS_PATH.len() + 1 {
                app_close_idx = Some(i);
                stack.pop();
                break; // the whole app block is closed — nothing more to scan
            }
            stack.pop();
        } else {
            let toks = quoted_tokens(line);
            match toks.len() {
                1 => pending_key = Some(toks.into_iter().next().unwrap()),
                2 => {
                    let mut it = toks.into_iter();
                    let key = it.next().unwrap();
                    let val = it.next().unwrap();
                    // Only a leaf that is a DIRECT child of the appid block counts —
                    // gate on the exact brace depth so a `LaunchOptions` buried in a
                    // nested subtable (e.g. a cloud/autocloud table) can't be picked.
                    if in_app && stack.len() == APPS_PATH.len() + 1 {
                        if inner_indent.is_none() {
                            inner_indent = Some(leading_ws(raw));
                        }
                        if key.eq_ignore_ascii_case("launchoptions") {
                            launchopts_idx = Some(i);
                            launchopts_val = Some(val);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let _ = app_close_idx;

    // Plan the new value: envelop existing options (or set fresh), refresh if
    // already ours, or refuse if unsafe to wrap.
    let new_value = match plan_value(
        launchopts_val.as_deref().unwrap_or(""),
        shim_cmd,
        ours_token,
    ) {
        PlanValue::Keep => return EditResult::AlreadyOurs,
        PlanValue::Refuse => return EditResult::Refused,
        PlanValue::Set(v) => v,
    };

    // Existing leaf → replace its value in place.
    if let Some(idx) = launchopts_idx {
        let raw = lines[idx];
        let indent = leading_ws(raw);
        let eol = line_ending(raw);
        let replacement = format!(
            "{indent}\"LaunchOptions\"\t\t\"{}\"{eol}",
            escape_vdf(&new_value)
        );
        let mut out = String::with_capacity(text.len() + replacement.len());
        for (i, l) in lines.iter().enumerate() {
            if i == idx {
                out.push_str(&replacement);
            } else {
                out.push_str(l);
            }
        }
        return EditResult::Changed(out);
    }

    // No LaunchOptions leaf: insert one right after the app block's `{`.
    let Some(open_idx) = app_open_idx else {
        return EditResult::AppNotFound;
    };
    let open_raw = lines[open_idx];
    let indent = inner_indent.unwrap_or_else(|| format!("{}\t", leading_ws(open_raw)));
    let eol = line_ending(open_raw);
    let inserted = format!(
        "{indent}\"LaunchOptions\"\t\t\"{}\"{eol}",
        escape_vdf(&new_value)
    );
    let mut out = String::with_capacity(text.len() + inserted.len());
    for (i, l) in lines.iter().enumerate() {
        out.push_str(l);
        if i == open_idx {
            out.push_str(&inserted);
        }
    }
    EditResult::Changed(out)
}

/// The planned LaunchOptions value.
enum PlanValue {
    /// Write this value.
    Set(String),
    /// Already exactly ours — no write.
    Keep,
    /// Unsafe to wrap — leave untouched.
    Refuse,
}

/// Decide how to (re)write LaunchOptions so the game launches through `shim_cmd`.
fn plan_value(existing: &str, shim_cmd: &str, ours: &str) -> PlanValue {
    let e = existing.trim();
    if e.is_empty() {
        return PlanValue::Set(format!("{shim_cmd} %command%"));
    }
    if is_ours(e, ours) {
        // Already enveloped: refresh the leading shim token (its path may have
        // moved), keep everything the user layered after it. Idempotent when the
        // shim path is unchanged — so re-injection never double-wraps.
        let rest = e
            .split_once(|c: char| c.is_whitespace())
            .map(|(_, r)| r.trim_start())
            .unwrap_or("");
        let rebuilt = if rest.is_empty() {
            shim_cmd.to_string()
        } else {
            format!("{shim_cmd} {rest}")
        };
        return if rebuilt == e {
            PlanValue::Keep
        } else {
            PlanValue::Set(rebuilt)
        };
    }
    if !safe_to_envelop(e) {
        return PlanValue::Refuse;
    }
    // Envelop: the shim becomes the outermost wrapper. With `%command%` present we
    // prepend the shim; without it (the options are game args) we insert
    // `%command%` so the shim still runs the game, then appends the user args.
    if e.contains("%command%") {
        PlanValue::Set(format!("{shim_cmd} {e}"))
    } else {
        PlanValue::Set(format!("{shim_cmd} %command% {e}"))
    }
}

/// Can these user options be safely prepend-enveloped? The shim runs them via
/// `env "$@"`, which honors leading `VAR=val` assignments and wrapper chains — but
/// shell control operators (`; | &` would split the command), quotes (whose
/// grouping we'd break), and multiple `%command%` (ambiguous substitution) cannot
/// be wrapped by a simple prepend, so those are refused.
fn safe_to_envelop(v: &str) -> bool {
    if v.matches("%command%").count() > 1 {
        return false;
    }
    if v.contains('"') || v.contains('\'') {
        return false;
    }
    const BAD: &[&str] = &[";", "|", "&", "`", "$(", ">", "<", "\n", "\r"];
    !BAD.iter().any(|b| v.contains(b))
}

/// Is `value` one of ours? Its first whitespace-delimited token's basename equals
/// `ours_token` (the shim basename), so it matches whether the option stores the
/// shim by full path or bare name.
fn is_ours(value: &str, ours_token: &str) -> bool {
    value
        .split_whitespace()
        .next()
        .map(|tok| tok.rsplit('/').next().unwrap_or(tok) == ours_token)
        .unwrap_or(false)
}

/// Leading whitespace (indentation) of a raw line.
fn leading_ws(raw: &str) -> String {
    raw.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// The line ending of a raw line (`\r\n`, `\n`, or empty at EOF).
fn line_ending(raw: &str) -> &'static str {
    if raw.ends_with("\r\n") {
        "\r\n"
    } else if raw.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// Extract the quoted tokens on a VDF line, honoring `\"` and `\\` escapes.
/// A subtable key line yields 1 token; a value leaf yields 2. Crate-visible so the
/// loginusers.vdf walk in [`super::linux`] can reuse the same tokenizer.
pub(crate) fn quoted_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == '"' {
            chars.next();
            let mut s = String::new();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            match n {
                                'n' => s.push('\n'),
                                't' => s.push('\t'),
                                other => s.push(other), // \" \\ and anything else literal
                            }
                        }
                    }
                    '"' => break,
                    other => s.push(other),
                }
            }
            out.push(s);
        } else {
            chars.next();
        }
    }
    out
}

/// Escape a value for a VDF quoted string: backslash and double-quote.
fn escape_vdf(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            other => s.push(other),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIM: &str = "/home/u/.local/share/pharos/pharos-redirect";
    const TOKEN: &str = "pharos-redirect";

    // A realistic two-app fixture: 3321460 has a user LaunchOptions, 1091500 has
    // an empty one, 10 has none, plus a decoy substring 1091500 vs 10.
    fn fixture() -> String {
        [
            "\"UserLocalConfigStore\"",
            "{",
            "\t\"Software\"",
            "\t{",
            "\t\t\"Valve\"",
            "\t\t{",
            "\t\t\t\"Steam\"",
            "\t\t\t{",
            "\t\t\t\t\"apps\"",
            "\t\t\t\t{",
            "\t\t\t\t\t\"10\"",
            "\t\t\t\t\t{",
            "\t\t\t\t\t\t\"LastPlayed\"\t\t\"123\"",
            "\t\t\t\t\t}",
            "\t\t\t\t\t\"1091500\"",
            "\t\t\t\t\t{",
            "\t\t\t\t\t\t\"LaunchOptions\"\t\t\"\"",
            "\t\t\t\t\t}",
            "\t\t\t\t\t\"3321460\"",
            "\t\t\t\t\t{",
            "\t\t\t\t\t\t\"LaunchOptions\"\t\t\"env WAYLAND_DISPLAY=wayland-1 %command%\"",
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

    fn changed(r: EditResult) -> String {
        match r {
            EditResult::Changed(s) => s,
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn inserts_when_no_launchoptions() {
        // appid 10 has no LaunchOptions leaf → insert one, indented inside its block.
        let out = changed(set_launch_options(&fixture(), 10, SHIM, TOKEN));
        assert!(out.contains(&format!("\"LaunchOptions\"\t\t\"{SHIM} %command%\"")));
        // Inserted right inside app 10's block (before LastPlayed's line's sibling).
        assert!(out.contains("\"10\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\""));
    }

    #[test]
    fn sets_empty_launchoptions() {
        let out = changed(set_launch_options(&fixture(), 1091500, SHIM, TOKEN));
        assert!(out.contains(&format!("\"LaunchOptions\"\t\t\"{SHIM} %command%\"")));
        // whole-field: appid 10's block did NOT get the value (no over-match).
        let ten_block = &out[out.find("\"10\"").unwrap()..out.find("\"1091500\"").unwrap()];
        assert!(
            !ten_block.contains("pharos-redirect"),
            "appid 10 untouched: {ten_block}"
        );
    }

    #[test]
    fn envelops_env_prefix_user_options() {
        // 3321460 = `env WAYLAND_DISPLAY=wayland-1 %command%` → the shim is
        // prepended, the user's env prefix preserved.
        let out = changed(set_launch_options(&fixture(), 3321460, SHIM, TOKEN));
        assert!(out.contains(&format!(
            "\"LaunchOptions\"\t\t\"{SHIM} env WAYLAND_DISPLAY=wayland-1 %command%\""
        )));
    }

    #[test]
    fn envelops_wrapper_user_options() {
        let f = fixture().replace(
            "env WAYLAND_DISPLAY=wayland-1 %command%",
            "mangohud %command% -skipStartScreen",
        );
        let out = changed(set_launch_options(&f, 3321460, SHIM, TOKEN));
        assert!(out.contains(&format!("\"{SHIM} mangohud %command% -skipStartScreen\"")));
    }

    #[test]
    fn envelops_arg_only_options_by_inserting_command() {
        // No %command% → the options are game args; wrap so the shim runs the game
        // and appends them.
        let f = fixture().replace("env WAYLAND_DISPLAY=wayland-1 %command%", "-dx11 -windowed");
        let out = changed(set_launch_options(&f, 3321460, SHIM, TOKEN));
        assert!(out.contains(&format!("\"{SHIM} %command% -dx11 -windowed\"")));
    }

    #[test]
    fn refuses_shell_operator_options() {
        let f = fixture().replace(
            "env WAYLAND_DISPLAY=wayland-1 %command%",
            "foo; bar %command%",
        );
        assert_eq!(
            set_launch_options(&f, 3321460, SHIM, TOKEN),
            EditResult::Refused
        );
    }

    // ── Malformed / duplicate configs: must never corrupt (braces never change) ──

    #[test]
    fn duplicate_launchoptions_in_app_block_never_corrupts() {
        // Two LaunchOptions leaves in one app block (a malformed hand-edit). We only
        // ever replace/insert a value line — braces are NEVER touched — so the file
        // stays structurally valid. We edit the last (Steam's effective one is
        // itself ambiguous); the first is left alone.
        let f = fixture().replace(
            "\t\t\t\t\t\t\"LaunchOptions\"\t\t\"env WAYLAND_DISPLAY=wayland-1 %command%\"",
            "\t\t\t\t\t\t\"LaunchOptions\"\t\t\"first %command%\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"second %command%\"",
        );
        let out = changed(set_launch_options(&f, 3321460, SHIM, TOKEN));
        assert_eq!(out.matches('{').count(), f.matches('{').count(), "braces intact");
        assert_eq!(out.matches('}').count(), f.matches('}').count(), "braces intact");
        assert!(out.contains("\"first %command%\""), "first duplicate untouched");
        assert!(out.contains(&format!("\"{SHIM} second %command%\"")), "last enveloped");
    }

    #[test]
    fn launchoptions_outside_apps_path_untouched() {
        // A LaunchOptions leaf under a DIFFERENT Steam subsection (not
        // …>Steam>apps>appid) is never matched — path + depth gate it out.
        let f = fixture().replace(
            "\t\t\t\t\"apps\"",
            "\t\t\t\t\"News\"\n\t\t\t\t{\n\t\t\t\t\t\"LaunchOptions\"\t\t\"DECOY\"\n\t\t\t\t}\n\t\t\t\t\"apps\"",
        );
        let out = changed(set_launch_options(&f, 10, SHIM, TOKEN));
        assert!(out.contains("\"DECOY\""), "decoy in other section untouched");
        assert!(out.contains(&format!("\"{SHIM} %command%\"")), "real app 10 got the shim");
    }

    #[test]
    fn duplicate_appid_blocks_edits_first_only() {
        // Two blocks for the same appid (malformed). We stop at the first block's
        // close, so only the first is edited and braces stay balanced.
        let f = fixture().replace(
            "\t\t\t\t\t\"3321460\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"env WAYLAND_DISPLAY=wayland-1 %command%\"\n\t\t\t\t\t}",
            "\t\t\t\t\t\"3321460\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"AAA %command%\"\n\t\t\t\t\t}\n\t\t\t\t\t\"3321460\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"BBB %command%\"\n\t\t\t\t\t}",
        );
        let out = changed(set_launch_options(&f, 3321460, SHIM, TOKEN));
        assert_eq!(out.matches('{').count(), f.matches('{').count(), "braces intact");
        assert!(out.contains(&format!("\"{SHIM} AAA %command%\"")), "first block enveloped");
        assert!(out.contains("\"BBB %command%\""), "second block untouched");
    }

    #[test]
    fn refuses_quoted_options() {
        let f = fixture().replace(
            "env WAYLAND_DISPLAY=wayland-1 %command%",
            "gamemoderun \\\"%command%\\\"",
        );
        assert_eq!(
            set_launch_options(&f, 3321460, SHIM, TOKEN),
            EditResult::Refused
        );
    }

    #[test]
    fn idempotent_already_ours() {
        // Inject (into empty), then re-inject → AlreadyOurs, no change.
        let once = changed(set_launch_options(&fixture(), 1091500, SHIM, TOKEN));
        let twice = set_launch_options(&once, 1091500, SHIM, TOKEN);
        assert_eq!(twice, EditResult::AlreadyOurs);
    }

    #[test]
    fn idempotent_after_envelop() {
        // Envelop a user value, then re-inject → AlreadyOurs (never double-wraps).
        let once = changed(set_launch_options(&fixture(), 3321460, SHIM, TOKEN));
        let twice = set_launch_options(&once, 3321460, SHIM, TOKEN);
        assert_eq!(twice, EditResult::AlreadyOurs);
    }

    #[test]
    fn refreshes_moved_shim_path_keeping_user_tail() {
        // A prior ours value with an OLD shim path → refresh the shim token, keep
        // the user's tail; never double-wrap.
        let stale = fixture().replace(
            "env WAYLAND_DISPLAY=wayland-1 %command%",
            "/old/pharos-redirect mangohud %command%",
        );
        let out = changed(set_launch_options(&stale, 3321460, SHIM, TOKEN));
        assert!(out.contains(&format!("\"{SHIM} mangohud %command%\"")));
        assert!(!out.contains("/old/pharos-redirect"));
    }

    #[test]
    fn ignores_launchoptions_in_nested_subtable() {
        // app 10 has NO direct LaunchOptions, but a nested subtable contains one —
        // we must insert a fresh DIRECT-child leaf and leave the nested one alone.
        let f = fixture().replace(
            "\t\t\t\t\t\t\"LastPlayed\"\t\t\"123\"",
            "\t\t\t\t\t\t\"cloud\"\n\t\t\t\t\t\t{\n\t\t\t\t\t\t\t\"LaunchOptions\"\t\t\"NESTED-KEEP\"\n\t\t\t\t\t\t}",
        );
        let out = changed(set_launch_options(&f, 10, SHIM, TOKEN));
        assert!(out.contains("\"NESTED-KEEP\""), "nested leaf untouched");
        assert!(
            out.contains(&format!("\"{SHIM} %command%\"")),
            "direct-child inserted"
        );
    }

    #[test]
    fn app_not_found() {
        let r = set_launch_options(&fixture(), 999999, SHIM, TOKEN);
        assert_eq!(r, EditResult::AppNotFound);
    }

    #[test]
    fn preserves_all_other_bytes() {
        let f = fixture();
        let out = changed(set_launch_options(&f, 10, SHIM, TOKEN));
        // Removing exactly the inserted line yields the original file.
        let inserted = format!("\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{SHIM} %command%\"\n");
        assert_eq!(out.replacen(&inserted, "", 1), f);
    }

    // Opt-in real-file check: `PHAROS_TEST_LOCALCONFIG=<copy> PHAROS_TEST_APPID=<id>
    // cargo test real_localconfig`. Verifies the surgical edit on a genuine
    // 900-line localconfig.vdf keeps braces balanced, changes exactly one line,
    // and is idempotent. Silent no-op when the env isn't set (CI-safe).
    #[test]
    fn real_localconfig() {
        let (Some(path), Some(appid)) = (
            std::env::var_os("PHAROS_TEST_LOCALCONFIG"),
            std::env::var("PHAROS_TEST_APPID")
                .ok()
                .and_then(|s| s.parse::<u32>().ok()),
        ) else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap();
        let v = "/x/pharos-redirect"; // shim_cmd (path), not the full value
        match set_launch_options(&text, appid, v, TOKEN) {
            EditResult::Changed(out) => {
                assert_eq!(
                    out.matches('{').count(),
                    text.matches('{').count(),
                    "braces balanced"
                );
                assert_eq!(
                    out.matches('}').count(),
                    text.matches('}').count(),
                    "braces balanced"
                );
                let d = out.lines().count() as i64 - text.lines().count() as i64;
                assert!((0..=1).contains(&d), "at most one line added, got {d}");
                assert_eq!(
                    set_launch_options(&out, appid, v, TOKEN),
                    EditResult::AlreadyOurs,
                    "idempotent"
                );
                eprintln!("real_localconfig: appid {appid} Changed (+{d} line), idempotent ✓");
            }
            other => eprintln!("real_localconfig: appid {appid} → {other:?}"),
        }
    }

    #[test]
    fn escapes_quotes_in_value() {
        let v = "/x/pharos-redirect \"q\" %command%";
        let out = changed(set_launch_options(&fixture(), 10, v, TOKEN));
        assert!(out.contains("\\\"q\\\""), "value quotes escaped: {out}");
    }
}
