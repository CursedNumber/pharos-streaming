// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Input tab — keyboard / mouse / HID forwarding policy.
//!
//! Most of this tab is the connecting Trireme *client's* input-forwarding config
//! (`crate::model::InputCfgUi`, persisted to `panel.toml` as a saved reference —
//! there is no host→client config push yet), grouped under a "Client input
//! defaults" heading after a banner that makes the host-vs-client split explicit:
//! capture mode, which classes forward, the safety chords, the client hotkeys.
//! The genuinely host-side part — the OS injector that receives whatever a client
//! forwards — is grouped separately under "This host", followed by a collapsed
//! reference of the wire message kinds.
//!
//! Per the panel-wide universal-design directive, the *visible* text on every
//! control is short; the generous explanation lives in `.describe(…)` — shown on
//! hover and announced by screen readers. Section descriptions ride on their
//! title via `theme::section`, never as a duplicated visible paragraph. Gamepad
//! / gyro tuning lives in the Controllers tab; we cross-link instead of
//! duplicating. Every user-facing string is keyed through `crate::i18n::tr`
//! (`input.*`) into `locales/input.toml`; dynamic values stay Rust-formatted.

use egui_phosphor::regular as ph;

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::{InjectorHealth, InputCfgUi, PanelState};
use crate::theme;

/// Tab entry point. `app.rs` calls exactly this.
pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    ui.heading(tr("input.heading"))
        .describe(tr("input.intro_desc"));
    ui.add_space(6.0);

    // Reframe banner (the host-vs-client split is the whole point of this tab):
    // the forwarding policy / safety chords / hotkeys below describe the connecting
    // *client's* behaviour, not this host — and the host doesn't push them to
    // clients yet, so they're a saved reference. The one genuinely host-side item
    // is the injector status, grouped separately below.
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("{}  {}", ph::INFO, tr("input.client_banner"))).small(),
            )
            .describe(tr("input.client_banner_desc"));
        });
    ui.add_space(8.0);

    // --- Client input defaults (configure the remote player, not this host) ---
    ui.label(
        egui::RichText::new(tr("input.client_section"))
            .strong()
            .small(),
    )
    .describe(tr("input.client_section_desc"));
    ui.add_space(4.0);

    forwarding_policy_card(ui, state);
    ui.add_space(10.0);

    local_escapes_card(ui, state);
    ui.add_space(10.0);

    hotkeys_card(ui, state);
    ui.add_space(10.0);

    save_bar(ui, state);
    ui.add_space(12.0);

    // --- This host (the OS injector that receives whatever the client forwards) ---
    ui.label(
        egui::RichText::new(tr("input.host_section"))
            .strong()
            .small(),
    )
    .describe(tr("input.host_section_desc"));
    ui.add_space(4.0);

    injection_backend_card(ui);
    ui.add_space(10.0);

    // Reference / navigation (neutral).
    event_reference_card(ui);
    ui.add_space(10.0);

    gamepad_crosslink(ui, state);
}

// ---------------------------------------------------------------------------
// Forwarding policy (mode + per-class toggles) — the main settings
// ---------------------------------------------------------------------------

/// Localised short menu label for each capture mode (`InputCfgUi::mode`
/// strings). Unknown modes loaded from `panel.toml` fall back to a "custom" key.
fn mode_label(mode: &str) -> String {
    match mode {
        "focus" | "toggle" | "always" | "off" => tr(&format!("input.mode.{mode}")),
        _ => tr("input.mode.custom"),
    }
}

/// Localised generous description (hover + screen reader) for each capture mode.
fn mode_describe(mode: &str) -> String {
    match mode {
        "focus" | "toggle" | "always" | "off" => tr(&format!("input.mode.{mode}_desc")),
        _ => tr("input.mode.custom_desc"),
    }
}

fn forwarding_policy_card(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "input_policy", |ui| {
        theme::section(
            ui,
            ph::KEYBOARD,
            &tr("input.policy.heading"),
            &tr("input.policy.heading_desc"),
        );

        ui.horizontal(|ui| {
            ui.label(tr("input.policy.mode_label"));
            let cur = state.input_cfg.mode.clone();
            egui::ComboBox::from_id_salt("input_mode_combo")
                .selected_text(mode_label(&cur))
                .show_ui(ui, |ui| {
                    for m in InputCfgUi::MODES {
                        if ui
                            .selectable_value(
                                &mut state.input_cfg.mode,
                                m.to_string(),
                                mode_label(m),
                            )
                            .describe(mode_describe(m))
                            .changed()
                        {
                            state.dirty_input = true;
                        }
                    }
                })
                .response
                .describe(tr("input.policy.mode_combo_desc"));
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        ui.label(egui::RichText::new(tr("input.policy.classes_label")).strong())
            .describe(tr("input.policy.classes_desc"));
        ui.add_space(2.0);

        if ui
            .checkbox(
                &mut state.input_cfg.forward_keyboard,
                tr("input.policy.keyboard"),
            )
            .describe(tr("input.policy.keyboard_desc"))
            .changed()
        {
            state.dirty_input = true;
        }
        if ui
            .checkbox(&mut state.input_cfg.forward_mouse, tr("input.policy.mouse"))
            .describe(tr("input.policy.mouse_desc"))
            .changed()
        {
            state.dirty_input = true;
        }
        if ui
            .checkbox(
                &mut state.input_cfg.forward_scroll,
                tr("input.policy.scroll"),
            )
            .describe(tr("input.policy.scroll_desc"))
            .changed()
        {
            state.dirty_input = true;
        }

        if state.input_cfg.mode == "off" {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("{}  {}", ph::WARNING, tr("input.policy.off_warn")))
                    .small()
                    .color(theme::warn()),
            )
            .describe(tr("input.policy.off_warn_desc"));
        }
    });
}

// ---------------------------------------------------------------------------
// Local escape chords
// ---------------------------------------------------------------------------

fn local_escapes_card(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "input_escapes", |ui| {
        theme::section(
            ui,
            ph::LOCK,
            &tr("input.escapes.heading"),
            &tr("input.escapes.heading_desc"),
        );
        ui.add_space(6.0);

        let mut remove: Option<usize> = None;
        if state.input_cfg.local_escapes.is_empty() {
            ui.label(
                egui::RichText::new(tr("input.escapes.empty"))
                    .small()
                    .italics()
                    .color(theme::warn()),
            )
            .describe(tr("input.escapes.empty_desc"));
        } else {
            for (i, chord) in state.input_cfg.local_escapes.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(egui::RichText::new(ph::TRASH).small()).small())
                        .describe_named(
                            tr("input.escapes.remove_name").replace("{chord}", chord),
                            tr("input.escapes.remove_desc"),
                        )
                        .clicked()
                    {
                        remove = Some(i);
                    }
                    // Flag a malformed (hand-edited) chord red so a dead binding is
                    // visible, not silent.
                    let valid = core_proto::chord::is_valid(chord);
                    let col = if valid { theme::info() } else { theme::error() };
                    let badge = crate::widgets::status_badge(ui, (chord.as_str(), col));
                    if !valid {
                        badge.describe(tr("input.escapes.invalid_desc"));
                    }
                });
            }
        }
        if let Some(i) = remove {
            state.input_cfg.local_escapes.remove(i);
            state.dirty_input = true;
        }

        ui.add_space(6.0);

        let id = egui::Id::new("input_escape_new");
        let mut buf: String = ui
            .data_mut(|d| d.get_temp::<String>(id))
            .unwrap_or_default();
        let trimmed = buf.trim().to_string();
        // Empty = nothing typed (no hint); a non-empty entry must be a structurally
        // valid chord before it can be added, so a typo can't become a dead binding.
        let valid = core_proto::chord::is_valid(&trimmed);
        ui.horizontal(|ui| {
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut buf)
                        .hint_text(tr("input.escapes.add_hint"))
                        .desired_width(160.0),
                )
                .describe(tr("input.escapes.add_field_desc"));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let add = ui
                .add_enabled(valid, egui::Button::new(tr("input.escapes.add")))
                .describe(tr("input.escapes.add_desc"))
                .clicked();
            if (enter || add) && valid {
                let chord = buf.trim().to_string();
                if !chord.is_empty() && !state.input_cfg.local_escapes.iter().any(|c| c == &chord) {
                    state.input_cfg.local_escapes.push(chord);
                    state.dirty_input = true;
                }
                buf.clear();
                resp.request_focus();
            }
        });
        if !trimmed.is_empty() && !valid {
            ui.colored_label(theme::error(), tr("input.escapes.invalid"))
                .describe(tr("input.escapes.invalid_desc"));
        }
        ui.data_mut(|d| d.insert_temp(id, buf));

        ui.add_space(4.0);
        if ui
            .button(tr("input.escapes.reset"))
            .describe(tr("input.escapes.reset_desc"))
            .clicked()
        {
            state.input_cfg.local_escapes = InputCfgUi::default().local_escapes;
            state.dirty_input = true;
        }
    });
}

// ---------------------------------------------------------------------------
// Client hotkeys
// ---------------------------------------------------------------------------

fn hotkeys_card(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "input_hotkeys", |ui| {
        theme::section(
            ui,
            ph::KEYBOARD,
            &tr("input.hotkeys.heading"),
            &tr("input.hotkeys.heading_desc"),
        );
        ui.add_space(6.0);

        egui::Grid::new("input_hotkey_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                hotkey_row(
                    ui,
                    state,
                    &tr("input.hotkeys.toggle_capture"),
                    &tr("input.hotkeys.toggle_capture_desc"),
                    |c| &mut c.toggle_capture,
                );
                hotkey_row(
                    ui,
                    state,
                    &tr("input.hotkeys.toggle_fullscreen"),
                    &tr("input.hotkeys.toggle_fullscreen_desc"),
                    |c| &mut c.toggle_fullscreen,
                );
                hotkey_row(
                    ui,
                    state,
                    &tr("input.hotkeys.disconnect"),
                    &tr("input.hotkeys.disconnect_desc"),
                    |c| &mut c.disconnect,
                );
            });
    });
}

/// One labelled hotkey text field row inside the grid.
fn hotkey_row(
    ui: &mut egui::Ui,
    state: &mut PanelState,
    label: &str,
    desc: &str,
    field: impl Fn(&mut InputCfgUi) -> &mut String,
) {
    ui.label(label).describe(desc.to_owned());
    ui.horizontal(|ui| {
        let buf = field(&mut state.input_cfg);
        let resp = ui
            .add(
                egui::TextEdit::singleline(buf)
                    .desired_width(170.0)
                    .hint_text(tr("input.hotkeys.field_hint")),
            )
            .describe(format!("{desc} {}", tr("input.hotkeys.field_desc")));
        if resp.changed() {
            state.dirty_input = true;
        }
        // A non-empty but malformed chord gets a red marker (empty = unset = fine).
        let v = field(&mut state.input_cfg).trim().to_string();
        if !v.is_empty() && !core_proto::chord::is_valid(&v) {
            ui.label(egui::RichText::new(ph::WARNING).color(theme::error()))
                .describe(tr("input.hotkeys.invalid_desc"));
        }
    });
    ui.end_row();
}

// ---------------------------------------------------------------------------
// Save + revert
// ---------------------------------------------------------------------------

fn save_bar(ui: &mut egui::Ui, state: &mut PanelState) {
    // Saving is global now (the Save-all button in the bottom bar). This row is
    // only the per-tab *discard*: roll these client defaults back to what's on disk.
    if ui
        .add_enabled(
            state.dirty_input,
            egui::Button::new(tr("input.save.revert")),
        )
        .describe(tr("input.save.revert_desc"))
        .clicked()
    {
        state.input_cfg = crate::data::load_input_cfg();
        state.dirty_input = false;
        state.status = Some(tr("input.save.reverted"));
    }
}

// ---------------------------------------------------------------------------
// Injection backend status (read-only)
// ---------------------------------------------------------------------------

/// Which OS-level injection backend the host binary uses, chosen at compile
/// time. The panel and host run on the same machine, so `cfg!(target_os)` here
/// reflects the real injector. `name`/`api` are short identifiers shown as-is;
/// `detail_key` resolves to the localised hover description.
struct Backend {
    name: &'static str,
    api: &'static str,
    detail_key: &'static str,
}

fn current_backend() -> Backend {
    if cfg!(target_os = "linux") {
        Backend {
            name: "uinput",
            api: "Linux /dev/uinput",
            detail_key: "input.backend.uinput_desc",
        }
    } else if cfg!(target_os = "windows") {
        Backend {
            name: "SendInput",
            api: "Windows SendInput()",
            detail_key: "input.backend.sendinput_desc",
        }
    } else if cfg!(target_os = "macos") {
        Backend {
            name: "CGEvent",
            api: "macOS CoreGraphics",
            detail_key: "input.backend.cgevent_desc",
        }
    } else {
        Backend {
            name: "none",
            api: "unsupported platform",
            detail_key: "input.backend.none_desc",
        }
    }
}

fn injection_backend_card(ui: &mut egui::Ui) {
    crate::widgets::card(ui, "input_backend", |ui| {
        let b = current_backend();
        // Live probe: is the injector actually usable (not just compiled in)?
        let health = crate::data::injector_health();
        theme::section(
            ui,
            ph::PLUGS,
            &tr("input.backend.heading"),
            &tr(b.detail_key),
        );
        ui.horizontal(|ui| {
            ui.label(tr("input.backend.active_label"));
            // Colour the badge by the probe, and describe the live state.
            let badge_desc = match health {
                InjectorHealth::Ready => tr("input.backend.health.ready"),
                InjectorHealth::Blocked(key) => tr(key),
                InjectorHealth::Unknown => tr("input.backend.health.unknown"),
            };
            crate::widgets::status_badge(ui, (b.name, health.color())).describe(badge_desc);
            ui.label(egui::RichText::new(format!("({})", b.api)).small().weak());
        });
        // When we positively know injection is blocked, surface the reason inline
        // (it's not just hover text — a broken injector silently drops all input).
        if let InjectorHealth::Blocked(key) = health {
            ui.add_space(2.0);
            ui.colored_label(theme::warn(), tr(key));
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(tr("input.backend.auto_note"))
                .small()
                .weak(),
        )
        .describe(tr(b.detail_key));
    });
}

// ---------------------------------------------------------------------------
// InputEvent reference (collapsed; informational only)
// ---------------------------------------------------------------------------

/// Wire type (`core_proto::InputEvent` variant) plus the `input.event.<key>.*`
/// stem for its localised name, one-line summary, and full detail. The wire
/// type is a code identifier shown verbatim; everything else is translated.
const EVENT_REFERENCE: &[(&str, &str)] = &[
    ("Key", "keys"),
    ("PointerAbs", "pos"),
    ("PointerRel", "motion"),
    ("PointerButton", "buttons"),
    ("Scroll", "scroll"),
];

fn event_reference_card(ui: &mut egui::Ui) {
    crate::widgets::card(ui, "input_event_reference", |ui| {
        egui::CollapsingHeader::new(egui::RichText::new(tr("input.event.heading")).strong())
            .id_salt("input_event_ref_hdr")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(tr("input.event.intro")).small().weak())
                    .describe(tr("input.event.intro_desc"));
                ui.add_space(6.0);

                for (wire, key) in EVENT_REFERENCE {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(tr(&format!("input.event.{key}.title"))).strong(),
                        );
                        ui.label(egui::RichText::new(*wire).small().weak().monospace())
                            .describe(format!(
                                "{} core_proto::InputEvent::{wire}",
                                tr("input.event.wire_prefix")
                            ));
                    });
                    ui.label(egui::RichText::new(tr(&format!("input.event.{key}.line"))).small())
                        .describe(tr(&format!("input.event.{key}.detail")));
                    ui.add_space(6.0);
                }
            });
    });
}

// ---------------------------------------------------------------------------
// Cross-link to Controllers
// ---------------------------------------------------------------------------

fn gamepad_crosslink(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "input_gamepad_hint", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(ph::GAME_CONTROLLER).size(16.0));
            ui.label(egui::RichText::new(tr("input.gamepad.lead")).small());
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(tr("input.gamepad.link"))
                            .small()
                            .strong(),
                    )
                    .small(),
                )
                .describe(tr("input.gamepad.link_desc"))
                .clicked()
            {
                state.tab = crate::model::Tab::Controllers;
            }
            ui.label(egui::RichText::new(tr("input.gamepad.tail")).small());
        });
    });
}

#[cfg(test)]
mod tests {
    use crate::i18n::tr;

    /// The injector-health reason keys reach `tr()` via a variable
    /// (`InjectorHealth::Blocked(key)`), so the static-scan completeness test can't
    /// see them — assert each resolves here.
    #[test]
    fn injector_health_keys_resolve() {
        crate::i18n::set_locale("en");
        for k in [
            "input.backend.health.ready",
            "input.backend.health.unknown",
            "input.backend.health.no_device",
            "input.backend.health.no_perm",
            "input.backend.health.error",
        ] {
            assert_ne!(tr(k), k, "missing locale key {k}");
        }
    }

    /// The probe must never panic, whatever this box's `/dev/uinput` state.
    #[test]
    fn injector_health_probe_is_total() {
        let _ = crate::data::injector_health();
    }
}
