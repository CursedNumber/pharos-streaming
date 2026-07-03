// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Encode backends (BATTLE_PLAN §7). HEVC/NVENC first; AV1 is cheap on Ada;
//! VAAPI/software behind the same `pal::Encoder` trait. M0 stub.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    NvencHevc,
    NvencAv1,
    Vaapi,
    Software,
}

/// HEVC/NVENC is the default focus (per kickoff).
pub fn default_backend() -> EncoderBackend {
    EncoderBackend::NvencHevc
}
