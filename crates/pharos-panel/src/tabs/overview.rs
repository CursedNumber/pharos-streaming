//! Overview tab — one host card up top, the Trireme fleet below.
//!
//! Two blocks:
//!   1. The host card: this box's hostname, advertised NIC MACs, a live
//!      descriptor of what the compositor is doing (read over the control
//!      socket), and the Launch / Relaunch control — everything about *this*
//!      machine in one place.
//!   2. "Paired clients": a "Pair a new device" entry on top, then a responsive
//!      grid of peer cards (one per paired client) — each with a liveness badge,
//!      a short pubkey fingerprint, and the data that matters at a glance
//!      (decoder/codec, resolution, fps, address). Unknown fields render as a
//!      subtle "—" with an explanatory tooltip rather than a faked value.
//!
//! Per-tab UI state (the search filter) lives in egui's temp memory — the panel
//! contract gives us only `&mut PanelState`, which has no filter field, so we
//! keep the filter in `ctx.data` keyed by a stable id. It survives across frames
//! but resets on relaunch, which is the right lifetime for a search box.

use std::time::Duration;

use control_ipc::PairOutcome;
use egui::{Color32, RichText};
use egui_phosphor::regular as ph;

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::{Liveness, PanelState, PeerInfo};

/// What the host-control row decided this frame (applied to `PanelState` after
/// the card closure releases its borrow). Keeps the button logic borrow-free.
enum HostAction {
    /// No host running → spawn one.
    Launch,
    /// Host running → ask it to restart in place.
    Relaunch,
    /// Host running → stop it (and the USB/IP bridge).
    Stop,
}

/// Stable id for the search-filter string we stash in egui temp memory.
fn filter_id() -> egui::Id {
    egui::Id::new("overview_peer_filter")
}

pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    // Host/peer liveness + device geometry + link state are pumped once per
    // frame globally (in `App::update`, before tab dispatch), so they're already
    // fresh here and on every other tab — nothing tab-local to refresh.
    ui.heading(tr("overview.heading"))
        .describe(tr("overview.subtitle_desc"));
    ui.add_space(8.0);

    // 1. The host: identity (name + NICs), a live descriptor of what it's doing,
    //    and the launch / relaunch control — everything about *this* machine in
    //    one card.
    host_card(ui, state);
    ui.add_space(16.0);

    // 2. Paired clients: "Pair a new device" on top, then the fleet below.
    paired_clients(ui, state);
}

// ---------------------------------------------------------------------------
// Paired-clients section: a "Pair a new device" entry on top, the fleet below
// ---------------------------------------------------------------------------

fn paired_clients(ui: &mut egui::Ui, state: &mut PanelState) {
    // Section header: title on the left, a Refresh affordance on the right.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{}  {}",
                ph::GAME_CONTROLLER,
                tr("overview.fleet.heading")
            ))
            .strong()
            .size(15.0),
        )
        .describe(tr("overview.fleet.heading_desc"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(format!(
                    "{}  {}",
                    ph::ARROWS_CLOCKWISE,
                    tr("overview.fleet.refresh")
                ))
                .describe(tr("overview.fleet.refresh_desc"))
                .clicked()
            {
                reload_peers(state);
            }
        });
    });
    ui.separator();
    ui.add_space(8.0);

    // Top element: pair a new device (its own inline, state-driven card).
    pairing_card(ui, state);
    ui.add_space(12.0);

    let total = state.peers.len();
    if total == 0 {
        empty_state(ui);
        return;
    }

    // Optional filter — only worth the chrome once the fleet is large.
    let mut filter: String = ui
        .data_mut(|d| d.get_temp::<String>(filter_id()))
        .unwrap_or_default();
    if total > 3 {
        ui.horizontal(|ui| {
            ui.label(tr("overview.fleet.filter_label"));
            let resp = ui
                .add(
                    egui::TextEdit::singleline(&mut filter)
                        .hint_text(tr("overview.fleet.filter_hint"))
                        .desired_width(220.0),
                )
                .describe(tr("overview.fleet.filter_desc"));
            if resp.changed() {
                ui.data_mut(|d| d.insert_temp(filter_id(), filter.clone()));
            }
            if !filter.is_empty()
                && ui
                    .button(ph::X)
                    .describe(tr("overview.fleet.clear_desc"))
                    .clicked()
            {
                filter.clear();
                ui.data_mut(|d| d.insert_temp(filter_id(), String::new()));
            }
        });
        ui.add_space(8.0);
    }

    // Filtered set.
    let needle = filter.trim().to_lowercase();
    let matches = |p: &PeerInfo| -> bool {
        if needle.is_empty() {
            return true;
        }
        let alias = p.alias.as_deref().unwrap_or("").to_lowercase();
        alias.contains(&needle) || p.pubkey.to_lowercase().contains(&needle)
    };
    let visible: Vec<&PeerInfo> = state.peers.iter().filter(|p| matches(p)).collect();

    if visible.is_empty() {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} \"{}\".",
                    tr("overview.fleet.no_match_prefix"),
                    filter.trim()
                ))
                .weak(),
            );
        });
        return;
    }

    peer_grid(ui, &visible);
}

/// Re-read the on-disk fleet + host state from disk and report the new peer
/// count in the status bar. Reloads `peers.toml` **and** the host identity
/// (hostname / NIC MACs) + `config.toml` (MQTT / network), so a Refresh isn't
/// silently peers-only — those used to stay stale until an app relaunch.
fn reload_peers(state: &mut PanelState) {
    state.peers = crate::data::load_peers();
    state.host = crate::data::host_info();
    state.host_config = crate::data::load_host_config();
    state.last_error = None;
    let n = state.peers.len();
    state.status = Some(if n == 1 {
        tr("overview.status.reloaded_one")
    } else {
        format!(
            "{} {} {}",
            tr("overview.status.reloaded_many_prefix"),
            n,
            tr("overview.status.reloaded_many_suffix")
        )
    });
}

// ---------------------------------------------------------------------------
// Host status header
// ---------------------------------------------------------------------------

/// The top card: this host's identity (name + advertised NICs), a live
/// descriptor of what it's doing (read over the control socket), and the
/// Launch / Relaunch control. Merges the old host header and live-status banner
/// so everything about *this* machine lives in one place.
fn host_card(ui: &mut egui::Ui, state: &mut PanelState) {
    let handle = crate::control::handle();
    let connected = handle.connected();
    let status = handle.status();

    let mut action = None;
    crate::widgets::card(ui, "overview_host_card", |ui| {
        // Compact rhythm — this is a status header, not a content card.
        ui.spacing_mut().item_spacing.y = 2.0;

        // --- Header: server icon + hostname + "This host" badge ------------
        ui.horizontal(|ui| {
            ui.label(RichText::new(ph::COMPUTER_TOWER).size(18.0))
                .describe_named(tr("overview.host.icon_name"), tr("overview.host.icon_desc"));
            ui.add_space(4.0);
            let hostname = if state.host.hostname.trim().is_empty() {
                tr("overview.host.unknown_hostname")
            } else {
                state.host.hostname.clone()
            };
            ui.label(RichText::new(hostname).strong().size(16.0))
                .describe(tr("overview.host.hostname_desc"));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let badge_label = tr("overview.host.badge");
                let badge_resp = ui.scope(|ui| {
                    crate::widgets::status_badge(ui, (badge_label.as_str(), crate::theme::info()));
                });
                badge_resp
                    .response
                    .describe_named(badge_label, tr("overview.host.badge_desc"));
            });
        });

        ui.add_space(4.0);

        // --- Descriptor: a live read of what the host is doing -------------
        live_descriptor(ui, connected, status.as_ref());

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);

        // --- Advertised MACs (for Wake-on-LAN + NIC binding) ---------------
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr("overview.host.nics_label")).small().weak())
                .describe(tr("overview.host.nics_label_desc"));
            ui.add_space(4.0);
            if state.host.macs.is_empty() {
                ui.label(RichText::new("—").monospace().small().weak())
                    .describe(tr("overview.host.nics_none_desc"));
            } else {
                let wash = ui.visuals().faint_bg_color;
                for mac in &state.host.macs {
                    ui.label(
                        RichText::new(mac)
                            .monospace()
                            .small()
                            .background_color(wash),
                    )
                    .describe(tr("overview.host.nic_mac_desc"));
                }
            }
        });

        // Bind-MAC pin status, if one is configured.
        let bind = state.host_config.bind_mac.trim();
        if !bind.is_empty() {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tr("overview.host.bound_label"))
                        .small()
                        .weak(),
                )
                .describe(tr("overview.host.bound_desc"));
                ui.add_space(4.0);
                ui.label(RichText::new(bind).monospace().small())
                    .describe(tr("overview.host.bound_desc"));
            });
        }

        ui.add_space(8.0);

        // --- Launch / Relaunch control (the "status row" action) -----------
        // The outer `horizontal` bounds the row to one line's height — a bare
        // `with_layout(right_to_left, …)` as a direct child of the card's
        // vertical layout would otherwise claim ALL remaining height and
        // vertically-centre into it, stretching the card to fill the panel.
        // Inside the inner right_to_left the confirm prompt adds its widgets in
        // reverse so they read left-to-right.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                action = host_control_button(ui);
            });
        });
    });

    apply_host_action(state, action);
}

/// One honest line describing what the host is doing right now, read live from
/// the control socket: offline → the reason; connecting → a placeholder; running
/// → a streaming/idle badge, the connected-client count, and the encode pipeline.
fn live_descriptor(ui: &mut egui::Ui, connected: bool, status: Option<&control_ipc::HostStatus>) {
    ui.horizontal_wrapped(|ui| {
        if !connected {
            crate::widgets::status_badge(
                ui,
                (tr("overview.live.offline").as_str(), crate::theme::warn()),
            );
            ui.label(
                RichText::new(crate::control::unavailable_reason())
                    .small()
                    .weak(),
            )
            .describe(tr("overview.live.offline_desc"));
        } else if let Some(s) = status {
            // Badge: GPU health takes visual priority over streaming/idle so a
            // degraded/faulted host is never hidden behind a "Streaming" badge.
            let (badge, color, reason): (String, egui::Color32, Option<String>) =
                match &s.gpu_health {
                    control_ipc::GpuHealth::Faulted { reason, last_xid } => (
                        tr("overview.gpu.faulted"),
                        crate::theme::error(),
                        Some(match last_xid {
                            Some(x) => format!("{reason} (Xid {x})"),
                            None => reason.clone(),
                        }),
                    ),
                    control_ipc::GpuHealth::Degraded { reason, throttle } => (
                        tr("overview.gpu.degraded"),
                        crate::theme::warn(),
                        Some(throttle.clone().unwrap_or_else(|| reason.clone())),
                    ),
                    control_ipc::GpuHealth::Healthy if s.streaming => (
                        tr("overview.live.streaming"),
                        crate::theme::streaming(),
                        None,
                    ),
                    control_ipc::GpuHealth::Healthy => {
                        (tr("overview.live.idle"), crate::theme::online(), None)
                    }
                };
            crate::widgets::status_badge(ui, (badge.as_str(), color));
            if let Some(reason) = reason {
                ui.label(RichText::new(format!("· {reason}")).small().weak())
                    .describe(tr("overview.gpu.reason_desc"));
            }

            // Connected-client count.
            let connected_clients = s.clients.iter().filter(|c| c.connected).count();
            let clients_word = if connected_clients == 1 {
                tr("overview.live.client_one")
            } else {
                tr("overview.live.client_many")
            };
            ui.label(
                RichText::new(format!("· {connected_clients} {clients_word}"))
                    .small()
                    .weak(),
            )
            .describe(tr("overview.live.clients_desc"));

            // The live pipeline. Connected client → its encoder + wire-rate fps;
            // idle → the *configured* codec, so the line is faithful even before
            // a client connects.
            if let Some(c) = s.clients.iter().find(|c| c.connected) {
                let enc = if c.encoder.trim().is_empty() {
                    c.codec.clone()
                } else {
                    c.encoder.clone()
                };
                ui.label(
                    RichText::new(format!("· {enc} · {} fps · Opus", c.fps))
                        .small()
                        .weak()
                        .background_color(ui.visuals().faint_bg_color),
                )
                .describe(tr("overview.live.encoder_desc"));
            } else {
                let codec = s.video_codec.clone().unwrap_or_else(|| "HEVC".into());
                let res = s
                    .outputs
                    .first()
                    .map(|o| format!(" · {}×{}", o.w, o.h))
                    .unwrap_or_default();
                ui.label(
                    RichText::new(format!("· NVENC {codec} · Opus{res}"))
                        .small()
                        .weak()
                        .background_color(ui.visuals().faint_bg_color),
                )
                .describe(tr("overview.live.configured_desc"));
            }
        } else {
            // Connected but no status snapshot yet (handshake in flight).
            crate::widgets::status_badge(
                ui,
                (
                    tr("overview.live.connecting").as_str(),
                    crate::theme::info(),
                ),
            );
            ui.label(
                RichText::new(tr("overview.live.connecting_desc"))
                    .small()
                    .weak(),
            )
            .describe(tr("overview.live.connecting_desc"));
        }
    });
}

/// The Launch / Relaunch control, rendered right-aligned inside the live status
/// banner (the "status row"). Returns the chosen [`HostAction`] (applied by the
/// caller after the card's borrow releases), or `None`. Transient UI state (the
/// confirm-while-streaming flag, the launch debounce) lives in egui temp memory
/// — the panel contract gives us no `PanelState` fields for it.
///
/// It is invoked inside a `right_to_left` layout, so multi-widget states (the
/// confirm prompt) add their widgets in reverse so they *read* left-to-right.
fn host_control_button(ui: &mut egui::Ui) -> Option<HostAction> {
    let handle = crate::control::handle();
    let connected = handle.connected();
    // "Streaming" for the confirm guard = streaming flag OR any connected client,
    // so we never silently kill a live session without asking.
    let streaming = handle
        .status()
        .map(|s| s.streaming || s.clients.iter().any(|c| c.connected))
        .unwrap_or(false);
    let supports_v2 = crate::control::host_supports_v2();

    let confirm_id = egui::Id::new("overview_confirm_relaunch");
    // The confirm prompt only makes sense for a connected, streaming host. Clear
    // a stale flag so a disconnect/reconnect — or a stream that has since stopped
    // — can't strand a spurious "confirm relaunch?" prompt on a later frame.
    if !connected || !streaming {
        ui.data_mut(|d| d.insert_temp(confirm_id, false));
    }
    let confirming = ui.data(|d| d.get_temp::<bool>(confirm_id)).unwrap_or(false);
    // A status-only host (the Windows host) can't be relaunched/controlled.
    let read_only = crate::control::host_is_read_only();

    // Launch debounce: remember when Launch was clicked; while the spawned host
    // is still binding (connected == false) and within the window, disable the
    // button so an impatient second click can't spawn a *second* compositor that
    // collides on the control socket / QUIC port. Relaunch gets the same
    // treatment so the button can read "Relaunching…" while the host restarts in
    // place (the control socket briefly drops, so without this the button would
    // flip back to "Launch host" mid-restart).
    let launch_at_id = egui::Id::new("overview_launch_at");
    let relaunch_at_id = egui::Id::new("overview_relaunch_at");
    let now = ui.input(|i| i.time);
    let launching = ui
        .data(|d| d.get_temp::<f64>(launch_at_id))
        .is_some_and(|t| now - t < 6.0);
    let relaunching = ui
        .data(|d| d.get_temp::<f64>(relaunch_at_id))
        .is_some_and(|t| now - t < 6.0);

    // Keep the transient label live so it clears itself once the window passes,
    // even with no further interaction.
    if launching || relaunching {
        ui.ctx().request_repaint_after(Duration::from_millis(400));
    }

    let mut action = None;
    if relaunching {
        // Host restarting in place — show a settled "Relaunching…" rather than
        // letting the brief socket drop flip the button to "Launch host".
        ui.add_enabled(
            false,
            egui::Button::new(format!(
                "{}  {}",
                ph::ARROWS_CLOCKWISE,
                tr("overview.host.relaunching_btn")
            )),
        )
        .describe(tr("overview.host.relaunching"));
    } else if !connected {
        // No host: offer to launch one (disabled while a launch is settling).
        let label = if launching {
            tr("overview.host.launching_btn")
        } else {
            tr("overview.host.launch")
        };
        if ui
            .add_enabled(
                !launching,
                egui::Button::new(format!("{}  {}", ph::PLAY, label)),
            )
            .describe(tr("overview.host.launch_desc"))
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp(launch_at_id, now));
            action = Some(HostAction::Launch);
        }
    } else if !supports_v2 {
        // Host predates the v2 control surface: be honest rather than send a
        // Relaunch it would silently drop. (Pairing is gated the same way.)
        ui.label(
            RichText::new(format!("{}  {}", ph::WARNING, tr("overview.host.too_old")))
                .small()
                .color(crate::theme::warn()),
        )
        .describe(tr("overview.host.too_old_hint"));
    } else if read_only {
        // Status-only host (the Windows host's serve_status): it answers status
        // but no control requests, so be honest rather than show a button that
        // would no-op. (Pairing below is gated the same way.)
        ui.label(
            RichText::new(format!("{}  {}", ph::INFO, tr("control.read_only")))
                .small()
                .color(crate::theme::warn()),
        )
        .describe(tr("control.read_only"));
    } else if confirming {
        // Confirm prompt. We're in a right_to_left layout, so add Cancel, then
        // Relaunch, then the warning — they then *read* "⚠ … [Relaunch] [Cancel]".
        if ui.button(tr("overview.host.cancel_btn")).clicked() {
            ui.data_mut(|d| d.insert_temp(confirm_id, false));
        }
        if ui
            .button(tr("overview.host.relaunch_confirm_yes"))
            .describe(tr("overview.host.relaunch_confirm_yes_desc"))
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp(confirm_id, false));
            ui.data_mut(|d| d.insert_temp(relaunch_at_id, now));
            action = Some(HostAction::Relaunch);
        }
        ui.label(
            RichText::new(format!(
                "{}  {}",
                ph::WARNING,
                tr("overview.host.relaunch_confirm")
            ))
            .small()
            .color(crate::theme::warn()),
        );
    } else {
        // Host up: relaunch (arms a confirm if a stream is live), with a Stop
        // button beside it.
        if ui
            .button(format!(
                "{}  {}",
                ph::ARROWS_CLOCKWISE,
                tr("overview.host.relaunch")
            ))
            .describe(tr("overview.host.relaunch_desc"))
            .clicked()
        {
            if streaming {
                ui.data_mut(|d| d.insert_temp(confirm_id, true));
            } else {
                ui.data_mut(|d| d.insert_temp(relaunch_at_id, now));
                action = Some(HostAction::Relaunch);
            }
        }
        if ui
            .button(format!("{}  {}", ph::STOP, tr("overview.host.stop")))
            .describe(tr("overview.host.stop_desc"))
            .clicked()
        {
            action = Some(HostAction::Stop);
        }
    }
    action
}

/// Apply a [`HostAction`] chosen by [`host_control_button`] to `PanelState`.
fn apply_host_action(state: &mut PanelState, action: Option<HostAction>) {
    match action {
        Some(HostAction::Launch) => match crate::control::launch_host() {
            Ok(()) => {
                state.last_error = None;
                state.status = Some(tr("overview.host.launching"));
            }
            Err(e) => {
                state.last_error = Some(format!("{} {e}", tr("overview.host.launch_error")));
            }
        },
        Some(HostAction::Relaunch) => {
            crate::control::relaunch_host();
            state.status = Some(tr("overview.host.relaunching"));
        }
        Some(HostAction::Stop) => {
            crate::control::stop_host();
            state.status = Some(tr("overview.host.stopped"));
        }
        None => {}
    }
}

// ---------------------------------------------------------------------------
// Pairing card (inline, expanding)
// ---------------------------------------------------------------------------

/// The inline pairing card — the prominent entry point for enrolling a Trireme
/// device, driven by the host's live `HostStatus.pairing`. Idle → a "Pair a new
/// device" button; active → the rotating PIN, the listen address, a waiting
/// spinner, and Cancel; with the latest attempt outcome shown either way. A
/// freshly-paired device triggers a one-shot reload of `peers.toml` so it lands
/// in the fleet grid below.
fn pairing_card(ui: &mut egui::Ui, state: &mut PanelState) {
    let handle = crate::control::handle();
    let connected = handle.connected();
    let pairing = handle.status().and_then(|s| s.pairing);
    let active = pairing.as_ref().is_some_and(|p| p.active);

    crate::widgets::card(ui, "overview_pairing_card", |ui| {
        if !connected {
            ui.horizontal_wrapped(|ui| {
                crate::widgets::status_badge(
                    ui,
                    (
                        tr("overview.pairing.needs_host").as_str(),
                        crate::theme::warn(),
                    ),
                );
                ui.label(
                    RichText::new(tr("overview.pairing.needs_host_desc"))
                        .small()
                        .weak(),
                )
                .describe(tr("overview.pairing.needs_host_desc"));
            });
            return;
        }

        if crate::control::host_is_read_only() {
            // Status-only host (the Windows host): pairing is done via the host's
            // own CLI, not the panel, so don't offer a button that would no-op.
            ui.horizontal_wrapped(|ui| {
                crate::widgets::status_badge(
                    ui,
                    (tr("control.read_only_short").as_str(), crate::theme::warn()),
                );
                ui.label(RichText::new(tr("control.read_only")).small().weak());
            });
            return;
        }

        if !crate::control::host_supports_v2() {
            // Host predates the pairing control surface — say so honestly rather
            // than offer a button whose request the host would silently drop.
            ui.horizontal_wrapped(|ui| {
                crate::widgets::status_badge(
                    ui,
                    (tr("overview.host.too_old").as_str(), crate::theme::warn()),
                );
                ui.label(
                    RichText::new(tr("overview.host.too_old_hint"))
                        .small()
                        .weak(),
                );
            });
            return;
        }

        if active {
            // Open round: show the PIN to type on the device + a waiting state.
            if let Some(p) = pairing.as_ref() {
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(tr("overview.pairing.enter_pin"))
                            .small()
                            .weak(),
                    );
                    ui.add_space(2.0);
                    let pin = p.pin.clone().unwrap_or_else(|| "······".into());
                    ui.label(
                        RichText::new(pin_spaced(&pin))
                            .monospace()
                            .size(38.0)
                            .strong(),
                    )
                    .describe(tr("overview.pairing.pin_desc"));
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            tr("overview.pairing.listening_on"),
                            p.addr
                        ))
                        .small()
                        .weak(),
                    );
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(4.0);
                    ui.label(RichText::new(tr("overview.pairing.waiting")).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(format!("{}  {}", ph::X, tr("overview.pairing.cancel")))
                            .describe(tr("overview.pairing.cancel_desc"))
                            .clicked()
                        {
                            crate::control::cancel_pairing();
                        }
                    });
                });

                if let Some(last) = p.last.as_ref() {
                    ui.add_space(6.0);
                    pairing_outcome_line(ui, last);
                }
            }
        } else {
            // Idle: the whole card body is the click target — no separate button,
            // no misleading "+". A trailing arrow signals it's clickable.
            let prompt = ui.scope(|ui| {
                // Labels are selectable by default — that would catch the click as
                // a text-selection (I-beam cursor) instead of letting it fall
                // through to the card's interact. Turn it off so the whole card is
                // one click target.
                ui.style_mut().interaction.selectable_labels = false;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(tr("overview.pairing.idle_title"))
                                .strong()
                                .size(15.0),
                        );
                        ui.label(
                            RichText::new(tr("overview.pairing.idle_desc"))
                                .small()
                                .weak(),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(ph::ARROW_LINE_RIGHT).size(20.0).weak());
                    });
                });
            });
            let resp = prompt
                .response
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .describe_named(
                    tr("overview.pairing.start"),
                    tr("overview.pairing.start_desc"),
                );
            if resp.clicked() {
                crate::control::start_pairing(None);
            }
            if let Some(last) = pairing.as_ref().and_then(|p| p.last.as_ref()) {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                pairing_outcome_line(ui, last);
            }
        }
    });

    // A device that just finished pairing → fold it into the fleet grid (once).
    maybe_reload_on_paired(ui, state, pairing.as_ref());

    // Keep the PIN / spinner / outcome lively while a round is open (the worker
    // polls status ~1 Hz; a faster repaint picks transitions up promptly).
    if active {
        ui.ctx().request_repaint_after(Duration::from_millis(300));
    }
}

/// One line describing the most recent pairing attempt's outcome.
fn pairing_outcome_line(ui: &mut egui::Ui, last: &PairOutcome) {
    match last {
        PairOutcome::Attempt { peer } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("{} {peer}", tr("overview.pairing.connecting")))
                        .small()
                        .weak(),
                );
            });
        }
        PairOutcome::Paired { alias, pubkey } => {
            let name = alias
                .clone()
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| crate::model::fingerprint(pubkey));
            ui.horizontal_wrapped(|ui| {
                crate::widgets::status_badge(
                    ui,
                    (
                        tr("overview.pairing.paired_badge").as_str(),
                        crate::theme::online(),
                    ),
                );
                ui.label(RichText::new(format!(
                    "{} {name}",
                    tr("overview.pairing.paired_with")
                )));
            });
        }
        PairOutcome::Failed { reason } => {
            ui.horizontal_wrapped(|ui| {
                crate::widgets::status_badge(
                    ui,
                    (
                        tr("overview.pairing.failed_badge").as_str(),
                        crate::theme::error(),
                    ),
                );
                ui.label(RichText::new(reason).small().weak());
            });
        }
    }
}

/// Group a six-digit PIN as `004 217` for readability; pass anything else
/// (placeholder dots, odd lengths) straight through.
fn pin_spaced(pin: &str) -> String {
    if pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} {}", &pin[..3], &pin[3..])
    } else {
        pin.to_string()
    }
}

/// When the host reports a *new* successful pairing, reload `peers.toml` once so
/// the device appears in the fleet grid. The last-folded pubkey is remembered in
/// egui temp memory (the panel contract gives us no `PanelState` field for it),
/// so we don't reload every frame the outcome lingers.
fn maybe_reload_on_paired(
    ui: &egui::Ui,
    state: &mut PanelState,
    pairing: Option<&control_ipc::PairingState>,
) {
    let Some(PairOutcome::Paired { pubkey, .. }) = pairing.and_then(|p| p.last.as_ref()) else {
        return;
    };
    let id = egui::Id::new("overview_last_paired_pubkey");
    let seen: Option<String> = ui.data(|d| d.get_temp(id));
    if seen.as_deref() == Some(pubkey.as_str()) {
        return; // already folded this one in
    }
    ui.data_mut(|d| d.insert_temp(id, pubkey.clone()));
    state.peers = crate::data::load_peers();
    state.last_error = None;
    state.status = Some(tr("overview.pairing.peer_added"));
}

// ---------------------------------------------------------------------------
// Live telemetry → peer cards
// ---------------------------------------------------------------------------

/// Reflect the host's live client list onto each known peer's `liveness` badge
/// and telemetry. With a live status in hand we have ground truth, so a peer
/// whose pubkey matches a connected client becomes Streaming/Online and every
/// *other* paired peer becomes Offline (the host would list it if it were
/// connected) — no more blanket "Unknown" while a client is plainly streaming.
/// Honest no-op (for liveness) when the control socket is absent: with no live
/// signal we can't know, so peers stay Unknown (set by `load_peers`).
///
/// Resolution + fps are **device-reported geometry**: recorded into the
/// persisted `device_geoms` map whenever a client connects (via
/// [`record_connected_geom`]) and backfilled onto every peer from that map, so a
/// device shows its last negotiated resolution/Hz whether it's connected or
/// offline. The Compositor tab reads the same map to size each device's tile.
pub(crate) fn apply_live_liveness(state: &mut PanelState) {
    let handle = crate::control::handle();
    let connected = handle.connected();
    let status = handle.status();

    // 1) Record the connected client's geometry (persist only on change), so the
    //    device map captures the most recent negotiation before we backfill.
    if connected {
        if let Some(s) = status.as_ref() {
            record_connected_geom(state, s);
        }
    }

    // 2) Backfill resolution/fps from the persisted device map for EVERY peer,
    //    so offline devices keep showing their last reported geometry.
    {
        let geoms = &state.device_geoms;
        for peer in &mut state.peers {
            if let Some(g) = geoms.get(peer.pubkey.as_str()) {
                peer.resolution = Some(format!("{}×{}", g.w, g.h));
                if g.fps > 0 {
                    peer.fps = Some(g.fps);
                }
            }
        }
    }

    // 3) Liveness + encoder/codec/addr from the live client list.
    if !connected {
        return;
    }
    let Some(status) = status else {
        return;
    };
    for peer in &mut state.peers {
        if let Some(client) = status
            .clients
            .iter()
            .filter(|c| c.connected)
            .find(|c| c.pubkey.as_deref() == Some(peer.pubkey.as_str()))
        {
            // A connected client means the host is encoding to it, so
            // host-wide `streaming` is true → Streaming. (Online is reserved
            // for a connected-but-idle client, which this host doesn't model.)
            peer.liveness = if status.streaming {
                Liveness::Streaming
            } else {
                Liveness::Online
            };
            if peer.addr.is_none() {
                peer.addr = client.addr.clone();
            }
            if peer.encoder.is_none() && !client.encoder.trim().is_empty() {
                peer.encoder = Some(client.encoder.clone());
            }
            // Codec = what this client is decoding (the closest honest proxy for
            // its decoder, which the host can't observe directly).
            if peer.codec.is_none() && !client.codec.trim().is_empty() {
                peer.codec = Some(client.codec.clone());
            }
        } else {
            // Host is up and this peer is not among its connected clients — we
            // have ground truth that it isn't connected to this host right now.
            peer.liveness = Liveness::Offline;
        }
    }
}

/// Record the currently-connected client's stream geometry (resolution from the
/// primary output, fps from the client's measured wire rate) into the persisted
/// per-pubkey device map. Writes `panel.toml` only when the value actually
/// changes (a new device or a renegotiated resolution), so it isn't rewritten
/// every frame. Anonymous clients (no pubkey) and zero-size/zero-fps snapshots
/// are skipped.
fn record_connected_geom(state: &mut PanelState, status: &control_ipc::HostStatus) {
    let Some(client) = status.clients.iter().find(|c| c.connected) else {
        return;
    };
    let Some(pubkey) = client.pubkey.as_deref() else {
        return;
    };
    let Some(out) = status
        .outputs
        .iter()
        .find(|o| o.connected)
        .or_else(|| status.outputs.first())
    else {
        return;
    };
    if client.fps == 0 || out.w == 0 || out.h == 0 {
        return;
    }
    let geom = crate::model::DeviceGeom {
        w: out.w,
        h: out.h,
        fps: client.fps,
    };
    if state.device_geoms.get(pubkey) != Some(&geom) {
        state.device_geoms.insert(pubkey.to_string(), geom);
        let _ = crate::data::save_device_geoms(&state.device_geoms);
    }
}

// ---------------------------------------------------------------------------
// Empty state
// ---------------------------------------------------------------------------

fn empty_state(ui: &mut egui::Ui) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(tr("overview.empty.title")).size(18.0).weak())
            .describe(tr("overview.empty.title_desc"));
        ui.add_space(8.0);
        ui.label(RichText::new(tr("overview.empty.prompt")).weak());
        ui.add_space(4.0);
        ui.label(
            RichText::new(tr("overview.empty.command"))
                .monospace()
                .background_color(ui.visuals().faint_bg_color),
        )
        .describe(tr("overview.empty.command_desc"));
        ui.add_space(8.0);
        ui.label(RichText::new(tr("overview.empty.hint")).small().weak());
    });
}

// ---------------------------------------------------------------------------
// Peer grid
// ---------------------------------------------------------------------------

/// Pick a column count for the fleet grid, **with hysteresis**.
///
/// The grid lives in a vertical `ScrollArea`. Near a column threshold, dropping
/// a column makes the content taller → a scrollbar appears → `available_width`
/// shrinks by the scrollbar's width → back below the threshold → fewer columns →
/// … and at one window width it flips 1↔2 columns every frame (the jitter you
/// see while resizing). A plain `width < 520` test has no memory, so it can't
/// damp that feedback loop.
///
/// Fix: remember the last column count and require the width to move a full
/// dead-band (wider than any scrollbar) *past* a threshold before changing it.
/// So entering 2 columns needs ≥ 520, but you don't fall back to 1 until the
/// width drops below 520 − `HYST`. The scrollbar's ~14 px wobble fits inside the
/// band, so the layout holds steady.
fn columns_for_width(ui: &egui::Ui, width: f32) -> usize {
    // Minimum width to *enter* 2, 3, 4 columns respectively.
    const TH: [f32; 3] = [520.0, 800.0, 1080.0];
    // Dead-band, comfortably wider than a scrollbar (so it can't oscillate).
    const HYST: f32 = 28.0;

    let id = egui::Id::new("overview_grid_cols");
    let prev: usize = ui.data(|d| d.get_temp(id)).unwrap_or(1).max(1);

    let mut cols = 1usize;
    for (i, &t) in TH.iter().enumerate() {
        let want = i + 2; // columns unlocked by clearing threshold i
                          // Already at/above this count → hold it until the width falls HYST below
                          // the threshold; otherwise require the full threshold to climb into it.
        let enter = if prev >= want { t - HYST } else { t };
        if width >= enter {
            cols = want;
        }
    }
    ui.data_mut(|d| d.insert_temp(id, cols));
    cols
}

fn peer_grid(ui: &mut egui::Ui, peers: &[&PeerInfo]) {
    let available = ui.available_width();
    let cols = columns_for_width(ui, available);
    // Tight gutter so the fleet reads as a dense board, not scattered tiles.
    let spacing = 8.0;
    // Shave a pixel of slack so `cols` cards + gaps are *strictly* narrower than
    // the row — otherwise a sub-pixel rounding tip can push the last card just
    // past the edge.
    let card_w = ((available - spacing * (cols as f32 - 1.0) - 1.0) / cols as f32).max(180.0);
    // Every card is allocated the EXACT same rectangle (see `peer_card`), so a
    // row has nothing to stagger — the diagonal "walk" only appears when cells
    // differ in height. Tall enough to hold the name + fingerprint + four rows
    // with headroom at any font scale (zoom scales this and the text together,
    // so a value that fits at 1× fits at every scale).
    let card_h = 144.0;

    // Place exactly `cols` cards per row in our OWN `horizontal` rows. We do not
    // use `horizontal_wrapped`: its width-based auto-wrap fights this explicit
    // column count, so right at a column boundary one frame wraps the last card
    // and the next doesn't — the 1↔2 column flicker while resizing. `peer_card`
    // paints into an explicit top-down child, so a plain horizontal row no
    // longer flips the card's own contents sideways.
    let mut idx = 0usize;
    for row in peers.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            for peer in row {
                peer_card(ui, peer, idx, card_w, card_h);
                idx += 1;
            }
        });
        ui.add_space(spacing);
    }
}

fn peer_card(ui: &mut egui::Ui, peer: &PeerInfo, idx: usize, w: f32, h: f32) {
    // Reserve an EXACT w×h cell, then paint into it with an explicit top-down
    // layout. The shared `widgets::card` both inherits the parent's layout
    // (inside a horizontal grid that flipped the card's contents sideways) and
    // lets the frame size to content (so the wrap layout's row tracking drifted
    // into the staggered/overflowing look). This mirrors the Trireme client
    // menu's proven tile layout.
    let frame = egui::Frame::group(ui.style())
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(12.0))
        .fill(ui.visuals().widgets.inactive.weak_bg_fill);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    frame.show(&mut content, |ui| {
        ui.set_min_size(egui::vec2(w - 24.0, h - 24.0));
        ui.push_id(("overview_peer", idx, peer.pubkey.as_str()), |ui| {
            // Tighter inter-row rhythm so the card reads as a compact tile.
            ui.spacing_mut().item_spacing.y = 3.0;

            // Title row: display name + liveness badge on the right.
            ui.horizontal(|ui| {
                ui.label(RichText::new(peer.display_name()).strong().size(15.0))
                    .describe(tr("overview.peer.name_desc"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let badge_resp = ui.scope(|ui| {
                        crate::widgets::status_badge(ui, peer.liveness);
                    });
                    badge_resp
                        .response
                        .describe_named(peer.liveness.label(), liveness_tip(peer.liveness));
                });
            });

            // Full pubkey fingerprint (monospace) with the full key on hover.
            ui.label(
                RichText::new(crate::model::fingerprint(&peer.pubkey))
                    .monospace()
                    .small()
                    .weak(),
            )
            .describe(format!(
                "{}\n{}\n{}",
                tr("overview.peer.fingerprint_desc"),
                tr("overview.peer.fingerprint_full_label"),
                peer.pubkey
            ));

            ui.add_space(5.0);

            // The data that actually matters at a glance — what the client
            // decodes, at what resolution and frame rate, and where it connects
            // from. Honest "—" while not live.
            kv_row(
                ui,
                &tr("overview.peer.row.decoder"),
                peer.codec.as_deref(),
                &tr("overview.peer.row.decoder_unknown_desc"),
                false,
            );
            kv_row(
                ui,
                &tr("overview.peer.row.resolution"),
                peer.resolution.as_deref(),
                &tr("overview.peer.row.resolution_unknown_desc"),
                false,
            );
            kv_row(
                ui,
                &tr("overview.peer.row.fps"),
                peer.fps.map(|f| f.to_string()).as_deref(),
                &tr("overview.peer.row.fps_unknown_desc"),
                false,
            );
            kv_row(
                ui,
                &tr("overview.peer.row.address"),
                peer.addr.as_deref(),
                &tr("overview.peer.row.address_unknown_desc"),
                true,
            );
        });
    });
}

/// A "label: value" row. When `value` is `None`, render a subtle "—" with the
/// supplied tooltip explaining why it's unknown (never a faked value). `mono`
/// renders the value in a monospace font (for addresses / encoder tokens).
fn kv_row(ui: &mut egui::Ui, label: &str, value: Option<&str>, unknown_tip: &str, mono: bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .small()
                .weak()
                .color(Color32::from_gray(140)),
        );
        ui.add_space(4.0);
        match value {
            Some(v) if !v.trim().is_empty() => {
                let mut rt = RichText::new(v).small();
                if mono {
                    rt = rt.monospace();
                }
                ui.label(rt);
            }
            _ => {
                ui.label(RichText::new("—").small().weak())
                    .describe_named(label.to_string(), unknown_tip.to_string());
            }
        }
    });
}

/// Tooltip text for a liveness badge — explains the honest limitation behind it.
fn liveness_tip(l: Liveness) -> String {
    match l {
        Liveness::Online => tr("overview.liveness.online"),
        Liveness::Streaming => tr("overview.liveness.streaming"),
        Liveness::Offline => tr("overview.liveness.offline"),
        Liveness::Unknown => tr("overview.liveness.unknown"),
    }
}
