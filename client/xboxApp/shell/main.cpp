// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors
//
// Trireme Xbox client shell. Layer 3: the D3D11 swapchain + HEVC decode
// (video_ffmpeg.cpp / video.cpp) + Opus audio (audio.cpp) behind a real
// gamepad-driven UI (ui.cpp): a Hosts screen fed by DNS-SD discovery
// (discovery.cpp) and a persisted known-hosts list, an on-screen PIN pad for
// pairing, and a streaming screen that forwards the controller to the host.
// LocalState\stream.txt (host=...) still short-circuits everything for the rig
// scripts. Debug lines land in LocalState\shell.log.

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
#include <cmath>
#include <csignal>
#include <exception>
#include <filesystem>
#include <fstream>
#include <map>
#include <mutex>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "audio.h"
#include "discovery.h"
#include "trireme_xbox.h"
#include "ui.h"
#include "video.h"

using namespace winrt;
using namespace winrt::Windows::ApplicationModel::Core;
using namespace winrt::Windows::UI::Core;
using namespace winrt::Windows::Gaming::Input;

namespace {
std::mutex g_logMx;
std::ofstream g_log;
}  // namespace

// Non-static: the other translation units log through this.
void shellLog(const std::string& msg) {
    std::lock_guard<std::mutex> lk(g_logMx);
    if (g_log.is_open()) { g_log << msg << "\n"; g_log.flush(); }
    OutputDebugStringA((msg + "\n").c_str());
}

namespace {

// Last-gasp crash reporter: an access violation on ANY thread lands here
// before the process dies; name the module + offset so the log tells us where.
// (Rust panic=abort uses __fastfail, which bypasses this — the panic hook set
// via trireme_set_panic_log covers that side.)
LONG WINAPI crashFilter(EXCEPTION_POINTERS* ep) {
    auto* er = ep->ExceptionRecord;
    void* addr = er->ExceptionAddress;
    char modName[192] = "?";
    void* base = nullptr;
    MEMORY_BASIC_INFORMATION mbi{};
    if (VirtualQuery(addr, &mbi, sizeof(mbi)) && mbi.AllocationBase) {
        base = mbi.AllocationBase;
        GetModuleFileNameA(static_cast<HMODULE>(base), modName, sizeof(modName));
    }
    char buf[384];
    snprintf(buf, sizeof(buf), "CRASH: code=0x%08X addr=%p module=%s rva=0x%llx",
             static_cast<unsigned>(er->ExceptionCode), addr, modName,
             static_cast<unsigned long long>(static_cast<char*>(addr) -
                                             static_cast<char*>(base)));
    shellLog(buf);
    if (er->ExceptionCode == EXCEPTION_ACCESS_VIOLATION && er->NumberParameters >= 2) {
        snprintf(buf, sizeof(buf), "CRASH: AV %s address %p",
                 er->ExceptionInformation[0] ? "writing" : "reading",
                 reinterpret_cast<void*>(er->ExceptionInformation[1]));
        shellLog(buf);
    }
    return EXCEPTION_CONTINUE_SEARCH;
}

void check(HRESULT hr, const char* what) {
    if (FAILED(hr)) {
        char b[16];
        snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(hr));
        shellLog(std::string("FATAL ") + what + " " + b);
        throw hresult_error(hr);
    }
}

std::wstring widen(const std::string& s) {
    if (s.empty()) return {};
    int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), nullptr, 0);
    std::wstring out(static_cast<size_t>(n), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), out.data(), n);
    return out;
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

// --- known hosts (LocalState\hosts.txt: name|addr|streamport|pubkey) --------

struct KnownHost {
    std::string name, addr, pubkey;
    int streamPort = 15777;
};

std::vector<KnownHost> loadHosts(const std::filesystem::path& p) {
    std::vector<KnownHost> out;
    std::ifstream f(p);
    std::string line;
    while (std::getline(f, line)) {
        std::stringstream ss(line);
        KnownHost h;
        std::string port;
        if (std::getline(ss, h.name, '|') && std::getline(ss, h.addr, '|') &&
            std::getline(ss, port, '|') && std::getline(ss, h.pubkey)) {
            h.streamPort = atoi(port.c_str());
            if (h.streamPort <= 0) h.streamPort = 15777;
            out.push_back(std::move(h));
        }
    }
    return out;
}

void saveHosts(const std::filesystem::path& p, const std::vector<KnownHost>& hosts) {
    std::ofstream f(p, std::ios::trunc);
    for (const auto& h : hosts)
        f << h.name << "|" << h.addr << "|" << h.streamPort << "|" << h.pubkey << "\n";
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
        // The decode thread, D2D, and the render thread all touch the immediate
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

// --- streaming globals ------------------------------------------------------

VideoPipeline g_video;
AudioPipeline g_audio;
std::atomic<bool> g_audioUp{false};
std::atomic<TriremeStream*> g_stream{nullptr};
std::atomic<bool> g_degraded{false};
std::atomic<bool> g_ended{false};
std::atomic<bool> g_userActive{false};  // UI took over; auto-connect stands down
std::mutex g_endMx;
std::string g_endReason;

// These fire on the Rust QUIC thread: a C++ exception here would unwind
// through Rust frames (UB/abort), so swallow and log.
void onFrame(void* ctx, const uint8_t* data, uintptr_t len) noexcept {
    try {
        static_cast<VideoPipeline*>(ctx)->submit(data, static_cast<size_t>(len));
    } catch (...) {
        shellLog("onFrame threw");
    }
}
void onAudio(void* ctx, const uint8_t* data, uintptr_t len) noexcept {
    try {
        static_cast<AudioPipeline*>(ctx)->submit(data, static_cast<size_t>(len));
    } catch (...) {
        shellLog("onAudio threw");
    }
}
void onEnd(void*, const char* err) {
    shellLog(std::string("stream ended: ") + (err ? err : "(clean)"));
    { std::lock_guard<std::mutex> lk(g_endMx); g_endReason = err ? err : ""; }
    g_ended = true;
}
void onLinkHealth(void*, bool degraded) { g_degraded = degraded; }

// Host app catalog (arrives on the stream's app relay; QUIC thread).
struct AppEntry {
    std::string id, name;
    uint8_t kind = 0;
    bool running = false;
};
std::mutex g_appsMx;
std::vector<AppEntry> g_apps;
std::string g_notice;  // latest host toast, under g_appsMx
std::atomic<double> g_noticeAt{0};

void onCatalog(void*, const TriremeAppInfo* apps, uintptr_t n) noexcept {
    try {
        std::vector<AppEntry> v;
        v.reserve(n);
        for (uintptr_t i = 0; i < n; ++i)
            v.push_back({apps[i].id ? apps[i].id : "", apps[i].name ? apps[i].name : "",
                         apps[i].kind, apps[i].running});
        {
            std::lock_guard<std::mutex> lk(g_appsMx);
            g_apps = std::move(v);
        }
        shellLog("catalog: " + std::to_string(n) + " apps");
    } catch (...) {
    }
}
void onNotice(void*, const char* msg) noexcept {
    try {
        std::lock_guard<std::mutex> lk(g_appsMx);
        g_notice = msg ? msg : "";
        g_noticeAt = std::chrono::duration<double>(
                         std::chrono::steady_clock::now().time_since_epoch())
                         .count();
        shellLog("host notice: " + g_notice);
    } catch (...) {
    }
}

TriremeStream* startStream(const std::string& addr, int streamPort, const std::string& pubkey,
                           const std::string& idPath) {
    const std::string target = addr + ":" + std::to_string(streamPort);
    shellLog("connecting " + target);
    g_ended = false;
    g_degraded = false;
    TriremeStream* s = trireme_start(target.c_str(), pubkey.empty() ? nullptr : pubkey.c_str(),
                                     idPath.c_str(), 0, 0, 60000, &g_video, onFrame, onEnd,
                                     onLinkHealth, g_audioUp.load() ? &g_audio : nullptr,
                                     g_audioUp.load() ? onAudio : nullptr, nullptr, onCatalog,
                                     onNotice);
    if (s) g_stream = s;
    else shellLog("trireme_start returned null");
    return s;
}

// The rig override: LocalState\stream.txt with host= (and optionally pin=)
// connects immediately, exactly like layer 2. Gives the deploy/test scripts a
// UI-free path and keeps a keyboard-free recovery hatch.
void autoConnectWorker(std::filesystem::path localState) {
    std::map<std::string, std::string> cfg;
    for (int i = 0; i < 10 && !g_userActive; ++i) {
        cfg = readConfig(localState / "stream.txt");
        if (cfg.count("host")) break;
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
    }
    if (!cfg.count("host") || g_userActive || g_stream.load()) return;
    // One-shot: consume the override so ordinary launches get the UI instead
    // of forever reconnecting to whatever a rig script last pushed.
    std::error_code ec;
    std::filesystem::remove(localState / "stream.txt", ec);
    const std::string host = cfg["host"];
    const std::string pin = cfgOr(cfg, "pin", "");
    const std::string idPath = (localState / "identity.toml").string();
    std::string pubkey = cfgOr(cfg, "pubkey", "");
    shellLog("auto-connect (stream.txt) host=" + host);
    if (!pin.empty()) {
        const std::string pairAddr = host + ":" + cfgOr(cfg, "pairport", "4432");
        char out[65] = {0};
        int rc = trireme_pair(pairAddr.c_str(), pin.c_str(), idPath.c_str(), out, sizeof(out));
        if (rc != 0) { shellLog("auto pair failed rc=" + std::to_string(rc)); return; }
        pubkey = out;
        shellLog("auto paired OK");
    }
    if (g_userActive || g_stream.load()) return;
    startStream(host, atoi(cfgOr(cfg, "streamport", "15777").c_str()), pubkey, idPath);
}

// --- pairing worker ---------------------------------------------------------

struct PairJob {
    std::string name, addr, pubkeyOut, error;
    int pairPort = 4432, streamPort = 15777;
    bool done = false, ok = false;
};
std::mutex g_pairMx;
std::optional<PairJob> g_pairJob;

void pairWorker(PairJob job, std::string pin, std::string idPath) {
    const std::string pairAddr = job.addr + ":" + std::to_string(job.pairPort);
    char out[65] = {0};
    int rc = trireme_pair(pairAddr.c_str(), pin.c_str(), idPath.c_str(), out, sizeof(out));
    job.done = true;
    job.ok = rc == 0;
    if (job.ok) job.pubkeyOut = out;
    else job.error = "pairing failed (rc=" + std::to_string(rc) + ")";
    std::lock_guard<std::mutex> lk(g_pairMx);
    g_pairJob = std::move(job);
}

// --- settings ---------------------------------------------------------------
//
// Mirrors the Android sliders: resolution / bitrate / fps caps, Auto = 0.
// Stored as indices into the value tables; persisted in LocalState\settings.txt.

constexpr uint32_t kResValues[] = {0, 720, 1080, 1440, 2160};
constexpr uint32_t kBitrateMbps[] = {0, 10, 20, 30, 50, 80, 100, 150, 200, 300};
constexpr uint32_t kFpsValues[] = {0, 30, 60, 120};

struct AppSettings {
    int res = 0, bitrate = 0, fps = 0;  // indices; 0 = Auto
    std::string last;                   // pubkey of the last-connected host

    static AppSettings load(const std::filesystem::path& p) {
        AppSettings s;
        auto cfg = readConfig(p);
        s.last = cfgOr(cfg, "last", "");
        s.res = std::clamp(atoi(cfgOr(cfg, "res", "0").c_str()), 0,
                           static_cast<int>(std::size(kResValues)) - 1);
        s.bitrate = std::clamp(atoi(cfgOr(cfg, "bitrate", "0").c_str()), 0,
                               static_cast<int>(std::size(kBitrateMbps)) - 1);
        s.fps = std::clamp(atoi(cfgOr(cfg, "fps", "0").c_str()), 0,
                           static_cast<int>(std::size(kFpsValues)) - 1);
        return s;
    }
    void save(const std::filesystem::path& p) const {
        std::ofstream f(p, std::ios::trunc);
        f << "res=" << res << "\nbitrate=" << bitrate << "\nfps=" << fps << "\nlast=" << last
          << "\n";
    }
    // Push onto a live stream (the QoS loop re-sends within ~1s).
    void apply(TriremeStream* s) const {
        if (!s) return;
        trireme_set_resolution_pref(s, kResValues[res], 0);
        trireme_set_bitrate_pref(s, 0, kBitrateMbps[bitrate] * 1000);
        trireme_set_fps_pref(s, 0, kFpsValues[fps]);
    }
    std::wstring resText() const {
        return res == 0 ? L"Auto" : std::to_wstring(kResValues[res]) + L"p";
    }
    std::wstring bitrateText() const {
        return bitrate == 0 ? L"Auto" : std::to_wstring(kBitrateMbps[bitrate]) + L" Mbps";
    }
    std::wstring fpsText() const {
        return fps == 0 ? L"Auto" : std::to_wstring(kFpsValues[fps]) + L" fps";
    }
};

// --- app --------------------------------------------------------------------

enum class Screen { Shell, Pin, Streaming };

// Canonical pad indices (core-proto): buttons 0=A 1=B 2=X 3=Y 4=LB 5=RB
// 6=Back 7=Start 8=Guide 9=LS 10=RS; axes 0=LX 1=LY 2=RX 3=RY 4=LT 5=RT
// 6=HatX 7=HatY (down/right positive — WGI's Y axes point up, so negate).
struct PadForwardState {
    bool connected = false;
    bool btn[11] = {};
    float ax[8] = {};
};

struct App : implements<App, IFrameworkViewSource, IFrameworkView> {
    Graphics gfx;
    UiRenderer ui;
    HostDiscovery disco;
    CoreWindow window{nullptr};
    std::atomic<bool> quit{false};
    std::filesystem::path localState;
    std::string idPath;
    std::filesystem::path hostsPath;

    Screen screen = Screen::Shell;
    std::vector<KnownHost> known;
    std::vector<DiscoveredHost> discovered;
    std::wstring status;
    bool uiReady = false;

    // Shell (rail + pane) focus state.
    int railFocus = 0;   // 0 Stream, 1 Hosts, 2 Apps, 3 Settings, 4 Exit
    bool inPane = false;
    int hostFocus = 0;
    int appFocus = 0;
    std::vector<AppEntry> appsNow;  // per-frame snapshot of g_apps

    // PIN screen state.
    DiscoveredHost pinTarget;
    std::string pinDigits;
    int pinFocus = 0;
    bool pairBusy = false;
    std::wstring pinStatus;

    // Streaming state.
    PadForwardState pad;
    std::string streamHostName;
    double streamStartAt = 0;
    uint64_t lastLoggedDecoded = 0;
    bool streamMenuOpen = false;
    int streamMenuFocus = 0;
    bool appsOpen = false;  // in-stream Apps modal
    int appsModalFocus = 0;

    // Settings: pane in the shell, modal over the stream menu.
    AppSettings settings;
    std::filesystem::path settingsPath;
    bool settingsOpen = false;  // streaming-only modal
    int settingsFocus = 0;

    GamepadButtons prevButtons{};
    double navNextAt = 0;

    IFrameworkView CreateView() { return *this; }
    void Initialize(CoreApplicationView const&) {}
    void Load(hstring const&) {}
    void Uninitialize() {}
    void SetWindow(CoreWindow const& w) {
        window = w;
        w.Closed([this](auto&&, auto&&) { quit = true; });
        // Claim the system back gesture: on Xbox an unhandled BackRequested
        // (the B button) navigates OUT of the app. All back behavior is ours,
        // driven from the gamepad polling; exit is explicit via the rail/menu.
        SystemNavigationManager::GetForCurrentView().BackRequested(
            [](auto&&, BackRequestedEventArgs const& a) { a.Handled(true); });
        // Lifecycle telemetry: if the system yanks us (suspend/background), the
        // log shows it — distinguishes an external kill from an internal crash.
        w.VisibilityChanged([](auto&&, VisibilityChangedEventArgs const& a) {
            shellLog(std::string("window visibility: ") + (a.Visible() ? "visible" : "HIDDEN"));
        });
        w.Activated([](auto&&, WindowActivatedEventArgs const& a) {
            shellLog("window activation: " +
                     std::to_string(static_cast<int>(a.WindowActivationState())));
        });
        CoreApplication::Suspending([](auto&&, auto&&) { shellLog("PLM: SUSPENDING"); });
        CoreApplication::Resuming([](auto&&, auto&&) { shellLog("PLM: resuming"); });
        CoreApplication::EnteredBackground([](auto&&, auto&&) { shellLog("PLM: entered background"); });
        CoreApplication::LeavingBackground([](auto&&, auto&&) { shellLog("PLM: leaving background"); });
    }

    static double now() {
        using namespace std::chrono;
        return duration<double>(steady_clock::now().time_since_epoch()).count();
    }

    void stopStream(const std::wstring& why) {
        if (auto s = g_stream.exchange(nullptr)) trireme_stop(s);
        pad = PadForwardState{};
        streamMenuOpen = false;
        appsOpen = false;
        settingsOpen = false;
        screen = Screen::Shell;
        status = why;
    }

    // --- shell (rail + panes) -----------------------------------------------

    struct Row {
        enum Kind { Known, Disco } kind;
        size_t idx = 0;
    };
    std::vector<Row> rows;  // hosts pane rows, rebuilt each frame with the model

    // The host this rail's Stream section connects to: the last-connected one
    // (settings.last pubkey) when it is still known, else the first known.
    const KnownHost* lastHost() const {
        if (known.empty()) return nullptr;
        for (const auto& k : known)
            if (!settings.last.empty() && k.pubkey == settings.last) return &k;
        return &known.front();
    }

    UiShellModel shellModel() {
        UiShellModel m;
        m.rail = railFocus;
        m.inPane = inPane;
        // Stream pane.
        if (const KnownHost* k = lastHost()) {
            m.streamHeadline = widen(k->name);
            m.streamDetail = widen(k->addr) + L" \x2022 paired";
            m.streamHint = L"A  Connect";
        } else {
            m.streamHeadline = L"No paired host";
            m.streamDetail = L"Pair one from the Hosts tab";
        }
        // Hosts pane.
        rows.clear();
        discovered = disco.snapshot();
        for (size_t i = 0; i < known.size(); ++i) {
            rows.push_back({Row::Known, i});
            m.hostRows.push_back({widen(known[i].name), widen(known[i].addr), L"paired"});
        }
        for (size_t i = 0; i < discovered.size(); ++i) {
            const auto& d = discovered[i];
            const bool isKnown = std::any_of(known.begin(), known.end(), [&](const KnownHost& k) {
                return !d.pubkey.empty() && k.pubkey == d.pubkey;
            });
            if (isKnown) continue;  // merged: the known row already covers it
            rows.push_back({Row::Disco, i});
            m.hostRows.push_back(
                {widen(d.alias.empty() ? d.instance : d.alias), widen(d.addr), L"new"});
        }
        hostFocus = std::clamp(hostFocus, 0, std::max(0, static_cast<int>(rows.size()) - 1));
        m.hostFocus = hostFocus;
        // Apps pane (catalog only flows during a stream).
        for (const auto& a : appsNow)
            m.appRows.push_back({widen(a.name), L"", a.running ? L"running" : L""});
        appFocus = std::clamp(appFocus, 0, std::max(0, static_cast<int>(appsNow.size()) - 1));
        m.appFocus = appFocus;
        m.appsEmpty = L"Connect to a host to browse its apps";
        // Settings pane.
        m.settingRows = {{L"Resolution", settings.resText()},
                         {L"Max bitrate", settings.bitrateText()},
                         {L"Max framerate", settings.fpsText()}};
        m.settingsFocus = settingsFocus;
        m.status = status;
        return m;
    }

    void connectTo(const KnownHost& k) {
        g_userActive = true;
        streamHostName = k.name;
        settings.last = k.pubkey;
        settings.save(settingsPath);
        if (startStream(k.addr, k.streamPort, k.pubkey, idPath)) enterStreaming();
        else status = L"connect failed";
    }

    void hostsActivate() {
        if (rows.empty()) return;
        const Row r = rows[static_cast<size_t>(hostFocus)];
        if (r.kind == Row::Known) {
            connectTo(known[r.idx]);
        } else {
            g_userActive = true;
            pinTarget = discovered[r.idx];
            pinDigits.clear();
            pinFocus = 0;
            pinStatus.clear();
            screen = Screen::Pin;
        }
    }

    // --- PIN screen ---------------------------------------------------------

    void pinActivate() {
        if (pairBusy) return;
        if (pinFocus <= 8) {  // digits 1..9
            if (pinDigits.size() < 6) pinDigits += static_cast<char>('1' + pinFocus);
        } else if (pinFocus == 9) {  // DEL
            if (!pinDigits.empty()) pinDigits.pop_back();
        } else if (pinFocus == 10) {  // 0
            if (pinDigits.size() < 6) pinDigits += '0';
        } else {  // OK
            if (pinDigits.size() != 6) { pinStatus = L"enter all 6 digits"; return; }
            pairBusy = true;
            pinStatus.clear();
            PairJob job;
            job.name = pinTarget.alias.empty() ? pinTarget.instance : pinTarget.alias;
            job.addr = pinTarget.addr;
            job.pairPort = pinTarget.pairPort;
            job.streamPort = pinTarget.streamPort;
            std::thread(pairWorker, job, pinDigits, idPath).detach();
        }
    }

    void pollPairResult() {
        std::optional<PairJob> job;
        {
            std::lock_guard<std::mutex> lk(g_pairMx);
            if (g_pairJob && g_pairJob->done) { job = std::move(g_pairJob); g_pairJob.reset(); }
        }
        if (!job) return;
        pairBusy = false;
        if (!job->ok) {
            pinStatus = widen(job->error) + L" — get a fresh PIN and retry";
            pinDigits.clear();
            return;
        }
        // Upsert by pubkey, persist, connect.
        auto it = std::find_if(known.begin(), known.end(),
                               [&](const KnownHost& k) { return k.pubkey == job->pubkeyOut; });
        KnownHost h{job->name, job->addr, job->pubkeyOut, job->streamPort};
        if (it == known.end()) known.push_back(h);
        else *it = h;
        saveHosts(hostsPath, known);
        shellLog("paired + saved host " + h.name + " (" + h.addr + ")");
        streamHostName = h.name;
        settings.last = h.pubkey;
        settings.save(settingsPath);
        if (startStream(h.addr, h.streamPort, h.pubkey, idPath)) enterStreaming();
        else { screen = Screen::Shell; status = L"paired, but connect failed"; }
    }

    // --- streaming ----------------------------------------------------------

    void enterStreaming() {
        screen = Screen::Streaming;
        streamStartAt = now();
        status.clear();
        settings.apply(g_stream.load());  // assert saved caps on every connect
    }

    // Send neutral state for everything currently forwarded as active — called
    // when the in-stream menu opens so the game isn't left with stuck input.
    void releaseForwarded(TriremeStream* s) {
        for (uint8_t i = 0; i < 11; ++i)
            if (pad.btn[i]) { pad.btn[i] = false; trireme_pad_button(s, 0, i, false); }
        for (uint8_t i = 0; i < 8; ++i)
            if (std::fabs(pad.ax[i]) > 0.0f) { pad.ax[i] = 0.f; trireme_pad_axis(s, 0, i, 0.f); }
    }

    void forwardPad(TriremeStream* s, GamepadReading const& r) {
        if (!pad.connected) {
            trireme_pad_connect(s, 0, 0x045E, 0x0B12);  // Xbox Series pad ids
            pad.connected = true;
        }
        // View+LB opens the in-stream menu; everything else forwards. NOT
        // View+B: that is Dev Mode's reserved "return to Dev Home" chord (the
        // system terminates the app externally — no in-process trap sees it).
        // Not LS+RS either: games bind stick-clicks (sprint/ultimates).
        const bool view = (r.Buttons & GamepadButtons::View) == GamepadButtons::View;
        const bool lb = (r.Buttons & GamepadButtons::LeftShoulder) == GamepadButtons::LeftShoulder;
        if (view && lb) {
            shellLog("menu: View+LB chord — releasing forwarded input");
            releaseForwarded(s);
            streamMenuOpen = true;
            streamMenuFocus = 0;
            shellLog("menu: open");
            return;
        }

        static constexpr std::pair<GamepadButtons, uint8_t> kBtns[] = {
            {GamepadButtons::A, 0},           {GamepadButtons::B, 1},
            {GamepadButtons::X, 2},           {GamepadButtons::Y, 3},
            {GamepadButtons::LeftShoulder, 4}, {GamepadButtons::RightShoulder, 5},
            {GamepadButtons::View, 6},        {GamepadButtons::Menu, 7},
            {GamepadButtons::LeftThumbstick, 9},
            {GamepadButtons::RightThumbstick, 10},
        };
        for (auto [wb, idx] : kBtns) {
            const bool v = (r.Buttons & wb) == wb;
            if (v != pad.btn[idx]) {
                pad.btn[idx] = v;
                trireme_pad_button(s, 0, idx, v);
            }
        }
        const bool dl = (r.Buttons & GamepadButtons::DPadLeft) == GamepadButtons::DPadLeft;
        const bool dr = (r.Buttons & GamepadButtons::DPadRight) == GamepadButtons::DPadRight;
        const bool du = (r.Buttons & GamepadButtons::DPadUp) == GamepadButtons::DPadUp;
        const bool dd = (r.Buttons & GamepadButtons::DPadDown) == GamepadButtons::DPadDown;
        const float axv[8] = {
            static_cast<float>(r.LeftThumbstickX),  static_cast<float>(-r.LeftThumbstickY),
            static_cast<float>(r.RightThumbstickX), static_cast<float>(-r.RightThumbstickY),
            static_cast<float>(r.LeftTrigger),      static_cast<float>(r.RightTrigger),
            (dr ? 1.f : 0.f) - (dl ? 1.f : 0.f),    (dd ? 1.f : 0.f) - (du ? 1.f : 0.f),
        };
        for (uint8_t i = 0; i < 8; ++i) {
            if (std::fabs(axv[i] - pad.ax[i]) > 0.005f) {
                pad.ax[i] = axv[i];
                trireme_pad_axis(s, 0, i, axv[i]);
            }
        }
    }

    UiOverlayModel overlayModel(bool freshNotice) {
        UiOverlayModel m;
        m.degraded = g_degraded.load();
        if (freshNotice) {
            std::lock_guard<std::mutex> lk(g_appsMx);
            m.line = widen(g_notice);
            return m;
        }
        std::wstringstream ss;
        ss << widen(streamHostName) << L"   " << g_video.frameWidth() << L"x"
           << g_video.frameHeight() << L"   frames " << g_video.decoded() << L"   audio "
           << g_audio.packets();
        if (g_audio.underruns() > 0) ss << L" (" << g_audio.underruns() << L" underruns)";
        if (m.degraded) ss << L"   LINK DEGRADED";
        else ss << L"   View+LB menu";
        m.line = ss.str();
        return m;
    }

    // --- input --------------------------------------------------------------

    bool edge(GamepadButtons cur, GamepadButtons b) {
        return (cur & b) == b && (prevButtons & b) != b;
    }
    // Edge with hold-to-repeat, for list/keypad navigation.
    bool nav(GamepadButtons cur, GamepadButtons b) {
        const bool held = (cur & b) == b;
        if (!held) return false;
        const double t = now();
        if ((prevButtons & b) != b) { navNextAt = t + 0.40; return true; }
        if (t >= navNextAt) { navNextAt = t + 0.15; return true; }
        return false;
    }

    void updateStreamMenuInput(GamepadReading const& r) {
        const GamepadButtons cur = r.Buttons;
        constexpr int kItems = 5;  // Resume / Apps / Settings / Disconnect / Exit
        if (nav(cur, GamepadButtons::DPadDown)) streamMenuFocus++;
        if (nav(cur, GamepadButtons::DPadUp)) streamMenuFocus--;
        streamMenuFocus = std::clamp(streamMenuFocus, 0, kItems - 1);
        if (edge(cur, GamepadButtons::B)) {
            shellLog("menu: closed (B)");
            streamMenuOpen = false;
        } else if (edge(cur, GamepadButtons::A)) {
            shellLog("menu: activate item " + std::to_string(streamMenuFocus));
            if (streamMenuFocus == 0) streamMenuOpen = false;
            else if (streamMenuFocus == 1) { appsOpen = true; appsModalFocus = 0; }
            else if (streamMenuFocus == 2) { settingsOpen = true; settingsFocus = 0; }
            else if (streamMenuFocus == 3) stopStream(L"disconnected");
            else quit = true;
        }
        prevButtons = cur;
    }

    // In-stream Apps modal: A launches (or switches to a running app), X ends a
    // running app, B returns to the stream menu.
    void updateAppsModalInput(GamepadReading const& r) {
        const GamepadButtons cur = r.Buttons;
        const int n = static_cast<int>(appsNow.size());
        if (nav(cur, GamepadButtons::DPadDown)) appsModalFocus++;
        if (nav(cur, GamepadButtons::DPadUp)) appsModalFocus--;
        appsModalFocus = std::clamp(appsModalFocus, 0, std::max(0, n - 1));
        if (edge(cur, GamepadButtons::B)) {
            appsOpen = false;
        } else if (edge(cur, GamepadButtons::A) && n > 0) {
            const AppEntry& a = appsNow[static_cast<size_t>(appsModalFocus)];
            TriremeStream* s = g_stream.load();
            if (s) {
                if (a.running) {
                    shellLog("apps: switch to " + a.id);
                    trireme_app_switch(s, a.id.c_str());
                } else {
                    shellLog("apps: launch " + a.id);
                    trireme_app_launch(s, a.id.c_str());
                }
            }
            appsOpen = false;
            streamMenuOpen = false;  // drop back to the stream to watch it come up
        } else if (edge(cur, GamepadButtons::X) && n > 0) {
            const AppEntry& a = appsNow[static_cast<size_t>(appsModalFocus)];
            if (a.running) {
                if (TriremeStream* s = g_stream.load()) {
                    shellLog("apps: end " + a.id);
                    trireme_app_end(s, a.id.c_str());
                }
            }
        }
        prevButtons = cur;
    }

    void updateSettingsInput(GamepadReading const& r) {
        const GamepadButtons cur = r.Buttons;
        constexpr int kRows = 3;
        if (nav(cur, GamepadButtons::DPadDown)) settingsFocus++;
        if (nav(cur, GamepadButtons::DPadUp)) settingsFocus--;
        settingsFocus = std::clamp(settingsFocus, 0, kRows - 1);
        int dir = 0;
        if (nav(cur, GamepadButtons::DPadRight)) dir = 1;
        if (nav(cur, GamepadButtons::DPadLeft)) dir = -1;
        if (dir != 0) adjustSetting(dir);
        if (edge(cur, GamepadButtons::B)) settingsOpen = false;
        prevButtons = cur;
    }

    void updateShellInput(GamepadReading const& r) {
        const GamepadButtons cur = r.Buttons;
        if (!inPane) {
            if (nav(cur, GamepadButtons::DPadDown)) railFocus++;
            if (nav(cur, GamepadButtons::DPadUp)) railFocus--;
            railFocus = std::clamp(railFocus, 0, 4);
            const bool enter =
                edge(cur, GamepadButtons::A) || nav(cur, GamepadButtons::DPadRight);
            if (enter) {
                if (railFocus == 4) {
                    if (edge(cur, GamepadButtons::A)) quit = true;
                } else {
                    inPane = true;
                }
            }
        } else {
            if (edge(cur, GamepadButtons::B)) { inPane = false; prevButtons = cur; return; }
            switch (railFocus) {
                case 0:  // Stream: one-press reconnect
                    if (nav(cur, GamepadButtons::DPadLeft)) inPane = false;
                    if (edge(cur, GamepadButtons::A))
                        if (const KnownHost* k = lastHost()) connectTo(*k);
                    break;
                case 1:  // Hosts
                    if (nav(cur, GamepadButtons::DPadLeft)) inPane = false;
                    if (nav(cur, GamepadButtons::DPadDown)) hostFocus++;
                    if (nav(cur, GamepadButtons::DPadUp)) hostFocus--;
                    if (edge(cur, GamepadButtons::A)) hostsActivate();
                    break;
                case 2:  // Apps (informational when idle; live control is in-stream)
                    if (nav(cur, GamepadButtons::DPadLeft)) inPane = false;
                    if (nav(cur, GamepadButtons::DPadDown)) appFocus++;
                    if (nav(cur, GamepadButtons::DPadUp)) appFocus--;
                    break;
                case 3: {  // Settings (left/right adjusts, so left does NOT exit)
                    if (nav(cur, GamepadButtons::DPadDown)) settingsFocus++;
                    if (nav(cur, GamepadButtons::DPadUp)) settingsFocus--;
                    settingsFocus = std::clamp(settingsFocus, 0, 2);
                    int dir = 0;
                    if (nav(cur, GamepadButtons::DPadRight)) dir = 1;
                    if (nav(cur, GamepadButtons::DPadLeft)) dir = -1;
                    if (dir != 0) adjustSetting(dir);
                    break;
                }
            }
        }
        prevButtons = cur;
    }

    void adjustSetting(int dir) {
        auto cycle = [&](int v, size_t n) {
            return (v + dir + static_cast<int>(n)) % static_cast<int>(n);
        };
        if (settingsFocus == 0) settings.res = cycle(settings.res, std::size(kResValues));
        else if (settingsFocus == 1)
            settings.bitrate = cycle(settings.bitrate, std::size(kBitrateMbps));
        else settings.fps = cycle(settings.fps, std::size(kFpsValues));
        settings.save(settingsPath);
        settings.apply(g_stream.load());  // live-tune if streaming
    }

    void updatePinInput(GamepadReading const& r) {
        const GamepadButtons cur = r.Buttons;
        if (!pairBusy) {
            if (nav(cur, GamepadButtons::DPadRight) && pinFocus % 3 < 2) pinFocus++;
            if (nav(cur, GamepadButtons::DPadLeft) && pinFocus % 3 > 0) pinFocus--;
            if (nav(cur, GamepadButtons::DPadDown) && pinFocus / 3 < 3) pinFocus += 3;
            if (nav(cur, GamepadButtons::DPadUp) && pinFocus / 3 > 0) pinFocus -= 3;
            if (edge(cur, GamepadButtons::A)) pinActivate();
            if (edge(cur, GamepadButtons::B)) { screen = Screen::Shell; status.clear(); }
        }
        prevButtons = cur;
    }

    // --- run loop -----------------------------------------------------------

    void Run() {
        window.Activate();
        float dpi =
            winrt::Windows::Graphics::Display::DisplayInformation::GetForCurrentView().LogicalDpi();
        gfx.create(window, dpi);
        shellLog("=== Trireme shell (layer 3: UI) up ===");

        if (!g_video.init(gfx.device.get(), gfx.context.get()))
            shellLog("video pipeline init FAILED");
        g_audioUp = g_audio.init();
        if (!g_audioUp) shellLog("audio pipeline unavailable — video only");
        uiReady = ui.init(gfx.device.get(), gfx.swap.get(), gfx.width, gfx.height);
        if (!uiReady) shellLog("ui init FAILED — falling back to headless behavior");

        localState = std::filesystem::path{std::wstring{
            winrt::Windows::Storage::ApplicationData::Current().LocalFolder().Path()}};
        idPath = (localState / "identity.toml").string();
        hostsPath = localState / "hosts.txt";
        known = loadHosts(hostsPath);
        settingsPath = localState / "settings.txt";
        settings = AppSettings::load(settingsPath);
        shellLog("known hosts: " + std::to_string(known.size()));

        disco.start();
        std::thread(autoConnectWorker, localState).detach();

        auto disp = window.Dispatcher();
        while (!quit) {
            disp.ProcessEvents(CoreProcessEventsOption::ProcessAllIfPresent);

            // Screen is derived from the stream handle so the auto-connect
            // worker and UI-driven connects converge on the same state.
            TriremeStream* s = g_stream.load();
            if (s && screen != Screen::Streaming) enterStreaming();
            if (g_ended.exchange(false)) {
                std::string why;
                { std::lock_guard<std::mutex> lk(g_endMx); why = g_endReason; }
                stopStream(why.empty() ? L"stream ended" : L"stream ended: " + widen(why));
                s = nullptr;
            }

            try {
                {
                    std::lock_guard<std::mutex> lk(g_appsMx);
                    appsNow = g_apps;
                }
                auto pads = Gamepad::Gamepads();
                if (pads.Size() > 0) {
                    auto r = pads.GetAt(0).GetCurrentReading();
                    if (screen == Screen::Streaming) {
                        if (appsOpen) updateAppsModalInput(r);
                        else if (settingsOpen) updateSettingsInput(r);
                        else if (streamMenuOpen) updateStreamMenuInput(r);
                        else {
                            if (s) forwardPad(s, r);
                            prevButtons = r.Buttons;
                        }
                    } else if (screen == Screen::Pin) {
                        updatePinInput(r);
                    } else {
                        updateShellInput(r);
                    }
                }
                if (screen == Screen::Pin) pollPairResult();
            } catch (hresult_error const& e) {
                char b[16];
                snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(e.code()));
                shellLog(std::string("input handling threw ") + b);
            } catch (...) {
                shellLog("input handling threw (unknown)");
            }

            auto back = gfx.backbuffer();
            const bool haveVideo =
                screen == Screen::Streaming && g_video.blitLatest(back.get(), gfx.width, gfx.height);
            if (!haveVideo) gfx.clear(back.get(), 0.106f, 0.165f, 0.255f);

            if (uiReady) try {
                if (screen == Screen::Shell) {
                    ui.drawShell(shellModel());
                } else if (screen == Screen::Pin) {
                    UiPinModel pm;
                    pm.title = L"Pair with " +
                               widen(pinTarget.alias.empty() ? pinTarget.instance : pinTarget.alias);
                    pm.digits = widen(pinDigits);
                    pm.focus = pinFocus;
                    pm.busy = pairBusy;
                    pm.status = pinStatus;
                    ui.drawPin(pm);
                } else if (screen == Screen::Streaming && appsOpen) {
                    UiMenuModel mm;
                    mm.title = L"APPS";
                    mm.hint = L"A Launch/Switch   X End   B Back";
                    if (appsNow.empty()) mm.items = {L"(no apps published by this host)"};
                    else
                        for (const auto& a : appsNow) {
                            // A "running" Desktop tile = the host is mirroring its
                            // real desktop; X ends that and returns to the virtual
                            // display.
                            const wchar_t* tag = !a.running ? L""
                                                 : a.kind == 2 ? L"  \x2022 desktop stream (X ends)"
                                                               : L"  \x2022 running";
                            mm.items.push_back(widen(a.name) + tag);
                        }
                    mm.focus = appsModalFocus;
                    ui.drawStreamMenu(mm);
                } else if (screen == Screen::Streaming && settingsOpen) {
                    UiSettingsModel sm;
                    sm.rows = {{L"Resolution", settings.resText()},
                               {L"Max bitrate", settings.bitrateText()},
                               {L"Max framerate", settings.fpsText()}};
                    sm.focus = settingsFocus;
                    ui.drawSettings(sm);
                } else if (screen == Screen::Streaming && streamMenuOpen) {
                    UiMenuModel mm;
                    mm.items = {L"Resume", L"Apps", L"Settings", L"Disconnect", L"Exit Trireme"};
                    mm.focus = streamMenuFocus;
                    ui.drawStreamMenu(mm);
                } else if (screen == Screen::Streaming) {
                    // Stats overlay for the first seconds / on degradation, and
                    // host notices (toasts) whenever fresh.
                    const bool freshNotice = now() - g_noticeAt.load() < 4.0;
                    if (now() - streamStartAt < 5.0 || g_degraded.load() || freshNotice)
                        ui.drawOverlay(overlayModel(freshNotice));
                }
            } catch (hresult_error const& e) {
                char b[16];
                snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(e.code()));
                shellLog(std::string("ui draw threw ") + b + " — disabling UI layer");
                uiReady = false;
            } catch (std::exception const& e) {
                shellLog(std::string("ui draw threw: ") + e.what() + " — disabling UI layer");
                uiReady = false;
            } catch (...) {
                shellLog("ui draw threw (unknown) — disabling UI layer");
                uiReady = false;
            }
            gfx.present();

            uint64_t d = g_video.decoded();
            if (screen == Screen::Streaming && d >= lastLoggedDecoded + 600) {
                shellLog("decoded=" + std::to_string(d) + " " + std::to_string(g_video.frameWidth()) +
                         "x" + std::to_string(g_video.frameHeight()) + " audio_pkts=" +
                         std::to_string(g_audio.packets()) + " underruns=" +
                         std::to_string(g_audio.underruns()));
                lastLoggedDecoded = d;
            }
        }
        if (auto s = g_stream.exchange(nullptr)) trireme_stop(s);
        disco.stop();
        ui.shutdown();
        g_audio.shutdown();
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
        {
            std::lock_guard<std::mutex> lk(g_logMx);
            g_log.open(ls / "shell.log", std::ios::trunc);
        }
        trireme_set_panic_log((ls / "panic.log").string().c_str());
    } catch (...) {
    }
    SetUnhandledExceptionFilter(crashFilter);
    // Uncaught C++ exceptions on background threads (audio graph, DeviceWatcher,
    // QUIC callbacks) go std::terminate -> __fastfail, which bypasses the SEH
    // filter AND the Rust panic hook. terminate handlers still run first.
    std::set_terminate([] {
        shellLog("TERMINATE: unhandled C++ exception on some thread");
        if (auto e = std::current_exception()) {
            try {
                std::rethrow_exception(e);
            } catch (hresult_error const& he) {
                char b[16];
                snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(he.code()));
                shellLog(std::string("TERMINATE: hresult_error ") + b + " " +
                         to_string(he.message()));
            } catch (std::exception const& se) {
                shellLog(std::string("TERMINATE: std::exception ") + se.what());
            } catch (...) {
                shellLog("TERMINATE: non-std exception");
            }
        }
        std::abort();
    });
    signal(SIGABRT, [](int) { shellLog("ABORT: abort() called"); });
    CoreApplication::Run(make<App>());
    return 0;
}
