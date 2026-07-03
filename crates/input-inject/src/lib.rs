// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Input injection backends (BATTLE_PLAN §7): uinput (Linux) /
//! SendInput + ViGEm (Windows). Implements `pal::InputSink`. M0 stub.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    Uinput,
    WindowsSendInput,
}

pub fn default_backend() -> InputBackend {
    if cfg!(windows) {
        InputBackend::WindowsSendInput
    } else {
        InputBackend::Uinput
    }
}
