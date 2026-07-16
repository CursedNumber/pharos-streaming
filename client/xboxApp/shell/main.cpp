// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Trireme Xbox client shell. Layer 2: the D3D11 swapchain (layer 1) plus a
// Media Foundation HEVC decode + present path (video.cpp). It connects to a
// Pharos host (config from LocalState\stream.txt for now; the D2D UI replaces
// that in layer 3), feeds access units to the decoder, and blits decoded frames
// to the swapchain. Debug lines land in LocalState\shell.log.

#include <windows.h>

#include <d3d11.h>
#include <d3d11_4.h>  // ID3D11Multithread
#include <dxgi1_3.h>
#include <winrt/Windows.ApplicationModel.Core.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Gaming.Input.h>
#include <winrt/Windows.Graphics.Display.h>
#include <winrt/Windows.Storage.h>
#include <winrt/Windows.UI.Core.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <filesystem>
#include <fstream>
#include <map>
#include <mutex>
#include <string>
#include <thread>

#include "trireme_xbox.h"
#include "video.h"

using namespace winrt;
using namespace winrt::Windows::ApplicationModel::Core;
using namespace winrt::Windows::UI::Core;
using namespace winrt::Windows::Gaming::Input;

namespace {
std::mutex g_logMx;
std::ofstream g_log;
}  // namespace

// Non-static: video.cpp logs through this.
void shellLog(const std::string& msg) {
    std::lock_guard<std::mutex> lk(g_logMx);
    if (g_log.is_open()) { g_log << msg << "\n"; g_log.flush(); }
    OutputDebugStringA((msg + "\n").c_str());
}

namespace {

void check(HRESULT hr, const char* what) {
    if (FAILED(hr)) {
        char b[16];
        snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(hr));
        shellLog(std::string("FATAL ") + what + " " + b);
        throw hresult_error(hr);
    }
}

// --- config (LocalState\stream.txt, key=value) -----------------------------

std::map<std::string, std::string> readConfig(const std::filesystem::path& p) {
    std::map<std::string, std::string> cfg;
    std::ifstream f(p);
    std::string line;
    while (std::getline(f, line)) {
        while (!line.empty() && (line.back() == '\r' || line.back() == '\n')) line.pop_back();
        auto eq = line.find('=');
        if (eq == std::string::npos || line.empty() || line[0] == '#') continue;
        cfg[line.substr(0, eq)] = line.substr(eq + 1);
    }
    return cfg;
}
std::string cfgOr(const std::map<std::string, std::string>& c, const char* k, const std::string& d) {
    auto it = c.find(k);
    return it == c.end() ? d : it->second;
}

// --- D3D11 device + swapchain (video-capable) -------------------------------

struct Graphics {
    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;
    com_ptr<IDXGISwapChain1> swap;
    uint32_t width = 0, height = 0;

    void create(CoreWindow const& window, float dpi) {
        UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        D3D_FEATURE_LEVEL want[] = {D3D_FEATURE_LEVEL_11_0};
        check(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags, want, 1,
                                D3D11_SDK_VERSION, device.put(), nullptr, context.put()),
              "D3D11CreateDevice");
        // MF's decode thread and our render thread both touch the immediate
        // context; make it thread-safe.
        if (auto mt = context.try_as<ID3D11Multithread>()) mt->SetMultithreadProtected(TRUE);

        auto b = window.Bounds();
        width = std::max<uint32_t>(1, static_cast<uint32_t>(b.Width * dpi / 96.f));
        height = std::max<uint32_t>(1, static_cast<uint32_t>(b.Height * dpi / 96.f));

        DXGI_SWAP_CHAIN_DESC1 sd{};
        sd.Width = width; sd.Height = height;
        sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        sd.SampleDesc.Count = 1;
        sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        sd.BufferCount = 2;
        sd.Scaling = DXGI_SCALING_STRETCH;
        sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
        sd.AlphaMode = DXGI_ALPHA_MODE_IGNORE;

        auto dxgiDevice = device.as<IDXGIDevice>();
        com_ptr<IDXGIAdapter> adapter;
        check(dxgiDevice->GetAdapter(adapter.put()), "GetAdapter");
        com_ptr<IDXGIFactory2> factory;
        check(adapter->GetParent(guid_of<IDXGIFactory2>(), factory.put_void()), "GetParent");
        check(factory->CreateSwapChainForCoreWindow(device.get(), get_unknown(window), &sd, nullptr,
                                                    swap.put()),
              "CreateSwapChainForCoreWindow");
        shellLog("graphics: " + std::to_string(width) + "x" + std::to_string(height) +
                 " (video-capable)");
    }

    com_ptr<ID3D11Texture2D> backbuffer() {
        com_ptr<ID3D11Texture2D> b;
        swap->GetBuffer(0, guid_of<ID3D11Texture2D>(), b.put_void());
        return b;
    }
    void clear(ID3D11Texture2D* back, float r, float g, float b) {
        com_ptr<ID3D11RenderTargetView> rtv;
        device->CreateRenderTargetView(back, nullptr, rtv.put());
        float c[4] = {r, g, b, 1.f};
        context->ClearRenderTargetView(rtv.get(), c);
    }
    void present() { swap->Present(1, 0); }
};

// --- streaming --------------------------------------------------------------

VideoPipeline g_video;
std::atomic<TriremeStream*> g_stream{nullptr};
std::atomic<bool> g_degraded{false};
std::atomic<bool> g_ended{false};

void onFrame(void* ctx, const uint8_t* data, uintptr_t len) {
    static_cast<VideoPipeline*>(ctx)->submit(data, static_cast<size_t>(len));
}
void onEnd(void*, const char* err) {
    shellLog(std::string("stream ended: ") + (err ? err : "(clean)"));
    g_ended = true;
}
void onLinkHealth(void*, bool degraded) { g_degraded = degraded; }

void connectWorker(std::filesystem::path localState) {
    // The config is pushed after launch, so wait for it rather than racing.
    std::map<std::string, std::string> cfg;
    for (int i = 0; i < 120; ++i) {
        cfg = readConfig(localState / "stream.txt");
        if (cfg.count("host")) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
    }
    if (cfg.find("host") == cfg.end()) {
        shellLog("no stream.txt (host=) after 60s; idle on nav background");
        return;
    }
    const std::string host = cfg["host"];
    const std::string pin = cfgOr(cfg, "pin", "");
    const std::string pairAddr = host + ":" + cfgOr(cfg, "pairport", "4432");
    const std::string streamAddr = host + ":" + cfgOr(cfg, "streamport", "15777");
    const uint32_t w = std::stoul(cfgOr(cfg, "width", "0"));
    const uint32_t h = std::stoul(cfgOr(cfg, "height", "0"));
    const uint32_t refresh = std::stoul(cfgOr(cfg, "refresh_mhz", "60000"));
    const std::string idPath = (localState / "identity.toml").string();
    std::string pubkey = cfgOr(cfg, "pubkey", "");

    trireme_init_logging("info");
    shellLog("connect host=" + host + " core proto=" +
             std::to_string(trireme_protocol_version()));

    if (!pin.empty()) {
        shellLog("pairing PIN " + pin + " ...");
        char out[65] = {0};
        int rc = trireme_pair(pairAddr.c_str(), pin.c_str(), idPath.c_str(), out, sizeof(out));
        if (rc != 0) { shellLog("pair failed rc=" + std::to_string(rc)); return; }
        pubkey = out;
        shellLog("paired OK");
    }
    TriremeStream* s = trireme_start(streamAddr.c_str(), pubkey.empty() ? nullptr : pubkey.c_str(),
                                     idPath.c_str(), w, h, refresh, &g_video, onFrame, onEnd,
                                     onLinkHealth, nullptr, nullptr);
    if (!s) { shellLog("trireme_start returned null"); return; }
    g_stream = s;
    shellLog("stream started");
}

// --- app --------------------------------------------------------------------

struct App : implements<App, IFrameworkViewSource, IFrameworkView> {
    Graphics gfx;
    CoreWindow window{nullptr};
    std::atomic<bool> quit{false};
    uint64_t lastLoggedDecoded = 0;

    IFrameworkView CreateView() { return *this; }
    void Initialize(CoreApplicationView const&) {}
    void Load(hstring const&) {}
    void Uninitialize() {}
    void SetWindow(CoreWindow const& w) {
        window = w;
        w.Closed([this](auto&&, auto&&) { quit = true; });
    }

    void pollInput() {
        auto pads = Gamepad::Gamepads();
        if (pads.Size() == 0) return;
        auto r = pads.GetAt(0).GetCurrentReading();
        if ((r.Buttons & GamepadButtons::Menu) == GamepadButtons::Menu) quit = true;
    }

    void Run() {
        window.Activate();
        float dpi =
            winrt::Windows::Graphics::Display::DisplayInformation::GetForCurrentView().LogicalDpi();
        gfx.create(window, dpi);
        shellLog("=== Trireme shell (layer 2: video) up ===");

        if (!g_video.init(gfx.device.get(), gfx.context.get()))
            shellLog("video pipeline init FAILED — staying on nav background");

        std::filesystem::path ls{std::wstring{
            winrt::Windows::Storage::ApplicationData::Current().LocalFolder().Path()}};
        std::thread(connectWorker, ls).detach();

        auto disp = window.Dispatcher();
        while (!quit) {
            disp.ProcessEvents(CoreProcessEventsOption::ProcessAllIfPresent);
            pollInput();

            auto back = gfx.backbuffer();
            if (!g_video.blitLatest(back.get(), gfx.width, gfx.height))
                gfx.clear(back.get(), 0.106f, 0.165f, 0.255f);  // nautical navy while idle
            gfx.present();

            uint64_t d = g_video.decoded();
            if (d >= lastLoggedDecoded + 60) {
                shellLog("decoded=" + std::to_string(d) + " submitted=" +
                         std::to_string(g_video.submitted()) + " " +
                         std::to_string(g_video.frameWidth()) + "x" +
                         std::to_string(g_video.frameHeight()));
                lastLoggedDecoded = d;
            }
        }
        if (auto s = g_stream.exchange(nullptr)) trireme_stop(s);
        g_video.shutdown();
        shellLog("shell exit");
    }
};

}  // namespace

int __stdcall wWinMain(void*, void*, wchar_t*, int) {
    init_apartment();
    try {
        std::filesystem::path ls{std::wstring{
            winrt::Windows::Storage::ApplicationData::Current().LocalFolder().Path()}};
        std::lock_guard<std::mutex> lk(g_logMx);
        g_log.open(ls / "shell.log", std::ios::trunc);
    } catch (...) {
    }
    CoreApplication::Run(make<App>());
    return 0;
}
