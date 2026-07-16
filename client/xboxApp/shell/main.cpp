// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Trireme Xbox client shell. Layer 1: a CoreApplication with a D3D11 swapchain
// on the CoreWindow and a gamepad-driven render loop. This is the first VISIBLE
// increment past the Phase-1 headless gate (client/xboxApp/phase1). Media
// Foundation decode, the D2D UI, and audio layer on from here.
//
// Debug channel: like Phase 1, every log line also lands in
// LocalState\shell.log so a headless console run can be inspected over the
// Device Portal file API (no attached debugger needed).

#include <windows.h>

#include <d3d11.h>
#include <dxgi1_3.h>
#include <winrt/Windows.ApplicationModel.Core.h>
#include <winrt/Windows.Foundation.Collections.h>  // full IVectorView<T>::Size/GetAt
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
#include <mutex>
#include <sstream>
#include <string>

#include "trireme_xbox.h"

using namespace winrt;
using namespace winrt::Windows::ApplicationModel::Core;
using namespace winrt::Windows::UI::Core;
using namespace winrt::Windows::Gaming::Input;
using winrt::Windows::Foundation::Size;

namespace {

std::mutex g_logMx;
std::ofstream g_log;

void logLine(const std::string& msg) {
    std::lock_guard<std::mutex> lk(g_logMx);
    if (g_log.is_open()) { g_log << msg << "\n"; g_log.flush(); }
    OutputDebugStringA((msg + "\n").c_str());
}

void check(HRESULT hr, const char* what) {
    if (FAILED(hr)) {
        logLine(std::string("FATAL ") + what + " hr=0x" + [&] {
            std::ostringstream o; o << std::hex << static_cast<uint32_t>(hr); return o.str();
        }());
        throw hresult_error(hr);
    }
}

// ---------------------------------------------------------------------------
// D3D11 device + DXGI swapchain bound to the CoreWindow. Owns the backbuffer
// render target; the video and UI layers draw into this same swapchain.

struct Graphics {
    com_ptr<ID3D11Device> device;
    com_ptr<ID3D11DeviceContext> context;
    com_ptr<IDXGISwapChain1> swap;
    com_ptr<ID3D11RenderTargetView> rtv;
    uint32_t width = 0, height = 0;

    void create(CoreWindow const& window, float dpi) {
        UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;  // D2D-compatible for the UI layer
        D3D_FEATURE_LEVEL want[] = {D3D_FEATURE_LEVEL_11_0};
        check(D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags, want,
                                1, D3D11_SDK_VERSION, device.put(), nullptr, context.put()),
              "D3D11CreateDevice");

        auto bounds = window.Bounds();
        width = std::max<uint32_t>(1, static_cast<uint32_t>(bounds.Width * dpi / 96.f));
        height = std::max<uint32_t>(1, static_cast<uint32_t>(bounds.Height * dpi / 96.f));

        DXGI_SWAP_CHAIN_DESC1 sd{};
        sd.Width = width;
        sd.Height = height;
        sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        sd.SampleDesc.Count = 1;
        sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        sd.BufferCount = 2;
        sd.Scaling = DXGI_SCALING_STRETCH;
        sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
        sd.AlphaMode = DXGI_ALPHA_MODE_IGNORE;

        com_ptr<IDXGIDevice> dxgiDevice = device.as<IDXGIDevice>();
        com_ptr<IDXGIAdapter> adapter;
        check(dxgiDevice->GetAdapter(adapter.put()), "GetAdapter");
        com_ptr<IDXGIFactory2> factory;
        check(adapter->GetParent(guid_of<IDXGIFactory2>(), factory.put_void()), "GetParent factory");
        check(factory->CreateSwapChainForCoreWindow(device.get(), get_unknown(window), &sd,
                                                    nullptr, swap.put()),
              "CreateSwapChainForCoreWindow");
        makeRtv();
        logLine("graphics: " + std::to_string(width) + "x" + std::to_string(height));
    }

    void makeRtv() {
        com_ptr<ID3D11Texture2D> back;
        check(swap->GetBuffer(0, guid_of<ID3D11Texture2D>(), back.put_void()), "GetBuffer");
        check(device->CreateRenderTargetView(back.get(), nullptr, rtv.put()), "CreateRTV");
    }

    void present(float r, float g, float b) {
        float clear[4] = {r, g, b, 1.f};
        ID3D11RenderTargetView* views[] = {rtv.get()};
        context->OMSetRenderTargets(1, views, nullptr);
        context->ClearRenderTargetView(rtv.get(), clear);
        swap->Present(1, 0);
    }
};

// ---------------------------------------------------------------------------
// The app: window + graphics + a gamepad-driven loop. Layer 1 just clears to
// the nautical background and exits on Menu (the render surface and input pump
// the later layers build on).

struct App : implements<App, IFrameworkViewSource, IFrameworkView> {
    Graphics gfx;
    CoreWindow window{nullptr};
    std::atomic<bool> quit{false};

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
        auto reading = pads.GetAt(0).GetCurrentReading();
        // Layer 1: Menu quits. Later layers route the full pad to UI nav / the
        // host via trireme_pad_* while streaming.
        if ((reading.Buttons & GamepadButtons::Menu) == GamepadButtons::Menu) quit = true;
    }

    void Run() {
        window.Activate();
        float dpi = winrt::Windows::Graphics::Display::DisplayInformation::GetForCurrentView()
                        .LogicalDpi();
        gfx.create(window, dpi);
        logLine("=== Trireme shell (layer 1) up ===");

        auto disp = window.Dispatcher();
        while (!quit) {
            disp.ProcessEvents(CoreProcessEventsOption::ProcessAllIfPresent);
            pollInput();
            gfx.present(0.106f, 0.165f, 0.255f);  // #1B2A41 nautical navy
        }
        logLine("shell exit");
    }
};

}  // namespace

int __stdcall wWinMain(void*, void*, wchar_t*, int) {
    init_apartment();
    // Log to LocalState so a headless console run is inspectable over WDP.
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
