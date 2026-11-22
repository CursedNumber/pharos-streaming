// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Video tab — the encode → codec → decode path matrix.
//!
//! Modelled on the Sunshine/Moonlight "supported codecs" flowchart: three
//! left-to-right columns (HOST ENCODE → CODEC → CLIENT DECODE) and a 1..6
//! performance legend. Every option box shows its
//! vendor, codec and a coloured performance chip; only the options that are
//! actually *available on this box right now* are shown prominently, with the
//! unavailable ones tucked behind a "show unavailable" toggle (off by default).
//! Routes we've validated end-to-end on real hardware get a ★ and a coloured
//! border with a "validated on …" tooltip.
//!
//! This tab is a **read-mostly path overview** — picking a specific
//! codec/bitrate/preset is a separate, upcoming tuning session. The only
//! mutating control is the Re-probe button, which re-runs the capability probes
//! fresh (decode via the uncached `reprobe`) on a worker thread so the UI thread
//! never blocks.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};

use egui::{Color32, RichText, Rounding, Stroke};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as ph;

use control_ipc::{LatencyReport, Request, VideoCodec};

use crate::a11y::Describe;
use crate::i18n::tr;
use crate::model::{CapabilityUi, PanelState, PipelineStage};

/// How many frames the on-demand latency benchmark samples (the user-requested
/// default; deep enough to average out jitter, short enough to feel snappy).
const BENCH_FRAMES: u32 = 240;

/// Localised performance-rank label (1..=6). `theme::perf_label` returns a
/// hardcoded English `&'static str` that this tab does not own, so we route the
/// rank through our own `video.perf.<n>` keys for translation.
fn perf_label(rank: u8) -> String {
    tr(&format!("video.perf.{}", rank.clamp(1, 6)))
}

// ---------------------------------------------------------------------------
// Background re-probe plumbing
//
// We cannot add fields to the shared PanelState, so the in-flight probe lives
// in a module-local cell. A click spawns a worker thread that re-runs the
// capability probes fresh (encoders re-listed; decode via the uncached reprobe)
// and sends the results back; we drain the channel once per frame and swap them
// into PanelState.
// ---------------------------------------------------------------------------

type ProbeResult = (Vec<CapabilityUi>, Vec<CapabilityUi>);

fn probe_inbox() -> &'static Mutex<Option<Receiver<ProbeResult>>> {
    static CELL: OnceLock<Mutex<Option<Receiver<ProbeResult>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// True while a worker re-probe is outstanding (drives the spinner + disables
/// the button).
fn probe_in_flight() -> bool {
    probe_inbox().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Kick off a background re-probe if one isn't already running.
fn start_reprobe() {
    let mut slot = match probe_inbox().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if slot.is_some() {
        return; // already running
    }
    let (tx, rx) = std::sync::mpsc::channel();
    *slot = Some(rx);
    std::thread::spawn(move || {
        // Both probes run fresh: encoders are re-listed from ffmpeg, and decode
        // uses the *uncached* reprobe so a GPU plugged/unplugged since startup is
        // reflected (the cached `capabilities()` is left alone for the live path).
        let encode = crate::data::encode_capabilities();
        let decode = crate::data::reprobe_decode();
        // Receiver may be gone if the window closed mid-probe — that's fine.
        let _ = tx.send((encode, decode));
    });
}

/// Drain a finished re-probe into `state`. Returns true if results landed.
fn poll_reprobe(state: &mut PanelState) -> bool {
    let mut slot = match probe_inbox().lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let done = if let Some(rx) = slot.as_ref() {
        match rx.try_recv() {
            Ok((encode, decode)) => {
                state.encode_caps = encode;
                state.decode_caps = decode;
                state.status = Some("Capability re-probe complete.".to_string());
                true
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                // Worker died without sending — clear the slot so the button
                // works again.
                state.last_error = Some("Re-probe worker exited unexpectedly.".to_string());
                true
            }
        }
    } else {
        return false;
    };
    if done {
        *slot = None;
    }
    done
}

// ---------------------------------------------------------------------------
// On-demand latency benchmark plumbing
//
// A second module-local channel trio (mirroring the reprobe one above). The
// benchmark itself runs on the *host* — we ask for it over the control socket
// (Request::RunLatencyBench) and the reply (Event::Latency) is drained from the
// crate::control handle. Two cells hold the UI-side state without touching the
// frozen PanelState:
//   * `bench_inflight` — true between the request and the first reply (drives the
//      spinner + disables the button), with a deadline so a never-answering host
//      doesn't wedge the button forever;
//   * `bench_report`  — the last LatencyReport (or an error string) to render.
// ---------------------------------------------------------------------------

/// In-flight deadline cell: `Some(Instant)` while a bench is outstanding. We
/// clear it when a Latency/Error event arrives or the deadline lapses.
fn bench_inflight() -> &'static Mutex<Option<std::time::Instant>> {
    static CELL: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// The last benchmark outcome: a report, or a human error message.
type BenchOutcome = Result<LatencyReport, String>;

fn bench_report() -> &'static Mutex<Option<BenchOutcome>> {
    static CELL: OnceLock<Mutex<Option<BenchOutcome>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// True while a benchmark is outstanding (and within its deadline).
fn bench_in_flight() -> bool {
    bench_inflight()
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

/// Kick off a benchmark: send the request over the control socket and arm the
/// in-flight deadline. No-op if one is already running or no host is connected.
fn start_bench(state: &mut PanelState) {
    if !crate::control::handle().connected() {
        state.last_error = Some(crate::control::unavailable_reason());
        return;
    }
    let mut slot = match bench_inflight().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if slot.is_some() {
        return; // already running
    }
    // Generous deadline: the host samples BENCH_FRAMES; at 60fps that's ~4s, so
    // 15s leaves slack for a slow link before we give up and re-enable the button.
    *slot = Some(std::time::Instant::now() + std::time::Duration::from_secs(15));
    crate::control::handle().send_request(control_ipc::Request::RunLatencyBench {
        frames: BENCH_FRAMES,
    });
    state.status = Some(tr("video.bench.started"));
}

/// Pump the control handle: fold a finished benchmark (Latency or Error) into the
/// bench cells, and expire a stale deadline. Other event kinds are ignored here
/// (the cached link state still absorbs them inside `drain_events`).
fn poll_bench(state: &mut PanelState) {
    for ev in crate::control::handle().drain_events() {
        match ev {
            control_ipc::Event::Latency(report) => {
                if let Ok(mut r) = bench_report().lock() {
                    *r = Some(Ok(report));
                }
                if let Ok(mut f) = bench_inflight().lock() {
                    *f = None;
                }
                state.status = Some(tr("video.bench.done"));
            }
            control_ipc::Event::Error { msg } => {
                if let Ok(mut r) = bench_report().lock() {
                    *r = Some(Err(msg.clone()));
                }
                if let Ok(mut f) = bench_inflight().lock() {
                    *f = None;
                }
                state.last_error = Some(format!("{} {msg}", tr("video.bench.error_prefix")));
            }
            _ => {}
        }
    }
    // Expire a stale in-flight deadline so the button never wedges.
    if let Ok(mut f) = bench_inflight().lock() {
        if let Some(deadline) = *f {
            if std::time::Instant::now() >= deadline {
                *f = None;
                if let Ok(mut r) = bench_report().lock() {
                    if r.is_none() || matches!(r.as_ref(), Some(Ok(_))) {
                        *r = Some(Err(tr("video.bench.timeout")));
                    }
                }
                state.last_error = Some(tr("video.bench.timeout"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tab entry point
// ---------------------------------------------------------------------------

pub fn ui(ui: &mut egui::Ui, state: &mut PanelState) {
    // Pull in any finished background probe before we render this frame.
    let landed = poll_reprobe(state);
    // Pump the control socket so a finished benchmark (or error) is reflected.
    poll_bench(state);
    let running = probe_in_flight();
    let benching = bench_in_flight();
    // Keep ticking while a probe or benchmark is in flight so the spinner
    // animates and the result is picked up promptly even without mouse motion.
    if running || benching {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(120));
    }
    let _ = landed;

    header(ui, state, running);
    ui.add_space(6.0);
    legend(ui);
    ui.add_space(10.0);

    // "Show unavailable" toggle — persisted in egui memory so it survives tab
    // switches without touching the shared state.
    let mem_id = egui::Id::new("video_show_unavailable");
    let mut show_unavailable = ui
        .ctx()
        .data_mut(|d| d.get_temp::<bool>(mem_id).unwrap_or(false));

    ui.horizontal(|ui| {
        if ui
            .checkbox(&mut show_unavailable, tr("video.show_unavailable"))
            .describe(tr("video.show_unavailable_desc"))
            .changed()
        {
            ui.ctx()
                .data_mut(|d| d.insert_temp(mem_id, show_unavailable));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let avail_enc = state.encode_caps.iter().filter(|c| c.available).count();
            let avail_dec = state.decode_caps.iter().filter(|c| c.available).count();
            let enc_word = if avail_enc == 1 {
                tr("video.avail.encoder_one")
            } else {
                tr("video.avail.encoder_many")
            };
            let dec_word = if avail_dec == 1 {
                tr("video.avail.decoder_one")
            } else {
                tr("video.avail.decoder_many")
            };
            ui.label(
                RichText::new(format!(
                    "{avail_enc} {enc_word} · {avail_dec} {dec_word} {}",
                    tr("video.avail.available_here"),
                ))
                .small()
                .weak(),
            )
            .describe(tr("video.avail_count_desc"));
        });
    });
    ui.add_space(10.0);

    pace_control(ui);
    ui.add_space(8.0);
    resolution_control(ui);
    ui.add_space(12.0);

    matrix(ui, state, show_unavailable);

    ui.add_space(14.0);
    latency_section(ui, state, benching);

    ui.add_space(14.0);
    footer_note(ui);
}

/// W8 live pacing control: an **Auto (adaptive)** toggle + a ceiling/fixed-fps
/// slider, sent to the host over control-ipc (`Request::SetFps`) with no relaunch.
/// The host doesn't report its auto/cap back in status (only the measured fps), so
/// the control reflects the panel's own intent (egui temp memory); the live "now N
/// fps" readout confirms the effect. Auto = the host self-tunes to its sustainable
/// rate (capped here); off = pace at a fixed fps.
fn pace_control(ui: &mut egui::Ui) {
    let id_auto = egui::Id::new("video_pace_auto");
    let id_cap = egui::Id::new("video_pace_cap");
    let now_fps = active_fps();
    let mut auto = ui
        .ctx()
        .data_mut(|d| d.get_temp::<bool>(id_auto).unwrap_or(true));
    let mut cap = ui.ctx().data_mut(|d| {
        d.get_temp::<u32>(id_cap)
            .unwrap_or_else(|| now_fps.unwrap_or(144).clamp(30, 144))
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Pacing").strong());
        let mut changed = ui.checkbox(&mut auto, "Auto (adaptive)").changed();
        let slider_label = if auto { "ceiling" } else { "fixed fps" };
        changed |= ui
            .add(egui::Slider::new(&mut cap, 30..=144).text(slider_label))
            .changed();
        if let Some(f) = now_fps {
            ui.label(RichText::new(format!("· now {f} fps")).weak());
        }
        if changed {
            ui.ctx().data_mut(|d| {
                d.insert_temp(id_auto, auto);
                d.insert_temp(id_cap, cap);
            });
            crate::control::handle().send_request(Request::SetFps { auto, fps: cap });
        }
    });
}

/// Resolution ladder (Phase 2) control: a TARGET coded-height selector, sent to
/// the host over control-ipc (`Request::SetResolution`) with no relaunch. `Auto`
/// (0) target streams native; the host preserves each output's aspect ratio and
/// may drop BELOW the target under sustained link pressure — but never below the
/// Min. The "now W×H" readout is the host's reported coded size; a "↓ link" note
/// shows when the ladder has auto-dropped below the target.
///
/// The MIN (floor) is READ-ONLY here: the connected client owns the [min, target]
/// range and re-asserts it continuously (a panel edit would be stomped on its next
/// send), so we mirror the client's live floor and point the user at the app. See
/// `crates/compositor/src/multi.rs` (DG_CLIENT_RESPREF apply loop).
fn resolution_control(ui: &mut egui::Ui) {
    const CEIL_RUNGS: &[(u32, &str)] = &[
        (0, "Auto"),
        (1440, "1440p"),
        (1080, "1080p"),
        (720, "720p"),
        (540, "540p"),
    ];
    let (coded, ceiling_live, floor_live) = active_coded();
    ui.horizontal(|ui| {
        if let Some(sel) = ladder_combo(
            ui,
            "video_res_ceiling",
            "Resolution",
            CEIL_RUNGS,
            ceiling_live,
        ) {
            crate::control::handle().send_request(Request::SetResolution { height: sel });
        }
        if let Some((w, h)) = coded {
            ui.label(RichText::new(format!("· now {w}×{h}")).weak());
            // Auto-drop indicator, driven off the HOST-reported ceiling (clamped to
            // native), not the local selection — so picking an above-native rung
            // (clamped to Auto on the host) can't false-fire it.
            if ceiling_live != 0 && h < ceiling_live {
                ui.label(RichText::new("↓ link").color(crate::theme::warn()))
                    .on_hover_text("auto-dropped below the target under link pressure");
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Min").strong());
        let floor_txt = if floor_live == 0 {
            "None".to_string()
        } else {
            format!("{floor_live}p")
        };
        // Read-only: the connected client owns this. Muted so it doesn't read as
        // an editable control.
        ui.label(RichText::new(floor_txt).weak())
            .on_hover_text("the connected client owns the min/target range");
        ui.label(RichText::new("· set in the Trireme app").weak());
    });
}

/// One resolution-ladder bound selector (target or min) with grace-window temp
/// reconciliation: the host reports the value it actually holds, so we trust the
/// local pick only briefly after an edit (before the ~250ms status poll reflects
/// it) — afterwards host state wins, so the control can't lie after a host
/// relaunch reset it. Returns `Some(height)` only when the user just changed it.
fn ladder_combo(
    ui: &mut egui::Ui,
    id_str: &str,
    label: &str,
    rungs: &[(u32, &str)],
    live: u32,
) -> Option<u32> {
    const GRACE_SECS: f64 = 1.5;
    let id = egui::Id::new(id_str);
    let now = ui.input(|i| i.time);
    let pending = ui.ctx().data_mut(|d| d.get_temp::<(u32, f64)>(id));
    let mut sel = match pending {
        Some((v, t)) if now - t < GRACE_SECS => v,
        _ => live,
    };
    ui.label(RichText::new(label).strong());
    let cur_label = rungs
        .iter()
        .find(|(h, _)| *h == sel)
        .map(|(_, l)| *l)
        .unwrap_or(rungs[0].1);
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(cur_label)
        .show_ui(ui, |ui| {
            for (h, label) in rungs {
                changed |= ui.selectable_value(&mut sel, *h, *label).changed();
            }
        });
    if changed {
        ui.ctx().data_mut(|d| d.insert_temp(id, (sel, now)));
        Some(sel)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Latency benchmark — header line, Run button, per-step results table
// ---------------------------------------------------------------------------

/// One renderable row of the latency table: a localised step label, the added
/// milliseconds (pre-formatted so the network row can carry its ± uncertainty),
/// an optional highlight colour (for the two attention rows — wire + buffer),
/// and the Total flag (bold, em-dash codec cell).
struct StepRow {
    label: String,
    /// Screen-reader / hover description for the step.
    desc: String,
    /// The ms value, already formatted (e.g. "3.4 ms" or "8.1 ± 0.6 ms").
    value: String,
    /// When set, the label + value are drawn bold in this colour (the wire row
    /// in the theme accent, the buffer row in `theme::warn`).
    highlight: Option<Color32>,
    /// The Total row — emphasised (bold).
    is_total: bool,
}

fn latency_section(ui: &mut egui::Ui, state: &mut PanelState, benching: bool) {
    crate::theme::section(
        ui,
        ph::TIMER,
        &tr("video.bench.heading"),
        &tr("video.bench.heading_desc"),
    );

    let connected = crate::control::handle().connected();

    // Run / spinner row.
    ui.horizontal(|ui| {
        if benching {
            ui.add(egui::Spinner::new())
                .describe_named(tr("video.bench.running"), tr("video.bench.running_desc"));
            ui.label(RichText::new(tr("video.bench.running")).small().weak())
                .describe(tr("video.bench.running_desc"));
        } else {
            let btn = ui
                .add_enabled(
                    connected,
                    egui::Button::new(format!("{}  {}", ph::TIMER, tr("video.bench.run"))),
                )
                .describe(tr("video.bench.run_desc"))
                .on_disabled_hover_text(crate::control::unavailable_reason());
            if btn.clicked() {
                start_bench(state);
            }
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

    ui.add_space(6.0);

    // Render the last outcome, if any.
    let outcome = bench_report().lock().ok().and_then(|g| g.clone());
    match outcome {
        Some(Ok(report)) => latency_table(ui, state, &report),
        Some(Err(msg)) => {
            ui.colored_label(
                crate::theme::error(),
                format!("{}  {} {msg}", ph::WARNING, tr("video.bench.error_prefix")),
            )
            .describe(tr("video.bench.error_desc"));
        }
        None => {
            crate::widgets::card(ui, "video_bench_hint", |ui| {
                ui.label(RichText::new(tr("video.bench.hint")).weak())
                    .describe(tr("video.bench.hint_desc"));
            });
        }
    }
}

/// Build the ordered per-step rows from a re-attributed report, skipping any step
/// whose value is below `EPS` (not reported at this granularity). Two rows are
/// highlighted: the wire leg (network/router, ± uncertainty, theme accent) and
/// the in-flight buffer (usually the dominant number, `theme::warn`). The Total
/// row is always last and emphasised.
fn build_step_rows(r: &LatencyReport) -> Vec<StepRow> {
    let mut rows = Vec::new();

    push_measured(
        &mut rows,
        "video.bench.step.compose",
        "video.bench.step.compose_desc",
        r.compose_ms,
    );
    push_measured(
        &mut rows,
        "video.bench.step.encode",
        "video.bench.step.encode_desc",
        r.encode_ms,
    );
    push_measured(
        &mut rows,
        "video.bench.step.host_send",
        "video.bench.step.host_send_desc",
        r.host_send_ms,
    );

    // Network · wire — the RTT/2 leg, with ± uncertainty, in the theme accent.
    // Always shown: it's the "unverified in-the-middle" the caption pins.
    rows.push(StepRow {
        label: tr("video.bench.step.nexus"),
        desc: tr("video.bench.step.nexus_desc"),
        value: format!("{:.1} ± {:.1} ms", r.nexus_ms, r.nexus_uncertainty_ms),
        highlight: Some(crate::theme::info()),
        is_total: false,
    });

    // Buffer · in-flight — usually the dominant number; warn-coloured so the eye
    // lands on it. Skipped only if it's effectively zero.
    push_highlighted(
        &mut rows,
        "video.bench.step.buffer",
        "video.bench.step.buffer_desc",
        r.buffer_ms,
        crate::theme::warn(),
    );

    // (No separate Decode row: the GPU decode kernel can't be isolated from the
    // in-flight buffer through an external pipe-fed ffmpeg, so it's folded into the
    // Buffer row above — see its description.)
    push_measured(
        &mut rows,
        "video.bench.step.present",
        "video.bench.step.present_desc",
        r.present_ms,
    );

    // Total — always last, emphasised.
    rows.push(StepRow {
        label: tr("video.bench.step.total"),
        desc: tr("video.bench.step.total_desc"),
        value: format!("{:.1} ms", r.total_ms),
        highlight: None,
        is_total: true,
    });

    rows
}

/// Below this (ms) a measured step is treated as "not reported" and skipped.
const STEP_EPS: f32 = 0.05;

/// Push a measured step row, skipping it when its value is below `STEP_EPS`
/// (not reported at this timing granularity).
fn push_measured(rows: &mut Vec<StepRow>, label_key: &str, desc_key: &str, v: f32) {
    if v >= STEP_EPS {
        rows.push(StepRow {
            label: tr(label_key),
            desc: tr(desc_key),
            value: format!("{v:.1} ms"),
            highlight: None,
            is_total: false,
        });
    }
}

/// Push a highlighted step row (label + value drawn bold in `color`), skipping it
/// when its value is below `STEP_EPS`.
fn push_highlighted(
    rows: &mut Vec<StepRow>,
    label_key: &str,
    desc_key: &str,
    v: f32,
    color: Color32,
) {
    if v >= STEP_EPS {
        rows.push(StepRow {
            label: tr(label_key),
            desc: tr(desc_key),
            value: format!("{v:.1} ms"),
            highlight: Some(color),
            is_total: false,
        });
    }
}

/// Serialise a benchmark report as a single-row ("wide") CSV: one header line
/// plus one data row, so successive runs paste into a spreadsheet as
/// accumulating rows. Delivered via the clipboard — the most platform-agnostic
/// route (egui has it built in; native file dialogs are the cross-platform
/// pain point, and would drag in a dialog dependency). All stage values are in
/// milliseconds, at sub-µs precision for analysis.
fn report_to_csv(r: &LatencyReport) -> String {
    let header = "codec,samples,rtt_ms,clock_offset_ms,compose_ms,encode_ms,\
host_send_ms,nexus_ms,nexus_uncertainty_ms,buffer_ms,decode_ms,present_ms,total_ms";
    let data = format!(
        "{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
        r.codec,
        r.samples,
        r.rtt_ms,
        r.clock_offset_ms,
        r.compose_ms,
        r.encode_ms,
        r.host_send_ms,
        r.nexus_ms,
        r.nexus_uncertainty_ms,
        r.buffer_ms,
        r.decode_ms,
        r.present_ms,
        r.total_ms,
    );
    format!("{header}\n{data}\n")
}

fn latency_table(ui: &mut egui::Ui, state: &mut PanelState, report: &LatencyReport) {
    // Summary line: samples · RTT · clock offset (diagnostic only).
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!(
                "{} {} · {} {:.1} ms · {} {:+.1} ms",
                tr("video.bench.summary.samples"),
                report.samples,
                tr("video.bench.summary.rtt"),
                report.rtt_ms,
                tr("video.bench.summary.offset"),
                report.clock_offset_ms,
            ))
            .small()
            .weak(),
        )
        .describe(tr("video.bench.summary_desc"));
    });
    ui.add_space(4.0);

    let rows = build_step_rows(report);
    let weak = ui.visuals().weak_text_color();

    crate::widgets::card(ui, "video_bench_table", |ui| {
        // Header underline + Total overline, so the header band and the Total
        // read as distinct from the striped per-step rows (not just bold).
        let rule = Stroke::new(1.5_f32, ui.visuals().widgets.noninteractive.fg_stroke.color);
        let head_color = ui.visuals().strong_text_color();
        TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::auto().at_least(150.0)) // Step
            .column(Column::auto().at_least(70.0)) // Codec
            .column(Column::remainder().at_least(110.0)) // Added latency
            .header(24.0, |mut header| {
                // Bigger, strong-coloured header text + a rule under each cell.
                let head = |ui: &mut egui::Ui, key: &str, desc: &str| {
                    ui.label(RichText::new(tr(key)).strong().size(14.0).color(head_color))
                        .describe(tr(desc));
                    let r = ui.max_rect();
                    ui.painter().hline(r.x_range(), r.bottom(), rule);
                };
                header.col(|ui| head(ui, "video.bench.col.step", "video.bench.col.step_desc"));
                header.col(|ui| head(ui, "video.bench.col.codec", "video.bench.col.codec_desc"));
                header.col(|ui| {
                    head(
                        ui,
                        "video.bench.col.latency",
                        "video.bench.col.latency_desc",
                    )
                });
            })
            .body(|mut body| {
                for row in &rows {
                    // The Total row is taller, with an overline across all cells.
                    let row_h = if row.is_total { 26.0 } else { 22.0 };
                    body.row(row_h, |mut tr_row| {
                        // Step label.
                        tr_row.col(|ui| {
                            if row.is_total {
                                let r = ui.max_rect();
                                ui.painter().hline(r.x_range(), r.top(), rule);
                            }
                            let mut label = RichText::new(&row.label);
                            if row.is_total {
                                label = label.strong().size(15.0);
                            }
                            if let Some(color) = row.highlight {
                                label = label.color(color).strong();
                            }
                            ui.label(label).describe(row.desc.clone());
                        });
                        // Codec column (constant per report).
                        tr_row.col(|ui| {
                            if row.is_total {
                                let r = ui.max_rect();
                                ui.painter().hline(r.x_range(), r.top(), rule);
                                // The total spans every codec stage — no single
                                // codec; show an em dash to keep the column honest.
                                ui.label(RichText::new("—").small().color(weak))
                                    .describe(tr("video.bench.col.codec_total_desc"));
                            } else {
                                ui.label(RichText::new(&report.codec).small().monospace())
                                    .describe(tr("video.bench.col.codec_desc"));
                            }
                        });
                        // Added-latency value.
                        tr_row.col(|ui| {
                            if row.is_total {
                                let r = ui.max_rect();
                                ui.painter().hline(r.x_range(), r.top(), rule);
                            }
                            let mut v = RichText::new(&row.value).monospace();
                            if row.is_total {
                                v = v.strong().size(15.0);
                            }
                            if let Some(color) = row.highlight {
                                v = v.color(color).strong();
                            }
                            ui.label(v).describe(row.desc.clone());
                        });
                    });
                }
            });
    });

    // Wire-row caption: RTT/2 from the clock-sync probes — the true wire/router
    // leg, the "unverified in-the-middle". Drawn in the same accent as its row.
    let accent = crate::theme::info();
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(ph::INFO).color(accent).small());
        ui.label(
            RichText::new(tr("video.bench.nexus_caption"))
                .small()
                .color(accent),
        )
        .describe(tr("video.bench.step.nexus_desc"));
    });

    // Buffer-row caption: it's the QUIC flow-control window + send queue, not
    // compute — the knob that usually dominates. Warn-coloured to match its row.
    let warn = crate::theme::warn();
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(ph::WARNING).color(warn).small());
        ui.label(
            RichText::new(tr("video.bench.buffer_caption"))
                .small()
                .color(warn),
        )
        .describe(tr("video.bench.step.buffer_desc"));
    });

    // Export the result as CSV to the clipboard (platform-agnostic — no native
    // file dialog). Paste straight into a spreadsheet; successive runs stack as
    // rows under the shared header.
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .button(format!("{}  {}", ph::COPY, tr("video.bench.copy_csv")))
            .describe(tr("video.bench.copy_csv_desc"))
            .clicked()
        {
            ui.ctx().copy_text(report_to_csv(report));
            state.status = Some(tr("video.bench.copied"));
        }
        ui.label(
            RichText::new(tr("video.bench.copy_csv_hint"))
                .small()
                .weak(),
        );
    });
}

// ---------------------------------------------------------------------------
// Header — title, intro, Re-probe button
// ---------------------------------------------------------------------------

fn header(ui: &mut egui::Ui, _state: &PanelState, running: bool) {
    ui.horizontal(|ui| {
        ui.heading(tr("video.heading")).describe(tr("video.intro"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if running {
                ui.add(egui::Spinner::new())
                    .describe_named(tr("video.probing"), tr("video.probing_desc"));
                ui.label(RichText::new(tr("video.probing")).small().weak())
                    .describe(tr("video.probing_desc"));
            } else {
                let btn = ui
                    .button(format!("{}  {}", ph::ARROWS_CLOCKWISE, tr("video.reprobe")))
                    .describe(tr("video.reprobe_desc"));
                if btn.clicked() {
                    start_reprobe();
                }
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Performance legend (1..6)
// ---------------------------------------------------------------------------

fn legend(ui: &mut egui::Ui) {
    crate::widgets::card(ui, "video_legend", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr("video.legend.heading")).strong())
                .describe(tr("video.legend.desc"));
            ui.add_space(6.0);
            for rank in 1u8..=6 {
                perf_chip(ui, rank);
                ui.label(RichText::new(perf_label(rank)).small().weak())
                    .describe(tr("video.legend.desc"));
                ui.add_space(10.0);
            }
        });
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("★").color(crate::theme::validated()))
                .describe_named(
                    tr("video.box.status.validated"),
                    tr("video.legend.validated_mark_desc"),
                );
            ui.label(
                RichText::new(tr("video.legend.validated_text"))
                    .small()
                    .weak(),
            )
            .describe(tr("video.legend.validated_mark_desc"));
        });
        // Impact-chart legend: the three columns on each card.
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("impact").small().strong())
                .describe("Each card's mini chart: relative cost in three dimensions.");
            ui.add_space(4.0);
            let colors = impact_colors();
            for (c, label) in
                colors
                    .iter()
                    .zip(["GPU / hardware", "CPU / software", "size / network"])
            {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 11.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(2.0), *c);
                ui.label(RichText::new(label).small().weak());
                ui.add_space(8.0);
            }
        });
    });
}

/// A small "N" performance chip on the green→red ramp (theme::perf_color).
fn perf_chip(ui: &mut egui::Ui, rank: u8) {
    let c = crate::theme::perf_color(rank);
    ui.label(
        RichText::new(format!(" {rank} "))
            .color(Color32::WHITE)
            .small()
            .strong()
            .background_color(c),
    )
    .describe_named(perf_label(rank), tr("video.perf.chip_desc"));
}

// ---------------------------------------------------------------------------
// The three-column matrix
// ---------------------------------------------------------------------------

fn matrix(ui: &mut egui::Ui, state: &mut PanelState, show_unavailable: bool) {
    // Live host codec drives which cards light up "active" (full highlight +
    // arrows); a pending click glows until the host catches up.
    let active = active_codec();
    let active_key = active.as_deref().map(codec_key);
    let pending = read_pending(ui);
    let pending_key = pending.as_deref().map(codec_key);
    let fps = active_fps();
    let report = last_bench_report();

    // Render inside a block that borrows `state` immutably; the borrow ends
    // before we touch `state.status` below.
    let (clicked, rects) = {
        let mut encoders: Vec<&CapabilityUi> = state.encode_caps.iter().collect();
        sort_caps(&mut encoders);
        let mut decoders: Vec<&CapabilityUi> = state.decode_caps.iter().collect();
        sort_caps(&mut decoders);
        // Float the active (live) card, then the selected (pending) one, to the
        // top of each column. Stable, so best-first order holds within a group.
        encoders.sort_by_key(|c| float_rank(c, active_key, pending_key, true));
        decoders.sort_by_key(|c| float_rank(c, active_key, pending_key, false));

        // Exactly one card per column is the active one: the first available
        // card whose codec matches the host's live codec (encoder must also be
        // a hardware/NVENC path — that's what the host runs — EXCEPT VVC, whose
        // only host path is the software libvvenc encoder).
        let active_enc = encoders.iter().position(|c| {
            c.available
                && Some(codec_key(&c.codec)) == active_key
                && (is_hw(&c.token) || codec_key(&c.codec) == "VVC")
        });
        let active_dec = decoders
            .iter()
            .position(|c| c.available && Some(codec_key(&c.codec)) == active_key);

        let mut clicked: Option<VideoCodec> = None;
        let mut rects: [Option<egui::Rect>; 3] = [None, None, None];

        ui.columns(3, |cols| {
            // --- Left: HOST ENCODE ---------------------------------------
            column_header(
                &mut cols[0],
                &format!("{}  {}", ph::MONITOR, tr("video.col.encode")),
                crate::theme::info(),
                &tr("video.col.encode_desc"),
            );
            let mut shown = 0usize;
            for (i, cap) in encoders.iter().enumerate() {
                if cap.available || show_unavailable {
                    let is_active = Some(i) == active_enc;
                    let is_pending = pending_key == Some(codec_key(&cap.codec))
                        && active_key != Some(codec_key(&cap.codec));
                    // Hardware-path cards drive a host codec switch; the one
                    // software exception is VVC (libvvenc IS the host path).
                    let switchable = switch_codec_for(&cap.codec).is_some()
                        && (is_hw(&cap.token) || codec_key(&cap.codec) == "VVC")
                        && cap.available;
                    // The encoder card shows ITS step — the encode stage — not the
                    // whole-pipeline total. Measured when this card is live and a
                    // benchmark has run; otherwise a reference estimate.
                    let metric = card_metric(
                        cap.available,
                        is_active,
                        report.as_ref().map(|r| r.encode_ms),
                        encode_reference_ms(&cap.token),
                        fps,
                    );
                    let (rect, click) =
                        option_box(&mut cols[0], cap, is_active, is_pending, switchable, metric);
                    if is_active {
                        rects[0] = Some(rect);
                    }
                    if let Some(c) = click {
                        clicked = Some(c);
                    }
                    shown += 1;
                }
            }
            empty_state(&mut cols[0], shown, show_unavailable, "encoders");

            // --- Center: CODEC (transport is always QUIC) ----------------
            column_header(
                &mut cols[1],
                &format!("{}  {}", ph::FILM_SLATE, tr("video.col.codec")),
                crate::theme::info(),
                &tr("video.col.codec_desc"),
            );
            rects[1] = codec_cards(&mut cols[1], active_key, pending_key, &mut clicked);

            // --- Right: CLIENT DECODE ------------------------------------
            column_header(
                &mut cols[2],
                &format!("{}  {}", ph::FILM_STRIP, tr("video.col.decode")),
                crate::theme::info(),
                &tr("video.col.decode_desc"),
            );
            let mut shown = 0usize;
            for (i, cap) in decoders.iter().enumerate() {
                if cap.available || show_unavailable {
                    let is_active = Some(i) == active_dec;
                    // The decoder card shows the decode stage (the codec turning
                    // bytes into a frame) — measured `decode_ms` from the client
                    // when available, else a per-decoder reference.
                    let metric = card_metric(
                        cap.available,
                        is_active,
                        report.as_ref().map(|r| r.decode_ms),
                        decode_reference_ms(&cap.token),
                        fps,
                    );
                    let (rect, _) = option_box(&mut cols[2], cap, is_active, false, false, metric);
                    if is_active {
                        rects[2] = Some(rect);
                    }
                    shown += 1;
                }
            }
            empty_state(&mut cols[2], shown, show_unavailable, "decoders");
        });
        (clicked, rects)
    };

    // A click on a switchable card: send the codec switch and glow it pending.
    if let Some(c) = clicked {
        write_pending(ui, c);
        crate::control::handle().send_request(Request::SetVideoCodec { codec: c });
        state.status = Some(format!(
            "Switching video to {} — host is restarting the encoder…",
            c.label()
        ));
    }
    // Once the host reports the new codec live, settle: clear the pending marker
    // and the transient "switching…" note so the UI doesn't read as stuck.
    if pending_key.is_some() && pending_key == active_key {
        ui.ctx()
            .data_mut(|d| d.remove_temp::<String>(egui::Id::new("video_pending_codec")));
        if state
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("Switching video"))
        {
            state.status = None;
        }
    }

    // Connect the active encoder → codec → decoder with arrows (drawn over the
    // just-rendered columns, in screen space).
    draw_arrows(ui, &rects);
}

/// Best-first ordering inside a column: available before unavailable, then by
/// performance rank, then validated first, then label for a stable tie-break.
fn sort_caps(caps: &mut [&CapabilityUi]) {
    caps.sort_by(|a, b| {
        b.available
            .cmp(&a.available)
            .then(a.perf_rank.cmp(&b.perf_rank))
            .then(b.validated.cmp(&a.validated))
            .then(a.label.cmp(&b.label))
    });
}

fn column_header(ui: &mut egui::Ui, title: &str, color: Color32, hover: &str) {
    ui.label(RichText::new(title).strong().size(14.0).color(color))
        .describe(hover.to_string());
    ui.separator();
    ui.add_space(2.0);
}

/// `what` is the locale stem ("encoders" / "decoders"); we pick the matching
/// `video.empty.<what>_all` / `_avail` key.
fn empty_state(ui: &mut egui::Ui, shown: usize, show_unavailable: bool, what: &str) {
    if shown == 0 {
        let key = if show_unavailable {
            format!("video.empty.{what}_all")
        } else {
            format!("video.empty.{what}_avail")
        };
        ui.add_space(4.0);
        ui.label(RichText::new(tr(&key)).small().weak().italics());
    }
}

// ---------------------------------------------------------------------------
// Dashboard layer: selection, impact charts, active-path arrows
// ---------------------------------------------------------------------------

/// Impact-bar colours, in column order: GPU, CPU, Network.
fn impact_colors() -> [Color32; 3] {
    [
        crate::theme::info(),      // GPU / hardware
        crate::theme::warn(),      // CPU / software
        crate::theme::available(), // Size / network
    ]
}

/// Normalise any codec string ("H.265", "hevc", "AVC", "h264", "VVC", "h266")
/// to a stable abbreviation key: "HEVC" / "AV1" / "AVC" / "VVC".
fn codec_key(codec: &str) -> &'static str {
    let c = codec.to_ascii_uppercase();
    if c.contains("AV1") {
        "AV1"
    } else if c.contains("264") || c.contains("AVC") {
        "AVC"
    } else if c.contains("266") || c.contains("VVC") {
        "VVC"
    } else {
        "HEVC"
    }
}

/// The host video codec a card maps to when clicked, or `None` if selecting it
/// can't drive a host switch (decoders are client-side). All four codecs route
/// to `Request::SetVideoCodec`; the host preflights VVC (software libvvenc) and
/// refuses with a readable error when its ffmpeg can't encode it.
fn switch_codec_for(codec: &str) -> Option<VideoCodec> {
    match codec_key(codec) {
        "HEVC" => Some(VideoCodec::Hevc),
        "AV1" => Some(VideoCodec::Av1),
        "AVC" => Some(VideoCodec::H264),
        "VVC" => Some(VideoCodec::Vvc),
        _ => None,
    }
}

/// Is this ffmpeg/NVENC token a hardware path (vs a software codec)?
fn is_hw(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    !(t.contains("libx")
        || t.contains("libsvt")
        || t.contains("libaom")
        || t.contains("dav1d")
        || t.contains("vvenc")
        || t.contains("vvdec")
        || t.contains("software"))
}

/// Relative GPU / CPU / Network impact (each 0..1) for the mini chart. GPU-vs-CPU
/// is set by whether the path is hardware; Network is the codec's relative
/// bitrate (VVC < AV1 < HEVC < AVC). A static reference profile, not live data.
fn impact_profile(codec: &str, token: &str, stage: PipelineStage) -> [f32; 3] {
    let net = match codec_key(codec) {
        "AV1" => 0.38,
        "AVC" => 0.80,
        // Realtime vvenc at streaming presets keeps only part of VVC's paper
        // gain over HEVC; still the smallest stream of the four.
        "VVC" => 0.34,
        _ => 0.55,
    };
    let (gpu, cpu) = if is_hw(token) {
        if stage == PipelineStage::Decode {
            (0.55, 0.18)
        } else {
            (0.72, 0.15)
        }
    } else if matches!(codec_key(codec), "AV1" | "VVC") {
        (0.05, 0.96) // software AV1/VVC are the heaviest CPU paths
    } else {
        (0.06, 0.82)
    };
    [gpu, cpu, net]
}

/// Draw the mini 3-column impact chart (GPU / CPU / Network) — columns only; the
/// legend lives once on the tab. ~40×24px, bottom-aligned bars.
fn impact_bars(ui: &mut egui::Ui, imp: [f32; 3]) {
    let colors = impact_colors();
    let (rect, _r) = ui.allocate_exact_size(egui::vec2(40.0, 24.0), egui::Sense::hover());
    let p = ui.painter();
    let bw = 8.0_f32;
    let gap = 6.0_f32;
    for (i, &v) in imp.iter().enumerate() {
        let x = rect.left() + i as f32 * (bw + gap);
        let h = v.clamp(0.06, 1.0) * rect.height();
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, rect.bottom() - h),
            egui::pos2(x + bw, rect.bottom()),
        );
        p.rect_filled(bar, Rounding::same(1.5), colors[i]);
    }
}

/// The host's live video codec ("HEVC"/"AV1") from HostStatus; `None` if the
/// host isn't reachable (then nothing is marked active).
fn active_codec() -> Option<String> {
    crate::control::handle()
        .status()
        .and_then(|s| s.video_codec)
}

/// Wire-rate fps of the connected client, if any (shown on the active encoder).
fn active_fps() -> Option<u32> {
    crate::control::handle()
        .status()
        .and_then(|s| s.clients.iter().find(|c| c.connected).map(|c| c.fps))
}

/// The connected client's current coded `(w, h)` (if known) plus its selected
/// resolution ceiling and floor heights (`0` = Auto / no floor). Drives the
/// resolution control's live readout, target + min selectors, and the auto-drop
/// indicator.
fn active_coded() -> (Option<(u32, u32)>, u32, u32) {
    crate::control::handle()
        .status()
        .and_then(|s| {
            s.clients.iter().find(|c| c.connected).map(|c| {
                let coded = (c.coded_w > 0 && c.coded_h > 0).then_some((c.coded_w, c.coded_h));
                (coded, c.res_ceiling_h, c.res_floor_h)
            })
        })
        .unwrap_or((None, 0, 0))
}

/// The most recent successful benchmark report this session, if any.
fn last_bench_report() -> Option<LatencyReport> {
    bench_report()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|o| o.as_ref().ok().cloned()))
}

/// One card's latency/fps figure. `measured` distinguishes a live benchmark
/// value (bold, streaming colour) from a static reference estimate (a weak hint).
#[derive(Clone, Copy)]
struct CardMetric {
    latency_ms: f32,
    measured: bool,
    fps: Option<u32>,
}

/// Rough reference ENCODE latency (ms, 1080p60) per encoder token — shown when
/// no live benchmark is available for that card.
fn encode_reference_ms(token: &str) -> f32 {
    let t = token.to_ascii_lowercase();
    if t.contains("nvenc") {
        if t.contains("av1") {
            5.0
        } else {
            4.0 // hevc/h264 — same NVENC latency class
        }
    } else if t.contains("qsv") || t.contains("amf") || t.contains("vaapi") {
        6.0
    } else if t.contains("svtav1") || t.contains("aom") {
        30.0
    } else if t.contains("vvenc") {
        45.0 // software VVC (vvenc "faster", 1080p) — the slowest encode here
    } else if t.contains("x264") {
        12.0 // libx264 software — much lighter than x265
    } else {
        22.0 // libx265 software
    }
}

/// Rough reference DECODE latency (ms, 1080p60) per decoder token — the codec's
/// compute time, used when the client doesn't report a live `decode_ms`. This is
/// the codec turning bytes into a frame, NOT the display's panel latency (which
/// is a fixed hardware spec that lives after our last timestamp).
fn decode_reference_ms(token: &str) -> f32 {
    let t = token.to_ascii_lowercase();
    if t.contains("cuvid") || t.contains("nvdec") {
        2.0
    } else if t.contains("qsv") || t.contains("rkmpp") {
        3.0
    } else if t.contains("drm") || t.contains("v4l2") {
        4.0
    } else if t.contains("dav1d") {
        8.0
    } else if t.contains("av1") {
        7.0 // hw av1 decode
    } else if t.contains("vvc") || t.contains("vvdec") {
        18.0 // software VVC decode (ffmpeg native / vvdec) — heaviest here
    } else if t.contains("h264") {
        6.0 // software H.264 — the lightest software decode
    } else {
        10.0 // software
    }
}

/// Sort key that floats the active (0) then selected (1) card to the top of a
/// column; everything else is 2, keeping the prior best-first order. `want_hw`
/// gates "active" to hardware paths in the encoder column (the host runs NVENC).
fn float_rank(
    cap: &CapabilityUi,
    active_key: Option<&str>,
    pending_key: Option<&str>,
    want_hw: bool,
) -> u8 {
    let k = Some(codec_key(&cap.codec));
    // want_hw gates "active" to hardware paths in the encoder column, except
    // VVC — its only host encoder is software (libvvenc).
    let is_active = cap.available
        && k == active_key
        && (!want_hw || is_hw(&cap.token) || codec_key(&cap.codec) == "VVC");
    let is_pending = k == pending_key && k != active_key;
    if is_active {
        0
    } else if is_pending {
        1
    } else {
        2
    }
}

/// Build a card's per-step metric: measured (bold) when the card is live and the
/// benchmark reported a non-zero value for its stage; otherwise a reference
/// estimate (weak hint). `None` for unavailable cards.
fn card_metric(
    available: bool,
    is_active: bool,
    measured_stage: Option<f32>,
    reference_ms: f32,
    fps: Option<u32>,
) -> Option<CardMetric> {
    if !available {
        return None;
    }
    if is_active {
        match measured_stage.filter(|v| *v > 0.0) {
            Some(v) => Some(CardMetric {
                latency_ms: v,
                measured: true,
                fps,
            }),
            None => Some(CardMetric {
                latency_ms: reference_ms,
                measured: false,
                fps,
            }),
        }
    } else {
        Some(CardMetric {
            latency_ms: reference_ms,
            measured: false,
            fps: None,
        })
    }
}

/// The panel's pending codec selection (a click not yet reflected by the host).
fn read_pending(ui: &egui::Ui) -> Option<String> {
    ui.ctx()
        .data(|d| d.get_temp::<String>(egui::Id::new("video_pending_codec")))
}
fn write_pending(ui: &egui::Ui, codec: VideoCodec) {
    ui.ctx().data_mut(|d| {
        d.insert_temp(
            egui::Id::new("video_pending_codec"),
            codec.label().to_string(),
        )
    });
}

/// A small per-step latency (+ fps) figure on a card. A measured value (live
/// benchmark) is bold in the streaming colour; a reference estimate is a weak,
/// italic hint prefixed `~`.
fn metrics_line(ui: &mut egui::Ui, metric: Option<CardMetric>) {
    let m = match metric {
        Some(m) => m,
        None => return,
    };
    let lat = if m.measured {
        format!("{:.1} ms", m.latency_ms)
    } else {
        format!("~{:.0} ms", m.latency_ms)
    };
    let text = match m.fps {
        Some(f) => format!("{lat} · {f} fps"),
        None => lat,
    };
    ui.horizontal(|ui| {
        let rt = if m.measured {
            RichText::new(text)
                .small()
                .strong()
                .color(crate::theme::streaming())
        } else {
            RichText::new(text).small().weak().italics()
        };
        let desc = if m.measured {
            "Measured latency for this pipeline step (this stage only, not the whole chain)."
        } else {
            "Reference estimate for this step — run the benchmark for a measured value."
        };
        ui.label(rt).describe(desc);
    });
}

/// Connect the active encoder → codec → decoder cards with arrows, drawn over
/// the rendered columns in screen space.
fn draw_arrows(ui: &egui::Ui, rects: &[Option<egui::Rect>; 3]) {
    let stroke = Stroke::new(2.5_f32, crate::theme::streaming());
    let painter = ui.painter();
    let connect = |from: egui::Rect, to: egui::Rect| {
        let a = from.right_center();
        let b = to.left_center();
        painter.arrow(a, b - a, stroke);
    };
    if let (Some(e), Some(c)) = (rects[0], rects[1]) {
        connect(e, c);
    }
    if let (Some(c), Some(d)) = (rects[1], rects[2]) {
        connect(c, d);
    }
}

// ---------------------------------------------------------------------------
// One encoder / decoder option box
// ---------------------------------------------------------------------------

fn option_box(
    ui: &mut egui::Ui,
    cap: &CapabilityUi,
    is_active: bool,
    is_selected: bool,
    switchable: bool,
    metrics: Option<CardMetric>,
) -> (egui::Rect, Option<VideoCodec>) {
    // Snapshot the few visuals colours we need so we don't hold an immutable
    // borrow of `ui` across `frame.show(ui, …)` (which needs it mutably).
    let base_fill = ui.visuals().widgets.inactive.bg_fill;
    let weak_text = ui.visuals().weak_text_color();

    // Border: active (live path) → full streaming highlight; selected (pending
    // click) → an info-colour ring "glow"; else validated → green, available →
    // faint teal, unavailable → dim.
    let stroke = if is_active {
        Stroke::new(2.5_f32, crate::theme::streaming())
    } else if is_selected {
        Stroke::new(2.0_f32, crate::theme::info())
    } else if cap.validated {
        Stroke::new(2.0_f32, crate::theme::validated())
    } else if cap.available {
        Stroke::new(1.0_f32, crate::theme::available().gamma_multiply(0.8))
    } else {
        Stroke::new(1.0_f32, crate::theme::unavailable().gamma_multiply(0.5))
    };
    // A subtle fill tint reinforces active/selected without a literal glow.
    let fill = if is_active {
        crate::theme::streaming().gamma_multiply(0.10)
    } else if is_selected {
        crate::theme::info().gamma_multiply(0.08)
    } else {
        base_fill
    };

    let frame = egui::Frame::group(ui.style())
        .rounding(Rounding::same(10.0))
        .inner_margin(egui::Margin::same(10.0))
        .fill(fill)
        .stroke(stroke);

    let resp = frame
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());

            // Title row: label (+ ★ when validated) | perf chip far right.
            ui.horizontal(|ui| {
                let mut title = RichText::new(&cap.label).strong();
                if !cap.available {
                    title = title.weak();
                }
                ui.label(title);
                if cap.validated {
                    let star_desc = match &cap.validated_on {
                        Some(on) => format!("{} {on}", tr("video.box.validated_on")),
                        None => tr("video.box.validated_no_rig"),
                    };
                    ui.label(RichText::new("★").color(crate::theme::validated()).small())
                        .describe_named(tr("video.box.status.validated"), star_desc);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    perf_chip(ui, cap.perf_rank);
                });
            });

            ui.add_space(3.0);

            // Vendor · codec line.
            let meta = format!("{} · {}", cap.vendor, cap.codec);
            let meta_rt = if cap.available {
                RichText::new(meta).small().weak()
            } else {
                RichText::new(meta).small().weak().italics()
            };
            ui.label(meta_rt);

            // ffmpeg token, monospace, like the client's address line.
            ui.label(RichText::new(&cap.token).monospace().small().weak().color(
                if cap.available {
                    weak_text
                } else {
                    crate::theme::unavailable()
                },
            ));

            ui.add_space(5.0);

            // Status badge row + the mini 3-column impact chart (bottom-right).
            ui.horizontal(|ui| {
                let wash = ui.visuals().faint_bg_color;
                if is_active {
                    ui.label(
                        RichText::new("streaming")
                            .color(crate::theme::streaming())
                            .small()
                            .strong()
                            .background_color(wash),
                    )
                    .describe("This is the live path right now.");
                } else if is_selected {
                    ui.label(
                        RichText::new("selected")
                            .color(crate::theme::info())
                            .small()
                            .strong()
                            .background_color(wash),
                    )
                    .describe("Pending — the host is switching to this.");
                } else if cap.validated {
                    ui.label(
                        RichText::new(tr("video.box.status.validated"))
                            .color(crate::theme::validated())
                            .small()
                            .background_color(wash),
                    )
                    .describe(tr("video.box.status.validated_desc"));
                } else if cap.available {
                    ui.label(
                        RichText::new(tr("video.box.status.available"))
                            .color(crate::theme::available())
                            .small()
                            .background_color(wash),
                    )
                    .describe(tr("video.box.status.available_desc"));
                } else {
                    ui.label(
                        RichText::new(tr("video.box.status.unavailable"))
                            .color(crate::theme::unavailable())
                            .small()
                            .background_color(wash),
                    )
                    .describe(tr("video.box.status.unavailable_desc"));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                    impact_bars(ui, impact_profile(&cap.codec, &cap.token, cap.stage));
                });
            });

            // Live latency / fps for the active card.
            metrics_line(ui, metrics);
        })
        .response;

    // Whole-box description: a one-line role line, the probe detail, then the
    // validated rig. The probe `detail` is dynamic data (kept verbatim); the
    // static framing around it is translated.
    let role = if cap.stage == crate::model::PipelineStage::Decode {
        tr("video.box.decoder_desc")
    } else {
        tr("video.box.encoder_desc")
    };
    let mut tip = role;
    if !cap.detail.trim().is_empty() {
        tip = format!("{tip}\n{}", cap.detail);
    }
    if cap.validated {
        if let Some(on) = &cap.validated_on {
            tip = format!("{tip}\n★ {} {on}", tr("video.box.validated_on"));
        } else {
            tip = format!("{tip}\n★ {}", tr("video.box.validated_no_rig"));
        }
    }
    if switchable {
        tip = format!("{tip}\nClick to stream this codec.");
    }
    let rect = resp.rect;

    // Make the whole card a click target when it can drive a host codec switch.
    let mut clicked_codec = None;
    if switchable {
        let hit = ui.interact(
            rect,
            ui.make_persistent_id(("video_card", &cap.id)),
            egui::Sense::click(),
        );
        if hit.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if hit.clicked() {
            clicked_codec = switch_codec_for(&cap.codec);
        }
        hit.describe(tip);
    } else {
        resp.describe(tip);
    }

    ui.add_space(6.0);
    (rect, clicked_codec)
}

// ---------------------------------------------------------------------------
// Center codec column — selectable HEVC / AV1 / AVC / VVC cards.
// (Transport is always QUIC, so this column is about the codec on the wire.)
// ---------------------------------------------------------------------------

/// Render the four codec cards (HEVC / AV1 / AVC / VVC). All four are clickable
/// and drive a host codec switch (the host preflights and may refuse VVC). The
/// active codec (from `HostStatus`) gets the full highlight; a pending click
/// glows. Returns the active card's screen Rect so the caller can draw the
/// connector arrows.
fn codec_cards(
    ui: &mut egui::Ui,
    active_key: Option<&str>,
    pending_key: Option<&str>,
    clicked: &mut Option<VideoCodec>,
) -> Option<egui::Rect> {
    // (abbreviation key = the card headline, full standard name, multi.toml
    //  value, perf rank, switchable, one-line trade-off hint)
    let codecs: [(&str, &str, &str, u8, bool, &str); 4] = [
        (
            "HEVC",
            "H.265 · High Efficiency Video Coding",
            "hevc",
            1,
            true,
            "The default — best balance of quality, latency, and device support",
        ),
        (
            "AV1",
            "AOMedia Video 1",
            "av1",
            1,
            true,
            "−31% bitrate vs HEVC · needs Ada+ NVENC and an AV1-capable client",
        ),
        (
            "AVC",
            "H.264 · Advanced Video Coding",
            "h264",
            2,
            true,
            "+38% bitrate vs HEVC · decodes on virtually every device ever made",
        ),
        (
            "VVC",
            "H.266 · Versatile Video Coding",
            "vvc",
            5,
            true,
            "Smallest stream · SOFTWARE encode (CPU-heavy) — experimental",
        ),
    ];
    let mut active_rect = None;
    for (key, name, toml_value, rank, switchable, hint) in codecs {
        let is_active = active_key == Some(key);
        let is_selected = pending_key == Some(key) && !is_active;
        let stroke = if is_active {
            Stroke::new(2.5_f32, crate::theme::streaming())
        } else if is_selected {
            Stroke::new(2.0_f32, crate::theme::info())
        } else if switchable {
            Stroke::new(1.0_f32, crate::theme::available().gamma_multiply(0.8))
        } else {
            Stroke::new(1.0_f32, crate::theme::unavailable().gamma_multiply(0.5))
        };
        let fill = if is_active {
            crate::theme::streaming().gamma_multiply(0.10)
        } else if is_selected {
            crate::theme::info().gamma_multiply(0.08)
        } else {
            ui.visuals().widgets.inactive.bg_fill
        };

        let resp = egui::Frame::group(ui.style())
            .rounding(Rounding::same(10.0))
            .inner_margin(egui::Margin::same(10.0))
            .fill(fill)
            .stroke(stroke)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    // Headline: the codec ABBREVIATION (HEVC/AV1/AVC/VVC) — the
                    // name the rest of the stack (status line, logs, client
                    // sniffer) uses; the full standard name is the line below.
                    let mut t = RichText::new(key).strong().size(15.0);
                    if !switchable && !is_active {
                        t = t.weak();
                    }
                    ui.label(t);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        perf_chip(ui, rank);
                    });
                });
                ui.label(RichText::new(name).small().weak());
                ui.add_space(3.0);
                ui.label(RichText::new(hint).small().weak());
                ui.label(
                    RichText::new(format!("video_codec = \"{toml_value}\""))
                        .monospace()
                        .small()
                        .weak(),
                );
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    let wash = ui.visuals().faint_bg_color;
                    let (txt, col) = if is_active {
                        ("streaming", crate::theme::streaming())
                    } else if is_selected {
                        ("selected", crate::theme::info())
                    } else if switchable {
                        ("ready", crate::theme::available())
                    } else {
                        ("not a host option", crate::theme::unavailable())
                    };
                    ui.label(
                        RichText::new(txt)
                            .color(col)
                            .small()
                            .strong()
                            .background_color(wash),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                        // VVC has no hardware encoder anywhere — its bars show
                        // the software-encode reality, not a GPU path.
                        let token = if key == "VVC" { "software" } else { "hardware" };
                        impact_bars(ui, impact_profile(key, token, PipelineStage::Encode));
                    });
                });
            })
            .response;

        let rect = resp.rect;
        if is_active {
            active_rect = Some(rect);
        }
        if switchable {
            let hit = ui.interact(
                rect,
                ui.make_persistent_id(("video_codec_card", key)),
                egui::Sense::click(),
            );
            if hit.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if hit.clicked() {
                *clicked = switch_codec_for(key);
            }
            hit.describe(format!(
                "{key} ({name}) — click to stream this codec. {hint}."
            ));
        } else {
            resp.describe(format!(
                "{key} ({name}) — {hint}. Not a host encode option here."
            ));
        }
        ui.add_space(6.0);
    }
    active_rect
}

// ---------------------------------------------------------------------------
// Footer note
// ---------------------------------------------------------------------------

fn footer_note(ui: &mut egui::Ui) {
    ui.separator();
    ui.label(RichText::new(tr("video.footer_title")).small().weak())
        .describe(tr("video.footer"));
}
