// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors
//
// Phase-1 gate harness: prove quinn's QUIC/UDP survives the UWP AppContainer
// sandbox BEFORE any decode code exists (see crates/trireme-xbox-ffi/README.md,
// "Phase order"). This is deliberately not the shell — no XAML, no D3D, no
// Media Foundation. It pairs, starts a stream, counts the access units the
// core hands it, and writes a per-second log + a final PASS/FAIL verdict to
// LocalState\phase1.log.
//
// Config comes from LocalState\phase1.txt (key=value, one per line):
//   host=192.168.1.100       required; the Pharos host's IP or name
//   pin=123456               optional; when present, SPAKE2-pair first
//   pubkey=<64 hex>          optional; expected host pubkey (else TOFU)
//   seconds=25               optional; stream duration (default 25)
//   width=0 height=0         optional; 0,0 = Auto (host-native canvas)
//   refresh_mhz=60000        optional; panel refresh in millihertz
//   pairport=4432 streamport=15777   optional overrides
//
// Deployment is a loose-file Add-AppxPackage -Register (no signing); see
// build-phase1.ps1 next to this file and rig/xbox-winvm/ for the headless
// Windows-VM driver.

#include <windows.h>  // OutputDebugStringA (UWP-allowed, debugapi family)

#include <winrt/Windows.ApplicationModel.Core.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Storage.h>
#include <winrt/Windows.UI.Core.h>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <future>
#include <map>
#include <mutex>
#include <sstream>
#include <string>
#include <thread>

#include "trireme_xbox.h"

using winrt::Windows::ApplicationModel::Core::CoreApplication;
using winrt::Windows::ApplicationModel::Core::CoreApplicationView;
using winrt::Windows::ApplicationModel::Core::IFrameworkView;
using winrt::Windows::ApplicationModel::Core::IFrameworkViewSource;
using winrt::Windows::UI::Core::CoreDispatcher;
using winrt::Windows::UI::Core::CoreProcessEventsOption;
using winrt::Windows::UI::Core::CoreWindow;

namespace {

// ---------------------------------------------------------------------------
// Logging: append-line file in LocalState + OutputDebugString. The log file is
// the only channel out of the AppContainer on a headless run, so every line is
// flushed immediately.

std::mutex g_logMx;
std::ofstream g_log;
std::chrono::steady_clock::time_point g_t0;

void logLine(const std::string& msg) {
    std::lock_guard<std::mutex> lk(g_logMx);
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                  std::chrono::steady_clock::now() - g_t0)
                  .count();
    std::ostringstream line;
    line << "[" << (ms / 1000) << "." << (ms % 1000) / 100 << "s] " << msg;
    if (g_log.is_open()) {
        g_log << line.str() << "\n";
        g_log.flush();
    }
    OutputDebugStringA((line.str() + "\n").c_str());
}

// ---------------------------------------------------------------------------
// Stream statistics filled from the core's QUIC thread. Callbacks must not
// block (they run on the QUIC loop), so everything is a relaxed atomic.

struct Stats {
    std::atomic<uint64_t> frames{0};
    std::atomic<uint64_t> bytes{0};
    std::atomic<uint64_t> audioPackets{0};
    std::atomic<uint64_t> audioBytes{0};
    std::atomic<int64_t> firstFrameMs{-1};
    std::atomic<bool> ended{false};
    std::atomic<int> degraded{-1};  // -1 unknown, else last heartbeat
    std::mutex endMx;
    std::string endError;
};

Stats g_stats;

void onFrame(void*, const uint8_t*, uintptr_t len) {
    if (g_stats.frames.fetch_add(1, std::memory_order_relaxed) == 0) {
        auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                      std::chrono::steady_clock::now() - g_t0)
                      .count();
        g_stats.firstFrameMs.store(ms, std::memory_order_relaxed);
    }
    g_stats.bytes.fetch_add(len, std::memory_order_relaxed);
}

void onAudio(void*, const uint8_t*, uintptr_t len) {
    g_stats.audioPackets.fetch_add(1, std::memory_order_relaxed);
    g_stats.audioBytes.fetch_add(len, std::memory_order_relaxed);
}

void onEnd(void*, const char* error) {
    {
        std::lock_guard<std::mutex> lk(g_stats.endMx);
        g_stats.endError = error ? error : "";
    }
    g_stats.ended.store(true, std::memory_order_relaxed);
}

void onLinkHealth(void*, bool degraded) {
    g_stats.degraded.store(degraded ? 1 : 0, std::memory_order_relaxed);
}

// ---------------------------------------------------------------------------
// key=value config from LocalState\phase1.txt.

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

std::string cfgOr(const std::map<std::string, std::string>& c, const char* k,
                  const std::string& dflt) {
    auto it = c.find(k);
    return it == c.end() ? dflt : it->second;
}

// ---------------------------------------------------------------------------
// The worker. Runs on its own thread; the UI thread just pumps the dispatcher.

CoreDispatcher g_dispatcher{nullptr};

void requestExit() {
    if (g_dispatcher) {
        g_dispatcher.RunAsync(winrt::Windows::UI::Core::CoreDispatcherPriority::Normal,
                              [] { CoreApplication::Exit(); });
    }
}

void verdict(bool pass, const std::string& why) {
    logLine(std::string("PHASE1 VERDICT: ") + (pass ? "PASS" : "FAIL") + " — " + why);
    requestExit();
}

void workerInner(std::filesystem::path localState);

void worker(std::filesystem::path localState) {
    // Any escaped exception must still land in the log as a verdict — the log
    // file is the only channel out of a headless run.
    try {
        workerInner(std::move(localState));
    } catch (const std::exception& e) {
        verdict(false, std::string("worker exception: ") + e.what());
    } catch (...) {
        verdict(false, "worker exception (non-std)");
    }
}

void workerInner(std::filesystem::path localState) {
    auto cfg = readConfig(localState / "phase1.txt");
    if (cfg.find("host") == cfg.end()) {
        verdict(false, "no phase1.txt config (or missing host=) in LocalState");
        return;
    }
    const std::string host = cfg["host"];
    const std::string pin = cfgOr(cfg, "pin", "");
    const int seconds = std::stoi(cfgOr(cfg, "seconds", "25"));
    const uint32_t width = std::stoul(cfgOr(cfg, "width", "0"));
    const uint32_t height = std::stoul(cfgOr(cfg, "height", "0"));
    const uint32_t refresh = std::stoul(cfgOr(cfg, "refresh_mhz", "60000"));
    const std::string pairAddr = host + ":" + cfgOr(cfg, "pairport", "4432");
    const std::string streamAddr = host + ":" + cfgOr(cfg, "streamport", "15777");
    const std::string idPath = (localState / "identity.toml").string();
    std::string pubkey = cfgOr(cfg, "pubkey", "");

    trireme_init_logging("info");
    logLine("phase1 harness up; core protocol version = " +
            std::to_string(trireme_protocol_version()));
    logLine("host=" + host + " pair=" + pairAddr + " stream=" + streamAddr +
            " mode=" + std::to_string(width) + "x" + std::to_string(height) + "@" +
            std::to_string(refresh) + "mHz for " + std::to_string(seconds) + "s");

    // --- Pair (when a PIN was provided) -----------------------------------
    // trireme_pair blocks; run it under a watchdog so a silently-dropped UDP
    // path (the capability gotcha, a firewall) reads as a clear timeout
    // instead of a hung harness.
    if (!pin.empty()) {
        logLine("pairing with PIN " + pin + " ...");
        auto fut = std::async(std::launch::async, [&] {
            char out[65] = {0};
            int rc = trireme_pair(pairAddr.c_str(), pin.c_str(), idPath.c_str(), out,
                                  sizeof(out));
            return std::make_pair(rc, std::string(out));
        });
        if (fut.wait_for(std::chrono::seconds(30)) != std::future_status::ready) {
            verdict(false, "trireme_pair timed out after 30s (UDP blocked? "
                           "missing privateNetworkClientServer?)");
            return;
        }
        auto [rc, hostKey] = fut.get();
        if (rc != 0) {
            verdict(false, "trireme_pair failed rc=" + std::to_string(rc));
            return;
        }
        pubkey = hostKey;
        logLine("paired OK; host pubkey = " + pubkey);
    } else {
        logLine(pubkey.empty() ? "no PIN/pubkey: connecting TOFU"
                               : "connecting with saved pubkey");
    }

    // --- Stream ------------------------------------------------------------
    TriremeStream* h =
        trireme_start(streamAddr.c_str(), pubkey.empty() ? nullptr : pubkey.c_str(),
                      idPath.c_str(), width, height, refresh, nullptr, onFrame, onEnd,
                      onLinkHealth, nullptr, onAudio, nullptr, nullptr, nullptr);
    if (!h) {
        verdict(false, "trireme_start returned null");
        return;
    }

    uint64_t lastFrames = 0, lastBytes = 0;
    for (int t = 1; t <= seconds; ++t) {
        std::this_thread::sleep_for(std::chrono::seconds(1));
        uint64_t f = g_stats.frames.load(), b = g_stats.bytes.load();
        uint64_t ap = g_stats.audioPackets.load(), ab = g_stats.audioBytes.load();
        std::ostringstream s;
        s << "t=" << t << "s frames=" << f << " (+" << (f - lastFrames) << "/s) bytes=" << b
          << " (+" << (b - lastBytes) << "/s) audio=" << ap << "pkt/" << ab << "B";
        int64_t ff = g_stats.firstFrameMs.load();
        if (ff >= 0 && lastFrames == 0 && f > 0) s << " firstFrame@" << ff << "ms";
        int deg = g_stats.degraded.load();
        if (deg >= 0) s << (deg ? " DEGRADED" : "");
        logLine(s.str());
        lastFrames = f;
        lastBytes = b;
        if (g_stats.ended.load()) {
            std::lock_guard<std::mutex> lk(g_stats.endMx);
            logLine("stream ended early: " +
                    (g_stats.endError.empty() ? "(clean close)" : g_stats.endError));
            break;
        }
    }
    trireme_stop(h);

    uint64_t frames = g_stats.frames.load(), bytes = g_stats.bytes.load();
    std::ostringstream sum;
    sum << "totals: " << frames << " video AUs / " << bytes << " bytes, "
        << g_stats.audioPackets.load() << " audio pkts / " << g_stats.audioBytes.load()
        << " bytes, firstFrame=" << g_stats.firstFrameMs.load() << "ms";
    logLine(sum.str());

    // The gate: sustained access-unit delivery through the AppContainer. 100
    // frames ≈ 2-4s of video — far beyond anything a lucky handshake explains.
    if (frames >= 100 && bytes >= 1'000'000) {
        verdict(true, "QUIC pair+stream survived the UWP AppContainer");
    } else if (g_stats.ended.load()) {
        std::lock_guard<std::mutex> lk(g_stats.endMx);
        verdict(false, "stream ended: " +
                           (g_stats.endError.empty() ? "(clean)" : g_stats.endError));
    } else {
        verdict(false, "insufficient delivery (frames=" + std::to_string(frames) +
                           " bytes=" + std::to_string(bytes) + ")");
    }
}

// ---------------------------------------------------------------------------
// Minimal CoreApplication scaffolding (no XAML): activate the window, hand the
// real work to the worker thread, pump events until it requests exit.

struct App : winrt::implements<App, IFrameworkViewSource, IFrameworkView> {
    IFrameworkView CreateView() { return *this; }
    void Initialize(CoreApplicationView const&) {}
    void Load(winrt::hstring const&) {}
    void Uninitialize() {}
    void SetWindow(CoreWindow const&) {}

    void Run() {
        CoreWindow window = CoreWindow::GetForCurrentThread();
        window.Activate();
        g_dispatcher = window.Dispatcher();

        std::filesystem::path localState{std::wstring{
            winrt::Windows::Storage::ApplicationData::Current().LocalFolder().Path()}};
        g_t0 = std::chrono::steady_clock::now();
        {
            std::lock_guard<std::mutex> lk(g_logMx);
            g_log.open(localState / "phase1.log", std::ios::app);
        }
        logLine("=== TriremePhase1 start ===");

        std::thread(worker, localState).detach();
        g_dispatcher.ProcessEvents(CoreProcessEventsOption::ProcessUntilQuit);
    }
};

}  // namespace

int __stdcall wWinMain(void*, void*, wchar_t*, int) {
    winrt::init_apartment();
    CoreApplication::Run(winrt::make<App>());
    return 0;
}
