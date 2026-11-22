// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Per-controller tuning persistence (`controller-profiles.toml` in the Pharos
//! config dir).
//!
//! Pharos is the source of truth for controller feel: the host reads the IMU and
//! injects the virtual pad, so it's where gyro/swap settings are *applied*. They
//! persist here keyed by the canonical `"vvvv:pppp"` controller id the client
//! also uses, so a controller gets its saved feel the instant it connects — no
//! matter which client is driving. The client is a live remote editor that
//! relays edits over the back-channel; the host applies + writes them here.
//!
//! Like `peers.toml`, this is opt-in and non-fatal: a missing or unparseable
//! file yields the built-in defaults, never blocking a stream.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use core_proto::ControllerProfile;
use serde::{Deserialize, Serialize};

/// On-disk store: per-controller saved settings plus reusable named presets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControllerProfiles {
    /// Saved settings per controller, keyed `"vvvv:pppp"` (see [`key`]).
    #[serde(default)]
    pub controllers: HashMap<String, ControllerProfile>,
    /// Named presets the user can save once and apply to any controller.
    #[serde(default)]
    pub presets: HashMap<String, ControllerProfile>,
}

/// Canonical controller id, matching the client's `controllerKey(vid,pid)`.
pub fn key(vendor: u16, product: u16) -> String {
    format!("{vendor:04x}:{product:04x}")
}

pub fn profiles_path() -> Result<PathBuf> {
    Ok(crate::pair::config_dir()?.join("controller-profiles.toml"))
}

impl ControllerProfiles {
    /// Load from disk. A missing file or any parse error is **non-fatal** and
    /// returns an empty store (built-in defaults); parse errors are logged.
    pub fn load() -> Self {
        let path = match profiles_path() {
            Ok(p) => p,
            Err(_) => return Self::default(),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Self::default(), // no file → defaults
        };
        match toml::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "controller-profiles.toml at {} unparseable: {e}; using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// The saved profile for a controller, or the built-in default feel if the
    /// controller has never been tuned.
    pub fn get(&self, vendor: u16, product: u16) -> ControllerProfile {
        // Cloned (not `.copied()`) because `ControllerProfile` now carries an
        // `Option<String>` alias and is no longer `Copy`.
        self.controllers
            .get(&key(vendor, product))
            .cloned()
            .unwrap_or_default()
    }

    /// Replace a controller's profile in memory (call [`save`] to persist).
    pub fn set(&mut self, vendor: u16, product: u16, profile: ControllerProfile) {
        self.controllers.insert(key(vendor, product), profile);
    }

    /// Atomic write (temp file + rename), same shape as `peers.toml`.
    pub fn save(&self) -> Result<()> {
        let path = profiles_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).context("serialize controller profiles")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsaved_controller_gets_default() {
        let p = ControllerProfiles::default();
        assert_eq!(p.get(0x057e, 0x2009), ControllerProfile::default());
    }

    #[test]
    fn set_then_get_roundtrips_through_toml() {
        let mut store = ControllerProfiles::default();
        let mut prof = ControllerProfile {
            swap_ab: true,
            ..Default::default()
        };
        prof.gyro.sens_x = 22.5;
        prof.gyro.invert_y = true;
        prof.gyro.axis_h = 0;
        store.set(0x057e, 0x2009, prof.clone());

        // Serialize → parse, the same path load()/save() take on disk.
        let text = toml::to_string_pretty(&store).unwrap();
        let back: ControllerProfiles = toml::from_str(&text).unwrap();
        assert_eq!(back.get(0x057e, 0x2009), prof);
        assert_eq!(back.get(0x045e, 0x028e), ControllerProfile::default());
    }

    #[test]
    fn key_matches_client_format() {
        assert_eq!(key(0x057e, 0x2009), "057e:2009");
    }
}
