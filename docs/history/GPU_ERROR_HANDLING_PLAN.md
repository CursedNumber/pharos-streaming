# Pharos GPU Error Handling — Design & Implementation Plan

**Status:** Designed 2026-06-24; **P1–P5 implemented + committed 2026-06-25** (commits
`300d7cf` P1 · `c246b6b` P2 · `2f2c3a2` P3 · `ff082e8` P4 · `4807606` P5) — builds,
clippy + rustfmt clean, unit tests green. ⚠️ **NOT yet live-validated end-to-end**: a
supervised host run with `PHAROS_FAULT_INJECT` and the real flaky **RTX 3070 Ti Strix**
(stable at idle, faults under sustained encode load — GDDR6X memory-junction heat the
prime suspect) remain the validation campaign. Minor deferrals: the in-panel "Try again"
button (reset-failed is wired into `launch-host.sh`), the explicit client "host recovering"
banner (clients already detect disconnect + resync off the ~1s IDR), and rung-2's in-stream
bitrate back-off actuation (detection/surfacing done; render-thread-serviced actuation is
the follow-up). Produced by a multi-agent design pass (audit → fault taxonomy → 3-angle
design panel → synthesis) followed by an adversarial review that caught two ship-blockers;
**both fixes are folded into this document** (see "Corrections from review").

## Why now / the load-bearing bug

Today a *single transient* NVENC return code kills the whole host. At
[multi.rs:1595-1600](crates/compositor/src/multi.rs:1595) the encode path is:

```rust
p._session.encode_picture(&mut p.reg, ob, Default::default()).map_err(|e| anyhow!("encode [{}]: {e:?}", p.name))?;
let lock = ob.lock().map_err(|e| anyhow!("lock [{}]: {e:?}", p.name))?;
```

Every `ErrorKind` unwinds the loop identically via `?`, so a `NeedMoreInput`/`EncoderBusy`
— which NVIDIA documents as "retry in a few ms, not fatal" — terminates the daemon. The
existing `3×>200ms` wedge counter ([multi.rs:1614](crates/compositor/src/multi.rs:1614))
only *logs* "NVENC LIKELY WEDGED — restart pharos-host" and takes no action. There is no
GPU-health field on `HostStatus`, so the tray/panel are blind to faults. This plan turns
all of that into detect → classify → recover → surface.

## Hard invariants (every rung obeys these)

1. **Render loop must not block.** Only a `classify()` match, a frame drop (`Ok(None)`),
   relaxed atomic stores, an optional in-loop `reconfigure_bitrate`, and one cheap
   `sd_notify(WATCHDOG=1)` are allowed on the hot path. All slow work runs on the detached
   `pharos-gpu-watchdog` thread.
2. **No in-process NVENC re-init.** `rebuild_pipeline_session`
   ([multi.rs:351](crates/compositor/src/multi.rs:351)) stays dead-code — re-initialising
   NVENC in a long-running process SIGSEGVs libnvcuvid. The *only* in-place lever is the
   vendored `reconfigure_bitrate` (RC-only, `set_resetEncoder(0)`, proven crash-free); the
   *only* full rebuild is a **fresh process** via `systemd Restart=on-failure`.
3. **Single-client slot never wedges.** A fault tears the whole process down (slot freed
   wholesale); a faulted client is released cleanly before exit.
4. **Reconnect rides the existing ~1s IDR** — never an NVENC rebuild on reconnect.
5. **GPU health is surfaced** over the existing control-ipc `HostStatus` to tray + panel +
   client. Never silent.

## Architecture: one health plane, three detectors, one ladder

```
  RENDER LOOP (hot, must not block)            DETACHED pharos-gpu-watchdog (slow work)
  ┌──────────────────────────────┐            ┌────────────────────────────────────────┐
  │ encode_picture()/lock()      │            │  • heartbeat: last_frame_ns stale>1.5s   │
  │   └─ classify(kind)──► A.in-band            │      ⇒ wedge confirmed                   │
  │      Transient→Ok(None)+count│ ──atomics──►│  • NVML sampler: throttle/temp/power/    │
  │      Degraded→set flag       │  GpuHealth  │      clocks/pcie_replay (ring buffer)    │
  │      Fatal→set flag+exit-req │  +last_ns   │  • kmsg/journalctl -k Xid scraper        │
  │ 3×>200ms in-loop wedge ctr   │  +reconfig_ │  • DECIDES rungs; sets atomics; captures │
  │ stamp last_frame_ns each iter│   pending   │      post-mortem; requests process exit  │
  │ sd_notify(WATCHDOG=1) if live│  +req_kbps  │  • NEVER calls an NVENC API (see fix #1) │
  │ services reconfigure req here │            │                                          │
  └──────────────────────────────┘            └────────────────────────────────────────┘
                       │ (written in the existing ~4Hz status.lock block, multi.rs:1991)
                       ▼
        control-ipc HostStatus { gpu_health, last_xid, throttle, t_into_stream }
              │                    │                         │
            TRAY               PANEL                       CLIENT
       green/amber/red    Overview badge + fault       Trireme "host recovering"
       (sticky)           banner + diagnostics +        banner via ConnStatus;
                          "Try again"                   resync off the ~1s IDR
```

### Three detection planes (all ~80% already present)

- **A — in-band return-code classifier (the one new render-path seam).** Replace the two
  bare `?` with `classify(kind: ErrorKind) -> Severity`. Wrap the CUDA bridge sites
  ([multi.rs:1568](crates/compositor/src/multi.rs:1568)) the same way for sticky-CUDA.
- **B — cadence.** Keep the existing `>50ms` warn / `3×>200ms` wedge fingerprint verbatim;
  only its *action* changes (set health, not log-and-reset). Add an `AtomicU64
  last_frame_ns` stamped each iteration — the **only** detector for a fully-hung blocking
  `encode_picture` that never returns. `measured_fps_ewma` is a third corroborating signal.
- **C — NVML + Xid sampler (diagnostic spine, watchdog-only).** Continuous NVML ring
  (throttle reasons, temp, power, clocks, `pcie_replay` delta, pstate) + `/dev/kmsg`
  (`journalctl -k`) tail for `NVRM: Xid (PCI:...): N` + AER/`IO_PAGE_FAULT`. Runs live so
  the seconds-before-fault context is *captured, not reconstructed*. **kmsg is
  authoritative for Xid 79/62** (NVML hangs once the card is gone); NVML and the heartbeat
  run on **separate timers** so an NVML hang can't blind the wedge detector.

### Classification → three severities

- **TRANSIENT** (never blanks the stream; health stays Healthy, only a rate counter moves):
  NVENC `NeedMoreInput(17)` / `EncoderBusy(18)` / `LockBusy(13)`; self-clearing thermal/
  power **throttle** (a bitmask, surfaced as a *distinct amber "throttling" sub-state*, fed
  to the adaptive controller — NOT a fault); CUDA non-sticky OOM verified by a probe alloc.
- **DEGRADED** (still encoding but suspect; amber, serve at reduced quality, no restart):
  *sustained* `EncoderBusy` rate or *N×>200ms within a window* (see fix #4 — never a single
  spike), rising `pcie_replay`/AER (Xid-79 early warning), recurring Xid 13/31/32/56/68
  under steady load, single uncontained ECC.
- **FATAL** (session poisoned or device gone; the *only* rung that exits the process):
  NVENC `DeviceNotExist(5)`/`NoEncodeDevice(1)`/`InvalidDevice(4)`/`OutOfMemory(10)`/
  `Generic(20)`/`EncoderNotInitialized(11)`; confirmed wedge (`3×>200ms` or heartbeat
  stale `>1.5s`); **sticky CUDA** (`ILLEGAL_ADDRESS 700`, `ECC_UNCORRECTABLE 214`,
  `LAUNCH_TIMEOUT 702`, `DEVICE_UNAVAILABLE`) — `cudaDeviceReset` does not clear it; kernel
  Xid 43/45/48. **TERMINAL-HARD sub-class:** Xid 79 (fell off bus) / 62 (GSP/PMU halt) —
  won't init even in a fresh process until cold boot ⇒ skip the restart loop, go sticky-red.
  Conservative default: `Generic`/unknown ⇒ Fatal.

**3070 Ti hardware-vs-software discriminator (baked into the post-mortem):** the *same* Xid
under the *unchanged* load that *clears when power-capped/downclocked* ⇒ degrading hardware,
not a Pharos bug. `HwPowerBrakeSlowdown` rising right before Xid 79 = power-delivery
fingerprint. Encode-stall trip + Xid 43 in one window = confirmed engine hang.

## Recovery ladder

| Rung | Trigger | Action | Invariant note |
|---|---|---|---|
| **0** | Host startup, before advertising a session | **Probe-gate:** encode one IDR per configured codec; refuse to advertise a codec that won't produce a clean bitstream; if the preferred fails, demote HEVC↔AV1 *before going live* and record the active codec in `HostStatus` (never silent). Snapshot NVML baseline into the first log + ring. | Runs before the render loop exists; per-codec probe sessions are built **and dropped entirely at cold startup** (fix #6), never live. |
| **1** | `classify()==TRANSIENT` (NeedMoreInput/EncoderBusy/LockBusy), self-clearing throttle, or verified non-sticky CUDA OOM | Skip emit for **this** pipeline this tick, `return Ok(None)`. For EncoderBusy: arm a forced IDR on the next good frame + bump a busy-rate counter. Throttle → adaptive controller + amber "throttling". Health stays Healthy. **This is the daemon-killing bug-fix.** | Render-path work = a frame drop + counter + `Ok(None)`. No session touched. |
| **2** | `classify()==DEGRADED` or busy-rate/slow-encode crosses a **windowed** threshold | Watchdog sets `requested_avg_kbps` + `reconfigure_pending`; **the render thread services it** (fix #1) by calling `reconfigure_all_bitrate(...)` — the in-place RC lever — backing bitrate down a step + forcing an IDR in one call. `gpu_health=Degraded(reason)` → amber. Stream continues at reduced quality. | In-place `reconfigure_bitrate` only (crash-free); the call runs **on the render thread**, same as the existing in-loop reconfigure ([multi.rs:337](crates/compositor/src/multi.rs:337)). |
| **3** | Health → FATAL with a client attached, just before exit | Notify the single client over the existing `ConnStatus`/`Preempted` plumbing ("host GPU fault — restarting") so it shows a banner and prepares to resync; release the slot cleanly. | Worker-side notify, not render-path. Slot freed wholesale by the teardown. |
| **4** | `classify()==FATAL` (non-terminal-hard) | **Fresh-process restart.** Render loop already set `Faulted` + returned `Ok(None)`. Watchdog: (a) capture post-mortem to a durable best-effort file; (b) notify+release client; (c) for a *responsive* loop reuse the proven `drop(pipelines)` + ~1500ms settle + `reexec_self()` so NVENC releases via Drop **before** re-init (fix #3); for the *hung* case, request a clean non-zero exit and let `systemd Restart=on-failure` respawn, with a short init retry/backoff on `Encoder::initialize_with_cuda` to ride the concurrent-session-cap window. Add `StartLimitBurst=5`/`StartLimitIntervalSec=60`. | A brand-new **process** is the only full rebuild — the explicit libnvcuvid-SIGSEGV dodge. Reconnect rides the ~1s IDR. |
| **5** | Breaker trips (5 re-faults/60s) **or** terminal-hard Xid 79/62 | **Fail loud and stop.** Don't auto-retry a card that dies on every start. Let the unit settle to systemd `failed`. Persist sticky `Faulted('GPU lost — cold boot required')` to the durable file first → tray sticky-red "GPU faulted — host stopped" (distinct from bare Idle), panel shows the full power/thermal/Xid trail. Panel "Try again" resets the breaker. | No init attempted on a gone device. Entirely worker/systemd-side. |

## Corrections from the adversarial review (folded in above)

1. **BLOCKER — the watchdog must never call an NVENC API.** `Session` is `!Send + !Sync`
   (raw `*mut c_void`) and the `Pipeline` is self-referential (`Box<Session>` with
   `'static`-transmuted `bitstreams`/`reg` borrows pointing into it), owned solely by the
   render thread. Rung 2 is therefore **render-thread-serviced**: the watchdog only sets
   `AtomicU32 requested_avg_kbps` + `AtomicBool reconfigure_pending`; the render loop checks
   the flag each iteration and calls `reconfigure_all_bitrate` itself. This kills a
   cross-thread `NvEncReconfigureEncoder` vs `NvEncEncodePicture` data race.
2. **BLOCKER — external recovery for a full driver hang.** A real fault can hang *both* the
   render thread (in `encode_picture`) and the watchdog (NVML). With no external watchdog,
   systemd sees a live process and never restarts → dark forever. Fix: `Type=notify` +
   `WatchdogSec=5` on the transient unit, and the **render loop calls
   `sd_notify(WATCHDOG=1)` once per iteration** (gated to "actively streaming" so an idle
   host doesn't trip it). systemd force-kills externally, independent of any thread.
3. **MAJOR — drop the "pgrep-reap releases the prior session" claim.** `Restart=on-failure`
   re-execs `ExecStart` directly, not `launch-host.sh`, so the reap never runs on
   auto-restart. Use graceful `drop` + settle + `reexec_self()` for responsive exits; for
   the hung case rely on process death + an init retry/backoff. **Validate on the real 3070
   that a fresh process re-acquires NVENC within the StartLimit window.**
4. **MAJOR — windowed degrade, not single-spike.** A lone `>200ms` is normal (GC, a heavy
   I-frame, a compositor hitch). Require N×>200ms in a sliding window (or sustained
   EncoderBusy rate) before Healthy→Degraded; keep single spikes Healthy (counter only) +
   hysteresis, or a healthy 4070/3070 ratchets its own bitrate down on noise.
5. **MAJOR — harness injects on the `pub` `ErrorKind`, not a fabricated `EncodeError`**
   (`EncodeError::new` is crate-private). `classify()` operates on `ErrorKind`; the
   injection seam reads `PHAROS_FAULT_INJECT` at the classify call site.
6. **MINOR** — per-codec probe sessions are built+dropped entirely at cold startup; FATAL
   `OutOfMemory(10)` will lack a driver string (it's in the vendored skip-list) — document
   the gap; don't conflate CUDA OOM(2) with NVENC OOM(10); don't bump `PROTOCOL_VERSION`
   for a `serde(default)` additive field (read the Hello handler first).

## Phased implementation plan

- **P1 — Encode-error classifier (the bug-fix). LOW risk.** `crates/compositor/src/multi.rs`.
  Add `enum Severity` + `classify(ErrorKind)`; replace the two bare `?` at 1595-1600 and the
  CUDA bridge sites at 1568-1581; Transient → `continue`/skip-emit + arm IDR; Degraded/Fatal
  → a `handle_fatal()` stub (still returns `Err` for now, no regression). Ships the single
  highest-value fix: transients no longer kill the host.
- **P2 — Additive `gpu_health` on `HostStatus` + tray/panel consumers (inert until set).
  LOW.** `control-ipc`, `pharos-tray`, `pharos-panel`. `enum GpuHealth { Healthy, Degraded,
  Faulted }` (`#[serde(default)]`); tray `Degraded`(amber)/`Faulted`(sticky-red SVG) states;
  panel `Liveness` variants + banner + "Try again" placeholder + i18n.
- **P3 — In-loop health state-machine + heartbeat stamp; rewire the wedge counter. LOW-MED.**
  `multi.rs`. `static GPU_HEALTH: AtomicU8` + `LAST_FRAME_NS: AtomicU64` + `Mutex<FaultRecord>`;
  stamp the heartbeat each iter; `3×>200ms` branch sets Faulted; windowed degrade + hysteresis;
  write `st.gpu_health` in the existing `status.lock()` block.
- **P4 — Detached `pharos-gpu-watchdog` thread (heartbeat + NVML + kmsg) + render-serviced
  degrade. MEDIUM.** `multi.rs`, `compositor/Cargo.toml` (`nvml-wrapper`, best-effort). Three
  independent timers; watchdog sets atomics only; render thread services the reconfigure
  request (fix #1). NVML/kmsg best-effort/feature-guarded. **Also wire `sd_notify(WATCHDOG=1)`
  + `Type=notify`/`WatchdogSec` (fix #2).**
- **P5 — FATAL fresh-process path: clean exit, durable fault file, systemd breaker, client
  notify. MED-HIGH.** `multi.rs`, `rig/launch-host.sh`, `pharos-tray`, `pharos-panel`,
  `control-ipc`. Real `handle_fatal()`; post-mortem file; `StartLimitBurst/IntervalSec`; sticky
  terminal-hard handling; tray reads the durable file when the control plane is down; "Try
  again" → `systemctl --user reset-failed`.
- **P6 — Windows (pharos-win) thinner ladder. LOW-MED, build/test on the rig only.**
  `pharos-win/src/encode.rs`, `main.rs`. ffmpeg stderr `piped()` + scan (broaden to
  DXGI/D3D11/AMF device-loss strings, fix #6); bounded subprocess respawn with backoff (dodge
  TDR 0x117); `gpu_health` over TCP; document `TdrDelay`/`TdrLevel=3`. No NVML, no in-place
  degrade — partial parity by construction.

## Fault-injection harness (validates each rung)

Two modes. **(A) the real RTX 3070 Ti** under sustained load: standard 4K/high-fps config +
a Trireme client + a stacked load (busy full-motion source ± a `gpu-burn`/`memtest_vulkan`
co-tenant) for a long soak; the watchdog's NVML ring + kmsg tail *is* the instrument
(confirm seconds-before-fault telemetry is recorded). Characterize by re-running with
`nvidia-smi -pl <lower>` / clock pin / undervolt — same Xid that clears when power-capped =
degrading hardware. **(B) software injection** via `PHAROS_FAULT_INJECT=busy|need_more|
wedge|device_lost|cuda_sticky|xid79` at the `classify()` seam (synthetic `ErrorKind`, fix
#5) + a `sleep(250ms)`/permanent block for the two wedge detectors + a synthetic Xid line
into the kmsg parser (temp file in test mode) + a re-firing fault to exercise the breaker —
so recovery is testable on the stable 4070 and in CI with no faulty card.

Per-rung pass/fail criteria are enumerated per rung above (e.g. Rung 1 PASS = stream never
blanks + host stays up + Healthy + IDR fills the gap; Rung 4 PASS = no in-process re-init +
post-mortem written + clean non-zero exit + systemd respawn + client resync off the IDR).

## Open questions

- kmsg/Xid access under the `pharos-host` *user* unit — `/dev/kmsg` is often root/`adm`-gated;
  may need `journalctl -k`. If neither, the Xid plane degrades to NVML-only (terminal-hard
  79/62 detection depends on kmsg being authoritative when NVML hangs).
- `WatchdogSec` value vs the longest legitimate frame under heavy load (don't false-kill).
- `StartLimitBurst` tuning vs the 3070 Ti's real time-to-fault (measure from harness runs).
- Forced-IDR mechanism: is there a per-frame force-IDR pic-param, or is the only IDR lever
  the `reconfigure_bitrate` round-trip?
- NVML on a non-NVIDIA host: separate Cargo feature vs runtime `Result`-guard (don't break a
  build on a box without NVML).
- Durable fault-file path (`$XDG_RUNTIME_DIR` is tmpfs, cleared on reboot — correct for
  "cold boot required" stickiness).
- P6 Windows fault-injection: the only Windows rig is the AMD RX 570 (.213) — no NVENC/NVML,
  emits AMF/DXGI device-loss strings, can't exercise the CUDA branch; real-TDR validation is
  deferred until an NVENC-capable Windows rig exists (e.g. the 3070 Ti box).
