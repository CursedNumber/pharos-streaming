# xboxApp — Trireme client for Xbox Series X|S (UWP / Dev Mode)

A C++/WinRT UWP application that links `trireme-xbox-ffi` (`trireme_xbox.lib`)
and does everything platform-specific the shared Rust core does not: decode,
present, audio, input, discovery. This directory is the native shell; the Rust
core is at `crates/trireme-xbox-ffi`.

**This project builds only on Windows** with Visual Studio 2022 (UWP + C++
workloads). A Windows 11 VM is fine; no GPU passthrough is needed (the console
renders). Deployment is over the LAN to the console's Device Portal.

## Shell responsibilities and the Xbox API for each

| Concern | Xbox API | Fed by / drives |
|---|---|---|
| Video decode | Media Foundation HEVC/AV1 decoder MFT (DXVA) to D3D11 textures. Series X (RDNA2) has HW HEVC Main10 + AV1. | `on_frame` access units |
| Present | D3D12 (or D3D11) swapchain, FL 11.0, **Game** resource tier. HDR10 = `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` + `SetHDRMetaData`. Port the existing client tonemap as HLSL. | decoded textures |
| Audio | Bundle libopus (built for UWP); decode to PCM to WASAPI/AudioGraph. MF has no guaranteed Opus decoder on Xbox. | `on_packet` Opus packets |
| Input | `Windows.Gaming.Input.Gamepad`; map to canonical `core_proto::gamepad` indices. | `trireme_pad_*` |
| Discovery | `Windows.Networking.ServiceDiscovery.Dnssd` (native mDNS) to find advertised Pharos hosts. | `trireme_start` addr |

## Resource tier

Set the app to the **Game** tier in Dev Home (not the 1 GB app sandbox): 5 GB
foreground memory, full GPU, 6 cores, full D3D12 FL 11.0. A decode + present
pipeline fits inside 5 GB comfortably.

## Networking capability (the silent-failure gotcha)

UWP blocks sockets unless the manifest declares the capability. This is the UWP
equivalent of `ufw allow 15777/udp`: without it, `trireme_start` connects to
nothing and you will chase a phantom. In `Package.appxmanifest`:

```xml
<Capabilities>
  <!-- LAN peers (host on your local network) -->
  <Capability Name="privateNetworkClientServer" />
  <!-- WAN (DDNS failover); drop if you only ever stream on the LAN -->
  <Capability Name="internetClientServer" />
</Capabilities>
```

Also target **x64** (required for Xbox) and package as **MSIX**.

## Deploy

1. `./rig/build-xbox.ps1` (produces `trireme_xbox.lib` + `trireme_xbox.h`).
2. Reference both from this VS project; build x64 Release.
3. Boot the console into Developer Mode; note its Device Portal URL.
4. Deploy from Visual Studio (Remote Machine) or upload the MSIX in the Device
   Portal, then set the app to the Game tier and launch.

## Status

Scaffold only. Start at Phase 0/1 in `crates/trireme-xbox-ffi/README.md`: link
the lib, print the protocol version, then pair + stream and log frame sizes to
prove QUIC works under the sandbox before writing a line of decode code.
