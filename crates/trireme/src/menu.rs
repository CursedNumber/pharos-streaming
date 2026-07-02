//! The Trireme server selector — the first screen.
//!
//! UX (desktop):
//!   * Tile per remembered server (friendly name, host:port, paired badge).
//!   * Checkbox in the tile's corner toggles multi-select.
//!   * Clicking the tile body connects immediately to just that server.
//!   * The bottom Connect button appears as soon as ≥1 tile is checked and
//!     opens the player with every checked tile as a source.
//!   * "+ Add server" opens a modal with host/port/alias fields.
//!   * Right-click a tile for Edit / Delete.
//!
//! When the user picks something, [`run`] returns [`MenuOutcome::Connect`]
//! with the chosen entries; `main` then hands them to `play::run`. Window
//! close → [`MenuOutcome::Quit`].
//!
//! On mobile (Android/iOS) the same logical model — tap = connect, long
//! press = enter multi-select — will be implemented in the Compose / SwiftUI
//! shells, not in this file. The desktop and mobile front-ends share the
//! server registry ([`crate::servers`]).

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use core_crypto::Identity;
use eframe::egui;

use crate::discover::{self, Browser, DiscoverEvent, Discovered};
use crate::pair::{run_pair, PairOutcome};
use crate::servers::{self, Registry, ServerEntry};

#[derive(Debug, Clone)]
pub enum MenuOutcome {
    /// User picked one or more servers to connect to.
    Connect(Vec<ServerEntry>),
    /// User closed the window without picking anything.
    Quit,
}

/// Show the menu. Blocks until the user closes the window or hits Connect.
/// `identity` is the local Trireme keypair, used by the Pair action.
pub fn run(initial: Registry, identity: Arc<Identity>) -> Result<(Registry, MenuOutcome)> {
    let shared = Arc::new(Mutex::new(SharedState {
        registry: initial,
        outcome: MenuOutcome::Quit,
    }));
    let app_shared = shared.clone();
    let app_identity = identity.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Trireme")
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([520.0, 380.0]),
        ..Default::default()
    };

    // No explicit `set_visuals` — eframe's `follow_system_theme` (default
    // true) detects the OS light/dark setting and applies the matching
    // visuals. Honoring it means egui colors track the system, which keeps
    // parity with the platform UIs we'll add for Android/iOS.
    eframe::run_native(
        "Trireme",
        options,
        Box::new(move |_cc| Ok(Box::new(MenuApp::new(app_shared, app_identity)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;

    let s = Arc::try_unwrap(shared)
        .map_err(|_| anyhow::anyhow!("menu state still referenced after exit"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("menu state poisoned"))?;
    Ok((s.registry, s.outcome))
}

/// Mutable state the menu both renders from and writes through. We hand the
/// outcome back to `main` via the `Arc` so eframe's owning closure can
/// publish without a channel.
struct SharedState {
    registry: Registry,
    outcome: MenuOutcome,
}

struct MenuApp {
    shared: Arc<Mutex<SharedState>>,
    identity: Arc<Identity>,
    selected: std::collections::HashSet<String>,
    add_dialog: Option<AddDialog>,
    edit_dialog: Option<EditDialog>,
    delete_confirm: Option<String>,
    pair_dialog: Option<PairDialog>,
    last_error: Option<String>,
    dirty: bool,
    /// Background mDNS browser; `None` if the daemon failed to start.
    browser: Option<Browser>,
    /// Currently-visible mDNS results, keyed by instance name. Cleared on
    /// `ServiceRemoved`.
    discovered: HashMap<String, Discovered>,
}

/// One in-flight Pair attempt. The handshake runs on a worker thread so the
/// UI stays responsive; the result lands in `rx` and is polled each frame.
struct PairDialog {
    server_id: String,
    server_label: String,
    addr: std::net::SocketAddr,
    pin: String,
    state: PairUiState,
    rx: Option<mpsc::Receiver<Result<PairOutcome, String>>>,
}

enum PairUiState {
    Idle,
    InProgress,
    Done(PairOutcome),
    Failed(String),
}

struct AddDialog {
    host: String,
    port: String,
    alias: String,
}

impl Default for AddDialog {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: core_proto::ports::STREAM.to_string(),
            alias: String::new(),
        }
    }
}

struct EditDialog {
    id: String,
    host: String,
    port: String,
    alias: String,
}

impl MenuApp {
    fn new(shared: Arc<Mutex<SharedState>>, identity: Arc<Identity>) -> Self {
        Self {
            shared,
            identity,
            selected: Default::default(),
            add_dialog: None,
            edit_dialog: None,
            delete_confirm: None,
            pair_dialog: None,
            last_error: None,
            dirty: false,
            browser: discover::start(),
            discovered: HashMap::new(),
        }
    }

    /// Pull any new mDNS events into `discovered`. Called once per frame
    /// from `update`.
    fn drain_discoveries(&mut self) {
        let Some(browser) = self.browser.as_ref() else {
            return;
        };
        loop {
            match browser.rx.try_recv() {
                Ok(DiscoverEvent::Found(d)) => {
                    self.discovered.insert(d.instance.clone(), d);
                }
                Ok(DiscoverEvent::Removed { instance }) => {
                    self.discovered.remove(&instance);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.browser = None;
                    break;
                }
            }
        }
    }

    /// A discovered host whose host:port isn't already in the registry,
    /// promoted to a `ServerEntry`. Used both to render the tile and to
    /// persist on first action (connect/pair).
    fn discovered_as_entry(d: &Discovered) -> ServerEntry {
        // `d.port` is the *pairing* port (where the service is advertised); the
        // entry's `port` is the *stream* port the host published (or the
        // well-known default on older hosts that don't advertise it).
        let stream_port = d.stream_port.unwrap_or(core_proto::ports::STREAM);
        let mut e = ServerEntry::new(d.host.clone(), stream_port, d.alias.clone());
        e.host_pubkey = d.pubkey_hex.clone();
        e.pair_port = Some(d.port);
        e
    }

    /// Persist a discovered host into the registry. Returns the entry's id.
    fn promote_discovered(&mut self, d: &Discovered) -> String {
        let alias = d.alias.clone();
        let stream_port = d.stream_port.unwrap_or(core_proto::ports::STREAM);
        let mut shared = self.shared.lock().unwrap();
        let id = shared.registry.upsert(d.host.clone(), stream_port, alias);
        if let Some(s) = shared.registry.find_mut(&id) {
            if s.host_pubkey.is_none() {
                s.host_pubkey = d.pubkey_hex.clone();
            }
            if s.pair_port.is_none() {
                s.pair_port = Some(d.port);
            }
        }
        drop(shared);
        self.dirty = true;
        id
    }

    fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let registry = self.shared.lock().unwrap().registry.clone();
        match servers::save(&registry) {
            Ok(()) => self.dirty = false,
            Err(e) => self.last_error = Some(format!("save: {e}")),
        }
    }

    fn finish_with_connect(&self, ctx: &egui::Context, entries: Vec<ServerEntry>) {
        if entries.is_empty() {
            return;
        }
        self.shared.lock().unwrap().outcome = MenuOutcome::Connect(entries);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for MenuApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull any pending mDNS resolutions before drawing.
        self.drain_discoveries();
        if self.browser.is_some() && !self.discovered.is_empty() {
            // While discovery is active, keep ticking so newly arrived
            // services show up promptly (winit otherwise idles).
            ctx.request_repaint_after(std::time::Duration::from_millis(400));
        }

        // -- Top bar -----------------------------------------------------
        egui::TopBottomPanel::top("trireme_top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Trireme");
                ui.label(egui::RichText::new("server selector").small().weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("+ Add server").clicked() {
                        self.add_dialog = Some(AddDialog::default());
                    }
                });
            });
            ui.add_space(6.0);
        });

        // -- Bottom bar (Connect button shows when selection non-empty) --
        egui::TopBottomPanel::bottom("trireme_bottom").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if let Some(err) = &self.last_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let n = self.selected.len();
                    if n > 0 {
                        let label = if n == 1 {
                            "Connect".to_string()
                        } else {
                            format!("Connect ({n})")
                        };
                        let resp = ui.add(
                            egui::Button::new(egui::RichText::new(label).strong().size(16.0))
                                .min_size(egui::vec2(140.0, 32.0)),
                        );
                        if resp.clicked() {
                            let registry = self.shared.lock().unwrap().registry.clone();
                            let entries: Vec<ServerEntry> = self
                                .selected
                                .iter()
                                .filter_map(|id| registry.find(id).cloned())
                                .collect();
                            self.finish_with_connect(ctx, entries);
                        }
                        ui.add_space(8.0);
                        if ui.button("Clear selection").clicked() {
                            self.selected.clear();
                        }
                    }
                });
            });
            ui.add_space(8.0);
        });

        // -- Tile grid ---------------------------------------------------
        egui::CentralPanel::default().show(ctx, |ui| {
            let registry = self.shared.lock().unwrap().registry.clone();
            // Discovered hosts whose host:stream-port aren't already saved.
            // (`d.port` is the pair port; compare against the resolved stream
            // port, the same value `discovered_as_entry` would persist.)
            let new_discoveries: Vec<Discovered> = self
                .discovered
                .values()
                .filter(|d| {
                    let stream_port = d.stream_port.unwrap_or(core_proto::ports::STREAM);
                    !registry
                        .servers
                        .iter()
                        .any(|s| s.host == d.host && s.port == stream_port)
                })
                .cloned()
                .collect();
            if registry.servers.is_empty() && new_discoveries.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(
                        egui::RichText::new("No servers remembered yet.")
                            .size(18.0)
                            .weak(),
                    );
                    ui.add_space(8.0);
                    let hint = if self.browser.is_some() {
                        "Click \"+ Add server\", or start `pharos-compositor pair` \
                         on the LAN and a tile will appear here."
                    } else {
                        "Click \"+ Add server\" above to get started."
                    };
                    ui.label(egui::RichText::new(hint).weak());
                });
                return;
            }
            self.draw_tile_grid(ui, &registry, &new_discoveries, ctx);
        });

        // -- Modals ------------------------------------------------------
        self.draw_add_modal(ctx);
        self.draw_edit_modal(ctx);
        self.draw_delete_confirm(ctx);
        self.draw_pair_modal(ctx);

        // -- Persist any pending changes (debounced via `dirty` flag) ----
        self.save_if_dirty();
    }
}

impl MenuApp {
    /// Pick the column count so tiles stay roughly tile-sized regardless of
    /// window width — 1 col under ~520px, 2 under ~780px, 3 under ~1040px,
    /// 4 above.
    fn columns_for_width(width: f32) -> usize {
        if width < 520.0 {
            1
        } else if width < 780.0 {
            2
        } else if width < 1040.0 {
            3
        } else {
            4
        }
    }

    fn draw_tile_grid(
        &mut self,
        ui: &mut egui::Ui,
        registry: &Registry,
        new_discoveries: &[Discovered],
        ctx: &egui::Context,
    ) {
        let available = ui.available_width();
        let cols = Self::columns_for_width(available);
        let spacing = 12.0;
        let tile_w = ((available - spacing * (cols as f32 - 1.0)) / cols as f32).max(160.0);
        let tile_h = 140.0;

        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut connect_one: Option<ServerEntry> = None;
            let mut to_edit: Option<EditDialog> = None;
            let mut to_delete: Option<String> = None;
            let mut to_pair: Option<ServerEntry> = None;
            let mut promote_then_connect: Option<Discovered> = None;
            let mut promote_then_pair: Option<Discovered> = None;

            let mut col = 0usize;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);
                for server in &registry.servers {
                    let resp = self.draw_tile(ui, server, tile_w, tile_h, false);
                    if resp.clicked_tile {
                        connect_one = Some(server.clone());
                    }
                    if resp.context_edit {
                        to_edit = Some(EditDialog {
                            id: server.id.clone(),
                            host: server.host.clone(),
                            port: server.port.to_string(),
                            alias: server.alias.clone().unwrap_or_default(),
                        });
                    }
                    if resp.context_delete {
                        to_delete = Some(server.id.clone());
                    }
                    if resp.context_pair {
                        to_pair = Some(server.clone());
                    }
                    col += 1;
                    if col >= cols {
                        ui.end_row();
                        col = 0;
                    }
                }
                // Discovered tiles: render as ServerEntry-shaped temporaries
                // with a "Discovered" badge from `draw_tile`'s `discovered`
                // flag. Acting on one promotes it into the registry first.
                for d in new_discoveries {
                    let temp = Self::discovered_as_entry(d);
                    let resp = self.draw_tile(ui, &temp, tile_w, tile_h, true);
                    if resp.clicked_tile {
                        promote_then_connect = Some(d.clone());
                    }
                    if resp.context_pair {
                        promote_then_pair = Some(d.clone());
                    }
                    // Edit/Delete on a not-yet-persisted entry is a no-op.
                    col += 1;
                    if col >= cols {
                        ui.end_row();
                        col = 0;
                    }
                }
            });

            if let Some(d) = promote_then_connect {
                let id = self.promote_discovered(&d);
                if let Some(s) = self.shared.lock().unwrap().registry.find(&id).cloned() {
                    connect_one = Some(s);
                }
            }
            if let Some(d) = promote_then_pair {
                let id = self.promote_discovered(&d);
                if let Some(s) = self.shared.lock().unwrap().registry.find(&id).cloned() {
                    to_pair = Some(s);
                }
            }

            if let Some(s) = connect_one {
                self.finish_with_connect(ctx, vec![s]);
            }
            if let Some(e) = to_edit {
                self.edit_dialog = Some(e);
            }
            if let Some(id) = to_delete {
                self.delete_confirm = Some(id);
            }
            if let Some(s) = to_pair {
                use std::net::ToSocketAddrs;
                let pair_port = s.pair_port_or_default();
                match (s.host.as_str(), pair_port).to_socket_addrs() {
                    Ok(mut iter) => match iter.next() {
                        Some(addr) => {
                            self.pair_dialog = Some(PairDialog {
                                server_id: s.id.clone(),
                                server_label: s.friendly_name(),
                                addr,
                                pin: String::new(),
                                state: PairUiState::Idle,
                                rx: None,
                            });
                        }
                        None => {
                            self.last_error =
                                Some(format!("no addresses for {}:{pair_port}", s.host));
                        }
                    },
                    Err(e) => {
                        self.last_error = Some(format!("resolve {}: {e}", s.host));
                    }
                }
            }
        });
    }

    fn draw_tile(
        &mut self,
        ui: &mut egui::Ui,
        server: &ServerEntry,
        w: f32,
        h: f32,
        discovered: bool,
    ) -> TileResponse {
        let mut out = TileResponse::default();
        let id_src = if discovered {
            format!("tile_disc_{}_{}", server.host, server.port)
        } else {
            format!("tile_{}", server.id)
        };
        let frame = egui::Frame::group(ui.style())
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(12.0))
            .fill(ui.visuals().widgets.inactive.bg_fill);

        let (rect, body_resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());

        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );
        frame.show(&mut content, |ui| {
            ui.set_min_size(egui::vec2(w - 24.0, h - 24.0));
            // Top row: friendly name + checkbox far right.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(server.friendly_name())
                        .strong()
                        .size(16.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if discovered {
                        // Not in the registry yet — selection doesn't apply.
                        ui.label(
                            egui::RichText::new("●")
                                .color(egui::Color32::from_rgb(80, 150, 230))
                                .small(),
                        )
                        .on_hover_text("Discovered via mDNS");
                    } else {
                        let mut checked = self.selected.contains(&server.id);
                        if ui
                            .add(egui::Checkbox::without_text(&mut checked))
                            .on_hover_text("Multi-select for grid view")
                            .changed()
                        {
                            if checked {
                                self.selected.insert(server.id.clone());
                            } else {
                                self.selected.remove(&server.id);
                            }
                        }
                    }
                });
            });
            ui.add_space(4.0);
            // Address line.
            ui.label(
                egui::RichText::new(server.address_label())
                    .monospace()
                    .small()
                    .weak(),
            );
            ui.add_space(8.0);
            // Status badges — raw colors so they pop on either theme. The
            // green/amber pair is legible on the light-mode default panel
            // (no white-on-white) and the dark-mode one.
            ui.horizontal(|ui| {
                let (label, color) = if server.paired {
                    ("Paired", egui::Color32::from_rgb(60, 140, 75))
                } else {
                    ("Unpaired", egui::Color32::from_rgb(170, 120, 40))
                };
                let wash = ui.visuals().faint_bg_color;
                ui.label(
                    egui::RichText::new(label)
                        .color(color)
                        .small()
                        .background_color(wash),
                );
                if discovered {
                    ui.label(
                        egui::RichText::new("Discovered")
                            .color(egui::Color32::from_rgb(80, 150, 230))
                            .small()
                            .background_color(wash),
                    );
                }
                if let Some(p) = &server.pace {
                    ui.label(egui::RichText::new(p.as_str()).small().weak());
                }
            });
            ui.add_space(6.0);
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                if let Some(t) = &server.last_connected {
                    ui.label(
                        egui::RichText::new(format!("last connected: {}", t))
                            .small()
                            .weak(),
                    );
                }
            });
        });

        // Whole-tile click → connect to this one only. The checkbox already
        // handles its own click via egui's hit-test, so checking it does NOT
        // also fire a connect.
        let click_id = ui.make_persistent_id(&id_src);
        let interact = ui.interact(rect, click_id, egui::Sense::click());
        if interact.clicked() {
            out.clicked_tile = true;
        }
        interact.context_menu(|ui| {
            let pair_label = if server.paired {
                "Re-pair…"
            } else {
                "Pair…"
            };
            if ui.button(pair_label).clicked() {
                out.context_pair = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Edit").clicked() {
                out.context_edit = true;
                ui.close_menu();
            }
            if ui.button("Delete").clicked() {
                out.context_delete = true;
                ui.close_menu();
            }
            ui.separator();
            let mut checked = self.selected.contains(&server.id);
            if ui
                .checkbox(&mut checked, "Select for multi-connect")
                .changed()
            {
                if checked {
                    self.selected.insert(server.id.clone());
                } else {
                    self.selected.remove(&server.id);
                }
            }
        });
        // Suppress the dead body response — we drew via `content` above.
        let _ = body_resp;
        out
    }

    fn draw_add_modal(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut commit = false;
        if let Some(dialog) = self.add_dialog.as_mut() {
            egui::Window::new("Add server")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.horizontal(|ui| {
                        ui.label("Host");
                        ui.text_edit_singleline(&mut dialog.host);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Port");
                        ui.text_edit_singleline(&mut dialog.port);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Alias");
                        ui.text_edit_singleline(&mut dialog.alias);
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let ok = ui.add_enabled(
                                !dialog.host.trim().is_empty()
                                    && dialog.port.trim().parse::<u16>().is_ok(),
                                egui::Button::new("Add"),
                            );
                            if ok.clicked() {
                                commit = true;
                            }
                        });
                    });
                });
        }
        if commit {
            if let Some(d) = self.add_dialog.take() {
                if let Ok(port) = d.port.trim().parse::<u16>() {
                    let alias = (!d.alias.trim().is_empty()).then(|| d.alias.trim().to_string());
                    let host = d.host.trim().to_string();
                    let mut shared = self.shared.lock().unwrap();
                    let id = shared.registry.upsert(host.clone(), port, alias);
                    drop(shared);
                    self.dirty = true;
                    self.kick_reverse_lookup(id, host);
                }
            }
        } else if close {
            self.add_dialog = None;
        }
    }

    /// Spawn a one-off worker that resolves the host's reverse DNS and
    /// writes the result into `hostname_cache`. Cheap on success, harmless
    /// on failure — the menu just keeps showing whatever it already had.
    fn kick_reverse_lookup(&self, server_id: String, host: String) {
        // Only IPs benefit; saves a thread spawn for the hostname case.
        if host.parse::<std::net::IpAddr>().is_err() {
            return;
        }
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("trireme-revdns".into())
            .spawn(move || {
                let Some(name) = servers::reverse_lookup(&host) else {
                    return;
                };
                let mut st = shared.lock().unwrap();
                if let Some(entry) = st.registry.find_mut(&server_id) {
                    if entry.hostname_cache.as_deref() != Some(&name) {
                        entry.hostname_cache = Some(name);
                        let snapshot = st.registry.clone();
                        drop(st);
                        // Save from the worker thread directly; the UI's
                        // `dirty` flag batches changes from the UI side only.
                        let _ = servers::save(&snapshot);
                    }
                }
            })
            .ok();
    }

    fn draw_edit_modal(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut commit = false;
        if let Some(dialog) = self.edit_dialog.as_mut() {
            egui::Window::new("Edit server")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.horizontal(|ui| {
                        ui.label("Host");
                        ui.text_edit_singleline(&mut dialog.host);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Port");
                        ui.text_edit_singleline(&mut dialog.port);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Alias");
                        ui.text_edit_singleline(&mut dialog.alias);
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let ok = ui.add_enabled(
                                !dialog.host.trim().is_empty()
                                    && dialog.port.trim().parse::<u16>().is_ok(),
                                egui::Button::new("Save"),
                            );
                            if ok.clicked() {
                                commit = true;
                            }
                        });
                    });
                });
        }
        if commit {
            if let Some(d) = self.edit_dialog.take() {
                if let Ok(port) = d.port.trim().parse::<u16>() {
                    let alias = (!d.alias.trim().is_empty()).then(|| d.alias.trim().to_string());
                    let mut shared = self.shared.lock().unwrap();
                    // Remove the old (id may change because id is derived
                    // from host:port), keep paired + cache + last_connected.
                    let preserved = shared
                        .registry
                        .find(&d.id)
                        .map(|s| (s.paired, s.hostname_cache.clone(), s.last_connected.clone()));
                    shared.registry.remove(&d.id);
                    let new_id = shared
                        .registry
                        .upsert(d.host.trim().to_string(), port, alias);
                    if let Some((paired, cache, last)) = preserved {
                        if let Some(s) = shared.registry.find_mut(&new_id) {
                            s.paired = paired;
                            s.hostname_cache = cache;
                            s.last_connected = last;
                        }
                    }
                    self.dirty = true;
                    // Move selection if it referred to the old id.
                    if self.selected.remove(&d.id) {
                        self.selected.insert(new_id);
                    }
                }
            }
        } else if close {
            self.edit_dialog = None;
        }
    }

    fn draw_delete_confirm(&mut self, ctx: &egui::Context) {
        let mut confirm = false;
        let mut cancel = false;
        if let Some(id) = self.delete_confirm.clone() {
            let name = self
                .shared
                .lock()
                .unwrap()
                .registry
                .find(&id)
                .map(|s| s.friendly_name())
                .unwrap_or_default();
            egui::Window::new("Delete server?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Forget '{}'?", name));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button(
                                    egui::RichText::new("Delete").color(egui::Color32::LIGHT_RED),
                                )
                                .clicked()
                            {
                                confirm = true;
                            }
                        });
                    });
                });
            if confirm {
                let id = self.delete_confirm.take().unwrap();
                self.shared.lock().unwrap().registry.remove(&id);
                self.selected.remove(&id);
                self.dirty = true;
            } else if cancel {
                self.delete_confirm = None;
            }
        }
    }
}

impl MenuApp {
    fn draw_pair_modal(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.pair_dialog.as_mut() else {
            return;
        };

        // Poll the worker thread for a finished handshake.
        if let Some(rx) = &dialog.rx {
            match rx.try_recv() {
                Ok(Ok(outcome)) => {
                    dialog.state = PairUiState::Done(outcome.clone());
                    dialog.rx = None;
                    let host_pubkey_hex = outcome.host_pubkey_hex.clone();
                    let host_alias = outcome.host_alias.clone();
                    let server_id = dialog.server_id.clone();
                    let pair_port = dialog.addr.port();
                    let mut shared = self.shared.lock().unwrap();
                    if let Some(s) = shared.registry.find_mut(&server_id) {
                        s.paired = true;
                        s.host_pubkey = Some(host_pubkey_hex);
                        s.pair_port = Some(pair_port);
                        if s.alias.is_none() {
                            if let Some(a) = host_alias {
                                s.alias = Some(a);
                            }
                        }
                    }
                    self.dirty = true;
                }
                Ok(Err(e)) => {
                    dialog.state = PairUiState::Failed(e);
                    dialog.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Keep redrawing every 60 ms so the spinner moves.
                    ctx.request_repaint_after(std::time::Duration::from_millis(60));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    dialog.state = PairUiState::Failed("pairing thread died".into());
                    dialog.rx = None;
                }
            }
        }

        let mut close = false;
        let mut start = false;
        egui::Window::new(format!("Pair with {}", dialog.server_label))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(format!(
                    "Pairing port: {}  (set on the Pharos host with \
                     `pharos-compositor pair`)",
                    dialog.addr
                ));
                ui.add_space(6.0);
                match &dialog.state {
                    PairUiState::Idle => {
                        ui.horizontal(|ui| {
                            ui.label("PIN");
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut dialog.pin)
                                    .hint_text("6 digits, shown by the host")
                                    .desired_width(160.0),
                            );
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                start = true;
                            }
                        });
                    }
                    PairUiState::InProgress => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("running SPAKE2 handshake…");
                        });
                    }
                    PairUiState::Done(outcome) => {
                        ui.colored_label(egui::Color32::from_rgb(60, 140, 75), "Paired ✓");
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "host pubkey: {}",
                                fingerprint(&outcome.host_pubkey_hex)
                            ))
                            .small()
                            .monospace()
                            .weak(),
                        );
                    }
                    PairUiState::Failed(e) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("Failed: {e}"));
                    }
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let close_label = matches!(dialog.state, PairUiState::Done(_))
                        .then_some("Close")
                        .unwrap_or("Cancel");
                    if ui.button(close_label).clicked() {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_start = matches!(dialog.state, PairUiState::Idle)
                            && dialog.pin.trim().len() == 6
                            && dialog.pin.trim().chars().all(|c| c.is_ascii_digit());
                        if ui
                            .add_enabled(can_start, egui::Button::new("Start"))
                            .clicked()
                        {
                            start = true;
                        }
                    });
                });
            });

        if start {
            // Spawn the worker. The handshake runs on its own runtime
            // inside `pair::run_pair`, so we just need a thread.
            let (tx, rx) = mpsc::channel();
            let addr = dialog.addr;
            let pin = dialog.pin.trim().to_string();
            let identity = self.identity.clone();
            let alias = std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty());
            std::thread::spawn(move || {
                let res = run_pair(addr, &pin, &identity, alias).map_err(|e| format!("{e:#}"));
                let _ = tx.send(res);
            });
            dialog.rx = Some(rx);
            dialog.state = PairUiState::InProgress;
        }
        if close {
            self.pair_dialog = None;
        }
    }
}

/// Short fingerprint of a hex-encoded 32-byte key — first 8 + last 8 chars.
fn fingerprint(hex: &str) -> String {
    if hex.len() <= 20 {
        return hex.to_string();
    }
    format!("{}…{}", &hex[..8], &hex[hex.len() - 8..])
}

#[derive(Default)]
struct TileResponse {
    clicked_tile: bool,
    context_edit: bool,
    context_delete: bool,
    context_pair: bool,
}
