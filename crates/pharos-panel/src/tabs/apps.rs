// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Apps tab — a box-art launcher grid plus a full visual "Create application"
//! editor.
//!
//! The grid shows the saved [`app_launcher`] catalog (with the built-in Desktop
//! tile first) as cards with box art / a kind placeholder; each can be launched
//! on the live host (control-IPC `LaunchApp`), edited, or deleted. The editor is
//! easy-mode first: scan Steam, pick an installed `.desktop` app, or browse for
//! an executable, and the launch command + box art are filled in for you — then
//! every field is yours to refine.
//!
//! Catalog read/write goes through the shared `app-launcher` crate, so the host
//! reads back exactly what's edited here (one `apps.toml`, one schema).

use std::path::Path;

use egui_phosphor::regular as ph;

use app_launcher::config::{AppEntry, AppKind, FpsCap, GyroMode, LaunchTarget};

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::{AppEditor, PanelState};

/// Box-art card thumbnail size (portrait, ~2:3 like Steam grid art).
const THUMB: egui::Vec2 = egui::vec2(132.0, 198.0);

pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    ui.heading(format!("{}  {}", ph::ROCKET_LAUNCH, tr("apps.heading")))
        .describe(tr("apps.intro"));
    ui.label(egui::RichText::new(tr("apps.intro")).weak());
    ui.add_space(8.0);

    let connected = crate::control::handle().connected();

    ui.horizontal(|ui| {
        if ui
            .button(format!("{}  {}", ph::PLUS, tr("apps.toolbar.add")))
            .describe(tr("apps.toolbar.add_desc"))
            .clicked()
        {
            state.apps_editor = Some(AppEditor::new());
        }
        ui.add_space(12.0);
        if connected {
            ui.label(
                egui::RichText::new(format!("{}  {}", ph::PLAY, tr("apps.live_on")))
                    .color(crate::theme::online()),
            );
        } else {
            ui.label(egui::RichText::new(tr("apps.live_off")).weak());
        }
    });
    ui.add_space(10.0);

    // The editor takes over the top of the tab while open.
    if state.apps_editor.is_some() {
        editor_ui(ui, state);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
    }

    grid_ui(ui, state, connected);
}

// ---------------------------------------------------------------------------
// Launcher grid
// ---------------------------------------------------------------------------

/// What a card click asked for, applied after the immutable iteration.
enum Action {
    Launch(String),
    Edit(usize),
    Delete(usize),
}

fn grid_ui(ui: &mut egui::Ui, state: &mut PanelState, connected: bool) {
    let mut action: Option<Action> = None;

    // Top-aligned wrapping row (Align::Min, not horizontal_wrapped's default
    // Align::Center): with cards of slightly different height, centre-aligning
    // each within its row offsets their tops and reads as a staggered "walk".
    // Min pins every card in a row to a shared top edge.
    ui.with_layout(
        egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true),
        |ui| {
            // Built-in Desktop tile (launch-only — returns the stream to the
            // plain virtual desktop; not editable).
            let desktop = AppEntry::desktop_builtin();
            card(ui, &desktop, connected, None, &mut action);

            // User apps, in saved order. `idx` indexes state.apps.apps.
            for (idx, app) in state.apps.apps.iter().enumerate() {
                card(ui, app, connected, Some(idx), &mut action);
            }
        },
    );

    if state.apps.apps.is_empty() {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(tr("apps.empty")).weak());
    }

    match action {
        Some(Action::Launch(slug)) => {
            crate::control::launch_app(slug.clone());
            state.status = Some(format!("{} {slug}", tr("apps.launched")));
        }
        Some(Action::Edit(idx)) => {
            if let Some(app) = state.apps.apps.get(idx).cloned() {
                state.apps_editor = Some(AppEditor::edit(idx, app));
            }
        }
        Some(Action::Delete(idx)) if idx < state.apps.apps.len() => {
            let removed = state.apps.apps.remove(idx);
            state.dirty_apps = true;
            state.status = Some(format!("{} {}", tr("apps.deleted"), removed.name));
            // Keep the open editor pointing at the right entry: removing an
            // entry shifts every later index down by one. Close it if it was
            // editing the deleted one; decrement if it was editing a later one
            // (else a save would overwrite the wrong app).
            if let Some(e) = &mut state.apps_editor {
                match e.editing {
                    Some(n) if n == idx => state.apps_editor = None,
                    Some(n) if n > idx => e.editing = Some(n - 1),
                    _ => {}
                }
            }
        }
        // Out-of-range delete index (a stale UI event after the list shrank): no-op.
        Some(Action::Delete(_)) => {}
        None => {}
    }
}

/// One launcher card.
fn card(
    ui: &mut egui::Ui,
    app: &AppEntry,
    connected: bool,
    idx: Option<usize>,
    action: &mut Option<Action>,
) {
    let card_w = THUMB.x + 24.0;
    // Reserve an EXACT cell and force the frame to fill it (set_min_size), so
    // every tile is the same height regardless of content. Otherwise the shared
    // `widgets::card` sizes to content, and the built-in Desktop tile (a one-line
    // "Built-in" label) ends up shorter than the editable tiles (a taller
    // Edit/Delete button row). Generous enough to hold the tallest tile at any
    // font scale (thumb + name + kind + launch + edit/delete).
    let card_h = THUMB.y + 130.0;
    let frame = egui::Frame::group(ui.style())
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(12.0))
        .fill(ui.visuals().widgets.inactive.weak_bg_fill);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(card_w, card_h), egui::Sense::hover());
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    frame.show(&mut content, |ui| {
        ui.set_min_size(egui::vec2(card_w - 24.0, card_h - 24.0));
        ui.push_id(("apps_card", &app.slug), |ui| {
            ui.vertical_centered(|ui| {
                draw_thumb(ui, app.box_art.as_deref(), app.kind, THUMB);
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&app.name).strong());
                ui.label(egui::RichText::new(kind_label(app.kind)).small().weak());

                ui.add_space(4.0);
                // A status-only host (Windows) can't launch apps — disable.
                let read_only = crate::control::host_is_read_only();
                let launch = ui
                    .add_enabled(
                        connected && !read_only,
                        egui::Button::new(format!("{}  {}", ph::PLAY, tr("apps.card.launch"))),
                    )
                    .on_disabled_hover_text(if read_only {
                        tr("control.read_only")
                    } else {
                        crate::control::unavailable_reason()
                    })
                    .describe(tr("apps.card.launch_desc"));
                if launch.clicked() {
                    *action = Some(Action::Launch(app.slug.clone()));
                }

                if let Some(idx) = idx {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button(format!(
                                "{}  {}",
                                ph::PENCIL_SIMPLE,
                                tr("apps.card.edit")
                            ))
                            .clicked()
                        {
                            *action = Some(Action::Edit(idx));
                        }
                        if ui
                            .small_button(format!("{}  {}", ph::TRASH, tr("apps.card.delete")))
                            .clicked()
                        {
                            *action = Some(Action::Delete(idx));
                        }
                    });
                } else {
                    ui.label(egui::RichText::new(tr("apps.card.builtin")).small().weak());
                }
            });
        });
    });
}

// ---------------------------------------------------------------------------
// Create / edit editor
// ---------------------------------------------------------------------------

fn editor_ui(ui: &mut egui::Ui, state: &mut PanelState) {
    // Take the editor out so we can borrow both it and `state.apps` freely; put
    // it back at the end unless the user saved or cancelled.
    let mut editor = match state.apps_editor.take() {
        Some(e) => e,
        None => return,
    };
    let mut keep = true;

    crate::widgets::card(ui, "apps_editor", |ui| {
        let title = if editor.editing.is_some() {
            tr("apps.editor.edit_title")
        } else {
            tr("apps.editor.new_title")
        };
        ui.heading(title);
        ui.add_space(6.0);

        easy_mode(ui, &mut editor);
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        form_fields(ui, &mut editor.draft);

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let valid =
                !editor.draft.name.trim().is_empty() && !editor.draft.command.trim().is_empty();
            let save = ui
                .add_enabled(
                    valid,
                    egui::Button::new(format!("{}  {}", ph::FLOPPY_DISK, tr("apps.editor.save"))),
                )
                .describe(tr("apps.editor.save_desc"));
            if save.clicked() {
                if let Err(e) = commit(state, &mut editor) {
                    state.last_error = Some(e);
                } else {
                    keep = false;
                }
            }
            if ui
                .button(format!("{}  {}", ph::X, tr("apps.editor.cancel")))
                .clicked()
            {
                keep = false;
            }
        });
    });

    if keep {
        state.apps_editor = Some(editor);
    }
}

/// Validate and write the draft back into the catalog, marking it dirty.
fn commit(state: &mut PanelState, editor: &mut AppEditor) -> Result<(), String> {
    let draft = &mut editor.draft;
    draft.name = draft.name.trim().to_string();
    // Auto-derive a slug from the name if the user left it blank.
    if draft.slug.trim().is_empty() {
        draft.slug = app_launcher::scan::slugify(&draft.name);
    } else {
        draft.slug = app_launcher::scan::slugify(&draft.slug);
    }

    // Reject a slug that collides with another entry (or the Desktop built-in).
    if draft.slug == "desktop" {
        return Err(tr("apps.editor.slug_reserved"));
    }
    let clashes = state
        .apps
        .apps
        .iter()
        .enumerate()
        .any(|(i, a)| Some(i) != editor.editing && a.slug.eq_ignore_ascii_case(&draft.slug));
    if clashes {
        return Err(format!("{} {}", tr("apps.editor.slug_taken"), draft.slug));
    }

    match editor.editing {
        Some(idx) if idx < state.apps.apps.len() => state.apps.apps[idx] = draft.clone(),
        _ => state.apps.apps.push(draft.clone()),
    }
    state.dirty_apps = true;
    state.status = Some(format!("{} {}", tr("apps.saved"), draft.name));
    Ok(())
}

/// Easy-mode row: scan Steam / installed apps / browse, plus the picker list.
fn easy_mode(ui: &mut egui::Ui, editor: &mut AppEditor) {
    ui.label(egui::RichText::new(tr("apps.easy.title")).strong());
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(format!(
                "{}  {}",
                ph::STEAM_LOGO,
                tr("apps.easy.scan_steam")
            ))
            .describe(tr("apps.easy.scan_steam_desc"))
            .clicked()
        {
            editor.steam = Some(app_launcher::scan::scan_steam());
            editor.desktop = None;
            editor.picker_filter.clear();
        }
        if ui
            .button(format!(
                "{}  {}",
                ph::SQUARES_FOUR,
                tr("apps.easy.installed")
            ))
            .describe(tr("apps.easy.installed_desc"))
            .clicked()
        {
            editor.desktop = Some(app_launcher::scan::scan_desktop_entries());
            editor.steam = None;
            editor.picker_filter.clear();
        }
        if ui
            .button(format!("{}  {}", ph::FOLDER, tr("apps.easy.browse_exe")))
            .describe(tr("apps.easy.browse_exe_desc"))
            .clicked()
        {
            if let Some(p) = pick_executable(&editor.draft.command) {
                let suggestion = app_launcher::scan::suggest_from_executable(Path::new(&p));
                apply_suggestion(&mut editor.draft, suggestion);
                editor.steam = None;
                editor.desktop = None;
            }
        }
    });

    // Steam picker.
    if let Some(games) = &editor.steam {
        ui.add_space(6.0);
        if games.is_empty() {
            ui.label(egui::RichText::new(tr("apps.easy.none_found")).weak());
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut editor.picker_filter)
                    .hint_text(tr("apps.easy.filter")),
            );
            let filter = editor.picker_filter.to_lowercase();
            let mut chosen: Option<AppEntry> = None;
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .id_salt("apps_steam_picker")
                .show(ui, |ui| {
                    for g in games
                        .iter()
                        .filter(|g| filter.is_empty() || g.name.to_lowercase().contains(&filter))
                    {
                        if ui
                            .selectable_label(false, format!("{}  ·  {}", g.name, g.appid))
                            .clicked()
                        {
                            chosen = Some(app_launcher::scan::suggest_from_steam(g));
                        }
                    }
                });
            if let Some(s) = chosen {
                apply_suggestion(&mut editor.draft, s);
                editor.steam = None;
            }
        }
    }

    // Installed-app (.desktop) picker.
    if let Some(apps) = &editor.desktop {
        ui.add_space(6.0);
        if apps.is_empty() {
            ui.label(egui::RichText::new(tr("apps.easy.none_found")).weak());
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut editor.picker_filter)
                    .hint_text(tr("apps.easy.filter")),
            );
            let filter = editor.picker_filter.to_lowercase();
            let mut chosen: Option<AppEntry> = None;
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .id_salt("apps_desktop_picker")
                .show(ui, |ui| {
                    for a in apps
                        .iter()
                        .filter(|a| filter.is_empty() || a.name.to_lowercase().contains(&filter))
                    {
                        if ui.selectable_label(false, &a.name).clicked() {
                            chosen = Some(app_launcher::scan::suggest_from_desktop(a));
                        }
                    }
                });
            if let Some(s) = chosen {
                apply_suggestion(&mut editor.draft, s);
                editor.desktop = None;
            }
        }
    }
}

/// Merge an easy-mode suggestion into the draft, keeping any name the user
/// already typed (so re-scanning doesn't clobber a deliberate rename).
fn apply_suggestion(draft: &mut AppEntry, s: AppEntry) {
    let keep_name = !draft.name.trim().is_empty();
    let prev_name = draft.name.clone();
    *draft = s;
    if keep_name {
        draft.name = prev_name;
    }
}

/// All the editable fields.
fn form_fields(ui: &mut egui::Ui, draft: &mut AppEntry) {
    egui::Grid::new("apps_form")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label(tr("apps.field.name"));
            ui.add(egui::TextEdit::singleline(&mut draft.name).desired_width(320.0));
            ui.end_row();

            ui.label(tr("apps.field.slug"));
            ui.add(
                egui::TextEdit::singleline(&mut draft.slug)
                    .hint_text(tr("apps.field.slug_hint"))
                    .desired_width(320.0),
            )
            .describe(tr("apps.field.slug_desc"));
            ui.end_row();

            ui.label(tr("apps.field.kind"));
            egui::ComboBox::from_id_salt("apps_kind")
                .selected_text(kind_label(draft.kind))
                .show_ui(ui, |ui| {
                    for k in [
                        AppKind::App,
                        AppKind::Game,
                        AppKind::Media,
                        AppKind::Desktop,
                    ] {
                        ui.selectable_value(&mut draft.kind, k, kind_label(k));
                    }
                });
            ui.end_row();

            // Gyro policy: how the host handles controller gyro while this app
            // is foreground. Mirrors the Kind picker pattern above.
            ui.label(tr("apps.field.gyro"));
            let _ = egui::ComboBox::from_id_salt("apps_gyro")
                .selected_text(gyro_label(draft.gyro_mode))
                .show_ui(ui, |ui| {
                    for m in [GyroMode::Enabled, GyroMode::Mouse, GyroMode::Disabled] {
                        ui.selectable_value(&mut draft.gyro_mode, m, gyro_label(m));
                    }
                })
                .response
                .describe(tr("apps.field.gyro_desc"));
            ui.end_row();

            // FPS cap + perf overlay both ride MangoHud, so gate them on its
            // presence — the env still no-ops harmlessly without it, but we don't
            // want to offer a dead knob. (Checked on the panel's box; normally the
            // host box too. The Windows RTSS/Afterburner equivalent isn't wired yet,
            // so these stay disabled there.)
            let mango = app_launcher::launch::mangohud_available();

            // FPS cap: off / auto (match the virtual output's refresh) / fixed.
            // Realised as a MangoHud frame limiter on launch — pure efficiency
            // for a stream whose output is a fixed refresh.
            ui.label(tr("apps.field.fps_cap"));
            ui.add_enabled_ui(mango, |ui| fps_cap_picker(ui, draft));
            ui.end_row();

            // Perf overlay: burn MangoHud's HUD into the stream so it's readable on
            // the client (the productivity/monitoring case). Flips no_display=0.
            ui.label(tr("apps.field.perf_overlay"));
            ui.vertical(|ui| {
                ui.add_enabled_ui(mango, |ui| {
                    ui.checkbox(&mut draft.perf_overlay, tr("apps.field.perf_overlay_on"))
                        .describe(tr("apps.field.perf_overlay_desc"));
                });
                if !mango {
                    ui.label(
                        egui::RichText::new(tr("apps.field.mangohud_absent"))
                            .small()
                            .weak(),
                    );
                }
            });
            ui.end_row();

            ui.label(tr("apps.field.command"));
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut draft.command)
                        .hint_text("setsid -f steam steam://rungameid/…")
                        .desired_width(360.0),
                )
                .describe(tr("apps.field.command_desc"));
                ui.checkbox(&mut draft.shell, tr("apps.field.shell"))
                    .describe(tr("apps.field.shell_desc"));
            });
            ui.end_row();

            ui.label(tr("apps.field.target"));
            target_picker(ui, draft);
            ui.end_row();

            ui.label(tr("apps.field.box_art"));
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let mut art = draft.box_art.clone().unwrap_or_default();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut art)
                            .hint_text(tr("apps.field.box_art_hint"))
                            .desired_width(280.0),
                    );
                    if resp.changed() {
                        draft.box_art = (!art.trim().is_empty()).then_some(art);
                    }
                    if ui
                        .button(format!("{}  {}", ph::FOLDER, tr("apps.field.browse")))
                        .clicked()
                    {
                        if let Some(p) = pick_image(draft.box_art.as_deref().unwrap_or("")) {
                            draft.box_art = Some(p);
                        }
                    }
                });
                // Live preview of the chosen art.
                if let Some(p) = draft.box_art.as_deref() {
                    if !p.trim().is_empty() {
                        ui.add_space(4.0);
                        draw_thumb(ui, Some(p), draft.kind, egui::vec2(96.0, 144.0));
                    }
                }
            });
            ui.end_row();
        });

    // Advanced (working dir + extra env), collapsed by default.
    ui.add_space(4.0);
    egui::CollapsingHeader::new(tr("apps.advanced"))
        .id_salt("apps_advanced")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(tr("apps.field.cwd"));
                let mut cwd = draft.cwd.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut cwd)
                            .hint_text(tr("apps.field.cwd_hint"))
                            .desired_width(320.0),
                    )
                    .changed()
                {
                    draft.cwd = (!cwd.trim().is_empty()).then_some(cwd);
                }
            });

            ui.add_space(6.0);
            ui.label(egui::RichText::new(tr("apps.field.env")).small().weak());
            let mut remove: Option<usize> = None;
            for (i, (k, v)) in draft.env.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(k)
                            .desired_width(140.0)
                            .hint_text("KEY"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(v)
                            .desired_width(200.0)
                            .hint_text("VALUE"),
                    );
                    if ui.small_button(ph::X).clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                draft.env.remove(i);
            }
            if ui
                .button(format!("{}  {}", ph::PLUS, tr("apps.field.env_add")))
                .clicked()
            {
                draft.env.push((String::new(), String::new()));
            }
        });
}

/// Launch-target picker. The virtual-display targets work today; the physical
/// ones are the carried-but-not-yet-wired seam (a note explains).
fn target_picker(ui: &mut egui::Ui, draft: &mut AppEntry) {
    ui.vertical(|ui| {
        egui::ComboBox::from_id_salt("apps_target")
            .selected_text(target_label(&draft.target))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(
                        matches!(draft.target, LaunchTarget::VirtualDisplay),
                        tr("apps.target.virtual"),
                    )
                    .clicked()
                {
                    draft.target = LaunchTarget::VirtualDisplay;
                }
                if ui
                    .selectable_label(
                        matches!(draft.target, LaunchTarget::VirtualOutput(_)),
                        tr("apps.target.virtual_output"),
                    )
                    .clicked()
                    && !matches!(draft.target, LaunchTarget::VirtualOutput(_))
                {
                    draft.target = LaunchTarget::VirtualOutput(String::new());
                }
                if ui
                    .selectable_label(
                        matches!(draft.target, LaunchTarget::ExtendPhysical),
                        tr("apps.target.extend"),
                    )
                    .clicked()
                {
                    draft.target = LaunchTarget::ExtendPhysical;
                }
                if ui
                    .selectable_label(
                        matches!(draft.target, LaunchTarget::DuplicatePhysical),
                        tr("apps.target.duplicate"),
                    )
                    .clicked()
                {
                    draft.target = LaunchTarget::DuplicatePhysical;
                }
            });

        if let LaunchTarget::VirtualOutput(name) = &mut draft.target {
            ui.add(
                egui::TextEdit::singleline(name)
                    .hint_text(tr("apps.target.output_name_hint"))
                    .desired_width(200.0),
            );
        }
        if !draft.target.is_supported() {
            ui.label(
                egui::RichText::new(format!(
                    "{}  {}",
                    ph::WARNING,
                    tr("apps.target.physical_note")
                ))
                .small()
                .color(crate::theme::info()),
            );
        }
    });
}

/// FPS-cap picker: off / auto / a fixed number. Mirrors [`target_picker`] — the
/// `Fixed` variant carries a value, so it uses `selectable_label` + `matches!`
/// rather than `selectable_value`, and reveals a number entry when chosen.
fn fps_cap_picker(ui: &mut egui::Ui, draft: &mut AppEntry) {
    ui.vertical(|ui| {
        egui::ComboBox::from_id_salt("apps_fps_cap")
            .selected_text(fps_cap_label(draft.fps_cap))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(matches!(draft.fps_cap, FpsCap::Off), tr("apps.fps_cap.off"))
                    .clicked()
                {
                    draft.fps_cap = FpsCap::Off;
                }
                if ui
                    .selectable_label(
                        matches!(draft.fps_cap, FpsCap::Auto),
                        tr("apps.fps_cap.auto"),
                    )
                    .clicked()
                {
                    draft.fps_cap = FpsCap::Auto;
                }
                if ui
                    .selectable_label(
                        matches!(draft.fps_cap, FpsCap::Fixed(_)),
                        tr("apps.fps_cap.fixed"),
                    )
                    .clicked()
                    && !matches!(draft.fps_cap, FpsCap::Fixed(_))
                {
                    // Seed with a sane default the user can dial in below.
                    draft.fps_cap = FpsCap::Fixed(60);
                }
            })
            .response
            .describe(tr("apps.field.fps_cap_desc"));

        if let FpsCap::Fixed(n) = &mut draft.fps_cap {
            ui.add(
                egui::DragValue::new(n)
                    .range(1..=1000)
                    .speed(1.0)
                    .suffix(" fps"),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

/// Draw box art at `size`, or a kind-appropriate placeholder if there's no
/// image (or it fails to decode). Aspect-preserving, centered, on a rounded card.
fn draw_thumb(ui: &mut egui::Ui, art_path: Option<&str>, kind: AppKind, size: egui::Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let rounding = egui::Rounding::same(8.0);
    let painter = ui.painter();
    painter.rect_filled(rect, rounding, ui.visuals().widgets.inactive.weak_bg_fill);

    let tex = art_path
        .filter(|p| !p.trim().is_empty())
        .and_then(|p| art_texture(ui.ctx(), p));
    match tex {
        Some(t) => {
            let [tw, th] = t.size();
            let (tw, th) = (tw as f32, th as f32);
            let scale = (size.x / tw).min(size.y / th);
            let draw = egui::vec2(tw * scale, th * scale);
            let inner = egui::Rect::from_center_size(rect.center(), draw);
            painter.image(
                t.id(),
                inner,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                kind_glyph(kind),
                egui::FontId::proportional(size.x * 0.4),
                ui.visuals().weak_text_color(),
            );
        }
    }
}

/// Load (and cache in egui temp memory) a box-art texture from a path.
fn art_texture(ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
    // Key on (path, mtime) so replacing the file on disk refreshes the texture
    // instead of showing the stale cached one (same pattern as the compositor
    // preview). A missing file → mtime None → still cached under that key.
    let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let id = egui::Id::new(("apps_art_tex", path, mtime));
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let img = load_image_any(Path::new(path))?;
    let handle = ctx.load_texture(
        format!("apps_art_{path}"),
        img,
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    Some(handle)
}

/// Decode a PNG/JPEG (format guessed) into an egui image. `None` on any failure
/// (missing file, unsupported format like SVG) — the caller falls back to a glyph.
fn load_image_any(path: &Path) -> Option<egui::ColorImage> {
    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        rgba.as_raw(),
    ))
}

// ---------------------------------------------------------------------------
// Labels + file dialogs
// ---------------------------------------------------------------------------

fn kind_label(k: AppKind) -> String {
    match k {
        AppKind::App => tr("apps.kind.app"),
        AppKind::Game => tr("apps.kind.game"),
        AppKind::Desktop => tr("apps.kind.desktop"),
        AppKind::Media => tr("apps.kind.media"),
    }
}

/// User-facing label for a [`GyroMode`] dropdown row — short headline + the
/// concrete behaviour so the user doesn't have to remember which mode does what.
fn gyro_label(m: GyroMode) -> String {
    match m {
        GyroMode::Enabled => tr("apps.gyro.enabled"),
        GyroMode::Mouse => tr("apps.gyro.mouse"),
        GyroMode::Disabled => tr("apps.gyro.disabled"),
    }
}

fn kind_glyph(k: AppKind) -> &'static str {
    match k {
        AppKind::App => ph::APP_WINDOW,
        AppKind::Game => ph::GAME_CONTROLLER,
        AppKind::Desktop => ph::MONITOR,
        AppKind::Media => ph::FILM_STRIP,
    }
}

fn target_label(t: &LaunchTarget) -> String {
    match t {
        LaunchTarget::VirtualDisplay => tr("apps.target.virtual"),
        LaunchTarget::VirtualOutput(_) => tr("apps.target.virtual_output"),
        LaunchTarget::ExtendPhysical => tr("apps.target.extend"),
        LaunchTarget::DuplicatePhysical => tr("apps.target.duplicate"),
    }
}

/// Dropdown headline for an [`FpsCap`]. The `Fixed` value itself is shown by the
/// adjacent number entry, so the label stays generic.
fn fps_cap_label(c: FpsCap) -> String {
    match c {
        FpsCap::Off => tr("apps.fps_cap.off"),
        FpsCap::Auto => tr("apps.fps_cap.auto"),
        FpsCap::Fixed(_) => tr("apps.fps_cap.fixed"),
    }
}

fn pick_executable(current: &str) -> Option<String> {
    let mut dlg = rfd::FileDialog::new().set_title(tr("apps.easy.browse_exe"));
    if let Some(dir) = dialog_dir(current) {
        dlg = dlg.set_directory(dir);
    }
    dlg.pick_file().map(|p| p.to_string_lossy().into_owned())
}

fn pick_image(current: &str) -> Option<String> {
    let mut dlg = rfd::FileDialog::new()
        .set_title(tr("apps.field.box_art"))
        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "bmp"])
        .add_filter("All files", &["*"]);
    if let Some(dir) = dialog_dir(current) {
        dlg = dlg.set_directory(dir);
    }
    dlg.pick_file().map(|p| p.to_string_lossy().into_owned())
}

/// Best starting directory for a file dialog given the current value.
fn dialog_dir(current: &str) -> Option<std::path::PathBuf> {
    let t = current.trim();
    if !t.is_empty() {
        let p = Path::new(t);
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
