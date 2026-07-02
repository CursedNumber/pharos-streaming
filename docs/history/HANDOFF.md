# Session Handoff — 2026-07-01 (HDR10 end-to-end validation + the controller-input saga)

Branch: **`main`**, committed. Workspace builds (`cargo build --release` + `gradle
:androidApp:assembleDebug` green; my changed files are `cargo fmt`-clean — a pre-existing
fmt drift lives in `app-launcher/src/launch.rs`, not touched here). Codenames: **Pharos** =
host, **Trireme** = client. Test device: Sony **Bravia X90J** (`192.0.2.122`, 4K60,
**32-bit / armeabi-v7a only**, Android TV, HDR10/HLG/DV capable, ~2000 nit).

This was a long live-validation session on a real Proton game (**Crimson Desert**, HDR-capable).
Two big threads: (1) prove the HDR10 pipeline end-to-end, (2) chase down why the controller
"didn't work" — which turned out to be nothing to do with the controller.

---

## TL;DR — what's true now

- ✅ **Host HDR10 pipeline is real and correct**: 10-bit HEVC Main10, BT.2020 primaries, PQ
  (ST-2084) transfer, Mastering-Display + Content-Light-Level SEI. Gated behind `PHAROS_HDR=1`.
- ✅ **Client HDR *output* works** — a genuine, unexpected win. The Bravia **enters HDR mode**
  off our stream on its own: `dumpsys SurfaceFlinger` showed the framebuffer in
  `dataspace=BT2020_ITU_PQ (298188800), hdr metadata types=3`, `HWC ... hdr10=true`, and the
  MTK decoder parsing HDR10+ info. So Trireme's Main10 decode + HDR presentation path functions;
  the TV recognizes the signal. (Set the TV to **Auto**, not forced-HDR — forcing it applies the
  panel's HDR curve to everything and washes the picture out; that fooled us for a while.)
- ✅ **Controller input works** (see the saga). The real bug was a host-side gate, not the pad.
- ✅ **SDR path is correct and playable** (native 8-bit; the user is on this now).
- ✅ **The `wp_color_manager_v1` handshake blocker is FIXED** (2026-07-01, commit `47a6102`).
  The proven root cause was an **incomplete `get_information`**: the display's image
  description advertised only `primaries_named` + `tf_named`, omitting the mandatory
  `primaries`/`luminances`/`target_luminance` block, so a real v2 client (Mesa Vulkan WSI /
  libplacebo) read the partial caps, abandoned the v2 manager, and spin-bound v1 ~30×/s. Now
  the full conformant info set + `done()` is delivered and the client accepts it (live-traced).
  ⏳ Remaining: **on-device end-to-end validation** (Bravia + Proton, render loop running) —
  the nested test rig can't drive a vulkan client to *present* (see caveat below).

---

## The HDR blocker — FIXED 2026-07-01 (`47a6102`)

**Original symptom:** with color-management ON, a real game (winewayland/DXVK on NVIDIA) **binds
`wp_color_manager_v1` ~15×/second forever**, queries the output's HDR caps, but **never** proceeds
to `create_windows_scrgb` / `set_image_description` — spin-binds and never presents (blank window).

**Proven root cause (live-traced with mpv gpu-next + a 5-agent conformance research pass):**
`get_information` on the display's output/preferred image description sent only `primaries_named`
+ `tf_named`, **omitting the mandatory `primaries` (chromaticity), `luminances`, and
`target_luminance` events**. A real v2 client (this is **Mesa's Vulkan WSI / libplacebo**, driven
by DXVK's HDR colorspace — not wine app code) reads that incomplete cap set, decides the color
manager is unusable, **drops from the v2 manager to a v1 manager** (where `windows_scrgb` isn't
even offered) and re-binds ~30×/s. **No protocol error on the wire — a purely semantic give-up.**
The manager `supported_*`+`done` burst and per-surface `ready2` were already correct; this was the
one gap. Trace: before, 1 v2 bind then 184 v1 binds in 6 s; after, the client gets the full info
set + `done()` and stays on v2.

**What was fixed (all in `47a6102`):**
- `color_mgmt.rs send_image_desc_info()` — emit the full conformant parametric info sequence:
  `primaries` (BT.2020/sRGB/P3 chromaticities ×1e6 + D65), `primaries_named`, `tf_named`,
  `luminances` (min ×10000; PQ = `0.005 / 10000 / 203`), `target_luminance`, `target_max_cll`,
  `target_max_fall`, then deferred `done()`. **This is the unblock.**
- `ImageDescData.allow_info` — `get_information` is now ALLOWED only on compositor-originated
  (output/preferred/reference) descriptions and FORBIDDEN (raises `no_information`) on
  client-authored ones (`create_windows_scrgb`, parametric `create`, ICC), per the XML.
- Advertise `Feature::WindowsScrgb` unconditionally (a v1 feature in our bundled XML) so a
  v1-fallback client still has a path to declare its scRGB HDR surface.
- `multi.rs` — flush the deferred `done()` in the **pre-client idle wait loop** too (a game can
  run its CM handshake before the first Trireme connects; a never-flushed `done()` strands it).
- `multi.rs log_top_surface_buffer_format()` — debug diagnostic logging the frontmost toplevel's
  committed buffer fourcc on change (the "is it really fp16?" ground truth for on-device).

**⚠️ Test-rig caveat (why local end-to-end present isn't validated):** the nested compositor can't
drive a **vulkan** client to create a swapchain here — `mpv --gpu-api=vulkan`, `gamescope`, and
`vkcube` all fail with `VK_ERROR_OUT_OF_HOST_MEMORY` / `vulkan_remake_swapchain` assert, and this
happens for **plain SDR too** (CM-independent nested-NVIDIA WSI limitation).
**[CORRECTED 2026-07-02 — this caveat is now OUTDATED.** Native SDR `vkcube --c 150` **CREATES + PRESENTS 150 frames cleanly nested on `wayland-1` (exit 0, no error)** — the nested compositor DOES back a native Vulkan swapchain. Native `WAYLAND_DISPLAY=wayland-1 vulkaninfo` also offers the full HDR set (R16G16B16A16_SFLOAT+EXTENDED_SRGB_LINEAR, A2B10G10R10+HDR10_ST2084, `VK_EXT_swapchain_colorspace` rev5). The real game-HDR blocker is that **Wine's Vulkan instance never gets `VK_EXT_swapchain_colorspace`** (`proton-cachyos 11.0`), so DXVK/vkd3d can't allocate an HDR swapchain (→ the same OUT_OF_HOST_MEMORY, for DX11/DXVK *and* D3D12/vkd3d). Evidence: `rig/hdr-traces-2026-07-01/dxvk-dx11-hdr-oom-2026-07-02.txt`; verdict + decision (defer Proton HDR, do it Windows-side) in memory `project-hdr-vrr-plan`.]**
So the CM *negotiation*
is validated (complete info + `done`, no v1 spin) but `create_windows_scrgb → set_image_description
→ fp16 present` must be confirmed **on-device** (Bravia + a real Proton game, Trireme connected so
the render loop runs).

**Note:** upstream `wp_color_manager_v1` is at v3 (adds `create_windows_bt2100`) but the XML
**bundled in our `wayland-protocols-0.32.12`** is **v2 only** — v3/`bt2100` is NOT available in
this tree, so the scRGB path (advertise v2 + `windows_scrgb`, convert scRGB→PQ in-shader) is the
correct and only path. Don't chase v3.

**Citations:** bundled `color-management-v1.xml` (v2); Mesa MR !31991 (Vulkan WSI color mgmt);
gamescope#1867; wayland.app/protocols/color-management-v1.

**Env flags** (color-management is still **OFF by default**; the handshake is fixed but the
scRGB→PQ present path is not yet on-device-validated):
- `PHAROS_HDR=1` — 10-bit Main10 HDR encode + BT.2020/PQ + SEI (host).
- `PHAROS_ENABLE_COLOR_MGMT=1` — advertise `wp_color_manager_v1`. The handshake now completes;
  expect a real game to declare an scRGB HDR surface.
- Validation diagnostic: add `--setenv=RUST_LOG=info,compositor::color=debug` to see the buffer
  format log (`Abgr16161616f` = true fp16 HDR content; 8-bit = SDR fallback).

**Reproduce the CM negotiation locally** (proves the fix; won't present, per the caveat):
```bash
# start an isolated CM-enabled host on a spare port, then:
WAYLAND_DISPLAY=<its socket> WAYLAND_DEBUG=1 mpv --vo=gpu-next --gpu-api=vulkan \
  --target-colorspace-hint=yes /tmp/hdr_test.mp4 2>&1 | grep image_description_info
# expect: primaries / luminances(50,10000,203) / target_luminance(50,1000) / done()
```

---

## The controller-input saga (root cause + the real follow-ups)

The user's controller "did nothing in-game." Half a session of chasing. The actual chain:

1. **Real client bug #1 (fixed, committed):** `AndroidManifest.xml` `MainActivity` was missing
   `keyboard|navigation` in `configChanges`. A game controller is a keyboard/navigation-class
   device, so toggling it on/off **destroyed + recreated the Activity** → stream black-framed +
   audio-dropped, and nulled `StreamHub`'s `ControllerManager`. Added `keyboard|navigation`.
2. **Real client bug #2 (fixed, committed):** `StreamHub.startAudioAndControllersLocked` only
   built the `ControllerManager` when `controllers == null`, so after a reconnect/re-dial that
   swapped `Session.gamepad` for a fresh handle, the manager kept sending into the **dead old
   handle** — `onKey`/`onMotion` returned true but nothing reached the host. Now tracks
   `controllersGamepad` and rebuilds on handle change (reference identity).
3. **THE root cause (NOT fixed in code — see follow-ups):** even after both fixes, input still
   didn't reach the game. Instrumenting the host proved the client forwards **perfectly** — the
   host **received every event and discarded it**: `HOST rx input GamepadButton{..}
   allowed=false`. The Bravia's pubkey was in `transport::INPUT_DISABLED`. Sending
   `SetInputEnabled … enabled:true` made the controller work instantly.

Also fixed: `rig/build-android.sh` was cross-compiling **arm64-v8a** for a **32-bit** X90J →
corrected to `armeabi-v7a` (an arm64 `.so` won't load there).

### Input-gate follow-ups (worth doing — this will bite again)
- **Dead config:** `[[output]] input_enabled = true` in `multi.toml` is **parsed and ignored** —
  nothing maps it to `transport::set_input_enabled`. Wire it: at startup, for each configured
  peer, call `set_input_enabled(peer_pubkey, input_enabled)`. Only caller today is
  `control.rs` (panel `SetInputEnabled`).
- **The disabler:** `pharos-panel` (spawned by `pharos-tray.service`) connects to the host
  control socket and **disables the Bravia's input on connect** — likely a device-tile that
  defaults to off (green = enabled). Fix the default / persist the enabled state, or a fresh
  host + panel running = the controller silently dead again.

---

## How to reproduce the live test rig

The validation host runs as a transient `--user` unit (the real `pharos-host.service` is
separate — don't confuse them). Config `/tmp/multi-hdr-val.toml` is pinned to the Bravia's
native **4K** so there's no resolution-grow restart (a grow-restart drops the game — it loses
its Wayland socket).

```bash
# Native SDR (what the user is on now — correct colors, playable):
systemd-run --user --unit=pharos-hdr-val --collect \
  --property=Restart=on-failure --property=RestartSec=1 --property=StartLimitIntervalSec=0 \
  --setenv=RUST_LOG=info,compositor::color=warn,compositor::input=warn \
  --setenv=XDG_RUNTIME_DIR=/run/user/1000 \
  --setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  /home/user/KVM/target/release/pharos-compositor capture-zc-multi /tmp/multi-hdr-val.toml
# HDR encode:              add  --setenv=PHAROS_HDR=1
# Work on the CM handshake: add --setenv=PHAROS_ENABLE_COLOR_MGMT=1
```

Notes:
- `Restart=on-failure` is REQUIRED — the compositor deliberately `exit(3)`s to grow the canvas
  to a client's panel res and relies on a supervisor to bring it back. Without it it just dies
  (looked like a client "reconnect loop" for a while).
- **Enable the Bravia's input** (host discards it otherwise — see saga). Control socket is
  newline-JSON at `$XDG_RUNTIME_DIR/pharos/control.sock`:
  ```bash
  python3 -c "import socket;s=socket.socket(socket.AF_UNIX);s.connect('/run/user/1000/pharos/control.sock');s.sendall(b'{\"kind\":\"SetInputEnabled\",\"pubkey\":\"<peer-hex>\",\"enabled\":true}\n');print(s.recv(4096))"
  ```
  Bravia peer hex: `<bravia-peer-pubkey>`.
- adb: `adb connect 192.0.2.122:5555` (authorized). App id `dev.pharos.trireme.debug`.
  Rebuild+install: `rig/build-android.sh` (now armeabi-v7a) then
  `adb install -r client/androidApp/build/outputs/apk/debug/androidApp-debug.apk`. Kotlin-only
  changes: just `gradle :androidApp:assembleDebug`. TV HDR state:
  `adb shell dumpsys SurfaceFlinger | grep -iE "dataspace|hdr"`.

---

## Commits this session

- **compositor: HDR10 compositing + wp_color_manager_v1 (default-off, incomplete handshake)** —
  fp16 scratch + scRGB→PQ / sRGB→PQ post-pass, `color_mgmt` `ready`/`ready2` version gate +
  deferred-`done` crash fix, CM global gated OFF by default (`PHAROS_ENABLE_COLOR_MGMT` opt-in).
- **trireme: controller input survives reconnect/hotplug + 32-bit ABI build** — Activity
  `configChanges keyboard|navigation`, `ControllerManager` rebinds to the live gamepad handle,
  `build-android.sh` → `armeabi-v7a`.

## Suggested next-session order
1. **On-device HDR validation** (the handshake is fixed; this is the last mile): run the host
   with `PHAROS_HDR=1 PHAROS_ENABLE_COLOR_MGMT=1` + `compositor::color=debug`, Trireme on the
   Bravia (Auto mode), launch Crimson Desert with `PROTON_ENABLE_WAYLAND=1 DXVK_HDR=1`. Confirm
   in the log: the game does `create_windows_scrgb` → `set_image_description`, the buffer-format
   diagnostic reports **`Abgr16161616f`** (true fp16), and the picture is real HDR (not the
   washed-out grayscale from HDR-shading an SDR frame). If it still stalls, grab a
   `WAYLAND_DEBUG` trace of the game and diff against the mpv trace recipe above.
2. **Wire `input_enabled` from the toml** + fix the panel's default-disable (unchanged from below).
3. Conformance-hardening follow-ups (won't block HDR, from the research audit): reject
   unadvertised render-intents; validate `set_luminances`/`create` (`invalid_luminance`,
   `max_fall ≤ max_cll`); output `image_description_changed` lifecycle. Optional: make the
   scRGB→PQ post-pass **transfer-aware** (PQ-passthrough for already-PQ content like HDR video),
   so mpv/other non-scRGB HDR clients render correctly too — today only the scRGB path is right.
