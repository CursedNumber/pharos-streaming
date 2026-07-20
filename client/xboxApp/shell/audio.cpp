// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// See audio.h. The AudioGraph is created on a worker thread (CreateAsync +
// .get() would deadlock the STA UI thread), so init() returns immediately and
// playback starts once the graph comes up; packets submitted before then are
// decoded into the jitter buffer and play from the first quantum.

#include "audio.h"

#ifdef TRIREME_FFMPEG

#include <windows.h>

#include <memorybuffer.h>  // IMemoryBufferByteAccess
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Media.Audio.h>
#include <winrt/Windows.Media.MediaProperties.h>
#include <winrt/Windows.Media.Render.h>
#include <winrt/Windows.Media.h>

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
}

#include <algorithm>
#include <atomic>
#include <deque>
#include <mutex>
#include <string>
#include <thread>

using namespace winrt;
using namespace winrt::Windows::Media;
using namespace winrt::Windows::Media::Audio;
using namespace winrt::Windows::Media::MediaProperties;
using namespace winrt::Windows::Media::Render;

void shellLog(const std::string& s);  // main.cpp

namespace {
constexpr uint32_t kRate = 48000;
constexpr uint32_t kChannels = 2;
// Jitter buffer, in interleaved sample-frames: prime to ~60 ms before playing,
// drop oldest past ~120 ms (host paces at 10 ms/packet; cf. the Android
// client's bounded adaptive queue).
constexpr size_t kPrimeFrames = kRate * 60 / 1000;
constexpr size_t kMaxFrames = kRate * 120 / 1000;
}  // namespace

struct AudioPipeline::Impl {
    // Decoder (QUIC thread only, but guarded for shutdown).
    std::mutex decMx;
    AVCodecContext* avctx = nullptr;
    AVFrame* frame = nullptr;

    // Jitter buffer of interleaved f32 stereo frames.
    std::mutex qMx;
    std::deque<float> queue;  // interleaved samples (frames * kChannels)
    bool primed = false;

    // Graph (owned by the worker thread's apartment; torn down in shutdown).
    AudioGraph graph{nullptr};
    AudioFrameInputNode inNode{nullptr};
    AudioDeviceOutputNode outNode{nullptr};
    std::thread initTh;
    std::atomic<bool> graphUp{false};
    std::atomic<bool> stopping{false};

    std::atomic<uint64_t> nPkts{0}, nUnderruns{0};

    bool openDecoder();
    void buildGraph();
    void onQuantum(AudioFrameInputNode const& node, int32_t required);
};

bool AudioPipeline::Impl::openDecoder() {
    const AVCodec* codec = avcodec_find_decoder(AV_CODEC_ID_OPUS);
    if (!codec) { shellLog("audio: no Opus decoder in libavcodec"); return false; }
    avctx = avcodec_alloc_context3(codec);
    avctx->sample_rate = kRate;
    av_channel_layout_default(&avctx->ch_layout, kChannels);
    if (avcodec_open2(avctx, codec, nullptr) < 0) {
        shellLog("audio: avcodec_open2(opus) failed");
        return false;
    }
    frame = av_frame_alloc();
    return true;
}

void AudioPipeline::Impl::buildGraph() {
    try {
        AudioGraphSettings settings(AudioRenderCategory::Media);
        settings.QuantumSizeSelectionMode(QuantumSizeSelectionMode::LowestLatency);
        auto gr = AudioGraph::CreateAsync(settings).get();
        if (gr.Status() != AudioGraphCreationStatus::Success) {
            shellLog("audio: AudioGraph create failed status=" +
                     std::to_string(static_cast<int>(gr.Status())));
            return;
        }
        graph = gr.Graph();
        auto onr = graph.CreateDeviceOutputNodeAsync().get();
        if (onr.Status() != AudioDeviceNodeCreationStatus::Success) {
            shellLog("audio: output node failed status=" +
                     std::to_string(static_cast<int>(onr.Status())));
            return;
        }
        outNode = onr.DeviceOutputNode();

        auto props = AudioEncodingProperties::CreatePcm(kRate, kChannels, 32);
        props.Subtype(L"Float");
        inNode = graph.CreateFrameInputNode(props);
        inNode.AddOutgoingConnection(outNode);
        inNode.QuantumStarted([this](AudioFrameInputNode const& n,
                                     FrameInputNodeQuantumStartedEventArgs const& a) {
            // An exception escaping this handler = std::terminate on the audio
            // graph thread; log instead.
            try {
                if (!stopping.load(std::memory_order_relaxed)) onQuantum(n, a.RequiredSamples());
            } catch (hresult_error const& e) {
                shellLog("audio: quantum threw 0x" +
                         std::to_string(static_cast<uint32_t>(e.code())));
            } catch (...) {
                shellLog("audio: quantum threw (unknown)");
            }
        });
        graph.Start();
        graphUp = true;
        shellLog("audio: graph up (48k stereo f32, lowest-latency quantum)");
    } catch (hresult_error const& e) {
        shellLog("audio: graph init threw 0x" + std::to_string(static_cast<uint32_t>(e.code())));
    }
}

void AudioPipeline::Impl::onQuantum(AudioFrameInputNode const& node, int32_t required) {
    if (required <= 0) return;
    std::lock_guard<std::mutex> lk(qMx);
    const size_t have = queue.size() / kChannels;
    if (!primed) {
        if (have < kPrimeFrames) return;  // silence until the jitter buffer fills
        primed = true;
    }
    if (have == 0) {
        // Underrun: fall back to priming so we rebuild margin instead of
        // stuttering one quantum at a time.
        primed = false;
        nUnderruns.fetch_add(1, std::memory_order_relaxed);
        return;
    }
    const size_t frames = std::min<size_t>(static_cast<size_t>(required), have);
    const uint32_t bytes = static_cast<uint32_t>(frames * kChannels * sizeof(float));

    AudioFrame af(bytes);
    {
        auto buf = af.LockBuffer(AudioBufferAccessMode::Write);
        auto ref = buf.CreateReference();
        uint8_t* dst = nullptr;
        uint32_t cap = 0;
        check_hresult(ref.as<::Windows::Foundation::IMemoryBufferByteAccess>()->GetBuffer(&dst, &cap));
        float* out = reinterpret_cast<float*>(dst);
        for (size_t i = 0; i < frames * kChannels; ++i) {
            out[i] = queue.front();
            queue.pop_front();
        }
    }
    node.AddFrame(af);
}

// --- public -----------------------------------------------------------------

bool AudioPipeline::init() {
    p_ = new Impl();
    if (!p_->openDecoder()) { delete p_; p_ = nullptr; return false; }
    p_->initTh = std::thread([impl = p_] {
        init_apartment();  // worker MTA for the async graph creation
        impl->buildGraph();
    });
    return true;
}

void AudioPipeline::submit(const uint8_t* data, size_t len) {
    if (!p_ || p_->stopping.load(std::memory_order_relaxed)) return;
    p_->nPkts.fetch_add(1, std::memory_order_relaxed);

    std::lock_guard<std::mutex> dlk(p_->decMx);
    if (!p_->avctx) return;
    AVPacket pkt{};
    pkt.data = const_cast<uint8_t*>(data);
    pkt.size = static_cast<int>(len);
    if (avcodec_send_packet(p_->avctx, &pkt) != 0) return;
    while (avcodec_receive_frame(p_->avctx, p_->frame) == 0) {
        AVFrame* f = p_->frame;
        const int n = f->nb_samples;
        const int ch = f->ch_layout.nb_channels;
        std::lock_guard<std::mutex> qlk(p_->qMx);
        if (f->format == AV_SAMPLE_FMT_FLTP && ch >= 1) {
            const float* l = reinterpret_cast<const float*>(f->data[0]);
            const float* r = ch >= 2 ? reinterpret_cast<const float*>(f->data[1]) : l;
            for (int i = 0; i < n; ++i) {
                p_->queue.push_back(l[i]);
                p_->queue.push_back(r[i]);
            }
        } else if (f->format == AV_SAMPLE_FMT_FLT && ch == 2) {
            const float* s = reinterpret_cast<const float*>(f->data[0]);
            p_->queue.insert(p_->queue.end(), s, s + static_cast<size_t>(n) * 2);
        } else {
            continue;  // unexpected layout; skip rather than play noise
        }
        // Drop-oldest past the cap: fresher audio beats accumulating lag.
        while (p_->queue.size() > kMaxFrames * kChannels) p_->queue.pop_front();
        av_frame_unref(f);
    }
}

void AudioPipeline::shutdown() {
    if (!p_) return;
    p_->stopping = true;
    if (p_->initTh.joinable()) p_->initTh.join();
    if (p_->graphUp) {
        try {
            p_->graph.Stop();
            p_->graph.Close();
        } catch (...) {}
    }
    {
        std::lock_guard<std::mutex> lk(p_->decMx);
        if (p_->frame) av_frame_free(&p_->frame);
        if (p_->avctx) avcodec_free_context(&p_->avctx);
    }
    delete p_;
    p_ = nullptr;
}

uint64_t AudioPipeline::packets() const { return p_ ? p_->nPkts.load() : 0; }
uint64_t AudioPipeline::underruns() const { return p_ ? p_->nUnderruns.load() : 0; }

#else  // !TRIREME_FFMPEG — no Opus decoder without FFmpeg; stub out.

bool AudioPipeline::init() { return false; }
void AudioPipeline::submit(const uint8_t*, size_t) {}
void AudioPipeline::shutdown() {}
uint64_t AudioPipeline::packets() const { return 0; }
uint64_t AudioPipeline::underruns() const { return 0; }

#endif  // TRIREME_FFMPEG
