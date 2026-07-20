// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Opus audio playback: libavcodec's native Opus decoder (48 kHz stereo, float
// planar) feeding a WinRT AudioGraph frame-input node in pull mode. submit()
// runs on the core's QUIC callback thread; the graph's QuantumStarted handler
// drains a bounded jitter buffer on the audio thread. Only available in the
// TRIREME_FFMPEG build (Media Foundation has no Opus decoder); the fallback
// build gets a stub whose init() returns false.

#pragma once

#include <cstddef>
#include <cstdint>

class AudioPipeline {
public:
    bool init();
    // One Opus packet (48 kHz stereo), from the QUIC thread.
    void submit(const uint8_t* data, size_t len);
    void shutdown();

    uint64_t packets() const;
    uint64_t underruns() const;

private:
    struct Impl;
    Impl* p_ = nullptr;
};
