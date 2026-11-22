// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Capture backends (BATTLE_PLAN §6): Linux hand-rolled Smithay compositor
//! output, Windows IddCx + WGC/DXGI. M0 stub — backends land M1 (Linux) / M3 (Win).

/// Which capture backend the host selects at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Linux: hand-rolled Smithay headless compositor (primary path).
    SmithayHeadless,
    /// Linux: mirror an existing KDE/GNOME session via PipeWire portal; scaffolded
    /// behind the compositor crate's `portal-mirror` feature (compositor::portal_mirror).
    PipewirePortal,
    /// Windows: IddCx virtual display + Windows.Graphics.Capture.
    WindowsIddcx,
}

/// The default backend for the current platform.
pub fn default_backend() -> CaptureBackend {
    if cfg!(windows) {
        CaptureBackend::WindowsIddcx
    } else {
        CaptureBackend::SmithayHeadless
    }
}
