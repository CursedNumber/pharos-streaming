// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Pairing-window manager — moved to `host_core::pairing` so the Windows host
//! (`pharos-win`) drives the identical GUI pairing flow. Re-exported here so the
//! compositor's existing `crate::pairing::*` call sites keep working unchanged.

pub use host_core::pairing::{PairingManager, DEFAULT_PAIR_ADDR};
