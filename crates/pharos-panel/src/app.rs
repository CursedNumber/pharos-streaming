// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! The eframe application shell: top bar (host identity), left sidebar (tab
//! switcher), central panel (the selected tab's body), and a bottom status bar
//! with the Save indicator. Applies the user's chosen theme + font scale (see
//! `apply_appearance`) and requests a slow repaint so live data can tick.

use std::time::Duration;

use crate::a11y::Describe;
use crate::model::{FontScale, PanelState, Tab, Theme};
use crate::tabs;

/// Localised one-line description of each tab (hover tooltip + screen-reader
/// announcement on the sidebar buttons).
fn tab_describe(t: Tab) -> String {
    let key = match t {
        Tab::Overview => "tab.status_desc",
        Tab::Compositor => "tab.compositor_desc",
        Tab::Video => "tab.video_desc",
        Tab::Audio => "tab.audio_desc",
        Tab::Input => "tab.input_desc",
        Tab::Controllers => "tab.controller_desc",
        Tab::Apps => "tab.apps_desc",
        Tab::Settings => "tab.settings_desc",
    };
    crate::i18n::tr(key)
}

/// Stable id of the focusable landmark at the top of the panel content. The
/// sidebar moves focus here when you press Tab or the Right arrow on a nav item.
fn content_anchor_id() -> egui::Id {
    egui::Id::new("pharos_content_anchor")
}

/// The whole app — the panel state plus the appearance we last pushed to egui.
pub struct PanelApp {
    state: PanelState,
    /// Last theme/scale/language we pushed to egui, so we only re-apply on change.
    applied_theme: Option<Theme>,
    applied_scale: Option<FontScale>,
    applied_lang: Option<String>,
    /// Tracks the currently-shown transient status note + when it first appeared,
    /// so a one-shot progress string ("Launching …") self-expires after a TTL
    /// instead of lingering for the whole session (the writers don't timestamp it).
    status_shown: Option<String>,
    status_shown_at: std::time::Instant,
}

impl PanelApp {
    fn new(state: PanelState) -> Self {
        // Eagerly start the control-socket worker at launch so the panel begins
        // fetching live host status (connected clients, streaming state) from the
        // very first frame — instead of only once a tab that touches `control::`
        // happens to render. The worker connects + self-polls GetStatus ~1 Hz and
        // reconnects on its own, so a host that starts later is picked up too.
        let _ = crate::control::handle();
        // Bring up the rest of the GUI suite as the control center loads: make
        // sure the tray daemon is running, and start the host if none is up
        // (idempotent — never restarts a live stream). Linux-only (user units).
        #[cfg(unix)]
        {
            crate::control::ensure_tray_running();
            crate::control::ensure_host_running();
        }
        Self {
            state,
            applied_theme: None,
            applied_scale: None,
            applied_lang: None,
            status_shown: None,
            status_shown_at: std::time::Instant::now(),
        }
    }

    /// Push the selected theme + font scale to egui whenever the user changes
    /// them. We set the visuals for *both* egui themes (Dark + Light) so a
    /// system light/dark flip can't revert our explicit look.
    fn apply_appearance(&mut self, ctx: &egui::Context) {
        let theme = self.state.runtime.theme;
        if self.applied_theme != Some(theme) {
            crate::theme::set_active(theme);
            let v = crate::theme::visuals(theme);
            ctx.set_visuals_of(egui::Theme::Dark, v.clone());
            ctx.set_visuals_of(egui::Theme::Light, v);
            self.applied_theme = Some(theme);
        }
        let scale = self.state.runtime.font_scale;
        if self.applied_scale != Some(scale) {
            ctx.set_zoom_factor(scale.factor());
            self.applied_scale = Some(scale);
        }
        // Language: set the global locale so this frame's `t!`/`tr` calls render
        // in the chosen language. Cheap; only re-applied on change.
        if self.applied_lang.as_deref() != Some(self.state.runtime.language.as_str()) {
            crate::i18n::set_locale(&self.state.runtime.language);
            self.applied_lang = Some(self.state.runtime.language.clone());
        }
    }

    /// Render one sidebar nav item; returns its Response so the caller can wire
    /// keyboard focus. Clicking selects the tab (and takes focus).
    fn nav_item(&mut self, ui: &mut egui::Ui, t: Tab) -> egui::Response {
        let selected = self.state.tab == t;
        let label = format!("{}  {}", t.icon(), t.label());
        let resp = ui
            .add_sized(
                [ui.available_width(), 34.0],
                egui::SelectableLabel::new(selected, egui::RichText::new(label).size(15.0)),
            )
            .describe(tab_describe(t));
        if resp.clicked() {
            self.state.tab = t;
            resp.request_focus();
        }
        resp
    }

    /// The left sidebar: a flat main-nav group at the top + Settings pinned to
    /// the bottom. Nothing is nested, so each section is one arrow-key step away.
    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("pharos_sidebar")
            .resizable(false)
            .default_width(186.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);

                // Main nav (top), in order. Collect responses for key handling.
                let mut items: Vec<(Tab, egui::Response)> = Vec::new();
                for t in Tab::main_nav() {
                    let r = self.nav_item(ui, t);
                    items.push((t, r));
                    ui.add_space(2.0);
                }

                // Pin Settings to the bottom: pad the gap, divider, then Settings.
                let pad = (ui.available_height() - 48.0).max(8.0);
                ui.add_space(pad);
                ui.separator();
                ui.add_space(2.0);
                let settings = self.nav_item(ui, Tab::Settings);
                items.push((Tab::Settings, settings));

                self.handle_nav_keys(ui, &items);
            });
    }

    /// Keyboard navigation across the flat nav list. Acts only while one of the
    /// nav items holds focus, so arrows never hijack sliders / text fields.
    /// Up/Down move *and* select (panels load instantly); Right or Tab hands
    /// focus to the panel-content landmark so you can interact with the panel.
    fn handle_nav_keys(&mut self, ui: &egui::Ui, items: &[(Tab, egui::Response)]) {
        let Some(fid) = ui.memory(|m| m.focused()) else {
            return;
        };
        let Some(idx) = items.iter().position(|(_, r)| r.id == fid) else {
            return;
        };
        let n = items.len();
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
            let j = (idx + 1) % n;
            self.state.tab = items[j].0;
            items[j].1.request_focus();
        } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
            let j = (idx + n - 1) % n;
            self.state.tab = items[j].0;
            items[j].1.request_focus();
        } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight))
            || ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
        {
            ui.memory_mut(|m| m.request_focus(content_anchor_id()));
        }
    }

    /// The top bar: the Pharos wordmark + this host's name + a status line.
    fn topbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("pharos_top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(crate::i18n::tr("app.wordmark"));
                ui.label(
                    egui::RichText::new(crate::i18n::tr("app.subtitle"))
                        .small()
                        .weak(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Host name + a compact MAC/peer summary on the right.
                    let host = &self.state.host;
                    let hostname = if host.hostname.is_empty() {
                        crate::i18n::tr("app.unknown_host")
                    } else {
                        host.hostname.clone()
                    };
                    let peers = self.state.peers.len();
                    let peer_word = if peers == 1 {
                        crate::i18n::tr("app.peer_one")
                    } else {
                        crate::i18n::tr("app.peer_many")
                    };
                    ui.label(
                        egui::RichText::new(format!("{hostname} · {peers} {peer_word}"))
                            .small()
                            .weak(),
                    )
                    .describe(if host.macs.is_empty() {
                        crate::i18n::tr("app.topbar.host_desc_no_nics")
                    } else {
                        format!(
                            "{} {}",
                            crate::i18n::tr("app.topbar.host_desc_nics_prefix"),
                            host.macs.join(", ")
                        )
                    });
                });
            });
            ui.add_space(6.0);
        });
    }

    /// The bottom bar: last error (left), status note + Save indicator (right).
    fn bottombar(&mut self, ctx: &egui::Context) {
        // Expire a stale transient status. The status writers set `state.status`
        // without a timestamp and most have no completion event (e.g.
        // "Launching <app>…"), so without this a one-shot note lingers for the
        // whole session — the exact "still launching an hour later" bug. We watch
        // the note at this single render chokepoint: a CHANGED note (re)starts the
        // TTL; once a note has been shown past the TTL we clear it. The 1s idle
        // repaint (see `update`) guarantees this runs with no input, so the bar
        // actually clears on its own. `last_error` is intentionally NOT expired —
        // errors stick until the user acts.
        const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(6);
        match &self.state.status {
            Some(note) if self.status_shown.as_deref() != Some(note.as_str()) => {
                self.status_shown = Some(note.clone());
                self.status_shown_at = std::time::Instant::now();
            }
            None => self.status_shown = None,
            _ => {
                if self.status_shown_at.elapsed() >= STATUS_TTL {
                    self.state.status = None;
                    self.status_shown = None;
                }
            }
        }
        egui::TopBottomPanel::bottom("pharos_bottom").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if let Some(err) = &self.state.last_error {
                    ui.colored_label(crate::theme::error(), format!("⚠ {err}"));
                } else if let Some(note) = &self.state.status {
                    ui.colored_label(crate::theme::info(), note.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.state.any_dirty() {
                        let unsaved = crate::i18n::tr("app.unsaved");
                        crate::widgets::status_badge(ui, (unsaved.as_str(), crate::theme::warn()));
                        // Global save: flush every dirty section at once, instead of
                        // hunting for each tab's own Save.
                        if ui
                            .button(format!(
                                "{}  {}",
                                egui_phosphor::regular::FLOPPY_DISK,
                                crate::i18n::tr("app.save_all")
                            ))
                            .describe(crate::i18n::tr("app.save_all_desc"))
                            .clicked()
                        {
                            match crate::data::save_all_dirty(&mut self.state) {
                                Ok(_) => {
                                    self.state.last_error = None;
                                    self.state.status = Some(crate::i18n::tr("app.saved_all"));
                                }
                                Err(e) => {
                                    self.state.last_error = Some(format!(
                                        "{} {e}",
                                        crate::i18n::tr("app.save_all_error")
                                    ));
                                }
                            }
                        }
                    } else {
                        ui.label(
                            egui::RichText::new(crate::i18n::tr("app.saved"))
                                .small()
                                .weak(),
                        );
                    }
                });
            });
            ui.add_space(6.0);
        });
    }
}

impl eframe::App for PanelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_appearance(ctx);
        // Pump the control socket once per frame, independent of which tab is
        // showing, so EVERY tab sees fresh host/peer liveness, device geometry,
        // and link state — not just Overview. The Compositor palette colours its
        // chips by `peer.liveness`, so it was stale until you opened Overview.
        let _ = crate::control::handle().drain_events();
        tabs::overview::apply_live_liveness(&mut self.state);
        // Re-assert per-device input gates to the host on (re)connect — every
        // frame, on ANY tab, so a device disabled in the panel stays disabled
        // after an unattended host restart (which clears the host's runtime gate).
        tabs::compositor::reassert_input_on_connect(ctx, &self.state);
        self.topbar(ctx);
        self.bottombar(ctx);
        self.sidebar(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            // Focusable landmark: the sidebar hands focus here on Tab / Right
            // arrow, then the next Tab moves into the panel's first control.
            let anchor = ui.interact(
                egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(1.0, 1.0)),
                content_anchor_id(),
                egui::Sense::focusable_noninteractive(),
            );
            let _ =
                anchor.describe_named(crate::i18n::tr("a11y.enter_panel"), self.state.tab.label());

            egui::ScrollArea::vertical().show(ui, |ui| match self.state.tab {
                Tab::Overview => tabs::overview::ui(ui, &mut self.state),
                Tab::Compositor => tabs::compositor::ui(ui, &mut self.state),
                Tab::Video => tabs::video::ui(ui, &mut self.state),
                Tab::Audio => tabs::audio::ui(ui, &mut self.state),
                Tab::Input => tabs::input::ui(ui, &mut self.state),
                Tab::Controllers => tabs::controllers::ui(ui, &mut self.state),
                Tab::Apps => tabs::apps::ui(ui, &mut self.state),
                Tab::Settings => tabs::settings::ui(ui, &mut self.state),
            });
        });

        // Save-on-quit: persist dirty edits when the window close is requested. We
        // do NOT cancel the close (no risk of trapping the window) — just flush
        // best-effort on the way out, so unsaved edits aren't silently lost.
        if ctx.input(|i| i.viewport().close_requested()) && self.state.any_dirty() {
            let _ = crate::data::save_all_dirty(&mut self.state);
        }

        // Tick once a second so tabs that later show live telemetry refresh
        // without the user moving the mouse (winit otherwise idles).
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

/// Single-instance guard: bind a per-user socket. If another panel already holds
/// it, connecting pings that instance (which raises its window) and we return
/// `None` so the caller exits — so the tray's left-click, the auto-start, and any
/// direct launch can never stack up duplicate windows. A stale socket from a
/// crashed instance is reclaimed.
#[cfg(unix)]
fn single_instance() -> Option<std::os::unix::net::UnixListener> {
    use std::os::unix::net::{UnixListener, UnixStream};
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("pharos-panel.sock");
    match UnixListener::bind(&path) {
        Ok(l) => Some(l),
        Err(_) => {
            if UnixStream::connect(&path).is_ok() {
                None // a live instance answered (and raised itself) → we exit
            } else {
                let _ = std::fs::remove_file(&path); // stale socket → reclaim
                UnixListener::bind(&path).ok()
            }
        }
    }
}

/// Launch the control panel. Blocks until the window is closed.
pub fn run() -> anyhow::Result<()> {
    // Single-instance: if a panel is already up, the connect above just raised it
    // — don't open a second window.
    #[cfg(unix)]
    let instance_listener = match single_instance() {
        Some(l) => l,
        None => {
            tracing::info!("pharos-panel already running — raised the existing window");
            return Ok(());
        }
    };

    // Renderer tuning for a snappy window. The eframe default present mode is
    // AutoVsync (FIFO), which stalls window resize on NVIDIA + Wayland (confirmed
    // on an RTX 4070 / KDE: FIFO laggy, no-vsync ultra-smooth). AutoNoVsync makes
    // wgpu pick a supported no-vsync mode (Immediate -> Mailbox -> Fifo), so it's
    // snappy AND cross-platform-safe (egui_wgpu configures the surface with this
    // value directly, so a concrete unsupported mode would error — Auto* never
    // does). `desired_maximum_frame_latency = 1` trims presentation latency. We
    // also allow the GL backend so `WGPU_BACKEND=gl` is A/B-testable, and expose
    // `PHAROS_PRESENT_MODE` (vsync|novsync|mailbox|immediate|fifo|fifo_relaxed) to
    // override per run.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration {
        present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
        desired_maximum_frame_latency: Some(1),
        supported_backends: eframe::wgpu::Backends::PRIMARY | eframe::wgpu::Backends::GL,
        ..Default::default()
    };
    let default_pm = wgpu_options.present_mode;
    if let Ok(pm) = std::env::var("PHAROS_PRESENT_MODE") {
        use eframe::wgpu::PresentMode;
        wgpu_options.present_mode = match pm.to_ascii_lowercase().as_str() {
            "novsync" | "auto_novsync" => PresentMode::AutoNoVsync,
            "vsync" | "auto_vsync" => PresentMode::AutoVsync,
            "mailbox" => PresentMode::Mailbox,
            "immediate" => PresentMode::Immediate,
            "fifo" => PresentMode::Fifo,
            "fifo_relaxed" => PresentMode::FifoRelaxed,
            other => {
                tracing::warn!("unknown PHAROS_PRESENT_MODE={other}; keeping default");
                default_pm
            }
        };
    }

    tracing::info!("wgpu present mode: {:?}", wgpu_options.present_mode);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pharos Control")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 480.0]),
        wgpu_options,
        ..Default::default()
    };

    eframe::run_native(
        "Pharos Control",
        options,
        Box::new(move |cc| {
            // Diagnostic: which GPU/backend did wgpu actually select? A `Cpu`
            // device type means software (lavapipe) — the real performance cliff.
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                tracing::info!(
                    "wgpu adapter: {} | backend {:?} | type {:?} | driver {} {}",
                    info.name,
                    info.backend,
                    info.device_type,
                    info.driver,
                    info.driver_info,
                );
            }
            let state = PanelState::load();
            // Bundle our fonts (icons + symbol fallbacks) before anything draws.
            crate::fonts::install(&cc.egui_ctx);
            // Apply the saved locale + theme immediately so the first frame is
            // already correct before `update()` runs.
            crate::i18n::set_locale(&state.runtime.language);
            crate::theme::set_active(state.runtime.theme);
            let v = crate::theme::visuals(state.runtime.theme);
            cc.egui_ctx.set_visuals_of(egui::Theme::Dark, v.clone());
            cc.egui_ctx.set_visuals_of(egui::Theme::Light, v);
            cc.egui_ctx
                .set_zoom_factor(state.runtime.font_scale.factor());
            // Raise this window whenever another launch pings the single-instance
            // socket. egui's viewport command is thread-safe, so the listener
            // thread can call it directly.
            #[cfg(unix)]
            {
                let ctx = cc.egui_ctx.clone();
                std::thread::spawn(move || {
                    for conn in instance_listener.incoming() {
                        if conn.is_ok() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                            ctx.request_repaint();
                        }
                    }
                });
            }
            Ok(Box::new(PanelApp::new(state)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
