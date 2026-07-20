// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors
//
// Direct2D/DirectWrite UI drawn over the swapchain backbuffer after the video
// blit (or the clear). Pure presentation: the screens are drawn from small
// view-model structs owned by main.cpp's state machine. 10-foot layout,
// nautical navy + gold theme matching the Android shell.

#pragma once

#include <d3d11.h>
#include <dxgi1_3.h>

#include <cstdint>
#include <string>
#include <vector>

struct UiHostRow {
    std::wstring name;
    std::wstring detail;  // address / hint line under the name
    std::wstring tag;     // right-aligned badge: "paired", "new", ""
};

struct UiHostsModel {
    std::vector<UiHostRow> rows;  // last row is Exit
    int focus = 0;
    std::wstring status;  // bottom status line
};

struct UiPinModel {
    std::wstring title;   // "Pair with <host>"
    std::wstring digits;  // entered so far, <= 6
    int focus = 4;        // keypad focus 0..11 (3x4 grid: 1..9, DEL 0 OK)
    bool busy = false;    // pairing in flight
    std::wstring status;
};

struct UiOverlayModel {
    std::wstring line;
    bool degraded = false;
};

struct UiMenuModel {
    std::vector<std::wstring> items;
    int focus = 0;
};

struct UiSettingsRow {
    std::wstring label;
    std::wstring value;  // shown as "< value >" on the focused row
};

struct UiSettingsModel {
    std::vector<UiSettingsRow> rows;
    int focus = 0;
    std::wstring hint;
};

class UiRenderer {
public:
    bool init(ID3D11Device* device, IDXGISwapChain1* swap, uint32_t w, uint32_t h);
    void drawHosts(const UiHostsModel& m);
    void drawPin(const UiPinModel& m);
    void drawOverlay(const UiOverlayModel& m);
    void drawStreamMenu(const UiMenuModel& m);
    void drawSettings(const UiSettingsModel& m);
    void shutdown();

private:
    struct Impl;
    Impl* p_ = nullptr;
};
