# pharos-streaming: Pharos (host) · Trireme (client)

**Self-hosted, low-latency game streaming and remote desktop (KVM-over-IP).**
A clean-slate, from-scratch successor to Sunshine/Moonlight; Rust host and shared
core, Kotlin/Android + Rust desktop client, a hand-rolled headless Wayland
compositor, and zero-copy NVENC/AV1 over QUIC.

- **Pharos**: the host daemon (Rust). The lighthouse the client navigates by.
- **Trireme**: the client (Kotlin Multiplatform, Android first; plus a Rust
  desktop/reference client). The small craft that comes alongside.
- **core**: one Rust core (protocol, crypto/pairing, session, USB/IP framing),
  shared by both ends via UniFFI, so the wire format and pairing have exactly one
  implementation.

> Detailed CLI, environment variables, and config reference live in
> [`DOCUMENTATION.md`](DOCUMENTATION.md). Archived design notes are in
> [`docs/history/`](docs/history/).

---

## Highlights

Proven on real hardware, and used day-to-day for game streaming:

- **120 fps @ 1080p, zero dropped frames**: headless Smithay → GLES→CUDA→NVENC
  HEVC → QUIC → decode/render, **~3 ms encode**, zero-copy end to end on an
  RTX 4070; pixel-correct on a Sony Bravia and a Xiaomi phone. This is the
  regression baseline later work may not fall below.
- **Real Proton games stream end-to-end** over native Wayland (winewayland /
  proton-cachyos); gamepad passthrough, live resolution/bitrate ladder, and an
  optional Steam-launch redirect, validated in live sessions.
- **End-to-end HDR10**: the host emits conformant 10-bit HEVC Main10 (BT.2020 /
  PQ + Mastering-Display/CLL SEI) and the TV **enters HDR mode off the stream on
  its own**.
- **AV1 validated end-to-end**, including **4K60**: nearly free on Ada GPUs, and
  a royalty-free alternative to HEVC.
- **Sub-second mid-stream resync**: 1 s IDRs with repeated sequence headers, so a
  client that joins mid-stream is watching in ≤1 s.
- **Reversed USB/IP**: export a controller, stick, or drive **from the client**
  and it appears as a *real* device on the host (HID + mass-storage, verified over
  the LAN).
- **Mirror your real desktop**: stream the physical KDE/GNOME session you are
  sitting at (not just the headless virtual display) through the same zero-copy
  NVENC pipeline, with libei keyboard/mouse injection back and best-fit letterbox
  scaling for odd aspect ratios; live-validated at ~60 fps HEVC on an RTX 4070.
- **Windows host**: WGC/ddagrab + an IddCx headless virtual display →
  Quick Sync / NVENC / AMF HEVC with SendInput; NVENC HEVC hosting validated on a
  real GPU rig.

## Features

**Streaming pipeline**
- One QUIC pipeline for **keyboard, video, mouse, and USB**: operate a headless
  host remotely; game streaming is a first-class *mode* on the same
  capture→encode→transport path, not a separate product.
- **Headless Smithay compositor** (Linux) that owns the display and captures its
  own GLES output **zero-copy** through CUDA into NVENC. Xwayland inside for X11
  apps. No EVDI, no gamescope wrap; full frame-lifecycle control for latency.
- **Selectable HEVC + AV1**, plus a real **HDR10** 10-bit Main10 path (behind
  `PHAROS_HDR=1`).
- **End-to-end low-latency Opus** audio (48 kHz stereo, 20 ms frames, LowDelay),
  captured from a per-session PipeWire null-sink monitor.
- **Adaptive resolution / bitrate / fps ladder**: a QoS pacer and live actuators
  step quality down under sustained link pressure; the top rung is a
  byte-identical direct blit.

**Input**
- **Keyboard + mouse (move, buttons, scroll) + gamepad** from the desktop client
  into the host seat; **relative mouse** (for pointer-locked / FPS games) is
  handled host-side on both Linux and Windows.
- **Controller profiles + gyro-aim**: per-controller button-swap and gyro tuning,
  relayed live and persisted host-side; gyro angular velocity becomes relative
  mouse motion. Client gamepads land on per-slot virtual Xbox 360 pads.

**Beyond one screen**
- **Real-desktop mirror** (Linux): launch the built-in `desktop` app and Pharos
  streams your physical KDE/GNOME session through the full HEVC pipeline, captured
  via xdg-desktop-portal + PipeWire off the render loop, with libei input
  injection and best-fit letterboxing. Distinct from the multi-client fan-out
  below (this mirrors the *real* screen; that fans *one virtual display* out).
- **Steam Big Picture session** (Linux): a built-in tile runs the whole Steam
  client Deck-style — gamescope-hosted, inside the stream — so Steam Input API
  games get controllers and every Steam dialog is drivable from the couch;
  *shipped, not yet live-validated* (issue #38).
- **Native VNC and SPICE console sources** in the client, so a Proxmox/QEMU VM
  console can share a split-screen cell alongside a Pharos stream, fully
  interactive.
- **Multi-client mirror/extend** fan-out (keep-N); *experimental, off by default,
  not yet two-device-validated.*
- **Home Assistant MQTT + Wake-on-LAN**: auto-discovered HA entities and remote
  wake.

**Control & trust**
- **Native control plane, no browser**: a cross-platform egui panel
  (`pharos-panel`) and a KDE tray (`pharos-tray`) drive the host live over an
  authenticated channel. No self-signed-cert wall.
- **SPAKE2 pairing + per-connection Ed25519 auth, on by default**: pairing pins a
  long-term identity; every connection is authorized against `peers.toml`. The
  QUIC TLS cert is a throwaway opaque tunnel, never a trust anchor.

## ⚠️ Security notice

Pharos is a **network-listening daemon that injects input and can bridge USB
devices** into the host. Treat it accordingly:

- Run it on a **trusted LAN**, not the open internet. There is no hardening audit yet.
- Client authentication is **on by default**; `PHAROS_ALLOW_UNPAIRED=1` disables
  it; don't set that on an untrusted network.
- Exposing the stream port to the internet is at your own risk; prefer a
  VPN/WireGuard tunnel.

Found a security issue? Please report it privately; see [`SECURITY.md`](SECURITY.md).

## Architecture

One shared Rust **core** is the keystone; `core-proto` (QUIC channel map,
length-prefixed access-unit framing, the input wire format), `core-crypto` (SPAKE2
pairing + Ed25519 identity/session keys), `core-session` (session FSM + capability
negotiation), and `core-usbip` (USB/IP URB framing) are compiled natively into the
host and exported to the Kotlin/Swift clients via `core-ffi`/**UniFFI**, so pairing
and the protocol have exactly one implementation and test suite. A `pal` trait
crate is the Windows/Linux co-equal seam.

```
                          ┌───────────────────────────────┐
                          │   core (Rust, shared)         │
                          │  proto · crypto/SPAKE2 ·       │
                          │  session · usbip framing       │
                          └──────▲──────────────▲──────────┘
                       native    │              │  UniFFI (JNI)
              ┌──────────────────┴──┐    ┌──────┴───────────────────┐
              │  Pharos host (Rust)  │    │  Trireme client          │
              │  compositor·NVENC·   │    │  decode·render·input·    │
              │  inject·usbip-host   │    │  USB export (Kotlin+Rust)│
              └──────────────────────┘    └──────────────────────────┘
```

**QUIC channel map** (`transport`, quinn): a reliable bidi **control** stream, a
reliable **input** back-channel, **video/audio** as unreliable deadline-aware
datagrams, and **one reliable stream per USB/IP device** (so a stalled device can't
head-of-line-block your gamepad).

- **Linux host**: `compositor` (headless Smithay → zero-copy NVENC), `trireme-kms`
  (direct DRM/KMS scanout to a real TV), `usbip-host` (`pharos-usbimport`),
  `app-launcher` (launch into the display + Steam redirect).
- **Windows host**: `pharos-win` (`pharos-host`): WGC/ddagrab + IddCx virtual
  display → Quick Sync/NVENC/AMF HEVC with SendInput.
- **Clients**: `trireme` (Rust desktop/reference: ffmpeg decode, winit render,
  input, USB export) and the Kotlin Multiplatform Android app (MediaCodec decode,
  controller/gyro input), with a SwiftUI iOS baseline.

For the full crate map, see [`DOCUMENTATION.md`](DOCUMENTATION.md).

## Install

Prebuilt packages come from [GitHub Releases](https://github.com/CursedNumber/pharos-streaming/releases)
(built by CI from the tagged source; x86_64 Linux and Windows). Two packages:
**pharos** is the host (the gaming PC that streams out; the Linux host needs an
**NVIDIA GPU** for now, AMD/VAAPI encode is stubbed and unvalidated), **trireme**
is the client (the machine you play on).

**Ubuntu / Debian** (22.04+/12+): the install script fetches the latest
release's `.deb` and installs it with apt:

```sh
# host, client, or --both
curl -fsSL https://raw.githubusercontent.com/CursedNumber/pharos-streaming/main/packaging/install.sh | bash -s -- --host
curl -fsSL https://raw.githubusercontent.com/CursedNumber/pharos-streaming/main/packaging/install.sh | bash -s -- --client
```

Or grab the `.deb` from the release page yourself and
`sudo apt install ./pharos_*_amd64.deb` (apt resolves the dependencies).

**Arch / CachyOS**: AUR packages `pharos-streaming` (build from source) and
`pharos-streaming-bin` (prebuilt release binaries); both split into `pharos`
and `trireme`.

**Windows host**: `pharos-<ver>-x86_64.msi` installs the host stack
(`pharos-host`, `pharos-tray`, `pharos-panel`) with Start Menu shortcuts and
the firewall exception streaming needs. The same three exes are also on the
release page bare, as the portable/no-admin option.

**Android / Android TV client**: sideload `trireme-<ver>-<abi>.apk` from the
release page — `arm64-v8a` for phones and recent TVs, `armeabi-v7a` for
32-bit TVs (e.g. Sony X90J), or `universal` if unsure.

**Anything else**: `pharos-<ver>-linux-x86_64.tar.gz` and
`trireme-<ver>-linux-x86_64.tar.gz` on the release page carry the binaries,
systemd units, and desktop files; copy them where your distro likes.

**After installing the host:**

```sh
systemctl --user enable --now pharos-host pharos-tray
sudo systemctl enable --now pharos-usbimport   # optional: USB/IP forwarding
pharos-panel                                   # configure outputs, apps, clients
```

After installing the client, run `trireme` (also in the app menu): hosts on
the LAN are discovered automatically, then you pair once with the on-screen
PIN. Packaging sources live in [`packaging/`](packaging/README.md).

## Quickstart (Linux host)

**System packages** (Debian/Ubuntu names; use your distro's equivalents):

```sh
sudo apt-get install -y \
  libwayland-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev \
  libdrm-dev libudev-dev libusb-1.0-0-dev libxkbcommon-dev \
  libinput-dev libxcb1-dev libopus-dev pkg-config \
  clang libclang-dev libpipewire-0.3-dev
```

`ffmpeg` is required at **runtime** (client decode / some capture paths).
`clang`/`libpipewire-0.3-dev` are for the compositor's desktop-mirror capture
(issue #6, `portal-mirror` feature — on by default on Linux); at runtime it
also needs `xdg-desktop-portal` plus a backend for your desktop (e.g.
`xdg-desktop-portal-kde`/`-gnome`/`-wlr`) to actually mirror the real screen —
without one, the host just falls back to virtual displays.

**Build.** The `compositor` crate needs an NVIDIA driver + CUDA (it links
`libnvidia-encode` / `libnvcuvid`); NVENC is the only wired Linux encoder today,
with an AMD/VAAPI path stubbed but not yet validated. Without NVIDIA/CUDA, build
everything else:

```sh
cargo build --workspace --exclude compositor   # no NVIDIA/CUDA
cargo build --workspace                         # full (needs NVIDIA + CUDA)
cargo run -p hostd                              # quick core self-test
```

**Run the streaming host** and connect a client:

```sh
pharos-compositor capture-zc-multi              # resident multi-output host
pharos-compositor --help                        # every subcommand
```

The Android client is built via `rig/build-android.sh`; see
[`client/README.md`](client/README.md). Full command reference, env vars, and
config keys: [`DOCUMENTATION.md`](DOCUMENTATION.md).

## Status & honesty

Pre-1.0 research-quality software; interfaces, config keys, and env vars may change
between commits. Known honest caveats:

- **AMD (VAAPI) GPU encode on the Linux host is a stub, pending validation.** The
  Linux compositor encodes through **NVENC (NVIDIA only)** today; the VAAPI backend
  is scaffolded behind the shared encoder trait but not yet wired or hardware-
  validated. On an AMD box, run the Windows host (AMF) for now.
- **Linux Proton *game*-HDR** is blocked upstream in Wine's Vulkan/Wayland WSI and
  is deferred to the Windows-native host path. Host HDR10 encode itself works.
- **Multi-client mirror/extend** (fan one virtual display out to several clients)
  is shipped but **off by default and not two-device validated**. This is a
  separate feature from real-desktop mirroring, which is validated.
- USB/IP **isochronous** (USB-audio / webcams) has wire format + framing but iso
  servicing is deferred.

## Codecs & patents

This project ships **no HEVC/AV1 codec implementation**: encoding runs on the
GPU's hardware encoder (NVENC / Quick Sync / AMF) or an external `ffmpeg` binary;
decoding uses the client OS's decoder (Android MediaCodec) or `ffmpeg`. Any HEVC
patent obligations attach to those hardware/OS vendors, not to this software. For
royalty-free assurances, prefer the **AV1** path.

## Localization

The Pharos control panel is localized (**English + Norwegian Bokmål**,
live-switchable). The Trireme desktop client, tray, and CLI output are English-only
for now. Translations welcome; add your locale alongside `en` in
`crates/pharos-panel/locales/*.toml`.

## License

Copyright (C) 2026 The Pharos/Trireme authors.

This program is free software: you can redistribute it and/or modify it under the
terms of the **GNU General Public License** as published by the Free Software
Foundation, either **version 3 of the License, or (at your option) any later
version**: [`GPL-3.0-or-later`](LICENSE).

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

Given freely, for everyone, and copyleft so it stays that way for everyone
downstream. Bundled third-party material (the NVIDIA interface headers under
`vendor/`, the Noto Sans Symbols fonts) carries its own licenses; see
[`NOTICE`](NOTICE).
