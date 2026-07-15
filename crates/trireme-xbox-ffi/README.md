# trireme-xbox-ffi

C-ABI shim that exposes the Trireme client core to a C++/WinRT UWP shell on Xbox
Series X|S (Developer Mode). It is the Xbox analogue of the iOS staticlib: the
**shell** owns decode, present, audio, and input; the **core** owns transport,
pairing, and the wire protocol. The core never decodes or renders; it hands the
shell raw HEVC/AV1 access units and Opus packets and forwards the shell's
gamepad back to the host.

This crate is a thin adapter. It implements core-ffi's `FrameSink`/`AudioSink`
callback traits over C function pointers and delegates to the same
`start_video_stream` / `client_pair` every other client uses, so there is no
duplicated protocol logic.

## What has to compile for Xbox

Only the core-ffi dependency closure: `transport` (quinn + rustls-**ring**),
`core-crypto` (SPAKE2), `core-proto`, `core-usbip`, `tokio`. No media libraries.
`ring` was chosen over aws-lc precisely so the client cross-compiles cleanly, and
that pays off again here (BCrypt-backed RNG is in the UWP-allowed API set).

## Build (on Windows)

The target `x86_64-uwp-windows-msvc` is **tier 3**: no prebuilt std, so we
compile std with `-Z build-std` on nightly, and everything is x64 (which Xbox
requires anyway).

```powershell
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
# from repo root:
./rig/build-xbox.ps1
```

Output: `target/x86_64-uwp-windows-msvc/release/trireme_xbox.lib`.

## Header

```
cargo install cbindgen
cbindgen --config cbindgen.toml --crate trireme-xbox-ffi --output trireme_xbox.h
```

`build-xbox.ps1` runs this automatically when cbindgen is on PATH. Add both
`trireme_xbox.lib` and `trireme_xbox.h` to the C++/WinRT project (see
`client/xboxApp/README.md`).

## The C surface (see trireme_xbox.h)

| Function | Purpose |
|---|---|
| `trireme_init_logging` | route the core's `tracing` to the VS debugger output |
| `trireme_protocol_version` | Phase 0 "hello core" round-trip |
| `trireme_pair` | SPAKE2 pairing; writes the host pubkey hex, returns a status code |
| `trireme_start` | connect; frames/audio arrive via your callbacks; returns a handle |
| `trireme_stop` | stop + free the handle (exactly once) |
| `trireme_request_idr` | ask the host for a fresh keyframe on decoder corruption |
| `trireme_pad_*`, `trireme_key`, `trireme_pointer_*` | forward input to the host |

Callbacks fire from the QUIC thread; the shell must marshal to its render/audio
threads and must not block in them.

## Phase order (validate the gate first)

0. Link the lib into a stub UWP app; print `trireme_protocol_version()`.
1. **`trireme_pair` + `trireme_start`, log frame sizes, no decode.** This proves
   quinn's UDP survives the UWP sandbox. Do it before writing any decode code; if
   it fails, the whole native path is in doubt and the fallback is a browser
   (WebCodecs + WebTransport) client that needs no Dev Mode.
2. Media Foundation HEVC decode to a D3D11/12 SDR swapchain. First pixels.
3. `Windows.Gaming.Input` to `trireme_pad_*`. Playable.
4. Opus audio (bundle libopus for UWP).
5. HDR10 swapchain + AV1.
6. Native mDNS discovery (`Windows.Networking.ServiceDiscovery.Dnssd`), settings.

## Plan B

If `uniffi`/`core-ffi` will not build for the UWP target under build-std (the
`cli` feature drags clap in; the proc-macro scaffolding is the risk), drop the
`core-ffi` dependency and depend on `transport` + `core-crypto` + `core-proto` +
`tokio` directly, reimplementing the ~200-line stream glue from
`core-ffi::start_video_stream` here. The C surface above does not change.
