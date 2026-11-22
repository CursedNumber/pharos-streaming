// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Theming: explicit, contrast-controlled palettes + the status/accent colours.
//!
//! Unlike the Trireme client (which honours `follow_system_theme`), the panel
//! ships **four explicit themes** so text is always legible — the system theme
//! tended to give grey-on-grey secondary text. The active [`crate::model::Theme`]
//! is published here via an atomic so the accent helpers (`online()`, `warn()`,
//! `perf_color()`, …) can resolve per-theme colours without threading the theme
//! through every call site. `app.rs` calls [`set_active`] + applies [`visuals`]
//! whenever the user changes the theme.

use std::sync::atomic::{AtomicU8, Ordering};

use egui::{Color32, Stroke};

use crate::a11y::Describe;
use crate::model::Theme;

#[inline]
const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

// --- active-theme plumbing -------------------------------------------------

static ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Publish the active theme (called by `app.rs` when it changes). Lets the
/// accent helpers below resolve the right palette with no extra arguments.
pub fn set_active(theme: Theme) {
    ACTIVE.store(theme as u8, Ordering::Relaxed);
}

fn active() -> Theme {
    Theme::from_u8(ACTIVE.load(Ordering::Relaxed))
}

// --- per-theme accent palette ----------------------------------------------

/// Status + capability accents for one theme. Kept together so [`visuals`] and
/// the public accent helpers stay in sync.
struct Palette {
    online: Color32,
    streaming: Color32,
    warn: Color32,
    info: Color32,
    error: Color32,
    validated: Color32,
    available: Color32,
    unavailable: Color32,
    /// 1..=6 performance ramp (excellent → very poor).
    perf: [Color32; 6],
}

fn palette(theme: Theme) -> Palette {
    match theme {
        // Dark backgrounds (Comfort + Midnight) share bright, saturated accents.
        Theme::Comfort | Theme::Midnight => Palette {
            online: rgb(0x55, 0xc0, 0x70),
            streaming: rgb(0x66, 0xd9, 0x86),
            warn: rgb(0xe2, 0xb2, 0x44),
            info: rgb(0x6f, 0xb2, 0xff),
            error: rgb(0xf0, 0x72, 0x72),
            validated: rgb(0x55, 0xc0, 0x70),
            available: rgb(0x66, 0xc2, 0xcf),
            unavailable: rgb(0x9a, 0x9d, 0xa5),
            perf: [
                rgb(0x55, 0xc8, 0x78),
                rgb(0x8e, 0xcf, 0x66),
                rgb(0x66, 0xc2, 0xcf),
                rgb(0xe2, 0xb2, 0x44),
                rgb(0xe8, 0x86, 0x55),
                rgb(0xee, 0x66, 0x66),
            ],
        },
        // Light background needs darker accents so coloured text stays readable.
        Theme::Daylight => Palette {
            online: rgb(0x1e, 0x7e, 0x3c),
            streaming: rgb(0x13, 0x8a, 0x45),
            warn: rgb(0x97, 0x60, 0x05),
            info: rgb(0x1b, 0x5f, 0xc8),
            error: rgb(0xc2, 0x29, 0x29),
            validated: rgb(0x1e, 0x7e, 0x3c),
            available: rgb(0x14, 0x73, 0x88),
            unavailable: rgb(0x5d, 0x60, 0x67),
            perf: [
                rgb(0x1e, 0x7e, 0x3c),
                rgb(0x4a, 0x7d, 0x1f),
                rgb(0x14, 0x73, 0x88),
                rgb(0x97, 0x60, 0x05),
                rgb(0xbd, 0x4e, 0x1a),
                rgb(0xc2, 0x29, 0x29),
            ],
        },
        // High contrast: colourblind-safe Okabe–Ito accents on pure black.
        Theme::HighContrast => Palette {
            online: rgb(0x00, 0xc8, 0x8e),
            streaming: rgb(0x35, 0xe2, 0xaa),
            warn: rgb(0xff, 0xb0, 0x00),
            info: rgb(0x6f, 0xc6, 0xff),
            error: rgb(0xff, 0x7a, 0x33),
            validated: rgb(0x00, 0xc8, 0x8e),
            available: rgb(0x8c, 0xd3, 0xff),
            unavailable: rgb(0xc8, 0xc8, 0xc8),
            perf: [
                rgb(0x6f, 0xc6, 0xff),
                rgb(0x00, 0xc8, 0x8e),
                rgb(0xf5, 0xe1, 0x4b),
                rgb(0xff, 0xb0, 0x00),
                rgb(0xff, 0x7a, 0x33),
                rgb(0xff, 0x55, 0x55),
            ],
        },
    }
}

// --- public accent helpers (theme-aware) -----------------------------------

/// Online / OK.
pub fn online() -> Color32 {
    palette(active()).online
}
/// Actively streaming.
pub fn streaming() -> Color32 {
    palette(active()).streaming
}
/// Warning.
pub fn warn() -> Color32 {
    palette(active()).warn
}
/// Info / discovered.
pub fn info() -> Color32 {
    palette(active()).info
}
/// Error.
pub fn error() -> Color32 {
    palette(active()).error
}
/// Validated-on-our-hardware.
pub fn validated() -> Color32 {
    palette(active()).validated
}
/// Available but unvalidated.
pub fn available() -> Color32 {
    palette(active()).available
}
/// Unavailable / disabled.
pub fn unavailable() -> Color32 {
    palette(active()).unavailable
}

/// Map a 1..=6 performance rank to a colour on the active theme's ramp
/// (excellent → very poor). Out-of-range ranks clamp to "very poor".
pub fn perf_color(rank: u8) -> Color32 {
    let idx = (rank.clamp(1, 6) - 1) as usize;
    palette(active()).perf[idx]
}

/// Human label for a 1..=6 performance rank.
pub fn perf_label(rank: u8) -> &'static str {
    match rank {
        1 => "Excellent",
        2 => "Very good",
        3 => "Good",
        4 => "Fair",
        5 => "Poor",
        _ => "Very poor",
    }
}

// --- shared widget helpers -------------------------------------------------

/// A small coloured pill badge with a faint background wash. Returns the painted
/// [`egui::Response`] so callers can attach an a11y description
/// (`.describe()` / `.describe_named()`) — screen readers can then name it.
pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let wash = ui.visuals().faint_bg_color;
    ui.label(
        egui::RichText::new(text)
            .color(color)
            .small()
            .background_color(wash),
    )
}

/// A bold section header followed by a thin separator.
pub fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(text).strong().size(15.0));
    ui.separator();
}

/// A section header with an optional leading icon (a Phosphor glyph), where the
/// generous description is attached to the **title** as hover + screen-reader
/// text — it is *not* rendered inline. The panel's standard pattern: the visible
/// UI stays concise; the explanation appears only on hover / for assistive tech.
/// Pass already-localised strings (`icon` may be empty).
pub fn section(ui: &mut egui::Ui, icon: &str, title: &str, desc: &str) {
    ui.add_space(4.0);
    let label = if icon.is_empty() {
        title.to_string()
    } else {
        format!("{icon}  {title}")
    };
    ui.label(egui::RichText::new(label).strong().size(15.0))
        .describe(desc);
    ui.separator();
}

// --- per-theme egui Visuals ------------------------------------------------

/// Build the egui [`egui::Visuals`] for a theme. The key levers for legibility:
/// `noninteractive.fg_stroke.color` (base text) and
/// `noninteractive.weak_bg_fill` (egui derives weak/secondary text as the
/// midpoint between the base text colour and *that* — so we keep it close enough
/// to the text that `.weak()` labels never sink into the background).
pub fn visuals(theme: Theme) -> egui::Visuals {
    let p = palette(theme);
    let white = rgb(0xff, 0xff, 0xff);
    let mut v = if theme.is_light() {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };

    v.hyperlink_color = p.info;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.error;

    match theme {
        Theme::Comfort => {
            v.panel_fill = rgb(0x2c, 0x2e, 0x33);
            v.window_fill = rgb(0x33, 0x35, 0x3b);
            v.extreme_bg_color = rgb(0x1f, 0x21, 0x25);
            v.code_bg_color = rgb(0x26, 0x28, 0x2d);
            v.faint_bg_color = rgb(0x3d, 0x40, 0x47);
            v.widgets.noninteractive.fg_stroke.color = rgb(0xf3, 0xf4, 0xf7);
            // Brighter weak-text floor: egui makes `.weak()` text the midpoint of
            // the base text and this, so a lighter value keeps secondary text legible.
            v.widgets.noninteractive.weak_bg_fill = rgb(0x7a, 0x7e, 0x87);
            v.widgets.inactive.fg_stroke.color = rgb(0xe6, 0xe8, 0xed);
            v.widgets.inactive.bg_fill = rgb(0x3a, 0x3d, 0x44);
            v.widgets.inactive.weak_bg_fill = rgb(0x34, 0x37, 0x3d);
            v.widgets.hovered.fg_stroke.color = white;
            v.widgets.hovered.bg_fill = rgb(0x45, 0x49, 0x52);
            v.widgets.active.fg_stroke.color = white;
            v.widgets.active.bg_fill = rgb(0x51, 0x57, 0x61);
            v.selection.bg_fill = rgb(0x2f, 0x5a, 0x86);
        }
        Theme::Midnight => {
            v.panel_fill = rgb(0x14, 0x15, 0x18);
            v.window_fill = rgb(0x1b, 0x1c, 0x20);
            v.extreme_bg_color = rgb(0x0c, 0x0d, 0x0f);
            v.code_bg_color = rgb(0x16, 0x17, 0x1a);
            v.faint_bg_color = rgb(0x25, 0x27, 0x2c);
            v.widgets.noninteractive.fg_stroke.color = rgb(0xf2, 0xf3, 0xf7);
            v.widgets.noninteractive.weak_bg_fill = rgb(0x72, 0x76, 0x7e);
            v.widgets.inactive.fg_stroke.color = rgb(0xe4, 0xe6, 0xec);
            v.widgets.inactive.bg_fill = rgb(0x22, 0x24, 0x29);
            v.widgets.inactive.weak_bg_fill = rgb(0x1e, 0x20, 0x25);
            v.widgets.hovered.fg_stroke.color = white;
            v.widgets.hovered.bg_fill = rgb(0x2c, 0x2f, 0x35);
            v.widgets.active.fg_stroke.color = white;
            v.widgets.active.bg_fill = rgb(0x37, 0x3b, 0x42);
            v.selection.bg_fill = rgb(0x28, 0x52, 0x80);
        }
        Theme::Daylight => {
            let ink = rgb(0x18, 0x1a, 0x1f);
            v.panel_fill = rgb(0xf4, 0xf5, 0xf7);
            v.window_fill = rgb(0xfd, 0xfd, 0xff);
            v.extreme_bg_color = rgb(0xff, 0xff, 0xff);
            v.code_bg_color = rgb(0xec, 0xee, 0xf1);
            v.faint_bg_color = rgb(0xe6, 0xe8, 0xec);
            v.widgets.noninteractive.fg_stroke.color = ink;
            v.widgets.noninteractive.weak_bg_fill = rgb(0xb6, 0xba, 0xc2);
            v.widgets.inactive.fg_stroke.color = rgb(0x24, 0x27, 0x2d);
            v.widgets.inactive.bg_fill = rgb(0xe9, 0xeb, 0xee);
            v.widgets.inactive.weak_bg_fill = rgb(0xdf, 0xe1, 0xe5);
            v.widgets.hovered.fg_stroke.color = rgb(0x00, 0x00, 0x00);
            v.widgets.hovered.bg_fill = rgb(0xde, 0xe1, 0xe6);
            v.widgets.active.fg_stroke.color = rgb(0x00, 0x00, 0x00);
            v.widgets.active.bg_fill = rgb(0xd2, 0xd6, 0xdd);
            v.selection.bg_fill = rgb(0xb6, 0xd4, 0xff);
            v.selection.stroke = Stroke::new(1.0_f32, rgb(0x10, 0x10, 0x10));
        }
        Theme::HighContrast => {
            v.dark_mode = true;
            v.override_text_color = Some(white);
            v.panel_fill = rgb(0x00, 0x00, 0x00);
            v.window_fill = rgb(0x00, 0x00, 0x00);
            v.extreme_bg_color = rgb(0x00, 0x00, 0x00);
            v.code_bg_color = rgb(0x10, 0x10, 0x10);
            v.faint_bg_color = rgb(0x1c, 0x1c, 0x1c);
            // Strong borders carry structure when colour is minimised.
            v.window_stroke = Stroke::new(1.0_f32, rgb(0x9a, 0x9a, 0x9a));
            v.widgets.noninteractive.fg_stroke.color = white;
            v.widgets.noninteractive.weak_bg_fill = rgb(0xc4, 0xc4, 0xc4);
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, rgb(0x80, 0x80, 0x80));
            v.widgets.inactive.fg_stroke.color = white;
            v.widgets.inactive.bg_fill = rgb(0x12, 0x12, 0x12);
            // Darker than `bg_fill` so the slider rail (which egui paints with
            // `bg_fill`) stays visible on a card (cards fill with `weak_bg_fill`).
            v.widgets.inactive.weak_bg_fill = rgb(0x08, 0x08, 0x08);
            v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, rgb(0xa0, 0xa0, 0xa0));
            v.widgets.hovered.fg_stroke.color = white;
            v.widgets.hovered.bg_fill = rgb(0x2a, 0x2a, 0x2a);
            v.widgets.hovered.bg_stroke = Stroke::new(2.0_f32, white);
            v.widgets.active.fg_stroke.color = white;
            v.widgets.active.bg_fill = rgb(0x3a, 0x3a, 0x3a);
            v.widgets.active.bg_stroke = Stroke::new(2.0_f32, white);
            v.selection.bg_fill = rgb(0x00, 0x5a, 0xb0);
            v.selection.stroke = Stroke::new(1.0_f32, white);
        }
    }

    // Make sliders legible everywhere: egui draws the slider rail with
    // `widgets.inactive.bg_fill` — the same colour our cards fill with — so the
    // track vanished into the card. `card()` now fills with `inactive.weak_bg_fill`
    // (an adjacent shade) so the rail contrasts, and trailing-fill paints the
    // value portion in the theme accent (`selection.bg_fill`). Both are per-theme,
    // so this stays correct in all four themes.
    v.slider_trailing_fill = true;

    v
}
