# pharos-streaming — Pharos (host) · Trireme (client)

A clean-slate, low-latency **KVM-over-IP** (keyboard, video, mouse + USB over the
network), with game streaming as a first-class mode on the same pipeline.
A from-scratch spiritual successor to Sunshine/Moonlight (and the Apollo·Artemis /
Polaris·Nova forks), written in **Rust** (host + shared core) and **Kotlin** (client).

- **Pharos** — the host daemon (Rust). The lighthouse the client navigates by.
- **Trireme** — the client (Kotlin Multiplatform; Android first, plus a Rust
  desktop client). The small craft that comes alongside.
- **core** — one Rust core (protocol, crypto/pairing, session, USB/IP framing)
  shared by both ends via UniFFI.

See [`BATTLE_PLAN.md`](BATTLE_PLAN.md) for the full architecture and roadmap, and
[`docs/history/`](docs/history/) for archived design notes.

## Status

Proven on real hardware, actively used for day-to-day game streaming:

- **Linux host → client:** headless Smithay compositor → zero-copy NVENC **HEVC/AV1**
  → QUIC → Trireme decode/render, **120 fps @ 1080p**, pixel-correct on a Sony Bravia
  and a Xiaomi phone; keyboard + relative/absolute mouse + gamepad injected back into
  the compositor seat.
- **Game mode:** real Proton games stream end-to-end (native Wayland), with gamepad
  passthrough, a live resolution/bitrate ladder, and an optional Steam-launch redirect.
- **Audio:** end-to-end low-latency Opus.
- **USB/IP relay** (HID + mass-storage): a client exports a local USB device to the
  host's virtual hub, verified over the LAN.
- **Windows host:** the Rust desktop client plays on Windows (Quick Sync decode); the
  Windows host captures desktop **and** a headless virtual display → Quick Sync / NVENC /
  AMF HEVC. NVENC HEVC hosting validated on a real GPU rig.
- **HDR:** host HDR10 encode (BT.2020/PQ + SEI) is implemented; Proton *game*-HDR on
  Linux is currently blocked upstream in Wine's Vulkan/Wayland WSI and is deferred to
  the Windows-native host path.

This is pre-1.0 research-quality software. Interfaces, config keys, and env vars may
change between commits.

## ⚠️ Security notice

Pharos is a **network-listening daemon that injects input and can bridge USB devices**
into the host. Treat it accordingly:

- Run it on a **trusted LAN**, not the open internet. There is no hardening audit yet.
- Client authentication (SPAKE2 pairing + per-connection Ed25519) is **on by default**;
  `PHAROS_ALLOW_UNPAIRED=1` disables it — don't set that on an untrusted network.
- Exposing the stream port to the internet (e.g. via port-forward or a reverse proxy)
  is at your own risk. Prefer a VPN/WireGuard tunnel.

Found a security issue? Please report it privately rather than opening a public issue
(see `SECURITY.md`).

## Layout

```
crates/                  # --- shared core (host + client, exported via UniFFI) ---
  pal            platform-abstraction traits (the Windows/Linux co-equal seam)
  core-proto     wire protocol: channel map, framing, access units   [+tests]
  core-crypto    pairing / identity / session keys (SPAKE2)
  core-session   session FSM + capability negotiation
  core-usbip     USB/IP URB wire protocol + device-class policy       [+tests]
  core-ffi       UniFFI surface → Kotlin + Swift bindings
  host-core      shared host glue (pairing/identity) for both OS hosts
  host-info      host capability / backend probing
  control-ipc    control-plane IPC (panel ⇄ host: status, config, live actuation)
  transport      QUIC (quinn) channels: video, input, USB/IP
                         # --- Linux host ---
  compositor     hand-rolled headless Smithay → zero-copy GLES→CUDA→NVENC HEVC/AV1
  trireme-kms    direct DRM/KMS atomic-scanout client (drive a real TV headless)
  usbip-host     USB/IP import side — bin `pharos-usbimport` (vhci-hcd virtual hub)
  app-launcher   launch apps/games into the virtual display; Steam redirect shim
  hostd          host daemon: proto self-test + banner
                         # --- Windows host ---
  pharos-win     bin `pharos-host` — WGC/ddagrab → Quick Sync/NVENC/AMF HEVC, SendInput
                         # --- media (real impls live in the host/client crates above) ---
  media-audio    Opus capture/encode (real)
  media-decode   client decode helpers (real)
  media-spice    native SPICE source via libspice FFI (feature-gated)
  media-capture / media-encode / input-inject   trait crates (thin; refactor targets)
                         # --- desktop UI / integrations ---
  pharos-panel   cross-platform egui control panel (Overview/Compositor/Video/HID/…)
  pharos-tray    KDE StatusNotifier tray (lighthouse states from host GPU health)
  pharos-assets  shared embedded art (lighthouse icons, idle screen)
  gamepad-bridge gamepad handling / virtual-pad glue
  integration-mqtt  Home-Assistant MQTT discovery + Wake-on-LAN
                         # --- client / device export ---
  trireme        desktop + reference client: decode, render, input, USB export
  usbip-export   bin `trireme-usbexport` — share a local USB device to the host
client/          Trireme — Kotlin Multiplatform (Android) + SwiftUI iOS baseline
rig/             build + launch helper scripts, embedded icon sources, workarounds log
```

## Build requirements

**Rust:** stable toolchain (CI builds on `stable`; no MSRV is pinned yet).

**Linux system packages** (Debian/Ubuntu names; use your distro's equivalents):

```sh
sudo apt-get install -y \
  libwayland-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev \
  libdrm-dev libudev-dev libusb-1.0-0-dev libxkbcommon-dev \
  libinput-dev libxcb1-dev libopus-dev pkg-config
```

**`ffmpeg`** is required at **runtime** as a subprocess (client decode, some capture/
encode paths) — install it from your distro.

**The `compositor` crate additionally needs an NVIDIA driver + CUDA** (it links
`libnvidia-encode` / `libnvcuvid` for zero-copy NVENC). On a machine without them,
build everything else and skip it:

```sh
# No NVIDIA / CUDA present — build the whole workspace except the compositor:
cargo build --workspace --exclude compositor
cargo test  --workspace --exclude compositor

# Full build (needs NVIDIA driver + CUDA):
cargo build --workspace
```

Quick self-test of the shared core:

```sh
cargo run -p hostd     # prints platform backends + proto self-test
```

### Kotlin bindings (UniFFI)

Generate the Kotlin bindings from the Rust core (library mode):

```sh
cargo build -p core-ffi
cargo run -p core-ffi --bin uniffi-bindgen -- \
  generate --library target/debug/libcore_ffi.so \
  --language kotlin --out-dir client/shared/generated/kotlin
```

The Android client has its own build path — see [`client/README.md`](client/README.md)
(`rig/build-android.sh` is the canonical build; note 32-bit TVs need `armeabi-v7a`).

## Codecs & patents

This project ships **no HEVC/AV1 codec implementation**. Encoding runs on the GPU's
hardware encoder (NVENC / Quick Sync / AMF) or an external `ffmpeg` binary; decoding
uses the client OS's decoder (Android MediaCodec) or `ffmpeg`. Any HEVC patent
obligations attach to those hardware/OS vendors, not to this software. If you need
royalty-free assurances, prefer the **AV1** path.

## Localization

The Pharos control panel UI is localized (**English + Norwegian Bokmål**, live-switchable).
The Trireme desktop client, tray, and CLI output are English-only for now. Translations
are welcome — add your locale alongside `en` in `crates/pharos-panel/locales/*.toml`.

## License

[**GPL-3.0-only**](LICENSE) — in keeping with the Sunshine/Moonlight lineage this
project descends from. Bundled third-party assets (the NVIDIA interface headers under
`vendor/`, Noto Sans Symbols fonts) carry their own permissive licenses; see their
respective files.
