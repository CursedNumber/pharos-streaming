// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Controllers tab — per-controller gyro/swap tuning + reusable presets.
//!
//! Mirrors the Trireme Android controller UI (HANDOFF.md + the
//! `project-controller-profiles` memory): the panel edits the host's
//! *persisted* profiles in `controller-profiles.toml` (keyed by the canonical
//! `"vvvv:pppp"` controller id). During a live stream the same numbers are
//! relayed as `core_proto::InputEvent::ControllerTuning`; here we are the
//! source-of-truth editor that a pad picks up the moment it connects.
//!
//! All state that has to survive between frames (the add/preset dialogs, the
//! preset-picker selection) lives in egui's per-id temp memory — this file may
//! only touch `state.controllers` + the `dirty_controllers` flag, never the
//! shared `PanelState` shape, so we keep transient UI state out of the model.

use core_proto::controllers::{classify, ControllerCaps};
use core_proto::GyroTuning;
use egui_phosphor::regular as ph;

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::PanelState;
use crate::theme;

// ---------------------------------------------------------------------------
// Gyro defaults (shown to the user as the "factory feel" reference).
// ---------------------------------------------------------------------------

/// The shipped gyro defaults, for the "Reset to defaults" affordance + the help
/// text. Kept in sync with `core_proto::GyroTuning::default()` (Switch Pro feel
/// dialled in on the dev host).
const GYRO_DEFAULTS: GyroTuning = GyroTuning {
    enabled: true,
    sens_x: 16.0,
    sens_y: 9.0,
    accel: 2.5,
    accel_pow: 2.0,
    accel_ref: 110.0,
    accel_max: 10.0,
    invert_x: false,
    invert_y: false,
    axis_h: 2,
    axis_v: 1,
};

/// The three raw gyro axes a controller exposes, as `(value, token)` for the
/// axis-mapping combo boxes (`0=rx, 1=ry, 2=rz`). `token` is the fixed HID axis
/// identifier (RX/RY/RZ); the translatable "(axis N)" descriptor is composed at
/// the call site so the static text routes through i18n.
const GYRO_AXES: [(u8, &str); 3] = [(0, "RX"), (1, "RY"), (2, "RZ")];

/// The display label for a raw gyro axis: the fixed `RX`/`RY`/`RZ` token plus the
/// translated "(axis N)" descriptor, e.g. `RX (axis 0)`.
fn axis_label(a: u8) -> String {
    let token = GYRO_AXES
        .iter()
        .find(|(v, _)| *v == a)
        .map(|(_, t)| *t)
        .unwrap_or("RX");
    format!("{token} ({} {a})", tr("controllers.axis.raw"))
}

// ---------------------------------------------------------------------------
// Transient dialog state (egui temp memory — file-local, not in PanelState).
// ---------------------------------------------------------------------------

/// "Add controller" dialog buffer.
#[derive(Clone, Default)]
struct AddDialog {
    open: bool,
    vid: String,
    pid: String,
    name: String,
}

/// "Save as preset" / "rename preset" dialog buffer. `source_key` is the
/// controller id whose current profile seeds a new preset (empty for rename).
#[derive(Clone, Default)]
struct PresetDialog {
    open: bool,
    /// "save" = create a preset from a controller; "rename" = rename existing.
    mode: String,
    name: String,
    /// For "save": the controller key to copy the profile from.
    source_key: String,
    /// For "rename": the original preset name.
    original: String,
}

/// Per-controller "apply preset" picker selection (preset name, "" = none).
type ApplySel = String;

fn add_dialog(ui: &egui::Ui) -> AddDialog {
    ui.ctx()
        .data_mut(|d| d.get_temp::<AddDialog>(add_id()).unwrap_or_default())
}
fn set_add_dialog(ui: &egui::Ui, v: AddDialog) {
    ui.ctx().data_mut(|d| d.insert_temp(add_id(), v));
}
fn add_id() -> egui::Id {
    egui::Id::new("pharos_ctrl_add_dialog")
}

fn preset_dialog(ui: &egui::Ui) -> PresetDialog {
    ui.ctx()
        .data_mut(|d| d.get_temp::<PresetDialog>(preset_id()).unwrap_or_default())
}
fn set_preset_dialog(ui: &egui::Ui, v: PresetDialog) {
    ui.ctx().data_mut(|d| d.insert_temp(preset_id(), v));
}
fn preset_id() -> egui::Id {
    egui::Id::new("pharos_ctrl_preset_dialog")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    // Subscribe to the host's per-controller liveness rebuild while this tab
    // is mounted. The host gates the (expensive) `evdev::enumerate()` walk
    // behind a subscriber deadline — re-poked here on every render tick — so
    // a closed tab pays nothing and an open tab pays at most ~1 Hz on the
    // host. TTL is generous (5s) so a momentary tick miss doesn't drop the
    // subscription mid-frame; the host clamps to 30s anyway.
    crate::control::subscribe_controllers(5_000);

    ui.heading(tr("controllers.heading"))
        .describe(tr("controllers.intro_desc"));
    // Discoverability: the host-wide A/B fallback for pads without a per-controller
    // override lives in Settings -> Gamepad. Link to it rather than leaving it buried
    // in a default-collapsed Settings section (it's a fallback, not a duplicate —
    // per-controller swaps below always win).
    if ui
        .link(egui::RichText::new(tr("controllers.global_fallback_link")).small())
        .describe(tr("controllers.global_fallback_desc"))
        .clicked()
    {
        state.tab = crate::model::Tab::Settings;
    }
    ui.add_space(8.0);

    // --- toolbar: add (saving is global, in the bottom bar) ------------------
    if ui
        .button(format!("{}  {}", ph::PLUS, tr("controllers.toolbar.add")))
        .describe(tr("controllers.toolbar.add_desc"))
        .clicked()
    {
        set_add_dialog(
            ui,
            AddDialog {
                open: true,
                ..Default::default()
            },
        );
    }

    ui.add_space(8.0);

    // --- controllers ---------------------------------------------------------
    theme::section(
        ui,
        ph::GAME_CONTROLLER,
        &tr("controllers.section.configured"),
        &tr("controllers.section.configured_desc"),
    );

    // Sorted, joined list: every saved controller PLUS every live controller
    // the host has seen this run that we don't have a profile for yet, ordered
    // by (connected desc, last_connected_ms desc, effective_name asc). Falls
    // back to a static alphabetical list if there's no live host (the live vec
    // is empty against a v3 host or with no socket).
    let ordered_keys = ordered_controller_keys(state);
    if ordered_keys.is_empty() {
        empty_state(ui);
    } else {
        for key in ordered_keys {
            controller_card(ui, state, &key);
            ui.add_space(6.0);
        }
    }

    ui.add_space(10.0);

    // --- presets -------------------------------------------------------------
    theme::section(
        ui,
        "",
        &tr("controllers.section.presets"),
        &tr("controllers.section.presets_intro"),
    );
    ui.add_space(4.0);
    presets_card(ui, state);

    ui.add_space(10.0);

    // --- USB/IP forwarding reference ----------------------------------------
    theme::section(
        ui,
        ph::PLUGS,
        &tr("controllers.section.usbip"),
        &tr("controllers.usbip.intro_desc"),
    );
    usbip_reference_card(ui);

    // --- modal dialogs (drawn last so they float over everything) -----------
    draw_add_dialog(ui, state);
    draw_preset_dialog(ui, state);
}

// ---------------------------------------------------------------------------
// Empty state
// ---------------------------------------------------------------------------

fn empty_state(ui: &mut egui::Ui) {
    crate::widgets::card(ui, "ctrl_empty", |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(ph::GAME_CONTROLLER).size(28.0))
                .describe_named(tr("controllers.empty.title"), tr("controllers.empty.desc"));
            ui.label(
                egui::RichText::new(tr("controllers.empty.title"))
                    .strong()
                    .size(15.0),
            );
            ui.label(
                egui::RichText::new(tr("controllers.empty.body"))
                    .small()
                    .weak(),
            )
            .describe(tr("controllers.empty.desc"));
            ui.add_space(6.0);
        });
    });
}

// ---------------------------------------------------------------------------
// One controller card
// ---------------------------------------------------------------------------

fn controller_card(ui: &mut egui::Ui, state: &mut PanelState, key: &str) {
    let (vid, pid) = parse_vidpid(key);
    // Classify from the ids; we have no live name here, so pass "" (the
    // name-spoof override only matters for live-connected 8BitDo/Flydigi).
    let info = classify(vid, pid, "");

    let live = live_status(key);
    let connected = live.as_ref().map(|s| s.connected).unwrap_or(false);

    let bus = live.as_ref().map(|s| s.bus).unwrap_or_default();

    crate::widgets::card(ui, ("ctrl_card", key), |ui| {
        // Header row (inline order: liveness dot · name editor · bus pill ·
        // ids + caps · remove). Bus pill is omitted on `ControllerBus::Unknown`
        // (no live evdev to classify) so a saved-but-absent row stays clean.
        ui.horizontal(|ui| {
            liveness_dot(ui, connected);
            ui.add_space(4.0);
            alias_inline_editor(ui, state, key, vid, pid, info.kind.label());
            bus_pill(ui, bus, connected);
            theme::badge(ui, &format!("{vid:04x}:{pid:04x}"), theme::info());
            caps_badges(ui, &info.caps);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(ph::TRASH)
                    .describe(tr("controllers.card.remove_desc"))
                    .clicked()
                {
                    state.controllers.controllers.remove(key);
                    state.controllers.alias_draft.remove(key);
                    state.dirty_controllers = true;
                    state.status =
                        Some(format!("{} {key}.", tr("controllers.card.removed_status")));
                }
            });
        });

        ui.add_space(2.0);

        // --- preset actions (Apply / Save-as) for this controller -----------
        // Done before the in-place borrow below so we can mutate the map here.
        let mut changed = controller_preset_actions(ui, state, key);

        ui.add_space(2.0);

        // The profile is edited in place; take a mutable handle. A live-only
        // controller (live in `status.controllers`, not in the on-disk map)
        // has no profile yet → we skip the tuning rows; the inline editor +
        // dot still rendered above, so the row is still useful (the user can
        // rename it; saving the rename auto-creates the entry host-side, but
        // that round-trips through the host's `SetControllerAlias` handler).
        let Some(profile) = state.controllers.controllers.get_mut(key) else {
            if changed {
                state.dirty_controllers = true;
            }
            return;
        };

        // --- face-button swaps ----------------------------------------------
        ui.horizontal(|ui| {
            changed |= ui
                .checkbox(&mut profile.swap_ab, tr("controllers.swap.ab"))
                .describe(tr("controllers.swap.ab_desc"))
                .changed();
            ui.add_space(12.0);
            changed |= ui
                .checkbox(&mut profile.swap_xy, tr("controllers.swap.xy"))
                .describe(tr("controllers.swap.xy_desc"))
                .changed();
        });

        ui.add_space(4.0);

        // --- gyro tuning (folded so the card stays compact) -----------------
        let gyro_hint = if info.caps.gyro {
            None
        } else {
            Some(tr("controllers.gyro.no_sensor_hint"))
        };
        let state_word = if profile.gyro.enabled {
            tr("controllers.presets.gyro_on")
        } else {
            tr("controllers.presets.gyro_off")
        };
        let gyro_title =
            egui::RichText::new(format!("{} · {state_word}", tr("controllers.gyro.heading")))
                .strong();
        let gyro_resp = egui::CollapsingHeader::new(gyro_title)
            .id_salt(("gyro_section", key))
            .default_open(false)
            .show(ui, |ui| gyro_editor(ui, key, &mut profile.gyro, gyro_hint));
        changed |= gyro_resp.body_returned.unwrap_or(false);

        if changed {
            state.dirty_controllers = true;
        }
    });
}

/// Capability pills (gyro / touchpad / paddles / rumble), greyed when absent.
fn caps_badges(ui: &mut egui::Ui, caps: &ControllerCaps) {
    let cap = |ui: &mut egui::Ui, on: bool, label: &str| {
        let color = if on {
            theme::validated()
        } else {
            theme::unavailable()
        };
        let text = if on {
            format!("✓ {label}")
        } else {
            format!("✕ {label}")
        };
        theme::badge(ui, &text, color);
    };
    cap(ui, caps.gyro, &tr("controllers.caps.gyro"));
    cap(ui, caps.touchpad, &tr("controllers.caps.touchpad"));
    cap(ui, caps.paddles, &tr("controllers.caps.paddles"));
    cap(ui, caps.rumble, &tr("controllers.caps.rumble"));
}

// ---------------------------------------------------------------------------
// Gyro editor (shared by per-controller cards + the preset editor)
// ---------------------------------------------------------------------------

/// Render the full gyro tuning editor for `gyro`, returning true if any value
/// changed. `id_seed` namespaces the widgets (combo boxes need stable ids).
fn gyro_editor(
    ui: &mut egui::Ui,
    id_seed: &str,
    gyro: &mut GyroTuning,
    hint: Option<String>,
) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        changed |= ui
            .checkbox(&mut gyro.enabled, tr("controllers.gyro.enabled"))
            .describe(tr("controllers.gyro.enabled_desc"))
            .changed();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(tr("controllers.gyro.reset"))
                .describe(tr("controllers.gyro.reset_desc"))
                .clicked()
            {
                *gyro = GYRO_DEFAULTS;
                changed = true;
            }
        });
    });

    if let Some(h) = hint {
        ui.label(egui::RichText::new(&h).small().color(theme::warn()))
            .describe(tr("controllers.gyro.no_sensor_desc"));
    }

    // Sliders live in a tidy two-column grid. Disable when gyro is off so the
    // UI reads as "these knobs don't apply", but still let the user see them.
    ui.add_enabled_ui(gyro.enabled, |ui| {
        egui::Grid::new(("gyro_grid", id_seed))
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                changed |= slider_row(
                    ui,
                    &tr("controllers.slider.sens_x"),
                    &mut gyro.sens_x,
                    1.0..=50.0,
                    &tr("controllers.slider.sens_x_help"),
                    GYRO_DEFAULTS.sens_x,
                );
                changed |= slider_row(
                    ui,
                    &tr("controllers.slider.sens_y"),
                    &mut gyro.sens_y,
                    1.0..=50.0,
                    &tr("controllers.slider.sens_y_help"),
                    GYRO_DEFAULTS.sens_y,
                );
                changed |= slider_row(
                    ui,
                    &tr("controllers.slider.accel"),
                    &mut gyro.accel,
                    0.0..=5.0,
                    &tr("controllers.slider.accel_help"),
                    GYRO_DEFAULTS.accel,
                );
                changed |= slider_row(
                    ui,
                    &tr("controllers.slider.accel_pow"),
                    &mut gyro.accel_pow,
                    0.5..=3.0,
                    &tr("controllers.slider.accel_pow_help"),
                    GYRO_DEFAULTS.accel_pow,
                );
                changed |= slider_row(
                    ui,
                    &tr("controllers.slider.accel_ref"),
                    &mut gyro.accel_ref,
                    30.0..=200.0,
                    &tr("controllers.slider.accel_ref_help"),
                    GYRO_DEFAULTS.accel_ref,
                );
                changed |= slider_row(
                    ui,
                    &tr("controllers.slider.accel_max"),
                    &mut gyro.accel_max,
                    1.0..=20.0,
                    &tr("controllers.slider.accel_max_help"),
                    GYRO_DEFAULTS.accel_max,
                );
            });

        // Visualise the curve the four accel knobs above shape — output gain vs
        // how fast you rotate — so they can be tuned by sight instead of blind.
        ui.add_space(6.0);
        draw_gyro_curve(ui, gyro);

        ui.add_space(2.0);

        // Invert + axis mapping.
        ui.horizontal(|ui| {
            changed |= ui
                .checkbox(&mut gyro.invert_x, tr("controllers.invert.x"))
                .describe(tr("controllers.invert.x_desc"))
                .changed();
            ui.add_space(12.0);
            changed |= ui
                .checkbox(&mut gyro.invert_y, tr("controllers.invert.y"))
                .describe(tr("controllers.invert.y_desc"))
                .changed();
        });

        ui.horizontal(|ui| {
            changed |= axis_combo(
                ui,
                ("axis_h", id_seed),
                &tr("controllers.axis.horizontal"),
                &tr("controllers.axis.horizontal_desc"),
                &mut gyro.axis_h,
            );
            ui.add_space(12.0);
            changed |= axis_combo(
                ui,
                ("axis_v", id_seed),
                &tr("controllers.axis.vertical"),
                &tr("controllers.axis.vertical_desc"),
                &mut gyro.axis_v,
            );
        });
    });

    changed
}

/// One labelled slider grid row with a "default" hint in the tooltip. Returns
/// true on change.
fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    help: &str,
    default: f32,
) -> bool {
    let desc = format!(
        "{help}\n{} {default}",
        tr("controllers.slider.default_label")
    );
    ui.label(label).describe(desc.clone());
    let resp = ui
        .add(egui::Slider::new(value, range).clamping(egui::SliderClamping::Always))
        .describe(desc);
    ui.end_row();
    resp.changed()
}

/// Effective gyro output multiplier (base sensitivity is multiplied by this) at a
/// combined rotation `speed` in deg/s. Mirrors the host's `accel_gain` in
/// `compositor::gyro_imu`: `1 + min(accel · (speed/accel_ref)^accel_pow, accel_max)`.
fn accel_multiplier(g: &GyroTuning, speed: f32) -> f32 {
    if g.accel <= 0.0 || speed <= 0.0 {
        return 1.0;
    }
    let aref = if g.accel_ref != 0.0 {
        g.accel_ref
    } else {
        100.0
    };
    let extra = (g.accel * (speed / aref).powf(g.accel_pow)).min(g.accel_max);
    1.0 + extra
}

/// A small painter plot of the acceleration curve the four accel knobs shape:
/// output gain (y, ×1 at the bottom) vs how fast you rotate the pad (x, deg/s),
/// with the knee (`accel_ref`) and the clamp ceiling (`1 + accel_max`) marked.
/// Pure painter, no new deps. The curve dims when gyro is disabled.
fn draw_gyro_curve(ui: &mut egui::Ui, g: &GyroTuning) {
    const X_MAX: f32 = 600.0; // deg/s — slow aim ~50, fast flick ~500
    let y_top = ((1.0 + g.accel_max) * 1.05).max(1.5);
    let desired = egui::vec2(ui.available_width().min(300.0), 84.0);
    let (resp, painter) = ui.allocate_painter(desired, egui::Sense::hover());
    let rect = resp.rect;
    let vis = ui.visuals();

    painter.rect(
        rect,
        egui::Rounding::same(4.0),
        vis.extreme_bg_color,
        egui::Stroke::new(1.0, vis.weak_text_color()),
    );

    let map = |sp: f32, gain: f32| {
        let fx = (sp / X_MAX).clamp(0.0, 1.0);
        let fy = ((gain - 1.0) / (y_top - 1.0)).clamp(0.0, 1.0);
        egui::pos2(
            rect.left() + fx * rect.width(),
            rect.bottom() - fy * rect.height(),
        )
    };

    let faint = egui::Stroke::new(1.0, vis.weak_text_color());
    // Knee (accel_ref) and clamp ceiling (1 + accel_max).
    if g.accel_ref > 0.0 && g.accel_ref <= X_MAX {
        let x = map(g.accel_ref, 1.0).x;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            faint,
        );
    }
    let ceil = 1.0 + g.accel_max;
    if ceil <= y_top {
        let y = map(0.0, ceil).y;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            faint,
        );
    }

    // The curve itself, as connected segments.
    let col = if g.enabled {
        vis.hyperlink_color
    } else {
        vis.weak_text_color()
    };
    let stroke = egui::Stroke::new(1.6, col);
    let n = 48;
    let mut prev = map(0.0, accel_multiplier(g, 0.0));
    for i in 1..=n {
        let sp = X_MAX * i as f32 / n as f32;
        let p = map(sp, accel_multiplier(g, sp));
        painter.line_segment([prev, p], stroke);
        prev = p;
    }

    // Corner labels: gain ceiling, unity, and the speed range.
    let lab = egui::FontId::monospace(9.0);
    let weak = vis.weak_text_color();
    painter.text(
        rect.left_top() + egui::vec2(3.0, 1.0),
        egui::Align2::LEFT_TOP,
        format!("{y_top:.0}×"),
        lab.clone(),
        weak,
    );
    painter.text(
        rect.left_bottom() + egui::vec2(3.0, -1.0),
        egui::Align2::LEFT_BOTTOM,
        "1×",
        lab.clone(),
        weak,
    );
    painter.text(
        rect.right_bottom() + egui::vec2(-3.0, -1.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{X_MAX:.0}°/s"),
        lab,
        weak,
    );

    resp.describe(tr("controllers.gyro.curve_desc"));
}

/// A combo box over the three raw gyro axes (RX/RY/RZ). Returns true on change.
/// `label` names the direction; `desc` is the screen-reader / hover explanation.
fn axis_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    label: &str,
    desc: &str,
    axis: &mut u8,
) -> bool {
    let mut changed = false;
    ui.label(label).describe(desc.to_string());
    egui::ComboBox::from_id_salt(id)
        .selected_text(axis_label(*axis))
        .width(120.0)
        .show_ui(ui, |ui| {
            for (val, _) in GYRO_AXES {
                if ui
                    .selectable_value(axis, val, axis_label(val))
                    .describe(tr("controllers.axis.combo_desc"))
                    .changed()
                {
                    changed = true;
                }
            }
        })
        .response
        .describe(desc.to_string());
    changed
}

// ---------------------------------------------------------------------------
// Presets card
// ---------------------------------------------------------------------------

fn presets_card(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "presets_card", |ui| {
        if state.controllers.presets.is_empty() {
            ui.label(
                egui::RichText::new(tr("controllers.presets.empty"))
                    .small()
                    .weak(),
            );
            ui.add_space(6.0);
            // Seed a starting preset so "Apply preset" isn't dead on first run.
            if ui
                .button(format!(
                    "{}  {}",
                    ph::PLUS,
                    tr("controllers.presets.create_default")
                ))
                .describe(tr("controllers.presets.create_default_desc"))
                .clicked()
            {
                state.controllers.presets.insert(
                    tr("controllers.presets.default_name"),
                    core_proto::ControllerProfile::default(),
                );
                state.dirty_controllers = true;
                state.status = Some(tr("controllers.presets.created_status"));
            }
            return;
        }

        let names: Vec<String> = state.controllers.presets.keys().cloned().collect();
        for name in names {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&name).strong())
                    .describe(tr("controllers.presets.name_desc"));

                // Summarise the preset's headline numbers inline.
                if let Some(p) = state.controllers.presets.get(&name) {
                    let g = &p.gyro;
                    let summary = format!(
                        "{} {} · {} {:.0}/{:.0} · {} {:.1}{}{}",
                        tr("controllers.summary.gyro"),
                        if g.enabled {
                            tr("controllers.presets.gyro_on")
                        } else {
                            tr("controllers.presets.gyro_off")
                        },
                        tr("controllers.summary.sens"),
                        g.sens_x,
                        g.sens_y,
                        tr("controllers.summary.accel"),
                        g.accel,
                        if p.swap_ab { " · A↔B" } else { "" },
                        if p.swap_xy { " · X↔Y" } else { "" },
                    );
                    theme::badge(ui, &summary, theme::available());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(tr("controllers.presets.delete"))
                        .describe(tr("controllers.presets.delete_desc"))
                        .clicked()
                    {
                        state.controllers.presets.remove(&name);
                        state.dirty_controllers = true;
                        state.status = Some(format!(
                            "{} “{name}”.",
                            tr("controllers.presets.deleted_status")
                        ));
                    }
                    if ui
                        .small_button(tr("controllers.presets.rename"))
                        .describe(tr("controllers.presets.rename_desc"))
                        .clicked()
                    {
                        let d = PresetDialog {
                            open: true,
                            mode: "rename".into(),
                            name: name.clone(),
                            original: name.clone(),
                            source_key: String::new(),
                        };
                        set_preset_dialog(ui, d);
                    }
                });
            });
            ui.separator();
        }
    });
}

// ---------------------------------------------------------------------------
// USB/IP forwarding reference
// ---------------------------------------------------------------------------

fn usbip_reference_card(ui: &mut egui::Ui) {
    crate::widgets::card(ui, "usbip_ref", |ui| {
        let row =
            |ui: &mut egui::Ui, badge: &str, color: egui::Color32, title: &str, body: &str| {
                ui.horizontal_top(|ui| {
                    theme::badge(ui, badge, color);
                    ui.label(egui::RichText::new(title).strong())
                        .describe_named(title.to_string(), body.to_string());
                });
                ui.add_space(4.0);
            };

        row(
            ui,
            &tr("controllers.usbip.relay_badge"),
            theme::validated(),
            &tr("controllers.usbip.relay_title"),
            &tr("controllers.usbip.relay_body"),
        );
        row(
            ui,
            &tr("controllers.usbip.bt_badge"),
            theme::warn(),
            &tr("controllers.usbip.bt_title"),
            &tr("controllers.usbip.bt_body"),
        );
        row(
            ui,
            &tr("controllers.usbip.iso_badge"),
            theme::error(),
            &tr("controllers.usbip.iso_title"),
            &tr("controllers.usbip.iso_body"),
        );
    });
}

// ---------------------------------------------------------------------------
// Add-controller dialog
// ---------------------------------------------------------------------------

fn draw_add_dialog(ui: &mut egui::Ui, state: &mut PanelState) {
    let mut d = add_dialog(ui);
    if !d.open {
        return;
    }
    let ctx = ui.ctx().clone();
    let mut close = false;
    let mut commit = false;

    egui::Window::new(tr("controllers.add.title"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(&ctx, |ui| {
            ui.set_min_width(320.0);
            ui.label(
                egui::RichText::new(tr("controllers.add.body"))
                    .small()
                    .weak(),
            );
            ui.add_space(6.0);
            egui::Grid::new("add_ctrl_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label(tr("controllers.add.vid_label"));
                    ui.add(egui::TextEdit::singleline(&mut d.vid).hint_text("045e"))
                        .describe(tr("controllers.add.vid_desc"));
                    ui.end_row();
                    ui.label(tr("controllers.add.pid_label"));
                    ui.add(egui::TextEdit::singleline(&mut d.pid).hint_text("028e"))
                        .describe(tr("controllers.add.pid_desc"));
                    ui.end_row();
                    ui.label(tr("controllers.add.name_label"));
                    ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("Flydigi Vader 3"))
                        .describe(tr("controllers.add.name_desc"));
                    ui.end_row();
                });

            // Live preview of how it'll classify.
            let parsed = parse_hex(&d.vid).zip(parse_hex(&d.pid));
            let key = parsed.map(|(v, p)| format!("{v:04x}:{p:04x}"));
            let already = key
                .as_ref()
                .map(|k| state.controllers.controllers.contains_key(k))
                .unwrap_or(false);
            match parsed {
                Some((v, p)) => {
                    let info = classify(v, p, d.name.trim());
                    ui.horizontal(|ui| {
                        ui.label(tr("controllers.add.detected"))
                            .describe(tr("controllers.add.detected_desc"));
                        theme::badge(ui, info.kind.label(), theme::info());
                        caps_badges(ui, &info.caps);
                    });
                    if already {
                        ui.label(
                            egui::RichText::new(tr("controllers.add.exists"))
                                .small()
                                .color(theme::warn()),
                        )
                        .describe(tr("controllers.add.exists_desc"));
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new(tr("controllers.add.invalid"))
                            .small()
                            .weak(),
                    )
                    .describe(tr("controllers.add.invalid_desc"));
                }
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(tr("controllers.add.cancel"))
                    .describe(tr("controllers.add.cancel_desc"))
                    .clicked()
                {
                    close = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = ui.add_enabled(
                        parsed.is_some() && !already,
                        egui::Button::new(tr("controllers.add.confirm")),
                    );
                    if ok.describe(tr("controllers.add.confirm_desc")).clicked() {
                        commit = true;
                    }
                });
            });
        });

    if commit {
        if let (Some(v), Some(p)) = (parse_hex(&d.vid), parse_hex(&d.pid)) {
            let key = format!("{v:04x}:{p:04x}");
            state
                .controllers
                .controllers
                .entry(key.clone())
                .or_default();
            state.dirty_controllers = true;
            state.status = Some(format!("{} {key}.", tr("controllers.add.added_status")));
        }
        d = AddDialog::default(); // closed + cleared
        set_add_dialog(ui, d);
    } else if close {
        set_add_dialog(ui, AddDialog::default());
    } else {
        // Persist edits made this frame (text fields, etc.).
        set_add_dialog(ui, d);
    }
}

// ---------------------------------------------------------------------------
// Save-as / rename preset dialog
// ---------------------------------------------------------------------------

fn draw_preset_dialog(ui: &mut egui::Ui, state: &mut PanelState) {
    let mut d = preset_dialog(ui);
    if !d.open {
        return;
    }
    let ctx = ui.ctx().clone();
    let mut close = false;
    let mut commit = false;

    let title = if d.mode == "rename" {
        tr("controllers.preset_dialog.rename_title")
    } else {
        tr("controllers.preset_dialog.save_title")
    };

    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(&ctx, |ui| {
            ui.set_min_width(320.0);
            ui.horizontal(|ui| {
                ui.label(tr("controllers.preset_dialog.name_label"));
                ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("Couch flick aim"))
                    .describe(tr("controllers.preset_dialog.name_desc"));
            });

            let trimmed = d.name.trim().to_string();
            let clashes = !trimmed.is_empty()
                && trimmed != d.original
                && state.controllers.presets.contains_key(&trimmed);
            if clashes {
                ui.label(
                    egui::RichText::new(tr("controllers.preset_dialog.clashes"))
                        .small()
                        .color(theme::warn()),
                )
                .describe(tr("controllers.preset_dialog.clashes_desc"));
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(tr("controllers.preset_dialog.cancel"))
                    .describe(tr("controllers.preset_dialog.cancel_desc"))
                    .clicked()
                {
                    close = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = ui.add_enabled(
                        !trimmed.is_empty() && !clashes,
                        egui::Button::new(tr("controllers.preset_dialog.save")),
                    );
                    if ok
                        .describe(tr("controllers.preset_dialog.save_desc"))
                        .clicked()
                    {
                        commit = true;
                    }
                });
            });
        });

    if commit {
        let name = d.name.trim().to_string();
        if d.mode == "rename" {
            if let Some(profile) = state.controllers.presets.remove(&d.original) {
                state.controllers.presets.insert(name.clone(), profile);
                state.dirty_controllers = true;
                state.status = Some(format!(
                    "{} “{name}”.",
                    tr("controllers.preset_dialog.renamed_status")
                ));
            }
        } else {
            // "save": copy the source controller's current profile (or a default
            // if the source vanished) into a new preset.
            let profile = state
                .controllers
                .controllers
                .get(&d.source_key)
                .cloned()
                .unwrap_or_default();
            state.controllers.presets.insert(name.clone(), profile);
            state.dirty_controllers = true;
            state.status = Some(format!(
                "{} “{name}”.",
                tr("controllers.preset_dialog.saved_status")
            ));
        }
        set_preset_dialog(ui, PresetDialog::default());
    } else if close {
        set_preset_dialog(ui, PresetDialog::default());
    } else {
        set_preset_dialog(ui, d);
    }
}

// ---------------------------------------------------------------------------
// vid:pid parsing
// ---------------------------------------------------------------------------

/// Parse a single 4-hex-digit field (tolerates a leading `0x` and whitespace).
fn parse_hex(s: &str) -> Option<u16> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if t.is_empty() || t.len() > 4 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(t, 16).ok()
}

/// Parse a canonical `"vvvv:pppp"` key into `(vendor, product)`. Unparseable
/// halves fall back to 0 so the card still renders (as a Generic pad).
fn parse_vidpid(key: &str) -> (u16, u16) {
    let mut it = key.splitn(2, ':');
    let v = it.next().and_then(parse_hex).unwrap_or(0);
    let p = it.next().and_then(parse_hex).unwrap_or(0);
    (v, p)
}

/// Effective on-screen name for a controller: a user-given alias (host-side
/// or panel-side — they share the field) wins, else the classifier label.
/// Used both for the inline editor's seed text and for the sort comparator.
fn effective_name(state: &PanelState, key: &str, vid: u16, pid: u16) -> String {
    let from_profile = state
        .controllers
        .controllers
        .get(key)
        .and_then(|p| p.alias.clone());
    let from_live = live_status(key).and_then(|s| s.alias);
    from_live
        .or(from_profile)
        .unwrap_or_else(|| classify(vid, pid, "").kind.label().to_string())
}

/// The live `ControllerStatus` for `key` (from the host's last status push),
/// or `None` if there's no live host or the controller hasn't been seen.
fn live_status(key: &str) -> Option<control_ipc::ControllerStatus> {
    let s = crate::control::handle().status()?;
    let (v, p) = parse_vidpid(key);
    s.controllers
        .into_iter()
        .find(|c| c.vendor == v && c.product == p)
}

/// The list of controller keys to render, in display order: every saved
/// controller plus every live-but-unsaved controller, sorted by
/// (connected desc, last_connected_ms desc, effective_name asc).
fn ordered_controller_keys(state: &PanelState) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut keys: BTreeSet<String> = state.controllers.controllers.keys().cloned().collect();
    if let Some(status) = crate::control::handle().status() {
        for c in &status.controllers {
            keys.insert(format!("{:04x}:{:04x}", c.vendor, c.product));
        }
    }
    let mut v: Vec<String> = keys.into_iter().collect();
    v.sort_by(|a, b| {
        let (av, ap) = parse_vidpid(a);
        let (bv, bp) = parse_vidpid(b);
        let als = live_status(a);
        let bls = live_status(b);
        let aconn = als.as_ref().map(|s| s.connected).unwrap_or(false);
        let bconn = bls.as_ref().map(|s| s.connected).unwrap_or(false);
        // Connected first.
        match bconn.cmp(&aconn) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        // Then most-recently-seen first.
        let alast = als.as_ref().and_then(|s| s.last_connected_ms).unwrap_or(0);
        let blast = bls.as_ref().and_then(|s| s.last_connected_ms).unwrap_or(0);
        match blast.cmp(&alast) {
            std::cmp::Ordering::Equal => {}
            ord => return ord,
        }
        // Finally alphabetical by effective display name.
        effective_name(state, a, av, ap).cmp(&effective_name(state, b, bv, bp))
    });
    v
}

/// Inline "USB" / "BT" pill rendered next to the controller name. Mirrors the
/// Trireme Android card's bus icon: green wash when the controller is live on
/// that bus, gray when saved/disconnected. Hidden entirely on
/// [`control_ipc::ControllerBus::Unknown`] (no live evdev → no pill, cleaner
/// row) — the panel only knows the bus if the host actually saw the device.
fn bus_pill(ui: &mut egui::Ui, bus: control_ipc::ControllerBus, connected: bool) {
    use control_ipc::ControllerBus as Bus;
    let text = match bus {
        Bus::Usb => "USB",
        Bus::Bluetooth => "BT",
        Bus::Unknown => return,
    };
    let color = if connected {
        // Matches the liveness-dot green so the eye reads "this is on".
        egui::Color32::from_rgb(70, 180, 90)
    } else {
        egui::Color32::from_rgb(120, 120, 120)
    };
    theme::badge(ui, text, color);
}

/// A small filled liveness dot rendered inline before the controller name.
/// Green = connected, gray = saved/disconnected. The colors mirror the Trireme
/// Android client's `Dot()` so the same controller looks the same on both.
fn liveness_dot(ui: &mut egui::Ui, connected: bool) {
    let color = if connected {
        egui::Color32::from_rgb(70, 180, 90)
    } else {
        egui::Color32::from_rgb(120, 120, 120)
    };
    let size = egui::vec2(10.0, 10.0);
    let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

/// Inline alias editor for a controller. When idle, renders the effective name
/// as a clickable label; clicking it puts the card into edit mode (a single-line
/// TextEdit + commit/cancel buttons). Commit dispatches a
/// [`crate::control::set_controller_alias`] over IPC and removes the draft;
/// cancel just drops the draft. Enter = commit, Escape = cancel.
fn alias_inline_editor(
    ui: &mut egui::Ui,
    state: &mut PanelState,
    key: &str,
    vid: u16,
    pid: u16,
    label_fallback: &str,
) {
    // Determine "are we editing?" from the draft map; otherwise just render
    // the effective name as a clickable label. Two-step (check then mutate)
    // because the IPC call needs to outlive the borrow on `state`.
    let is_editing = state.controllers.alias_draft.contains_key(key);

    if !is_editing {
        let display = effective_name(state, key, vid, pid);
        // Render the truncated form; storage still holds the full alias so an
        // edit click still seeds the editor with the raw string.
        let shown = crate::widgets::truncate_for_card(&display);
        let resp = ui
            .add(
                egui::Label::new(egui::RichText::new(shown.as_ref()).strong().size(15.0))
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(tr("controllers.card.kind_desc"));
        if resp.clicked() {
            // Seed the draft with the currently-displayed effective name.
            state
                .controllers
                .alias_draft
                .insert(key.to_string(), display);
        }
        return;
    }

    // Editing — own the draft string mutably for the TextEdit, then put it back
    // (or commit / discard) after the row is built.
    let mut draft = state
        .controllers
        .alias_draft
        .get(key)
        .cloned()
        .unwrap_or_default();
    let mut action: Option<bool> = None; // Some(true) = commit, Some(false) = cancel
    let resp = ui.add(
        egui::TextEdit::singleline(&mut draft)
            .desired_width(160.0)
            .hint_text(label_fallback),
    );
    // Enter / Escape semantics inside the field.
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        action = Some(true);
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = Some(false);
    }
    if ui
        .add(
            egui::Button::new(
                egui::RichText::new("\u{2713}").color(egui::Color32::from_rgb(70, 180, 90)),
            )
            .small(),
        )
        .clicked()
    {
        action = Some(true);
    }
    if ui
        .add(
            egui::Button::new(
                egui::RichText::new("\u{2717}").color(egui::Color32::from_rgb(200, 80, 80)),
            )
            .small(),
        )
        .clicked()
    {
        action = Some(false);
    }

    match action {
        Some(true) => {
            // Commit: dispatch + drop the draft. Trim; empty → clear.
            let trimmed = draft.trim().to_string();
            let alias = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
            crate::control::set_controller_alias(vid, pid, alias);
            state.controllers.alias_draft.remove(key);
        }
        Some(false) => {
            state.controllers.alias_draft.remove(key);
        }
        None => {
            // Keep editing — write the in-progress draft back.
            state.controllers.alias_draft.insert(key.to_string(), draft);
        }
    }
}

// ---------------------------------------------------------------------------
// Per-controller preset actions (Apply / Save-as) — appended to each card.
// ---------------------------------------------------------------------------

/// The Apply-preset picker + "Save as preset…" button for one controller.
/// Returns true if the controller's profile was mutated (caller marks dirty).
fn controller_preset_actions(ui: &mut egui::Ui, state: &mut PanelState, key: &str) -> bool {
    let mut changed = false;
    let preset_names: Vec<String> = state.controllers.presets.keys().cloned().collect();

    ui.horizontal(|ui| {
        // Apply-preset combo (only if there are presets).
        if preset_names.is_empty() {
            ui.add_enabled(false, egui::Button::new(tr("controllers.apply.button")))
                .describe(tr("controllers.apply.none_desc"));
        } else {
            let sel_id = egui::Id::new(("apply_sel", key));
            let mut sel: ApplySel = ui
                .ctx()
                .data_mut(|d| d.get_temp::<ApplySel>(sel_id).unwrap_or_default());
            let shown = if sel.is_empty() {
                tr("controllers.apply.choose")
            } else {
                sel.clone()
            };
            egui::ComboBox::from_id_salt(sel_id)
                .selected_text(shown)
                .width(160.0)
                .show_ui(ui, |ui| {
                    for n in &preset_names {
                        ui.selectable_value(&mut sel, n.clone(), n)
                            .describe(tr("controllers.apply.combo_desc"));
                    }
                })
                .response
                .describe(tr("controllers.apply.combo_desc"));
            ui.ctx().data_mut(|d| d.insert_temp(sel_id, sel.clone()));

            let can_apply = !sel.is_empty();
            if ui
                .add_enabled(
                    can_apply,
                    egui::Button::new(tr("controllers.apply.confirm")),
                )
                .describe(tr("controllers.apply.confirm_desc"))
                .clicked()
            {
                if let Some(p) = state.controllers.presets.get(&sel).cloned() {
                    if let Some(profile) = state.controllers.controllers.get_mut(key) {
                        // Preset overwrites tuning; preserve the per-controller alias
                        // (renames live in the controllers map, not in presets).
                        let saved_alias = profile.alias.clone();
                        *profile = p;
                        profile.alias = saved_alias;
                        changed = true;
                        state.status = Some(format!(
                            "{} “{sel}” {} {key}.",
                            tr("controllers.apply.applied_status"),
                            tr("controllers.apply.applied_to")
                        ));
                    }
                }
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(tr("controllers.apply.save_as"))
                .describe(tr("controllers.apply.save_as_desc"))
                .clicked()
            {
                let d = PresetDialog {
                    open: true,
                    mode: "save".into(),
                    name: String::new(),
                    source_key: key.to_string(),
                    original: String::new(),
                };
                set_preset_dialog(ui, d);
            }
        });
    });

    changed
}

#[cfg(test)]
mod tests {
    use super::{accel_multiplier, GYRO_DEFAULTS};

    /// The plotted curve must match the host's `gyro_imu::accel_gain` so the
    /// preview isn't lying: unity at rest, `1 + accel` at the knee, clamped at
    /// `1 + accel_max`, and monotonic non-decreasing.
    #[test]
    fn accel_curve_matches_host_formula() {
        let g = GYRO_DEFAULTS; // accel 2.5, pow 2.0, ref 110, max 10
        assert_eq!(accel_multiplier(&g, 0.0), 1.0);
        // At the knee, (ref/ref)^pow = 1, so the gain is 1 + accel.
        assert!((accel_multiplier(&g, g.accel_ref) - (1.0 + g.accel)).abs() < 1e-4);
        // Very fast rotation saturates at the clamp ceiling.
        assert!((accel_multiplier(&g, 100_000.0) - (1.0 + g.accel_max)).abs() < 1e-4);
        // Monotonic non-decreasing in speed.
        assert!(accel_multiplier(&g, 50.0) <= accel_multiplier(&g, 200.0));
    }

    #[test]
    fn accel_disabled_is_unity() {
        let mut g = GYRO_DEFAULTS;
        g.accel = 0.0;
        assert_eq!(accel_multiplier(&g, 300.0), 1.0);
    }
}
