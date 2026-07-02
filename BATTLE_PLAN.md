# Battle Plan — Clean-slate game-streaming + USB/IP host & client

**Codename:** **Pharos** (host) · **Trireme** (client) — in the Apollo·Artemis / Polaris·Nova tradition.
Repo / working dir: **KVM** — and that's literally the product: a software **KVM-over-IP** (keyboard, video, mouse + USB over the network). Low-latency game streaming is a first-class *mode* riding the same pipeline, not a separate product.

**Status (2026-06-19):** **M0 + M1 done and proven on real hardware**; M2 (pure-KVM MVP) and M3 (Windows host) partially landed. ~20k LOC across 18 crates on a single branch (`main`); `cargo check --workspace` green. See §10 for the live milestone ladder.

---

## 0. North star

A from-scratch Sunshine/Moonlight replacement that is **fast, reliable, and passably OS/device-agnostic**, written as:

- **One Rust core** (protocol, crypto, session state machine, USB/IP) shared by host *and* client.
- A **Rust host daemon** on top of that core.
- A **Kotlin Multiplatform client** (Android + desktop) that consumes the same Rust core via FFI.

Plus a **VirtualHere-style USB/IP channel** so the client's controllers, headsets and other USB devices appear as *real* devices on the host.

**Product priority:** the headline use case is **pure KVM** — sit at Trireme and fully operate a headless Pharos box (its real desktop + your local USB devices), over the network, at near-local feel. Game streaming is the same pipeline pushed to its limits (120fps, gamepads, dynamic mode changes). We build KVM first, but every pipeline decision is made *game-ready* so games don't require a re-architecture later (see §6 invariants).

### Locked decisions (from kickoff)

| Decision | Choice | Consequence |
|---|---|---|
| **Primary product** | **Pure KVM** (headless remote desktop + USB-over-IP) | Game streaming is a first-class *mode* on the same pipeline. Build game-ready from day one. |
| Protocol | **Clean-slate (QUIC)** | No Moonlight interop; we ship host + client together. Freedom to design for latency + reliability. |
| Host OS | **Windows + Linux co-equal** | Platform Abstraction Layer (PAL) built up front; no "port later" rework. macOS host deferred. |
| USB/IP scope (v1) | **HID + audio + mass-storage**, architected for arbitrary classes later | Isochronous (webcams) and exotic devices are post-v1. |
| Client | **Kotlin Multiplatform**, shared core | Android first; desktop client shares the same KMP + Rust core. |
| Display | **Fully headless; hand-rolled Smithay** (Linux) / **IddCx** (Windows) | Pharos owns the display server → full latency control to tune. **No EVDI.** Games run as an isolated session on the *same* compositor. |
| Codecs | **HEVC via NVENC first**; AV1 if cheap | VAAPI/AMD and SW encode behind the same trait; AV1 is ~free on Ada (RTX 40xx). |

---

## 1. Lessons carried forward from Apollo/Artemis + Polaris/Nova experiments

These are load-bearing — they're *why* the architecture looks the way it does.

1. **No EVDI / no virtual-display-driver path on Linux.** The GBM `gbm_create_device` SIGSEGV, the EDID-advertisement bugs, the KScreen-primary/kwin-enumeration hacks — all symptoms of capturing from a fake DRM connector. Render into an **isolated nested compositor** and capture *that* instead. (Polaris/gamescope model.)
2. **Stay zero-copy.** The EVDI CPU-buffer lane worked but was the slow path. A real nested compositor hands us a GPU buffer → keep DMA-BUF/D3D11 → NVENC zero-copy. Target ≤3 ms encode (proven on RTX 4070).
3. **We control the "EDID" now.** With a synthesized headless output we declare the mode/resolution/refresh ourselves — no advertised-mode bugs.
4. **Pairing must be written once, correctly.** GameStream's salted-PIN-AES + cert-signing dance cost four debugging rounds. Replace it with a real **PAKE** (SPAKE2+/CPace) implemented *once* in the shared Rust core, used identically by host and client.
5. **Bake in recovery.** The prior setup needed an external `apollo-monitor-recovery.sh` watchdog. Make session/display watchdog + auto-restart a first-class daemon feature.
6. **Regression baseline:** RTX 4070, 1080p120, 0 dropped, ~3 ms `hevc_nvenc`. The rewrite is not allowed to regress below this on the same hardware.

---

## 2. Why "one Rust core" is the keystone

The single highest-leverage decision. The painful, easy-to-get-wrong parts — pairing handshake, session/RTSP-equivalent setup, crypto, congestion/FEC policy, USB/IP URB framing — are written **once** in Rust and compiled into:

- the host daemon (native), and
- the Kotlin clients via **UniFFI** (generates Kotlin bindings to a `.so`/`.dll`/`.dylib`/`.framework`).

So pairing has exactly one implementation and one test suite, not one per side. This directly attacks the worst pain point from the experiments.

```
                 ┌────────────────────────────────────────┐
                 │            core (Rust, no_std-ish)       │
                 │  protocol · crypto/PAKE · session FSM ·  │
                 │  congestion/FEC · usbip framing · config │
                 └───────────────▲───────────────▲──────────┘
                                 │ native        │ UniFFI (JNI/cinterop)
                ┌────────────────┴───┐     ┌──────┴───────────────────┐
                │  host daemon (Rust)│     │ KMP client (Kotlin)      │
                │  capture·encode·   │     │ decode·render·input·UI   │
                │  inject·usbip-host │     │ (Android + desktop)      │
                └────────────────────┘     └──────────────────────────┘
```

---

## 3. Rust workspace layout (host + core)

```
crates/
  core-proto       # wire format, QUIC stream map, framing, versioning
  core-crypto      # identity, PAKE pairing, cert pinning, session keys
  core-session     # session FSM, capability negotiation, setup/teardown
  core-usbip       # URB encode/decode, device-class policy, per-device channel
  core-ffi         # UniFFI surface exported to Kotlin
  media-capture    # trait CaptureSource; backends:
                   #   linux-compositor (Smithay/gamescope-style headless)
                   #   windows-iddcx     (virtual display) + WGC/DXGI
  media-encode     # trait Encoder; backends: nvenc (HEVC/AV1), vaapi, software
  media-audio      # capture (PipeWire vsink / WASAPI loopback) + Opus
  input-inject     # uinput (Linux) / SendInput+ViGEm (Windows)
  usbip-host       # import side: vhci-hcd (Linux) / driver bridge (Windows)
  pal              # platform traits: VirtualDisplay, Capture, AudioSink, Input
  hostd            # the daemon: orchestration, telemetry (control panel = §9, native app over QUIC)
```

**PAL traits** are the seam that makes Windows+Linux co-equal:

```rust
trait VirtualDisplay { fn create(&self, mode: Mode, hdr: bool) -> Result<CaptureSource>; }
trait CaptureSource  { fn next_frame(&mut self) -> GpuFrame; }  // DMA-BUF or D3D11 tex
trait Encoder        { fn submit(&mut self, f: GpuFrame) -> EncodedPkt; }  // zero-copy
trait InputSink      { fn inject(&mut self, ev: InputEvent); }
```

---

## 4. Kotlin Multiplatform client layout

```
client/
  shared/        # commonMain: wraps core-ffi (pairing, session, telemetry, config)
  androidApp/    # MediaCodec decode → Surface, controller/IME input, USB host APIs
  desktopApp/    # Compose Desktop UI; decode via platform (DXVA2/VideoToolbox/VAAPI)
                 #   or ffmpeg; same shared core
```

Hot path (decode orchestration, network, crypto) lives in Rust core. Kotlin owns UI (Compose Multiplatform), platform decoders, surface/render, and input capture.

---

## 5. Protocol (clean-slate, over QUIC)

**Transport:** QUIC (`quinn`). TLS 1.3 is built in; pairing pins a long-term client identity.

**Stream/datagram map:**

| Channel | QUIC type | Why |
|---|---|---|
| Control | reliable bidi stream | setup, capability negotiation, config, resume |
| Input | reliable ordered stream (or seq'd datagrams) | low latency, must not drop |
| Video | **datagrams** (RFC 9221) + app FEC | unreliable + deadline-aware; no head-of-line blocking |
| Audio | datagrams + FEC | same |
| USB/IP | **one reliable stream per device** | independent flow control; a stalled webcam can't block the gamepad |

**Pairing:** PAKE (SPAKE2+/CPace) over the control channel — PIN authenticates the channel, then long-term keys are exchanged and pinned. One Rust implementation, host + client.

**Reliability wins over GameStream/ENet:**
- **Connection migration** — survive Wi-Fi roam / IP change without dropping the session.
- **Resume tokens** — reattach after a transient drop without re-pairing.
- App-level **FEC** + deadline-aware selective retransmit for media.
- ABR driven by QUIC congestion signal + receiver feedback.

---

## 6. Headless display — Pharos owns the display server

The unifying idea: **Pharos runs its own headless display server**, you remote into *that*, and a game is just an app launched inside it. One compositor serves both pure-KVM (your whole remote desktop) and game mode (an isolated fullscreen surface) — same capture → encode → transport path, no second pipeline.

### Linux — hand-rolled Smithay (locked: no gamescope wrap, no EVDI)
- Build our own **headless Wayland compositor on Smithay**. Full control over the frame lifecycle, buffer allocation, output timing and pacing — which is exactly the knob set we need to tune latency. Wrapping gamescope was rejected: too little control over the hot path.
- It runs **headless by construction** (no DRM connector, no physical monitor) — we synthesize the output mode/refresh ourselves, so the EVDI-class EDID/connector bugs simply don't exist.
- Capture the compositor's output buffer via **DMA-BUF**, import to NVENC/VAAPI **zero-copy** (keeps us on the ≤3 ms path; the CPU-buffer lane stays only as a fallback).
- **Xwayland** inside for X11 apps/games.
- **Two display modes, one compositor:**
  - *KVM/desktop mode* — a full desktop session inside Pharos-Smithay (run a panel/launcher or your normal apps); this is what you remote into.
  - *Isolated app/game mode* — launch a single app/game onto its own output surface with its own mode/refresh, without disturbing the desktop session.
- **Optional secondary source — mirror an existing session:** for users who want to remote their *real* KDE/GNOME desktop instead of a Pharos session, support a PipeWire + `xdg-desktop-portal` ScreenCast capture path behind the same `CaptureSource` trait. (Still needs the running compositor to have an output — prefer the compositor's **native virtual-output** support over EVDI; flagged as research in §11.) Primary/tuned path remains Pharos-Smithay.

### Windows — IddCx virtual display
- An **Indirect Display Driver (IddCx)** presents a headless virtual monitor that *becomes* the desktop (no physical monitor needed) — the Windows analogue of "Pharos owns the output".
- Capture via **Windows.Graphics.Capture** (or DXGI Desktop Duplication) → NVENC from the D3D11 texture, zero-copy.
- Game mode = app launched on that virtual desktop (borderless/exclusive fullscreen); a second virtual display can isolate it if desired.
- ⚠️ Driver packaging/signing required (see risks).

### Game-readiness invariants (so KVM-first doesn't create late blockers)

KVM could tempt us into an RDP-style "send dirty rectangles on change" design that can never do 120fps games. We refuse that. From M1 the pipeline obeys:

1. **Continuous, paced full-frame video** (not damage-rectangle-only). Idle desktops are handled by *skipping* paced frames / dropping bitrate, not by switching to a different protocol.
2. **Relative + absolute pointer + analog input** from the start — pointer-lock/relative mode and gamepad analog axes, not just an absolute cursor.
3. **Dynamic mode & refresh changes** are a normal event (games switch resolution): virtual display, capture, encoder and client renderer all handle live re-negotiation.
4. **Low-latency Opus audio** is in the core path, not bolted on.
5. **Session model supports an isolated fullscreen surface** alongside the desktop, so "launch a game" is a new session, not a new architecture.
6. **Per-frame deadline discipline** end-to-end (encode/network/decode budget tracked), so the same path that feels fine for desktop is already inside a game's frame budget.

---

## 7. Capture → encode → audio → input

- **Encode:** NVENC HEVC first via the NVIDIA Video Codec SDK (FFI, zero-copy from DMA-BUF/D3D11). **AV1 is nearly free on Ada/RTX 40xx** — same `nvEncodeAPI`, different codec GUID — so it's an early stretch, not a rewrite. VAAPI + software behind the same `Encoder` trait.
- **Audio:** isolate game audio like we isolate video — Linux **PipeWire virtual sink**, Windows **WASAPI loopback** — then **Opus**.
- **Input inject:** Linux **uinput** (virtual kbd/mouse/evdev gamepad); Windows **SendInput** + **ViGEm** (virtual Xbox/DS4). Latency-sensitive standard input goes this route; special devices go USB/IP (§8).
- **Color:** ship honest **SDR** first (as Polaris does); HDR once compositor/codec path is solid.

---

## 8. USB/IP channel (VirtualHere-style) — note the reversed direction

In game streaming the device usually lives on the **client** (your controller/headset) and must appear on the **host** (so the game sees a real device). That's the inverse of textbook USB/IP:

- **Client = exporter** (shares its local USB device)
- **Host = importer** (virtual hub; device shows up as locally plugged)

**v1 classes:** HID (gamepads/wheels), mass storage — both proven (bulk/interrupt/control; relay is byte-transparent). **Reality check (mapped 2026-06-19):** USB *audio* and webcams are **isochronous**, and iso is the hard tier — `rusb` exposes no iso API (needs raw `libusb1-sys` async FFI + an event-loop thread), Android's iso stack can't hold real-time cadence, and it's partly redundant with the Opus path (§7) for host↔client audio. So iso *servicing* is deferred; what's done is the iso **wire format** + framing-safety so sharing an audio/UVC device no longer desyncs the relay (its iso endpoints reject cleanly; control/HID endpoints still work).

**Import side:** Linux **`vhci-hcd`** (in-kernel) for v1; Windows needs a **signed bridge driver** (lean on `usbipd-win`/`usbip-win2` rather than rolling our own kernel driver early).

**Transport:** URBs encapsulated over **one reliable QUIC stream per device** — independent flow control, no cross-device head-of-line blocking.

**Client-side reality check:** Android can't cleanly *export* arbitrary USB devices, so on phones controllers go as **input events** (§7), not USB/IP. USB/IP's real home is the **desktop/thin-client** path and special devices (wheels, DInput pads, auth dongles) — i.e. the true "KVM" use case.

---

## 9. Control plane / application panel

The control plane — pairing management, app/session config (display mode, USB routing, A/V output routes, per-device persistence), and **live telemetry** (fps, bitrate, dropped frames, encode-ms, RTT; also the perf-regression dashboard against the §1.6 baseline) — is delivered as a **native application panel, not a browser-served web console.**

**Why not a web SPA (decision reversed 2026-06-19).** The original plan was an axum-served SPA. Killed it: browsers actively sabotage a locally-served console. Firefox HTTPS-Only Mode auto-upgrades `http://localhost`, fails the cert, and shows a "Potential Security Risk / your passwords may be stolen" page; with HSTS it refuses any bypass ("there is nothing you can do"). Self-signed HTTPS is worse (the literal Nova/Polaris cert-wall). "localhost is a secure context" is true for web *APIs* but does **not** stop the browser chrome from walling the user — a real, user-hostile UX we don't control.

**The panel is native and rides the authenticated channel.** Primary surface = the **Trireme** client (Compose Multiplatform — the app you already use to connect): pairing, config and telemetry become screens that talk to the host over the **paired QUIC control channel** (§5) via the shared Rust core. One trust path (Ed25519 pairing), one UI toolkit, zero browser, zero cert prompts. The host keeps a minimal local surface for first-run / at-the-box actions (PIN display, start/stop) — TUI now, optional small native window later. (Open: exact client-side vs host-side split — see §12.) The QUIC TLS cert stays a throwaway opaque tunnel (§5, transport `SkipServerVerification`), never a trust anchor.

---

## 10. Roadmap (milestones)

Ordered for **pure-KVM first**; game mode lands at M5 but the pipeline is game-ready (§6 invariants) from M1.

Status: ✅ done · 🟡 partial · ⬜ not started.

| M | Goal | Status | Exit criteria |
|---|---|---|---|
| **M0** | Foundations | ✅ done | Cargo workspace + PAL traits; Win+Linux CI; core-proto/crypto skeleton; UniFFI→Kotlin "hello core" round-trips on Android. |
| **M1** | Linux KVM vertical slice — *operate the headless box* | ✅ done — proven RTX 4070 → Bravia/phone | Hand-rolled Smithay headless session → zero-copy NVENC HEVC → QUIC → Trireme decode/render → **kbd + relative/absolute mouse injection** into the seat. Continuous paced **120fps** pipeline. |
| **M2** | Pure-KVM MVP | ✅ done | SPAKE2 pairing + **mutual auth enforced by default** (`peers.toml`; `PHAROS_ALLOW_UNPAIRED=1` opts out) — desktop, Android, and `trireme-kms` all present their identity; **end-to-end Opus audio** (capture → QUIC datagrams → `pacat`, rig-verified); **USB/IP** HID + mass-storage relay verified, iso (USB-audio/UVC) wire-format + framing-safety landed (iso *servicing* deferred: `rusb` has no iso API, Android can't, partly redundant with §7). Post-v1 tails: optional iso servicing (libusb FFI, desktop-only); on-device Android rebuild check. The core product is usable. |
| **M3** | Windows host parity | 🟡 ~60% — capture/encode proven on T14 | Done: WGC/ddagrab capture, Quick Sync/NVENC/AMF HEVC, IddCx headless virtual display (all validated on the T14). Remaining: live SendInput, NVENC/AMF on real GPU rigs, WASAPI audio, ViGEm, USB import bridge. |
| **M4** | Reliability | ⬜ not started | FEC, ABR, QUIC connection migration, resume tokens, daemon watchdog/auto-recovery, telemetry + native control panel (§9, no browser). |
| **M5** | Game mode | ⬜ not started | Isolated fullscreen app/game session on the Smithay compositor; dynamic mode/refresh; gamepad passthrough; pacing tuned to client refresh; **AV1**. Hit/beat the RTX 4070 1080p120 / 0-dropped / ~3ms baseline. |
| **M6** | Breadth + polish | ⬜ not started | Compose **desktop** Trireme; HDR; more USB classes; packaging + driver signing; session-mirror capture path. |

---

## 11. Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| **Windows USB/IP driver signing** (kernel bridge) | High | Reuse `usbipd-win`/`usbip-win2`; keep input-event path so v1 doesn't *need* the driver. |
| NVENC concurrent-session cap on consumer GPUs | Med | Single-session focus v1; document; (patched drivers out of scope/EULA). |
| **Hand-rolling a Smithay compositor is real work** (no gamescope fallback) | High | Accepted cost — full control is the goal. De-risk: start from Smithay's `anvil` headless example; M1 targets only a minimal desktop + Xwayland, defer multi-output/game-session to M5. |
| Headless desktop output for the *session-mirror* path (KDE/GNOME w/o monitor) | Med | Prefer compositor-native **virtual outputs** over EVDI; research KWin 6 virtual-output API. Non-blocking: Pharos-Smithay session is the primary path; mirror is M6. |
| QUIC datagram MTU/pacing at 4K120 high bitrate | Med | Frame pacing + FEC tuning; fragment policy; measure early in M1. |
| Android USB *export* limitations | Med | Scope USB/IP to desktop/thin-client; phones use input events. |
| HEVC patent/licensing for distribution | Low-Med | AV1 path as royalty-free alternative; document distribution stance. |
| macOS host demand | Low | Deferred; client (KMP/desktop) can still target macOS first. |

---

## 12. Decisions resolved + remaining questions

**Resolved this session:**
- ✅ Codename: **Pharos** (host) / **Trireme** (client).
- ✅ Linux compositor: **hand-roll Smithay** (no gamescope wrap) — full control to tune.
- ✅ Product priority: **pure KVM is the primary product**; game mode is a first-class mode on the same pipeline, built game-ready from M1 (§6 invariants).

**Resolved since (now in code):**
- ✅ Pairing PAKE: **SPAKE2** implemented in `core-crypto` (M2 hardening ongoing).
- ✅ Wire format: custom **length-prefixed binary** access units over QUIC (`transport` + `core-proto`).
- ✅ Linux decode (reference client): **NVDEC via ffmpeg `hevc_cuvid`** subprocess; Windows: **Quick Sync `hevc_qsv`** + NV12/GPU color.
- ✅ Client→host auth: **mutual per-connection Ed25519 challenge** + `peers.toml` authorization (`transport`/`core-crypto`), **enforced by default** (`PHAROS_ALLOW_UNPAIRED=1` opts out). Desktop + Android present their paired identity on connect. The TLS cert stays a throwaway opaque tunnel — never a trust anchor.
- ✅ Control plane (§9): a **native application panel, not a browser/web console** — browsers (Firefox HTTPS-Only + HSTS) wall even `http://localhost`, so the panel lives in the Trireme client over the paired QUIC channel. No self-signed HTTPS, no cert-wall.

**Still open:**
- Desktop-client (Trireme) decode strategy: keep the ffmpeg subprocess, or in-process libavcodec / per-OS native decoders behind a trait? (Refine by M6.)
- Exact KWin 6 virtual-output mechanism for the optional session-mirror path (M6 research).
- Architecture debt: real capture/encode/audio logic lives inside `compositor`/`pharos-win`/`trireme`, not the §3 trait-crates (`media-*` are still placeholders) — refactor toward the trait seam as each is touched.
- Mutual-auth tail: `trireme-kms` and the `pharos-win` Windows host still connect/serve anonymously (use `PHAROS_ALLOW_UNPAIRED` for those paths); give `trireme-kms` a persistent identity, and verify the Android identity path on a real device rebuild.
- Control panel (§9) shape: how much lives in the Trireme client (Compose, over QUIC) vs a host-side native surface (TUI / small window) for first-run + at-the-box admin. Leaning client-primary + thin host-side. (M4.)
