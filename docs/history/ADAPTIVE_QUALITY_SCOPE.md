# Network-aware auto-quality: what we have & how to wire it

> Scoping inventory generated 2026-06-23 (6-agent sweep of the tree). Motivated by the
> WAN test: over a marginal LTE link the host pushed ~85 fps / ~11 Mbps while the client
> decoded ~14 fps — because the only adaptive loop (W8 auto-pacer) is **compute-bound and
> network-blind**, and congestion is absorbed passively by drop-newest queues.

**Scope:** make Pharos/Trireme react to the *path* (RTT/loss/client-recv-fps), not just host GPU cost.

---

## 1. Signals we already have

### Host-side
| Signal | Where | Status |
|---|---|---|
| QUIC `write_all()` wall-clock backpressure (write_ms>200 warn) — the only *direct* host congestion signal | `crates/transport/src/lib.rs:1243` (write_t0), `:1254-1258`, `:1274-1290` | **Log-only.** Stranded on quic-streamer thread; never reaches pacer/status. |
| Drop-newest bounded video queue full = backlog forming | `crates/transport/src/lib.rs:424`, `:496-503`, `:514-519`; audio `:437/:527` | Drops silently discarded (`let _ =`). **Uncounted, unlogged.** |
| Measured **wire fps** (frames actually pushed to `send`) + EWMA(α0.3) | `crates/compositor/src/multi.rs:1520`, `:1550-1557`, log `:1575-1579` | Surfaced as `ClientStatus.fps` (`multi.rs:1661`). Conflates GPU vs net; **not fed to pacer**. |
| Wire **bytes/1s** (encoder egress rate) | `crates/compositor/src/multi.rs:1521`, reset `:1582`, log `:1578` | Not in any status struct; not used by pacer. |
| quinn `Connection::stats()` / `PathStats{rtt, cwnd, lost_packets, congestion_events, black_holes}` | available on live `conn` at `crates/transport/src/lib.rs:874`, send loop `:1201-1307` | **NEVER read** — zero matches repo-wide. Single biggest free signal. |
| Clock-burst RTT (min/max) | `crates/transport/src/lib.rs:1727-1822`; `DG_CLOCK_PING/PONG` `:1145-1169` | Bench-only, one-shot, host-triggered. Inert in steady state. |
| Connected/preempted liveness (binary) | `crates/transport/src/lib.rs:1003`, `:1201-1217`, idle/keepalive `:84-87` | All-or-nothing; keepalives **mask** a slow-but-alive peer. |

### Client-side
| Signal | Where | Status |
|---|---|---|
| Android `DecoderStats`: queueDepth/capacity, drops, decodeLagMs (wall-clock PTS), fps, pace | `client/androidApp/.../stream/HevcDecoder.kt:78-90`, build `:432-453`, lag `:395-398/:434-437`; HUD `ui/DebugHud.kt:98` | **DEBUG-variant only**, dead-ends at `StateFlow` (`StreamHub.kt:101-102`, `:667-672`). Never hits QUIC. decode-lag fix = commit `a6290ca`. |
| Desktop `LatencyTaps`: decode-stage + present-stage ms | `crates/trireme/src/play.rs:2056-2058`, `:1576-1578`; taps `crates/transport/src/lib.rs:644-740` | Pharos-QUIC only (VNC/SPICE = None). Accumulate continuously but **read only during bench**. |
| `WireLatencyReport` (rtt_ms + per-stage) — the one client→host upstream msg | `crates/core-proto/src/latency.rs:77-93`; built `transport/src/lib.rs:1832-1894`; sent `:1917-1922` (`DG_LATENCY_REPORT` 0x05) | **One-shot bench, host-initiated** (fires on `DG_FRAME_META`). Carries **no loss/drop/queue-depth**, only RTT + stage timings. |

**Key fact:** there is **no per-frame sequence number** on the video wire — AUs are bare `u32-len‖bytes` (`transport/src/lib.rs:1255-1256`). Loss is currently *unobservable* at the app layer; `DecoderStats.drops` counts **local FIFO eviction**, not network loss.

---

## 2. Actuators we can drive live

| Actuator | Where | Live vs needs re-init |
|---|---|---|
| **FPS pacing** (`SetFps{auto,fps}`) | handler `crates/compositor/src/multi.rs:1136-1156`; pacer `:1562-1568/:1608-1622` | **LIVE.** Mutates `auto_pace/fps_cap/target_fps/current_pace`; pacer-sleep only. Clamp ceiling = boot `cfg.fps` (can lower, can't exceed). Idle SetFps rejected (`control.rs:903-909`). Commit `444d30c`. |
| **W8 auto-pacer** self-tune | state `multi.rs:790-802`; loop `:1558-1584`; `work_us` `:1613-1614` | **LIVE** but **compute-only** — `sustainable=1e6/(work_us_ewma*1.18)`, clamp[24, fps_cap]. Zero network term. Commit `08e0123`. |
| **Bitrate / RC mode** (CBR/capped-VBR, avg/max/VBV) | `resolve_rc_bitrate multi.rs:297-305`; apply `:487-512`; cfg `:95-106`; panel slider 2-80Mbps `pharos-panel/src/tabs/settings.rs:201` | **NEEDS RE-INIT today.** Written onto preset **before** `start_session`, then frozen. No live `SetBitrate`, no `NvEncReconfigureEncoder` call (grep reconfigure = 0). NVENC *can* live-change RC (no maxEncode envelope needed) — wrapper just unbuilt. Commit `e026924`. |
| **GOP / IDR period** (=fps, 1s) | `multi.rs:484-486`; `apply_codec_lowlatency:267-290` | **INIT-ONLY.** Reconnect resync relies on periodic IDR (`:1239-1247`); per-frame forced IDR unreachable under `enablePTD` (`:1236-1238`). |
| **Resolution** | `multi.rs:447-461`, `EncoderInitParams::new(guid,oc.w,oc.h):513` | **NEEDS RE-INIT — double-blocked.** No `maxEncodeWidth/Height` envelope at create (builder has no setter) + no reconfigure wrapper. Per `nvEncodeAPI.h:4141` res-change requires the envelope. This is HDR_VRR_PLAN W7. |
| **Codec swap** (HEVC↔AV1) | `control.rs:129-132/:656-681`; `multi.rs:1119-1134`; `reexec_self control.rs:1015-1035` | **Full relaunch.** NVENC can't change codec in place. |
| Passive load-shed (drop-newest queues) | `transport/src/lib.rs:415-438`, `:496-528` | The *entire* current congestion "reaction" — discards, never lowers quality. Commits `a322c23`, `f913848`. |

**Hard blocker:** `NvEncReconfigureEncoder` has only a raw FFI ptr in the vendored SDK (`nvidia-video-codec-sdk-0.4.0/src/safe/api.rs:143`, loaded `:257`) — **no safe wrapper** on Session/Encoder. The only in-tree "rebuild session" path `rebuild_pipeline_session` (`multi.rs:307-385`) is `#[allow(dead_code)]` and tagged **"re-init crashes the NVIDIA driver"** (lesson-nvenc-driver-crash). Any new-session change must go through `reexec_self` (drops stream ~seconds) — unusable as a fast reaction.

---

## 3. Control plumbing that already exists

- **`SetFps` trifecta** = the exact template for a new live knob (additive, no `PROTOCOL_VERSION` bump, not persisted):
  `Request::SetFps{auto,fps}` (`control-ipc/src/lib.rs:144`) → serve handler (`control.rs:682-705`) → `ControlCmd::SetFps` (`control.rs:133-139`) → render-loop drain (`multi.rs:1136-1156`) → Ack.
- **W8 control tick** = the only periodic control point in the render loop, runs 1×/s inside the wire window (`multi.rs:1558-1584`), owns `target_fps/current_pace/fps_floor(24)/fps_cap`.
- **Latency back-channel intake (bench-scoped):** `take_latency_rx` (`transport/src/lib.rs:576-580`, `multi.rs:879-884`) drained at `multi.rs:1209-1216` → `wire_report_to_control` → panel oneshot (`control.rs:1040-1056`).
- **`HostStatus`/`ClientStatus` 1Hz push** (`multi.rs:1632-1669`, `control.rs:346-352`; struct `control-ipc/src/lib.rs:256-281`) — observability channel, currently carries only `fps` + `video_codec`.
- **Panel surfaces:** live pacing row (`pharos-panel/src/tabs/video.rs:288`, `:306-335`, readback `active_fps() :1071`); bitrate/window/queue sliders (`settings.rs:191-247`) — but those **only emit env at launch** (`model.rs:658-695`, `data.rs:312-316/:459-461`), no control-ipc round-trip. Panel `bitrate_kbps` (`model.rs:659-660,691`, default 20000) is a **dead-end UI control** (HDR_VRR_PLAN.md:233).

---

## 4. The gaps to close the loop

1. **No network signal reaches any controller.** Pacer keys off `work_us_ewma` only (`multi.rs:1558-1574`); a lossy-but-fast-GPU path gets zero response. (HDR_VRR_PLAN.md:239 self-flags this.)
2. **quinn PathStats never read** — free RTT/cwnd/loss available at `conn` (`transport/src/lib.rs:874`), unused.
3. **Backpressure signal stranded** — `write_ms` is a log line (`lib.rs:1282`), no accessor/counter/EWMA/channel to the compositor thread.
4. **Drops uncounted** — queue-full evictions (`lib.rs:499/527`) thrown away; cheapest congestion proxy lost.
5. **No continuous client telemetry** — `WireLatencyReport` is bench-only/one-shot and carries **no loss/queue-depth**; Android stats never leave the device.
6. **No wire seq → loss invisible** — bare-length AU framing (`lib.rs:1255-1256`).
7. **Two of three levers have no live path:** bitrate = init-only; resolution = init-only + double-blocked. Only **fps** is live.
8. **Signals split across threads, no bridge** — net truth on quic-streamer thread, pacer on render thread; only cross-thread signal is `AtomicBool connected` (binary).
9. **No status fields** for the controller's decisions in `HostStatus` (`control-ipc/src/lib.rs:256-281`).

---

## 5. Minimal wiring plan

Goal: smallest loop reading **{RTT, loss, client-recv-fps}** → actuating **{fps via SetFps (now), bitrate via NVENC reconfigure (next)}** with hysteresis. Fps first (side-effect-free live lever); bitrate second (needs the unsafe wrapper).

**Step 0 (smallest first step — start here): expose host-side network signal with zero wire/protocol change.**
- In the send loop `crates/transport/src/lib.rs:1201-1307`, add a ~250ms–1s timer arm calling `conn.stats().path.{rtt, lost_packets, sent_packets, cwnd, congestion_events}`.
- Record `write_ms` EWMA at the existing completion arm `lib.rs:1274-1290`; increment an `Arc<AtomicU64>` drop counter at the `try_send` Err branches `lib.rs:499/:527` (replace `let _ =`).
- Publish via a new `net_stats_rx` watch/bounded channel on `QuicStreamer`, mirroring `latency_rx` (`lib.rs:385`, `:576-580`); add `QuicStreamer::take_health()` next to `is_connected()` (`lib.rs:551`).
*This alone gives continuous RTT+loss+drops with no client changes and no protocol bump — independently shippable as observability.*

**Step 1: drain it in the render loop + surface it.**
- Drain `net_stats_rx` non-blockingly alongside the existing latency drain at `multi.rs:1209-1216`.
- Add fields to `HostStatus` (`control-ipc/src/lib.rs:256-281`) at the push site `multi.rs:1632-1669`: measured loss, RTT, current target fps/bitrate, throttle reason → panel can render it (`video.rs:288`).

**Step 2: inject the network term into the pacer (fps actuation, fully live).**
- At `multi.rs:1562-1568`, change the clamp to `target_fps = sustainable.min(network_allowed_fps).clamp(fps_floor, fps_cap)`.
- Add controller state next to pacer vars (`multi.rs:790-805`): loss EWMA, current bitrate setpoint, last-raise timestamp.
- **Hysteresis:** step **down fast** (1 tick on loss>threshold or write_ms spike), **raise slow** (N consecutive clear ticks + cooldown). No new IPC — reuses the live SetFps path (`multi.rs:1136`).

**Step 3: client-recv-fps into the signal (small wire add).**
- Add a continuous `DG_CLIENT_QOS` datagram tag beside `transport/src/lib.rs:745-759`; client sends on a timer regardless of bench (today only inside bench `:1620-1671`); plumbing identical to `send_latency_report` (`:1917-1922`).
- Payload from already-computed metrics: Android `DecoderStats` (`HevcDecoder.kt:432-453`, dead-ending at `StreamHub.kt:101-102`), desktop `LatencyTaps` (`play.rs:2056/:1577`). New msg next to `WireLatencyReport` (`core-proto/src/latency.rs:77-93`); carry queue_depth, drops, decode_lag_ms, present_fps.

**Step 4: bitrate actuator (bigger lift — gated on the wrapper).**
- Hand-roll an unsafe wrapper around `reconfigure_encoder` (`nvidia-video-codec-sdk-0.4.0/src/safe/api.rs:143`) building `NV_ENC_RECONFIGURE_PARAMS` (RC-only → **no** maxEncode envelope needed, spike-gated per HDR_VRR_PLAN.md:299-301).
- Add `Request::SetBitrate{kbps}` cloning the SetFps trifecta; re-drive `resolve_rc_bitrate` (`multi.rs:297-305`) and refresh `vbvBufferSize` against the **live** target fps (init math `multi.rs:487-512` assumes boot `cfg.fps`).
- **Wire `bitrate ↔ PHAROS_STREAM_WINDOW_KB`** (`transport/src/lib.rs:82-106`): window/bitrate = buffered latency — dropping bitrate without shrinking the window grows latency.

*(Resolution = out of scope for the minimal loop; HDR_VRR_PLAN W7, double-blocked. Never call dead `rebuild_pipeline_session` `multi.rs:307-385` — it SIGSEGVs.)*

---

## 6. Open design questions

1. **Reliable uni-stream-per-frame vs datagram+FEC.** Today video = reliable QUIC stream framing `u32-len‖bytes` (`transport/src/lib.rs:1255-1256`) behind a fixed flow-control window (`:82-106`) → head-of-line blocking on loss is exactly what parks `write_all` (`:1243-1290`). Keep reliable streams (let the controller lower rate) or move video to **unreliable datagrams + FEC**? Adding a seq header (gap #6) is a prerequisite for *measuring* loss either way.
2. **Where does the controller live?** Co-locate in the **W8 render-loop tick** (`multi.rs:1558-1584`) — single-threaded/race-free with the 1Hz aggregation, already owns fps/pacer state. Bridge channel (Step 0) feeds it transport-side signal.
3. **How does client telemetry reach the host?** Continuous `DG_CLIENT_QOS` datagram vs host-only quinn PathStats. PathStats = host-path truth for free (ship first); client QoS = actual viewer experience (decode queue, present jitter, drops) PathStats can't see (add next). Decide cadence (1Hz vs 250ms) + whether to add loss-bearing seq to AU framing.
4. **Short-GOP / intra-refresh for loss recovery.** GOP/IDR init-only at 1s (`multi.rs:484-486`); per-frame forced IDR unreachable under `enablePTD` (`:1236-1238`). If we move to lossy datagrams, intra-refresh or shorter GOP avoids a full second of corruption per loss. Interacts with the reconfigure wrapper (forceIDR) and bitrate (more I-frames = higher rate). Open until Q1 lands.

---
**Implementer files:** `crates/transport/src/lib.rs`, `crates/compositor/src/multi.rs`, `crates/compositor/src/control.rs`, `crates/control-ipc/src/lib.rs`, `crates/core-proto/src/latency.rs`, `crates/trireme/src/play.rs`, `client/androidApp/.../stream/{HevcDecoder.kt,StreamHub.kt}`, `crates/pharos-panel/src/tabs/{video.rs,settings.rs}`, `crates/pharos-panel/src/model.rs`, vendored `nvidia-video-codec-sdk-0.4.0/src/safe/api.rs`.
**Commits:** `444d30c` (SetFps live pacing), `08e0123` (W8 auto-pacer), `e026924` (W3b RC), `a6290ca` (decode-lag fix), `a322c23`/`f913848` (bounded queues). **Plan refs:** `HDR_VRR_PLAN.md:233/237/239/299-301`, W7.
</content>
