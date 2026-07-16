// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Media Foundation HEVC decode of Trireme access units into D3D11 NV12
// textures, presented to the swapchain backbuffer via ID3D11VideoProcessor
// (which handles NV12->RGB color conversion and scaling). submit() runs on the
// core's QUIC callback thread; blitLatest() on the render thread. The decoder
// MFT is driven with the async event model (Xbox's HEVC decoder is a hardware
// async MFT).

#pragma once

#include <d3d11.h>

#include <cstddef>
#include <cstdint>

class VideoPipeline {
public:
    bool init(ID3D11Device* device, ID3D11DeviceContext* context);
    // One compressed access unit (HEVC Annex-B), from the QUIC thread.
    void submit(const uint8_t* data, size_t len);
    // Blit the most recently decoded frame onto the backbuffer. Returns false
    // if no frame has decoded yet (caller should clear instead).
    bool blitLatest(ID3D11Texture2D* backbuffer, uint32_t bw, uint32_t bh);
    void shutdown();

    uint64_t submitted() const;
    uint64_t decoded() const;
    uint32_t frameWidth() const;
    uint32_t frameHeight() const;

private:
    struct Impl;
    Impl* p_ = nullptr;
};
