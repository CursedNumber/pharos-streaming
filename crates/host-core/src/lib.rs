// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Platform-agnostic host-side glue for Pharos.
//!
//! Whatever the display backend (Linux Smithay compositor, Windows IddCx host),
//! the *pairing handshake*, *long-term identity*, and *config directory* are
//! identical — one PAKE implementation, one identity file, one peer registry.
//! They live here so both hosts depend on the same code instead of forking it.
//!
//! See [`pair`] for the listener; the crypto + wire shape live in `core-crypto`
//! and `transport::pair_proto` respectively.

pub mod config;
pub mod controller_profiles;
pub mod pair;
