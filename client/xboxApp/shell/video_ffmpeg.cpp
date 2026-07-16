// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Hardware HEVC decode via FFmpeg's D3D11VA hwaccel. The Xbox AppContainer
// doesn't expose a hardware decoder MFT, but FFmpeg talks to ID3D11VideoDevice
// (DXVA) directly, so libavcodec + AV_HWDEVICE_TYPE_D3D11VA gets real GPU decode
// and outputs AV_PIX_FMT_D3D11 frames (NV12 textures) that ID3D11VideoProcessor
// blits to the swapchain. This is the moonlight-xbox-dx approach.
//
// Compiled only when TRIREME_FFMPEG is defined (build-shell.ps1 sets it once the
// vcpkg x64-uwp FFmpeg libs exist); otherwise video.cpp's Media Foundation path
// is used. Same VideoPipeline surface either way.

#ifdef TRIREME_FFMPEG

#include "video.h"

#include <windows.h>

#include <d3d11.h>
#include <winrt/base.h>

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
}

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

using winrt::com_ptr;

void shellLog(const std::string& s);  // main.cpp

namespace {
bool ok(HRESULT hr, const char* what) {
    if (FAILED(hr)) {
        char b[16];
        snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(hr));
        shellLog(std::string("video: ") + what + " " + b);
        return false;
    }
    return true;
}
}  // namespace

struct VideoPipeline::Impl {
    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;

    const AVCodec* codec = nullptr;
    AVCodecContext* avctx = nullptr;
    AVBufferRef* hwDevice = nullptr;

    // Video processor for NV12 -> RGB backbuffer.
    com_ptr<ID3D11VideoDevice> vdev;
    com_ptr<ID3D11VideoContext> vctx;
    com_ptr<ID3D11VideoProcessor> vproc;
    com_ptr<ID3D11VideoProcessorEnumerator> venum;

    // Latest decoded frame: FFmpeg's own D3D11 texture (array) kept alive by the
    // AVFrame ref until the render thread has used it.
    std::mutex frameMx;
    AVFrame* latest = nullptr;
    uint32_t fw = 0, fh = 0;

    std::mutex qMx;
    std::condition_variable qCv;
    std::deque<std::vector<uint8_t>> queue;

    std::thread th;
    std::atomic<bool> running{false};
    std::atomic<uint64_t> nSub{0}, nDec{0};

    bool start();
    bool ensureVproc(uint32_t ow, uint32_t oh);
    void run();
    void onDecoded(AVFrame* f);
};

bool VideoPipeline::Impl::start() {
    codec = avcodec_find_decoder(AV_CODEC_ID_HEVC);
    if (!codec) { shellLog("video: no HEVC decoder in libavcodec"); return false; }

    // D3D11VA hw device wrapping OUR D3D11 device (share the swapchain's device).
    hwDevice = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!hwDevice) { shellLog("video: av_hwdevice_ctx_alloc failed"); return false; }
    auto* dctx = reinterpret_cast<AVHWDeviceContext*>(hwDevice->data);
    auto* d3d = reinterpret_cast<AVD3D11VADeviceContext*>(dctx->hwctx);
    d3d->device = device.get();
    device->AddRef();  // FFmpeg releases it on free
    if (av_hwdevice_ctx_init(hwDevice) < 0) { shellLog("video: av_hwdevice_ctx_init failed"); return false; }

    avctx = avcodec_alloc_context3(codec);
    avctx->hw_device_ctx = av_buffer_ref(hwDevice);
    avctx->get_format = [](AVCodecContext*, const AVPixelFormat* fmts) {
        for (const AVPixelFormat* p = fmts; *p != AV_PIX_FMT_NONE; ++p)
            if (*p == AV_PIX_FMT_D3D11) return AV_PIX_FMT_D3D11;
        return fmts[0];
    };
    avctx->thread_count = 1;
    if (avcodec_open2(avctx, codec, nullptr) < 0) { shellLog("video: avcodec_open2 failed"); return false; }
    shellLog("video: FFmpeg HEVC/D3D11VA decoder ready");
    return true;
}

bool VideoPipeline::Impl::ensureVproc(uint32_t ow, uint32_t oh) {
    if (vproc) return true;
    vdev = device.try_as<ID3D11VideoDevice>();
    vctx = context.try_as<ID3D11VideoContext>();
    if (!vdev || !vctx) { shellLog("video: no ID3D11VideoDevice"); return false; }
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC cd{};
    cd.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    cd.InputWidth = fw; cd.InputHeight = fh;
    cd.OutputWidth = ow; cd.OutputHeight = oh;
    cd.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
    if (!ok(vdev->CreateVideoProcessorEnumerator(&cd, venum.put()), "CreateVPEnum")) return false;
    if (!ok(vdev->CreateVideoProcessor(venum.get(), 0, vproc.put()), "CreateVP")) return false;
    return true;
}

void VideoPipeline::Impl::onDecoded(AVFrame* f) {
    std::lock_guard<std::mutex> lk(frameMx);
    if (static_cast<uint32_t>(f->width) != fw || static_cast<uint32_t>(f->height) != fh) {
        fw = f->width; fh = f->height;
        vproc = nullptr; venum = nullptr;  // re-enumerate for the new size
        shellLog("video: frame " + std::to_string(fw) + "x" + std::to_string(fh));
    }
    if (latest) av_frame_free(&latest);
    latest = av_frame_alloc();
    av_frame_ref(latest, f);  // keep the D3D11 texture alive for the render thread
    nDec.fetch_add(1, std::memory_order_relaxed);
}

void VideoPipeline::Impl::run() {
    AVPacket* pkt = av_packet_alloc();
    AVFrame* frame = av_frame_alloc();
    while (running.load()) {
        std::vector<uint8_t> au;
        { std::unique_lock<std::mutex> lk(qMx);
          qCv.wait_for(lk, std::chrono::milliseconds(8), [&] { return !queue.empty() || !running; });
          if (!queue.empty()) { au = std::move(queue.front()); queue.pop_front(); } }
        if (au.empty()) continue;
        pkt->data = au.data();
        pkt->size = static_cast<int>(au.size());
        if (avcodec_send_packet(avctx, pkt) == 0) {
            while (avcodec_receive_frame(avctx, frame) == 0) {
                onDecoded(frame);
                av_frame_unref(frame);
            }
        }
    }
    av_packet_free(&pkt);
    av_frame_free(&frame);
}

// --- public -----------------------------------------------------------------

bool VideoPipeline::init(ID3D11Device* device, ID3D11DeviceContext* context) {
    p_ = new Impl();
    p_->device.copy_from(device);
    p_->context.copy_from(context);
    if (!p_->start()) return false;
    p_->running = true;
    p_->th = std::thread([this] { p_->run(); });
    return true;
}

void VideoPipeline::submit(const uint8_t* data, size_t len) {
    if (!p_ || !p_->running) return;
    p_->nSub.fetch_add(1, std::memory_order_relaxed);
    { std::lock_guard<std::mutex> lk(p_->qMx);
      if (p_->queue.size() > 8) p_->queue.pop_front();
      p_->queue.emplace_back(data, data + len); }
    p_->qCv.notify_one();
}

bool VideoPipeline::blitLatest(ID3D11Texture2D* backbuffer, uint32_t bw, uint32_t bh) {
    if (!p_ || p_->nDec.load() == 0) return false;
    std::lock_guard<std::mutex> lk(p_->frameMx);
    if (!p_->latest || !p_->ensureVproc(bw, bh)) return false;

    // FFmpeg's D3D11 output frame: data[0] = ID3D11Texture2D array, data[1] = index.
    auto* tex = reinterpret_cast<ID3D11Texture2D*>(p_->latest->data[0]);
    UINT slice = static_cast<UINT>(reinterpret_cast<intptr_t>(p_->latest->data[1]));
    if (!tex) return false;

    com_ptr<ID3D11VideoProcessorInputView> iv;
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivd{};
    ivd.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    ivd.Texture2D.ArraySlice = slice;
    if (!ok(p_->vdev->CreateVideoProcessorInputView(tex, p_->venum.get(), &ivd, iv.put()),
            "CreateVPInputView"))
        return false;
    com_ptr<ID3D11VideoProcessorOutputView> ov;
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovd{};
    ovd.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
    if (!ok(p_->vdev->CreateVideoProcessorOutputView(backbuffer, p_->venum.get(), &ovd, ov.put()),
            "CreateVPOutputView"))
        return false;
    D3D11_VIDEO_PROCESSOR_STREAM st{};
    st.Enable = TRUE;
    st.pInputSurface = iv.get();
    return SUCCEEDED(p_->vctx->VideoProcessorBlt(p_->vproc.get(), ov.get(), 0, 1, &st));
}

void VideoPipeline::shutdown() {
    if (!p_) return;
    p_->running = false;
    p_->qCv.notify_all();
    if (p_->th.joinable()) p_->th.join();
    { std::lock_guard<std::mutex> lk(p_->frameMx); if (p_->latest) av_frame_free(&p_->latest); }
    if (p_->avctx) avcodec_free_context(&p_->avctx);
    if (p_->hwDevice) av_buffer_unref(&p_->hwDevice);
    delete p_;
    p_ = nullptr;
}

uint64_t VideoPipeline::submitted() const { return p_ ? p_->nSub.load() : 0; }
uint64_t VideoPipeline::decoded() const { return p_ ? p_->nDec.load() : 0; }
uint32_t VideoPipeline::frameWidth() const { return p_ ? p_->fw : 0; }
uint32_t VideoPipeline::frameHeight() const { return p_ ? p_->fh : 0; }

#endif  // TRIREME_FFMPEG
