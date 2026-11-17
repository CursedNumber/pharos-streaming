# Pharos HDR — Wayland Color-Management (PQ preserve for real games)

Status: **design locked, rig-verified 2026-07-01** (5-agent research, session `47c7e518`).
Prereq DONE: the host 10-bit HDR10 **encode** path is live-proven (`Abgr2101010` → EGL/CUDA →
NVENC `ABGR10` Main10 + BT.2020/PQ VUI + MDCV/CLL SEI; commits `a061814`, `1030b0c`, behind
`PHAROS_HDR=1`). This doc covers the remaining blocker: making a real Proton/Wine game's HDR
survive the compositor instead of being flattened to 8-bit sRGB before capture.

## The one fact that determines everything (verified on THIS rig)

`proton-cachyos-11.0-20260602` `winewayland.so` binds the **ratified `wp_color_manager_v1` (v2)**
and hands the compositor a **Windows-scRGB** surface: **FP16** (`R16G16B16A16_FLOAT` →
DRM `ABGR16161616F`), **BT.709 primaries, extended-linear transfer, 1.0 = 80 nits, 125.0 = 10000
nits**, via the `create_windows_scrgb` request (`feature.windows_scrgb` = 7).

The PQ/BT.2020 path (`create_windows_bt2100`) exists in the driver but is **gated on protocol
version 3**, which no shipping compositor (KDE included, it advertises v2) exposes — so games use
scRGB. **Consequence: we advertise v2 + `windows_scrgb`, accept scRGB, and do the scRGB→PQ/BT.2020
conversion ourselves in a GLES shader.** (evidence: `strings winewayland.so`; `wayland-info` on
the dev host shows KWin at `wp_color_manager_v1 v2`.)

Enable for testing: `PROTON_ENABLE_WAYLAND=1 DXVK_HDR=1` (ENABLE_HDR_WSI *not* needed on driver
610). Test clients already installed (no game needed): **mpv 0.41 gpu-next** (cleanest first test),
**gamescope --hdr-enabled**, vkcube.

## What's already in the tree (no work / no Cargo changes)

- **Server bindings compiled in-tree**: `smithay::reexports::wayland_protocols::wp::color_management::v1::server::*` (Smithay enables wayland-protocols `staging` via `wayland_frontend`). Interfaces: `wp_color_manager_v1`, `wp_color_management_surface_v1`, `wp_color_management_surface_feedback_v1`, `wp_color_management_output_v1`, `wp_image_description_creator_params_v1`, `wp_image_description_creator_icc_v1`, `wp_image_description_v1`, `wp_image_description_info_v1`. Types are PascalCase (`WpColorManagerV1`), each with a `Request` enum + named enums (`Feature`/`RenderIntent`/`Primaries`/`TransferFunction`).
- **GlesRenderer can do the compositing** (all verified in `smithay-0.7.0/src/backend/renderer/gles/`): `import_dmabuf` accepts any driver-advertised fourcc (`ABGR16161616F`→RGBA16F, `Abgr2101010`→RGB10_A2, `format.rs:22-23`); 10-bit + FP16 render targets work (Cap `_10Bit` auto-on for GLES≥3.0; 4070 is 3.2); the **default shader does NO sRGB/EOTF math** (`shaders/implicit/texture.frag`, mediump passthrough, premultiplied-alpha blend) — so there is no unwanted conversion to defeat, only precision + the scRGB→PQ math to add.
- **Custom-shader hook**: `compile_custom_texture_shader(src, uniforms)` → `GlesTexProgram` (lets us set our own `#version`/`precision highp`), wrapped in `TextureShaderElement` (`gles/element.rs:135`) whose `draw` calls `render_texture_from_to(..., Some(program), ...)`. Per-surface injection, clean.
- **Templates to mirror**: `smithay-0.7.0/src/wayland/alpha_modifier/{mod,dispatch}.rs` (manager global + per-surface double-buffered `Cacheable` state — exact structural twin) and `content_type/dispatch.rs` (`WEnum::Value` unwrap + `cached_state.pending()`).
- **Reference impls**: Smithay PR #1143 (branch `hdr_test`) = full handler skeleton BUT written against a newer CICP draft (`set_tf_cicp`) that does **not** match our local `set_tf_named` enums, and MISSING `create_windows_scrgb` — adapt, don't copy. gamescope `src/shaders/colorimetry.h` = drop-in scRGB→PQ math (BSD).

## The shader math (the one piece of new color code)

scRGB FP16 (BT.709 linear, 1.0=80 nits) → PQ/BT.2020 10-bit, at `highp`:
1. sample FP16 texel = BT.709 linear light
2. × 80 → absolute nits (BT.709)
3. BT.709→BT.2020 primary matrix (3×3)
4. `nitsToPq()` (ST.2084 OETF) → [0,1]
5. write to `Abgr2101010`

PQ: `c1=0.8359375 c2=18.8515625 c3=18.6875 m1=0.1593017578125 m2=78.84375`;
`nitsToPq(n)=pow((c1+c2*pow(n/10000,m1))/(1+c3*pow(n/10000,m1)), m2)`.
For a single fullscreen surface (no blend), write straight into the 10-bit target — **no fp16
intermediate needed** (PQ is perceptually uniform, 10-bit PQ doesn't band; this is why KWin moved
to 10-bit encoded blending). Precision `highp` is load-bearing (mediump bands highlights).
⚠️ NVIDIA HDR dmabufs often import as **EXTERNAL** (`samplerExternalOES`) — the custom shader must
carry the PQ math in the EXTERNAL `//_DEFINES_` variant too, or HDR surfaces silently mis-render.

## Staged build

**Stage A — protocol server (`crates/compositor/src/color_mgmt.rs`).** Hand-rolled
GlobalDispatch/Dispatch on `State`, mirroring `alpha_modifier`. Advertise on bind: intent
`Perceptual`; features `Parametric` + `WindowsScrgb` (+ `SetLuminances`/`SetMasteringDisplayPrimaries`
if consumed); tf `St2084Pq` (+ `Srgb`/`Bt709` for SDR); primaries `Bt2020` + `Srgb`; then `done`.
Handle: `get_surface`, `create_windows_scrgb` (→ immediate `ready2`), `create_parametric_creator`
(setters → `create` → `ready2`), `set_image_description` (double-buffered `ColorSurfaceCachedState`
via `Cacheable`), `unset`, `get_surface_feedback` (advertise the virtual output as PQ/BT.2020 so
the game turns HDR on). Register global next to viewporter (`multi.rs:~1194` + `lib.rs:816` single
path). **Tracer-bullet first: log every request** → run mpv/game with `WAYLAND_DEBUG=1` to confirm
exactly which requests + buffer format land (resolves the last empirical unknowns).

**Stage B — HDR compositing shader.** `compile_custom_texture_shader` (scRGB→PQ, all 3 variants incl.
EXTERNAL, highp), cached in the `Pipeline`. In the render loop, for the fullscreen surface whose
committed color state is HDR, render via a hand-built `TextureShaderElement` with the PQ program
instead of the stock `WaylandSurfaceRenderElement` (which forces program=None). Everything else
stays on the default path.

**Stage C — drive capture from the surface signal.** Replace the `PHAROS_HDR` env gate with the
per-surface color state: fullscreen surface carries an HDR image description ⇒ 10-bit target +
Main10 encode. Optionally source MDCV/CLL from the game's declared `max_cll`/`max_fall`.

**Stage D — validate.** mpv first (isolates server bugs), then gamescope nested, then a real Proton
game (`PROTON_ENABLE_WAYLAND=1 DXVK_HDR=1`), each with a `WAYLAND_DEBUG` trace + a PQ-gradient
eyeball. Do NOT call it "working" until a real HDR game passes the fake-HDR check un-flattened.

## Open questions the Stage-A tracer bullet resolves
- Does winewayland use `create_windows_scrgb` or the parametric creator on this build? (advertise both.)
- Exact dmabuf fourcc (`ABGR16161616F` inferred) + whether it imports as EXTERNAL vs 2D.
- Whether the game needs `get_surface_feedback` to return a real HDR output description before it enables HDR at all.
