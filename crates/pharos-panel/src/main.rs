// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Pharos Control — the host-side control panel.
//!
//! A cross-platform egui/eframe desktop app for managing a Pharos streaming
//! host: paired clients, the encode/decode pipeline, the virtual-display
//! layout, input forwarding, controller tuning, and MQTT/Wake-on-LAN. It mirrors
//! the Trireme client's look (system theme, card tiles, status badges) and the
//! same on-disk config files the host reads, so panel edits and the host agree.

// This crate is a scaffold: `model`/`data`/`theme`/`widgets` export the frozen
// contract the six tab modules build on. Until every tab is filled in, much of
// that surface is legitimately unused — silence the noise so real warnings
// stand out. (Tab agents may keep or drop this; it is not load-bearing.)
#![allow(dead_code)]

// Internationalisation: load every locale file under `locales/`, defaulting and
// falling back to English. Must be at the crate root for the `t!` macro to
// resolve. See `i18n.rs` for the wrappers + the per-module locale-file layout.
rust_i18n::i18n!("locales", fallback = "en");

mod a11y;
mod app;
mod control;
mod data;
mod fonts;
mod i18n;
mod model;
mod tabs;
mod theme;
mod widgets;

fn main() -> anyhow::Result<()> {
    // Env-filterable logging (RUST_LOG wins). The default is `info`, but wgpu is
    // muted: `wgpu_core` logs `Device::maintain: waiting for submission index …`
    // at INFO on *every* repaint, and writing that to the terminal synchronously
    // throttles the whole UI thread (worse in release, which runs more frames).
    // `wgpu_hal` is dropped to ERROR to also hide the benign "Unrecognized present
    // mode" WARN a newer Vulkan driver triggers. Pass RUST_LOG to override.
    const DEFAULT_LOG: &str = "info,wgpu_core=warn,wgpu_hal=error,naga=warn";
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG)),
        )
        .init();

    app::run()
}
