// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Compositor tab — the marquee feature: a drag-and-drop virtual-display
//! layout editor that writes the compositor's `multi.toml` (`MultiCfg` /
//! `OutputCfg`).
//!
//! Layout:
//!   * a left **palette** of available Triremes (from `state.peers`) plus a
//!     manual-address affordance — each entry is an egui drag *source*;
//!   * a central **canvas** that renders the composite virtual-desktop space as
//!     a scaled mini-map. It is an egui drop *zone*: dragging a client (or the
//!     "blank output" chip) onto it spawns an `OutputTile` at the drop point.
//!     Tiles can be moved by dragging them around the canvas and removed with a
//!     per-tile close (X) button;
//!   * an **editor list** below with numeric `DragValue`s for w/h/x/y/refresh, a
//!     "snap right of selected" helper for one-click side-by-side spans, and an
//!     Apply / Clear row.
//!
//! All state lives in `state.layout` (`crate::model::CompositorLayout`); we
//! mutate it in place and flip `state.dirty_layout`. The active
//! `state.runtime.resolution_policy` drives how new tiles size + snap, and is
//! surfaced with a hint pointing at the Settings tab.
//!
//! Drag-and-drop uses the egui 0.29 API verified present in Cargo.lock:
//! `Ui::dnd_drag_source`, `Ui::dnd_drop_zone`, `egui::DragAndDrop`, plus a
//! manual `Sense::drag()` interaction for moving tiles inside the canvas (the
//! drop-zone payload only tells us *that* a drop happened, so we read the
//! pointer position ourselves to place the new tile).

use std::collections::BTreeSet;

use egui::{Align2, Color32, FontId, Pos2, Rect, Rounding, Sense, Stroke, Vec2};
use egui_phosphor::regular as ph;

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::{OutputTile, PanelState, RefreshPolicy, ResolutionPolicy};

/// What a palette entry hands to the canvas when dropped. Cheap to clone; held
/// by egui in an `Arc` for the duration of the drag.
#[derive(Clone)]
enum DropPayload {
    /// A paired Trireme: (display name, pubkey).
    Peer { name: String, pubkey: String },
    /// A manually-entered sink address.
    Manual { name: String, addr: String },
    /// A blank 1080p output with no peer pinned.
    Blank,
}

/// egui temp-memory keys (namespaced) so transient UI state survives frames
/// without polluting the persisted model.
fn id_maximized() -> egui::Id {
    egui::Id::new("compositor_maximized")
}
fn id_selected() -> egui::Id {
    egui::Id::new("compositor_selected_tile")
}
/// The multi-selection SET (distinct from the single `id_selected` primary used
/// by the editor panel). Holds every tile in the current selection so the
/// Arrange tools can operate on more than one display at once.
fn id_selset() -> egui::Id {
    egui::Id::new("compositor_selected_set")
}
fn id_manual_name() -> egui::Id {
    egui::Id::new("compositor_manual_name")
}

/// Read the multi-selection set from egui temp memory (empty if unset).
fn read_selset(ui: &egui::Ui) -> BTreeSet<u64> {
    ui.ctx()
        .data(|d| d.get_temp::<BTreeSet<u64>>(id_selset()))
        .unwrap_or_default()
}

/// Persist the multi-selection set into egui temp memory.
fn write_selset(ui: &egui::Ui, set: &BTreeSet<u64>) {
    ui.ctx()
        .data_mut(|d| d.insert_temp(id_selset(), set.clone()));
}

/// Drop any selection-set members that no longer correspond to a live tile
/// (e.g. after a remove / clear / reload), so the Arrange tools never act on
/// stale ids. Returns the cleaned set.
fn prune_selset(ui: &egui::Ui, state: &PanelState) -> BTreeSet<u64> {
    let live: BTreeSet<u64> = state.layout.outputs.iter().map(|o| o.id).collect();
    let mut set = read_selset(ui);
    let before = set.len();
    set.retain(|id| live.contains(id));
    if set.len() != before {
        write_selset(ui, &set);
    }
    set
}
fn id_manual_addr() -> egui::Id {
    egui::Id::new("compositor_manual_addr")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    // Keep peer-pinned tiles bound to their device: the output name follows the
    // peer's alias, and the tile size follows the device's last-reported
    // resolution. (Both are "device-reported" — the device is the source of
    // truth for its own name and mode.)
    sync_peer_tiles(state);

    ui.horizontal(|ui| {
        ui.heading(tr("compositor.heading"))
            .describe(tr("compositor.intro"));
        crate::widgets::status_badge(
            ui,
            (
                tr("compositor.badge.experimental").as_str(),
                crate::theme::warn(),
            ),
        );
    });
    ui.add_space(8.0);

    mirror_section(ui);
    ui.add_space(8.0);

    presets_section(ui, state);
    ui.add_space(8.0);

    // The "Maximize canvas" toggle lives in egui temp memory so it resets on
    // restart but persists while the panel is open. The toggle itself is a small
    // icon in the canvas pane's toolbar (canvas_toolbar), not a separate row.
    let mut maximized = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(id_maximized()))
        .unwrap_or(false);

    // --- Palette + canvas pane ---------------------------------------------
    if maximized {
        // Full-width canvas pane, palette folded into a top strip.
        palette_strip(ui, state);
        ui.add_space(6.0);
        canvas_pane(
            ui,
            state,
            &mut maximized,
            ui.available_height().max(260.0) - 8.0,
        );
    } else {
        // Side-by-side: palette on the left (natural height — no inner scrollbar,
        // it flows into the tab's own scroll), canvas pane filling the rest.
        let avail_h = ui.available_height().max(320.0);
        let canvas_h = (avail_h - 220.0).max(260.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(230.0, 0.0),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    ui.set_width(230.0);
                    palette_column(ui, state);
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                canvas_pane(ui, state, &mut maximized, canvas_h);
            });
        });
    }
    ui.ctx()
        .data_mut(|d| d.insert_temp(id_maximized(), maximized));

    // Layout warnings (duplicate sinks, overlaps, over-budget) + the dirty-flag
    // reconcile. The Clear/Reload + mode actions now live in the canvas toolbar.
    ui.add_space(8.0);
    validation_row(ui, state);

    // --- Per-display cards -------------------------------------------------
    ui.add_space(10.0);
    crate::theme::section(
        ui,
        ph::MONITOR,
        &tr("compositor.displays.heading"),
        &tr("compositor.displays.heading_desc"),
    );
    tile_editors(ui, state);

    ui.add_space(10.0);
    play_media_card(ui, state);

    // --- Expandable settings cards, in order: Policy, Performance, About ----
    ui.add_space(12.0);
    collapsing_card(
        ui,
        "compositor_display_policy",
        ph::MONITOR,
        &tr("compositor.policy.title"),
        |ui| display_policy_section(ui, state),
    );
    ui.add_space(6.0);
    collapsing_card(
        ui,
        "compositor_perf",
        ph::PULSE,
        &tr("compositor.perf.title"),
        performance_section,
    );
    ui.add_space(6.0);
    collapsing_card(
        ui,
        "compositor_about",
        ph::INFO,
        &tr("compositor.footnote.heading"),
        about_section,
    );

    // Keep ticking so the live device geometry + palette badges stay fresh.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs(1));
}

/// An expandable "card" — a [`crate::widgets::card`] whose sole content is a
/// `CollapsingHeader`, so the whole thing reads as a titled card that folds open.
/// Used for the bottom-of-tab settings (Display Policy / Performance / About).
fn collapsing_card(
    ui: &mut egui::Ui,
    id: &'static str,
    icon: &str,
    title: &str,
    body: impl FnOnce(&mut egui::Ui),
) {
    crate::widgets::card(ui, id, |ui| {
        egui::CollapsingHeader::new(egui::RichText::new(format!("{icon}  {title}")).strong())
            .id_salt(id)
            .default_open(false)
            .show_unindented(ui, body);
    });
}

/// Keep each peer-pinned tile bound to its device: name follows the peer alias,
/// size follows the device's last-reported resolution (`device_geoms`). Flags
/// the layout dirty when anything changed, so a renegotiated resolution persists
/// on the next save. A no-op for blank/manual tiles (no `peer_pubkey`).
/// Push each peer-pinned tile's input-enabled state to the host the moment its
/// device transitions to connected. The host's per-device input gate is runtime
/// state (cleared on host restart), so without this a device that reconnects
/// would forward input even if its card is toggled off in the panel. We diff the
/// currently-connected pubkeys against last frame's (stashed in egui temp memory)
/// and assert only the newly-connected ones, so it's one push per connect — not
/// per frame.
///
/// Driven from `app::update` EVERY frame regardless of the active tab — an
/// unattended host restart (Relaunch, GPU-fault watchdog, crash) can land on any
/// tab and the gate must be re-asserted then, not only when the Compositor tab is
/// open. Takes the egui `Context` (not a `Ui`) so it runs outside any panel.
pub fn reassert_input_on_connect(ctx: &egui::Context, state: &PanelState) {
    let connected_now: std::collections::BTreeSet<String> = crate::control::handle()
        .status()
        .map(|s| {
            s.clients
                .iter()
                .filter(|c| c.connected)
                .filter_map(|c| c.pubkey.clone())
                .collect()
        })
        .unwrap_or_default();
    let id = egui::Id::new("compositor_input_asserted");
    let prev: std::collections::BTreeSet<String> = ctx.data(|d| d.get_temp(id).unwrap_or_default());
    for pk in connected_now.difference(&prev) {
        if let Some(tile) = state
            .layout
            .outputs
            .iter()
            .find(|o| o.peer_pubkey.as_deref() == Some(pk.as_str()))
        {
            crate::control::set_input_enabled(pk, tile.input_enabled);
        }
    }
    if connected_now != prev {
        ctx.data_mut(|d| d.insert_temp(id, connected_now));
    }
}

fn sync_peer_tiles(state: &mut PanelState) {
    let mut changed = false;
    for tile in &mut state.layout.outputs {
        let Some(pk) = tile.peer_pubkey.clone() else {
            continue;
        };
        // Output name ← device (peer) name. The compositor uses the name as an
        // identifier, so run it through the same sanitiser used at drop time.
        if let Some(peer) = state.peers.iter().find(|p| p.pubkey == pk) {
            let synced = sanitize_name(&peer.display_name());
            if !synced.is_empty() && tile.name != synced {
                tile.name = synced;
                changed = true;
            }
        }
        // Tile size ← device's last-reported resolution.
        if let Some(g) = state.device_geoms.get(&pk) {
            if g.w > 0 && g.h > 0 && (tile.w, tile.h) != (g.w, g.h) {
                tile.w = g.w;
                tile.h = g.h;
                changed = true;
            }
        }
    }
    if changed {
        state.dirty_layout = true;
    }
}

// ---------------------------------------------------------------------------
// Toolbar: fps, resolution-policy hint, maximize toggle
// ---------------------------------------------------------------------------

/// Display policy: how new tiles are sized + snapped on the canvas (resolution +
/// refresh). Panel-local — the host never reads it; it only shapes the geometry
/// you produce here. Moved from the Settings tab so it lives where its effects do.
/// (Keys stay in the `settings.resolution.*` namespace to avoid churn.)
/// Compositor presets bar: save / load / delete named layout snapshots. Each
/// preset captures the full layout — display setup, tile positions, AND the
/// per-device input-enabled flags — so the user can flip between arrangements
/// (e.g. "couch co-op" vs "solo desk") in one click. Persisted to
/// `compositor-presets.toml`; loading one applies the layout (positions take
/// effect on the next Save like any layout edit) and pushes the input flags live.
fn presets_section(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "compositor_presets_card", |ui| {
        ui.label(egui::RichText::new(tr("compositor.presets.heading")).strong())
            .describe(tr("compositor.presets.heading_desc"));
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let names: Vec<String> = state.presets.presets.keys().cloned().collect();
            let cur = if state.presets.selected.is_empty() {
                tr("compositor.presets.none")
            } else {
                state.presets.selected.clone()
            };
            egui::ComboBox::from_id_salt("compositor_preset_combo")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for n in &names {
                        ui.selectable_value(&mut state.presets.selected, n.clone(), n);
                    }
                });
            let has_sel = state.presets.presets.contains_key(&state.presets.selected);
            if ui
                .add_enabled(has_sel, egui::Button::new(tr("compositor.presets.load")))
                .clicked()
            {
                if let Some(preset) = state.presets.presets.get(&state.presets.selected).cloned() {
                    apply_preset(ui, state, preset);
                }
            }
            if ui
                .add_enabled(has_sel, egui::Button::new(tr("compositor.presets.delete")))
                .clicked()
            {
                let name = state.presets.selected.clone();
                state.presets.presets.remove(&name);
                state.presets.selected.clear();
                state.dirty_presets = true;
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut state.presets.draft_name)
                    .hint_text(tr("compositor.presets.name_hint"))
                    .desired_width(150.0),
            );
            let name = state.presets.draft_name.trim().to_string();
            if ui
                .add_enabled(
                    !name.is_empty(),
                    egui::Button::new(tr("compositor.presets.save")),
                )
                .clicked()
            {
                // Snapshot the CURRENT layout (setup + positions + input flags).
                state
                    .presets
                    .presets
                    .insert(name.clone(), state.layout.clone());
                state.presets.selected = name;
                state.presets.draft_name.clear();
                state.dirty_presets = true;
            }
        });
    });
}

/// Apply a preset: swap in its layout (dirty → persists/applies via the normal
/// Save flow, so positions don't disrupt a live stream mid-toggle) and push each
/// device's input-enabled flag to the running host so the gate matches at once.
///
/// NOTE: for peer-pinned tiles only POSITIONS (x/y) and `input_enabled` are truly
/// snapshotted — `sync_peer_tiles` re-binds name + size to the live device each
/// frame, so a preset can't pin a stale name/resolution against a device.
fn apply_preset(ui: &egui::Ui, state: &mut PanelState, preset: crate::model::CompositorLayout) {
    state.layout = preset;
    state.dirty_layout = true;
    // The new layout has fresh ids — drop any stale selection so the editor card
    // / arrange tools don't act on a tile id that no longer exists.
    ui.ctx().data_mut(|d| d.insert_temp(id_selected(), 0u64));
    write_selset(ui, &BTreeSet::new());
    for o in &state.layout.outputs {
        if let Some(pk) = o.peer_pubkey.as_deref().filter(|s| !s.is_empty()) {
            crate::control::set_input_enabled(pk, o.input_enabled);
        }
    }
}

/// Mirror-mode toggle + the "quality limited" throttle badge.
///
/// The user asked that the panel make it unmistakable, when mirror is turned on,
/// that resolution + frame rate are throttled to the weakest client (mirror
/// shares one encode across all clients). We surface that as a warn badge with
/// the real clamp numbers. While the transport fan-out is still being built we
/// also show an honest "engine not yet live" note so the toggle never overstates
/// behaviour. See `MIRROR_EXTEND_PLAN.md`.
fn mirror_section(ui: &mut egui::Ui) {
    let connected = crate::control::handle().connected();
    // Gated on the host's advertised stream-mode capability (issue #22) — the
    // Windows host is single-client only and doesn't serve SetStreamMode.
    let can_mode = crate::control::host_caps().stream_mode;
    let mode = crate::control::handle().status().map(|s| s.stream_mode);
    let mut mirror_on = matches!(mode, Some(control_ipc::StreamMode::Mirror { .. }));

    ui.horizontal(|ui| {
        let resp = ui
            .add_enabled(
                connected && can_mode,
                egui::Checkbox::new(&mut mirror_on, tr("compositor.mirror.label")),
            )
            .describe(tr("compositor.mirror.desc"))
            .on_disabled_hover_text(if connected && !can_mode {
                tr("control.no_cap.stream_mode")
            } else {
                crate::control::unavailable_reason()
            });
        if resp.changed() {
            crate::control::set_stream_mode(mirror_on);
        }
    });

    if let Some(control_ipc::StreamMode::Mirror { clamp }) = mode {
        // Active throttle (≥2 clients sharing) vs merely armed (clamp not biting yet).
        if clamp.client_count >= 2 {
            crate::widgets::status_badge(
                ui,
                (
                    tr("compositor.mirror.throttle_title").as_str(),
                    crate::theme::warn(),
                ),
            );
        } else {
            ui.label(
                egui::RichText::new(tr("compositor.mirror.armed"))
                    .small()
                    .color(crate::theme::info()),
            );
        }
        // The real numbers the single shared encode is producing right now.
        let mut detail = format!(
            "{}: {}×{} · {} fps · {} kbps · {} client(s)",
            tr("compositor.mirror.encoding"),
            clamp.w,
            clamp.h,
            clamp.fps,
            clamp.bitrate_kbps,
            clamp.client_count,
        );
        if let Some(by) = clamp.limited_by.as_deref() {
            detail.push_str(&format!(" — {} {by}", tr("compositor.mirror.capped_by")));
        }
        ui.label(egui::RichText::new(detail).small().weak());
        // Honest status while the fan-out engine is still being built (Stage 2).
        ui.label(
            egui::RichText::new(tr("compositor.mirror.pending"))
                .small()
                .weak()
                .italics(),
        );
    }
}

fn display_policy_section(ui: &mut egui::Ui, state: &mut PanelState) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(tr("settings.resolution.scaling")).strong())
        .describe(tr("settings.resolution.desc"));
    for p in ResolutionPolicy::all() {
        let recommended = p == ResolutionPolicy::NativePerDisplay;
        let selected = state.runtime.resolution_policy == p;
        ui.horizontal(|ui| {
            let mut label = egui::RichText::new(p.label());
            if selected {
                label = label.strong();
            }
            if ui.radio(selected, label).describe(p.describe()).clicked() && !selected {
                state.runtime.resolution_policy = p;
                state.dirty_runtime = true;
            }
            if recommended {
                crate::theme::badge(
                    ui,
                    &tr("settings.resolution.recommended"),
                    crate::theme::online(),
                );
            }
        });
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);

    ui.label(egui::RichText::new(tr("settings.resolution.refresh")).strong());
    for p in RefreshPolicy::all() {
        let recommended = p == RefreshPolicy::LowestCommon;
        let selected = state.runtime.refresh_policy == p;
        ui.horizontal(|ui| {
            let mut label = egui::RichText::new(p.label());
            if selected {
                label = label.strong();
            }
            if ui.radio(selected, label).describe(p.describe()).clicked() && !selected {
                state.runtime.refresh_policy = p;
                state.dirty_runtime = true;
            }
            if recommended {
                crate::theme::badge(
                    ui,
                    &tr("settings.resolution.recommended"),
                    crate::theme::online(),
                );
            }
            // The shared frame rate lives inline with "Lowest common": one pacer
            // drives every output, so the FPS is a single layout-wide value.
            if p == RefreshPolicy::LowestCommon {
                ui.add_space(8.0);
                ui.separator();
                ui.label(
                    egui::RichText::new(tr("compositor.toolbar.fps"))
                        .small()
                        .weak(),
                );
                let mut fps = state.layout.fps;
                let resp = ui
                    .add(
                        egui::DragValue::new(&mut fps)
                            .speed(1.0)
                            .range(15..=480)
                            .suffix(" fps"),
                    )
                    .describe(tr("compositor.toolbar.fps_desc"));
                if resp.changed() {
                    state.layout.fps = fps.clamp(1, 1000);
                    // The compositor derives one pacer from `fps`; keep each
                    // tile's display-only refresh in step.
                    for o in &mut state.layout.outputs {
                        o.refresh_mhz = state.layout.fps * 1000;
                    }
                    state.dirty_layout = true;
                }
            }
        });
    }

    ui.add_space(10.0);
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .rounding(Rounding::same(6.0))
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(tr("settings.resolution.why_title"))
                    .strong()
                    .small(),
            );
            ui.label(
                egui::RichText::new(tr("settings.resolution.why_body"))
                    .small()
                    .weak(),
            );
        });
}

/// The canvas pane: a one-line action toolbar (arrangement modes + Clear/Reload
/// + a Maximize icon) above the canvas itself.
fn canvas_pane(ui: &mut egui::Ui, state: &mut PanelState, maximized: &mut bool, height: f32) {
    canvas_toolbar(ui, state, maximized);
    ui.add_space(6.0);
    canvas(ui, state, height);
}

/// The action toolbar that sits atop the canvas. Arrangement modes
/// (Mirror/Extend/Single, shown once there's something to arrange), the
/// Clear/Reload discard actions, an optional UniformLcd Equalize, and a compact
/// Maximize/restore icon on the right. Mutations are deferred past the immutable
/// button pass.
fn canvas_toolbar(ui: &mut egui::Ui, state: &mut PanelState, maximized: &mut bool) {
    let (mut do_mirror, mut do_extend, mut do_single) = (false, false, false);
    let (mut do_clear, mut do_reload, mut do_equalize) = (false, false, false);
    let has_outputs = !state.layout.outputs.is_empty();

    ui.horizontal_wrapped(|ui| {
        if has_outputs {
            if ui
                .button(format!("{}  {}", ph::COPY, tr("compositor.modes.mirror")))
                .describe(tr("compositor.modes.mirror_desc"))
                .clicked()
            {
                do_mirror = true;
            }
            if ui
                .button(format!(
                    "{}  {}",
                    ph::ARROW_LINE_RIGHT,
                    tr("compositor.modes.extend")
                ))
                .describe(tr("compositor.modes.extend_desc"))
                .clicked()
            {
                do_extend = true;
            }
            if ui
                .button(format!("{}  {}", ph::SQUARE, tr("compositor.modes.single")))
                .describe(tr("compositor.modes.single_desc"))
                .clicked()
            {
                do_single = true;
            }
            ui.separator();
            if ui
                .button(tr("compositor.apply.clear"))
                .describe(tr("compositor.apply.clear_desc"))
                .clicked()
            {
                do_clear = true;
            }
        }
        if ui
            .button(tr("compositor.apply.reload"))
            .describe(tr("compositor.apply.reload_desc"))
            .clicked()
        {
            do_reload = true;
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Inset from the right edge so the tab's (overlay) scrollbar can't sit
            // on top of the first right-aligned control — the Maximize icon.
            ui.add_space(ui.spacing().scroll.bar_width + 10.0);
            let icon = if *maximized {
                ph::ARROWS_IN
            } else {
                ph::ARROWS_OUT
            };
            if ui
                .button(egui::RichText::new(icon).size(16.0))
                .describe(tr("compositor.toolbar.maximize_desc"))
                .clicked()
            {
                *maximized = !*maximized;
            }
            if state.runtime.resolution_policy == ResolutionPolicy::UniformLcd
                && state.layout.outputs.len() > 1
                && ui
                    .button(tr("compositor.toolbar.equalize"))
                    .describe(tr("compositor.toolbar.equalize_desc"))
                    .clicked()
            {
                do_equalize = true;
            }
        });
    });

    if do_mirror {
        apply_mirror(state);
    }
    if do_extend {
        apply_extend(state);
    }
    if do_single {
        apply_single(ui, state);
    }
    if do_equalize {
        equalize_uniform(state);
    }
    if do_clear {
        state.layout.outputs.clear();
        state.dirty_layout = true;
        ui.ctx().data_mut(|d| d.insert_temp(id_selected(), 0u64));
        write_selset(ui, &BTreeSet::new());
        state.status = Some(tr("compositor.status.cleared"));
    }
    if do_reload {
        state.layout = crate::data::load_compositor_layout();
        state.dirty_layout = false;
        ui.ctx().data_mut(|d| d.insert_temp(id_selected(), 0u64));
        write_selset(ui, &BTreeSet::new());
        state.status = Some(tr("compositor.status.reloaded"));
    }
}

/// Reconcile the per-frame "layout changed" flag the DragValue helpers set, then
/// surface any soft layout problems (duplicate sinks, overlaps, over-budget).
fn validation_row(ui: &mut egui::Ui, state: &mut PanelState) {
    let changed = ui
        .ctx()
        .data_mut(|d| d.remove_temp::<bool>(egui::Id::new("compositor_changed_this_frame")))
        .unwrap_or(false);
    if changed {
        state.dirty_layout = true;
    }
    validation_hints(ui, state);
}

/// Clamp every output to the smallest existing tile's mode — the UniformLcd
/// policy made concrete. No-op if there are fewer than two outputs.
fn equalize_uniform(state: &mut PanelState) {
    let smallest = state
        .layout
        .outputs
        .iter()
        .map(|o| (o.w, o.h))
        .min_by_key(|(w, h)| (*w as u64) * (*h as u64));
    if let Some((w, h)) = smallest {
        for o in &mut state.layout.outputs {
            // A device tile's size is dictated by the device — never resize it
            // (sync_peer_tiles would revert it next frame anyway). Only free-form
            // tiles take the shared LCD size.
            if o.peer_pubkey.is_none() {
                o.w = w;
                o.h = h;
            }
        }
        state.dirty_layout = true;
        state.status = Some(format!(
            "{} {w}×{h} {}",
            tr("compositor.status.equalized_prefix"),
            tr("compositor.status.equalized_suffix")
        ));
    }
}

// ---------------------------------------------------------------------------
// Palette — the draggable client list
// ---------------------------------------------------------------------------

/// Left-column palette (windowed mode). Scrolls if there are many peers.
fn palette_column(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::theme::section(
        ui,
        ph::PLUGS,
        &tr("compositor.palette.heading"),
        &tr("compositor.palette.heading_desc"),
    );
    ui.label(
        egui::RichText::new(tr("compositor.palette.drag_hint"))
            .small()
            .weak(),
    );
    ui.add_space(4.0);

    // No inner ScrollArea — the palette flows at its natural height into the
    // tab's own scroll, so it never grows a second, shorter scrollbar beside a
    // taller sidebar. Lead with the real clients; the placeholder + the advanced
    // manual address entry come after, so the common case is front and centre.
    peer_chips(ui, state);
    ui.add_space(6.0);
    blank_chip(ui);
    ui.add_space(6.0);
    egui::CollapsingHeader::new(
        egui::RichText::new(tr("compositor.palette.manual_heading"))
            .small()
            .strong(),
    )
    .id_salt("compositor_manual_advanced")
    .default_open(false)
    .show(ui, manual_entry);
}

/// Compact palette strip (maximized mode) — chips laid out horizontally.
fn palette_strip(ui: &mut egui::Ui, state: &mut PanelState) {
    crate::widgets::card(ui, "compositor_palette_strip", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(tr("compositor.palette.drag_hint_strip"))
                    .small()
                    .weak(),
            );
            peer_chips(ui, state);
            blank_chip(ui);
            manual_entry_inline(ui);
        });
    });
}

/// The "blank 1080p output" drag chip.
fn blank_chip(ui: &mut egui::Ui) {
    let resp = ui
        .dnd_drag_source(
            egui::Id::new("compositor_drag_blank"),
            DropPayload::Blank,
            |ui| {
                chip_body(
                    ui,
                    &format!("{}  {}", ph::PLUS, tr("compositor.palette.blank")),
                    crate::theme::available(),
                    &tr("compositor.palette.blank_sub"),
                );
            },
        )
        .response;
    resp.describe_named(
        tr("compositor.palette.blank"),
        tr("compositor.palette.blank_desc"),
    );
}

/// One drag chip per paired peer.
fn peer_chips(ui: &mut egui::Ui, state: &PanelState) {
    if state.peers.is_empty() {
        ui.label(
            egui::RichText::new(tr("compositor.palette.no_peers"))
                .small()
                .weak(),
        );
        return;
    }
    for peer in &state.peers {
        let name = peer.display_name();
        let payload = DropPayload::Peer {
            name: name.clone(),
            pubkey: peer.pubkey.clone(),
        };
        let color = peer.liveness.color();
        // Sub-line shows what you're dropping: the device's last-reported
        // resolution (so its tile size is no surprise) + its liveness; falls
        // back to the address / liveness label when no geometry is known yet.
        let sub = match state.device_geoms.get(peer.pubkey.as_str()) {
            Some(g) => format!("{}×{} · {}", g.w, g.h, peer.liveness.label()),
            None => peer
                .addr
                .clone()
                .unwrap_or_else(|| peer.liveness.label().to_string()),
        };
        let resp = ui
            .dnd_drag_source(
                egui::Id::new(("compositor_drag_peer", peer.pubkey.as_str())),
                payload,
                |ui| {
                    chip_body(ui, &name, color, &sub);
                },
            )
            .response;
        resp.describe_named(
            name.clone(),
            format!(
                "{name}\n{}\n{}",
                crate::model::fingerprint(&peer.pubkey),
                tr("compositor.palette.peer_desc_suffix")
            ),
        );
    }
}

/// A small bordered chip used as a drag-source body.
fn chip_body(ui: &mut egui::Ui, title: &str, color: Color32, sub: &str) {
    let frame = egui::Frame::group(ui.style())
        .rounding(Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(8.0, 5.0))
        .fill(ui.visuals().widgets.inactive.bg_fill);
    frame.show(ui, |ui| {
        // Labels are selectable by default, so a press on the chip's text would
        // start a text-selection (I-beam) instead of a drag. Turn selection off
        // so the WHOLE chip surface is one drag handle.
        ui.style_mut().interaction.selectable_labels = false;
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(ph::DOTS_SIX_VERTICAL).weak()); // grip glyph
                ui.label(egui::RichText::new(title).strong().color(color));
            });
            if !sub.is_empty() {
                ui.label(egui::RichText::new(sub).small().weak());
            }
        });
    });
}

/// Manual address entry (column mode): name + addr fields stacked, then Add.
fn manual_entry(ui: &mut egui::Ui) {
    // Heading is supplied by the "Advanced" fold this lives in.
    let (mut name, mut addr) = read_manual(ui);
    ui.add(
        egui::TextEdit::singleline(&mut name)
            .hint_text(tr("compositor.palette.manual_name_hint"))
            .desired_width(f32::INFINITY),
    )
    .describe(tr("compositor.palette.manual_name_desc"));
    ui.add(
        egui::TextEdit::singleline(&mut addr)
            .hint_text(tr("compositor.palette.manual_addr_hint"))
            .desired_width(f32::INFINITY),
    )
    .describe(tr("compositor.palette.manual_addr_desc"));
    write_manual(ui, &name, &addr);
    manual_drag_chip(ui, &name, &addr);
}

/// Manual address entry (strip mode): compact inline fields.
fn manual_entry_inline(ui: &mut egui::Ui) {
    ui.separator();
    let (mut name, mut addr) = read_manual(ui);
    ui.add(
        egui::TextEdit::singleline(&mut name)
            .hint_text(tr("compositor.palette.manual_name_hint_short"))
            .desired_width(90.0),
    )
    .describe(tr("compositor.palette.manual_name_desc"));
    ui.add(
        egui::TextEdit::singleline(&mut addr)
            .hint_text(tr("compositor.palette.manual_addr_hint"))
            .desired_width(150.0),
    )
    .describe(tr("compositor.palette.manual_addr_desc"));
    write_manual(ui, &name, &addr);
    manual_drag_chip(ui, &name, &addr);
}

/// The draggable chip for whatever's currently typed into the manual fields.
/// Disabled-looking (greyed) until an address is present.
fn manual_drag_chip(ui: &mut egui::Ui, name: &str, addr: &str) {
    let addr = addr.trim();
    if addr.is_empty() {
        ui.add_enabled(
            false,
            egui::Button::new(format!(
                "{}  {}",
                ph::DOTS_SIX_VERTICAL,
                tr("compositor.palette.manual_drag_disabled")
            )),
        )
        .describe(tr("compositor.palette.manual_drag_disabled_desc"));
        return;
    }
    let name = if name.trim().is_empty() {
        "manual".to_string()
    } else {
        name.trim().to_string()
    };
    let payload = DropPayload::Manual {
        name: name.clone(),
        addr: addr.to_string(),
    };
    ui.dnd_drag_source(egui::Id::new("compositor_drag_manual"), payload, |ui| {
        // `chip_body` already prepends a grip glyph, so pass the bare name.
        chip_body(ui, &name, crate::theme::info(), addr);
    })
    .response
    .describe_named(name, tr("compositor.palette.manual_drag_desc"));
}

fn read_manual(ui: &egui::Ui) -> (String, String) {
    let name = ui
        .ctx()
        .data(|d| d.get_temp::<String>(id_manual_name()))
        .unwrap_or_default();
    let addr = ui
        .ctx()
        .data(|d| d.get_temp::<String>(id_manual_addr()))
        .unwrap_or_default();
    (name, addr)
}

fn write_manual(ui: &egui::Ui, name: &str, addr: &str) {
    ui.ctx().data_mut(|d| {
        d.insert_temp(id_manual_name(), name.to_string());
        d.insert_temp(id_manual_addr(), addr.to_string());
    });
}

// ---------------------------------------------------------------------------
// Canvas — the scaled mini-map + drop zone + in-canvas tile dragging
// ---------------------------------------------------------------------------

fn canvas(ui: &mut egui::Ui, state: &mut PanelState, height: f32) {
    let frame = egui::Frame::group(ui.style())
        .rounding(Rounding::same(10.0))
        .inner_margin(egui::Margin::same(2.0))
        .fill(ui.visuals().extreme_bg_color);

    // The drop zone wraps the painter so egui highlights it when a compatible
    // payload hovers. We read the pointer position ourselves (the payload only
    // signals *that* something dropped) to place the new tile precisely.
    let (inner, dropped) = ui.dnd_drop_zone::<DropPayload, ()>(frame, |ui| {
        let desired = Vec2::new(ui.available_width().max(320.0), height.max(220.0));
        let (resp, painter) = ui.allocate_painter(desired, Sense::click());
        resp.clone()
            .describe_named(tr("compositor.heading"), tr("compositor.canvas.desc"));
        draw_canvas(ui, state, &resp, &painter);
    });

    if let Some(payload) = dropped {
        // Where did it land? Prefer the interact pos at release time.
        let drop_pos = ui
            .ctx()
            .pointer_interact_pos()
            .or_else(|| ui.ctx().pointer_hover_pos());
        let canvas_rect = inner.response.rect;
        handle_drop(state, &payload, drop_pos, canvas_rect);
    }
}

/// View transform between virtual-desktop pixels and canvas (screen) pixels.
#[derive(Clone, Copy)]
struct View {
    /// Top-left of the virtual bounding box, in virtual px.
    origin: Vec2,
    /// virtual-px → screen-px scale (uniform).
    scale: f32,
    /// Canvas rect top-left in screen px (after centring margin).
    screen_min: Pos2,
}

impl View {
    fn to_screen(self, x: i32, y: i32) -> Pos2 {
        Pos2::new(
            self.screen_min.x + (x as f32 - self.origin.x) * self.scale,
            self.screen_min.y + (y as f32 - self.origin.y) * self.scale,
        )
    }
    fn screen_to_virtual(self, p: Pos2) -> (i32, i32) {
        (
            (self.origin.x + (p.x - self.screen_min.x) / self.scale).round() as i32,
            (self.origin.y + (p.y - self.screen_min.y) / self.scale).round() as i32,
        )
    }
}

/// Compute the view that fits every tile (plus a default frame when empty)
/// inside `rect`, with a small margin.
fn compute_view(state: &PanelState, rect: Rect) -> View {
    const MARGIN: f32 = 14.0;
    let inner = Rect::from_min_max(
        rect.min + Vec2::splat(MARGIN),
        rect.max - Vec2::splat(MARGIN),
    );

    // Virtual bounding box across all tiles, with a fallback to a single 1080p
    // frame so an empty canvas still has sensible scale.
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (0i32, 0i32, 1920i32, 1080i32);
    if !state.layout.outputs.is_empty() {
        min_x = i32::MAX;
        min_y = i32::MAX;
        max_x = i32::MIN;
        max_y = i32::MIN;
        for o in &state.layout.outputs {
            min_x = min_x.min(o.x);
            min_y = min_y.min(o.y);
            max_x = max_x.max(o.x + o.w as i32);
            max_y = max_y.max(o.y + o.h as i32);
        }
        // Pad the box by ~6% so tiles aren't flush against the edge.
        let pad_x = ((max_x - min_x) as f32 * 0.06).max(80.0) as i32;
        let pad_y = ((max_y - min_y) as f32 * 0.06).max(80.0) as i32;
        min_x -= pad_x;
        min_y -= pad_y;
        max_x += pad_x;
        max_y += pad_y;
    }

    let vw = (max_x - min_x).max(1) as f32;
    let vh = (max_y - min_y).max(1) as f32;
    let scale = (inner.width() / vw).min(inner.height() / vh).max(0.0001);

    // Centre the scaled content within the canvas.
    let used = Vec2::new(vw * scale, vh * scale);
    let screen_min = inner.min + (inner.size() - used) * 0.5;

    View {
        origin: Vec2::new(min_x as f32, min_y as f32),
        scale,
        screen_min,
    }
}

fn draw_canvas(
    ui: &mut egui::Ui,
    state: &mut PanelState,
    resp: &egui::Response,
    painter: &egui::Painter,
) {
    let rect = resp.rect;
    let visuals = ui.visuals().clone();
    let view = compute_view(state, rect);

    // Backdrop grid (every 480 virtual px ≈ a quarter of 1080p height).
    draw_grid(painter, rect, &view, &visuals);

    if state.layout.outputs.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            tr("compositor.canvas.empty"),
            FontId::proportional(16.0),
            visuals.weak_text_color(),
        );
        return;
    }

    let selected = ui
        .ctx()
        .data(|d| d.get_temp::<u64>(id_selected()))
        .unwrap_or(0);
    let selset = read_selset(ui);

    // Keyboard modifiers decide single- vs multi-select on a tile click.
    let (mods_shift, mods_cmd) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
    let additive = mods_shift || mods_cmd;

    // Collect mutations to apply after the immutable draw pass (avoids holding a
    // borrow of state.layout.outputs while we also touch egui memory).
    let mut new_selected: Option<u64> = None;
    // A plain click replaces the whole set; an additive click toggles one tile.
    let mut set_replace: Option<u64> = None;
    let mut set_toggle: Option<u64> = None;
    let mut clear_selection = false;
    let mut moved: Option<(u64, i32, i32)> = None;
    let mut removed: Option<u64> = None;
    // A plain click on a peer-pinned device tile toggles its input forwarding.
    let mut toggle_input: Option<u64> = None;
    // True once any tile (or its close box) consumed a click this frame, so an
    // empty-canvas click can be told apart from a tile click for clearing.
    let mut tile_consumed_click = false;

    // Snapshot every tile's geometry + the shared fps so the drag handler can
    // snap a dragged tile's edges against its neighbours without re-borrowing.
    let fps = state.layout.fps;
    let tile_geoms: Vec<(u64, i32, i32, u32, u32)> = state
        .layout
        .outputs
        .iter()
        .map(|o| (o.id, o.x, o.y, o.w, o.h))
        .collect();

    // Pre-pass: cluster tiles whose virtual rects overlap (union-find), then
    // grid-pack each cluster's per-tile "client cards" within the cluster's
    // bounding box. This guarantees the cards never overlap — the key case being
    // mirror, where every peer tile sits at the same (0,0) so the labels would
    // otherwise stack. A non-overlapping (singleton) tile gets one cell = itself.
    let card_rects: std::collections::HashMap<u64, Rect> =
        compute_card_rects(&state.layout.outputs, &view);

    for o in &state.layout.outputs {
        let tl = view.to_screen(o.x, o.y);
        let br = view.to_screen(o.x + o.w as i32, o.y + o.h as i32);
        let tile_rect = Rect::from_two_pos(tl, br);
        let is_primary = o.id == selected;
        let in_set = selset.contains(&o.id);

        // Per-tile drag interaction (move within canvas). Unique id per tile.
        let tile_id = ui.make_persistent_id(("compositor_tile", o.id));
        let tile_resp = ui.interact(tile_rect, tile_id, Sense::click_and_drag());

        // Colours: a peer-pinned device whose INPUT is enabled reads GREEN (it
        // can drive the host); other pinned tiles are info-blue; non-peer (raw)
        // tiles teal.
        let input_on = o.input_enabled && o.peer_pubkey.is_some();
        let base = if input_on {
            crate::theme::online()
        } else if o.peer_pubkey.is_some() {
            crate::theme::info()
        } else {
            crate::theme::available()
        };
        let fill = base.gamma_multiply(if is_primary || in_set { 0.42 } else { 0.26 });
        // Overlap detection: an output that intersects another maps to the same
        // virtual region — a real misconfiguration. Surface it right on the
        // canvas (error stroke) instead of only in the off-screen Apply warning.
        let overlaps = tile_geoms.iter().any(|t| {
            t.0 != o.id
                && o.x < t.1 + t.3 as i32
                && t.1 < o.x + o.w as i32
                && o.y < t.2 + t.4 as i32
                && t.2 < o.y + o.h as i32
        });
        // Overlap takes visual priority; otherwise the primary selection gets the
        // bright "online" stroke, other multi-selection members a distinct
        // secondary (streaming) stroke so the two are separable.
        // Input-enabled reads via the GREEN FILL (below), not the stroke — stroke
        // is reserved for overlap (red) / primary-selection (online) / multi-set
        // (streaming) so "this device drives input" and "this tile is selected"
        // stay visually distinct (they'd otherwise both be `online()`).
        let stroke_col = if overlaps {
            crate::theme::error()
        } else if is_primary {
            crate::theme::online()
        } else if in_set {
            crate::theme::streaming()
        } else if input_on {
            // A subtle green border (not the bright selection green) so an
            // unselected input-on tile still reads green without aliasing the
            // primary-selection stroke.
            crate::theme::online().gamma_multiply(0.6)
        } else if tile_resp.hovered() || tile_resp.dragged() {
            base
        } else {
            visuals.widgets.inactive.fg_stroke.color
        };
        let stroke_w = if overlaps || is_primary || in_set {
            2.5_f32
        } else {
            1.5_f32
        };

        painter.rect(
            tile_rect,
            Rounding::same(4.0),
            fill,
            Stroke::new(stroke_w, stroke_col),
        );

        // Metrics as a compact "client card" packed into this tile's slot of its
        // overlap cluster (see `card_rects`): name + res·fps + port stacked and
        // CLIPPED to the cell so they never bleed into a neighbour. When tiles
        // overlap (mirror = every peer tile at the same spot) the cards tile in a
        // grid within the shared bounding box instead of all three labels
        // colliding at the same fixed anchors.
        let short_addr = o.addr.strip_prefix("quic://").unwrap_or(&o.addr);
        let cell = card_rects.get(&o.id).copied().unwrap_or(tile_rect);
        draw_client_card(
            painter,
            cell,
            &o.name,
            &format!("{}×{} · {fps} Hz", o.w, o.h),
            short_addr,
            overlaps,
            &visuals,
        );

        // A small close (X) delete hot-zone in the top-right corner of the tile.
        if tile_rect.width() > 30.0 && tile_rect.height() > 24.0 {
            let close = Rect::from_min_size(
                Pos2::new(tile_rect.right() - 18.0, tile_rect.top() + 2.0),
                Vec2::splat(16.0),
            );
            let close_id = ui.make_persistent_id(("compositor_tile_close", o.id));
            let close_resp = ui.interact(close, close_id, Sense::click());
            let cc = if close_resp.hovered() {
                crate::theme::error()
            } else {
                visuals.weak_text_color()
            };
            painter.text(
                close.center(),
                Align2::CENTER_CENTER,
                ph::X,
                FontId::proportional(13.0),
                cc,
            );
            if close_resp.clicked() {
                removed = Some(o.id);
                tile_consumed_click = true;
            }
        }

        if tile_resp.clicked() {
            tile_consumed_click = true;
            new_selected = Some(o.id);
            if additive {
                // Shift / Ctrl / Cmd: toggle this tile in the selection set
                // (group ops) — no input toggle, so a group can be built.
                set_toggle = Some(o.id);
            } else {
                // Plain click: this tile becomes the entire selection AND, for a
                // peer-pinned device, toggles its input forwarding (the green
                // card). Non-peer tiles (raw/manual outputs) just select.
                set_replace = Some(o.id);
                if o.peer_pubkey.is_some() {
                    toggle_input = Some(o.id);
                }
            }
        }
        // The UNSNAPPED drag accumulator (virtual px) lives in temp memory per
        // tile. Snapping commits a snapped position to the tile, but the next
        // frame's proposal is computed from this raw accumulator — NOT the
        // snapped tile position — so the cursor can always peel a tile off a
        // neighbour once it moves past the snap threshold (no sticky feedback loop).
        let unsnap_id = ui.make_persistent_id(("compositor_drag_unsnap", o.id));
        if tile_resp.drag_started() {
            // Starting a drag makes the tile the primary; if it wasn't already
            // part of a multi-selection, collapse the set onto it so a drag of an
            // unselected tile doesn't silently move a stale group.
            new_selected = Some(o.id);
            if !in_set {
                set_replace = Some(o.id);
            }
            ui.ctx()
                .data_mut(|d| d.insert_temp(unsnap_id, Vec2::new(o.x as f32, o.y as f32)));
        }
        if tile_resp.dragged() && view.scale > 0.0 {
            let d = tile_resp.drag_delta();
            // Accumulate raw cursor motion, independent of snapping.
            let mut unsnap = ui
                .ctx()
                .data(|m| m.get_temp::<Vec2>(unsnap_id))
                .unwrap_or_else(|| Vec2::new(o.x as f32, o.y as f32));
            unsnap += d / view.scale;
            ui.ctx().data_mut(|m| m.insert_temp(unsnap_id, unsnap));
            let px = unsnap.x.round() as i32;
            let py = unsnap.y.round() as i32;
            // A multi-tile group drags freely (keeps its internal arrangement);
            // a single tile snaps its edges to neighbours, Windows-style.
            let in_group = selset.contains(&o.id) && selset.len() > 1;
            let (nx, ny) = if in_group {
                (px, py)
            } else {
                let others: Vec<(i32, i32, u32, u32)> = tile_geoms
                    .iter()
                    .filter(|t| t.0 != o.id)
                    .map(|t| (t.1, t.2, t.3, t.4))
                    .collect();
                let thresh = (8.0 / view.scale).round().max(2.0) as i32;
                snap_position(px, py, o.w, o.h, &others, thresh)
            };
            if (nx, ny) != (o.x, o.y) {
                moved = Some((o.id, nx, ny));
            }
        }
    }

    // A click on the empty canvas (not on any tile or close box) clears the
    // whole selection.
    if resp.clicked() && !tile_consumed_click {
        clear_selection = true;
    }

    // Apply collected mutations.
    if let Some(id) = removed {
        state.layout.remove(id);
        state.dirty_layout = true;
        new_selected = Some(0);
        let mut set = read_selset(ui);
        set.remove(&id);
        write_selset(ui, &set);
    }
    // Apply a per-device input toggle: flip the tile's flag, persist it, and push
    // the new state to the running host live (by the device's pubkey) so input
    // starts/stops immediately while streaming.
    if let Some(id) = toggle_input {
        if let Some(o) = state.layout.outputs.iter_mut().find(|o| o.id == id) {
            o.input_enabled = !o.input_enabled;
            state.dirty_layout = true;
            if let Some(pk) = o.peer_pubkey.as_deref() {
                crate::control::set_input_enabled(pk, o.input_enabled);
            }
        }
    }
    if let Some((id, nx, ny)) = moved {
        // `moved` is the dragged tile's snapped ABSOLUTE target. Convert to a
        // delta and apply to the whole selection when it's a group drag, so the
        // group keeps its internal arrangement; otherwise just the one tile.
        if let Some((cx, cy)) = state
            .layout
            .outputs
            .iter()
            .find(|o| o.id == id)
            .map(|o| (o.x, o.y))
        {
            let (dx, dy) = (nx - cx, ny - cy);
            let group: Vec<u64> = if selset.contains(&id) && selset.len() > 1 {
                selset.iter().copied().collect()
            } else {
                vec![id]
            };
            for gid in group {
                if let Some(o) = state.layout.outputs.iter_mut().find(|o| o.id == gid) {
                    o.x += dx;
                    o.y += dy;
                }
            }
            state.dirty_layout = true;
        }
    }
    if clear_selection {
        new_selected = Some(0);
        write_selset(ui, &BTreeSet::new());
    }
    if let Some(id) = set_replace {
        let mut set = BTreeSet::new();
        set.insert(id);
        write_selset(ui, &set);
    } else if let Some(id) = set_toggle {
        let mut set = read_selset(ui);
        if !set.insert(id) {
            set.remove(&id);
        }
        write_selset(ui, &set);
    }
    if let Some(sel) = new_selected {
        ui.ctx().data_mut(|d| d.insert_temp(id_selected(), sel));
    }

    // Status overlay, bottom-left (replaces the old scale tip): display count,
    // total megapixels + fps, and the encode pixel-rate budget usage.
    let n = state.layout.outputs.len();
    let total_px: u64 = state
        .layout
        .outputs
        .iter()
        .map(|o| o.w as u64 * o.h as u64)
        .sum();
    let mp = total_px as f64 / 1_000_000.0;
    // In MIRROR the host runs ONE shared encode for every client, so the pixel
    // budget cost is the single (largest) output, not the sum of the tiles —
    // count it 1×, not N×. Single/extend keep the Σ. (Reads the live host mode;
    // falls back to Σ when no host or not mirroring.)
    let mirror = crate::control::handle()
        .status()
        .is_some_and(|s| matches!(s.stream_mode, control_ipc::StreamMode::Mirror { .. }));
    let budget_px = if mirror {
        state
            .layout
            .outputs
            .iter()
            .map(|o| o.w as u64 * o.h as u64)
            .max()
            .unwrap_or(0)
    } else {
        total_px
    };
    let pps = budget_px.saturating_mul(fps as u64); // encoded pixels per second
    let budget = state.encode_budget.max(1);
    let pct = ((pps as f64 / budget as f64) * 100.0).round() as i64;

    let displays_word = if n == 1 {
        tr("compositor.apply.summary_display_singular")
    } else {
        tr("compositor.apply.summary_display_plural")
    };
    let info = format!(
        "{n} {} · {mp:.1} {} · {fps} {}",
        displays_word,
        tr("compositor.apply.summary_mp"),
        tr("compositor.apply.summary_fps"),
    );
    // Budget colour ramp: comfortable green → amber as it fills → red over 100%.
    let budget_col = if pct > 100 {
        crate::theme::error()
    } else if pct >= 85 {
        crate::theme::warn()
    } else {
        crate::theme::online()
    };
    let budget_line = if mirror {
        // Make the saving explicit: 1× shared encode instead of N× tiles.
        let saved = n.saturating_sub(1);
        let shared = tr("compositor.canvas.budget_mirror");
        if saved > 0 {
            format!(
                "{pct}% {} · {shared} (−{saved}×)",
                tr("compositor.canvas.budget_suffix")
            )
        } else {
            format!(
                "{pct}% {} · {shared}",
                tr("compositor.canvas.budget_suffix")
            )
        }
    } else {
        format!("{pct}% {}", tr("compositor.canvas.budget_suffix"))
    };

    let pill_bg = Color32::from_black_alpha(150);
    let info_rect = overlay_pill(
        painter,
        Pos2::new(rect.left() + 6.0, rect.bottom() - 6.0),
        &info,
        visuals.strong_text_color(),
        pill_bg,
    );
    let budget_rect = overlay_pill(
        painter,
        Pos2::new(info_rect.left(), info_rect.top() - 4.0),
        &budget_line,
        budget_col,
        pill_bg,
    );
    // Name the painter-drawn overlay for hover + screen readers (not a widget).
    ui.interact(
        info_rect.union(budget_rect),
        ui.id().with("compositor_canvas_status"),
        egui::Sense::hover(),
    )
    .describe(format!(
        "{info} · {budget_line}\n{}",
        tr("compositor.canvas.budget_desc")
    ));
}

/// Draw a small rounded "pill" of text anchored by its BOTTOM-LEFT corner, with
/// a translucent backdrop so it stays legible over the canvas. Returns the pill
/// rect (for stacking / a11y). Used by the canvas status overlay.
fn overlay_pill(
    painter: &egui::Painter,
    bottom_left: Pos2,
    text: &str,
    color: Color32,
    bg: Color32,
) -> Rect {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.0), color);
    let pad = Vec2::new(6.0, 3.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(Pos2::new(bottom_left.x, bottom_left.y - size.y), size);
    painter.rect_filled(rect, Rounding::same(5.0), bg);
    painter.galley(rect.min + pad, galley, color);
    rect
}

/// Assign every tile a non-overlapping on-screen "client card" rect. Tiles whose
/// VIRTUAL rects overlap are clustered (union-find by AABB); each cluster's cards
/// are grid-packed inside the cluster's bounding box ([`pack_grid`]), so cards
/// never collide — the mirror case (all peer tiles coincident) becomes a tidy
/// grid instead of three labels stacking at the same anchors. A non-overlapping
/// tile is its own cluster → one cell == its own rect (the card fills it).
fn compute_card_rects(outs: &[OutputTile], view: &View) -> std::collections::HashMap<u64, Rect> {
    let n = outs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for i in 0..n {
        for j in (i + 1)..n {
            let (a, b) = (&outs[i], &outs[j]);
            let overlap = a.x < b.x + b.w as i32
                && b.x < a.x + a.w as i32
                && a.y < b.y + b.h as i32
                && b.y < a.y + a.h as i32;
            if overlap {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut groups: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let mut map = std::collections::HashMap::new();
    for members in groups.values() {
        // Cluster bounding box = union of member screen rects.
        let mut bounds: Option<Rect> = None;
        for &idx in members {
            let o = &outs[idx];
            let r = Rect::from_two_pos(
                view.to_screen(o.x, o.y),
                view.to_screen(o.x + o.w as i32, o.y + o.h as i32),
            );
            bounds = Some(bounds.map_or(r, |b| b.union(r)));
        }
        let cells = pack_grid(bounds.unwrap_or(Rect::NOTHING), members.len());
        for (k, &idx) in members.iter().enumerate() {
            map.insert(outs[idx].id, cells[k]);
        }
    }
    map
}

/// Split `bounds` into `n` non-overlapping grid cells (`ceil(sqrt(n))` columns),
/// each inset by a small gutter so adjacent cards read as distinct.
fn pack_grid(bounds: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f32).sqrt().ceil().max(1.0) as usize;
    let rows = n.div_ceil(cols);
    let pad = 2.0_f32;
    let cell_w = ((bounds.width() - pad) / cols as f32 - pad).max(0.0);
    let cell_h = ((bounds.height() - pad) / rows as f32 - pad).max(0.0);
    (0..n)
        .map(|i| {
            let (c, r) = (i % cols, i / cols);
            Rect::from_min_size(
                Pos2::new(
                    bounds.left() + pad + c as f32 * (cell_w + pad),
                    bounds.top() + pad + r as f32 * (cell_h + pad),
                ),
                Vec2::new(cell_w, cell_h),
            )
        })
        .collect()
}

/// Draw one client card — name + `res·fps` + port stacked, CLIPPED to `cell` so
/// text can't bleed into a neighbouring card. `boxed` (set when this tile shares
/// an overlap cluster) draws a faint frame so stacked mirror clients read as
/// separate cards. Adapts to the cell size: name always, res if tall enough,
/// port if taller still; nothing if the cell is too small to be legible.
fn draw_client_card(
    painter: &egui::Painter,
    cell: Rect,
    name: &str,
    res: &str,
    port: &str,
    boxed: bool,
    visuals: &egui::Visuals,
) {
    if cell.width() < 30.0 || cell.height() < 16.0 {
        return;
    }
    let cp = painter.with_clip_rect(cell);
    if boxed {
        cp.rect(
            cell.shrink(1.0),
            Rounding::same(3.0),
            Color32::from_black_alpha(70),
            Stroke::new(1.0_f32, visuals.weak_text_color()),
        );
    }
    let pad = Vec2::new(5.0, 4.0);
    cp.text(
        cell.min + pad,
        Align2::LEFT_TOP,
        name,
        FontId::proportional(12.0),
        visuals.strong_text_color(),
    );
    if cell.height() > 30.0 {
        cp.text(
            cell.min + pad + Vec2::new(0.0, 15.0),
            Align2::LEFT_TOP,
            res,
            FontId::proportional(11.0),
            visuals.text_color(),
        );
    }
    if cell.height() > 44.0 {
        cp.text(
            Pos2::new(cell.left() + pad.x, cell.bottom() - 4.0),
            Align2::LEFT_BOTTOM,
            port,
            FontId::monospace(9.0),
            visuals.weak_text_color(),
        );
    }
}

/// Snap a dragged tile's proposed top-left so its left/right edge aligns with
/// any neighbour's left/right edge (and top/bottom likewise) when within
/// `thresh` virtual px — the Windows-display-settings "stick to the neighbour"
/// feel. Each axis snaps independently. Returns the adjusted `(x, y)`.
fn snap_position(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    others: &[(i32, i32, u32, u32)],
    thresh: i32,
) -> (i32, i32) {
    let (w, h) = (w as i32, h as i32);
    // (best candidate, its distance) per axis.
    let mut best_x = (x, thresh + 1);
    let mut best_y = (y, thresh + 1);
    for &(ox, oy, ow, oh) in others {
        let (ox2, oy2) = (ox + ow as i32, oy + oh as i32);
        // Align our LEFT edge (cand = edge) or RIGHT edge (cand = edge - w) to
        // each neighbour's left/right edge.
        for edge in [ox, ox2] {
            for cand in [edge, edge - w] {
                let d = (x - cand).abs();
                if d < best_x.1 {
                    best_x = (cand, d);
                }
            }
        }
        for edge in [oy, oy2] {
            for cand in [edge, edge - h] {
                let d = (y - cand).abs();
                if d < best_y.1 {
                    best_y = (cand, d);
                }
            }
        }
    }
    let nx = if best_x.1 <= thresh { best_x.0 } else { x };
    let ny = if best_y.1 <= thresh { best_y.0 } else { y };
    (nx, ny)
}

/// Faint grid behind the tiles, spaced every 480 virtual px.
fn draw_grid(painter: &egui::Painter, rect: Rect, view: &View, visuals: &egui::Visuals) {
    const STEP: i32 = 480;
    let line = Stroke::new(
        1.0_f32,
        visuals
            .widgets
            .noninteractive
            .bg_stroke
            .color
            .gamma_multiply(0.5),
    );

    // Snap the start to a multiple of STEP at or before the visible origin.
    let start_x = (view.origin.x as i32).div_euclid(STEP) * STEP;
    let start_y = (view.origin.y as i32).div_euclid(STEP) * STEP;

    let mut x = start_x;
    while {
        let sx = view.to_screen(x, 0).x;
        sx < rect.right() + 1.0
    } {
        let sx = view.to_screen(x, 0).x;
        if sx >= rect.left() {
            painter.line_segment(
                [Pos2::new(sx, rect.top()), Pos2::new(sx, rect.bottom())],
                line,
            );
        }
        x += STEP;
        if x > start_x + STEP * 200 {
            break; // safety against absurd scales
        }
    }
    let mut y = start_y;
    while {
        let sy = view.to_screen(0, y).y;
        sy < rect.bottom() + 1.0
    } {
        let sy = view.to_screen(0, y).y;
        if sy >= rect.top() {
            painter.line_segment(
                [Pos2::new(rect.left(), sy), Pos2::new(rect.right(), sy)],
                line,
            );
        }
        y += STEP;
        if y > start_y + STEP * 200 {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Drop handling — turn a dropped payload into a new OutputTile
// ---------------------------------------------------------------------------

fn handle_drop(
    state: &mut PanelState,
    payload: &DropPayload,
    drop_pos: Option<Pos2>,
    canvas_rect: Rect,
) {
    // New-tile size: a dropped device uses its last-reported resolution (so it
    // lands at the right size); a blank/manual output honours the resolution
    // policy. (For a device, `sync_peer_tiles` keeps the size in step thereafter.)
    let (w, h) = match payload {
        DropPayload::Peer { pubkey, .. } => state
            .device_geoms
            .get(pubkey)
            .map(|g| (g.w, g.h))
            .unwrap_or_else(|| default_mode(state)),
        _ => default_mode(state),
    };

    // Compute the placement view from the *existing* tiles (before we add the
    // newcomer) so the new tile lands where the cursor is relative to the
    // current layout, not a layout that includes itself.
    let existing = state_outputs_excluding(state, u64::MAX); // exclude nothing
    let view = compute_view_excluding(existing, canvas_rect, state.layout.fps);
    let placement = drop_pos.filter(|p| canvas_rect.contains(*p)).map(|p| {
        let (vx, vy) = view.screen_to_virtual(p);
        (snap10(vx - w as i32 / 2), snap10(vy - h as i32 / 2))
    });

    // Now create the tile (its right-of-everything default x/y is overridden by
    // `placement` if we have a valid drop position).
    let id = state.layout.add_output();
    if let Some(t) = state.layout.outputs.iter_mut().find(|o| o.id == id) {
        if let Some((nx, ny)) = placement {
            t.x = nx;
            t.y = ny;
        }
        t.w = w;
        t.h = h;
        t.refresh_mhz = state.layout.fps * 1000;
        match payload {
            DropPayload::Peer { name, pubkey } => {
                t.name = sanitize_name(name);
                t.peer_pubkey = Some(pubkey.clone());
            }
            DropPayload::Manual { name, addr } => {
                t.name = sanitize_name(name);
                t.addr = addr.clone();
            }
            DropPayload::Blank => {}
        }
    }

    // If the policy is UniformLcd, fold the newcomer into the shared size.
    if state.runtime.resolution_policy == ResolutionPolicy::UniformLcd {
        equalize_uniform(state);
    }

    state.dirty_layout = true;
    state.status = Some(match payload {
        DropPayload::Peer { name, .. } => {
            format!("{} “{name}”.", tr("compositor.status.added_for_prefix"))
        }
        DropPayload::Manual { name, .. } => {
            format!("{} “{name}”.", tr("compositor.status.added_manual_prefix"))
        }
        DropPayload::Blank => tr("compositor.status.added_blank"),
    });
}

/// Snap a coordinate to the nearest 10 px so dropped tiles align cleanly.
fn snap10(v: i32) -> i32 {
    ((v as f32 / 10.0).round() as i32) * 10
}

/// The default mode for a freshly-dropped tile, per resolution policy.
fn default_mode(state: &PanelState) -> (u32, u32) {
    match state.runtime.resolution_policy {
        ResolutionPolicy::UniformLcd => {
            // Match the smallest existing tile if any, else 1080p.
            state
                .layout
                .outputs
                .iter()
                .map(|o| (o.w, o.h))
                .min_by_key(|(w, h)| (*w as u64) * (*h as u64))
                .unwrap_or((1920, 1080))
        }
        _ => (1920, 1080),
    }
}

/// Borrow helper: a snapshot of every output *except* `id`, for computing the
/// drop view without the just-added tile skewing the scale.
fn state_outputs_excluding(state: &PanelState, id: u64) -> Vec<(i32, i32, u32, u32)> {
    state
        .layout
        .outputs
        .iter()
        .filter(|o| o.id != id)
        .map(|o| (o.x, o.y, o.w, o.h))
        .collect()
}

/// A reduced `compute_view` over a bare list of tiles (used at drop time so the
/// new tile lands where the cursor is relative to the *existing* layout).
fn compute_view_excluding(tiles: Vec<(i32, i32, u32, u32)>, rect: Rect, _fps: u32) -> View {
    const MARGIN: f32 = 14.0;
    let inner = Rect::from_min_max(
        rect.min + Vec2::splat(MARGIN),
        rect.max - Vec2::splat(MARGIN),
    );
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (0i32, 0i32, 1920i32, 1080i32);
    if !tiles.is_empty() {
        min_x = i32::MAX;
        min_y = i32::MAX;
        max_x = i32::MIN;
        max_y = i32::MIN;
        for (x, y, w, h) in &tiles {
            min_x = min_x.min(*x);
            min_y = min_y.min(*y);
            max_x = max_x.max(*x + *w as i32);
            max_y = max_y.max(*y + *h as i32);
        }
        let pad_x = ((max_x - min_x) as f32 * 0.06).max(80.0) as i32;
        let pad_y = ((max_y - min_y) as f32 * 0.06).max(80.0) as i32;
        min_x -= pad_x;
        min_y -= pad_y;
        max_x += pad_x;
        max_y += pad_y;
    }
    let vw = (max_x - min_x).max(1) as f32;
    let vh = (max_y - min_y).max(1) as f32;
    let scale = (inner.width() / vw).min(inner.height() / vh).max(0.0001);
    let used = Vec2::new(vw * scale, vh * scale);
    let screen_min = inner.min + (inner.size() - used) * 0.5;
    View {
        origin: Vec2::new(min_x as f32, min_y as f32),
        scale,
        screen_min,
    }
}

/// Keep output names to a clean token (the compositor uses them as identifiers).
fn sanitize_name(s: &str) -> String {
    let t: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if t.is_empty() {
        "output".to_string()
    } else {
        t
    }
}

// ---------------------------------------------------------------------------
// Tile editors — numeric controls per output
// ---------------------------------------------------------------------------

fn tile_editors(ui: &mut egui::Ui, state: &mut PanelState) {
    if state.layout.outputs.is_empty() {
        // Nothing to edit, but still clear any stale selection set.
        write_selset(ui, &BTreeSet::new());
        ui.label(egui::RichText::new(tr("compositor.displays.empty")).weak());
        return;
    }

    // Drop any selection-set members that no longer exist (the canvas group-drag
    // still uses the set). The layout-mode presets now live above the canvas.
    let _selset = prune_selset(ui, state);

    let selected = ui
        .ctx()
        .data(|d| d.get_temp::<u64>(id_selected()))
        .unwrap_or(0);

    // Mutations collected during the (mutable) row loop are applied directly,
    // but cross-tile actions (remove, select) are deferred to after.
    let mut remove: Option<u64> = None;
    let mut select: Option<u64> = None;

    // Shared refresh (one pacer drives every output) — shown read-only per tile.
    let fps = state.layout.fps;

    let ids: Vec<u64> = state.layout.outputs.iter().map(|o| o.id).collect();
    for id in ids {
        let is_sel = id == selected;
        // index is stable within this frame
        let idx = match state.layout.outputs.iter().position(|o| o.id == id) {
            Some(i) => i,
            None => continue,
        };
        let port_hint = format!("quic://0.0.0.0:{}", CompositorBasePort + idx as u16);

        crate::widgets::card(ui, ("compositor_editor", id), |ui| {
            let o = &mut state.layout.outputs[idx];
            // A device tile (pinned to a peer) takes its name + size from the
            // device — both are read-only here. A blank/manual output is free-form.
            let is_peer = o.peer_pubkey.is_some();

            ui.horizontal(|ui| {
                if ui
                    .selectable_label(is_sel, egui::RichText::new("◉").strong())
                    .describe(tr("compositor.editor.select"))
                    .clicked()
                {
                    select = Some(id);
                }
                if is_peer {
                    // Name follows the device — synced, not editable here.
                    ui.label(egui::RichText::new(o.name.as_str()).strong())
                        .describe(tr("compositor.editor.name_device_desc"));
                    crate::widgets::status_badge(
                        ui,
                        (
                            tr("compositor.editor.peer_pinned").as_str(),
                            crate::theme::info(),
                        ),
                    )
                    .describe(tr("compositor.editor.peer_pinned_desc"));
                } else if ui
                    .add(
                        egui::TextEdit::singleline(&mut o.name)
                            .hint_text(tr("compositor.editor.name_hint"))
                            .desired_width(140.0),
                    )
                    .describe(tr("compositor.editor.name_desc"))
                    .changed()
                {
                    mark_dirty(ui);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(ph::TRASH)
                        .describe(tr("compositor.editor.remove"))
                        .clicked()
                    {
                        remove = Some(id);
                    }
                    if ui
                        .button(tr("compositor.editor.auto_port"))
                        .describe(format!(
                            "{} {port_hint}",
                            tr("compositor.editor.auto_port_desc")
                        ))
                        .clicked()
                    {
                        o.addr = port_hint.clone();
                        mark_dirty(ui);
                    }
                });
            });

            if ui
                .add(
                    egui::TextEdit::singleline(&mut o.addr)
                        .hint_text(tr("compositor.editor.addr_hint"))
                        .desired_width(f32::INFINITY),
                )
                .describe(tr("compositor.editor.addr_desc"))
                .changed()
            {
                mark_dirty(ui);
            }

            // Resolution + refresh. For a device tile both are read-only,
            // device-reported (resolution governs the tile's size on the canvas).
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(tr("compositor.editor.resolution"))
                        .small()
                        .weak(),
                );
                if is_peer {
                    ui.label(egui::RichText::new(format!("{}×{}", o.w, o.h)).strong())
                        .describe(tr("compositor.editor.resolution_device_desc"));
                } else {
                    size_drag(ui, &mut o.w, 320..=7680, tr("compositor.editor.width_desc"));
                    ui.label("×");
                    size_drag(
                        ui,
                        &mut o.h,
                        240..=4320,
                        tr("compositor.editor.height_desc"),
                    );
                }
                ui.separator();
                ui.label(format!("{fps} Hz"))
                    .describe(tr("compositor.editor.refresh_desc"));
            });

            // Resolution presets only for free-form (non-device) tiles.
            if !is_peer {
                ui.horizontal_wrapped(|ui| {
                    for (label, w, h) in [
                        ("1080p", 1920u32, 1080u32),
                        ("1440p", 2560, 1440),
                        ("4K", 3840, 2160),
                        ("720p", 1280, 720),
                    ] {
                        if ui
                            .small_button(label)
                            .describe(format!(
                                "{} {w}×{h}",
                                tr("compositor.editor.preset_desc_prefix")
                            ))
                            .clicked()
                        {
                            o.w = w;
                            o.h = h;
                            mark_dirty(ui);
                        }
                    }
                });
            }
        });

        ui.add_space(4.0);
    }

    // Deferred cross-tile actions.
    if let Some(sel) = select {
        ui.ctx().data_mut(|d| d.insert_temp(id_selected(), sel));
        // Selecting via the editor's ◉ button also makes it the whole set.
        let mut set = BTreeSet::new();
        set.insert(sel);
        write_selset(ui, &set);
    }
    if let Some(id) = remove {
        state.layout.remove(id);
        state.dirty_layout = true;
        ui.ctx().data_mut(|d| d.insert_temp(id_selected(), 0u64));
        let mut set = read_selset(ui);
        set.remove(&id);
        write_selset(ui, &set);
    }

    // Every editable widget above flags `compositor_changed_this_frame` (via
    // `mark_dirty`) on change; `apply_row` reconciles that into
    // `state.dirty_layout` later this frame. The widget helpers have no
    // `&mut PanelState`, so the egui temp-memory flag is how they signal dirty.
}

// ---------------------------------------------------------------------------
// Layout-mode actions — Mirror / Extend / Single (buttons live in canvas_toolbar)
// ---------------------------------------------------------------------------

/// Mirror / Duplicate: every output shows the *same* desktop, so they all share
/// one mode at the origin. The mode is the largest existing tile (by area), or
/// 1920×1080 if the outputs are mixed / empty.
fn apply_mirror(state: &mut PanelState) {
    let (w, h) = mirror_mode(state);
    if state.layout.outputs.is_empty() {
        state.layout.add_output();
    }
    let fps = state.layout.fps;
    for o in &mut state.layout.outputs {
        o.x = 0;
        o.y = 0;
        // Mirror stacks every output at the origin. A device tile keeps its
        // device-dictated size (resizing it would just revert next frame via
        // sync_peer_tiles); only free-form tiles take the shared mirror mode.
        if o.peer_pubkey.is_none() {
            o.w = w;
            o.h = h;
        }
        o.refresh_mhz = fps * 1000;
    }
    state.dirty_layout = true;
    state.status = Some(format!(
        "{} {w}×{h}.",
        tr("compositor.status.mirror_prefix")
    ));
}

/// The mirror mode: the largest existing tile's size, else 1080p. "Mixed" sizes
/// collapse onto the largest so nothing is cropped when every output shares it.
fn mirror_mode(state: &PanelState) -> (u32, u32) {
    state
        .layout
        .outputs
        .iter()
        .map(|o| (o.w, o.h))
        .max_by_key(|(w, h)| (*w as u64) * (*h as u64))
        .unwrap_or((1920, 1080))
}

/// Extend: lay every output left-to-right, each flush to the previous one's
/// right edge, top-aligned — distinct regions of one wide virtual desktop.
/// Reuses the snap-right geometry (flush x, shared y=0). Sizes are left as-is so
/// a mixed-resolution wall keeps each panel's native mode.
fn apply_extend(state: &mut PanelState) {
    if state.layout.outputs.is_empty() {
        state.layout.add_output();
    }
    let mut cursor_x = 0i32;
    for o in &mut state.layout.outputs {
        o.x = cursor_x;
        o.y = 0;
        cursor_x += o.w as i32;
    }
    state.dirty_layout = true;
    let n = state.layout.outputs.len();
    state.status = Some(format!(
        "{} {n} {}",
        tr("compositor.status.extend_prefix"),
        tr("compositor.status.extend_suffix")
    ));
}

/// Single: keep exactly one output. Prefer the primary selection if it still
/// exists, else the first; if there are none, add a fresh 1920×1080 one.
fn apply_single(ui: &egui::Ui, state: &mut PanelState) {
    let primary = ui
        .ctx()
        .data(|d| d.get_temp::<u64>(id_selected()))
        .unwrap_or(0);
    // Pick the keeper id: the primary if live, else the first output's id.
    let keep = state
        .layout
        .outputs
        .iter()
        .find(|o| o.id == primary)
        .map(|o| o.id)
        .or_else(|| state.layout.outputs.first().map(|o| o.id));

    if let Some(keep_id) = keep {
        state.layout.outputs.retain(|o| o.id == keep_id);
        if let Some(o) = state.layout.outputs.first_mut() {
            o.x = 0;
            o.y = 0;
        }
    } else {
        // Empty layout — give them a sensible default to start from.
        let id = state.layout.add_output();
        if let Some(o) = state.layout.outputs.iter_mut().find(|o| o.id == id) {
            o.x = 0;
            o.y = 0;
            o.w = 1920;
            o.h = 1080;
        }
    }

    // The kept tile is now the whole selection.
    let kept_id = state.layout.outputs.first().map(|o| o.id).unwrap_or(0);
    ui.ctx().data_mut(|d| d.insert_temp(id_selected(), kept_id));
    let mut set = BTreeSet::new();
    if kept_id != 0 {
        set.insert(kept_id);
    }
    write_selset(ui, &set);

    state.dirty_layout = true;
    state.status = Some(tr("compositor.status.single"));
}

// ---------------------------------------------------------------------------
// Play media on the wall — drive a clip into the running compositor
// ---------------------------------------------------------------------------

/// egui temp-memory key for the media path field (mirrors the manual-addr field).
fn id_media_path() -> egui::Id {
    egui::Id::new("compositor_media_path")
}

/// egui temp-memory key for the chosen subtitle mode.
fn id_media_subs() -> egui::Id {
    egui::Id::new("compositor_media_subs")
}

/// egui temp-memory key for the chosen audio track.
fn id_media_audio() -> egui::Id {
    egui::Id::new("compositor_media_audio")
}

/// egui temp-memory key for the cached `(path, audio_tracks, sub_tracks)` probe.
fn id_media_tracks() -> egui::Id {
    egui::Id::new("compositor_media_tracks")
}

/// Display label for one embedded track: "<prefix> N · lang · title", omitting
/// empty / `und` parts (falling back to the codec when there's no lang/title).
fn track_label(prefix: &str, t: &crate::data::MediaTrack) -> String {
    let mut extra: Vec<String> = Vec::new();
    if let Some(l) = &t.lang {
        if !l.is_empty() && l != "und" {
            extra.push(l.clone());
        }
    }
    if let Some(ti) = &t.title {
        if !ti.is_empty() {
            extra.push(ti.clone());
        }
    }
    if extra.is_empty() {
        if let Some(c) = &t.codec {
            extra.push(c.clone());
        }
    }
    let mut s = format!("{prefix} {}", t.id);
    if !extra.is_empty() {
        s.push_str(&format!(" · {}", extra.join(" · ")));
    }
    s
}

/// Selected-text for the subtitle combo.
fn subs_label(s: &control_ipc::Subtitles, tracks: &[crate::data::MediaTrack]) -> String {
    match s {
        control_ipc::Subtitles::Auto => tr("compositor.media.subs_auto"),
        control_ipc::Subtitles::Off => tr("compositor.media.subs_off"),
        control_ipc::Subtitles::Track(id) => tracks
            .iter()
            .find(|t| t.id == *id)
            .map(|t| track_label(&tr("compositor.media.subs_track"), t))
            .unwrap_or_else(|| format!("{} {id}", tr("compositor.media.subs_track"))),
        control_ipc::Subtitles::File(p) if p.is_empty() => tr("compositor.media.subs_choose"),
        control_ipc::Subtitles::File(p) => basename(p),
    }
}

/// Selected-text for the audio combo.
fn audio_label(a: &control_ipc::Audio, tracks: &[crate::data::MediaTrack]) -> String {
    match a {
        control_ipc::Audio::Auto => tr("compositor.media.subs_auto"),
        control_ipc::Audio::Off => tr("compositor.media.subs_off"),
        control_ipc::Audio::Track(id) => tracks
            .iter()
            .find(|t| t.id == *id)
            .map(|t| track_label(&tr("compositor.media.audio_track"), t))
            .unwrap_or_else(|| format!("{} {id}", tr("compositor.media.audio_track"))),
    }
}

// ---------------------------------------------------------------------------
// Live preview — a small viewport of the actual streamed output
// ---------------------------------------------------------------------------

/// A small, throttled viewport of the streamed output. The host writes a
/// downscaled JPEG (~2fps) only while enabled and auto-stops after 20s, so the
/// panel re-asks to keep it going. We decode the JPEG to an egui texture,
/// swapping it only when the file changes.
fn preview_controls(ui: &mut egui::Ui) {
    let until_id = egui::Id::new("compositor_preview_until");
    let connected = crate::control::handle().connected();
    let now = ui.input(|i| i.time);
    let deadline: Option<f64> = ui.data(|d| d.get_temp(until_id));
    let active = deadline.is_some_and(|t| now < t);

    // Lives inside the Media Player card — a divider + label, no own card.
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(tr("compositor.preview.heading"))
                .small()
                .weak(),
        );
        let label = if active {
            tr("compositor.preview.stop")
        } else {
            tr("compositor.preview.start")
        };
        // Gated on the host's advertised preview capability (issue #22) — the
        // Windows host has no GPU-readback preview writer.
        let can_preview = crate::control::host_caps().preview;
        if ui
            .add_enabled(
                connected && can_preview,
                egui::Button::new(format!("{}  {}", ph::MONITOR, label)),
            )
            .describe(tr("compositor.preview.start_desc"))
            .on_disabled_hover_text(if connected && !can_preview {
                tr("control.no_cap.preview")
            } else {
                crate::control::unavailable_reason()
            })
            .clicked()
        {
            if active {
                crate::control::set_preview(false);
                // Expire immediately (avoids the remove-by-type API).
                ui.data_mut(|d| d.insert_temp(until_id, now));
            } else {
                crate::control::set_preview(true);
                ui.data_mut(|d| d.insert_temp(until_id, now + 20.0));
            }
        }
        if active {
            let left = (deadline.unwrap_or(now) - now).max(0.0);
            ui.label(
                egui::RichText::new(format!("{} {:.0}s", tr("compositor.preview.left"), left))
                    .small()
                    .weak(),
            );
        } else {
            ui.label(
                egui::RichText::new(tr("compositor.preview.hint"))
                    .small()
                    .weak(),
            );
        }
    });

    if active {
        ui.add_space(6.0);
        show_preview_image(ui);
        // Refresh to pick up new frames + tick the countdown.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// Load the latest preview JPEG into a texture (re-decoding only when the file's
/// mtime changes) and draw it scaled to the card width.
fn show_preview_image(ui: &mut egui::Ui) {
    let tex_id = egui::Id::new("compositor_preview_tex");
    let mtime_id = egui::Id::new("compositor_preview_mtime");
    let path = control_ipc::preview_path();
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let last: Option<std::time::SystemTime> = ui.data(|d| d.get_temp(mtime_id));
    let mut tex: Option<egui::TextureHandle> = ui.data(|d| d.get_temp(tex_id));

    if mtime.is_some() && mtime != last {
        if let Some(img) = load_jpeg(&path) {
            let handle = ui
                .ctx()
                .load_texture("pharos_preview", img, egui::TextureOptions::LINEAR);
            ui.data_mut(|d| {
                d.insert_temp(tex_id, handle.clone());
                d.insert_temp(mtime_id, mtime);
            });
            tex = Some(handle);
        }
    }

    match tex {
        Some(t) => {
            let sz = t.size();
            let w = ui.available_width().min(360.0);
            let h = if sz[0] > 0 {
                w * sz[1] as f32 / sz[0] as f32
            } else {
                w * 9.0 / 16.0
            };
            ui.image(egui::load::SizedTexture::new(t.id(), egui::vec2(w, h)));
        }
        None => {
            ui.label(
                egui::RichText::new(tr("compositor.preview.waiting"))
                    .small()
                    .weak(),
            );
        }
    }
}

/// Decode a JPEG file to an egui `ColorImage` (RGBA). `None` on any error.
fn load_jpeg(path: &std::path::Path) -> Option<egui::ColorImage> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    ))
}

/// File basename for display (the full path lives in the field / tooltip).
fn basename(p: &str) -> String {
    std::path::Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Best starting directory for a file dialog: the directory of `current` if it
/// has one, else the user's home.
fn dialog_dir(current: &str) -> Option<std::path::PathBuf> {
    let t = current.trim();
    if !t.is_empty() {
        let p = std::path::Path::new(t);
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                return Some(parent.to_path_buf());
            }
        }
    }
    dirs::home_dir()
}

/// Native file browser for the video to play (blocks until the dialog closes —
/// it's modal). `rfd`'s `xdg-portal` backend is used on Linux (no GTK), native
/// dialogs on Windows/macOS, so this stays platform-agnostic.
fn pick_video_file(current: &str) -> Option<String> {
    let mut dlg = rfd::FileDialog::new()
        .set_title("Play on the wall — choose a video")
        .add_filter(
            "Video",
            &[
                "mp4", "mkv", "webm", "mov", "m4v", "avi", "ts", "flv", "wmv", "mpg", "mpeg",
            ],
        )
        .add_filter("All files", &["*"]);
    if let Some(dir) = dialog_dir(current) {
        dlg = dlg.set_directory(dir);
    }
    dlg.pick_file().map(|p| p.to_string_lossy().into_owned())
}

/// Native file browser for an external subtitle file, started in the video's
/// directory so same-folder sidecars are one click away.
fn pick_subtitle_file(video: &str) -> Option<String> {
    let mut dlg = rfd::FileDialog::new()
        .set_title("Choose a subtitle file")
        .add_filter(
            "Subtitles",
            &["srt", "ass", "ssa", "vtt", "sub", "sup", "idx"],
        )
        .add_filter("All files", &["*"]);
    if let Some(dir) = dialog_dir(video) {
        dlg = dlg.set_directory(dir);
    }
    dlg.pick_file().map(|p| p.to_string_lossy().into_owned())
}

/// A card to play / stop a media file *on the live compositor* via the control
/// socket. The clip plays into the running compositor and streams to every
/// connected Trireme over the existing QUIC sink — so it's only live when a host
/// is actually connected. Disabled (with an explanatory hover) when the path is
/// empty or no host is reachable.
fn play_media_card(ui: &mut egui::Ui, state: &mut PanelState) {
    let connected = crate::control::handle().connected();

    crate::widgets::card(ui, "compositor_play_media", |ui| {
        crate::theme::section(
            ui,
            ph::FILM_STRIP,
            &tr("compositor.media.heading"),
            &tr("compositor.media.heading_desc"),
        );

        // Path field + native Browse button, persisted in egui temp memory.
        let mut path: String = ui
            .ctx()
            .data(|d| d.get_temp::<String>(id_media_path()))
            .unwrap_or_default();
        ui.horizontal(|ui| {
            if ui
                .button(format!("{}  {}", ph::FOLDER, tr("compositor.media.browse")))
                .describe(tr("compositor.media.browse_desc"))
                .clicked()
            {
                if let Some(picked) = pick_video_file(&path) {
                    path = picked;
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(id_media_path(), path.clone()));
                }
            }
            if ui
                .add(
                    egui::TextEdit::singleline(&mut path)
                        .hint_text(tr("compositor.media.path_hint"))
                        .desired_width(f32::INFINITY),
                )
                .describe(tr("compositor.media.path_desc"))
                .changed()
            {
                ui.ctx()
                    .data_mut(|d| d.insert_temp(id_media_path(), path.clone()));
            }
        });

        // Probe the file's embedded audio + subtitle tracks once per path
        // (cached in temp memory) so the pickers can list them. Only scans a
        // real existing file, so partial typing doesn't fire ffprobe.
        let want = path.trim().to_string();
        let (audio_tracks, sub_tracks): (
            Vec<crate::data::MediaTrack>,
            Vec<crate::data::MediaTrack>,
        ) = {
            type Cached = (
                String,
                Vec<crate::data::MediaTrack>,
                Vec<crate::data::MediaTrack>,
            );
            let cached: Option<Cached> = ui.ctx().data(|d| d.get_temp(id_media_tracks()));
            match cached {
                Some((p, a, s)) if p == want => (a, s),
                _ => {
                    let (a, s) = if !want.is_empty() && std::path::Path::new(&want).is_file() {
                        crate::data::probe_media_tracks(&want)
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    ui.ctx().data_mut(|d| {
                        d.insert_temp(id_media_tracks(), (want.clone(), a.clone(), s.clone()))
                    });
                    (a, s)
                }
            }
        };

        // Subtitle + audio track pickers. Auto = mpv default (embedded +
        // same-dir sidecars for subs); Off; a specific embedded track; or, for
        // subtitles, an external file via the native browser.
        let mut subs: control_ipc::Subtitles = ui
            .ctx()
            .data(|d| d.get_temp(id_media_subs()))
            .unwrap_or_default();
        let mut audio: control_ipc::Audio = ui
            .ctx()
            .data(|d| d.get_temp(id_media_audio()))
            .unwrap_or_default();
        let prev_subs = subs.clone();

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(tr("compositor.media.subs_label"))
                    .small()
                    .weak(),
            );
            egui::ComboBox::from_id_salt("compositor_subs_combo")
                .selected_text(subs_label(&subs, &sub_tracks))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut subs,
                        control_ipc::Subtitles::Auto,
                        tr("compositor.media.subs_auto"),
                    );
                    ui.selectable_value(
                        &mut subs,
                        control_ipc::Subtitles::Off,
                        tr("compositor.media.subs_off"),
                    );
                    for t in &sub_tracks {
                        ui.selectable_value(
                            &mut subs,
                            control_ipc::Subtitles::Track(t.id),
                            track_label(&tr("compositor.media.subs_track"), t),
                        );
                    }
                    // Empty-path File = the "Choose file…" marker, resolved below.
                    ui.selectable_value(
                        &mut subs,
                        control_ipc::Subtitles::File(String::new()),
                        format!("{}  {}", ph::FOLDER, tr("compositor.media.subs_choose")),
                    );
                });

            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(tr("compositor.media.audio_label"))
                    .small()
                    .weak(),
            );
            egui::ComboBox::from_id_salt("compositor_audio_combo")
                .selected_text(audio_label(&audio, &audio_tracks))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut audio,
                        control_ipc::Audio::Auto,
                        tr("compositor.media.subs_auto"),
                    );
                    ui.selectable_value(
                        &mut audio,
                        control_ipc::Audio::Off,
                        tr("compositor.media.subs_off"),
                    );
                    for t in &audio_tracks {
                        ui.selectable_value(
                            &mut audio,
                            control_ipc::Audio::Track(t.id),
                            track_label(&tr("compositor.media.audio_track"), t),
                        );
                    }
                });
        });
        // Resolve the subtitle "Choose file…" marker via the native browser.
        if matches!(&subs, control_ipc::Subtitles::File(p) if p.is_empty()) {
            subs = match pick_subtitle_file(&path) {
                Some(picked) => control_ipc::Subtitles::File(picked),
                None => prev_subs, // cancelled — keep the previous choice
            };
        }
        ui.ctx().data_mut(|d| {
            d.insert_temp(id_media_subs(), subs.clone());
            d.insert_temp(id_media_audio(), audio.clone());
        });

        let has_path = !path.trim().is_empty();

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let play = ui
                .add_enabled(
                    has_path && connected,
                    egui::Button::new(
                        egui::RichText::new(format!(
                            "{}  {}",
                            ph::PLAY,
                            tr("compositor.media.play")
                        ))
                        .strong(),
                    ),
                )
                .describe(tr("compositor.media.play_desc"))
                .on_disabled_hover_text(if !connected {
                    crate::control::unavailable_reason()
                } else {
                    tr("compositor.media.play_need_path")
                });
            if play.clicked() {
                crate::control::handle().send_request(control_ipc::Request::PlayMedia {
                    path: path.trim().to_string(),
                    output: None,
                    fullscreen: true,
                    subtitles: subs.clone(),
                    audio: audio.clone(),
                });
                state.status = Some(format!(
                    "{} {}",
                    tr("compositor.media.status_play_prefix"),
                    path.trim()
                ));
            }

            let stop = ui
                .add_enabled(
                    connected,
                    egui::Button::new(format!("{}  {}", ph::STOP, tr("compositor.media.stop"))),
                )
                .describe(tr("compositor.media.stop_desc"))
                .on_disabled_hover_text(crate::control::unavailable_reason());
            if stop.clicked() {
                crate::control::handle().send_request(control_ipc::Request::StopMedia);
                state.status = Some(tr("compositor.media.status_stop"));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (label, color) = if connected {
                    (tr("control.connected"), crate::theme::online())
                } else {
                    (tr("control.disconnected"), crate::theme::unavailable())
                };
                crate::widgets::status_badge(ui, (label.as_str(), color));
            });
        });

        // Live preview lives inside this card.
        preview_controls(ui);

        // Drain control events so a PlayMedia/StopMedia Ack lands in the status
        // bar (and so the connected flag stays fresh while this card is visible).
        for ev in crate::control::handle().drain_events() {
            match ev {
                control_ipc::Event::Ack { ok, msg } => {
                    let text = msg.unwrap_or_else(|| tr("compositor.media.ack_ok"));
                    if ok {
                        state.status = Some(text);
                    } else {
                        state.last_error =
                            Some(format!("{} {text}", tr("compositor.media.ack_fail_prefix")));
                    }
                }
                control_ipc::Event::Error { msg } => {
                    state.last_error =
                        Some(format!("{} {msg}", tr("compositor.media.ack_fail_prefix")));
                }
                control_ipc::Event::MediaStarted { path } => {
                    state.status = Some(format!(
                        "{} {path}",
                        tr("compositor.media.status_play_prefix")
                    ));
                }
                control_ipc::Event::MediaStopped => {
                    state.status = Some(tr("compositor.media.status_stop"));
                }
                // Status / Latency / Hello are consumed by other surfaces; the
                // cached link state still picks them up via `drain_events`.
                _ => {}
            }
        }
    });
}

/// A `DragValue` for a pixel dimension. Flags the layout dirty (via
/// `mark_dirty`) on change. `desc` is the screen-reader + hover description.
fn size_drag(
    ui: &mut egui::Ui,
    v: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    desc: impl Into<String>,
) {
    let before = *v;
    ui.add(
        egui::DragValue::new(v)
            .speed(2.0)
            .range(range)
            .suffix(" px"),
    )
    .describe(desc);
    if *v != before {
        mark_dirty(ui);
    }
}

/// Stash a "layout changed this frame" flag in egui memory; consumed by
/// `apply_row`'s reconciliation so we don't need to thread a `&mut bool`
/// through every widget helper.
fn mark_dirty(ui: &egui::Ui) {
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new("compositor_changed_this_frame"), true));
}

// A const used as a base port in editor hints. Mirrors CompositorLayout::BASE_PORT
// (15777 — the deliberate Pharos connect port) without touching the shared type's API.
#[allow(non_upper_case_globals)]
const CompositorBasePort: u16 = 15777;

// ---------------------------------------------------------------------------
// Layout validation
// ---------------------------------------------------------------------------

/// Surface soft problems with the layout (duplicate sinks, overlapping tiles).
fn validation_hints(ui: &mut egui::Ui, state: &PanelState) {
    let mut warnings: Vec<String> = Vec::new();

    // Duplicate addresses.
    let mut addrs: Vec<&str> = state
        .layout
        .outputs
        .iter()
        .map(|o| o.addr.as_str())
        .collect();
    addrs.sort_unstable();
    let mut dup = false;
    for w in addrs.windows(2) {
        if w[0] == w[1] {
            dup = true;
            break;
        }
    }
    if dup {
        warnings.push(tr("compositor.warn.dup_addr"));
    }

    // Empty names.
    if state
        .layout
        .outputs
        .iter()
        .any(|o| o.name.trim().is_empty())
    {
        warnings.push(tr("compositor.warn.empty_name"));
    }

    // Overlapping tiles (O(n²); layouts are tiny).
    if has_overlap(&state.layout.outputs) {
        warnings.push(tr("compositor.warn.overlap"));
    }

    // Over the configured encode pixel-rate budget. Soft warning — the canvas
    // already shows the live %, this surfaces it at save time too. Mirror runs ONE
    // shared encode, so the cost is the largest output once, not Σ (matches the
    // canvas overlay).
    let mirror = crate::control::handle()
        .status()
        .is_some_and(|s| matches!(s.stream_mode, control_ipc::StreamMode::Mirror { .. }));
    let budget_px: u64 = if mirror {
        state
            .layout
            .outputs
            .iter()
            .map(|o| o.w as u64 * o.h as u64)
            .max()
            .unwrap_or(0)
    } else {
        state
            .layout
            .outputs
            .iter()
            .map(|o| o.w as u64 * o.h as u64)
            .sum()
    };
    let pps: u64 = budget_px.saturating_mul(state.layout.fps as u64);
    let budget = state.encode_budget.max(1);
    if pps > budget {
        let pct = ((pps as f64 / budget as f64) * 100.0).round() as i64;
        warnings.push(format!("{} ({pct}%)", tr("compositor.warn.over_budget")));
    }

    for w in &warnings {
        ui.colored_label(crate::theme::warn(), format!("{}  {w}", ph::WARNING));
    }
}

fn has_overlap(outputs: &[OutputTile]) -> bool {
    for i in 0..outputs.len() {
        for j in (i + 1)..outputs.len() {
            let a = &outputs[i];
            let b = &outputs[j];
            let ax2 = a.x + a.w as i32;
            let ay2 = a.y + a.h as i32;
            let bx2 = b.x + b.w as i32;
            let by2 = b.y + b.h as i32;
            let sep = a.x >= bx2 || b.x >= ax2 || a.y >= by2 || b.y >= ay2;
            if !sep {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Performance — live per-stage frame timing
// ---------------------------------------------------------------------------

/// Live per-stage frame-timings table. Issues a `GetFrameTimings` request on
/// every render tick while this section is mounted; that request also acts as
/// an implicit subscribe (the host arms instrumentation for ~5s, so re-asking
/// at any panel-tick cadence keeps it lit). Reads the cached snapshot +
/// sparkline history from the control handle.
///
/// Renders one row per stage: stage name (left) + mean/p95/max µs columns +
/// a tiny in-place sparkline (a Painter line through the last 30 mean
/// samples). All numerics right-aligned, monospace, so a stage with a spike
/// shows up immediately. When no host is connected the rows still render
/// but with zeros / empty sparklines — so the layout doesn't jump as a host
/// comes and goes.
fn performance_section(ui: &mut egui::Ui) {
    use crate::control;
    use control_ipc::StageStats;

    // Subscribe + poll on the render tick: each call is the panel's heartbeat
    // keeping host instrumentation hot. The host implicitly arms a 5s
    // subscription, so the cost gracefully goes to zero ~5s after the user
    // closes this expander or switches tabs (no explicit unsubscribe needed).
    control::request_frame_timings();
    // Repaint sooner than the default 1Hz tick so the sparkline animates
    // smoothly between snapshots — once-per-second still drives the data,
    // but we ease in between with the cached history.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(900));

    let snap = control::handle().frame_timings().unwrap_or_default();
    let history = control::handle().frame_history();
    let connected = control::handle().connected();

    // No own card — the enclosing collapsing_card supplies the frame.
    {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(tr("compositor.perf.subtitle"))
                    .small()
                    .weak(),
            );
            if !connected {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::theme::badge(ui, &tr("compositor.perf.no_link"), crate::theme::warn());
                });
            } else if snap.window_frames == 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::theme::badge(ui, &tr("compositor.perf.warming"), crate::theme::info());
                });
            } else {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  {} {}",
                            ph::FILM_STRIP,
                            snap.window_frames,
                            tr("compositor.perf.frames")
                        ))
                        .small()
                        .weak(),
                    );
                });
            }
        });
        ui.add_space(6.0);

        let stages: [(&str, &StageStats, &[u32]); 8] = [
            (
                "compositor.perf.stage.dispatch",
                &snap.dispatch,
                &history.dispatch,
            ),
            ("compositor.perf.stage.ctrl", &snap.ctrl, &history.ctrl),
            ("compositor.perf.stage.input", &snap.input, &history.input),
            ("compositor.perf.stage.gyro", &snap.gyro, &history.gyro),
            (
                "compositor.perf.stage.render",
                &snap.render,
                &history.render,
            ),
            ("compositor.perf.stage.post", &snap.post, &history.post),
            ("compositor.perf.stage.pace", &snap.pace, &history.pace),
            (
                "compositor.perf.stage.status",
                &snap.status,
                &history.status,
            ),
        ];

        // Compute a single shared y-axis max across all sparklines so a row's
        // bar height is comparable to its neighbours (otherwise a tiny stage
        // with a 100µs max can look as spiky as a 5ms stage and the user
        // misreads it). Lower-bounded so a flat-zero stage doesn't divide by
        // zero in the normalize step.
        let global_max = stages
            .iter()
            .flat_map(|(_, _, h)| h.iter().copied())
            .max()
            .unwrap_or(0)
            .max(1);

        // Header row. Each numeric column gets a FIXED rect via
        // `allocate_ui_with_layout` so the right-to-left layout applies only
        // *inside* that 70px-wide box. The previous shape — a bare
        // `with_layout(right_to_left, …)` inside a horizontal scope — handed
        // each column the full remaining row width, so the first column ate
        // everything and Mean / p95 / Max stacked at column zero.
        ui.horizontal(|ui| {
            ui.add_sized(
                [120.0, 16.0],
                egui::Label::new(
                    egui::RichText::new(tr("compositor.perf.col.stage"))
                        .small()
                        .strong(),
                ),
            );
            for key in [
                "compositor.perf.col.mean",
                "compositor.perf.col.p95",
                "compositor.perf.col.max",
            ] {
                ui.allocate_ui_with_layout(
                    egui::Vec2::new(70.0, 16.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.label(egui::RichText::new(tr(key)).small().strong());
                    },
                );
            }
            ui.add_sized(
                [80.0, 16.0],
                egui::Label::new(
                    egui::RichText::new(tr("compositor.perf.col.trend"))
                        .small()
                        .strong(),
                ),
            );
        });
        ui.separator();

        for (label_key, stats, hist) in stages.iter() {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [120.0, 20.0],
                    egui::Label::new(egui::RichText::new(tr(label_key))),
                );
                let dim = stats.samples == 0;
                for value in [stats.mean_us, stats.p95_us, stats.max_us] {
                    // Same fix as the header: a fixed 70×20 rect with a
                    // right-to-left layout *inside* it, so each column gets
                    // exactly one cell instead of eating the rest of the row.
                    ui.allocate_ui_with_layout(
                        egui::Vec2::new(70.0, 20.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let text = egui::RichText::new(format!("{} µs", value)).monospace();
                            let text = if dim { text.weak() } else { text };
                            ui.label(text);
                        },
                    );
                }
                draw_sparkline(ui, hist, global_max);
            });
        }
    }
}

/// Draw a tiny per-row line chart: one polyline through the rolling history,
/// normalised against the table-wide max so all rows share a y-axis. Empty
/// history draws a flat baseline so the row doesn't collapse vertically.
fn draw_sparkline(ui: &mut egui::Ui, samples: &[u32], y_max: u32) {
    use egui::{Pos2, Stroke, Vec2};
    // Allocate a fixed-size rect on the right so every row stays aligned even
    // with a wide font scale. 80×16 fits cleanly next to the µs columns.
    let (rect, _resp) = ui.allocate_exact_size(Vec2::new(80.0, 16.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    // Faint baseline so an empty row reads as "no data" not "missing widget".
    let baseline_y = rect.bottom() - 1.0;
    painter.line_segment(
        [
            Pos2::new(rect.left(), baseline_y),
            Pos2::new(rect.right(), baseline_y),
        ],
        Stroke::new(0.5_f32, ui.visuals().weak_text_color()),
    );
    if samples.len() < 2 {
        return;
    }
    let h = rect.height() - 2.0;
    let w = rect.width();
    let span = (samples.len() - 1) as f32;
    let y_max = y_max.max(1) as f32;
    let mut prev: Option<Pos2> = None;
    for (i, &v) in samples.iter().enumerate() {
        let x = rect.left() + (i as f32 / span) * w;
        let norm = (v as f32 / y_max).clamp(0.0, 1.0);
        let y = baseline_y - norm * h;
        let p = Pos2::new(x, y);
        if let Some(q) = prev {
            painter.line_segment(
                [q, p],
                Stroke::new(1.2_f32, ui.visuals().widgets.active.fg_stroke.color),
            );
        }
        prev = Some(p);
    }
}

// ---------------------------------------------------------------------------
// Footnote
// ---------------------------------------------------------------------------

fn about_section(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(tr("compositor.footnote.what")).small());
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(tr("compositor.footnote.status"))
            .small()
            .color(crate::theme::warn()),
    );
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(tr("compositor.footnote.tip"))
            .small()
            .weak(),
    );
}
