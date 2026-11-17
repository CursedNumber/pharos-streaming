# Pharos + Trireme

**Play the games on your PC from your couch, your phone, or another room.**

Pharos runs on your gaming PC. Trireme runs on whatever you want to play on: a
TV, a phone, a laptop, a Raspberry Pi behind the telly. You pair them once with
a PIN, and from then on your PC's games, desktop, and controllers are a tap
away. It fills the same role as Sunshine and Moonlight, but it's a separate
design written from scratch in Rust and Kotlin: its own protocol over QUIC, and a
virtual screen for every client. (That also means Moonlight clients can't
connect to it, and Trireme doesn't talk to Sunshine.)

> *Pharos* was the lighthouse of Alexandria; a *trireme* is the small ship that
> steers by it. The host shines, the client sails home.

---

## Why you might like it

- **It leaves your monitor alone.** Pharos gives each client its own invisible
  screen, sized to fit that device exactly (your phone's odd aspect ratio, your
  TV's 4K panel). Someone can keep using the desk while you play on the TV.
  Prefer to see your actual desktop? It can mirror that too.
- **Built for WiFi, not just Ethernet.** Quality adapts on the fly: when the
  link gets crowded it lowers resolution before the picture falls apart, then
  climbs back up. 4K60 HDR over WiFi works on decent hardware.
- **Pair once, then forget about it.** A short PIN the first time; after that the
  devices recognise each other. No certificate warnings, no web console.
- **Controllers just work.** Gamepads appear on the PC as ordinary Xbox pads,
  with per-controller button and gyro tuning. A full Steam Big Picture session
  runs inside the stream, so even Steam Input games get your pad. Gyro aiming
  reaches emulators too, through a built-in DSU motion server: aim in Lichii
  with your phone's gyro ([how](docs/gyro-emulators.md)).
- **Plug a USB device into the client, and the PC sees it.** A controller, a
  flight stick, or a thumb drive plugged into your TV box shows up on the PC as a
  real local device.
- **The little things.** Text clipboard in both directions, HDR10, HEVC/AV1/H.264,
  low-latency Opus audio, Wake-on-LAN, and Home Assistant integration.

## What you need

| | Runs on | Notes |
|---|---|---|
| **Pharos** (host) | Linux (Wayland) | NVIDIA GPU required for now |
| | Windows 10/11 | NVIDIA, Intel, or AMD GPU |
| **Trireme** (client) | Android phones and Android/Google TV | arm64 and 32-bit TVs |
| | Linux and Windows desktops | |
| | Raspberry Pi and other ARM boards | can drive a TV directly, no desktop needed |
| | Xbox Series X\|S in Developer Mode | experimental |

## Getting started

1. **Install the host** on your gaming PC (see [Install](#install) below), then
   open **Pharos Panel** from your app menu.
2. **Install the client** on the device you want to play on and open Trireme.
   Your PC appears automatically if both are on the same network.
3. **Pick it, type the PIN** the host shows, and press play.

## Install

Prebuilt packages are on the
[Releases page](https://github.com/CursedNumber/pharos-streaming/releases),
built by CI from the tagged source.

**Ubuntu / Debian** (22.04+ / 12+): this script fetches the latest `.deb` and
installs it with apt.

```sh
# pick --host, --client, or --both
curl -fsSL https://raw.githubusercontent.com/CursedNumber/pharos-streaming/main/packaging/install.sh | bash -s -- --host
```

Or download the `.deb` yourself and run `sudo apt install ./pharos_*_amd64.deb`.

**Arch / CachyOS**: AUR packages `pharos-streaming` (builds from source) and
`pharos-streaming-bin` (prebuilt).

**Windows host**: run `pharos-<ver>-x86_64.msi`. It adds Start Menu shortcuts
and the firewall rule streaming needs. Portable `.exe` files are on the release
page too, if you'd rather not install. The Windows host also needs ffmpeg,
which isn't bundled: run `winget install Gyan.FFmpeg`, or drop `ffmpeg.exe`
next to `pharos-host.exe`. If it's missing, the tray tells you.

**Android / Android TV**: sideload `trireme-<ver>-<abi>.apk`. Use `arm64-v8a`
for phones and recent TVs, `armeabi-v7a` for older 32-bit TVs (such as the Sony
X90J), or `universal` if unsure.

**Anything else**: the `linux-x86_64.tar.gz` archives carry the binaries,
systemd units, and desktop files.

After installing the Linux host:

```sh
systemctl --user enable --now pharos-host pharos-tray
sudo systemctl enable --now pharos-usbimport   # optional: USB device forwarding
pharos-panel                                   # set up screens, apps, and clients
```

More packaging detail lives in [`packaging/`](packaging/README.md).

## What works today

Tested on real hardware and used every day:

- **120 fps at 1080p with zero dropped frames**, about 3 ms to encode a frame on
  an RTX 4070, on both a Sony Bravia TV and a Xiaomi phone.
- **4K60 HDR over WiFi**; the TV switches into HDR mode by itself.
- **Real Proton games** streaming end to end, with gamepads and live quality
  changes. No Man's Sky runs at the phone's native resolution with the
  controller fully working through the Steam session.
- **Mirroring the real KDE/GNOME desktop** at about 60 fps, keyboard and mouse
  included.
- **Gyro aiming in the Lichii emulator**, from the phone's own gyro under a
  clamp controller, tunable from the couch.
  See [Gyro aiming in emulators](docs/gyro-emulators.md).
- **Games tested so far** include No Man's Sky, Cyberpunk 2077, Crimson Desert,
  and the Lichii emulator. Not yet tested with Grand Theft Auto VI.
- **Windows hosting** with NVENC, plus clients on Android, Linux, Windows, a
  Raspberry Pi, and an Xbox Series X in Developer Mode.

### Honest caveats

This is pre-1.0 software. Settings and config keys may still change.

- **AMD and Intel GPUs on a Linux host** are not supported yet; the Linux host
  encodes on NVIDIA only. On an AMD box, the Windows host works.
- **HDR inside Proton games on Linux** is blocked by Wine itself. Pharos's own HDR
  streaming works.
- **Streaming one screen to several clients at once** exists but is off by
  default and not yet tested on two devices. (Mirroring your real desktop is a
  different feature, and that one is tested.)
- **USB audio devices and webcams** cannot be forwarded yet.

## Security

Pharos listens on the network, types on your PC's keyboard, and can attach USB
devices to it. Treat it with that respect:

- Run it on a network you trust. There has been no independent security audit.
- Only paired devices may connect (that's on by default). Setting
  `PHAROS_ALLOW_UNPAIRED=1` turns this off; don't, except for testing.
- To play away from home, prefer a VPN such as WireGuard over opening a port.

Found a vulnerability? Please report it privately; see [`SECURITY.md`](SECURITY.md).

## Under the hood

For the curious and for contributors. Every command, environment variable, and
config file is documented in [`DOCUMENTATION.md`](DOCUMENTATION.md); old design
notes live in [`docs/history/`](docs/history/).

One shared Rust **core** holds the protocol, pairing (SPAKE2 + Ed25519
identities), session logic, and USB/IP framing. The host links it natively and
the clients reach it through UniFFI, so the wire format and pairing have exactly
one implementation and one test suite.

```
                          ┌───────────────────────────────┐
                          │   core (Rust, shared)         │
                          │  proto · crypto/SPAKE2 ·      │
                          │  session · usbip framing      │
                          └──────▲──────────────▲─────────┘
                       native    │              │  UniFFI (JNI)
              ┌──────────────────┴───┐    ┌─────┴────────────────────┐
              │  Pharos host (Rust)  │    │  Trireme client          │
              │  compositor·NVENC·   │    │  decode·render·input·    │
              │  inject·usbip-host   │    │  USB export (Kotlin+Rust)│
              └──────────────────────┘    └──────────────────────────┘
```

- **Linux host**: a headless Smithay Wayland compositor renders each client's
  screen and hands frames straight from GLES through CUDA into NVENC, with no
  copies through system memory. Xwayland runs inside for X11 apps.
- **Windows host**: WGC/ddagrab capture plus an IddCx virtual display, encoded
  with Quick Sync, NVENC, or AMF; input via SendInput.
- **Transport**: everything travels over one QUIC connection. Control and input
  use reliable streams, video and audio go as unreliable deadline-aware
  datagrams, and each USB device gets its own stream so a stalled device can't
  hold up your gamepad.
- **Trust**: pairing pins a long-term identity and every connection is checked
  against `peers.toml`. The QUIC TLS certificate is just a tunnel, never a trust
  anchor.
- **Control**: a native egui panel (`pharos-panel`) and a tray icon
  (`pharos-tray`) drive the host live; there is no browser UI.

### Building from source

System packages (Debian/Ubuntu names):

```sh
sudo apt-get install -y \
  libwayland-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev \
  libdrm-dev libudev-dev libusb-1.0-0-dev libxkbcommon-dev \
  libinput-dev libxcb1-dev libopus-dev pkg-config \
  clang libclang-dev libpipewire-0.3-dev
```

`ffmpeg` is needed at runtime. Mirroring the real desktop also needs
`xdg-desktop-portal` with a backend for your desktop (`-kde`, `-gnome`, or
`-wlr`); without one, the host just uses virtual screens.

The `compositor` crate links NVIDIA's encoder libraries, so it needs an NVIDIA
driver and CUDA. Everything else builds without them:

```sh
cargo build --workspace --exclude compositor   # no NVIDIA/CUDA
cargo build --workspace                         # full build
pharos-compositor capture-zc-multi              # run the streaming host
```

The Android client builds with `rig/build-android.sh`; see
[`client/README.md`](client/README.md).

## Codecs & patents

This project ships **no HEVC or AV1 codec of its own**. Encoding runs on the
GPU's hardware encoder (NVENC, Quick Sync, AMF) or an external `ffmpeg`;
decoding uses the client's own decoder (Android MediaCodec) or `ffmpeg`. Any HEVC
patent obligations sit with those hardware and OS vendors. For a royalty-free
path, choose AV1.

## Languages

The control panel speaks **English and Norwegian Bokmål** and switches live. The
desktop client, tray, and command-line output are English only for now.
Translations are welcome: add your locale next to `en` in
`crates/pharos-panel/locales/*.toml`.

## License

Copyright (C) 2026 The Pharos/Trireme authors.

This program is free software: you can redistribute it and/or modify it under
the terms of the **GNU General Public License** as published by the Free Software
Foundation, either **version 3 of the License, or (at your option) any later
version**: [`GPL-3.0-or-later`](LICENSE).

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

Given freely, for everyone, and copyleft so it stays that way downstream.
Bundled third-party material (the NVIDIA interface headers under `vendor/`, the
Noto Sans Symbols fonts) carries its own licenses; see [`NOTICE`](NOTICE).
