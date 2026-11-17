# Pharos Desktop Panel — TODO / Improvement Suggestions

This file is the shared deposit box for the UX build of **`pharos-panel`** (the
cross-platform Pharos host control app, egui/eframe). Suggestions were collected
from the 14 build/QC agents and consolidated here. Items are advisory — not
committed work.

Severity legend: **[blocker]** must-fix before ship · **[high]** important ·
**[med]** worthwhile · **[low]** nice-to-have / polish.

Status of the crate as delivered: **builds clean**, `clippy -p pharos-panel
--all-targets -- -D warnings` clean, `cargo fmt` clean, **runs + verified
on the CachyOS box** (Overview/Compositor/Video/Settings/Controllers eyes-checked
reading real `peers.toml` / `controller-profiles.toml` / host NIC + ffmpeg caps).

---

## 2026-07-02 — [med] Windows host HDR capture+encode (the deferred path to game-HDR)

**[med]** — HDR is nice-to-have (user); do this when the M3 Windows host is picked back up.

**Why this exists / why NOT on Linux:** Proton/Linux game-HDR is BLOCKED at the Wine
Vulkan-on-Wayland WSI, not in Pharos. Verified live 2026-07-02 (Witcher 3 DX11/DXVK
*and* Crimson Desert D3D12/vkd3d both black-screen with `VK_ERROR_OUT_OF_HOST_MEMORY`
on swapchain create): `wine-cachyos 11.0` does NOT expose `VK_EXT_swapchain_colorspace`
to the game (`extSwapchainColorSpace:0`), so the surface only offers `SRGB_NONLINEAR`
and the HDR swapchain can't be allocated. The **compositor is proven correct** — native
`WAYLAND_DISPLAY=wayland-1 vulkaninfo` offers the full HDR set (scRGB fp16 + HDR10 ST2084
+ `VK_EXT_swapchain_colorspace` rev5). So Linux game-HDR is deferred to upstream Wine.
Full detail: `HANDOFF.md`, memory `project-hdr-vrr-plan.md`, traces `rig/hdr-traces-2026-07-01/`.

**Already done (reusable):** transport `wants_hdr` wire negotiation (`a3b2a3e`, auth v4) +
Trireme HDR *display* path for Linux/Windows clients (`479b6a6`). So host→client HDR
transport and client-side render already exist. The Linux host also already has a working
10-bit encode path to port: `apply_hdr10()` + `crate::nvenc::hdr10_sei_payloads()` (Main10
+ BT.2020/PQ VUI + MDCV/CLL SEI).

**The work (Windows host-side, native DXGI HDR — no Wine translation):**
1. **HDR capture** on the Windows host: DXGI Desktop Duplication / ddagrab in 10-bit
   (`R10G10B10A2` or fp16 scRGB), capturing the HDR swapchain + its mastering/CLL metadata
   from a game running HDR natively on Windows.
2. **NVENC HEVC Main10 encode** (+ BT.2020/PQ VUI + per-IDR MDCV/CLL SEI) — reuse the Linux
   `apply_hdr10`/`hdr10_sei_payloads` logic in the Windows host-core encode path.
3. **Wire to `wants_hdr`** (already negotiated) so a capable client flips the host to Main10.

**Where:** `pharos-win` / host-core (see memory `project-pharos-windows`, M3). Windows renders
HDR natively (same reason Sunshine HDR is a Windows-host feature), so none of the Wine/Vulkan-WSI
pain applies.

**Nice-to-have Linux fallback (lower priority than Windows):** run games inside **gamescope
`--hdr-enabled`** nested in Pharos — gamescope owns the HDR swapchain so winewayland never
has to (the Steam-Deck/Sunshine route). Adds a compositor hop + latency; only if a Linux HDR
option is ever wanted before upstream Wine matures.

---

## 2026-06-24 — Hardcoded IPs / hostnames / ports in code (design smell — catalog, refactor later) — ✅ RESOLVED 2026-06-24

**All of A–G shipped this session** (`c40aee0` derive USB/IP target from the paired host; `4f4d37e`/`20138fb` de-personalize
example IPs, the MQTT broker hint, `devbox`/`peripheralmox`/`couchpi` names, the `/home/user` doc path + `personal-name …`
examples; `032185c` centralizes ports into `core_proto::ports`). An adversarial completeness sweep confirmed no
`192.0.2.*`, broker IP, or personal machine-name survives in tracked source; `bücher.example` serves as the IDNA example. The one remaining literal-port note (pharos-win help text shows `4432`/`4435`) is cosmetic display drift only.

Inventory of personal-network literals and magic ports baked into the source. None
are bugs; the concern is **publication** (this repo will go public) and **portability**
(someone else's LAN is not `192.0.2.x`). The fix in every case is the same shape:
lift the literal into config/env/a named constant with a neutral default. Listed
worst-first. Generated bindings under `client/shared/generated/` are
gitignored, so their mirrored doc-comments don't ship.

### A. Functional — a personal LAN address ships as the default behavior

- **[high]** [`UsbExportEngine.kt:28`](client/androidApp/src/main/java/dev/pharos/trireme/UsbExportEngine.kt:28)
  — `const val PHAROS_DEFAULT = "192.0.2.100:4434"`. The USB/IP export target defaults
  to *my* rig's LAN IP. Anyone else gets a dead default. → derive from the paired host's
  address, or surface as a required field with a neutral placeholder.

### B. Panel UI ships a personal IP as visible hint text

- **[high]** [`settings.rs:489`](crates/pharos-panel/src/tabs/settings.rs:489)
  — `.hint_text("192.0.2.4")` is my actual HA/MQTT broker IP, rendered as the
  placeholder in the MQTT-broker field. → use a generic example (`mqtt.local` /
  `192.0.2.10`, the RFC 5737 doc range) or empty.

### C. Example/sample config shipping a personal subnet

- **[med]** [`trireme-sources.example.toml:23,30`](crates/trireme/trireme-sources.example.toml:23)
  — `192.0.2.100:4433`, `192.0.2.101:4433`. Example configs are the first thing a
  new user copies. → switch to `192.0.2.x` / `host.local`.
  (Note: `crates/compositor/multi.example.toml` already uses neutral `0.0.0.0` — good.)

### D. Doc-comment examples using my subnet

- **[low]** [`core-ffi/src/lib.rs:345`](crates/core-ffi/src/lib.rs:345),
  [`play.rs:87,92`](crates/trireme/src/play.rs:87),
  [`servers.rs:254`](crates/trireme/src/servers.rs:254) — all use `192.0.2.100/101`
  as the illustrative address in `///` docs. Cosmetic, but it's my network in the public
  API docs. → `192.0.2.x`.

### E. Test fixtures with my IPs / device names

- **[low]** [`control-ipc/src/lib.rs:631,645,658`](crates/control-ipc/src/lib.rs:631)
  — `event_roundtrip` fixture hardcodes `192.0.2.122`, `192.0.2.2`, alias `couchpi`.
- **[low]** [`transport/src/lib.rs:2438`](crates/transport/src/lib.rs:2438) — `resolve_addr`
  test asserts on `192.0.2.100:4433`.
  (Good contrast: `servers.rs` tests already use the neutral `192.0.2.10`.) → neutral
  placeholders; behavior is identical.

### F. Personal rig/host names in comments

- **[low]** `devbox` in [`core-proto/src/profile.rs:40`](crates/core-proto/src/profile.rs:40)
  and [`controllers.rs:30`](crates/pharos-panel/src/tabs/controllers.rs:30) (the controller
  feel was "dialled in live on devbox"); `peripheralmox` in
  [`trireme-kms/src/main.rs:3`](crates/trireme-kms/src/main.rs:3) +
  [`trireme-kms/Cargo.toml:7`](crates/trireme-kms/Cargo.toml:7); `couchpi` in
  [`data.rs:572`](crates/pharos-panel/src/data.rs:572). → generic role names
  ("the host", "a display-less SBC") in comments.

### G. Magic port numbers scattered as literals (the broader smell)

The wire ports — **4432** pair · **4433** legacy stream · **4434** usbip · **4435** win
stream · **15777** current stream — are repeated as string literals across
[`compositor/src/main.rs:84`](crates/compositor/src/main.rs:84),
[`pairing.rs:29`](crates/compositor/src/pairing.rs:29),
[`trireme/src/main.rs:60,85`](crates/trireme/src/main.rs:60),
[`trireme-kms/src/main.rs:53`](crates/trireme-kms/src/main.rs:53),
[`pharos-win/src/main.rs:34,40,70`](crates/pharos-win/src/main.rs:34),
[`usbip-host/.../pharos-usbimport.rs:43`](crates/usbip-host/src/bin/pharos-usbimport.rs:43),
and the `quic-recv` helper. → centralize as named constants (a `ports` module) so the
defaults live in one place. The `127.0.0.1` / `0.0.0.0` bind hosts themselves are
conventional and fine; it's the duplicated port magic that bites on a re-number.

---

## 2026-06-23 — Future / research: client-side upscaling negotiation ("stream low, present high")

User-flagged "crazy dream" (good instinct, legit architecture). Today the stream is **compositor-bound** (~80fps at native 2712×1220 — the GL composite→copy→GL/CUDA→NVENC per-frame cost scales with pixels; the game's quality sliders move only the *game* render rate, not the stream ceiling; host-side DLSS doesn't help because the compositor still captures full-res). Inverting it:

- **Render + encode + stream a SMALLER frame on the host** (fewer pixels → higher host fps, less GPU, less WiFi) and **upscale on the CLIENT** to fill the panel. Upscaling *before* the encoder (host DLSS) pays the full render+encode+bandwidth cost; upscaling *after* the decoder saves all three.
- **Tier 0 (available now):** set a lower stream resolution; the client SurfaceView already bilinear-upscales to fill (`MainActivity` AndroidView aspect sizing). Quick fps/bandwidth-vs-sharpness test, no code.
- **Tier 1 (the dream):** Trireme **detects the client's upscaler** (MediaTek AI scaler / a GL/Vulkan spatial-upscale shader / Android `MediaCodec`/`ImageReader` post-process) → surfaces it as a client option → negotiates over the wire → **Pharos renders/streams at the matched lower res**. Decouples stream res from display res, the inverse of host DLSS. Sharp, not bilinear.
- Ties to W2b mode advertisement (host↔client geometry negotiation already needed) and the [[reference-streaming-latency-tuning]] bandwidth budget.

## 2026-06-23 — Duplicate app-launch on reconnect crashes the game (HIGH) — ✅ RESOLVED 2026-06-24

**Both action items are done (verified against current `main`); striking as stale.**

Hit live: client reinstall dropped the stream; on reconnect a **second** `launch 'crimson-desert'` fired (`transport: app relay: received launch` → `app_launcher::launch: steam-launch handoff spawned`) while the first instance still held the GPU → Crimson Desert's D3D/VKD3D `Failed to create device : 0x80004005` + crash (`crashpad_handler` dump). Recovery = kill the Proton tree (`kill` the SteamLaunch reaper + wineserver).

- **[✅ done] Make app-launch idempotent.** Shipped in `23fd646` ([launch.rs:162-200](crates/app-launcher/src/launch.rs:162)): a slug→pid registry no-ops a relaunch of a live direct-exec pid (`Ok(None)`), and a `/proc` scan (`steam_game_running`) skips the redundant `rungameid` handoff for an already-running Steam game — the exact host-side re-handoff that caused the live crash. Unit-tested.
- **[✅ done — never a real bug] Don't auto-relaunch on reconnect (client).** The Kotlin `StreamHub` reconnect path only re-binds video/audio/catalog; `AppMsg::Launch` is emitted **only** from a user `AppTile` tap. The duplicate was the host re-handoff above, not a client auto-relaunch — so nothing to suppress client-side.
- The related [[lesson-nvenc-driver-crash-on-gpu-app-launch]] cold-start-into-live-NVENC hazard is separately guarded ([launch.rs:137](crates/app-launcher/src/launch.rs:137) refuses cold-start into a physical target; `517e71d` pre-warms Steam instead of cold-starting it concurrently with live NVENC). The reconnect-path NVENC SIGSEGV is fixed in `dfb4057` (no NVENC re-init on reconnect — resync via the periodic IDR).

## 2026-06-23 — Trireme client text-input / focus bugs (found on Xiaomi 14T Pro during debug-build testing)

Surfaced live on the phone (touchscreen + controller) while pairing with host "devbox". **Root cause:** `ui/TvEditField.kt` was TV-first for ALL devices — it gated the IME behind a `readOnly` flag and assumed "any key reaching the field ⇒ the keyboard is already down ⇒ exit edit", which holds on a D-pad-only TV but breaks on a touch phone (tap never focused; D-pad keys collapsed edit + dismissed the IME).

- **[FIXED ✅ — verified on 14T Pro] Soft-keyboard text entry on touch.** `TvEditField` now splits on `FEATURE_TOUCHSCREEN`: touch devices get a plain editable `OutlinedTextField` (native focus→IME, nothing intercepted); D-pad-only TVs keep the validated gated model. Tapping a field focuses + opens the keyboard and it stays up. (Commit `trireme: fix touch text-input`.)
- **[FIXED — needs controller re-verify] Controller navigation of the soft keyboard.** On touch devices the field no longer intercepts D-pad keys, so the IME owns its own key navigation. Verified for touch typing; **re-confirm with the Vader driving the soft keyboard on the phone.**
- **[PARTIAL] Double focus glow rings.** Fixed for **text fields** (the focused native border is now transparent, so only the single gold `triremeFocus` ring shows). **Still TODO:** audit rows/tiles/navbar items for any focusable-within-focusable that draws two rings (the "never make a Card/container clickable" lesson). Point at a specific spot if one remains.
- **[med] Controller text entry on a phone soft keyboard is impossible (platform limit, not our bug).** With a focused field, the left stick moves the *caret* (focused field maps arrows → cursor) but the controller **cannot select on-screen keys** — Gboard/phone IMEs aren't D-pad-navigable (TVs get this free via the leanback IME). If we want controller text entry on phones, the only route is a **custom in-app focusable on-screen key grid**; otherwise touch/hardware-keyboard only. Not worth it unless phone-as-couch-with-controller becomes a real flow.
- **[med] Controllers pane only lists USB/IP-EXPORTED controllers, not Android-forwarded ones.** A pad connected directly to the phone (e.g. the Vader, read via `InputDevice` and forwarded as a virtual host pad) doesn't appear in the Controllers pane. Add a short **"connected (Android)"** overview at the TOP (hide when none), and **explicitly label the existing list "exported"** so the two paths are distinguishable. (`ui/panes/ControllersPane.kt`; the Android-connected set is what `ControllerManager`/`probeSensors` already enumerate.)
- **[done] HUD "VRR" label confusing.** The HUD showed the client `PaceMode` ("VRR") which read like a panel-VRR claim on a panel with no continuous VRR. Relabelled `pace VRR` + added a real `panel <Hz> multi-sync(N)/fixed` line (`ui/DebugHud.kt`).

> Context: the **debug Trireme variant + on-screen HUD shipped and works** (top-right metrics overlay verified live: codec/fps/queue/drops/lag on `c2.mtk.hevc.decoder`, thermal, per-pad gyro/accel sensor presence). A latent **UniFFI checksum drift** between the (git-ignored) generated `core_ffi.kt` and `libcore_ffi.so` was also found — a bare `gradle assembleDebug` reuses stale artifacts → "checksum mismatch" on the first FFI call; **always build via `rig/build-android.sh`** (lockstep regen). Consider a CI gate that rebuilds bindings+`.so` together so they can't drift.

---

## 2026-06-22 — Reference build: 60 fps locked, HW HEVC e2e, subscriber-gated push topics (SHIPPED, smoke-tested live)

Reference build landed (commit `7e9d926`, memory `project-reference-build-2026-06-22`): Pharos host (Wayland
compositor + NVENC HEVC) → QUIC over WiFi → Trireme on X90J (OMX.MTK hardware HEVC) streaming **60 fps locked
end-to-end** with `host_send_fps measured=60.0 target=60` sustained over WiFi (host -70 dBm, TV -51 dBm).
Crimson Desert is playable through Proton with vibrant colors and acceptable input lag.

**Landed and live-verified:** wire-rate fps ground truth in logs (`host_send_fps measured=X target=Y bytes_sent_1s=N` 1/sec),
**subscriber-gated push topics** with **per-stage framing histogram** (dispatch/ctrl/input/gyro/render/post/pace/status —
status-build cost dropped from 328 ms/frame to sub-10 ms when no panel subscribed), **hardware HEVC codec binding** on
client (MediaCodecList enumeration excludes `OMX.google.*` and `c2.android.*`; confirmed `OMX.MTK.VIDEO.DECODER.HEVC hw=true`),
NVENC encode watchdog + wall-clock QUIC `write_all` stall observability, **unified controller swap architecture**
(host authority, QUIC back-channel relay) with Swap A/B slot-routing fix (`usbExport` flag validated, hard-coded slot 0),
controller subscription gating (mirrors FrameTimings gate, 4 Hz throttle), control-ipc **FrameTimings snapshot API** with
panel live Performance view (per-stage mean/p95/max µs in Compositor tab, subscriber-gated, zero cost when closed), gyro
opt-in stream-time gating + per-app mode (Enabled/Mouse/Disabled), USB/BT bus icons inline + 32-char name truncate,
**pharos-tray** (KDE SNI lighthouse with idle/connected/streaming states), compositor mode merge (single+multi consolidated),
Spectacle wrapper for KDE portal-confusion, `rig/launch-host.sh` REAP+BUNDLE one-click stack manager, watercolor lighthouse
icon + three-state SVG tray icons.

**Unresolved / needs your follow-up:**
- **[high] Trireme Swap A/B / Swap X/Y toggles do not take effect live on connected streams.** ControllerTuning relay lands
  on wrong slot for USB/IP controllers that are also locally attached. Fix landed (validates `p.usbExport` flag in
  `ControllersPane.kt:573-579` and hard-codes slot 0), but live smoke test on X90J with USB/IP-exported controller is not
  yet confirmed. Workaround: set swaps before stream start to avoid perturbing pipeline.
- **[high → restart-only + CI-gate-blocked] NVENC encode stall → 2.5 fps wire regression.** ⚠️ This is a *wedge/stall*, NOT
  the NVENC SIGSEGV (those are separately fixed — see the resolved reconnect-crash section above + `dfb4057`). Host wire-rate
  `measured=2.5` instead of `target=60`; tcpdump showed ~20 pps (53.6 KB/s) vs expected ~1.9 MB/s. Hypothesis: NVENC
  `encode_picture` stalling ~400 ms/frame (documented libnvcuvid wedge). **Instrumentation shipped** (verified in code):
  framing split `gl_blit`/`encode`/`wire_enqueue` ([multi.rs:1506+](crates/compositor/src/multi.rs:1506)); NVENC watchdog
  warns >50 ms, logs "NVENC LIKELY WEDGED" after 3 consecutive >200 ms ([multi.rs:1588](crates/compositor/src/multi.rs:1588));
  QUIC `write_all` is preemption/close-raced with a >200 ms slow-WARN ([transport lib.rs](crates/transport/src/lib.rs)).
  **No auto-recovery, by design and by constraint:** clearing the wedge in-process would require re-initialising NVENC —
  which is the very libnvcuvid SIGSEGV we mitigate elsewhere — so **process restart (`Restart=on-failure`) is the only
  lever, already wired.** The only genuinely-open remnant is the proposed CI invariant `host_send_fps ≥ 0.95 × target`,
  which is **infra-blocked**: CI excludes the compositor crate (no CUDA runner), so the gate needs a self-hosted rig runner
  first. Optional: a live re-repro to confirm the wedge still fires at all post-mitigation. **Not a code task.**
- **[high] Status `fps` field briefly shows configured target on cold start instead of measured EWMA.** For first ~3 s
  after fresh connect the EWMA is seeded to `cfg.fps`, so panel displays too-high number until histogram converges. Not a
  regression vs prior lying behavior (always reported `cfg.fps`), but users may see spike-then-drop on cold start.
  Acceptable with documentation; verify on live rig.
- **[med] Per-app gyro Mouse mode does not reset on app exit to default Enabled state.** Only `LaunchApp` wires the per-app
  policy switch; exiting an app does not restore the default. Affects users who toggle Mouse mode for a FPS, then quit —
  next app launch inherits Mouse instead of Enabled. Fix: hook app-exit via `reap_exited_apps()` to restore default
  (currently only does child process reaping, not policy reset).
- **[med] Slot-0 hardcode for USB/IP pad routing assumes single-port pharos-usbimport; future multi-port setup needs
  bidirectional slot table.** If future setup adds multi-port USB/IP export (e.g. two USB hubs forwarded on slots 0 and 1),
  client needs a way to route controller to correct port. Currently no bidirectional slot mapping exists. Deferred;
  document in `WORKAROUNDS.md`.
- **[med] Trireme on-device bus icon uses MAC-pattern heuristic on `InputDevice.descriptor`; host evdev-derived bus is
  authoritative.** `ControllerManager.inferBusType()` matches vendor MAC patterns (e.g. Sony/Nintendo/8BitDo) — client-side
  best-effort only; host's actual evdev bus (from `/sys/class/input/{devname}/device/bus`) is ground truth. No sync; if
  pairing + descriptor changes (rare), icons could mismatch. Acceptable as display-only hint; document as "best-effort on
  Android".
- **[RESOLVED 2026-06-23] Spectacle wrapper RETIRED — its shim `Exec=` broke screenshots worse than it helped.** The
  per-user `.desktop` override pointed `Exec=` at `~/.local/bin/spectacle-safe`; KWin's `org.kde.KWin.ScreenShot2` auth
  validates the running binary against the resolved `.desktop`'s `Exec=`, so the shim → *"process is not authorized to
  take a screenshot"* (KDE bug 446628, firejail-class). Also redundant: session is already `wayland-0` and Spectacle is
  `DBusActivatable`. Fix = user `.desktop` `Exec=` pointed back at real `/usr/bin/spectacle` (now byte-identical to the
  system file); wrapper orphaned/inert; verified capturing with pharos up. ⚠️ don't *delete* the override out from under
  the cached shortcut (kwin_wayland owns kglobalaccel on Wayland) → "No such file" on next Print; mirror the system file
  instead. See `rig/WORKAROUNDS.md`. If the portal dark-screen recurs, fix pharos-side, do NOT re-shadow with a shim.
- **[med] Compositor mode merge open concern: raw-gyro streaming in multi-client scenario sends to FIRST connected
  streamer only.** Multi-mode merge consolidated single+multi backends; raw-gyro datagram per-frame sends only to first
  connected client (multi-client cal visualizer would differ if second client connects mid-stream). Low severity (rare use
  case), documented in code. Acceptable for reference build.
- **[med] pharos-tray subscriber-gated framing histogram doesn't broadcast to clients; MQTT/HA frame-timing publish is a
  natural extension.** Panel-only telemetry (Controllers + Compositor Performance subscribers). Frame timings would be
  valuable in MQTT state for Home Assistant automations (detect stutter, adaptive bitrate). Deferred; candidate for
  post-reference-build work.
- **[med] FrameTimings snapshot API: first `GetFrameTimings` after cold gate returns `window_frames=0` until histogram
  fills.** Panel shows "warming up" badge until instrumentation fills — correct behavior but UX friction on first open.
  Acceptable; consider a "loading…" badge or seed with 60 expected frames.
- **[med] FrameTimings sparkline y-axis shared across all 8 stages; tight-µs stages (status) appear flat next to wide-µs
  stages (render).** Cross-row severity comparison works, but relative spikes within a tight stage are visually obscured.
  Right trade-off per the design review, documented as a known visual trade-off in panel code.
- **[low] Window-close detection uses child-handle-based tracking; fast app relaunch could leak tracker entry.**
  `reap_exited_apps()` added in `multi.rs` per-frame. Drop of previous `Child` handle on re-launch (documented) is correct,
  but a very fast cycle could leave a stale tracker entry if reap misses the exit window. Unlikely in practice; monitor in
  live QC.
- **[low] Controller config as a headline feature needs vocabulary build-out: per-controller / per-game layout presets.**
  Framed in reference build session: Souls dodge style, Nintendo B-dodge, paddle remapping presets as first-class UX.
  Currently only host-authority swap A/B + X/Y + per-app gyro mode. Deferred feature direction.
- **[low] Reference-build invariants need CI enforcement; currently documented but not gated.** Invariants:
  `host_send_fps ≥ 0.95×target`, `status_p95 ≤ 20 ms`, TV decoder `hw=true`, no new per-frame syscalls without
  subscriber-gating. Today documented; CI gate would prevent regression. Candidate for automated QC harness.
- **[low] Android client bus icons are best-effort (descriptor MAC pattern); proper detection needs
  `BluetoothManager.bondedDevices` + `BLUETOOTH_CONNECT` runtime permission.** Heuristic in
  `ControllerManager.inferBusType()` matches vendor MAC patterns (Sony = WiFi, etc). Sufficient for reference build;
  deferred as client-polish task.
- **[low] NVENC init log doesn't capture NVIDIA driver version; `nvidia-smi` from outside binary remains post-mortem
  source of truth.** Instrumentation logs NVENC codec/dims/fps/preset but not driver version (SDK doesn't expose it). For
  wedge post-mortem, admin must run `nvidia-smi --query-gpu=driver_version` separately. Document in `WORKAROUNDS.md`.
- **[low] Sub-stage split (`gl_blit`/`encode`/`wire_enqueue`) visible only in host logs; panel wire snapshot still shows
  single `render` bucket.** Control-ipc `FrameTimingsSnapshot` unchanged (backward compat). Panel displays sum; split
  visible via `RUST_LOG=compositor::framing=info`. Full snapshot update is separate panel-side work.
- **[low] Host on WiFi at -70 dBm; planned wired migration will free headroom for higher bitrate or multi-tenant traffic.**
  Reference build invariants validated on WiFi deliberately (worst plausible link). No action needed; flagged for
  completeness.

## 2026-06-21 — Live control plane + modes + video + latency (SHIPPED, needs live QC)

Landed this session (commits `49601be` → `5a83847`, workspace build/clippy-D/fmt/tests green).
This **resolves the #1 theme below** (the panel is now live): a local UDS control socket
(`control-ipc` leaf crate) connects `pharos-panel` ↔ a control-server thread inside the
running compositor (both `capture-zc` and `capture-zc-multi`). Shipped: live host/connection
status (fixes the always-`Unknown` "not connected" impression), **Mirror/Extend/Single**
layout-mode presets (replaced the arrange tools), **"Play media on the wall"** (panel →
control socket → mpv as a Wayland client → existing NVENC→QUIC sink), and an on-demand
**latency benchmark** in the Video tab (per-step ms + Codec column + a distinct **NEXUS**
network/router row) over new QUIC datagrams + NTP-style clock sync. `capture-zc-multi` now
defaults to `~/.config/pharos/multi.toml` so the panel's saved layout is load-bearing.

**Unresolved / needs your QC + follow-ups:**
- **[blocker-for-trust] Not yet eyes-validated live.** All green in build/unit tests, but the
  full loop is unverified on the rig: panel↔compositor socket handshake, Play-media (needs a
  **Wayland-capable `mpv` on PATH** — Play fails gracefully to an Ack error otherwise; an
  `ffplay` fallback is a possible follow-up), the latency benchmark end-to-end against a live
  `trireme play`, and **Extend** showing genuinely distinct per-output content.
- **[med] Latency host-stage granularity is coarse.** `FrameStageMeta` carries 3 stamps
  (capture/encode_done/send) → 2 host intervals, so compose vs encode are lumped into
  "Capture→Encode" and `encode_ms` is ~0 (the panel skips ≤0.05ms rows). A clean compose/encode
  split needs a 4th `encode_start_ns` stamp in `core_proto::FrameStageMeta` (+ the client mapping).
- **[med] `nexus_ms` is offset-error-bounded by design.** It's `(recv − offset) − send`; the
  true one-way wire time is only known to ±(RTT/2 − min) on an asymmetric path. The panel shows
  the ± and labels it "unverified in-the-middle" — correct, but don't read it as exact.
- **[med] Decode/present matching assumes 1 AU → 1 frame, in order.** Holds for the current
  IPPP NVENC config (`frameIntervalP=1`, no B-frames); if reorder/B-frames are ever introduced
  the FIFO join in `play.rs` would mis-attribute. Note in `trireme/src/play.rs`.
- **[med] Live layout switching is file-driven, not hot.** `SetLayoutMode`/`ReloadLayout` over
  the socket just `Ack` (v1); applying a new layout = panel writes `multi.toml`, host picks it
  up on next (re)start. A live output rebuild is deferred (the NVENC `Pipeline`s are
  self-referential and must not be rebuilt mid-loop).
- **[med] `HostStatus` omits the connected peer's remote addr + pubkey** (`ClientStatus.addr`/
  `pubkey` = `None`): `QuicStreamer` doesn't expose `conn.remote_address()` / the verified
  client key. Add accessors on `QuicStreamer` to surface which peer/addr is connected.
- **[med] Control plane is Linux/UDS only.** No control server in `pharos-win` (Windows host);
  the panel client is `#[cfg(unix)]` with a clean no-op fallback. Windows = named pipe + a
  `pharos-win` control server, deferred. (Aligns with the existing MQTT-is-Windows-only gap.)
- **[low] Android client reports 0 decode/present.** `core-ffi` passes `None` for
  `latency_taps`, so a benchmark against the Android Trireme has real network/host stages but
  no client-side decode/present (MediaCodec). Wire taps through the FFI if wanted.
- **[low] Multi-output bench drives only the first connected output.** Per-output / per-client
  benchmarking isn't separated.

## 2026-06-21 (cont.) — Decoder latency dig + control-socket robustness

Triggered by the benchmark showing a ~137 ms client "buffer". Dug in (commit `40fe344`):
- **Root of the in-flight buffer is the decoder, not the network.** It is NOT the QUIC
  flow-control window (shrinking `PHAROS_STREAM_WINDOW_KB` 256→32 *raised* it), NOT the DPB
  (NVENC `-refs` 5→2 made no difference), NOT `-surfaces`. It's ffmpeg input buffering + NVDEC's
  intrinsic pipeline (~4 pictures held before output).
- **Shipped win:** `-flags low_delay` for cuvid (gated `LOW_DELAY_SAFE` set in `media-decode`) —
  **MD5 bit-exact** to baseline on ffmpeg n8.1.2, **~halves decode-side latency (99.8→51.4 ms
  paced)**. Live-confirmed perceptually. The old "low_delay = green frames" lore was wrong: the
  corruptor is `-fflags nobuffer` (MD5 differs); `low_delay` alone is clean.

**Follow-ups:**
- **[✅ done] Control server robustness** (was hit live): both halves resolved.
  - **(a)** Idle-host visibility — `ControlServer::spawn` now runs *before* the client-wait in
    both `run_capture_zc` and `run_capture_zc_multi` (commit `08d6fb5`).
  - **(b)** The "connection-refused after ~a dozen reconnect cycles" wedge was the **single-threaded
    synchronous accept loop**: `serve_client` ran inline, so while one client was being served (or
    sat half-open) the loop never returned to `accept()` and reconnects piled into the backlog →
    ECONNREFUSED/unresponsive. Fixed in `control.rs`: the accept loop now hands **each connection to
    its own serve thread** and is immediately back at `accept()`; a leaked/silent client can only tie
    up its own thread. Bounded by a **write timeout** (reaps a client that stops reading), a
    **`CMD_REPLY_TIMEOUT`** on the four blocking command replies (reaps a render-loop stall), and a
    **`MAX_CONTROL_CLIENTS` cap** (leaks can't starve the real panel). `bind()` already reclaims a
    stale socket (`remove_file` then bind), so it was left as-is. Verified virtually by 5 unit tests
    that reproduce the bug class deterministically (a silent client must not block another; survive
    64 reconnect cycles; isolate a mid-message disconnect; cap + recover concurrent clients) —
    **not yet re-confirmed on the live rig.**
- **[med] Validate `low_delay` on the other decoders** and extend `LOW_DELAY_SAFE`: QSV (T14),
  `rkmpp`/`drm` (ARM), `amf`, and especially **software** (the generic `hevc` decoder showed
  ~1 s via frame-threading — `low_delay`/`-threads 1`/`-thread_type slice` likely a big win).
  Each needs an MD5 bit-exact check first (the safe-flag story is per-decoder).
- **[low] Residual ~51 ms cuvid latency is NVDEC's HW pipeline** — no further ffmpeg lever
  (`-async_depth` isn't a cuvid option). Cutting it more means a zero-copy NVDEC low-latency
  path (drm_prime → GL), a large task tied to the deferred ARM zero-copy work.
- **[low] Startup lead is noisy.** Part of the buffer is a persistent lead established at stream
  start (warmup) that never drains at steady 60→60 fps; a client-side drain/resync-to-newest on
  first frames could trim it.

## 2026-06-21 — UX information-architecture pass (SHIPPED)

Audited all 8 panel tabs (each finding adversarially verified against the code). Implemented the
5 confirmed-sound findings + 2 real sub-issues surfaced by rejected ones (commit `f13c44c`):
- **Input tab reframed as client defaults** — its policy/chords/hotkeys configure the connecting
  *client*, not the host (and persist to a `[input]` section nothing reads yet). Added an explicit
  banner + a "Client input defaults" vs "This host" grouping.
- **Pairing dedup** — Settings had a hardcoded, non-localised `pharos-compositor pair` literal; now one
  canonical key + a jump-link to Status.
- **`client_wait_secs`** moved out of "Streaming/encoding" into the Security/session section.
- **Audio↔Video** cross-link (the Audio stub points to the live Opus status on Video).
- **Gamepad swap** discoverability cross-link from Controllers → Settings.
- **Resolution policy** stays owned by Settings; Compositor's hint is now a jump-link (no save-trap).
- **Per-output Refresh Hz** was an editable no-op → now a read-only derived label.
- One stale "Overview"→"Status" tooltip word.

Audit **rejected 4 of 9** as inaccurate/risky (e.g. "make Refresh Hz authoritative" needs per-output
compositor pacing that doesn't exist; "Status vs Overview mismatch" — the heading already reads
"Status", the internal name is intentional). Deferred: a host→client input-config push would make the
Input tab actually drive clients (today it's a write-only reference).

## Compositor: live-client palette (design, deferred — user-requested 2026-06-21)

Surface **live-connected** Trireme clients (incl. the local loopback `trireme play`
window, and the Bravia once it connects) in the Compositor-tab **palette**, draggable like
the paired-peer chips, so a user can build Mirror/Extend layouts that span the actual live
clients instead of guessing ports.

**Model constraint that shapes the UX (important):** the host is the *server* — each compositor
output is a `quic://…:port` the client *dials into*. The host can't relocate a client; the client
attaches to a port. So "duplicate the loopback window with the Bravia" concretely = **two outputs
(one port each)** + the existing **Mirror/Extend** geometry (same region vs side-by-side). The
palette's live clients are for *visual placement/targeting*, not for moving a stream around.

**Pieces to build (if picked up):**
1. **Prereq — expose the connected client's address + verified pubkey in `HostStatus.ClientStatus`**
   (today both are `None`). Needs `transport::QuicStreamer` to surface `conn.remote_address()` +
   the authorized `client_id`, plumbed into the compositor's per-frame status refresh.
2. A **"Live clients" section** in `tabs/compositor.rs` palette, fed from
   `crate::control::handle().status()`, rendered as `dnd_drag_source` chips (mirror the existing
   `peer_chips`); dropping one creates an output tile bound to that client.
3. Drop + Mirror/Extend emit a **per-client-port `multi.toml`** (one `[[output]]`/port per client).

**Hard limit:** the panel cannot force a remote client (the Bravia) to reconnect to a specific
port — its Trireme config dials `host:port`, so each output's port must match what its client is
pointed at. A future **client-control channel** ("reconnect to port X", pushed over the existing
QUIC/input back-channel) would lift this; separate feature.

## ⭐ Backend plumbing needed (the #1 theme — to make the UI fully *live*) — ✅ now addressed by the control socket above

The panel currently reads on-disk config truthfully but has no live host signal.
These are the highest-value follow-ups; the model already has the fields to receive them.

- **[high] No live-fleet telemetry source feeds the Overview.** `PeerInfo.liveness`
  is always `Unknown` and `addr/encoder/fps/last_connected` are always `None` —
  nothing writes live per-client state. `peers.toml` only stores `{alias,pubkey,
  paired_at}`, and `transport::QuicStreamer` keeps no per-connection remote
  addr/fps/encoder the panel can read. Needs a host-side status source the panel
  can poll: a runtime status file the compositor writes, a small local IPC/HTTP
  status endpoint, or reused MQTT state topics → `crate::data` populates `PeerInfo`.
  (The Overview already renders this honestly — `Unknown` badge + `—` + tooltips —
  so *only the backend is missing*.)
- **[med] MQTT telemetry is Windows-only.** Per project history the MQTT/HA
  telemetry lives on the `pharos-win` path, not the Linux compositor (the primary
  dev box). A cross-platform host status mechanism is needed before Overview
  liveness is meaningful on the main rig.
- **[✅ done — Linux] Injection-backend card now has a live permission probe.**
  `data::injector_health()` try-opens `/dev/uinput` for write (side-effect-free — no `UI_DEV_CREATE`)
  and returns `InjectorHealth::{Ready, Blocked(reason_key), Unknown}`. The HID badge is now coloured by
  the probe (green/amber/neutral) with a described live state, and a positive **Blocked** result surfaces
  the fix inline (load `uinput`, or join `input` / add a udev `uaccess` rule). Crucially this honours
  **udev/logind ACLs**, so it's correct where a plain `input`-group check would falsely warn (verified on
  the dev box: not in `input`, yet ACL-writable → correctly `Ready`). Windows/macOS return `Unknown`
  (not probed — the SendInput same-session check is deferred; the panel may not be co-located there).
- **[med] Live controller enumeration.** The panel can't see plugged-in pads, so
  users add by VID:PID by hand and `classify()` is called with an empty name (the
  8BitDo/Flydigi name-spoof override never fires). A backend probe (gilrs on
  desktop, or piping the host's last `GamepadConnect` over a status channel) →
  a `controllers_live` model field would enable one-click "add detected controller".
- **[✅ done] Re-probe now actually re-runs the decode test-decode.** Added
  `media_decode::reprobe()` — the same probe body as `capabilities()` but bypassing the `OnceLock`
  cache, returning a fresh owned `Capabilities` (the cached result is untouched, so the live decode's
  `input_args`/`selected_label` stay stable). `data::reprobe_decode()` maps it via a shared
  `map_decode_caps()` helper, and the Video tab's background Re-probe worker now calls it. GPU
  plug/unplug is reflected without a restart.
- **[low] `validated`/`validated_on` is a static rig catalogue, not real results.**
  The ★ in the Video tab reflects historical rigs (RTX 4070, T14, RX 570, RK3588,
  Pi), not an end-to-end run on the *current* box. A "run validation" action that
  streams the embedded sample host→client and records the pass would make ★ truthful.
- **[low] Push a profile to a live stream.** The Controllers tab edits persisted
  host profiles only. An "Apply to live session" emitting
  `InputEvent::ControllerTuning` for a connected slot would mirror the Android
  live-tuning flow (needs a command channel to the running host).
- **[low] Confirm the host consumes `resolution_policy`/`refresh_policy` and
  `client_wait_secs` from `panel.toml`.** These are persisted but the compositor's
  `multi.toml` only carries one shared `fps` + per-output `w/h`. If the host
  doesn't read the policy fields, the selector is advisory-only and should say so.

## Architecture & cross-platform (scaffold)

- **[✅ done] `save_host_config` is now read-merge-write.** `data.rs` parses the
  existing `config.toml` as a generic `toml::Table`, replaces only `[mqtt]`+`[network]`,
  and re-serializes — any host-authored section survives a panel Save. Logic lives in a
  pure `merge_host_config_toml(existing, &cfg)` helper with unit tests (preserves an
  unmodelled `[host_only]` section; empty optionals stay absent). No new deps (`toml 0.8`).
- **[✅ done] Compositor `multi.toml` save preserves top-level `frames`/`launch`/`mode`.**
  `save_compositor_layout` is read-merge-write via `merge_multi_toml(existing, &layout)`:
  it splats only the panel-owned `fps` + `[[output]]` array (the shape `to_multi_toml`
  still owns) over the on-disk table, so host-authored `frames`/`launch`/`mode` survive;
  an empty layout clears stale outputs. Unit-tested.
- **[✅ done] Global Save + save-on-quit.** `data::save_all_dirty()` flushes every dirty section
  (clearing each flag as its write succeeds, stopping at the first error so the user sees what didn't
  persist). The bottom bar now shows a **Save all** button beside the unsaved badge, and `update()`
  flushes best-effort when the window close is requested — **without** cancelling the close (no risk of
  trapping the window), so edits aren't silently lost on quit.
- **[low] `PanelState::load()` runs `ffmpeg -encoders` + the cached test-decode
  synchronously on the UI thread at startup** (~2 s on first run on a slow box).
  `PanelState` is `Clone`; move the two probes to a background thread + mpsc so the
  window appears instantly. (Resolved-by-design note: a `PHAROS_PANEL_TAB` env var
  was added during integration to deep-link a tab on launch.)
- **[low] `OutputTile.peer_pubkey` and `refresh_mhz` are display-only.** They don't
  round-trip through `multi.toml` (the compositor `OutputCfg` has no such fields);
  a peer pin + per-output refresh need a sidecar file to survive save/reload.

## Overview tab (fleet + liveness)

- **[low] Refresh reloads only `peers.toml`** — host identity/config (hostname,
  NICs, bind_mac, mqtt) stay stale until relaunch. Add a "reload all" path (likely
  in app.rs/data) or call `host_info()`/`load_host_config()` on refresh too.
- **[low] Peer cards are read-only.** Natural follow-ups once write paths exist:
  right-click forget/re-pair (needs `data::remove_peer/save_peers`), per-peer
  Wake-on-LAN (MACs already surfaced), and copy-pubkey.
- **[low] Filter text lives in egui temp memory, not the model** (deliberate, to
  keep the frozen `PanelState` shape). A model field would be cleaner if persistence
  or cross-tab reuse is ever wanted.

## Compositor tab (multi-Trireme drag-and-drop layout)

- **[🔧 in progress] Tile arrangement tools: snap / duplicate / align.** (user-requested)
  The canvas only has free drag + "tile right of this" today. Add: snap-to-grid
  and snap-to-edge (tiles cling to neighbours / a configurable grid so a wall is
  pixel-aligned without fiddling), **duplicate** a selected tile (same size/peer,
  offset placement + next auto-port), and **align** actions (left/right/top/bottom
  edges, center H/V, distribute evenly) for a selected set. Consider multi-select
  (shift-click / rubber-band) so align/distribute act on a group. Likely also a
  "stack/mirror" (same region → duplicate a display) once peer→sink mapping lands.
- **[med] Dropped peer tiles get a generic auto-port, not the peer's real sink
  addr.** `handle_drop()` keeps the auto-incremented `quic://0.0.0.0:<port>`. Once
  liveness/telemetry exists, derive/confirm a meaningful per-peer sink address.
- **[low] Per-display refresh can't be persisted** — `multi.toml` has one shared
  `fps`; `RefreshPolicy::PerDisplay` isn't expressible without a per-output fps field
  in `MultiCfg`/`OutputCfg`. (The control is labelled "display-only" to set
  expectations.)
- **[low] Drop placement is heuristic and can be ~1 tile-margin off** with many
  tiles, because `handle_drop` re-derives the fit-view rather than reading the drawn
  `View`. Stash the drawn `View` in egui temp memory from `draw_canvas` for exactness.
- **[low] No liveness probe feeds the palette chips** (color by `peer.liveness`,
  always `Unknown` today; `request_repaint_after(1s)` means they'll light up for free
  once a probe lands).

## Video tab (encode → transport → decode path)

- **[low] `encode_capabilities` spawns `ffmpeg -encoders` synchronously** (see the
  startup-probe architecture item) — move off the UI thread.
- **[low] No Audio-stage capability rows are emitted by `data.rs`** — the tab filters
  for `PipelineStage::Audio` and falls back to a hardcoded Opus card; add a real Opus
  row (probe libopus) if audio ever becomes multi-codec.
- **[low] Per-peer live route not surfaced** — once telemetry exists, a "live now"
  badge on the active encode+decode boxes (and the QUIC card) would make the tab
  actionable rather than informational.

## HID / Input tab

- **[✅ done] Chord fields are validated against a shared grammar.** New `core_proto::chord` module
  owns the chord grammar; `trireme::Chord::parse` delegates to it (so client + panel can't drift), and
  the panel uses `core_proto::chord::is_valid` to disable the local-escapes Add on a malformed entry,
  render malformed existing chips/hotkey fields red, and explain why. Residual gap (documented): a lone
  misspelled *modifier* like `Cntrl+Alt` is structurally valid (parses as key `Cntrl`) but won't match;
  a full key-name check or a "press to capture" affordance is the follow-up.
- **[low] Mode label/describe is duplicated in-tab** instead of on the model — move
  `label`/`describe` onto `InputCfgUi` (like `ResolutionPolicy`) so a new mode stays
  in sync across tabs.
- **[low] Per-class forward toggles stay editable when `mode == off`** — only an
  amber warning today; `add_enabled(mode != "off", …)` would make the dead state
  clearer (current behavior keeps values across mode flips, which is the safer default).

## Game Controllers tab

- **[✅ done] Gyro feel curve visualiser.** A small egui-painter plot under the accel sliders shows
  output gain vs rotation speed (deg/s), with the knee (`accel_ref`) and clamp ceiling (`1 + accel_max`)
  marked; it dims with the sliders when gyro is off. Pure painter, no new deps. `accel_multiplier()`
  mirrors the host's `gyro_imu::accel_gain` exactly and is unit-tested so the preview can't drift.
- **[✅ done] Seed presets from defaults on first run.** The empty preset state now has a "Create from
  defaults" button that inserts `ControllerProfile::default()` so the Apply-preset picker isn't dead.

## Settings tab (resolution policy + all config)

- **[✅ done] Collapse "Resolution policy" into a normal collapsible category.**
  (user-requested) Now a `🖥 Resolution policy` `CollapsingHeader` (default-open), uniform with
  the other Settings sections.
- **[✅ done] `bind_mac` is now format-validated.** A `mac_is_wellformed()` check (six colon-separated
  hex octets, case-insensitive) drives a red, described warning under the field when it's non-empty but
  malformed — it warns, never blocks. Stops an invalid MAC from landing silently in `config.toml` and
  failing the host bind cryptically. Unit-tested (canonical accepted; dash-form / wrong-length / non-hex
  / empty-octet rejected); 2 new `settings.network.mac_malformed*` keys (en + nb).
- **[low] MQTT password is stored plaintext in `config.toml`** (the UI masks the
  field + notes plaintext-at-rest). Consider a secrets path / keyring, or at least
  `chmod 600` on `config.toml`.
- **[low] `node_id` hint is panel-guessed** (slugified hostname, doesn't collapse `-`
  runs) and may differ from the host's authoritative default — expose the real default
  via `crate::data` if it matters.

## Accessibility & i18n (directives — now project-wide)

Two standing directives were adopted: **(a) every meaningful/interactive item gets a
generous description** via `crate::a11y::Describe` (`.describe(text)` / `.describe_named(name,
desc)`) — shown on hover AND announced by screen readers (AccessKit enabled); **(b) every
user-facing string routes through i18n** (`crate::i18n::tr("key")`, rust-i18n, English default +
fallback, per-module files under `locales/`). New code must follow both.

- **[✅ done] `status_badge` / `theme::badge` now return `egui::Response`.** Both return the painted
  label's `Response` (neither is `#[must_use]`, so the ~25 existing call sites that ignore it stay
  clean under `clippy -D warnings`). Per-badge `.describe()`/`.describe_named()` is now possible; the
  compositor's resolution-policy and peer-pinned badges carry AccessKit descriptions (see below).
- **[✅ done — was stale] Compositor status strings are all i18n'd.** Re-audited: `handle_drop()`
  (`compositor.status.added_for_prefix`/`added_manual_prefix`/`added_blank`), `snap_tile_right()`
  (`compositor.status.tile_right`), and every other `state.status = …` route through `tr()` with only
  dynamic values left in `format!()`. Covered by the i18n completeness test.
- **[low] Painted canvas hot-zones lack AccessKit names.** Compositor tile click/close zones are
  `painter`-drawn `interact()` regions with no per-zone name/description (the whole canvas has a
  `describe_named`, and the editor list below mirrors every tile with described controls, so the path
  is reachable). `.describe_named()` each painted hit-target for completeness.
- **[✅ done] Backfill i18n on the hand-done files.** `input.rs` (67 `tr()` calls) and `settings.rs`
  (109) were already fully keyed by `a56d28e`/`a6370fc` — the [high] claim was stale. `app.rs` was the
  last holdout (top/bottom bar): wordmark, subtitle, unknown-host, peer singular/plural, and the two
  host-description strings now route through `tr()`, with 6 new `app.*` keys added to `locales/app.toml`
  (en + nb). Verified by the new completeness test below — the whole panel is now localised.
- **[✅ done — was stale] Shared chrome i18n is complete.** Re-audited `model.rs`: every enum the item
  listed — Tab, ResolutionPolicy, RefreshPolicy, Theme, Liveness — plus FontScale already routes
  `label()`/`describe()` through `crate::i18n::tr` (keys in `model.toml`, en + nb; landed `a6370fc`).
  `data.rs` capability labels/details are **proper nouns / rig names** (NVENC, Quick Sync, NVIDIA,
  "RTX 4070") and codec/vendor tokens — correctly left verbatim, exactly like `compositor.heading`
  keeps "Compositor" identical across locales. The only non-i18n'd label is `PipelineStage::label()`,
  which is **dead code** (never called — the Video tab matches on the variant, never displays the
  label), so nothing English is actually surfaced.
- **[✅ done — was stale] Second locale exists.** Norwegian (Bokmål, `nb`) landed in `a6370fc` — every
  module locale file carries `en` + `nb`, `display_name`/`available_locales!()` discovery and live
  switching are exercised by `i18n::tests::locales_resolve`. New keys added this session ship both
  languages.
- **[low] i18n for dynamic/plural strings.** Counts like "N peers" use Rust formatting; adopt
  rust-i18n interpolation (`%{name}`) + plural handling if full localisation is pursued.
- **[low] Screen-reader names for painted regions.** The compositor canvas tiles and any future
  painter-drawn interactive area need `.describe_named()` on their interaction handles; audit that
  every painted hit-target exposes a name + description.

## Fonts / icons / i18n robustness (from the icon + hover-description sweep)

- **[✅ done] i18n key-completeness test.** `i18n::tests::every_static_tr_key_resolves` walks every
  `.rs` under `src/` (skipping `i18n.rs`, whose fallback test references a deliberate non-key),
  extracts every static `tr("…")` literal key, and asserts each resolves (a present key never returns
  itself, even via locale fallback). Dynamic `tr(&format!(…))` keys are skipped by shape; a `checked > 50`
  floor guards against the extractor silently matching nothing. Confirmed: the panel has **zero**
  unresolved static keys today.
- **[low] CI guard against tofu glyphs.** A future edit could reintroduce an emoji / uncovered glyph in
  a `.rs` or `.toml` value. Codify the integration scan used here (src + locales vs. bundled-font
  coverage) as a test that fails on any uncovered glyph.
- **[✅ done] Wired the 3 formerly-unused compositor locale keys.** `compositor.toolbar.policy_badge_desc`
  and `compositor.editor.peer_pinned_desc` are now `.describe()`d on their (newly `Response`-returning)
  badges; `compositor.canvas.legend_desc` is attached to a hover-interact region over the painter-drawn
  scale legend (`painter.text` returns its `Rect`), which also gives that painted hot-zone an AccessKit
  name. The i18n completeness test now covers all three.
- **[low] `settings.appearance.language*` keys live in `app.toml`, not `settings.toml`** (referenced by
  the Settings tab). Harmless, but splits the `settings.*` namespace; consolidate later.
- **[low] Sidebar icon choices** — Status = Phosphor `PULSE`, Compositor = `SQUARES_FOUR`; swap if preferred.
