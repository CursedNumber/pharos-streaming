# Pharos / Trireme — HDR10 + VRR + Native-Res + Gyro Implementation Plan (Finalized)

Target session: Xiaomi 14T Pro (charging, performance mode), Flydigi Vader 4 over USB, sub-optimal WiFi both ways. RTX 4070 / Ada host on KDE Wayland / CachyOS.

**Discovery confidence note.** The major anchors below were re-verified against the live tree as of this revision: no rate-control/bitrate plumbing exists in the compositor NVENC path (only `lookaheadDepth` is read — `nvenc.rs:69`, `multi.rs:319/461`); the panel's `bitrate_kbps` (`model.rs:660`, default 20000) is never sent to the host; the client decoder hardcodes `WIDTH=1920`/`HEIGHT=1080` (`HevcDecoder.kt:34-35`, used at `:147`); `AUTH_PROTO_VERSION` is checked with a hard *inequality* `bail!` (`transport:912`); `SetupSession{width,height,refresh_mhz}` exists in `core-proto:49-52` but is **never sent or matched anywhere** (it is a dead M2 stub); `ClockPing/ClockPong` are live (`transport:1154/1810/1901`) and `FrameStageMeta` stamps are host-clock UNIX-nanos (`multi.rs:1404`); the host gyro path is evdev-only (`gyro_imu::GyroImu::spawn` → `imu_loop` → `latest_raw` → `stream_raw_gyro`, `multi.rs:1254`). A handful of inherited line refs (marked `~` below) are approximate and must be re-grepped at edit time; the recent commits `dfb4057`/`190d251` are fresh and may have shifted them. **The FFI signature claim is verified** (`wants_gyro` is the last positional param in both `core-ffi/src/lib.rs:378` and `core_ffi.kt`), but the UniFFI checksum sequencing in W2 is a hard constraint, not a formality.

---

## 0. Decisions locked (2026-06-23)

The user confirmed four scope decisions after the first research pass:

1. **HDR → static HDR10, named plainly "HDR".** Drop the "HDR10+ Gaming" name (trademark + structurally impossible over a stream). See §1.
2. **Gyro → phone reads it via the standard Android sensor API (NS mode); host injects a new virtual DualSense.** Empirically verified live 2026-06-23 (§0b/§1/W9): NS mode = Switch-Pro `057e:2009`, gyro+accel exposed as `InputDevice` sensors. No USB/IP, no raw-HID, no spike needed. (Earlier "host-side USB/IP path B" is obsolete.)
3. **Refresh → 120 Hz locked baseline; 138/144 as a runtime-detected bonus.** See W8 (corrected below — this panel has no *continuous* VRR).
4. **Per-app resolution → PURSUED NOW** (overriding the earlier "global-output-only" recommendation). Delivered via `NvEncReconfigureEncoder` on the live session inside a 4K max envelope — no session re-init. See the rewritten **W7**.

## 0b. Device ground truth — Xiaomi 14T Pro (`rothko`), verified over ADB 2026-06-23

These replace the web-sourced specs. Scouted read-only via `dumpsys display` / `thermalservice` / codec XML / `getprop`.

- **SoC / GPU:** MediaTek **Dimensity 9300+** (`MT6989`), **Mali** (Immortalis-G720) — *not* Snapdragon. Decoders are `c2.mtk.*`.
- **OS:** **Android 16, SDK 36**, HyperOS 3.0 (`OS3.0.8.0.WNNEUXM`). Newest API surface — use `Display.Mode.getSupportedHdrTypes()` (API 34+) for the W1 caps probe.
- **Panel:** physical **1220×2712 portrait** → **2712×1220 landscape, 20:9** (user's spec confirmed). Density 520 (3.25×). Front-camera **cutout 138 px top-center** (becomes a left/right-edge notch in landscape — relevant to W7 UI safe-area, irrelevant to a letterboxed video surface).
- **Refresh:** **discrete seamless modes 60 / 90 / 120 / 144 Hz, all at full 1220×2712**, all in one seamless-switch group (`alternativeRefreshRates`). **`hasArrSupport=false` → NO Adaptive/continuous VRR.** OS **default peak refresh = 120** (`mRefreshRateData defaultPeakRefreshRate:121`); the 144 mode exists and is selectable but is gated above the OS default (app-request / Game-Turbo). **This is the big correction — see §1 VRR.**
- **HDR:** `supportedHdrTypes=[1,2,3,4]` on **every** mode = **Dolby Vision(1), HDR10(2), HLG(3), HDR10+(4)** — our HDR10 target is fully supported, even at 144 Hz. `hdrCapabilities`: **mMaxLuminance=500, mMaxAverageLuminance=500**, mMinLuminance=0. `supportedColorModes=[0,7,9]` (default / Display-P3 / wide). **The 500-nit advertised ceiling is the client-side tone-map target** — feeds W3 mastering-metadata defaults and W4.
- **HW decoders (`c2.mtk.*`):** HEVC (`c2.mtk.hevc.decoder` + `.lowlatency` + `.secure`), **AV1** (`c2.mtk.av1.decoder` + `.lowlatency` + `.secure`), VP9, AVC; plus Dolby-Vision (`c2.dolby.decoder.hevc`). So **HEVC Main10 HDR10 *and* AV1 10-bit HDR are both HW-decodable** — AV1-HDR is a live option, and there's a **`hevc.decoder.lowlatency`** variant worth selecting in W4. (Main10 profile to be confirmed at runtime via `MediaCodecList`, but it is effectively certain on this SoC.)
- **Thermal:** idle `THERMAL_STATUS_NONE`; skin/battery ~31–33 °C, CPU/GPU ~46 °C. **SKIN is the gating sensor** — `LIGHT` at **53 °C**, `MODERATE` 55, `SEVERE` 57 (CPU/GPU don't throttle until 85 °C). **W8b should react on `THERMAL_STATUS_LIGHT` (~53 °C skin)**, not wait for CPU temps.
- **Battery:** 100%, **not charging at scout time** (status discharging; user had it charging earlier — no impact on the plan).
- **Vader 4 (plugged into the phone, all 3 USB modes scouted live):**
  - **NS mode `057e:2009` (CHOSEN):** kernel `hid-nintendo` decodes the IMU → a separate "Pro Controller IMU" input node (`INPUT_PROP_ACCELEROMETER`, `MSC_TIMESTAMP`) → Android exposes **GYROSCOPE + ACCELEROMETER as `InputDevice` sensors** (`Sensor Input Mapper`, hardware-timestamped). 16-bit sticks. **Triggers digital only** (Switch-Pro protocol — the pad's analog/digital trigger toggle is irrelevant in NS mode). Nintendo face layout. → gyro is trivially readable on the phone.
  - **DInput mode `04b4:2412` (Flydigi VADER4):** analog triggers (`ABS_GAS`/`ABS_BRAKE`) + back paddles (`BTN_TRIGGER_HAPPY1-4`) + a mouse HID interface, **but NO sensor exposure** (gyro only in the raw `0x04 0xfe 0x66` HID report) and **8-bit sticks** (0-255).
  - **XInput mode:** not captured (predictable Xbox360 `045e:028e` — analog triggers, no IMU); skipped as useless for the gyro goal.
  - **Decision:** NS mode (native gyro + 16-bit sticks > analog triggers). See §1/W9.

## 1. Reality check & scope

### HDR: "HDR10+ Gaming" is not shippable; HDR10 (static) is.

**Verdict: drop the "HDR10+ Gaming" name entirely. Ship "HDR (HDR10)".**

"HDR10+ GAMING" is a trademarked, closed, render-time contract between three independently certified components in one box: a certified game engine, a certified GPU/driver, and a certified *directly-connected* display. The engine learns the panel's exact peak-nits, does Source-Side Tone Mapping, and flips the display into a no-tone-map reference mode — all negotiated over the local HDMI/DP link **before any frame is captured**. The moment we capture into a virtual framebuffer and ship it over WiFi to a phone, that contract is structurally broken. Calling our stream "HDR10+ Gaming" would be a false certification claim and trademark misuse.

**What is genuinely shippable** (and is what Sunshine/Moonlight ship): **static HDR10** — 10-bit HEVC Main10, BT.2020 primaries (VUI `colour_primaries=9`), PQ/ST-2084 transfer (`transfer_characteristics=16`), BT.2020-NCL matrix (`matrix_coeffs=9`), plus a Mastering Display Colour Volume SEI (HEVC payloadType 137) and Content Light Level SEI (payloadType 144). The RTX 4070's 4th-gen Ada NVENC has hardware Main10; the in-tree `nvidia-video-codec-sdk 0.4.0` (NVENCAPI 12.1) exposes `NV_ENC_HEVC_PROFILE_MAIN10_GUID`, the 10-bit buffer formats, `hevcVUIParameters`, and `seiPayloadArray`. The Xiaomi 14T Pro is an HDR10/HDR10+/DV panel and receives HDR10 fine.

- **Shippable:** static HDR10, capability-gated per client, 10-bit Main10 encode, BT.2020/PQ VUI + 2086/CLL SEI re-emitted on every IDR.
- **Out of scope:** HDR10+ dynamic metadata generation (no per-frame luminance-analysis source; the ffmpeg/SDK nvenc path can't inject it). HDR10+ passthrough (no source emits ST-2094-40). Dolby Vision.

### **HDR is only meaningful when the SOURCE is HDR — and we cannot detect that today.**

A correction to the draft's safety story: gating on `wants_hdr && host_hdr_capable` does **not** guarantee good output. W1 gates the client *toggle* on display+decoder caps; it does not establish that the **game's actual surface** is HDR. If a user with a capable phone toggles HDR on and launches an **SDR** game, the host would encode an SDR signal as Main10/PQ → washed-out, over-dark output. There is no "is the content HDR" probe anywhere in the tree, and Wayland gives no portable per-surface "is this PQ" signal on a headless compositor.

**Resolution for this milestone:** treat HDR as a **user-asserted, per-app opt-in**, not an auto-detected property. The HDR path is only armed when (a) the client toggle is on AND (b) the launched app is flagged HDR in its `AppEntry` (new `hdr: Option<bool>` field, see W7). A bare desktop or an unflagged app stays SDR even if the toggle is on. This makes "wrong tone mapping on SDR content" a deliberate per-app choice the user controls, not a silent default. Document this clearly in the panel and the W1 toggle summary ("Use only for HDR games"). **Open Question added (Q11).**

### Gyro: EMPIRICALLY SOLVED — the phone reads it natively in NS mode (no USB/IP, no raw-HID).

**Verdict (verified live over ADB 2026-06-23, decision locked to NS mode):** plug the Vader into the **phone** and set it to **NS mode**; it enumerates as a genuine **Nintendo Switch Pro Controller (`057e:2009`)** and the phone's kernel `hid-nintendo` driver fully decodes its IMU into a second input node ("…Pro Controller IMU", `INPUT_PROP_ACCELEROMETER`, `MSC_TIMESTAMP` hardware timestamps, accel ABS_X/Y/Z + gyro ABS_RX/RY/RZ). Android's InputReader exposes this as a **standard `InputDevice` GYROSCOPE + ACCELEROMETER sensor** (confirmed: `Sensor Input Mapper` with `GYROSCOPE`, `mHasHardwareTimestamp 1`). So the Trireme client reads gyro with `inputDevice.getSensorManager().getDefaultSensor(TYPE_GYROSCOPE)` — **no raw HID, no USB/IP, no root, no `hidraw`, no udev rule.**

This **overturns the old "Android controller IMU is walled" assumption** — that was X90J/Android-12-specific. On **Android 16 + Switch-Pro mode** the IMU is a first-class sensor.

**Two facts still shape the host side:**
1. **The host's virtual Xbox 360 pad (`045e:028e`, `gamepad.rs:88`) has no motion axes** — on Linux/Proton games read gyro only from a virtual DualSense/DualShock4. So we still add a **new virtual DualSense sink** on the host; only the gyro *source* changed (phone sensor, not host hidraw).
2. **NS-mode trade-offs (measured, accepted):** digital triggers only (Switch-Pro protocol has no analog trigger channel — the pad's analog/digital toggle is irrelevant here), 16-bit sticks (good for aim), Nintendo face layout (needs the existing A/B·X/Y swap profile), and the device reports as **Nintendo `057e:2009`, not Flydigi** (the classifier must key on the Switch-Pro VID/PID or the IMU-sensor presence, not `contains("flydigi")`).

Architecture: **phone reads gyro/accel via `InputDevice` sensors → relays over the wire → host injects into the new virtual DualSense → Steam Input gyro-to-mouse/native.** The entire raw-HID + USB/IP + `gyro_hid.rs` half of the earlier draft is **deleted**. (DInput mode — `04b4:2412`, analog triggers + paddles + raw-HID gyro + 8-bit sticks — remains the documented fallback if analog triggers ever outrank gyro; not chosen.)

### VRR: there is **no continuous VRR** on this panel — "~138" does not exist. The target is a **fixed seamless mode (120 default, 144 bonus)**.

**Corrected by the ADB scout (§0b):** the 14T Pro reports `hasArrSupport=false` and exposes **only discrete refresh modes 60 / 90 / 120 / 144**, all in one seamless-switch group. There is **no continuous VRR window**. Blur Busters' "~138 = max − 3" heuristic applies to desktop monitors with a *range*; it is meaningless for this internal panel. So:

- **The real target is a single fixed mode picked from {60, 90, 120, 144}, switched seamlessly.** The OS **default peak is 120** (`defaultPeakRefreshRate:121`); 144 exists but sits above the OS default and is realistically **Game-Turbo / app-request gated**. **Design for 120 Hz locked; treat 144 as a runtime-detected bonus.**
- Because there is no VRR range, smoothness over jittery WiFi comes from **timestamp-paced presentation (clock-mapped) + a small jitter buffer at a fixed cadence**, not from the display tracking a variable rate. Setting the host `cfg.fps` to a rate the panel isn't actually running produces a beat-stutter (Sunshine #2286 / #1545) — so the host pace must follow the **client-reported actual present rate** (W8), and `setFrameRate()` must use `CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS`.
- **VRR↔HDR mutual-exclusion is moot** (no VRR to lose). What *can* happen is a refresh-mode change on HDR engage; the W8 gate becomes "confirm the chosen fixed mode (e.g. 120) holds with HDR active," not "does VRR survive HDR."

### Native resolution: per-app resolution is **IN SCOPE** (user decision #4) — via live `NvEncReconfigureEncoder`, not a session re-init.

2712×1220 landscape (20:9, native) is just dimensions the Smithay headless `OutputMode` (`multi.rs:399-419`) accepts (no EDID validation). The earlier draft descoped *per-app* resolution because a naive runtime remode rebuilds the pipeline and re-inits NVENC (the SIGSEGV path). The W7 deep-dive found a path that **avoids re-init entirely**: create the NVENC session once at startup with a **4K max envelope** (`maxEncodeWidth/Height`), then on each app launch call **`NvEncReconfigureEncoder`** on the *live* handle to change `encodeWidth/Height` (≤ envelope) + re-register the CUDA input resource — no `nvEncDestroyEncoder`, no `start_session`. The proto already defines `ModeChange`/`SetupSession` and PAL already declares `VirtualDisplay::set_mode` (both currently unwired). **This is delivered as the rewritten W7**, gated on a SIGSEGV-avoidance spike with a clean-process-respawn fallback (B) that is safe because a fresh process gets a clean first-init. `AppEntry` gains `width/height/refresh` (and an `hdr` flag for content gating). **Open Question (Q4) updated accordingly.**

### Debug variant: straightforward Gradle work.

No `buildTypes`/`flavors` exist (`androidApp/build.gradle.kts`). Add a debug variant with `applicationIdSuffix` to co-install beside release, plus an on-screen metrics HUD. Low risk.

---

## 2. Architecture overview — end-to-end HDR10 + 10-bit pipeline

```
[Host: Pharos compositor]                          [Wire: QUIC]            [Client: Trireme Android]
Smithay GLES render
  └─ 10-bit target tex (Fourcc::Abgr2101010)   ── W3: capture surface + COLOR-MANAGED
        multi.rs:423  (today Abgr8888)               PQ preserve (W3a, see below)
  └─ GL→CUDA bridge (cuegl.rs)                  ── W3: 10-bit layout
        copy_gl_texture_to_linear (~122-185)
  └─ NVENC session (multi.rs:315/457)           ── W3 + NVENC-OWNER coordination
        NV_ENC_BUFFER_FORMAT_ABGR → ABGR10
        + hevcProfile MAIN10 + VUI + SEI/IDR
        + RATE CONTROL (W3b: NEW capped-VBR
          target — does NOT exist today)
        │
        ▼ mode advertisement (W2b: SetupSession)  v3 auth handshake (W2)
  host → client: actual w/h/refresh/hdr  ───────►  wants_hdr byte after wants_gyro
        │                                           transport/src/lib.rs:932/1955
        ▼ codec/HDR state                                  ▲
  control-ipc HostStatus.hdr_active  ◄───────────  gated on DisplayCaps.supportsHdr10 (W1)
        (multi.rs ~1542)                            && findDecoderForFormat(Main10HDR10)
        │                                                  │
        ▼                                          decode dims from SetupSession/first-AU
  pharos-panel Video pane (W6) + cards (W5)          (W7b: NOT the 1080 constant)
                                                           │
                                          MediaCodec HEVC Main10 decode (W4)
                                            HevcDecoder.kt:146 + dynamic w/h
                                            └─ SurfaceView → present off
                                               source-ts mapped via ClockPong offset (W8)
                                          status badge in HostsPane (W5)
```

### Files/structs to touch, by crate

**`core-proto`** (`crates/core-proto/src/lib.rs`)
- `PROTOCOL_VERSION` (line 21) — **do not bump** (HDR rides the auth wire, not `ControlMsg`).
- `ControlMsg::SetupSession{width,height,refresh_mhz}` (lines 49-52) — **currently a dead stub; wire it up** in W2b (add `hdr: bool`) for host→client mode advertisement.
- `latency.rs:80` `WireLatencyReport.codec_fourcc` — add a separate `hdr_mode` field (W6); host+client bench builds move in lockstep (postcard field-add compat callout in W6).

**`transport`** (`crates/transport/src/lib.rs`)
- `AUTH_PROTO_VERSION` (line 320) → **3** (see W2 for the hard-equality consequence).
- Host parse (lines 907-934): read `wants_hdr` after `wants_gyro`.
- Client send `do_video_auth` (lines ~1946-1968): write the byte.
- Mirror `wants_gyro: Arc<AtomicBool>` (lines 380/449/561/1002/1314) with `wants_hdr`; reset on disconnect.
- `FrameStageMeta` already carries host-clock capture stamps (sent at `multi.rs:1404`); `ClockPing/Pong` offset math lives at `:1810`. W8 reuses both — no new wire field for source-ts.

**`compositor`** (`multi.rs`, `nvenc.rs`, `cuegl.rs`, `gamepad.rs`)
- `multi.rs:423` capture Fourcc; `:315`/`:457` NVENC session format; `:244-267` `apply_codec_lowlatency` (SEI on IDR); `:1323` copy_framebuffer Fourcc; `:1033-1054` `SetVideoCodec` handler; `~1542` HostStatus write. (`:1254` `stream_raw_gyro` = host-attached-IMU path only; the Vader gyro now originates on the client — W9.)
- `nvenc.rs:51/69` — NVENC init + **new rate-control config (W3b)**.
- `cuegl.rs:122-185` GL→CUDA 10-bit layout.
- `gamepad.rs:58-98` — new virtual DualSense sink (W9); `core-proto/controllers.rs:110-112` classifier → add a Switch-Pro `057e:2009` branch (W9). Gyro *source* is the client `InputDevice` sensor — NO host-side `gyro_hid.rs`/hidraw.

**`control-ipc`** (`crates/control-ipc/src/lib.rs`)
- `VideoCodec` enum (208) — keep HDR orthogonal; add `SetHdrMode` request + `HostStatus.hdr_active: Option<String>` (`#[serde(default)]`).
- Add `SetBitrate`/plumb existing `bitrate_kbps` so the panel value reaches the host (W3b — this path does **not** exist today).
- `LatencyReport` (440-454) — add `hdr: Option<String>` (`#[serde(default)]`).

**`pharos-panel`** (`model.rs`, `tabs/video.rs`, `tabs/overview.rs`, `tabs/settings.rs`, `data.rs`)
- `model.rs:269-293` `CapabilityUi` — add `hdr: Option<bool>` + `encoder_variant`.
- `model.rs:660`/`settings.rs:201` `bitrate_kbps` — plumb to host (W3b).
- `video.rs` matrix/cards/filter (W6); `overview.rs:681-708` HDR status (W5).

**Client** (`client/androidApp/...`)
- `MainActivity.kt:375-430` `configureRefresh()` — HDR caps probe (W1); actual-present-rate report (W8).
- `HevcDecoder.kt:34-35/61-70/146-163` — **dynamic decode dims** (W7b) + HDR fields/config (W4).
- `StreamHub.kt:74-95/447-484` — `hdrSupported` flow, `wantsHdr` on dial; **gyro telemetry seam** (W9).
- `SettingsPane.kt` — gated HDR toggle (W1).
- `HostsPane.kt:378-415` — HDR badge (W5).
- `core_ffi.kt` — **regenerated in lockstep** `startVideoStream(..., wantsHdr)` (W2).
- `androidApp/build.gradle.kts` — buildTypes (W10).

---

## 3. Workstreams

> NVENC/driver-owner coordination is flagged ⚠️. Treat W3 as *their* deliverable with this plan as the integration contract.

### W1 — HDR capability detect + toggle in Trireme

**Goal:** Show an HDR toggle in Settings only when the device display *and* a Main10HDR10 decoder both support it.

**Changes:**
- `MainActivity.kt` `configureRefresh()` (375-430): query `display.getHdrCapabilities().getSupportedHdrTypes()` (API 24-33) / `display.getMode().getSupportedHdrTypes()` (API 34+; keep both branches). Flag `HDR_TYPE_HDR10 (=2)`.
- Probe a decoder: `MediaCodecList(REGULAR_CODECS).findDecoderForFormat(fmt)` with `fmt.KEY_PROFILE = HEVCProfileMain10HDR10`. Gate on BOTH display-type AND decoder-found.
- `HevcDecoder.kt` `DisplayCaps` (61-70): add `hdrTypes`, `supportsHdr10`; update `NONE`.
- `StreamHub.kt`: `hdrSupported: StateFlow<Boolean>`; `Session.hdrEnabled` + persist to SharedPreferences `hdr10_enabled` (mirror gyro at 799-806).
- `SettingsPane.kt`: `SettingRow` between gyro and controller rows, gated on `hdrSupported`. **Compose-TV lesson: the ROW is the click target, state is trailing display-only `Text`, no `Switch`.** Summary: "Use only for HDR games — requires an HDR10 display + host support." Apply on next dial.

**Back-compat:** none (local detection + prefs).
**Risks:** vendors report bit depth only after first frame; keep the toggle *hidden* (not disabled) when caps absent (gyro pattern). **Note: this gate proves the device CAN do HDR, not that the content IS HDR** — see Section 1 / Q11; content gating is per-app in W7.
**Decision:** per-host (`ServerEntry` field) recommended.

### W2 — Wire/proto HDR negotiation + **honest** back-compat story

**Goal:** Client advertises it wants HDR; host enables Main10 only when the active client asked, both sides support it, and the app is HDR-flagged; clean SDR fallback.

**Changes (the `wants_gyro` template is exact precedent):**
- `transport/src/lib.rs`: bump `AUTH_PROTO_VERSION` (320) `2 → 3`. Client `do_video_auth` (~1946-1968): after `wants_gyro`, write `wants_hdr`. Host parse (907-934): after reading `wants_gyro` (only when `head == AUTH_PROTO_MAGIC`), read trailing `wants_hdr`; default false.
- Add `wants_hdr: Arc<AtomicBool>` mirroring `wants_gyro` (380/449/561/1002/1314); reset on disconnect (1314).
- `control-ipc`: `HostStatus.hdr_active: Option<String>` (`#[serde(default)]`). Keep HDR orthogonal to codec via a new `SetHdrMode { mode: HdrMode }` request.

**Back-compat — CORRECTED (the draft was wrong on this):**
- `AUTH_PROTO_VERSION` is checked at `transport:912` with a **hard inequality** (`if version != AUTH_PROTO_VERSION { bail!(...) }`), **not** `>=`. Consequences:
  - **v3 client → v2 host = HARD REJECT**, not a graceful SDR degrade. The connection is refused; the user sees a failure, not SDR.
  - **v3 host → v3 client (or v2 client via the v1/v2 magic split): fine.** A v2 client omits the trailing byte → host short-reads → would error unless the read is made tolerant. **Therefore the host must read `wants_hdr` with a non-fatal "EOF ⇒ false" path** (treat `read_exact` failure on that single trailing byte as `wants_hdr=false`), exactly so a v2 client (which sends the byte stream ending after `wants_gyro`) still authenticates. This is the only direction that degrades gracefully.
- **Because host and client are built from one tree, the practical guidance is: ship both ends together.** The only realistic skew is a debug build on one side; the test plan adds an explicit v2↔v3 skew check. Do **not** claim "graceful degrade in both directions" — it is one-directional (newer host tolerates older client; older host rejects newer client).
- **Do NOT bump `core-proto:21 PROTOCOL_VERSION`** (a hard `Hello` break). `control-ipc`'s lenient reader handles the new request variant on older hosts.

**UniFFI checksum sequencing (hard constraint, not low-friction):** adding `wantsHdr` to `start_video_stream` changes the UniFFI **interface checksum**. The Rust scaffolding (`core-ffi/src/lib.rs:378`) and the generated Kotlin (`core_ffi.kt`) **must be regenerated in lockstep from one tree** — a checksum drift is a **hard runtime panic** at FFI init, not a soft failure. Sequence: edit Rust signature → regenerate bindings → rebuild both host and client from the same commit. Never hand-edit only one side.

**Graceful SDR fallback (core safety property):**
- Host gates Main10 on `wants_hdr && host_hdr_capable && app_is_hdr_flagged` (the third clause is new — Section 1 / W7).
- If the client didn't ask, or the app isn't HDR-flagged, encode SDR (unchanged path).
- ⚠️ **NVENC re-init = relaunch, not in-process rebuild.** Switching SDR↔Main10 changes the session format; per `dfb4057` + the SIGSEGV lesson, in-process NVENC re-init crashes libnvcuvid. HDR mode changes must trigger a **relaunch** (like `SetVideoCodec`, `multi.rs:1033-1054`). Hard constraint for W2/W3.

**Risks:** atomic-flag race (encoder gates on connected AND hdr_active); reconnect into a long-running Main10 re-init is the SIGSEGV corner — force clean relaunch + run the libnvcuvid wedge watchdog (3× >200ms encodes) here too, not only on reconnect.

### W2b — Mode advertisement (host → client) **[NEW — fills the no-negotiation gap]**

**Goal:** The client must learn the **actual** width/height/refresh/HDR the host is streaming, and the host should learn the client's panel-native preference. There is no such path today (`SetupSession` is a defined-but-unused stub).

**Changes:**
- **Host → client (primary):** on stream setup and on any resize, send `ControlMsg::SetupSession { width, height, refresh_mhz, hdr }` (extend the struct at `core-proto:49-52` with an `hdr: bool` field, `#[serde(default)]`). The client uses these to (a) size the decoder (W7b), (b) drive `setFrameRate` (W8), (c) confirm HDR for the badge (W5). This makes the geometry/refresh explicit rather than "the client just knows."
- **Client → host (preference, best-effort):** the client may include its panel-native res/refresh in the existing capability hint or a new `ControlMsg` so the host *could* honor a panel-native request; for this milestone the host config (`multi.toml`) remains the source of truth for the actual output, so the client request is advisory only. State plainly: per-client output sizing is not delivered (single-client/serial host, immutable OutputMode) — the client adapts to whatever mode the host advertises.

**Back-compat:** older clients that never read `SetupSession` keep their current behavior (they would mis-size — which is exactly why W7b makes decode dims dynamic; an older client paired with the native-res output is a known-incompatible combination and must be rebuilt from the same tree).
**Risk:** if `SetupSession` arrives after the first AU, the decoder may have already been configured — W7b must derive initial dims from the first access-unit SPS if `SetupSession` hasn't landed yet (belt-and-braces).

### W3 — Host 10-bit HDR encode path ⚠️ NVENC/driver-owner deliverable

**Goal:** Real 10-bit BT.2020/PQ HEVC Main10 with static HDR10 SEI. Owned by the NVENC/driver process; this plan is the integration contract.

**Changes (their scope, our integration points):**
1. **Capture surface to 10-bit:** `multi.rs:423` `Abgr8888 → Abgr2101010` (and `:1323`), behind the `wants_hdr`/hdr flag; SDR stays default. **This is NOT "like today":** the GBM allocator format, the GLES render target, and the EGL-image import in `cuegl.rs` must all support `Abgr2101010`, and **GLES 10-bit rendering is not guaranteed on all NVIDIA drivers** — this needs a standalone validation spike before the rest of W3 commits. `pal/src/lib.rs` has a `P010` enum + `create(mode, hdr)` scaffold to reuse.
2. **GL→CUDA bridge** (`cuegl.rs:122-185`): handle the 10-bit layout. ABGR10 packed (A2B10G10R10, single-plane, NVENC color-converts) vs P010 2-plane. Recommend ABGR10 packed to minimize bridge rework — but note the **8-bit-in-10-bit-out trap** (widening the encoder format alone, with an 8-bit source, yields no real precision). The whole chain must be 10-bit.
3. ⚠️ **NVENC session** (`nvenc.rs:51`, `multi.rs:315/457`): `NV_ENC_BUFFER_FORMAT_ABGR → ..._ABGR10`; `profileGUID = NV_ENC_HEVC_PROFILE_MAIN10_GUID`.
4. ⚠️ **VUI:** `hevcVUIParameters` — `videoSignalTypePresentFlag=1, colourDescriptionPresentFlag=1, colourPrimaries=9, transferCharacteristics=16, colourMatrix=9, videoFullRangeFlag` per source.
5. ⚠️ **Static SEI on IDR:** Mastering Display Colour Volume (type 137, 24-byte) + Content Light Level (type 144). Re-emit every IDR (aligns with `repeatSPSPPS=1`, `multi.rs:241`) so mid-stream joiners resync ≤1s. Do **not** use a post-encode patcher (corrupts streams with repeating NAL/SEI).

### W3a — Color-managed PQ capture on KDE Wayland **[NEW — the biggest HDR risk, was unplanned]**

Widening the Fourcc is **necessary but far from sufficient.** On KDE Wayland, the compositor (which here *is* Pharos/Smithay) must actually keep the client app's HDR swapchain in scRGB/PQ; otherwise the game's HDR output is tone-mapped to SDR **before** it reaches the 10-bit NVENC target, and we ship a worse picture than SDR.

- **What this requires:** Smithay-side support for the Wayland **color-management protocol** (`wp_color_management_v1` / `color-representation`) so a Proton/Wine game can declare a PQ/BT.2020 surface and have Pharos preserve it into the 10-bit framebuffer instead of flattening to sRGB. Pharos owns the compositor, so this is *our* code, not a driver feature.
- **Scope reality:** this is the single hardest item in the HDR set and may not land cleanly this milestone. If full color-management protocol support is too large, the fallback is to validate that **a specific Proton title with a known HDR swapchain reaches the 10-bit target without SDR flattening** (measured with the PQ test pattern in the test plan) and ship HDR only for that validated path — explicitly marking it experimental. **Do not ship HDR as "working" until the fake-HDR test (Section 5) passes on a real HDR game**, not just a synthetic pattern.

### W3b — Rate control / bitrate target **[NEW — W8's "dynamic bitrate" premise does not exist today]**

**The draft (and W8) assumed `PHAROS_BITRATE_KBPS` is consumed by the host. It is not.** Verified: there is no bitrate target, no RC mode, no VBV/maxBitRate, and no `PHAROS_BITRATE_KBPS` consumer anywhere in the compositor NVENC path — RC is purely P4-preset-heuristic-driven (`multi.rs:319/461`/`nvenc.rs:69` read only `lookaheadDepth`). The panel's `bitrate_kbps` (`model.rs:660`) is never plumbed to the host. At 2712×1220@120-144 + 10-bit, the host would otherwise emit an **uncapped preset-heuristic bitrate** straight into the bounded QUIC flow-control window (`PHAROS_STREAM_WINDOW_KB`) — the exact recipe for the ~2s backlog the reference build fought.

**This is new, in-scope work** (not an optional knob), and it gates W7/W8 over WiFi:
- ⚠️ Add an explicit NVENC rate-control config: set `rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR` or **capped-VBR** with `averageBitRate` + `maxBitRate` + a VBV (`vbvBufferSize`) sized to the QUIC window, so a bandwidth dip drops quality, not frames. Wire it into both NVENC init sites.
- Plumb the panel's `bitrate_kbps` to the host via a new `control-ipc::SetBitrate` request (and/or honor `PHAROS_BITRATE_KBPS` env at startup), and surface it as `HostStatus`.
- **Compute the budget (was missing):** 2712×1220 ≈ 3.31 MP ≈ 1.6× 1080p; at 120 fps (144 stretch) in 10-bit HEVC this is a heavy payload over "sub-optimal WiFi." Suggested defaults for capped-VBR: target ~35-45 Mbps, max ~60 Mbps at native-res HDR (tune live), with the VBV ≤ the QUIC window bytes so the window can't be exhausted. SDR/1080p stays at today's preset behavior unless the user sets a cap.
- **Adaptive on loss:** the host should be able to step the target down when the client reports rising queue depth / packet loss (reuse the W8 actual-present-rate / W10 metrics back-channel). Without this, W8's "smoothness" story has no mechanism.

**Mastering-display/CLL source — CORRECTED (was "pull from EDID", which is fictional here):** a **headless Smithay compositor has no physical connector and no monitor EDID** to read MaxCLL/MaxFALL/mastering primaries from (synthesized EDID per the lessons). So "pull from host EDID" is not available. The metadata must come from one of: (a) the game's own HDR metadata if the color-management protocol surfaces it (best, ties to W3a), or (b) sane **configured defaults** exposed in `multi.toml`/panel: BT.2020 primaries, mastering max-luminance 1000 nits / min 0.005, MaxCLL 1000, MaxFALL 400. Guessed defaults risk mis-tone-mapping on the client; flag this and prefer pulling from the game surface where W3a makes it available. **Open Question (Q8) reframed.**

**Encode-latency gate (was unmeasured/ungated):** encode latency at 2712×1220@120 (144 stretch) Main10 is unknown and may blow the ~3-6ms baseline into 10-20ms+. **Add a hard validation gate with a defined fallback ladder** (this is a blocking pre-condition for shipping native-res HDR):
- Bench encode latency at the target mode (W6 instrumentation + W10 HUD).
- If it exceeds budget, fall back in this order: **144 → 120 fps**, then **120 + 10-bit → 120 + 8-bit (SDR)**, then **native-res → 1080p**. The same libnvcuvid wedge watchdog (3× >200ms) guards this mode at runtime, not only at reconnect.

**Wire/back-compat:** none directly (enabled by W2's flag). Bitstream is self-describing (W4 relies on this).
**Coordination:** Windows (`pharos-win/encode.rs`) HDR fully out of scope (WGC/ddagrab SDR-only).
**Who owns the `cuegl.rs` 10-bit change — Open Question (Q9).**

### W4 — Android HDR Main10 decode + HDR surface output

**Goal:** Decode Main10 HDR10 and present HDR to the panel. **Device-confirmed (§0b):** the 14T Pro has HW `c2.mtk.hevc.decoder` (incl. a `.lowlatency` variant), HW AV1 (`c2.mtk.av1.decoder`), reports HDR types `[1,2,3,4]` (HDR10 ✓ at all refresh rates incl. 144), and a **500-nit** advertised HDR ceiling — all on **Android 16 / SDK 36**.

**Changes:**
- **Prefer the self-signalling path:** if the host emits a fully self-describing Main10 stream (W3: VUI + SEI on IDR), the decoder reports color aspects + `KEY_HDR_STATIC_INFO` on the OUTPUT format — no hand-built CTA-861.3 blob. `HevcDecoder.kt:146-163`: when HDR negotiated, set `KEY_PROFILE = HEVCProfileMain10HDR10`; optionally set input color keys but rely on in-stream signalling first. **Confirm Main10 via `MediaCodecList`** at probe time (W1) — near-certain on this SoC but verify.
- **Prefer the MediaTek `.lowlatency` HEVC decoder** where selectable (matches the project's low-latency posture); confirm it supports Main10 (the base `c2.mtk.hevc.decoder` does). **AV1 10-bit HDR is a viable alternative path** on this phone (HW `c2.mtk.av1.decoder`) — keep it in mind for the codec×HDR matrix (W6), though HEVC HDR10 is the primary target.
- **SurfaceView only** (already used, `MainActivity.kt:490`). Never route HDR through TextureView/GL copy (silently SDR-tone-maps on Android 13+). Do **not** call `Window.setColorMode` (no effect on SurfaceView).
- Handle `onOutputFormatChanged()`: read back `KEY_COLOR_STANDARD/_TRANSFER/_RANGE` + `KEY_HDR_STATIC_INFO` to **confirm** HDR engaged (the only programmatic confirmation) → feed W5 badge / W10 HUD.
- SDR fallback: if display can't do the type, request an SDR stream (W2 gating). On-device tone-map (`KEY_COLOR_TRANSFER_REQUEST`) is device-variable — probe, don't assume. **The 500-nit ceiling is the realistic client tone-map target** — content mastered at 1000 nits will be tone-mapped down by the panel; W3 mastering metadata should be honest about this so the client doesn't crush highlights.

**Risks:** non-tunneled decode can flatten HDR (verify SurfaceView path on the 14T Pro); HDR overlay may add display latency (measure e2e — less relevant than the X90J floor); decoder rebuild on a live HDR toggle stalls video (hence apply-on-dial); MediaTek codec quirks differ from the Snapdragon/Qualcomm assumptions in most streaming-client lore — validate on `c2.mtk.hevc.decoder` specifically.

### W5 — Status cards (panel + client) HDR indicator

**Goal:** Surface live HDR state on both ends.

**Changes:**
- **Panel** (`overview.rs:681-708`): append HDR from `HostStatus.hdr_active` to the pipeline line (~702), e.g. "NVENC HEVC HDR10 · 60fps". Use `status_badge()` (widgets.rs:48-53).
- **Client** (`HostsPane.kt:378-415`): HDR badge as a **focusable sibling** after the pace text (407-414), NOT nested in a clickable container (Compose-TV lesson). Read negotiated HDR from `StreamHub` state (populated from W4's `onOutputFormatChanged` confirmation, not just the request). Announce via the polite live region.

**Back-compat:** depends on W2's `HostStatus.hdr_active` (`serde(default)`).
**Risk:** stale badge if host switches HDR without updating `HostStatus` — write `hdr_active` at the `video_codec` site (`multi.rs ~1542`).

### W6 — pharos-panel Video pane redesign + benchmark HDR field

**Goal:** Split cards by encoder+HDR and codec+HDR, make the pane filterable, add an HDR field to benchmark reports.

**Changes:**
- `model.rs:269-293` `CapabilityUi`: add `hdr: Option<bool>` + `encoder_variant` (`serde(default)`). Label "NVENC HEVC HDR10" / "… SDR".
- `data.rs:672+` `encode_capabilities`: emit two rows per HDR-capable encoder. Same for decoders (559-593).
- `video.rs:758-901` matrix: group/sort by `(codec, hdr)` tuple. **This breaks the current 1:1 `codec_key()`→`float_rank` (1097-1113) mapping** — refactor `codec_key` to a `(codec, hdr)` key and update `is_active` (784) to compare the tuple. Carry HDR as a dedicated field, **not** parsed from the ffmpeg token (the `is_hw` token check won't contain it).
- Center codec cards (1414-1518): "HEVC SDR" / "HEVC HDR10" sub-cards. `option_box` (1218-1403): small "HDR10" badge.
- **Filter UX — specified (was undecided):** the filter row (`video.rs:247-260`) is already tight. Resolve to a **single segmented control** "All / SDR / HDR" (radio-style, persisted in egui temp memory like `show_unavailable`) placed on its own line below "Show unavailable" (the vertical rework), with the result count moving to the right of that line. Add new locale keys for the empty states: `video.empty.no_hdr`, `video.empty.no_sdr` (and Norwegian `nb` counterparts) alongside the existing `video.empty.*` set — these are currently missing and would otherwise render a raw key.

**Benchmark schema — specified (was cosmetic):**
- `control-ipc::LatencyReport` (440-454): add `hdr: Option<String>` (`#[serde(default)]` — older hosts deserialize fine; no `PROTOCOL_VERSION` bump).
- CSV export (`video.rs:491-511`): **append `hdr` as the LAST column** (back-compat-friendly — positional parsers reading earlier columns keep working). **Do NOT add a leading `# version 2` comment line** — that shifts the header off row 1 and breaks naive CSV parsers (the draft's own proposal was self-defeating). If a version marker is wanted, put it in the *filename* (`bench-v2.csv`) or a trailing column, not a comment header.
- `latency.rs:80` `WireLatencyReport`: add a **separate `hdr_mode` field** (don't overload `codec_fourcc` with `he10`). This is a postcard-serialized struct on the QUIC wire between host and client bench builds — **adding a field is a discriminant/field-compat concern, so host and client bench builds MUST move in lockstep** (same tree). State this; it is not `serde(default)`-safe across postcard skew the way the control-ipc JSON path is.

**Risk:** reference latencies (`encode_reference_ms`/`decode_reference_ms`) don't model HDR cost — HDR cards show approximate latency until a real bench runs (W3b gate). Note in the UI.

### W7 — Per-app launch resolution (live `NvEncReconfigureEncoder`, no re-init) ⚠️ NVENC-owner contract

**Status: ACTIVE (user decision #4).** True per-app resolution: a game at native 2712×1220 (20:9), desktop at 1920×1080, each app at its own mode. **Owners:** compositor / launcher / wire = us; NVENC create+reconfigure = the NVENC-owner (contract below). **Nothing here may destroy or re-init the live NVENC session.**

**Feasibility verdict:** achievable on a live session via `NvEncReconfigureEncoder` inside a fixed max envelope — *recommended (A)* — but it rests on an **inference** (that reconfigure-without-recreate sidesteps the `libnvcuvid` SIGSEGV, which `dfb4057` confirmed is tied to session *re-init*; `rebuild_pipeline_session` survives only as `#[allow(dead_code)]` "DO NOT call on reconnect", `multi.rs:269`). So it ships **behind a gating spike** with fallback B.

**Two hard preconditions for A:** (1) `maxEncodeWidth/maxEncodeHeight` must be set **at create time** — they're a fixed ceiling `NvEncReconfigureEncoder` can't raise; today sessions are created at exact `oc.w/oc.h` with no max (`multi.rs:449`), so they're not reconfigure-capable as-is. (2) The `nvidia-video-codec-sdk` 0.4.0 **safe** API exposes neither `max_encode_*` nor `reconfigure` — only the raw `sys` layer (`reconfigure_encoder`, `_NV_ENC_INITIALIZE_PARAMS.maxEncode*`). That surface is the **NVENC-owner's** to expose, not ours to hand-roll.

**Approach A (recommended):** create the session once with a **4K (3840×2160) max envelope**; pre-size CUDA input + bitstream buffers at the ceiling; per-app change = latch a `PendingMode`, apply it **at a frame boundary** (after frame callbacks `~:1477-1487`, before the pacer `~:1490`): `set_mode` remodes the Smithay output (`change_current_state` second call, via the unwired `VirtualDisplay::set_mode`, `pal/src/lib.rs:88`), realloc the GL render target (`:423`), re-register the CUDA input resource at the new logical size, then `NvEncReconfigureEncoder(resetEncoder=1, forceIDR=1)` on the **same handle**. Wire signal = host→client **`ModeChange{w,h,refresh_mhz}`** (in-session). One transition frame may slip its deadline — **drop it, don't block** (the render-loop-must-not-block lesson); client resyncs on the forced IDR.

**Fallback B (if the spike still SIGSEGVs):** per-app resolution = clean **disconnect → fresh-process respawn** at the new mode (host is `Restart=on-failure`), client reconnects via **`SetupSession`**. ~1-2 s stutter, never a crash — safe because it's a fresh first-init, not a live re-init. **A and B share all launcher/wire/compositor plumbing; only the final encoder step differs**, so the spike result swaps one step.

**Per-app config:** `AppEntry` (`app-launcher/config.rs:~254`) gains `width/height/refresh: Option<u32>` (mHz) **and** `hdr: Option<bool>` (HDR content gate, §1/W2). All `Option` + serde-default so old `apps.toml` loads. ⚠️ **TOML scalar-after-table caveat** (`fps_cap`'s own warning): declare the new scalars **before** `fps_cap` or move `fps_cap` last, and assert round-trip in a test. `LaunchContext` (`launch.rs:35-50`) gains `with_resolution(w,h,r)` mirroring `with_output_fps`; `launch_app` (`control.rs:740-750`) resolves the fields → latch `PendingMode` (≤ envelope ⇒ A) or respawn (> envelope ⇒ B) → spawn child into the remoded output.

**`ModeChange` / `SetupSession` already exist** in `core-proto:49-59` (both carry `width/height/refresh_mhz`) but **have no host receiver** — `run_capture_zc_multi` matches `PlayMedia/StopMedia/LaunchApp/...` only. W7 wires `ModeChange` (host-originated, live) and `SetupSession` (fallback B, new session). **W7 (host) and W7b (client decode dims) land together** or the client renders a mismatched surface.

**NVENC-owner contract (precise — the compositor calls exactly these for resolution, nothing else):**
- **C1 — max-envelope create:** `create_session(guid, init_w, init_h, max_w=3840, max_h=2160, ...)` sets `maxEncodeWidth/Height` and sizes bitstream buffers at the ceiling.
- **C2 — live reconfigure:** `reconfigure_resolution(new_w, new_h, force_idr, reset_rc)` → `NvEncReconfigureEncoder` with `resetEncoder=1, forceIDR=1`; **must not** call `nvEncDestroyEncoder`/`nvEncOpenEncodeSessionEx`/`start_session`; errors (not destroys) if `> ceiling`.
- **C3 — live re-register:** `reregister_input(dptr, new_pitch, new_w, new_h)` unregister+register on the **same** session (0.4.0 `RegisteredResource` only unregisters in `Drop`).
- **C4 — no hidden re-init:** owner asserts (we test) C2/C3 touch zero session-open/-destroy APIs; on reconfigure error, hold the old mode or escalate to B — **never** fall through to `rebuild_pipeline_session`.
- Owner also decides: vendor/fork the crate for a safe `reconfigure` + `max_encode_size`, or use `sys` directly (check crates.io for a >0.4.0 with safe reconfigure first).

**Risks:** the SIGSEGV-avoidance is an inference (the gating spike, below); **20:9 game UI scaling** — a 16:9-assuming game letterboxes/stretches, and desktop↔game may cross aspect ratios in one switch (remode must update physical mode *and* logical size so Trireme doesn't stretch); single-client serial host (one active output — concurrent apps at different modes = out of scope); the wedge watchdog (`880597a`) could mistake a hung reconfigure for a wedge — keep the latch-apply bounded.

**Spike (gating, blocks A):** on a **long-running** host (≥10 min, past first-init-stable), create with a 4K envelope then `reconfigure_resolution` cycling 1080p→2712×1220→720p ≥10× — **pass = zero `libnvcuvid` SIGSEGV, valid IDR each switch.** Until it passes, A is untrusted and we ship B. (Windows IddCx/`hevc_qsv`/`hevc_amf` reconfigure is a *separate* validation — do not assume transfer; Linux/Smithay backend first.)

### W7b — Stream-driven decoder dimensions **[NEW — decoder is hardcoded 1920×1080]**

**Goal:** The client decoder must size itself to the actual stream, not the `WIDTH=1920`/`HEIGHT=1080` constants (`HevcDecoder.kt:34-35`, used at `:147`). Some HW decoders mis-size output buffers when configured far from real geometry, and a 2712×1220 stream against a 16:9 init is a real corruption/mis-size risk.

**Changes:**
- Derive decode dimensions from **W2b `SetupSession{width,height}`** when available; otherwise from the **first access-unit SPS** parsed at decoder config time. Remove the dependence on the 1080p constants for the `createVideoFormat` call (`:147`); keep them only as a last-resort default if neither source is present.
- Re-configure on `ResizeOutput`/a new `SetupSession` (the existing live-resize path) — but a resolution change mid-session means a decoder rebuild (brief stall), so gate it to genuine changes.

**Risk:** initial AU may arrive before `SetupSession`; the SPS-parse fallback covers that. Verify the 14T Pro HW HEVC decoder accepts 2712×1220 (non-multiple-of-16 height handling) — part of the W7 test.

### W8 — Fixed-mode frame pacing (clock-domain-correct) — NO continuous VRR on this panel

**Goal:** Smooth pacing at a **fixed seamless mode (120 default, 144 bonus)** over jittery WiFi. **Corrected by §0b:** the 14T Pro has `hasArrSupport=false` and only discrete modes {60,90,120,144} — there is **no continuous VRR window** to pace into. Smoothness = clock-mapped timestamp presentation + a small jitter buffer at a fixed cadence, *not* a variable display rate. (The "VRR ~138" goal is reinterpreted: pick 120 or 144 and hold it seamlessly.)

**Changes:**
- **Host: ceiling, keep VFR.** `multi.rs:726` pace `1.0/cfg.fps` already caps. No fps floor / frame-doubling (Sunshine #4055). **Set the host pace to the client-reported actual present rate** (W10/W8 back-channel), not a hard-coded rate — a host-rate/panel-rate mismatch (e.g. 144-host/120-panel) is the beat-stutter (#2286/#1545). `FrameStageMeta` already carries the host-clock capture stamp (`multi.rs:1404`); no new wire field needed.
- **Client: present off the source timestamp, in the CLIENT clock domain.** This is the under-specified part of the draft. `FrameStageMeta` stamps are **host-clock UNIX-nanos** and are **not directly usable** as a client present time — they must be mapped through the existing **`ClockPing/ClockPong` offset** (`transport:1810`) into the client clock first. Then set the desired present time.
  - **Path reality:** Trireme presents via `codec.releaseOutputBuffer(index, presentationTimeNanos)` on a plain `SurfaceView` — it is **not** on the `ASurfaceTransaction` path. `ASurfaceTransaction_setDesiredPresentTime` requires a `SurfaceControl`/`ASurfaceTransaction`. So the realistic options are: (a) use `MediaCodec.releaseOutputBuffer(index, renderTimestampNanos)` with the clock-mapped present time (works on SurfaceView, minimal rework — **preferred**); or (b) migrate the present surface to `SurfaceControl` + `ASurfaceTransaction_setDesiredPresentTime` (larger rework). Choose (a) first; only escalate to (b) if release-timestamp pacing proves insufficient. This is a non-trivial item, not an "extend the meta" one-liner.
- `Surface.setFrameRate()` **once** (already at `HevcDecoder.kt:222/228`) with the achievable rate + `FRAME_RATE_COMPATIBILITY_FIXED_SOURCE` + `CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS` (never `_ALWAYS` → 1-2s black). Reset to 0 on stop. Source the rate from W2b `SetupSession.refresh_mhz`.
- **Jitter buffer:** small bounded 1-2 frame client jitter buffer (knob) via the existing `PaceMode` enum (LOW_LATENCY depth-1 … VRR depth-1 Fifo). Pair with the host's bounded video queue + `PHAROS_AUDIO_QUEUE_PACKETS`.
- **Actual-present-rate report:** the client reports its measured present cadence back to the host (reuse the W10 metrics / latency back-channel) so the host pace target follows reality and W3b adaptive-bitrate has a signal.
- **Game Turbo whitelisting (treat 120 as the default, not an edge case):** add a startup/runtime detection — compare requested `setFrameRate` vs the measured present rate; if the panel won't exceed 120 for Trireme, the host clamps to 120 and the HUD shows the clamp. Whitelisting Trireme into Game Turbo is documentation/UX, but the **120 fallback is the designed-for baseline**, not a surprise.

**Gate — does the chosen fixed mode hold with HDR active?** VRR↔HDR mutual-exclusion is moot (no VRR). The real check: confirm 120 (or 144) stays locked when an HDR10 surface is present and that engaging HDR doesn't force a mode change. Ordering if 144 won't sustain with HDR: **HDR + locked 120** (the higher-value combo) > 144 SDR > 120 SDR floor. Config-toggleable.

**Risks:** discrete-only panel — locked 120/144 + clock-mapped timestamped pacing + jitter buffer is the whole game; there is no VRR fallback to lean on. Present-on-arrival without the clock-mapped timestamp → refresh fluctuation (#1545). Charging at high fps + HDR is the throttle corner — see W8b (skin throttles at **53 °C**, per §0b).

### W8b — Thermal / charging-aware cap **[NEW — was an open question, now designed]**

**Goal:** Detect throttling pressure and step down before frames suffer. The 14T Pro runs hot under sustained load; charging + 144fps + HDR decode is the worst corner. **§0b: skin is the gating sensor — `LIGHT` at 53 °C, `MODERATE` 55, `SEVERE` 57.**

**Changes (concrete mechanisms, not a TODO):**
- **Detection:** `BatteryManager` (`isCharging` / charge rate) + `PowerManager.getThermalHeadroom(forecastSeconds)` and `addThermalStatusListener` (`THERMAL_STATUS_LIGHT/MODERATE/SEVERE`). Poll headroom on a low-frequency timer (it's rate-limited); react to status callbacks immediately.
- **Cap ladder (defined):** at `LIGHT`/`MODERATE` or headroom <0.2 while charging — step **144 → 120**. At `SEVERE` — step **120 + HDR → 120 + SDR** (relaunch per the NVENC re-init rule), then **native-res → 1080p** if still throttling. Steps are sent to the host as W8/W3b rate + W2 mode changes.
- **Isolate the driver:** the test plan separates "sustained HDR *decode* alone" from "high refresh alone" so we know which lever (refresh vs HDR vs charging power) actually throttles — the cap ladder order depends on this measurement.
- **UX:** HUD/badge shows when a thermal cap is active; user can pin a manual cap to avoid oscillation. **Q6 resolved: automatic with a manual override.**

### W9 — Vader 4 gyro (phone-side NS-mode sensor → host virtual DualSense) ⚠️ touches uinput/input owner

**Goal:** Real gyro from the Vader 4 (NS mode, plugged into the phone) into Proton games. **Source side is now trivial and verified** (§1); the work is the relay + host motion sink.

**Architecture (decision-locked, verified live):** Vader in **NS mode → phone** → `hid-nintendo` IMU → **Trireme reads gyro+accel via `InputDevice.getSensorManager()`** → relays samples over the wire → **host injects into a new virtual DualSense** → Steam Input gyro-to-mouse/native. No USB/IP, no `hidraw`, no `gyro_hid.rs`, no udev rule.

**Changes:**
1. **Client gyro read (Android):** for the gamepad `InputDevice` (Switch-Pro, vendor `0x057e`), get `getSensorManager().getDefaultSensor(TYPE_GYROSCOPE)` and `TYPE_ACCELEROMETER`; register a `SensorEventListener` at a low-latency rate (`SENSOR_DELAY_GAME` or a fixed period), use the hardware `SensorEvent.timestamp`. Gate the whole feature on the sensor existing (mirrors the gyro toggle that already exists in the UI). Samples are **rad/s (gyro)** / **m/s² (accel)** in Android's controller sensor frame.
2. **Wire relay:** stream gyro/accel samples to the host over the existing input back-channel (extend the gamepad/input message, or a new `Motion{gx,gy,gz,ax,ay,az,ts}` variant). Version it for back-compat (older host ignores unknown variant). The existing `wants_gyro` auth flag already signals intent.
3. **New virtual DualSense/DS4 sink (host):** `gamepad.rs:58-98` — add a second uinput device (DualSense-like: motion axes + accel/gyro + DS4/DualSense VID/PID + a HID descriptor Steam recognizes) **alongside** the Xbox 360. Gyro-capable controllers route here; others keep Xbox 360. **This is the decisive piece — Xbox 360 carries no motion.** Map the incoming Android-frame samples to DualSense's expected gyro/accel units/axis order (axis remap + scale; expect tuning).
4. **Classifier:** `core-proto/controllers.rs:110-112` keys on `contains("flydigi")` — **in NS mode the device reports as "Nintendo Switch Pro Controller" (`057e:2009`), NOT Flydigi.** Add a Switch-Pro VID/PID branch (or "has-IMU-sensor" detection) that enables both `gyro=true` and the **Nintendo A/B·X/Y face-swap** (the existing per-controller swap profile).
5. Let **Steam Input** do Gyro-to-Mouse/Joystick (no host aim-injection needed; the virtual DualSense + Steam Input is the standard Proton gyro path).

**Telemetry seam — SIMPLIFIED (the gyro now originates on the client).** The client calibration overlay (`GyroCalibrationDialog.kt`, `GyroTelemetry.kt`) can read the **local** `InputDevice` sensor directly — it no longer depends on the host streaming raw gyro back (`gyro_imu.rs::latest_raw` → `stream_raw_gyro`, `multi.rs:1254`). That host→client raw-gyro path stays for any *host-attached* evdev IMU but is **not on the Vader-4 critical path**. Net: one fewer cross-machine dependency than the old draft.

**Wire/back-compat:** new motion variant is additive + versioned; gyro tuning still reuses the `ControllerTuning` relay + `controller-profiles.toml` (key it by `057e:2009`).
**Risks (much lower than the old hidraw path):** virtual-DualSense descriptor must convince Steam/Proton it's a real DS4/DualSense (wrong identity silently kills motion); Android↔DualSense axis/sign/scale mapping needs tuning + drift handling; sensor sample rate vs latency over WiFi (the hardware timestamp helps); the controller-handling master switch is default-off (`af4c290`). **App-level confirmation pending:** the InputDevice sensor is proven at the input-stack layer (Sensor Input Mapper) — a 5-line `getSensorList()` log in the debug client (W10) is the final confirmation before building the relay.
⚠️ **Coordination with input owner:** the new virtual DualSense descriptor + gyro→motion mapping touch the uinput layer; coordinate so the descriptor convincingly mimics a real DualSense for native-Proton titles.

**Future (post-milestone, user-flagged): controller capability scout + per-genre mode guide.** The manual ADB scout we did (`/tmp/vader-scout.sh`) generalizes into a runtime feature: Trireme enumerates each connected controller and detects capabilities — gyro/accel `InputDevice` sensor present? analog trigger axes (`GAS`/`BRAKE`) vs digital? stick bit-depth? VID/PID → known-model profile — then surfaces guidance (e.g. "Vader 4: **NS mode** = gyro + 16-bit sticks, digital triggers; **DInput** = analog triggers + paddles, 8-bit sticks, no native gyro; **XInput** = analog triggers, no gyro"). Two pieces: (a) the **scout** feeds the existing `controller-profiles.toml` system (auto-suggest the swap/gyro profile per detected model), and (b) a **short doc** ("what controller mode to point where," by game genre — gyro-aim shooters → NS, racing/flight needing analog throttle → DInput). Not in this milestone; tracked here so it isn't lost. Builds directly on the per-controller profile work already shipped.

### W10 — Debug Trireme variant

**Goal:** A co-installable debug APK with an on-screen metrics HUD for e2e testing.

**Changes:**
- `androidApp/build.gradle.kts`: `buildTypes { debug { applicationIdSuffix=".debug"; isDebuggable=true; isMinifyEnabled=false } release {} }`. `applicationIdSuffix` enables side-by-side install. Add `buildFeatures { buildConfig = true }`.
- `strings.xml`: `app_name` "Trireme Debug" for the debug variant.
- **On-screen HUD** (gated by `BuildConfig.DEBUG`): minimal native overlay (Canvas, 4-6 lines — NOT Compose, to avoid jank) over the SurfaceView: codec, HDR state (W4 `onOutputFormatChanged`), resolution (W7b), refresh/**actual-present-rate** (W8), decode latency, queue depth, drops, **thermal status/headroom (W8b)**, **active bitrate cap (W3b)**. Feeds a new `StreamHub.debugMetrics` StateFlow.
- **Debug settings section — APPLY THE COMPOSE-TV FOCUS LESSON.** If a Debug settings section is added (Force Codec spinner, Debug Overlay toggle, VRR Test button): a **spinner/dropdown inside a clickable row is exactly the D-pad trap** the lesson warns about. Each control's **row** is the focusable click target; the value (codec name / on-off) is trailing display-only text; tapping the row cycles/opens — never embed a `Spinner`/`DropdownMenu`/`Switch` that traps the D-pad. The "VRR Test" button is its own focusable row. Restate this constraint inline in the Debug section.
- Reuse `debug.trireme.decode_trace` (`HevcDecoder.kt:15-28`); add HDR/VRR fields.
- `rig/build-android.sh`: build the debug flavor (`:androidApp:assembleDebug`). Optional `rig/test-android-hdr.sh`: adb install + logcat filter + `adb shell setprop` toggle.
- Desktop reference: optional `trireme debug-hdr` / `trireme-kms debug-metrics` subcommands (main.rs:43-50) — a non-phone oracle.

**Risks:** distinct `applicationId` mandatory or side-by-side install fails; `BuildConfig.DEBUG` refs must survive R8 (keep rules); HUD must stay minimal.

---

## 4. Sequencing / milestones

**Fastest path to an e2e HDR smoke test:** the spine W2 (negotiation) + W2b (mode advertisement) → W3/W3a/W3b (host encode) → W4/W7b (client decode/dims). Everything else is observability/polish.

**Milestone A — Native res + VRR (no HDR). Ship first; lowest risk, immediate value.**
- W7 (per-app 2712×1220 via `NvEncReconfigureEncoder` + the 4K-envelope spike; start at **120** until W8 confirms 144 is reachable) + W7b (dynamic decode dims, driven by `ModeChange`/SPS — **required**, the active output is no longer 1080p) + W2b (so the client learns the mode) + W8 (fixed-mode pacing, clock-mapped present-ts, jitter buffer, 144-reachability check) + W8b (thermal cap). Validates whether 144 ever engages before HDR complicates things. **W2b is a small wire add but is needed in A** because W7b depends on it. **Run the W7 SIGSEGV-avoidance spike first** — it decides approach A vs fallback B for the whole milestone.

**Milestone B — Debug variant. Parallel with A.**
- W10. The HUD is the instrument for A, C, D — build it first.

**Milestone C — HDR spine.**
- W2 (wire) first — the contract both ends code against; include the UniFFI lockstep regen and the v2↔v3 skew handling. Then the **W3a color-management spike** (gating — if PQ can't be preserved, HDR is experimental-only) in parallel with W3b (rate control — also gates WiFi viability) and the W3 10-bit-surface spike. **W3 (NVENC owner) and W4 (Android decode) run in parallel** against the agreed wire + self-signalling bitstream. First e2e HDR smoke test lands here; W5 (status) immediately after to *confirm* HDR engaged.

**Milestone D — Panel polish + gyro, parallelizable.**
- W6 (Video pane + benchmark HDR field) — depends only on W2's `HostStatus.hdr_active` + `LatencyReport.hdr`.
- W9 (gyro) — independent host+input work; **no hardware spike needed** (gyro already verified live in NS mode). Client reads `InputDevice` sensors → relays → host virtual DualSense. First confirm `getSensorList()` in the debug client (W10), then build the relay + DualSense sink.

**Parallelization:** A ∥ B at the start. Within C: W3a/W3b/W3-surface spikes gate, then W3 ∥ W4 once W2/W2b land. D's W6 ∥ W9 alongside C. W1 (toggle) gates on W2's flag but is small.

---

## 5. Test plan (real Xiaomi 14T Pro over WiFi)

**Prerequisites:** 5GHz WiFi (jittery link amplifies VRR issues); `ufw allow <stream-port>/udp` (moving the port silently fails until UFW opens it); Game-Turbo status for Trireme **explicitly checked** (else 120 cap). The **debug variant (W10) HUD is the primary instrument** for every test below.

**W7 / W7b — resolution:** Launch a game; confirm via HUD + `compositor::wire` log that the stream is 2712×1220 and the game renders 20:9. **Verify the 14T Pro HW HEVC decoder accepts 2712×1220** (non-multiple-of-16 height; mis-sized buffers = corruption). Confirm the decoder sized itself from `SetupSession`/SPS, not the 1080 constant.

**W8 / W8b — VRR/refresh/thermal:**
- HUD shows actual-present-rate; watch for refresh fluctuation (#1545) — if the panel swings, the clock-mapped present-ts + jitter buffer aren't absorbing jitter.
- Confirm whether `setFrameRate(144)` engages 144 or **clamps to 120** (OS default-peak / Game-Turbo gating). Test the seamless-switch path doesn't black-screen. Confirm the host pace follows the client-reported actual rate (no host-vs-panel beat).
- **Thermal:** run a sustained charging session; separately measure (a) high-refresh-only, (b) HDR-decode-only, (c) both — to identify the throttle driver. Verify the cap ladder fires (**144→120→SDR→1080p**, triggered on `THERMAL_STATUS_LIGHT` ≈ 53 °C skin per §0b) and the HUD shows the active cap.

**W2 / W2b / W3 / W4 — HDR e2e:**
- **Skew check (corrected back-compat):** v3 host + v2 client → authenticates, SDR (host tolerates missing trailing byte). **v3 client + v2 host → expected HARD REJECT** (document; both ship from one tree). Confirm a checksum-matched FFI (no UniFFI panic at init).
- Negotiation: HUD shows "HDR10 requested" → host log shows Main10 selected (and `SetupSession.hdr=true` advertised) → HUD shows decoder confirmed HDR (`onOutputFormatChanged` color aspects + `KEY_HDR_STATIC_INFO`). All three must agree.
- **The fake-HDR check (W3a — blocking):** with a **real HDR Proton game** (not just a synthetic pattern), verify the capture surface is genuinely 10-bit AND the game's PQ surface reached the encoder without SDR flattening. A PQ gradient test pattern shows no banding/correct highlights; compare against SDR. Washed-out/over-dark = either the 8-bit-in-10-bit trap (W3 #1) or color-management flattening (W3a). **Do not ship HDR as "working" until this passes on real HDR content.**
- **SDR-content-with-HDR-on check (Q11):** with HDR toggle on but launching an **SDR/unflagged** app, confirm the app stays SDR (per-app HDR flag gate) — i.e. no washed-out SDR-as-PQ.
- VUI correctness: wrong `videoFullRangeFlag`/matrix = washed/oversaturated — validate with the pattern, not by eye.
- **Mid-stream join:** reconnect after stream running; HDR SEI must arrive ≤1s (re-emitted on IDR).
- **Bandwidth/queue check (W3b):** at native-res HDR, confirm the capped-VBR target holds and the QUIC window isn't exhausted (no ~2s backlog); confirm the **audio queue doesn't back up behind a stalled video write_all** (the f913848 bug — higher HDR bitrate raises this risk; `PHAROS_AUDIO_QUEUE_PACKETS` must still bound it). Drop network quality and confirm bitrate adapts down (quality drops, not frames).
- **SDR fallback paths:** HDR off → clean SDR, no regression to the validated 60fps/0-drop path; non-HDR display/decoder → toggle hidden, SDR.
- ⚠️ **Reconnect SIGSEGV watch:** HDR toggle/reconnect triggers a *relaunch*, not in-process NVENC re-init; watch the libnvcuvid wedge fingerprint (3× >200ms). Confirm the watchdog also guards the native-res HDR encode mode.
- **Fixed-mode + HDR (gate):** confirm the chosen mode (120, or 144 if reached) stays locked with HDR10 active and that engaging HDR doesn't force a mode change; if 144 won't sustain with HDR, confirm the HDR+120 fallback engages. (No VRR on this panel — §0b.)

**W5 / W6 — observability:** Overview shows "NVENC HEVC HDR10" while streaming; Video pane shows split SDR/HDR cards, the All/SDR/HDR filter works (and the new `video.empty.*` locale keys render, not raw keys), benchmark CSV has the **appended** `hdr` column (header still on row 1; parsers unbroken). Client HostsPane badge shows HDR10 only when **confirmed live**.

**W9 — gyro (NS mode):**
- Client: confirm `inputDevice.getSensorManager().getSensorList()` lists GYROSCOPE+ACCELEROMETER for the Switch-Pro pad (the debug HUD logs it); register a listener, see live samples.
- Relay: gyro/accel samples reach the host; the **virtual DualSense** (not Xbox 360) is what the game sees; classifier applies the Nintendo A/B·X/Y swap.
- In-game: Steam Input Gyro-to-Mouse aims; tune the Android→DualSense axis/sign/scale mapping + drift. Verify buttons/sticks/digital-triggers all still map correctly.
- Calibration overlay reads the **local** sensor (no host→client raw-gyro dependency for this pad).

**W10 — debug variant:** debug + release co-install (`.debug` suffix); HUD overlays without jank; debug-section controls are **D-pad-navigable** (row-as-target, no embedded spinner trap); toggle via `adb shell setprop`.

---

## 6. Open questions / decisions for the user

**Resolved by the four locked decisions (§0) + the live device scout:** ~~Q1 HDR name~~ → "HDR" (HDR10 static). ~~Q4 per-app resolution~~ → **pursued via W7** (`NvEncReconfigureEncoder`, 4K envelope; spike-gated, fallback B). ~~Q5 refresh~~ → 120 baseline, 144 bonus (and §0b kills "VRR" — discrete panel). ~~Q7 gyro~~ → **NS-mode phone-side sensor + host virtual DualSense** (verified live; not the old USB/IP path). ~~Q10 gyro spike~~ → **moot** (gyro proven on hardware; only an in-app `getSensorList()` check remains, folded into W10).

**Still open / needs your call:**
2. **HDR toggle scope: per-host or global?** Recommend **per-host** (`ServerEntry` field).
3. **HDR orthogonal vs codec-coupled?** Recommend **orthogonal** (`SetHdrMode` + `hdr_active`).
6. **Charging-aware cap — designed (W8b):** automatic ladder via `PowerManager` thermal status + `BatteryManager`, manual-override pin. **§0b refines it: react on `THERMAL_STATUS_LIGHT` (~53 °C skin)**, the gating sensor. OK?
8. **Mastering-display/CLL source.** "Pull from EDID" is NOT available (headless compositor). Choose: pull from the game's HDR surface via the color-management protocol (best, ties to W3a) vs configured defaults. **§0b informs the default:** the client advertises a **500-nit** ceiling, so honest defaults are **BT.2020 / mastering max ~1000 nits / MaxCLL ~1000 / MaxFALL ~400** *and* trust the panel to tone-map to 500 — or cap mastering at 500 to match? Your call on the default profile.
9. **NVENC-owner coordination:** confirm the other process owns W3 (capture-surface widening + NVENC format/profile/VUI/SEI), W3b (rate control), W3a (color-management/PQ preserve), **and W7 contracts C1–C4** (max-envelope create + live `NvEncReconfigureEncoder` + live re-register + no-re-init guarantee). **Who owns the GL→CUDA 10-bit bridge (`cuegl.rs`) — them or us?**
10. ~~Gyro validation gate~~ — **RESOLVED:** gyro verified live in NS mode (native `InputDevice` sensor); no hidraw spike. Only open gyro sub-question: should the host virtual DualSense be **DualShock 4 vs DualSense** identity (whichever Steam Input + your Proton titles read motion from most reliably)? Recommend DS4 (broadest Steam Input support).
11. **HDR-on-SDR-content policy:** we can't auto-detect whether the *source* is HDR. Confirm the safeguard: arm Main10/PQ **only for per-app `hdr=true`-flagged apps** (per-game opt-in), so a capable phone with HDR on still streams SDR games as SDR. OK, or blunter "HDR follows the toggle, user beware"?
12. **NEW — 144 Hz target:** the panel *does* expose 144 at native res, but the OS default-caps at 120 (Game-Turbo/app-request gated). Want me to (a) ship 120 and treat 144 as best-effort runtime-detected, or (b) actively chase getting Trireme whitelisted/requesting 144 as a first-class target?

### Key file references
- Host encode/HDR/RC: `crates/compositor/src/multi.rs` (`:315`/`:457`/`:423`/`:1323`/`:244-267`/`:1033-1054`/`~1542`/`:726`/`:1254`/`:1404`), `crates/compositor/src/nvenc.rs:51/69`, `crates/compositor/src/cuegl.rs:122-185`, `crates/pal/src/lib.rs` (`P010`/`create`)
- Wire/proto: `crates/transport/src/lib.rs:320` (hard-equality version check ~912)/`907-934`/`~1946-1968`/`380`/`1810` (ClockPong offset)/`1901` (ClockPing), `crates/control-ipc/src/lib.rs:208/274/440-454`, `crates/core-proto/src/lib.rs:21` (do not bump)/`49-52` (SetupSession stub — wire up in W2b)/`56-58` (ResizeOutput), `crates/core-proto/src/latency.rs:80`
- Gyro (NS-mode, phone-side source): host `crates/compositor/src/gamepad.rs:58-98` (add virtual DualSense/DS4 sink + Android→DS motion map), `crates/core-proto/src/controllers.rs:110-112` (add Switch-Pro `057e:2009` branch → gyro + Nintendo face-swap); client `MainActivity.kt`/input layer (`InputDevice.getSensorManager()` → TYPE_GYROSCOPE/ACCELEROMETER listener), `GyroTelemetry.kt`/`GyroCalibrationDialog.kt` (read local sensor), `stream/StreamHub.kt:447-467` (relay samples). NO `gyro_hid.rs`/hidraw/udev. (`gyro_imu.rs` evdev stays for host-attached IMUs only.)
- Panel: `crates/pharos-panel/src/model.rs:269-293/660`, `settings.rs:201`, `tabs/video.rs:247-260/491-511/758-901/1097-1113/1218-1403/1414-1518`, `tabs/overview.rs:681-708`, `data.rs:559-593/672+`, `locales/` (new `video.empty.no_hdr`/`no_sdr`)
- Resolution/launch: `crates/app-launcher/src/config.rs:195-255` (add `hdr`, NOT resolution), `crates/compositor/src/control.rs:747-749`, `crates/compositor/multi.example.toml`
- Client: `MainActivity.kt:375-430/490/496-499`, `stream/HevcDecoder.kt:34-35` (1080 constants → dynamic)/`61-70/146-163/222-228`, `stream/StreamHub.kt:74-95/472-484`, `ui/panes/SettingsPane.kt`, `ui/panes/HostsPane.kt:378-415`, `client/shared/generated/kotlin/uniffi/core_ffi/core_ffi.kt` (lockstep regen), `androidApp/build.gradle.kts`
- FFI: `crates/core-ffi/src/lib.rs:378` (`wants_gyro` last param → add `wants_hdr`; checksum lockstep)