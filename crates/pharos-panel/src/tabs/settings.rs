// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Settings tab — every configurable knob the host owns: appearance, streaming/
//! encoding tuning, security, gamepad fallback, network NIC binding, Home
//! Assistant / MQTT, and the on-disk paths. (The display resolution/refresh policy
//! moved to the Compositor tab — it only shapes how tiles size + snap on that
//! canvas, and the host never reads it.)
//!
//! Two things persist from here, to two different files:
//!   * `state.runtime`     → `panel.toml`  via [`crate::data::save_runtime_settings`]
//!   * `state.host_config` → `config.toml` via [`crate::data::save_host_config`]
//!
//! Each section that mutates one of those sets the matching `dirty_*` flag so the
//! app's bottom bar shows "unsaved changes"; the single **Save all** button in
//! that bottom bar (and save-on-quit) is the only thing that writes — there are
//! no per-tab Save buttons. All sections are collapsible so the (long) page stays
//! navigable.
//!
//! UX: section headers carry a Phosphor icon and their generous description is
//! hover-/screen-reader-only (via [`crate::theme::section`]); per-control rows
//! show a concise visible label and keep the explanation on `.describe()` /
//! `.on_hover_text()`. Every user-facing string is keyed through
//! [`crate::i18n::tr`] into `locales/settings.toml`.
//!
//! Cross-platform: no OS-specific calls — pure egui + `crate::data` / `model`.

use egui_phosphor::regular as ph;

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::{FontScale, PanelState, Theme};
use crate::theme;

/// Tab entry point (called by `app.rs`).
pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    ui.heading(tr("settings.heading"))
        .describe(tr("settings.intro_desc"));
    ui.add_space(8.0);

    // Appearance (theme + font size) leads — it changes the whole panel and is
    // the quickest readability win.
    egui::CollapsingHeader::new(
        egui::RichText::new(format!(
            "{}  {}",
            ph::PALETTE,
            tr("settings.appearance.title")
        ))
        .strong(),
    )
    .id_salt("set_appearance")
    .default_open(true)
    .show(ui, |ui| appearance_section(ui, state));
    ui.add_space(6.0);

    // Startup: "Start on boot" (freedesktop autostart) — Linux-only.
    #[cfg(unix)]
    {
        egui::CollapsingHeader::new(
            egui::RichText::new(format!("{}  {}", ph::POWER, tr("settings.startup.title")))
                .strong(),
        )
        .id_salt("set_startup")
        .default_open(true)
        .show(ui, |ui| startup_section(ui, state));
        ui.add_space(6.0);
    }

    // (Resolution / refresh policy moved to the Compositor tab — it only shapes how
    // tiles size + snap on that canvas, and the host never reads it.)
    egui::CollapsingHeader::new(
        egui::RichText::new(format!(
            "{}  {}",
            ph::FILM_STRIP,
            tr("settings.streaming.title")
        ))
        .strong(),
    )
    .id_salt("set_streaming")
    .default_open(true)
    .show(ui, |ui| streaming_section(ui, state));

    egui::CollapsingHeader::new(
        egui::RichText::new(format!("{}  {}", ph::LOCK, tr("settings.security.title"))).strong(),
    )
    .id_salt("set_security")
    .default_open(false)
    .show(ui, |ui| security_section(ui, state));

    egui::CollapsingHeader::new(
        egui::RichText::new(format!(
            "{}  {}",
            ph::GAME_CONTROLLER,
            tr("settings.gamepad.title")
        ))
        .strong(),
    )
    .id_salt("set_gamepad")
    .default_open(false)
    .show(ui, |ui| gamepad_section(ui, state));

    egui::CollapsingHeader::new(
        egui::RichText::new(format!("{}  {}", ph::GLOBE, tr("settings.network.title"))).strong(),
    )
    .id_salt("set_network")
    .default_open(false)
    .show(ui, |ui| network_section(ui, state));

    egui::CollapsingHeader::new(
        egui::RichText::new(format!("{}  {}", ph::HOUSE, tr("settings.mqtt.title"))).strong(),
    )
    .id_salt("set_mqtt")
    .default_open(false)
    .show(ui, |ui| mqtt_section(ui, state));

    egui::CollapsingHeader::new(
        egui::RichText::new(format!("{}  {}", ph::FOLDER, tr("settings.paths.title"))).strong(),
    )
    .id_salt("set_paths")
    .default_open(false)
    .show(ui, paths_section);

    ui.add_space(8.0);
}

// ---------------------------------------------------------------------------
// Startup ("Start on boot" — shared with the tray menu via control_ipc)
// ---------------------------------------------------------------------------

/// Toggle the freedesktop autostart entry that runs the host launcher at login.
/// Same backing file the tray's "Start on boot" checkbox writes.
#[cfg(unix)]
fn startup_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "startup_card", |ui| {
        let mut enabled = control_ipc::autostart_enabled();
        if ui
            .checkbox(&mut enabled, tr("settings.startup.start_on_boot"))
            .on_hover_text(tr("settings.startup.start_on_boot_desc"))
            .changed()
        {
            if let Err(e) = control_ipc::set_autostart(enabled, &crate::control::host_launcher()) {
                state.last_error = Some(format!("start-on-boot: {e}"));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Appearance (theme + font size)
// ---------------------------------------------------------------------------

fn appearance_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "appearance_card", |ui| {
        // Theme picker (selectable pills; hover shows the description). The card
        // title carries the generous explanation as hover-only text.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tr("settings.appearance.theme")).strong())
                .describe(tr("settings.appearance.desc"));
            ui.add_space(6.0);
            for t in Theme::all() {
                let selected = state.runtime.theme == t;
                if ui
                    .selectable_label(selected, t.label())
                    .describe(t.describe())
                    .clicked()
                    && state.runtime.theme != t
                {
                    state.runtime.theme = t;
                    state.dirty_runtime = true;
                }
            }
        });
        ui.add_space(8.0);

        // Font-size picker.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tr("settings.appearance.text_size")).strong());
            ui.add_space(6.0);
            for f in FontScale::all() {
                let selected = state.runtime.font_scale == f;
                let tip = match f {
                    FontScale::Small => tr("settings.appearance.text_size_small_desc"),
                    FontScale::Medium => tr("settings.appearance.text_size_medium_desc"),
                    FontScale::Large => tr("settings.appearance.text_size_large_desc"),
                };
                if ui
                    .selectable_label(selected, f.label())
                    .describe(tip)
                    .clicked()
                    && state.runtime.font_scale != f
                {
                    state.runtime.font_scale = f;
                    state.dirty_runtime = true;
                }
            }
        });
        ui.add_space(8.0);

        // Language picker — driven by the locales actually present, so new
        // languages appear automatically. English is the default + fallback.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tr("settings.appearance.language")).strong());
            ui.add_space(6.0);
            let cur = state.runtime.language.clone();
            egui::ComboBox::from_id_salt("appearance_language")
                .selected_text(crate::i18n::display_name(&cur))
                .show_ui(ui, |ui| {
                    for code in crate::i18n::available() {
                        if ui
                            .selectable_label(cur == code, crate::i18n::display_name(&code))
                            .clicked()
                            && state.runtime.language != code
                        {
                            state.runtime.language = code;
                            state.dirty_runtime = true;
                        }
                    }
                })
                .response
                .describe(tr("settings.appearance.language_desc"));
        });
    });
}

// ---------------------------------------------------------------------------
// Streaming / encoding
// ---------------------------------------------------------------------------

fn streaming_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "streaming_card", |ui| {
        egui::Grid::new("streaming_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                // Bitrate -------------------------------------------------
                ui.label(tr("settings.streaming.bitrate"))
                    .describe(tr("settings.streaming.bitrate_label_desc"));
                let r = ui.add(
                    egui::Slider::new(&mut state.runtime.bitrate_kbps, 2_000..=80_000)
                        .suffix(" kbps")
                        .step_by(500.0),
                );
                if r.changed() {
                    state.dirty_runtime = true;
                }
                r.on_hover_text(tr("settings.streaming.bitrate_desc"));
                ui.end_row();

                // QUIC flow-control window --------------------------------
                ui.label(tr("settings.streaming.window"))
                    .describe(tr("settings.streaming.window_label_desc"));
                let r = ui.add(
                    egui::Slider::new(&mut state.runtime.stream_window_kb, 64..=2_048)
                        .suffix(" KiB")
                        .logarithmic(true),
                );
                if r.changed() {
                    state.dirty_runtime = true;
                }
                r.on_hover_text(tr("settings.streaming.window_desc"));
                ui.end_row();

                // Encoder queue depth -------------------------------------
                ui.label(tr("settings.streaming.queue"))
                    .describe(tr("settings.streaming.queue_label_desc"));
                let r = ui.add(
                    egui::Slider::new(&mut state.runtime.queue_frames, 1..=32).suffix(" frames"),
                );
                if r.changed() {
                    state.dirty_runtime = true;
                }
                r.on_hover_text(tr("settings.streaming.queue_desc"));
                ui.end_row();
            });

        ui.add_space(4.0);
        // A short pointer (a value/hint, not a description paragraph) carries the
        // streaming-section explanation on hover.
        ui.label(
            egui::RichText::new(tr("settings.streaming.encoder_tip"))
                .weak()
                .small(),
        )
        .describe(tr("settings.streaming.desc"));
    });
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

fn security_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "security_card", |ui| {
        let mut allow = state.runtime.allow_unpaired;
        if ui
            .checkbox(&mut allow, tr("settings.security.allow_unpaired"))
            .on_hover_text(tr("settings.security.allow_unpaired_desc"))
            .changed()
        {
            state.runtime.allow_unpaired = allow;
            state.dirty_runtime = true;
        }

        if state.runtime.allow_unpaired {
            ui.add_space(4.0);
            egui::Frame::none()
                .fill(ui.visuals().faint_bg_color)
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(egui::Margin::same(8.0))
                .show(ui, |ui| {
                    ui.colored_label(
                        theme::error(),
                        format!("{}  {}", ph::WARNING, tr("settings.security.warning")),
                    );
                    ui.label(
                        egui::RichText::new(tr("settings.security.warning_detail"))
                            .small()
                            .weak(),
                    );
                });
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(tr("settings.security.pairing"))
                .strong()
                .small(),
        )
        .describe(tr("settings.security.desc"));
        ui.label(
            egui::RichText::new(tr("settings.security.pairing_desc"))
                .small()
                .weak(),
        );
        ui.add_space(2.0);
        egui::Frame::none()
            .fill(ui.visuals().extreme_bg_color)
            .rounding(egui::Rounding::same(5.0))
            .inner_margin(egui::Margin::symmetric(8.0, 5.0))
            .show(ui, |ui| {
                // One canonical pairing command, shared with the Overview empty-state
                // (was a hardcoded, non-localised literal here — a divergence risk).
                ui.label(egui::RichText::new(tr("overview.empty.command")).monospace());
            });
        // Jump to the fleet instead of stating "listed elsewhere" as dead text.
        if ui
            .link(tr("settings.security.paired_listed"))
            .describe(tr("settings.security.paired_listed_desc"))
            .clicked()
        {
            state.tab = crate::model::Tab::Overview;
        }

        // Session lifecycle: how long the host waits for a client to (re)connect.
        // (Moved here from "Streaming / encoding" — it's a connection timeout, not
        // an encode parameter.)
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(tr("settings.security.client_wait"))
                .describe(tr("settings.security.client_wait_label_desc"));
            let r = ui.add(
                egui::Slider::new(&mut state.runtime.client_wait_secs, 10..=3_600)
                    .suffix(" s")
                    .logarithmic(true),
            );
            if r.changed() {
                state.dirty_runtime = true;
            }
            r.on_hover_text(tr("settings.security.client_wait_desc"));
        });
    });
}

// ---------------------------------------------------------------------------
// Gamepad
// ---------------------------------------------------------------------------

fn gamepad_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "gamepad_card", |ui| {
        // Card title anchors the generous section description (hover / screen
        // reader only — no visible prose paragraph).
        ui.label(
            egui::RichText::new(tr("settings.gamepad.fallback"))
                .strong()
                .small(),
        )
        .describe(tr("settings.gamepad.desc"));
        ui.add_space(4.0);

        let mut swap = state.runtime.gamepad_swap_face;
        if ui
            .checkbox(&mut swap, tr("settings.gamepad.swap_face"))
            .on_hover_text(tr("settings.gamepad.swap_face_desc"))
            .changed()
        {
            state.runtime.gamepad_swap_face = swap;
            state.dirty_runtime = true;
        }
    });
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

fn network_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "network_card", |ui| {
        ui.horizontal(|ui| {
            ui.label(tr("settings.network.bind_mac"))
                .describe(tr("settings.network.desc"));

            // ComboBox seeded from advertised NICs (+ a blank "all" choice),
            // but still free-text editable below for MACs we can't enumerate.
            let current = if state.runtime_bind_label().is_empty() {
                tr("settings.network.all_interfaces")
            } else {
                state.runtime_bind_label()
            };
            egui::ComboBox::from_id_salt("bind_mac_combo")
                .selected_text(current)
                .width(240.0)
                .show_ui(ui, |ui| {
                    // Blank = bind everything.
                    if ui
                        .selectable_label(
                            state.host_config.bind_mac.is_empty(),
                            tr("settings.network.all_interfaces"),
                        )
                        .clicked()
                        && !state.host_config.bind_mac.is_empty()
                    {
                        state.host_config.bind_mac.clear();
                        state.dirty_host = true;
                    }
                    for mac in &state.host.macs.clone() {
                        let sel = state.host_config.bind_mac.eq_ignore_ascii_case(mac);
                        if ui.selectable_label(sel, mac).clicked() && !sel {
                            state.host_config.bind_mac = mac.clone();
                            state.dirty_host = true;
                        }
                    }
                })
                .response
                .on_hover_text(tr("settings.network.bind_mac_desc"));
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(tr("settings.network.or_type"))
                .on_hover_text(tr("settings.network.or_type_desc"));
            if ui
                .add(
                    egui::TextEdit::singleline(&mut state.host_config.bind_mac)
                        .hint_text(tr("settings.network.mac_hint"))
                        .desired_width(220.0),
                )
                .changed()
            {
                state.dirty_host = true;
            }
        });

        // Light format check: warn (don't block) on a non-empty but malformed MAC.
        // An invalid value would otherwise land silently in config.toml and surface
        // only as a hard-to-diagnose host bind failure.
        let mac = state.host_config.bind_mac.trim();
        if !mac.is_empty() && !mac_is_wellformed(mac) {
            ui.add_space(2.0);
            ui.colored_label(theme::error(), tr("settings.network.mac_malformed"))
                .describe(tr("settings.network.mac_malformed_desc"));
        }

        if state.host.macs.is_empty() {
            ui.add_space(4.0);
            ui.colored_label(theme::warn(), tr("settings.network.no_nics"));
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        if ui
            .checkbox(&mut state.host_config.ipv6, tr("settings.network.ipv6"))
            .describe(tr("settings.network.ipv6_desc"))
            .changed()
        {
            state.dirty_host = true;
        }
    });
}

/// True if `s` is a well-formed `aa:bb:cc:dd:ee:ff` MAC: six colon-separated
/// two-digit hex octets, case-insensitive. Used only to *warn* on a malformed
/// entry — the empty "bind all interfaces" case is handled by the caller.
fn mac_is_wellformed(s: &str) -> bool {
    let mut octets = 0usize;
    for part in s.split(':') {
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        octets += 1;
    }
    octets == 6
}

// ---------------------------------------------------------------------------
// Home Assistant / MQTT
// ---------------------------------------------------------------------------

fn mqtt_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "mqtt_card", |ui| {
        let mut enabled = state.host_config.mqtt_enabled;
        if ui
            .checkbox(&mut enabled, tr("settings.mqtt.enable"))
            .describe(tr("settings.mqtt.desc"))
            .changed()
        {
            state.host_config.mqtt_enabled = enabled;
            state.dirty_host = true;
        }

        // Grey the rest out when disabled, but keep it editable so the user can
        // fill things in before flipping it on.
        ui.add_space(6.0);
        ui.add_enabled_ui(state.host_config.mqtt_enabled, |ui| {
            egui::Grid::new("mqtt_grid")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label(tr("settings.mqtt.broker_host"))
                        .on_hover_text(tr("settings.mqtt.broker_host_desc"));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut state.host_config.mqtt_host)
                                .hint_text("192.168.1.10")
                                .desired_width(220.0),
                        )
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();

                    ui.label(tr("settings.mqtt.port"));
                    if ui
                        .add(
                            egui::DragValue::new(&mut state.host_config.mqtt_port)
                                .range(1..=65_535),
                        )
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();

                    ui.label(tr("settings.mqtt.username"))
                        .on_hover_text(tr("settings.mqtt.username_desc"));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut state.host_config.mqtt_username)
                                .hint_text(tr("settings.mqtt.optional_hint"))
                                .desired_width(220.0),
                        )
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();

                    ui.label(tr("settings.mqtt.password"));
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut state.host_config.mqtt_password)
                                .password(true)
                                .hint_text(tr("settings.mqtt.optional_hint"))
                                .desired_width(220.0),
                        )
                        .on_hover_text(tr("settings.mqtt.password_desc"))
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();

                    ui.label(tr("settings.mqtt.node_id"))
                        .on_hover_text(tr("settings.mqtt.node_id_desc"));
                    let hint = node_id_hint_str(&state.host.hostname);
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut state.host_config.mqtt_node_id)
                                .hint_text(hint)
                                .desired_width(220.0),
                        )
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();

                    ui.label(tr("settings.mqtt.discovery_prefix"))
                        .on_hover_text(tr("settings.mqtt.discovery_prefix_desc"));
                    if ui
                        .add(
                            egui::TextEdit::singleline(
                                &mut state.host_config.mqtt_discovery_prefix,
                            )
                            .hint_text(tr("settings.mqtt.discovery_prefix_hint"))
                            .desired_width(220.0),
                        )
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();

                    ui.label(tr("settings.mqtt.publish_interval"))
                        .on_hover_text(tr("settings.mqtt.publish_interval_desc"));
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut state.host_config.mqtt_publish_interval_secs,
                                1..=300,
                            )
                            .suffix(" s")
                            .logarithmic(true),
                        )
                        .changed()
                    {
                        state.dirty_host = true;
                    }
                    ui.end_row();
                });
        });

        if !state.host_config.mqtt_enabled {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(tr("settings.mqtt.disabled_note"))
                    .small()
                    .weak(),
            );
        }
    });
}

/// A friendly placeholder for the node-id field derived from the hostname.
fn node_id_hint_str(hostname: &str) -> String {
    if hostname.is_empty() {
        tr("settings.mqtt.node_id_default_host")
    } else {
        let slug: String = hostname
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        format!("(default: {slug})")
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn paths_section(ui: &mut egui::Ui) {
    crate::widgets::card(ui, "paths_card", |ui| {
        let dir = crate::data::config_dir();
        ui.horizontal(|ui| {
            ui.label(tr("settings.paths.config_dir"))
                .describe(tr("settings.paths.desc"));
            ui.label(egui::RichText::new(dir.display().to_string()).monospace());
        });
        ui.label(
            egui::RichText::new(tr("settings.paths.config_dir_override"))
                .small()
                .weak(),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(tr("settings.paths.files_title"))
                .strong()
                .small(),
        );
        ui.add_space(4.0);

        // (filename, what it holds, written-by-this-tab?)
        let files: [(&str, String, bool); 5] = [
            ("config.toml", tr("settings.paths.file_config_desc"), true),
            ("panel.toml", tr("settings.paths.file_panel_desc"), true),
            ("peers.toml", tr("settings.paths.file_peers_desc"), false),
            (
                "controller-profiles.toml",
                tr("settings.paths.file_profiles_desc"),
                false,
            ),
            ("multi.toml", tr("settings.paths.file_multi_desc"), false),
        ];

        egui::Grid::new("paths_grid")
            .num_columns(3)
            .spacing([14.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for (name, desc, written_here) in &files {
                    ui.label(egui::RichText::new(*name).monospace());
                    if *written_here {
                        theme::badge(ui, &tr("settings.paths.this_tab"), theme::online());
                    } else {
                        theme::badge(ui, &tr("settings.paths.read_only"), theme::unavailable());
                    }
                    ui.label(egui::RichText::new(desc).small().weak());
                    ui.end_row();
                }
            });
    });
}

// ---------------------------------------------------------------------------
// Local helper on PanelState (kept here to avoid touching the shared model).
// ---------------------------------------------------------------------------

trait BindLabel {
    fn runtime_bind_label(&self) -> String;
}

impl BindLabel for PanelState {
    /// The bind_mac as a display string (empty if unset).
    fn runtime_bind_label(&self) -> String {
        self.host_config.bind_mac.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::mac_is_wellformed;

    #[test]
    fn accepts_canonical_macs() {
        assert!(mac_is_wellformed("aa:bb:cc:dd:ee:ff"));
        assert!(mac_is_wellformed("00:11:22:33:44:55"));
        assert!(mac_is_wellformed("AA:BB:CC:DD:EE:FF")); // case-insensitive
        assert!(mac_is_wellformed("0a:1B:2c:3D:4e:5F")); // mixed case
    }

    #[test]
    fn rejects_malformed_macs() {
        assert!(!mac_is_wellformed("aabbccddeeff")); // no separators
        assert!(!mac_is_wellformed("aa:bb:cc:dd:ee")); // 5 octets
        assert!(!mac_is_wellformed("aa:bb:cc:dd:ee:ff:00")); // 7 octets
        assert!(!mac_is_wellformed("aa-bb-cc-dd-ee-ff")); // dash separators
        assert!(!mac_is_wellformed("ga:bb:cc:dd:ee:ff")); // non-hex digit
        assert!(!mac_is_wellformed("a:bb:cc:dd:ee:ff")); // short octet
        assert!(!mac_is_wellformed("aaa:bb:cc:dd:ee:f")); // long octet
        assert!(!mac_is_wellformed("")); // empty (caller guards this anyway)
        assert!(!mac_is_wellformed("aa::cc:dd:ee:ff")); // empty octet
    }
}
