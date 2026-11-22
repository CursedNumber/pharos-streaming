// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Audio tab — the host's audio codecs + routing.
//!
//! Moved here from the Video tab: the audio codec strip (Opus today, AAC-ELD as
//! an opt-in for Apple clients) plus the per-session sink routing the host now
//! uses. Follows the panel-wide directives: strings via `crate::i18n::tr` where
//! a key exists, every element `.describe()`d for hover + screen readers.

use egui::{RichText, Rounding, Stroke};
use egui_phosphor::regular as ph;

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::PanelState;
use crate::theme;

/// Tab entry point. `app.rs` calls exactly this.
pub fn ui(ui: &mut egui::Ui, _state: &mut PanelState) {
    ui.heading(tr("audio.heading"));
    ui.label(RichText::new(tr("audio.intro")).weak())
        .describe(tr("audio.intro_desc"));
    ui.add_space(10.0);

    theme::section(
        ui,
        ph::SPEAKER_HIGH,
        "Audio codec",
        "How the host encodes audio and ships it to the client.",
    );
    ui.add_space(4.0);

    // The live codec: Opus, in-band over the same QUIC connection as video.
    codec_card(
        ui,
        "Opus",
        CardState::Active,
        "48 kHz stereo · 20 ms frames · ~96 kbps",
        "Low-delay, carried in-band over the video QUIC connection. The project default — best quality at this latency, decodes everywhere.",
        Some("Validated on the Bravia (X90J) + desktop"),
    );

    // Opt-in alternative for Apple clients.
    codec_card(
        ui,
        "AAC-ELD",
        CardState::OptIn,
        "48 kHz stereo · ~10 ms frames · AOT 39",
        "Opt-in for Apple clients (iOS/macOS) — native AudioToolbox decode. Host encode is libfdk, a feature-gated tool you provide (never bundled). Lowest-delay AAC, but a software path on Apple. Opus stays the default everywhere else.",
        None,
    );

    ui.add_space(10.0);
    theme::section(
        ui,
        ph::WAVE_SINE,
        "Routing",
        "Where the streamed audio comes from on the host.",
    );
    ui.add_space(4.0);
    crate::widgets::card(ui, "audio_routing", |ui| {
        ui.label(
            RichText::new(
                "Apps launched into the virtual display are routed to a per-session null sink \
                 (PULSE_SINK); the host captures that sink's monitor. So the game's audio is \
                 streamed, the physical desktop's audio (a YouTube tab, notifications) stays off \
                 the wire, and the couch game doesn't play out the host's speakers.",
            )
            .small()
            .weak(),
        )
        .describe(
            "The audio-routing model: game audio is isolated to a dedicated sink and captured; \
             desktop audio no longer leaks to the TV.",
        );
    });
}

#[derive(Clone, Copy, PartialEq)]
enum CardState {
    /// The live audio codec right now.
    Active,
    /// Available as an opt-in, not active.
    OptIn,
}

/// One audio-codec card, mirroring the Video tab's card language: full streaming
/// highlight for the live codec, a faint border for an opt-in one.
fn codec_card(
    ui: &mut egui::Ui,
    name: &str,
    state: CardState,
    meta: &str,
    desc: &str,
    validated_on: Option<&str>,
) {
    let active = state == CardState::Active;
    let stroke = if active {
        Stroke::new(2.5_f32, theme::streaming())
    } else {
        Stroke::new(1.0_f32, theme::available().gamma_multiply(0.8))
    };
    let fill = if active {
        theme::streaming().gamma_multiply(0.10)
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };

    egui::Frame::group(ui.style())
        .rounding(Rounding::same(10.0))
        .inner_margin(egui::Margin::same(10.0))
        .fill(fill)
        .stroke(stroke)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(name).strong());
                if active {
                    ui.label(RichText::new("★").color(theme::validated()).small())
                        .describe_named(name.to_string(), "The live audio codec.");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let wash = ui.visuals().faint_bg_color;
                    let (txt, col) = if active {
                        ("streaming", theme::streaming())
                    } else {
                        ("opt-in", theme::available())
                    };
                    ui.label(
                        RichText::new(txt)
                            .color(col)
                            .small()
                            .strong()
                            .background_color(wash),
                    );
                });
            });
            ui.add_space(3.0);
            ui.label(RichText::new(meta).small().weak());
            ui.add_space(2.0);
            ui.label(RichText::new(desc).small().weak());
            if let Some(on) = validated_on {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!("★ {on}"))
                        .small()
                        .weak()
                        .color(theme::validated()),
                );
            }
        })
        .response
        .describe(format!("{name} — {desc}"));
    ui.add_space(6.0);
}
