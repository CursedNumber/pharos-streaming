// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors
//
// See ui.h.

#include "ui.h"

#include <windows.h>  // before d2d1 so the DrawText macro state is consistent

#include <d2d1_1.h>
#include <dwrite.h>
#include <winrt/base.h>

#include <algorithm>
#include <string>

using winrt::com_ptr;

void shellLog(const std::string& s);  // main.cpp

namespace {

bool ok(HRESULT hr, const char* what) {
    if (FAILED(hr)) {
        char b[16];
        snprintf(b, sizeof(b), "0x%08X", static_cast<uint32_t>(hr));
        shellLog(std::string("ui: ") + what + " " + b);
        return false;
    }
    return true;
}

// Palette (nautical navy + lighthouse gold).
constexpr D2D1_COLOR_F kRailBg{0.078f, 0.125f, 0.184f, 1.f};  // #14202F
constexpr D2D1_COLOR_F kBg{0.106f, 0.165f, 0.255f, 1.f};      // #1B2A41
constexpr D2D1_COLOR_F kPanel{0.145f, 0.216f, 0.337f, 1.f};   // #253756
constexpr D2D1_COLOR_F kPanelHi{0.180f, 0.263f, 0.404f, 1.f}; // focused fill
constexpr D2D1_COLOR_F kGold{0.941f, 0.706f, 0.161f, 1.f};    // #F0B429
constexpr D2D1_COLOR_F kText{0.910f, 0.925f, 0.957f, 1.f};    // #E8ECF4
constexpr D2D1_COLOR_F kDim{0.576f, 0.631f, 0.722f, 1.f};     // #93A1B8
constexpr D2D1_COLOR_F kGreen{0.208f, 0.769f, 0.553f, 1.f};   // #35C48D
constexpr D2D1_COLOR_F kRed{0.898f, 0.282f, 0.302f, 1.f};     // #E5484D
constexpr D2D1_COLOR_F kScrim{0.f, 0.f, 0.f, 0.62f};

const wchar_t* kKeyLabels[12] = {L"1", L"2", L"3", L"4", L"5", L"6",
                                 L"7", L"8", L"9", L"DEL", L"0", L"OK"};

const wchar_t* kRailItems[5] = {L"Stream", L"Hosts", L"Apps", L"Settings", L"Exit"};

}  // namespace

struct UiRenderer::Impl {
    com_ptr<ID2D1Factory1> d2dFactory;
    com_ptr<ID2D1Device> d2dDevice;
    com_ptr<ID2D1DeviceContext> ctx;
    com_ptr<ID2D1Bitmap1> target;
    com_ptr<IDWriteFactory> dwrite;
    com_ptr<ID2D1SolidColorBrush> brush;  // one brush, recolored per draw

    com_ptr<IDWriteTextFormat> fmtTitle, fmtRow, fmtDetail, fmtSmall, fmtSmallRight, fmtCenter,
        fmtCenterSmall, fmtDigit, fmtKey;
    float w = 0, h = 0, s = 1.f;  // s scales the 1080p design grid

    bool makeFormat(const wchar_t* family, float size, DWRITE_FONT_WEIGHT weight,
                    DWRITE_TEXT_ALIGNMENT ha, DWRITE_PARAGRAPH_ALIGNMENT va,
                    com_ptr<IDWriteTextFormat>& out) {
        if (!ok(dwrite->CreateTextFormat(family, nullptr, weight, DWRITE_FONT_STYLE_NORMAL,
                                         DWRITE_FONT_STRETCH_NORMAL, size * s, L"",
                                         out.put()),
                "CreateTextFormat"))
            return false;
        out->SetTextAlignment(ha);
        out->SetParagraphAlignment(va);
        return true;
    }

    void fillRect(D2D1_RECT_F r, D2D1_COLOR_F c, float radius = 0.f) {
        brush->SetColor(c);
        if (radius > 0.f)
            ctx->FillRoundedRectangle({r, radius, radius}, brush.get());
        else
            ctx->FillRectangle(r, brush.get());
    }
    void strokeRect(D2D1_RECT_F r, D2D1_COLOR_F c, float width, float radius = 0.f) {
        brush->SetColor(c);
        if (radius > 0.f)
            ctx->DrawRoundedRectangle({r, radius, radius}, brush.get(), width);
        else
            ctx->DrawRectangle(r, brush.get(), width);
    }
    void text(const std::wstring& t, IDWriteTextFormat* f, D2D1_RECT_F r, D2D1_COLOR_F c) {
        brush->SetColor(c);
        ctx->DrawText(t.c_str(), static_cast<UINT32>(t.size()), f, r, brush.get(),
                      D2D1_DRAW_TEXT_OPTIONS_CLIP);
    }
    // Chrome shared by the full-screen views: background wash + brand mark.
    void chrome() {
        ctx->Clear(kBg);
        text(L"TRIREME", fmtTitle.get(), {120 * s, 64 * s, w, 160 * s}, kGold);
        fillRect({122 * s, 152 * s, 470 * s, 156 * s}, kGold);
    }
};

bool UiRenderer::init(ID3D11Device* device, IDXGISwapChain1* swap, uint32_t width,
                      uint32_t height) {
    p_ = new Impl();
    p_->w = static_cast<float>(width);
    p_->h = static_cast<float>(height);
    p_->s = p_->h / 1080.f;

    D2D1_FACTORY_OPTIONS fo{};
    if (!ok(D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, __uuidof(ID2D1Factory1), &fo,
                              p_->d2dFactory.put_void()),
            "D2D1CreateFactory"))
        return false;
    com_ptr<IDXGIDevice> dxgi;
    if (!ok(device->QueryInterface(__uuidof(IDXGIDevice), dxgi.put_void()), "as IDXGIDevice"))
        return false;
    if (!ok(p_->d2dFactory->CreateDevice(dxgi.get(), p_->d2dDevice.put()), "D2D CreateDevice"))
        return false;
    if (!ok(p_->d2dDevice->CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE, p_->ctx.put()),
            "CreateDeviceContext"))
        return false;

    // Wrap the backbuffer: in the D3D11 flip model, buffer 0 is always the
    // current render target, so one bitmap serves every frame.
    com_ptr<IDXGISurface> surface;
    if (!ok(swap->GetBuffer(0, __uuidof(IDXGISurface), surface.put_void()), "GetBuffer(surface)"))
        return false;
    D2D1_BITMAP_PROPERTIES1 bp{};
    bp.pixelFormat = {DXGI_FORMAT_B8G8R8A8_UNORM, D2D1_ALPHA_MODE_IGNORE};
    bp.bitmapOptions = D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW;
    bp.dpiX = 96.f;
    bp.dpiY = 96.f;
    if (!ok(p_->ctx->CreateBitmapFromDxgiSurface(surface.get(), &bp, p_->target.put()),
            "CreateBitmapFromDxgiSurface"))
        return false;
    p_->ctx->SetTarget(p_->target.get());

    if (!ok(p_->ctx->CreateSolidColorBrush(kText, p_->brush.put()), "CreateSolidColorBrush"))
        return false;
    if (!ok(DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, __uuidof(IDWriteFactory),
                                reinterpret_cast<IUnknown**>(p_->dwrite.put())),
            "DWriteCreateFactory"))
        return false;

    bool f = true;
    f &= p_->makeFormat(L"Segoe UI", 64, DWRITE_FONT_WEIGHT_BOLD, DWRITE_TEXT_ALIGNMENT_LEADING,
                        DWRITE_PARAGRAPH_ALIGNMENT_NEAR, p_->fmtTitle);
    f &= p_->makeFormat(L"Segoe UI", 30, DWRITE_FONT_WEIGHT_SEMI_BOLD,
                        DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_PARAGRAPH_ALIGNMENT_NEAR, p_->fmtRow);
    f &= p_->makeFormat(L"Segoe UI", 21, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING,
                        DWRITE_PARAGRAPH_ALIGNMENT_NEAR, p_->fmtDetail);
    f &= p_->makeFormat(L"Segoe UI", 22, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_LEADING,
                        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, p_->fmtSmall);
    f &= p_->makeFormat(L"Segoe UI", 22, DWRITE_FONT_WEIGHT_SEMI_BOLD,
                        DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                        p_->fmtSmallRight);
    f &= p_->makeFormat(L"Segoe UI", 30, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER,
                        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, p_->fmtCenter);
    f &= p_->makeFormat(L"Segoe UI", 22, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_TEXT_ALIGNMENT_CENTER,
                        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, p_->fmtCenterSmall);
    f &= p_->makeFormat(L"Segoe UI", 44, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_CENTER,
                        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, p_->fmtDigit);
    f &= p_->makeFormat(L"Segoe UI", 30, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_CENTER,
                        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, p_->fmtKey);
    if (!f) return false;
    shellLog("ui: D2D renderer ready");
    return true;
}

void UiRenderer::drawShell(const UiShellModel& m) {
    if (!p_) return;
    auto& I = *p_;
    const float s = I.s;
    I.ctx->BeginDraw();
    I.ctx->Clear(kBg);

    // Left nav rail.
    const float railW = 320 * s;
    I.fillRect({0, 0, railW, I.h}, kRailBg);
    I.text(L"TRIREME", I.fmtTitle.get(), {36 * s, 44 * s, railW, 130 * s}, kGold);
    I.fillRect({38 * s, 122 * s, 262 * s, 126 * s}, kGold);
    float ry = 190 * s;
    const float railRowH = 74 * s, railGap = 10 * s;
    for (int i = 0; i < 5; ++i) {
        const bool active = i == m.rail;
        D2D1_RECT_F r{20 * s, ry, railW - 20 * s, ry + railRowH};
        if (active && !m.inPane) {
            I.fillRect(r, kPanelHi, 10 * s);
            I.strokeRect(r, kGold, 3.f * s, 10 * s);
        } else if (active) {
            I.fillRect(r, kPanel, 10 * s);
            I.fillRect({20 * s, ry + 8 * s, 27 * s, ry + railRowH - 8 * s}, kGold, 3 * s);
        }
        I.text(kRailItems[i], I.fmtRow.get(), {r.left + 34 * s, r.top + 17 * s, r.right, r.bottom},
               active ? kText : kDim);
        ry += railRowH + railGap;
    }

    // Content pane for the focused section (Exit has none; show Stream).
    const int section = m.rail < 4 ? m.rail : 0;
    const float cx = railW + 60 * s, cw = I.w - cx - 80 * s;
    const float rowH = 92 * s, gap = 14 * s;
    float y = 190 * s;

    auto rowList = [&](const std::vector<UiHostRow>& rows, int focus) {
        for (size_t i = 0; i < rows.size(); ++i) {
            const bool focused = m.inPane && static_cast<int>(i) == focus;
            D2D1_RECT_F r{cx, y, cx + cw, y + rowH};
            I.fillRect(r, focused ? kPanelHi : kPanel, 10 * s);
            if (focused) I.strokeRect(r, kGold, 3.f * s, 10 * s);
            I.text(rows[i].name, I.fmtRow.get(),
                   {r.left + 28 * s, r.top + 12 * s, r.right - 180 * s, r.bottom}, kText);
            if (!rows[i].detail.empty())
                I.text(rows[i].detail, I.fmtDetail.get(),
                       {r.left + 28 * s, r.top + 54 * s, r.right - 180 * s, r.bottom}, kDim);
            if (!rows[i].tag.empty())
                I.text(rows[i].tag, I.fmtSmallRight.get(),
                       {r.left, r.top, r.right - 28 * s, r.bottom},
                       rows[i].tag == L"new" ? kGold : kGreen);
            y += rowH + gap;
        }
    };

    if (section == 0) {
        D2D1_RECT_F r{cx, y, cx + cw, y + 210 * s};
        I.fillRect(r, kPanel, 12 * s);
        if (m.inPane) I.strokeRect(r, kGold, 3.f * s, 12 * s);
        I.text(m.streamHeadline, I.fmtRow.get(),
               {r.left + 36 * s, r.top + 34 * s, r.right - 36 * s, r.bottom}, kText);
        I.text(m.streamDetail, I.fmtDetail.get(),
               {r.left + 36 * s, r.top + 92 * s, r.right - 36 * s, r.bottom}, kDim);
        if (!m.streamHint.empty())
            I.text(m.streamHint, I.fmtSmallRight.get(),
                   {r.left, r.top, r.right - 32 * s, r.bottom - 20 * s}, kGold);
    } else if (section == 1) {
        rowList(m.hostRows, m.hostFocus);
        if (m.hostRows.empty())
            I.text(L"No hosts yet — open pairing on the host to discover it", I.fmtSmall.get(),
                   {cx, y, cx + cw, y + 60 * s}, kDim);
    } else if (section == 2) {
        rowList(m.appRows, m.appFocus);
        if (m.appRows.empty())
            I.text(m.appsEmpty, I.fmtSmall.get(), {cx, y, cx + cw, y + 60 * s}, kDim);
    } else {
        for (size_t i = 0; i < m.settingRows.size(); ++i) {
            const bool focused = m.inPane && static_cast<int>(i) == m.settingsFocus;
            D2D1_RECT_F r{cx, y, cx + cw, y + rowH};
            I.fillRect(r, focused ? kPanelHi : kPanel, 10 * s);
            if (focused) I.strokeRect(r, kGold, 3.f * s, 10 * s);
            I.text(m.settingRows[i].label, I.fmtRow.get(),
                   {r.left + 28 * s, r.top + 26 * s, r.right, r.bottom}, kText);
            const std::wstring v = focused ? L"\x2039 " + m.settingRows[i].value + L" \x203A"
                                           : m.settingRows[i].value;
            I.text(v, I.fmtSmallRight.get(), {r.left, r.top, r.right - 28 * s, r.bottom},
                   focused ? kGold : kDim);
            y += rowH + gap;
        }
    }

    if (!m.status.empty())
        I.text(m.status, I.fmtSmall.get(), {cx, I.h - 96 * s, I.w - 120 * s, I.h - 40 * s}, kDim);
    I.text(m.inPane ? L"A Select   B Rail" : L"D-pad Move   A Open", I.fmtSmallRight.get(),
           {0, I.h - 96 * s, I.w - 80 * s, I.h - 40 * s}, kDim);
    I.ctx->EndDraw();
}

void UiRenderer::drawPin(const UiPinModel& m) {
    if (!p_) return;
    auto& I = *p_;
    const float s = I.s;
    I.ctx->BeginDraw();
    I.chrome();
    I.text(m.title, I.fmtCenter.get(), {0, 210 * s, I.w, 260 * s}, kText);
    I.text(L"Open pairing on the host, then enter the PIN", I.fmtCenterSmall.get(),
           {0, 262 * s, I.w, 300 * s}, kDim);

    // Six digit boxes.
    const float bw = 74 * s, bh = 96 * s, bgap = 18 * s;
    float x0 = (I.w - (6 * bw + 5 * bgap)) / 2, yD = 320 * s;
    for (int i = 0; i < 6; ++i) {
        D2D1_RECT_F r{x0 + i * (bw + bgap), yD, x0 + i * (bw + bgap) + bw, yD + bh};
        I.fillRect(r, kPanel, 10 * s);
        if (i == static_cast<int>(m.digits.size()) && !m.busy)
            I.strokeRect(r, kGold, 2.f * s, 10 * s);
        if (i < static_cast<int>(m.digits.size()))
            I.text(std::wstring(1, m.digits[i]), I.fmtDigit.get(), r, kText);
    }

    // 3x4 keypad.
    const float kw = 120 * s, kh = 88 * s, kgap = 16 * s;
    float kx0 = (I.w - (3 * kw + 2 * kgap)) / 2, ky0 = 470 * s;
    for (int i = 0; i < 12; ++i) {
        const int cx = i % 3, cy = i / 3;
        D2D1_RECT_F r{kx0 + cx * (kw + kgap), ky0 + cy * (kh + kgap),
                      kx0 + cx * (kw + kgap) + kw, ky0 + cy * (kh + kgap) + kh};
        const bool focused = i == m.focus && !m.busy;
        I.fillRect(r, focused ? kPanelHi : kPanel, 10 * s);
        if (focused) I.strokeRect(r, kGold, 3.f * s, 10 * s);
        I.text(kKeyLabels[i], I.fmtKey.get(), r, i == 11 ? kGreen : kText);
    }

    std::wstring status = m.busy ? L"Pairing\x2026" : m.status;
    if (!status.empty())
        I.text(status, I.fmtCenter.get(), {0, ky0 + 4 * (kh + kgap), I.w, ky0 + 4 * (kh + kgap) + 60 * s},
               m.busy ? kGold : kRed);
    I.text(L"B Back", I.fmtSmallRight.get(), {0, I.h - 96 * s, I.w - 120 * s, I.h - 40 * s}, kDim);
    I.ctx->EndDraw();
}

void UiRenderer::drawOverlay(const UiOverlayModel& m) {
    if (!p_) return;
    auto& I = *p_;
    const float s = I.s;
    I.ctx->BeginDraw();
    D2D1_RECT_F r{40 * s, 40 * s, 40 * s + 760 * s, 40 * s + 66 * s};
    I.fillRect(r, kScrim, 12 * s);
    if (m.degraded) I.strokeRect(r, kRed, 2.f * s, 12 * s);
    I.text(m.line, I.fmtSmall.get(), {r.left + 24 * s, r.top, r.right - 24 * s, r.bottom}, kText);
    I.ctx->EndDraw();
}

void UiRenderer::drawStreamMenu(const UiMenuModel& m) {
    if (!p_) return;
    auto& I = *p_;
    const float s = I.s;
    I.ctx->BeginDraw();
    I.fillRect({0, 0, I.w, I.h}, kScrim);  // dim the live video behind

    // Scroll window: show up to 7 rows, keeping the focused one in view.
    const int total = static_cast<int>(m.items.size());
    const int shown = std::min(total, 7);
    int off = std::clamp(m.focus - 3, 0, std::max(0, total - shown));

    const float pw = 620 * s, rowH = 88 * s, gap = 14 * s;
    const float ph = 120 * s + shown * (rowH + gap) + 40 * s;
    const float px = (I.w - pw) / 2, py = (I.h - ph) / 2;
    I.fillRect({px, py, px + pw, py + ph}, kBg, 16 * s);
    I.strokeRect({px, py, px + pw, py + ph}, kGold, 2.f * s, 16 * s);
    I.text(m.title, I.fmtCenter.get(), {px, py + 20 * s, px + pw, py + 90 * s}, kGold);

    float y = py + 110 * s;
    for (int i = off; i < off + shown; ++i) {
        const bool focused = i == m.focus;
        D2D1_RECT_F r{px + 32 * s, y, px + pw - 32 * s, y + rowH};
        I.fillRect(r, focused ? kPanelHi : kPanel, 10 * s);
        if (focused) I.strokeRect(r, kGold, 3.f * s, 10 * s);
        I.text(m.items[static_cast<size_t>(i)], I.fmtCenter.get(), r, kText);
        y += rowH + gap;
    }
    I.text(m.hint, I.fmtCenterSmall.get(), {px, y, px + pw, y + 40 * s}, kDim);
    I.ctx->EndDraw();
}

void UiRenderer::drawSettings(const UiSettingsModel& m) {
    if (!p_) return;
    auto& I = *p_;
    const float s = I.s;
    I.ctx->BeginDraw();
    I.fillRect({0, 0, I.w, I.h}, kScrim);

    const float pw = 720 * s, rowH = 92 * s, gap = 14 * s;
    const float ph = 120 * s + m.rows.size() * (rowH + gap) + 70 * s;
    const float px = (I.w - pw) / 2, py = (I.h - ph) / 2;
    I.fillRect({px, py, px + pw, py + ph}, kBg, 16 * s);
    I.strokeRect({px, py, px + pw, py + ph}, kGold, 2.f * s, 16 * s);
    I.text(L"SETTINGS", I.fmtCenter.get(), {px, py + 20 * s, px + pw, py + 90 * s}, kGold);

    float y = py + 110 * s;
    for (size_t i = 0; i < m.rows.size(); ++i) {
        const bool focused = static_cast<int>(i) == m.focus;
        D2D1_RECT_F r{px + 32 * s, y, px + pw - 32 * s, y + rowH};
        I.fillRect(r, focused ? kPanelHi : kPanel, 10 * s);
        if (focused) I.strokeRect(r, kGold, 3.f * s, 10 * s);
        I.text(m.rows[i].label, I.fmtRow.get(),
               {r.left + 28 * s, r.top + 26 * s, r.right, r.bottom}, kText);
        const std::wstring v =
            focused ? L"\x2039 " + m.rows[i].value + L" \x203A" : m.rows[i].value;
        I.text(v, I.fmtSmallRight.get(), {r.left, r.top, r.right - 28 * s, r.bottom},
               focused ? kGold : kDim);
        y += rowH + gap;
    }
    I.text(m.hint.empty() ? L"D-pad Adjust   B Back" : m.hint, I.fmtCenterSmall.get(),
           {px, y + 8 * s, px + pw, y + 48 * s}, kDim);
    I.ctx->EndDraw();
}

void UiRenderer::shutdown() {
    delete p_;
    p_ = nullptr;
}
