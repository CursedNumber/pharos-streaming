# Pharos / Trireme: Reference Documentation

Detailed reference for the command-line tools, environment variables, and
configuration files. For an overview and the feature list, see
[`README.md`](README.md).

**Contents**
- [Crate map](#crate-map)
- [Network ports](#network-ports)
- [Command-line reference](#command-line-reference)
  - [pharos-compositor](#pharos-compositor-linux-host) · [pharos-host](#pharos-host-windows-host) · [hostd](#hostd) · [trireme](#trireme-desktop-client) · [trireme-usbexport](#trireme-usbexport) · [pharos-usbimport](#pharos-usbimport)
- [Environment variables](#environment-variables)
- [Configuration files](#configuration-files)
- [Desktop mirroring (experimental)](#desktop-mirroring-experimental)
- [Pairing & trust model](#pairing--trust-model)
- [Input capture & pacing](#input-capture--pacing)

---

## Crate map

**Shared core** (compiled into the host, exported to clients via UniFFI)
| crate | role |
|---|---|
| `pal` | platform-abstraction traits (the Windows/Linux co-equal seam) |
| `core-proto` | wire protocol: QUIC channel map, framing, access units, `InputEvent` |
| `core-crypto` | SPAKE2 pairing, Ed25519 identity/session keys |
| `core-session` | session FSM + capability negotiation |
| `core-usbip` | USB/IP URB wire protocol + device-class policy |
| `core-ffi` | UniFFI surface → Kotlin + Swift bindings |
| `host-core` | shared host glue (pairing/identity, config) for both OS hosts |
| `host-info` | host capability / backend probing |
| `control-ipc` | control-plane IPC (panel ⇄ host) |
| `transport` | QUIC (quinn) channels: control, input, video, audio, USB/IP |

**Linux host**
| crate | binary | role |
|---|---|---|
| `compositor` | `pharos-compositor` | headless Smithay compositor → zero-copy NVENC HEVC/AV1 |
| `trireme-kms` | `trireme-kms` | direct DRM/KMS scanout client (drive a real TV headless) |
| `usbip-host` | `pharos-usbimport` | USB/IP import bridge (vhci-hcd virtual hub) |
| `app-launcher` |  | launch apps/games into the virtual display; Steam redirect |
| `hostd` | `hostd` | proto self-test daemon (M0 skeleton) |

**Windows host**
| crate | binary | role |
|---|---|---|
| `pharos-win` | `pharos-host` | WGC/ddagrab + IddCx → Quick Sync/NVENC/AMF HEVC, SendInput |

**Media / integrations**
| crate | role |
|---|---|
| `media-audio` | Opus capture/encode |
| `media-decode` | client decode helpers (HW decoder probing) |
| `media-spice` | native SPICE source via libspice FFI (feature-gated) |
| `media-capture` / `media-encode` / `input-inject` | trait crates (thin) |
| `pharos-panel` | cross-platform egui control panel |
| `pharos-tray` | KDE StatusNotifier tray (state follows host GPU health) |
| `pharos-assets` | shared embedded art |
| `gamepad-bridge` | gamepad handling / virtual-pad glue |
| `integration-mqtt` | Home-Assistant MQTT discovery + Wake-on-LAN |

**Clients**
| crate | binary | role |
|---|---|---|
| `trireme` | `trireme` | desktop/reference client: decode, render, input, USB export |
| `usbip-export` | `trireme-usbexport` | share a local USB device to the host |
| `client/` (Kotlin) |  | Android app (+ SwiftUI iOS baseline) |

## Network ports

| Port | Proto | Constant | Purpose |
|---|---|---|---|
| **15777** | UDP/QUIC | `STREAM` | video/audio/input stream (current default; both hosts) |
| **15778** | TCP | `CONTROL` | host↔panel control plane (`HostStatus`) |
| **4432** | UDP/QUIC | `PAIR` | SPAKE2 pairing listener |
| **4434** | UDP/QUIC | `USBIP` | USB/IP relay (export → import bridge) |
| 4433 | UDP/QUIC | `STREAM_LEGACY` | **deprecated** legacy stream port |

The Windows host previously bound its own stream port (`4435`); it now uses the
canonical `15777` like the Linux host, so one firewall/WAN rule covers both.
Migration: a client that had *manually* saved a Windows host at `:4435` must
update it to `:15777`; mDNS-discovered hosts carry the new port automatically, so
re-discovered hosts fix themselves.

If you move the stream port, remember to open it on the firewall
(`ufw allow 15777/udp`).

---

## Command-line reference

Every binary supports the usual `RUST_LOG` tracing filter. Details below.

### pharos-compositor (Linux host)

The Linux host: a headless Wayland compositor and NVENC capture/stream harness.
With **no command** it runs a resident 1920×1080@60 virtual display (a
`WAYLAND_DISPLAY` socket, output `HEADLESS-1`, no EDID/DRM connector); the bare
compositor only, it does **not** encode. For the real streaming host use
`capture-zc-multi`.

```
USAGE: pharos-compositor [COMMAND] [ARGS...]

MAIN / STREAMING
  capture-zc-multi [config.toml]   The production entry point. N virtual outputs in
                                   one event loop over one Wayland socket, each with
                                   its own NVENC session + QUIC stream. No path =
                                   $PHAROS_CONFIG_DIR/multi.toml. Runs until killed.
  capture-zc [out] [frames] [client] [fps]
                                   Single-output zero-copy capture/stream (one-output
                                   MultiCfg). out default "pharos-zc.hevc" (or
                                   quic://HOST:PORT). frames IGNORED (runs until
                                   killed). client default "alacritty" ("none"=empty).
                                   fps default 60.
  capture-gl [out] [frames] [client]   GLES capture path. out "pharos-capture-gl.mp4",
                                   frames 120, client "alacritty".
  capture [out] [frames]           Basic render→NVENC HEVC to a file (no client
                                   surfaces). out "pharos-capture.mp4", frames 120.

DIAGNOSTIC / TEST
  nvenc-test [out] [frames]        Direct NVENC-from-CUDA SDR test pattern.
  nvenc-test-hdr [out] [frames]    10-bit HEVC Main10 (BT.2020/PQ + HDR10 SEI) pattern.
  input-demo [out] [frames]        Launch `alacritty -e cat`, inject "pharos", capture.
  mirror                           EXPERIMENTAL session-mirror smoke test; needs a
                                   build with --features portal-mirror (see the
                                   Desktop mirroring section below).

CONTROL / MANAGEMENT
  launch-app <slug>                Ask the RUNNING host to launch a saved app
                                   (apps.toml slug) into the display. Needs a running
                                   host; non-zero if unreachable or slug refused.
  apps                             List saved apps (slug, kind, name) + built-ins.
  pair [addr]                      Standalone SPAKE2 pairing listener. addr default
                                   0.0.0.0:4432.
  -h, --help, help                 Print help, exit 0.
  -V, --version                    Print version, exit 0.

EXIT CODES: 0 success · 1 operation error · 2 launch-app with no <slug>.
```

> All capture/stream commands render at 1920×1080; only `capture-zc` takes an `fps`
> arg and only `capture-zc-multi` takes true per-output geometry (from its TOML).
> The host is single-client/serial: a new client pre-empts the active one unless
> mirror mode is enabled from the panel.

### pharos-host (Windows host)

Captures a display, encodes HEVC (Intel Quick Sync / NVIDIA NVENC / AMD AMF via an
ffmpeg subprocess), and serves it over the same QUIC transport the Linux compositor
uses. Input is injected via Win32 SendInput; audio is WASAPI-loopback Opus. Pairing
uses the shared core, so an unmodified Trireme client connects to either host OS.

```
USAGE: pharos-host <SUBCOMMAND> [ARGS...]

  (no subcommand)                        Run `stream` with all defaults, and surface
                                         the pairing PIN to the console. This is what
                                         a double-clicked / bare-invoked host does.
  pair [addr]                            SPAKE2 pairing listener; prints a rotating
                                         6-digit PIN, writes peers.toml. addr default
                                         0.0.0.0:4432. (Rarely needed now: the host
                                         opens an always-on pairing listener itself.)
  stream [bind] [output_idx] [fps] [qsv|nvenc|amf] [dda|wgc]
                                         Start the QUIC video host. bind default
                                         0.0.0.0:15777; output_idx 0; fps 60; encoder
                                         auto (GPU-vendor detect); capture dda
                                         (default); use wgc to capture an IddCx
                                         virtual display or across adapters.
  wgc-test [monitor_idx] [frames] [out.bgra]
                                         Capture N frames via Windows.Graphics.Capture,
                                         dump the last as raw BGRA (validates WGC).

With no subcommand: runs the streaming host (above). An UNRECOGNIZED subcommand
prints usage, exit 0. On non-Windows: prints a message and exits.
```

The `stream` path: loads/creates the Ed25519 identity, enforces pairing by default
(`PHAROS_ALLOW_UNPAIRED=1` to bypass), optionally pins to a NIC (`config.toml
[network] bind_mac`), publishes `HostStatus` on TCP `15778`, and runs a recovery
ladder (exponential backoff + circuit breaker → sticky-red **Faulted**).

**First connect (Windows).** Just run `pharos-host` (no arguments). The host:

- advertises itself on the LAN over mDNS (`_pharos._udp`), so a fresh Trireme lists
  it with no typed IP or port;
- opens an **always-on** pairing listener at startup, so there is no separate
  `pharos-host pair` terminal to run; and
- surfaces the current 6-digit PIN in the **panel**, the **tray**, and the
  **console** log (`pairing PIN: …`).

Enter that PIN on the Trireme's pair dialog and you are paired; the host streams on
the canonical port **15777**. After eight *consecutive* failed attempts the host
locks pairing out as a brute-force guard; **relaunch** `pharos-host` to re-enable it
(a successful pair resets the counter, so ordinary fumbles never trip it).

### hostd

Milestone-M0 self-test daemon. Takes **no** arguments or env vars; prints a
diagnostic block (version, protocol version, platform, PAL backends) and runs a
proto encode→decode round-trip assertion. Exit 0 on pass; aborts on assertion
failure.

```
USAGE: hostd
```

### trireme (desktop client)

The Linux/desktop reference client: a server-selector GUI, a windowed HEVC/AV1
player (single or multi-source grid, plus native VNC/SPICE), SPAKE2 pairing, a
server registry, Wake-on-LAN, and headless decode/USB utilities.

```
USAGE
  trireme                                Server-selector GUI, then the player (default)
  trireme menu                           Same as bare `trireme`
  trireme servers [list|add|remove ...]  Manage the saved-server registry
  trireme pair <host:pair_port> <PIN>    SPAKE2 pairing handshake (headless)
  trireme decode <host:port> [out_dir] [pubkey_hex]
                                         Headless: decode the live stream to PNG snapshots
  trireme play                           Load the default sources config (else a 1×1 fallback)
  trireme play <host:port> [pace]        Legacy single-source windowed playback
  trireme play <config.toml>             Explicit multi-source (grid) playback
  trireme wake <host|mac>                Send a Wake-on-LAN magic packet (opt-in)
  trireme usb-list                       Enumerate local USB devices (like lsusb)

EXIT CODES: 0 success · 1 runtime error · 2 unknown subcommand.
```

- `servers add <host> <port> [alias]` / `servers remove <id|host>` manage the registry.
- `pair` uses `$HOSTNAME` as the alias announced to the host; the PIN is redacted in logs.
- `decode` defaults to `127.0.0.1:15777` → `/tmp/trireme-frames`; a wrong-length
  `pubkey_hex` errors. Live-test env hooks (see below) can send one-shot
  resolution/bitrate/fps preferences.
- `play` probe order for the sources config: `$TRIREME_CONFIG`, then
  `$XDG_CONFIG_HOME/trireme/sources.toml`, then `~/.config/trireme/sources.toml`,
  then `./trireme-sources.toml`.

### trireme-usbexport

Claims local USB devices via libusb and services USB/IP URBs against them over the
QUIC relay to a Pharos import bridge; one connection per device, with QoS-lite
fast/bulk lanes.

```
USAGE
  trireme-usbexport list [config]            List local USB devices; flag shared ones
  trireme-usbexport run [config]             Share every device in the config (supervised)
  trireme-usbexport <pharos:port> <vid:pid>  Share exactly one device directly

CONFIG (trireme-usb.toml)
  pharos = "198.51.100.50:4434"   # import-bridge address (required)
  share  = ["045e:028e", ...]     # vid:pid of devices to export

EXIT CODES: 0 success · 1 runtime error · 2 no/invalid subcommand.
```

Routing is advisory: `relay` (HID, mass-storage, hubs…), `pair-and-forward`
(Bluetooth radio; pair on the client instead; USB/IP can't carry BT bonding),
`unsupported (iso)` (USB-audio / UVC; framed and cleanly rejected).

### pharos-usbimport

The Linux-only, root USB/IP import bridge. Listens for exporters, reads each
device's handshake, hands a socketpair to the kernel `vhci_hcd` driver to
materialize the remote device locally, and pumps URB bytes both ways.

```
USAGE: sudo pharos-usbimport [listen-addr]      # default 0.0.0.0:4434
```

Requires the `vhci_hcd` kernel module (`modprobe vhci-hcd`) and root. Serves
multiple devices concurrently, each its own connection + vhci port.

---

## Environment variables

Presence-only flags act on being *set to any value* unless noted. `RUST_LOG` (a
tracing `EnvFilter`) is honored by every binary.

**Paths & control plane**
| var | scope | default | effect |
|---|---|---|---|
| `PHAROS_CONFIG_DIR` | host | `~/.config/pharos` (Win: `%APPDATA%\pharos`) | host config dir (peers/apps/config/identity) |
| `PHAROS_CONTROL_ADDR` | tool | unset (local UDS) | point `pharos-panel` at a remote host (`host[:15778]`) |
| `PHAROS_CONTROL_SOCK` | both | `$XDG_RUNTIME_DIR/pharos/control.sock` | host↔panel control socket path (both must agree) |
| `PHAROS_PREVIEW_PATH` | both | `…/pharos/preview.jpg` | live-preview JPEG path |
| `PHAROS_HOST_CMD` | tool | unset | command the panel runs to launch a host process |

**Auth**
| var | scope | default | effect |
|---|---|---|---|
| `PHAROS_ALLOW_UNPAIRED` | host | unset (enforced) | accept **any** client without pairing (dev only) |

**Rate control & buffering (Linux host)**
| var | default | effect |
|---|---|---|
| `PHAROS_BITRATE_KBPS` | unset | target average encoder bitrate (kbit/s); overrides config |
| `PHAROS_MAX_BITRATE_KBPS` | unset (= avg → CBR) | bitrate ceiling (kbit/s); clamped ≥ average |
| `PHAROS_STREAM_WINDOW_KB` | unset (adaptive) | pins ALL QUIC flow-control windows to a fixed size (KB) and disables the adaptive sizing. Unset, the windows auto-size: 8MB memory cap, and the client resizes its receive window each second to ~250ms of the measured video rate (512KB floor). Set on BOTH ends when pinning |
| `PHAROS_QUEUE_FRAMES` | 8 | bounded video-frame channel depth (P-frames drop-newest) |
| `PHAROS_AUDIO_QUEUE_PACKETS` | 8 | bounded audio-packet channel depth (drop-newest) |
| `PHAROS_AUDIO_CAPTURE_MS` | 15 | `parec` fragment buffer (ms) for null-sink capture |
| `PHAROS_MIN_IDR_INTERVAL_MS` | 150 | minimum interval between forced IDRs |
| `PHAROS_CLIENT_WAIT_SECS` | 0 (wait forever) | bail if no client connects within N s (test harness) |

**HDR / color**
| var | default | effect |
|---|---|---|
| `PHAROS_HDR` | unset (SDR) | enable HDR10 (HEVC only; widens the whole chain to 10-bit) |
| `PHAROS_ENABLE_COLOR_MGMT` | unset | enable the `wp_color_manager_v1` handshake (experimental) |

**Gyro → mouse tuning** (seed the feel; live-tunable afterward via the panel)
| var | effect |
|---|---|
| `PHAROS_GYRO_SENS` | base sensitivity (px/degree), both axes |
| `PHAROS_GYRO_SENS_X` / `_Y` | per-axis sensitivity override |
| `PHAROS_GYRO_ACCEL` | acceleration strength (0 = off) |
| `PHAROS_GYRO_ACCEL_POW` | attack-curve exponent (1 = linear) |
| `PHAROS_GYRO_ACCEL_REF` | rotation speed (deg/s) where the curve hits 1× |
| `PHAROS_GYRO_ACCEL_MAX` | clamp on the extra acceleration gain |
| `PHAROS_GYRO_INVERT_X` / `_Y` | flip that axis |
| `PHAROS_GYRO_AXIS_H` / `_V` | pick the source gyro axis (`rx`/`x`/`0`, `ry`/`y`/`1`, `rz`/`z`/`2`) |
| `PHAROS_GAMEPAD_SWAP_FACE` | globally swap A/B and X/Y for un-profiled pads |

**Steam redirect** (host writes the `PHAROS_MARK_*` marker; the shim reads it)
| var | effect |
|---|---|
| `PHAROS_STEAM_REDIRECT` | master switch for the Steam-game→stream auto-redirect |
| `PHAROS_NO_REDIRECT` | per-launch opt-out (`1` = desktop passthrough) |
| `PHAROS_MARK_PID` / `_START` / `_SOCKET` | liveness/PID-reuse guards read by the shim |
| `PHAROS_WAYLAND_DISPLAY` | Wayland socket the shim exports so the game renders into the stream |
| `PHAROS_PULSE_SINK` / `PHAROS_PROTON_ENABLE_WAYLAND` | per-stream audio sink / Proton Wayland toggle |

**Diagnostics & Windows host**
| var | effect |
|---|---|
| `PHAROS_FRAME_TIMINGS` (+`_FLUSH_SECS`, default 5) | per-stage frame timing stats |
| `PHAROS_FAULT_INJECT` | Linux NVENC fault seam: `kind[:period]` (busy\|need_more\|lock_busy\|device_lost\|oom\|generic) |
| `PHAROS_WIN_FAULT_INJECT` | Windows analogue (`device_lost[:P]`) |
| `PHAROS_FFMPEG_CMD` | Windows host: full ffmpeg arg override (`%IDX%`/`%FPS%` tokens) |
| `PHAROS_FFMPEG_DEBUG` | Windows host: raise ffmpeg loglevel to `info` |

**Panel / tools**
| var | effect |
|---|---|
| `PHAROS_PRESENT_MODE` | panel wgpu present mode (`novsync`/`vsync`/`mailbox`/`immediate`/`fifo`…) |
| `PHAROS_PANEL_THEME` / `_FONT` / `_LANG` / `_TAB` | panel theme / font scale / language / initial tab |
| `PHAROS_GYRO_PROFILE` | vid:pid for the standalone `gyro_mouse` example (default `057e:2009`) |
| `PHAROS_TEST_LOCALCONFIG` / `PHAROS_TEST_APPID` | opt-in Steam localconfig unit test |

**Trireme client**
| var | default | effect |
|---|---|---|
| `TRIREME_CODEC` | negotiated | decode codec override (`hevc` / `av1`) |
| `TRIREME_DECODER` | auto | force a specific ffmpeg decoder/hwaccel |
| `TRIREME_AV1_FORMAT` | `obu` | AV1 ffmpeg input format (`obu`/`av1`) |
| `TRIREME_HDR` | unset (SDR) | insert an HDR10→SDR tonemap filter (set only for HDR streams) |
| `TRIREME_FFMPEG_DEBUG` | unset | verbose client ffmpeg decode logs |
| `TRIREME_WGPU_BACKEND` | platform | `gl` / `vulkan` / `dx12` render backend |
| `TRIREME_CONFIG` / `TRIREME_CONFIG_DIR` | see probe order | sources.toml path / config dir |
| `TRIREME_PRESENT_IDENTITY` | unset | present identity so an enforcing host authorizes the `decode` probe |
| `TRIREME_RES_TARGET` / `_FLOOR` | 0 | one-shot resolution preference (coded height px) |
| `TRIREME_BITRATE_MIN` / `_MAX` | 0 | one-shot bitrate range (kbps; max 0 = uncapped) |
| `TRIREME_FPS_MIN` / `_MAX` | 0 | one-shot framerate range (fps; max 0 = auto) |
| `TRIREME_KMS_CONFIG` / `_IDENTITY` | see loader | `trireme-kms` config / identity paths |

**Standard OS vars consulted:** `HOSTNAME`, `COMPUTERNAME`, `HOME`,
`XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_DATA_DIRS`,
`WAYLAND_DISPLAY` (startup tripwire; the host refuses to bind its own socket
name), and the Windows `LOCALAPPDATA` / `ProgramFiles*` / `PATH`.

---

## Configuration files

Host config files live in `$PHAROS_CONFIG_DIR` (default `~/.config/pharos/`,
`%APPDATA%\pharos\` on Windows). All host config is **opt-in and non-fatal**: a
missing or malformed file yields safe defaults with every feature off; config
problems never stop the host streaming.

### config.toml: host config

```toml
[mqtt]                       # Home Assistant integration (off by default)
enabled = true
host = "198.51.100.4"        # broker address
port = 1883
username = "pharos"
password = "secret"
node_id = "living-room"      # default: hostname
discovery_prefix = "homeassistant"
publish_interval_secs = 10

[network]
bind_mac = "AA:BB:CC:DD:EE:FF"   # pin the QUIC listener to this NIC's IP

[encode]
max_pixels_per_sec = 497664000   # Σ(w·h)·fps ceiling (default = one 4K@60; placeholder)
```

### multi.toml: the live compositor layout

Passed on the CLI (`capture-zc-multi <config.toml>`) or loaded from the config
dir. One `[[output]]` per virtual monitor; each gets its own NVENC session + QUIC
stream. Written by `pharos-panel`. A sibling `video-codec` sidecar file overrides
the codec across a panel relaunch.

| key | type | default | meaning |
|---|---|---|---|
| `fps` | u32 (req) |  | shared frame rate (single pacer + NVENC rate) |
| `fps_auto` | bool | false | adaptive pacing between ~24 fps and `fps` |
| `frames` | u32 | 0 | frame budget (0 = until killed) |
| `video_codec` | `hevc`\|`av1` | `hevc` | codec for every output (AV1 needs Ada+) |
| `bitrate_kbps` | u32 | none | target average bitrate (else preset RC) |
| `max_bitrate_kbps` | u32 | none (=avg→CBR) | capped-VBR ceiling |
| `launch` | string | none | client launched into the shared socket |
| `mode` | string | none | round-trip tag: `mirror`/`extend`/`single` |
| `[[output]].name` | string (req) |  | output name |
| `[[output]].w` / `.h` | u32 (req) |  | mode width / height |
| `[[output]].x` / `.y` | i32 | 0 | virtual-desktop placement (overlap at 0,0 = mirror) |
| `[[output]].addr` | string (req) |  | `quic://host:port` to stream, else a file path |
| `[[output]].peer` | string | none | pinned peer pubkey (applies the input gate) |
| `[[output]].input_enabled` | bool | true | whether this peer's input reaches the seat |

```toml
fps = 60
video_codec = "hevc"

[[output]]
name = "left"
w = 1920
h = 1080
x = 0
y = 0
addr = "quic://0.0.0.0:15777"
```

### apps.toml: the launchable app/game catalog

An array of `[[app]]` tables shown in the panel and the Trireme grid. A synthetic
`desktop` entry is always present. Only `slug`/`name`/`command` are required.

| key | type | default | meaning |
|---|---|---|---|
| `slug` | string (req) |  | stable lowercase id (the `launch-app` key) |
| `name` | string (req) |  | display name |
| `kind` | `app`\|`game`\|`desktop`\|`media` | `app` | drives the default icon |
| `command` | string | "" | launch line (whitespace-tokenized unless `shell`) |
| `[app.launch]` | tagged table | none | structured launch: `steam-game`(`appid`) / `desktop-file`(`exec`) / `raw-command`(`line`) |
| `shell` | bool | false | run through `/bin/sh -c` (pipes, `steam://` handlers) |
| `cwd` | string | none | working directory |
| `env` | `[["K","V"]]` | [] | extra child env |
| `box_art` | string | none | icon/box-art image path |
| `target` | enum | `virtual-display` | `virtual-display` / `{virtual-output="name"}` (physical targets refuse to launch in v1) |
| `gyro_mode` | `enabled`\|`mouse`\|`disabled` | `enabled` | how gamepad gyro is treated while foreground |
| `perf_overlay` | bool | false | burn MangoHud overlay into the stream |
| `fps_cap` | `off`\|`auto`\|`{fixed=N}` | `off` | MangoHud fps cap (direct-spawn only) |

```toml
[[app]]
slug = "elden-ring"
name = "Elden Ring"
kind = "game"
command = "setsid -f steam steam://rungameid/1245620"
gyro_mode = "enabled"

[app.launch]
kind = "steam-game"
appid = 1245620
```

### peers.toml: the host's paired-clients registry

Re-read on every connection: only listed pubkeys may stream (unless
`PHAROS_ALLOW_UNPAIRED=1`). Written after each successful pairing.

```toml
[[peer]]
pubkey = "3a7f1c9e…deadbeef"   # client's 32-byte Ed25519 public key (hex)
alias = "Living Room TV"
paired_at = "2026-07-01T20:15:00Z"
```

### controller-profiles.toml: per-controller tuning

Pharos is the source of truth for gamepad feel (button swaps + gyro), keyed by the
canonical `vendor:product` id, so a pad gets its saved feel the instant it
connects, regardless of which client drives it. Also holds reusable named presets.

| key | type | default | meaning |
|---|---|---|---|
| `swap_ab` / `swap_xy` | bool | false | swap face buttons before injecting |
| `gyro.enabled` | bool | true | gyro→mouse master switch |
| `gyro.sens_x` / `sens_y` | f32 | 16.0 / 9.0 | sensitivity (px/degree) |
| `gyro.accel` | f32 | 2.5 | acceleration strength (0 = off) |
| `gyro.accel_pow` | f32 | 2.0 | attack-curve exponent |
| `gyro.accel_ref` | f32 | 110.0 | knee rotation speed (deg/s) |
| `gyro.accel_max` | f32 | 10.0 | clamp on extra gain |
| `gyro.invert_x` / `invert_y` | bool | false | flip direction |
| `gyro.axis_h` / `axis_v` | u8 | 2 (rz) / 1 (ry) | which raw gyro axis drives H / V |

```toml
[controllers."057e:2009"]
alias = "Switch Pro"
swap_ab = true

[controllers."057e:2009".gyro]
sens_x = 22.5
sens_y = 9.0
invert_y = true
axis_h = 0
```

### compositor-presets.toml: named layout library

Managed by `pharos-panel`. Each preset is a full `CompositorLayout` snapshot (fps +
every output tile's position/size/addr/peer + per-device `input_enabled`), so you
can save an arrangement (e.g. *couch mirror*, *dual extend*) and re-apply it.
Distinct from `multi.toml` (the one live layout the compositor reads).

### trireme-sources.toml: the client's multi-source grid

Each `[[source]]` is one Pharos stream (its own QUIC connection + decode), or a
native VNC/SPICE console. The grid lays sources left→right, top→bottom.

| key | type | default | meaning |
|---|---|---|---|
| `present` | enum | `low-latency` | window present mode (see [pacing](#input-capture--pacing)) |
| `layout.cols` / `rows` | u32 (req) |  | grid dims (cols×rows ≥ #sources) |
| `highlight_px` / `highlight_color` | u32 / hex | 0 / `#33FF66` | active-cell border |
| `[[source]].name` | string | none | cell label |
| `[[source]].addr` | `host:port` (req) |  | primary address (QUIC endpoint for Pharos) |
| `[[source]].protocol` | `pharos`\|`vnc`\|`spice` | `pharos` | source type (spice needs the `spice` feature) |
| `[[source]].password` | string | none | auth for vnc/spice |
| `[[source]].alt_addrs` | `[host:port]` | [] | failover addresses for the same host |
| `[[source]].pace` | enum | = `present` | per-source jitter/take-newest |
| `[[source]].width` / `height` | u32 | host size | render-size hints |
| `[[source]].host_pubkey` | hex | none (TOFU) | pinned host Ed25519 pubkey |

```toml
present = "low-latency"
layout = { cols = 2, rows = 1 }

[[source]]
name = "desktop"
addr = "192.0.2.100:15777"
alt_addrs = ["198.51.100.100:15777"]

[[source]]
name = "vm-console"
addr = "192.0.2.5:5900"
protocol = "vnc"
password = "secret"

[input]
mode = "focus"
forward_keyboard = true
forward_mouse = true
forward_scroll = true

[input.hotkeys]
toggle_capture = "Ctrl+Alt+End"
toggle_fullscreen = "F10"
disconnect = "Ctrl+Alt+Backspace"
```

---

## Desktop mirroring (experimental)

Pharos's primary model is the **headless session**: the compositor owns a virtual
display, and apps launch into it. Mirroring your **real** desktop session (the
KDE/GNOME desktop you are physically using) is a separate capture source, in two
stages of doneness:

**Today, via VNC:** the Trireme player's native VNC source works against any VNC
server, so running one on your desktop session gives an interactive (keyboard +
pointer) mirror right now; add a `protocol = "vnc"` source pointing at it (see
the sources config above). Expect VNC-tier latency and quality, not the HEVC
pipeline; on Plasma 6 Wayland note that KDE's built-in remote desktop speaks RDP,
so you need an actual VNC server (e.g. `krfb`) for this route.

**Scaffolded, the real path:** the `portal-mirror` compositor feature
(`compositor::portal_mirror`) implements the portal side of low-latency session
mirroring: one combined xdg-desktop-portal **RemoteDesktop + ScreenCast** session
(a single permission dialog, restore-token aware), and a dedicated PipeWire
thread delivering frames as either DMA-BUF planes or SHM bytes over a bounded
channel. Not yet wired: the EGLImage import into the existing GLES -> CUDA ->
NVENC path, DMA-BUF modifier negotiation, and the libei input bridge (the
session already hands over the EIS fd). Build and smoke-test it with:

```sh
# needs libpipewire-0.3 headers + clang at build time
cargo build -p compositor --features portal-mirror
pharos-compositor mirror     # negotiate, stream ~10 s, report what arrives
```

The smoke test opens the permission dialog on the local desktop, connects to the
first granted monitor, and prints the negotiated format plus per-frame kind
(dmabuf or shm) and size; it proves the portal and PipeWire legs on a given rig
without touching the encoder.

## Pairing & trust model

Pairing uses **SPAKE2** (a PAKE) over a dedicated QUIC listener (`pair`
subcommand, port 4432). The client proves knowledge of a short PIN, and both ends
exchange and **pin long-term Ed25519 identities**. Thereafter every connection is
authorized by a per-connection Ed25519 challenge against `peers.toml` on the host
(and the client can pin the host's pubkey, else trust-on-first-use).

Enforcement is **on by default**; `PHAROS_ALLOW_UNPAIRED=1` disables it (dev/test
only). The QUIC TLS certificate is a **throwaway opaque tunnel**, never a trust
anchor; the identity pinning above is the trust, not the cert. This is why the
control panel is a native app over the paired channel rather than a browser
console: no self-signed-cert wall.

## Input capture & pacing

**Input capture modes** (`[input].mode` in the client sources config):
- `focus` (default); forward when the window is focused *and* the pointer is in a
  source cell.
- `toggle`; off until the `toggle_capture` hotkey turns it on.
- `always`; forward whenever connected.
- `off`; view-only, never forward.

`local_escapes` chords (Ctrl+Alt+Delete, Ctrl+Shift+Escape, Alt+Tab, F11, …) always
stay local so you can reach your own desktop. Per-class gates
(`forward_keyboard` / `forward_mouse` / `forward_scroll`) drop whole input classes.

**Pace modes** (per-window `present` / per-source `pace`):
| mode | jitter depth | present | notes |
|---|---|---|---|
| `low-latency` | 1 | Immediate | default; newest frame |
| `balanced` | 3 | Mailbox | newest frame, less tearing |
| `smooth` | 6 | Fifo | presents oldest-first |
| `vrr` | 1 | Fifo | for variable-refresh displays |

**What input reaches the host today:** the desktop client sends keyboard, absolute
pointer (move/buttons/scroll); the Linux and Windows hosts also inject **relative**
pointer motion (`PointerRel`) for pointer-locked/FPS games. Android/TV clients are
gamepad-first (buttons, axes, gyro→mouse) and forward hardware-keyboard scancodes,
but do not send mouse pointer events. All client input is gated **host-side per
device**: it is silently discarded until that peer is input-enabled (via the
panel or `multi.toml`).
