// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Media Foundation HEVC decode. The Xbox AppContainer exposes the *software*
// "Microsoft H265 Video Decoder MFT" (sync, system-memory NV12 output), not a
// hardware async MFT, so this drives it synchronously (ProcessInput/
// ProcessOutput) and uploads the decoded NV12 to a D3D texture, which
// ID3D11VideoProcessor blits (NV12->RGB + scale) to the swapchain backbuffer.

#include "video.h"

#include <windows.h>

#include <codecapi.h>
#include <d3d11.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfobjects.h>
#include <mftransform.h>
#include <winrt/base.h>

#include <atomic>
#include <cctype>
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
std::string hexstr(HRESULT hr) {
    char b[16];
    snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(hr));
    return b;
}
bool ok(HRESULT hr, const char* what) {
    if (FAILED(hr)) { shellLog(std::string("video: ") + what + " " + hexstr(hr)); return false; }
    return true;
}
}  // namespace

struct VideoPipeline::Impl {
    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;

    com_ptr<IMFTransform> mft;
    DWORD inId = 0, outId = 0;
    bool providesSamples = false;
    DWORD outBufSize = 0;

    com_ptr<ID3D11VideoDevice> vdev;
    com_ptr<ID3D11VideoContext> vctx;
    com_ptr<ID3D11VideoProcessor> vproc;
    com_ptr<ID3D11VideoProcessorEnumerator> venum;

    com_ptr<ID3D11Texture2D> nv12;       // DEFAULT, video-processor input
    com_ptr<ID3D11Texture2D> nv12stage;  // STAGING, CPU upload
    com_ptr<ID3D11VideoProcessorInputView> nv12View;
    std::mutex frameMx;
    uint32_t fw = 0, fh = 0;

    std::mutex qMx;
    std::condition_variable qCv;
    std::deque<std::vector<uint8_t>> queue;

    std::thread th;
    std::atomic<bool> running{false};
    std::atomic<uint64_t> nSub{0}, nDec{0};
    LONGLONG pts = 0;

    bool start();
    bool negotiateOutput();
    void ensureNv12(uint32_t w, uint32_t h);
    bool ensureVproc(uint32_t ow, uint32_t oh);
    void run();
    bool feedOne();
    int drainOnce();
    void uploadNV12(IMFSample* s);
};

bool VideoPipeline::Impl::start() {
    if (!ok(MFStartup(MF_VERSION, MFSTARTUP_FULL), "MFStartup")) return false;

    MFT_REGISTER_TYPE_INFO inInfo{MFMediaType_Video, MFVideoFormat_HEVC};
    IMFActivate** acts = nullptr;
    UINT32 count = 0;
    auto freeActs = [&] {
        if (acts) { for (UINT32 i = 0; i < count; ++i) if (acts[i]) acts[i]->Release(); CoTaskMemFree(acts); }
        acts = nullptr; count = 0;
    };
    auto nameOf = [](IMFActivate* a) -> std::string {
        WCHAR* n = nullptr; UINT32 l = 0;
        if (SUCCEEDED(a->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute, &n, &l))) {
            std::wstring w(n); CoTaskMemFree(n); return std::string(w.begin(), w.end());
        }
        return "?";
    };
    int chosen = -1;
    const UINT32 tries[] = {MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER};
    for (UINT32 fl : tries) {
        freeActs();
        MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, fl, &inInfo, nullptr, &acts, &count);
        shellLog("video: HEVC enum flags=" + hexstr(fl) + " -> " + std::to_string(count));
        if (count > 0) { chosen = 0; break; }
    }
    if (chosen < 0) {
        freeActs();
        MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_ALL, nullptr, nullptr, &acts, &count);
        for (UINT32 i = 0; i < count; ++i) {
            std::string nm = nameOf(acts[i]);
            std::string up = nm;
            for (auto& c : up) c = static_cast<char>(toupper(static_cast<unsigned char>(c)));
            if (chosen < 0 && (up.find("265") != std::string::npos || up.find("HEVC") != std::string::npos))
                chosen = static_cast<int>(i);
        }
    }
    if (chosen < 0) { shellLog("video: no HEVC decoder"); freeActs(); return false; }
    shellLog("video: using MFT '" + nameOf(acts[chosen]) + "'");
    HRESULT hr = acts[chosen]->ActivateObject(__uuidof(IMFTransform), mft.put_void());
    freeActs();
    if (!ok(hr, "ActivateObject")) return false;

    DWORD nIn = 1, nOut = 1;
    mft->GetStreamCount(&nIn, &nOut);
    DWORD inIds[4] = {0}, outIds[4] = {0};
    if (SUCCEEDED(mft->GetStreamIDs(nIn, inIds, nOut, outIds))) { inId = inIds[0]; outId = outIds[0]; }

    com_ptr<IMFMediaType> inType;
    MFCreateMediaType(inType.put());
    inType->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    inType->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_HEVC);
    inType->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    MFSetAttributeSize(inType.get(), MF_MT_FRAME_SIZE, 1920, 1080);
    MFSetAttributeRatio(inType.get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
    if (!ok(mft->SetInputType(inId, inType.get(), 0), "SetInputType")) return false;

    if (!negotiateOutput()) return false;

    MFT_OUTPUT_STREAM_INFO si{};
    mft->GetOutputStreamInfo(outId, &si);
    providesSamples =
        (si.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
    outBufSize = si.cbSize;
    shellLog("video: providesSamples=" + std::to_string(providesSamples) + " outBuf=" +
             std::to_string(outBufSize));

    mft->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    mft->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    shellLog("video: HEVC decoder ready (sync)");
    return true;
}

bool VideoPipeline::Impl::negotiateOutput() {
    for (DWORD i = 0;; ++i) {
        com_ptr<IMFMediaType> t;
        HRESULT hr = mft->GetOutputAvailableType(outId, i, t.put());
        if (hr == MF_E_NO_MORE_TYPES) break;
        if (FAILED(hr)) return false;
        GUID sub{};
        t->GetGUID(MF_MT_SUBTYPE, &sub);
        if (sub == MFVideoFormat_NV12) {
            if (!ok(mft->SetOutputType(outId, t.get(), 0), "SetOutputType")) return false;
            UINT32 w = 0, h = 0;
            MFGetAttributeSize(t.get(), MF_MT_FRAME_SIZE, &w, &h);
            if (w && h) ensureNv12(w, h);
            // Re-query on every (re)negotiation: a stream change to 1088 grows
            // the required output buffer, and a too-small buffer is MF_E_BUFFERTOOSMALL.
            MFT_OUTPUT_STREAM_INFO si{};
            mft->GetOutputStreamInfo(outId, &si);
            providesSamples =
                (si.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
            outBufSize = si.cbSize;
            shellLog("video: output NV12 " + std::to_string(w) + "x" + std::to_string(h) +
                     " buf=" + std::to_string(outBufSize));
            return true;
        }
    }
    shellLog("video: no NV12 output type");
    return false;
}

void VideoPipeline::Impl::ensureNv12(uint32_t w, uint32_t h) {
    std::lock_guard<std::mutex> lk(frameMx);
    if (nv12 && fw == w && fh == h) return;
    fw = w; fh = h;
    nv12 = nullptr; nv12stage = nullptr; nv12View = nullptr;
    vproc = nullptr; venum = nullptr;
    D3D11_TEXTURE2D_DESC d{};
    d.Width = w; d.Height = h; d.MipLevels = 1; d.ArraySize = 1;
    d.Format = DXGI_FORMAT_NV12; d.SampleDesc.Count = 1;
    d.Usage = D3D11_USAGE_DEFAULT; d.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    ok(device->CreateTexture2D(&d, nullptr, nv12.put()), "CreateTexture2D nv12");
    D3D11_TEXTURE2D_DESC s = d;
    s.Usage = D3D11_USAGE_STAGING; s.BindFlags = 0; s.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
    ok(device->CreateTexture2D(&s, nullptr, nv12stage.put()), "CreateTexture2D nv12stage");
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

void VideoPipeline::Impl::uploadNV12(IMFSample* s) {
    com_ptr<IMFMediaBuffer> buf;
    if (FAILED(s->GetBufferByIndex(0, buf.put()))) return;
    BYTE* src = nullptr;
    LONG pitch = static_cast<LONG>(fw);
    auto b2 = buf.try_as<IMF2DBuffer>();
    if (b2) {
        if (FAILED(b2->Lock2D(&src, &pitch))) return;
    } else {
        DWORD len = 0;
        if (FAILED(buf->Lock(&src, nullptr, &len))) return;
    }
    {
        std::lock_guard<std::mutex> lk(frameMx);
        if (nv12stage && nv12) {
            D3D11_MAPPED_SUBRESOURCE m{};
            if (SUCCEEDED(context->Map(nv12stage.get(), 0, D3D11_MAP_WRITE, 0, &m))) {
                BYTE* dst = static_cast<BYTE*>(m.pData);
                for (uint32_t r = 0; r < fh; ++r)
                    memcpy(dst + r * m.RowPitch, src + r * pitch, fw);
                BYTE* srcUV = src + static_cast<size_t>(pitch) * fh;
                BYTE* dstUV = dst + static_cast<size_t>(m.RowPitch) * fh;
                for (uint32_t r = 0; r < fh / 2; ++r)
                    memcpy(dstUV + r * m.RowPitch, srcUV + r * pitch, fw);
                context->Unmap(nv12stage.get(), 0);
                context->CopyResource(nv12.get(), nv12stage.get());
            }
        }
    }
    if (b2) b2->Unlock2D(); else buf->Unlock();
    nDec.fetch_add(1, std::memory_order_relaxed);
}

int VideoPipeline::Impl::drainOnce() {
    MFT_OUTPUT_DATA_BUFFER out{};
    out.dwStreamID = outId;
    com_ptr<IMFSample> alloc;
    if (!providesSamples) {
        MFCreateSample(alloc.put());
        com_ptr<IMFMediaBuffer> mb;
        DWORD need = fw * fh * 3 / 2 + 4096;
        MFCreateMemoryBuffer(outBufSize > need ? outBufSize : need, mb.put());
        alloc->AddBuffer(mb.get());
        out.pSample = alloc.get();
    }
    DWORD status = 0;
    HRESULT hr = mft->ProcessOutput(0, 1, &out, &status);
    if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) return 0;
    if (hr == MF_E_TRANSFORM_STREAM_CHANGE) { negotiateOutput(); return 1; }
    if (FAILED(hr)) { shellLog("video: ProcessOutput " + hexstr(hr)); return -1; }
    IMFSample* s = out.pSample ? out.pSample : alloc.get();
    if (s) uploadNV12(s);
    if (providesSamples && out.pSample) out.pSample->Release();
    return 1;
}

bool VideoPipeline::Impl::feedOne() {
    std::vector<uint8_t> au;
    { std::unique_lock<std::mutex> lk(qMx);
      if (queue.empty()) return false;
      au = std::move(queue.front()); queue.pop_front(); }
    com_ptr<IMFMediaBuffer> buf;
    if (!ok(MFCreateMemoryBuffer(static_cast<DWORD>(au.size()), buf.put()), "MFCreateMemoryBuffer"))
        return true;
    BYTE* dst = nullptr; DWORD maxLen = 0;
    if (SUCCEEDED(buf->Lock(&dst, &maxLen, nullptr))) {
        memcpy(dst, au.data(), au.size());
        buf->Unlock();
        buf->SetCurrentLength(static_cast<DWORD>(au.size()));
    }
    com_ptr<IMFSample> s;
    MFCreateSample(s.put());
    s->AddBuffer(buf.get());
    s->SetSampleTime(pts);
    s->SetSampleDuration(166667);
    pts += 166667;
    HRESULT hr = mft->ProcessInput(inId, s.get(), 0);
    if (hr == MF_E_NOTACCEPTING) {  // drain first, then this AU goes next round
        std::unique_lock<std::mutex> lk(qMx);
        queue.push_front(std::move(au));
        return false;
    }
    return true;
}

void VideoPipeline::Impl::run() {
    while (running.load()) {
        while (drainOnce() == 1) {}      // pull all ready frames
        if (!feedOne()) {                // then feed; if nothing to feed, wait
            std::unique_lock<std::mutex> lk(qMx);
            qCv.wait_for(lk, std::chrono::milliseconds(8), [&] { return !queue.empty() || !running; });
        }
    }
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
    if (!p_->nv12 || !p_->ensureVproc(bw, bh)) return false;
    if (!p_->nv12View) {
        D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC iv{};
        iv.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
        if (!ok(p_->vdev->CreateVideoProcessorInputView(p_->nv12.get(), p_->venum.get(), &iv,
                                                        p_->nv12View.put()),
                "CreateVPInputView"))
            return false;
    }
    com_ptr<ID3D11VideoProcessorOutputView> ov;
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC od{};
    od.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
    if (!ok(p_->vdev->CreateVideoProcessorOutputView(backbuffer, p_->venum.get(), &od, ov.put()),
            "CreateVPOutputView"))
        return false;
    D3D11_VIDEO_PROCESSOR_STREAM st{};
    st.Enable = TRUE;
    st.pInputSurface = p_->nv12View.get();
    return SUCCEEDED(p_->vctx->VideoProcessorBlt(p_->vproc.get(), ov.get(), 0, 1, &st));
}

void VideoPipeline::shutdown() {
    if (!p_) return;
    p_->running = false;
    p_->qCv.notify_all();
    if (p_->th.joinable()) p_->th.join();
    if (p_->mft) {
        p_->mft->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        p_->mft->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    MFShutdown();
    delete p_;
    p_ = nullptr;
}

uint64_t VideoPipeline::submitted() const { return p_ ? p_->nSub.load() : 0; }
uint64_t VideoPipeline::decoded() const { return p_ ? p_->nDec.load() : 0; }
uint32_t VideoPipeline::frameWidth() const { return p_ ? p_->fw : 0; }
uint32_t VideoPipeline::frameHeight() const { return p_ ? p_->fh : 0; }
