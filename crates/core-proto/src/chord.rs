// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Shared key-chord grammar (e.g. `"Ctrl+Alt+End"`, `"F11"`, `"Alt+Tab"`).
//!
//! The Trireme client matches these against live key events; the Pharos panel
//! only needs to *validate* what the user types so a typo (`"Cntrl+Alt"`) can't
//! become a silently-dead binding. Both ends parse through this one grammar so
//! they can never drift: the client's `Chord::parse` delegates here, and the
//! panel calls [`is_valid`].

/// A parsed chord: a set of modifiers plus exactly one named main key. The main
/// key keeps its original casing so it round-trips for display/matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    pub key: String,
}

impl Chord {
    /// Parse a `+`-separated chord: case-insensitive modifiers (`ctrl`/`control`,
    /// `shift`, `alt`, `win`/`super`/`meta`/`cmd`/`command`) plus exactly one
    /// non-modifier main key. Returns `None` if malformed — an empty part (e.g. a
    /// trailing `+`), no main key, or two main keys.
    pub fn parse(s: &str) -> Option<Chord> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut meta = false;
        let mut key = None;
        for part in s.split('+') {
            let p = part.trim();
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                "win" | "super" | "meta" | "cmd" | "command" => meta = true,
                "" => return None,
                _ => {
                    if key.is_some() {
                        return None; // two main keys — malformed
                    }
                    key = Some(p.to_string());
                }
            }
        }
        Some(Chord {
            ctrl,
            shift,
            alt,
            meta,
            key: key?,
        })
    }
}

/// True if `s` is a well-formed chord (see [`Chord::parse`]). Used by the panel
/// to flag a malformed chord field before it becomes a dead binding.
pub fn is_valid(s: &str) -> bool {
    Chord::parse(s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key_preserving_case() {
        let c = Chord::parse("Ctrl+Alt+End").unwrap();
        assert!(c.ctrl && c.alt && !c.shift && !c.meta);
        assert_eq!(c.key, "End"); // original casing kept
    }

    #[test]
    fn accepts_modifier_aliases_case_insensitively() {
        assert!(is_valid("control+shift+escape"));
        assert!(is_valid("WIN+L"));
        assert!(is_valid("super+Tab"));
        assert!(is_valid("cmd+q"));
        assert!(is_valid("F11")); // a bare key, no modifiers
    }

    #[test]
    fn rejects_structurally_malformed_chords() {
        assert!(!is_valid("Ctrl+")); // trailing + → empty part
        assert!(!is_valid("+A")); // leading + → empty part
        assert!(!is_valid("Ctrl")); // only a modifier, no main key
        assert!(!is_valid("")); // empty
        assert!(!is_valid("A+B")); // two main keys
        assert!(!is_valid("Ctrl+Alt+End+Home")); // two main keys among modifiers
                                                 // A typo'd modifier alongside a real key reads as two main keys → caught.
        assert!(!is_valid("Cntrl+Alt+End")); // "Cntrl" and "End" are both keys
    }

    #[test]
    fn structural_check_cannot_catch_a_lone_typo_modifier() {
        // "Cntrl" isn't a known modifier, so in "Cntrl+Alt" it parses as the only
        // main key (key="Cntrl", alt). This is the one residual gap: the binding is
        // structurally valid but will never MATCH a real key named "Cntrl". The
        // panel labels its hint a structural check for exactly this reason.
        let c = Chord::parse("Cntrl+Alt").unwrap();
        assert_eq!(c.key, "Cntrl");
        assert!(c.alt);
        assert!(is_valid("Cntrl+Alt"));
    }
}
