// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Bundled fonts, so glyphs render identically on every platform.
//!
//! egui's built-in fonts only cover a Latin subset — arrows (`→`), technical
//! symbols, and emoji all fall back to missing-glyph boxes (tofu). We bundle:
//!
//! - **Phosphor** (via `egui-phosphor`, shipped inside that crate): the panel's
//!   universal icon set. Use `egui_phosphor::regular::*` constants for icons
//!   instead of emoji (which have no monochrome coverage we can bundle).
//! - **Noto Sans Symbols** + **Noto Sans Symbols 2** (OFL; in `assets/fonts/`):
//!   text fallbacks that cover prose symbols — arrows `→ ↔`, gear `⚙`, keyboard
//!   `⌨`, braille `⠿`, geometric `◉`, dingbats `★ ● ✓ ✕`, etc.
//!
//! Call [`install`] once at startup (before the first frame).

/// Register the bundled fonts on the egui context. Idempotent; call once.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Phosphor icons (appends its font to the families + registers the data).
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    // Noto symbol fallbacks for non-icon prose glyphs.
    fonts.font_data.insert(
        "noto_symbols".to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSansSymbols-Regular.ttf"
        )),
    );
    fonts.font_data.insert(
        "noto_symbols2".to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSansSymbols2-Regular.ttf"
        )),
    );

    // Append the symbol fonts as fallbacks to both families so any glyph the
    // primary font lacks is picked up here rather than rendering as tofu.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        list.push("noto_symbols".to_owned());
        list.push("noto_symbols2".to_owned());
    }

    ctx.set_fonts(fonts);
}
