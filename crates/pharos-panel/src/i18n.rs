//! Internationalisation (i18n) — modular, English-default.
//!
//! Backed by `rust-i18n`. The `rust_i18n::i18n!("locales", fallback = "en")`
//! call lives in `main.rs` (it must be at the crate root); this module wraps the
//! runtime API so the rest of the panel never touches `rust_i18n` directly.
//!
//! **Project directive:** user-facing strings go through [`tr`] (or the `t!`
//! macro) keyed into the per-module files under `crates/pharos-panel/locales/`
//! (v2 format — one file per area, e.g. `overview.toml`, so parallel edits never
//! collide). English is the default and the fallback for any missing key, so a
//! partially-translated locale degrades gracefully to English rather than
//! showing raw keys.
//!
//! Adding a language is purely additive: drop its code (e.g. `"fr"`) alongside
//! `en` under each key, and it appears in Settings → Appearance → Language
//! automatically (the picker is driven by [`available`]).

/// Translate `key` for the current locale, falling back to English, and return
/// an owned `String` so it drops straight into egui (`RichText::new`, `ui.label`,
/// `.describe(…)`) with no borrow gymnastics.
pub fn tr(key: &str) -> String {
    rust_i18n::t!(key).to_string()
}

/// Set the active locale (e.g. `"en"`). No-op-safe for unknown codes — `t!`
/// still falls back to English.
pub fn set_locale(code: &str) {
    rust_i18n::set_locale(code);
}

/// The current locale code.
pub fn current() -> String {
    rust_i18n::locale().to_string()
}

/// All locales that have at least one translation present, sorted with English
/// first. Drives the language picker, so new locale files show up for free.
pub fn available() -> Vec<String> {
    let mut v: Vec<String> = rust_i18n::available_locales!()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    v.sort_by(|a, b| match (a.as_str(), b.as_str()) {
        ("en", _) => std::cmp::Ordering::Less,
        (_, "en") => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    });
    if v.is_empty() {
        v.push("en".to_string());
    }
    v
}

/// Human-readable name for a locale `code`, localised itself (key `lang.<code>`),
/// falling back to the bare code if no name is registered.
pub fn display_name(code: &str) -> String {
    let key = format!("lang.{code}");
    let name = tr(&key);
    // `t!` returns the key itself when missing; show the raw code in that case.
    if name == key {
        code.to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn locales_resolve() {
        // English (default).
        super::set_locale("en");
        assert_eq!(super::tr("app.title"), "Pharos Control");
        assert_eq!(super::tr("tab.status"), "Overview");
        // Missing keys fall back to the key itself (never panic).
        assert_eq!(super::tr("does.not.exist"), "does.not.exist");
        assert_eq!(super::display_name("en"), "English");

        // Norwegian (Bokmål) — added as a full locale.
        super::set_locale("nb");
        assert_eq!(super::tr("tab.settings"), "Innstillinger");
        assert_eq!(super::tr("tab.audio"), "Lyd");
        assert_eq!(super::display_name("nb"), "Norsk");
        // A key only present in English still resolves via fallback (never tofu/key).
        assert_eq!(super::tr("app.title"), "Pharos Control");

        // Both locales are discoverable for the language picker.
        let langs = super::available();
        assert!(langs.contains(&"en".to_string()));
        assert!(langs.contains(&"nb".to_string()));

        super::set_locale("en"); // restore default for any other tests
    }

    /// Every **static** `tr("…")` key used anywhere in `src/` must resolve in a
    /// locale file — otherwise the UI silently renders the raw key. Walks the
    /// source, extracts literal keys (dynamic `tr(&format!(…))` keys are skipped
    /// by shape), and asserts each one resolves. Because a present key falls back
    /// to English in any locale, "resolves" == `tr(key) != key`, which is what a
    /// *missing* key returns. Guards future i18n edits from introducing dead keys.
    #[test]
    fn every_static_tr_key_resolves() {
        use std::fs;
        use std::path::{Path, PathBuf};

        super::set_locale("en");

        fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).expect("read_dir src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs")
                    // Skip this file: its other test intentionally references a
                    // non-existent key to prove the fallback path.
                    && path.file_name().is_some_and(|n| n != "i18n.rs")
                {
                    out.push(path);
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect(&src, &mut files);
        assert!(!files.is_empty(), "no .rs files found under {src:?}");

        let mut missing: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for file in &files {
            let text = fs::read_to_string(file).expect("read source");
            let bytes = text.as_bytes();
            let mut i = 0;
            while let Some(rel) = text[i..].find("tr(\"") {
                let start = i + rel;
                // The char before `tr` must not be part of a longer identifier, so
                // `substr("…")` / `attr("…")` don't masquerade as a `tr` call.
                let boundary_ok = start == 0
                    || !matches!(bytes[start - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_');
                let key_start = start + 4; // past `tr("`
                let Some(end_rel) = text[key_start..].find('"') else {
                    break;
                };
                let key = &text[key_start..key_start + end_rel];
                // Real keys are dotted; `{`/`\` would mean an interpolated/dynamic
                // fragment we can't statically resolve.
                if boundary_ok && key.contains('.') && !key.contains(['{', '\\']) {
                    checked += 1;
                    if super::tr(key) == key {
                        missing.push(format!(
                            "{}: tr(\"{key}\")",
                            file.file_name().unwrap().to_string_lossy()
                        ));
                    }
                }
                i = key_start + end_rel + 1;
            }
        }

        // Sanity-check the scanner itself didn't silently match nothing.
        assert!(
            checked > 50,
            "only {checked} static keys scanned — the extractor likely broke"
        );
        assert!(
            missing.is_empty(),
            "{} static tr(\"…\") key(s) don't resolve in any locale (add them under locales/):\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }
}
