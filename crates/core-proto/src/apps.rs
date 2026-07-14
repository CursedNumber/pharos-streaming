// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! App-launcher catalog relay between the host and a Trireme client.
//!
//! Carried on a dedicated bidirectional QUIC stream the **host opens** after a
//! client connects (a direction the client doesn't otherwise accept, so it never
//! collides with the input/video streams). Both ends speak [`AppMsg`], framed as
//! `u32 BE length || postcard bytes` — the same framing as input events:
//!
//! - host → client: [`AppMsg::Catalog`] — the launchable apps, with box art, so
//!   the client can render a tile grid. Re-sent whenever the catalog changes.
//! - client → host: [`AppMsg::Launch`] — "the user picked this app id"; the host
//!   resolves it against `apps.toml` and spawns it into the virtual display.
//!
//! Kept independent of the `app-launcher` crate (core-proto is a pure wire crate
//! with no host deps): the [`AppInfo::kind`] tag mirrors app-launcher's `AppKind`
//! as a plain `u8` via [`app_kind`].

use serde::{Deserialize, Serialize};

use crate::WireError;

/// Kind tags for [`AppInfo::kind`], mirroring `app_launcher::config::AppKind`.
pub mod app_kind {
    pub const APP: u8 = 0;
    pub const GAME: u8 = 1;
    pub const DESKTOP: u8 = 2;
    pub const MEDIA: u8 = 3;
}

/// One launchable app as a Trireme client sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
    /// The catalog slug — echoed back verbatim in [`AppMsg::Launch`].
    pub id: String,
    /// Display name for the tile.
    pub name: String,
    /// Kind tag (see [`app_kind`]) — drives the placeholder glyph.
    pub kind: u8,
    /// Box-art bytes (PNG/JPEG). Empty = the client draws a kind placeholder.
    pub art: Vec<u8>,
    /// True if this app is currently running on the host (its launch scope is
    /// active). The client floats running apps to the top, highlights them, and
    /// offers Switch-To / End-process on them. The host re-sends the catalog
    /// whenever this changes. Additive, but postcard is positional, so the host
    /// and Trireme are rebuilt + deployed in lockstep.
    pub running: bool,
}

/// A message on the app relay stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AppMsg {
    /// Host → client: the full catalog (replaces any previously sent).
    Catalog { apps: Vec<AppInfo> },
    /// Client → host: launch the app with this id.
    Launch { id: String },
    /// Client → host: raise + focus the running app with this id (bring to front).
    SwitchTo { id: String },
    /// Client → host: end the running app with this id — kill its whole process
    /// tree, including stragglers like a crash reporter.
    EndProcess { id: String },
    /// Host → client: a transient, non-blocking status note to surface as a
    /// toast/snackbar (e.g. "Steam was running on the desktop — restarted it
    /// in-stream"). Additive variant: postcard tags enum variants by index, so
    /// this is safe to APPEND (never reorder), and old clients that don't match
    /// it just ignore the frame. Host and Trireme still ship in lockstep.
    Notice { message: String },
}

/// Encode an [`AppMsg`] (postcard).
pub fn encode_app(msg: &AppMsg) -> Result<Vec<u8>, WireError> {
    postcard::to_stdvec(msg).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode an [`AppMsg`] (postcard).
pub fn decode_app(bytes: &[u8]) -> Result<AppMsg, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_roundtrip() {
        let msg = AppMsg::Catalog {
            apps: vec![
                AppInfo {
                    id: "desktop".into(),
                    name: "Desktop".into(),
                    kind: app_kind::DESKTOP,
                    art: Vec::new(),
                    running: false,
                },
                AppInfo {
                    id: "elden-ring".into(),
                    name: "Elden Ring".into(),
                    kind: app_kind::GAME,
                    art: vec![1, 2, 3, 4],
                    running: true,
                },
            ],
        };
        assert_eq!(decode_app(&encode_app(&msg).unwrap()).unwrap(), msg);
    }

    #[test]
    fn notice_roundtrip() {
        let msg = AppMsg::Notice {
            message: "Steam was running — restarted it in-stream".into(),
        };
        assert_eq!(decode_app(&encode_app(&msg).unwrap()).unwrap(), msg);
        // Appended last, so the older variants keep their tag indices — a v1
        // Launch still decodes byte-for-byte identically.
        let launch = AppMsg::Launch { id: "x".into() };
        assert_eq!(decode_app(&encode_app(&launch).unwrap()).unwrap(), launch);
    }

    #[test]
    fn launch_roundtrip() {
        let msg = AppMsg::Launch {
            id: "elden-ring".into(),
        };
        assert_eq!(decode_app(&encode_app(&msg).unwrap()).unwrap(), msg);
    }
}
