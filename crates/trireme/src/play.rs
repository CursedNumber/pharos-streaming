// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Windowed multi-source player.
//!
//! Each "source" is one Pharos stream (its own QUIC connection, its own NVDEC
//! decode pipeline, its own GPU texture). The window is laid out as a
//! `cols × rows` grid; each cell shows one source, letterboxed to preserve its
//! aspect ratio. This is what makes multi-monitor (one Pharos with N virtual
//! outputs on N ports) and multi-host (N different Pharos boxes) work the same
//! way — the client just sees N independent streams.
//!
//! Pipeline (per source): QUIC (`transport`) → ffmpeg HW decode (NVDEC/Quick
//! Sync) → rawvideo **NV12** on a pipe → reader thread → bounded frame queue
//! (drop-old per source pace) → winit/wgpu present (one swapchain, YUV→RGB in
//! the fragment shader, viewport-per-cell). NV12 (not RGBA) keeps the CPU out
//! of color conversion — swscale rgba was ~half the decode cost — and moves 3
//! bytes/px instead of 8 through the pipe + GPU upload.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

/// Default per-source resolution if the config doesn't specify one. This is only
/// a pre-geometry fallback: the reader re-sizes and respawns the decode chain
/// from the host's `DG_STREAM_GEOMETRY` announce (the resolution ladder), so the
/// host is not fixed-size.
pub const DEFAULT_WIDTH: u32 = 1920;
pub const DEFAULT_HEIGHT: u32 = 1080;

#[derive(Clone, Copy, Debug)]
pub enum PaceMode {
    LowLatency,
    Balanced,
    Smooth,
    Vrr,
}

impl PaceMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "low" | "low-latency" | "lowlatency" => PaceMode::LowLatency,
            "balanced" => PaceMode::Balanced,
            "smooth" => PaceMode::Smooth,
            "vrr" => PaceMode::Vrr,
            _ => PaceMode::LowLatency,
        }
    }
    /// Jitter-buffer depth.
    fn depth(self) -> usize {
        match self {
            PaceMode::LowLatency | PaceMode::Vrr => 1,
            PaceMode::Balanced => 3,
            PaceMode::Smooth => 6,
        }
    }
    /// Swapchain present mode.
    fn present_mode(self) -> wgpu::PresentMode {
        match self {
            PaceMode::LowLatency => wgpu::PresentMode::Immediate,
            PaceMode::Balanced => wgpu::PresentMode::Mailbox,
            PaceMode::Smooth | PaceMode::Vrr => wgpu::PresentMode::Fifo,
        }
    }
    /// SMOOTH presents oldest-first; the others take the newest available.
    fn in_order(self) -> bool {
        matches!(self, PaceMode::Smooth)
    }
}

// --- Config -----------------------------------------------------------------

/// `sources.toml` schema. Example:
/// ```toml
/// present = "low-latency"     # swapchain present mode (window-level)
/// layout  = { cols = 2, rows = 1 }
///
/// [[source]]
/// name = "desktop"
/// addr = "192.168.1.100:15777"
/// pace = "low-latency"
///
/// [[source]]
/// name = "console"
/// addr = "192.168.1.101:15777"
/// pace = "vrr"
/// width  = 1920
/// height = 1080
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct PlayerCfg {
    #[serde(default)]
    pub present: Option<String>,
    pub layout: LayoutCfg,
    #[serde(rename = "source")]
    pub sources: Vec<SourceCfg>,
    /// `[input]` — capture mode, local-escape chords, hotkeys, class gates.
    /// Defaults to `CaptureMode::Focus` with the standard escape list (see
    /// `crate::input::InputCfg::default`) so existing configs keep working.
    #[serde(default)]
    pub input: crate::input::InputCfg,
    /// Border thickness, in pixels, drawn around the cell currently receiving
    /// input (the one the pointer is over) for clarity in a multi-cell grid.
    /// `0` disables it. Default `0` (off).
    #[serde(default = "default_highlight_px")]
    pub highlight_px: u32,
    /// Colour of that border, as a hex code (`#RGB` or `#RRGGBB`, leading `#`
    /// optional). Default a green (`#33FF66`).
    #[serde(default = "default_highlight_color")]
    pub highlight_color: String,
}

fn default_highlight_px() -> u32 {
    // Off by default. The active-cell border only makes sense for a multi-source
    // grid; it's distracting around a single stream and is always suppressed there
    // (see `active_highlight`). Opt in per-grid via `highlight_px` in sources.toml.
    0
}

fn default_highlight_color() -> String {
    "#33FF66".to_string()
}

/// Parse a `#RGB` / `#RRGGBB` hex colour (leading `#` optional) into sRGB
/// `[r, g, b]` in 0..=1. Falls back to the default green on anything malformed.
fn parse_hex_color(s: &str) -> [f32; 3] {
    let h = s.trim().strip_prefix('#').unwrap_or_else(|| s.trim());
    let parse = |hex: &str| u8::from_str_radix(hex, 16).ok();
    let rgb = match h.len() {
        // #RGB shorthand → each nibble doubled (f → ff).
        3 => {
            let c: Vec<char> = h.chars().collect();
            (
                parse(&format!("{0}{0}", c[0])),
                parse(&format!("{0}{0}", c[1])),
                parse(&format!("{0}{0}", c[2])),
            )
        }
        6 => (parse(&h[0..2]), parse(&h[2..4]), parse(&h[4..6])),
        _ => (None, None, None),
    };
    match rgb {
        (Some(r), Some(g), Some(b)) => [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0],
        _ => [0.2, 1.0, 0.4], // default green
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutCfg {
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceCfg {
    #[serde(default)]
    pub name: Option<String>,
    pub addr: SocketAddr,
    /// Source protocol: omitted / `"pharos"` = a Pharos QUIC/HEVC stream (the
    /// default); `"vnc"` = a native VNC server (e.g. a Proxmox VM console);
    /// `"spice"` = a native SPICE server (the usual QEMU/Proxmox console
    /// protocol). VNC/SPICE are decoded directly and bridged into the same
    /// render path. `"spice"` requires a build with the `spice` feature.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Auth password for `protocol = "vnc"`/`"spice"` servers that require one.
    #[serde(default)]
    pub password: Option<String>,
    /// Additional addresses for the *same* host (e.g. a second NIC). On a dropped
    /// connection the player reconnects, rotating through `[addr] + alt_addrs`, so
    /// a host-NIC failure is transient — the client just comes back on another
    /// path. Empty by default; no assumptions about the host's topology.
    #[serde(default)]
    pub alt_addrs: Vec<SocketAddr>,
    #[serde(default)]
    pub pace: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Hex-encoded 32-byte Ed25519 pubkey expected from the host. Populated
    /// from the registry when the menu launches the player. `None` ⇒ TOFU
    /// (signature is still verified, but any returned pubkey is accepted).
    #[serde(default)]
    pub host_pubkey: Option<String>,
}

impl PlayerCfg {
    /// One source on one address, 1×1 grid — used by the legacy CLI form
    /// (`trireme play <host:port> [pace]`).
    pub fn single(addr: SocketAddr, pace: PaceMode) -> Self {
        let pace_s = pace_str(pace);
        Self {
            present: Some(pace_s.into()),
            layout: LayoutCfg { cols: 1, rows: 1 },
            sources: vec![SourceCfg {
                name: None,
                addr,
                protocol: None,
                password: None,
                alt_addrs: Vec::new(),
                pace: Some(pace_s.into()),
                width: None,
                height: None,
                host_pubkey: None,
            }],
            input: crate::input::InputCfg::default(),
            highlight_px: default_highlight_px(),
            highlight_color: default_highlight_color(),
        }
    }

    fn present_pace(&self) -> PaceMode {
        self.present
            .as_deref()
            .map(PaceMode::parse)
            .unwrap_or(PaceMode::LowLatency)
    }

    fn validate(&self) -> Result<()> {
        let cells = (self.layout.cols as usize) * (self.layout.rows as usize);
        anyhow::ensure!(cells >= 1, "layout cols × rows must be ≥ 1");
        anyhow::ensure!(!self.sources.is_empty(), "no sources configured");
        anyhow::ensure!(
            self.sources.len() <= cells,
            "{} sources won't fit in a {}×{} grid",
            self.sources.len(),
            self.layout.cols,
            self.layout.rows,
        );
        Ok(())
    }
}

fn pace_str(p: PaceMode) -> &'static str {
    match p {
        PaceMode::LowLatency => "low-latency",
        PaceMode::Balanced => "balanced",
        PaceMode::Smooth => "smooth",
        PaceMode::Vrr => "vrr",
    }
}

/// Probe order: `$TRIREME_CONFIG`, `$XDG_CONFIG_HOME/trireme/sources.toml`,
/// `$HOME/.config/trireme/sources.toml`, `./trireme-sources.toml`. Returns
/// `Ok(None)` if no file is found so the caller can fall back to a default.
pub fn load_default_config() -> Result<Option<PlayerCfg>> {
    let mut paths: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("TRIREME_CONFIG") {
        paths.push(p);
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        paths.push(format!("{x}/trireme/sources.toml"));
    }
    if let Ok(h) = std::env::var("HOME") {
        paths.push(format!("{h}/.config/trireme/sources.toml"));
    }
    paths.push("trireme-sources.toml".into());
    for p in &paths {
        if let Ok(text) = std::fs::read_to_string(p) {
            let cfg: PlayerCfg = toml::from_str(&text).with_context(|| format!("parse {p}"))?;
            cfg.validate()?;
            tracing::info!(
                "loaded sources config {p}: {} source(s), {}×{} grid",
                cfg.sources.len(),
                cfg.layout.cols,
                cfg.layout.rows
            );
            return Ok(Some(cfg));
        }
    }
    Ok(None)
}

pub fn load_config_path(path: &Path) -> Result<PlayerCfg> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: PlayerCfg =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    cfg.validate()?;
    tracing::info!(
        "loaded sources config {}: {} source(s), {}×{} grid",
        path.display(),
        cfg.sources.len(),
        cfg.layout.cols,
        cfg.layout.rows
    );
    Ok(cfg)
}

// --- Frame queue ------------------------------------------------------------

/// One decoded NV12 (or, for the GPU-tonemap HDR path, P010) frame plus the
/// resolution it was produced at. Native sources (VNC/SPICE) can change
/// resolution mid-stream, so each frame carries its own size and the GPU
/// resizes its textures to match.
struct Frame {
    data: Vec<u8>,
    w: u32,
    h: u32,
    /// `true` = 10-bit P010 BT.2020/PQ from a [`DecodeChain::TonemapGpu`] chain
    /// (16-bit samples, tonemapped in the fragment shader); `false` = 8-bit
    /// BT.709 NV12. The GPU re-creates a source's plane textures when this
    /// flips, exactly as it does on a resolution change.
    hdr: bool,
    /// Instant this decoded frame became available (pushed onto the queue). The
    /// render path reads `produced.elapsed()` when the frame is actually
    /// presented to feed the live latency benchmark's present stage. Cheap to
    /// carry; ignored entirely when no benchmark is running.
    produced: Instant,
}

struct FrameQueue {
    inner: Mutex<VecDeque<Frame>>,
    depth: usize,
}

impl FrameQueue {
    fn push(&self, f: Frame) {
        let mut q = self.inner.lock().unwrap();
        q.push_back(f);
        while q.len() > self.depth {
            q.pop_front();
        }
    }
    /// `latest=true`: take newest and drop any backlog. `false`: take oldest.
    fn take(&self, latest: bool) -> Option<Frame> {
        let mut q = self.inner.lock().unwrap();
        if latest {
            let f = q.pop_back();
            q.clear();
            f
        } else {
            q.pop_front()
        }
    }
}

// --- GPU --------------------------------------------------------------------

/// Per-source GPU state — Y (full-res) + CbCr (half-res) planes and a bind
/// group, sampled + color-converted by the shared pipeline. SDR sources are
/// 8-bit NV12 (R8 + Rg8); HDR sources on the GPU-tonemap path are 16-bit P010
/// (R16 + Rg16, sampled by the HDR pipeline instead). The planes are re-created
/// when a source changes resolution OR colorimetry (see `Gpu::upload`).
struct SourceGpu {
    width: u32,
    height: u32,
    /// Whether the planes are 16-bit P010 (HDR) or 8-bit NV12 (SDR).
    hdr: bool,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

impl SourceGpu {
    /// Allocate the plane textures (+ bind group) for a `w × h` source and seed
    /// them with neutral black (limited-range black Y, centered chroma) so the
    /// cell is black, not green, until the first frame arrives. `w`/`h` are
    /// forced even (4:2:0 subsampling).
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        w: u32,
        h: u32,
        hdr: bool,
    ) -> SourceGpu {
        let (w, h) = (w.max(2) & !1, h.max(2) & !1);
        let (y_fmt, uv_fmt) = if hdr {
            (
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
            )
        } else {
            (wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm)
        };
        let tex = |label, fw, fh, fmt| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: fw,
                    height: fh,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y_texture = tex("source-y", w, h, y_fmt);
        let uv_texture = tex("source-uv", w / 2, h / 2, uv_fmt);
        // Per-texel seed patterns. SDR NV12: Y=16, Cb=Cr=128. HDR P010 stores
        // 10-bit codes left-justified in 16 bits (LE), so black Y = 64<<6 =
        // 0x1000 and centered chroma = 512<<6 = 0x8000.
        let (y_texel, uv_texel): (&[u8], &[u8]) = if hdr {
            (&[0x00, 0x10], &[0x00, 0x80, 0x00, 0x80])
        } else {
            (&[16], &[128, 128])
        };
        let write = |t: &wgpu::Texture, texel: &[u8], fw: u32, fh: u32| {
            let bpr = fw * texel.len() as u32;
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: t,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &texel.repeat((fw * fh) as usize),
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(fh),
                },
                wgpu::Extent3d {
                    width: fw,
                    height: fh,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&y_texture, y_texel, w, h);
        write(&uv_texture, uv_texel, w / 2, h / 2);
        let y_view = y_texture.create_view(&Default::default());
        let uv_view = uv_texture.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        SourceGpu {
            width: w,
            height: h,
            hdr,
            y_texture,
            uv_texture,
            bind_group,
        }
    }
}

/// Whether the GPU present path can tonemap HDR itself (16-bit P010 plane
/// textures + the PQ/hable fragment shader). Set once by [`Gpu::new`] from the
/// device's actual `TEXTURE_FORMAT_16BIT_NORM` support — which `resumed()`
/// runs BEFORE any decode pipeline spawns, so the decode chain choice always
/// sees the real capability. Unset (pre-GPU) reads as `false`, i.e. the safe
/// CPU-zscale fallback.
static GPU_TONEMAP_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// `TRIREME_GPU_TONEMAP=0` forces the CPU zscale tonemap even on a capable GPU
/// — the escape hatch if the shader's color math ever disagrees with the
/// reference zscale output on some driver.
fn gpu_tonemap_env_off() -> bool {
    std::env::var("TRIREME_GPU_TONEMAP").is_ok_and(|v| v == "0")
}

/// Which tonemap chain an HDR-confirmed stream should get on this machine:
/// GPU (P010 passthrough, shader tonemap) when the device supports 16-bit
/// normalized textures, else the CPU zscale chain (the GLES/ARM path — R16
/// sampled textures aren't guaranteed on Panfrost/V3D).
fn tonemap_chain() -> DecodeChain {
    if !gpu_tonemap_env_off() && *GPU_TONEMAP_OK.get().unwrap_or(&false) {
        DecodeChain::TonemapGpu
    } else {
        DecodeChain::TonemapCpu
    }
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    /// HDR variant of `pipeline`: samples 16-bit P010 planes and runs the
    /// BT.2020/PQ → hable → BT.709 tonemap in the fragment shader (issue #15).
    /// `None` when the device lacks `TEXTURE_FORMAT_16BIT_NORM` (GLES/ARM) —
    /// those clients keep the CPU zscale chain and never produce HDR frames.
    hdr_pipeline: Option<wgpu::RenderPipeline>,
    sources: Vec<SourceGpu>,
    /// Kept so `upload` can re-create a source's plane textures when it changes
    /// resolution (a native VNC/SPICE guest switching video mode).
    sampler: wgpu::Sampler,
    bgl: wgpu::BindGroupLayout,
    /// Alpha-blended overlay that draws the active-cell border. Its uniform is
    /// rewritten each present.
    glow_pipeline: wgpu::RenderPipeline,
    glow_buf: wgpu::Buffer,
    glow_bg: wgpu::BindGroup,
}

/// Active-cell border overlay. A fullscreen triangle is clipped to the cell
/// viewport; the fragment draws a crisp `thickness`-px inset border (1px of
/// anti-aliasing on the inner edge) in the configured colour.
const GLOW_SHADER: &str = r#"
struct Glow {
    rect:   vec4<f32>,  // x, y, w, h  (framebuffer pixels)
    color:  vec4<f32>,  // sRGB rgb + alpha
    params: vec4<f32>,  // x = border thickness in px; yzw unused
};
@group(0) @binding(0) var<uniform> g: Glow;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(p[i], 0.0, 1.0);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return select(lo, hi, c > vec3(0.04045));
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let left   = pos.x - g.rect.x;
    let right  = (g.rect.x + g.rect.z) - pos.x;
    let top    = pos.y - g.rect.y;
    let bottom = (g.rect.y + g.rect.w) - pos.y;
    let d = min(min(left, right), min(top, bottom)); // px to nearest cell edge
    // Solid out to `thickness`, then a 1px linear fade for anti-aliasing.
    let a = clamp(g.params.x - d + 0.5, 0.0, 1.0) * g.color.a;
    // The swapchain is sRGB and re-encodes on store; decode here so the hex
    // colour reaches the framebuffer unchanged.
    return vec4(srgb_to_linear(g.color.rgb), a);
}
"#;

/// Choose the wgpu backend set. Windows = DX12/Vulkan; x86 Linux = Vulkan with a
/// GL fallback (Pharos rig prefers Vulkan, kept GL for Intel/old GPUs). On
/// aarch64 (ARM SBCs) we default to **GLES**: the Raspberry Pi 5's V3DV Vulkan is
/// Khronos-conformant but lacks `FULL_DRAW_INDEX_UINT32`, which makes wgpu panic
/// when it requests a device, and Mali's open Vulkan (PanVK) is still young — so
/// GLES (V3D / Panfrost) is the trodden path, and it's what `trireme-kms` drives
/// by hand. Override with `TRIREME_WGPU_BACKEND=gl|vulkan|dx12`.
fn select_backends() -> wgpu::Backends {
    if let Some(v) = std::env::var_os("TRIREME_WGPU_BACKEND") {
        match v.to_string_lossy().to_ascii_lowercase().as_str() {
            "gl" | "gles" | "opengl" => return wgpu::Backends::GL,
            "vulkan" | "vk" => return wgpu::Backends::VULKAN,
            "dx12" | "d3d12" => return wgpu::Backends::DX12,
            other => tracing::warn!("ignoring unknown TRIREME_WGPU_BACKEND='{other}'"),
        }
    }
    if cfg!(target_os = "windows") {
        wgpu::Backends::DX12 | wgpu::Backends::VULKAN
    } else if cfg!(target_arch = "aarch64") {
        wgpu::Backends::GL
    } else {
        wgpu::Backends::VULKAN | wgpu::Backends::GL
    }
}

impl Gpu {
    async fn new(window: Arc<Window>, present: PaceMode, sources: &[Source]) -> Result<Self> {
        let size = window.inner_size();
        let backends = select_backends();
        tracing::info!("wgpu backends: {backends:?}");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let surface = instance.create_surface(window).context("create_surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("no suitable GPU adapter")?;
        tracing::info!("wgpu adapter: {}", adapter.get_info().name);
        // 16-bit normalized textures carry the P010 planes of the GPU HDR
        // tonemap (issue #15). Optional in wgpu — request only when the adapter
        // has it, and record the outcome so the decode chain knows whether an
        // HDR stream can skip the CPU zscale tonemap. Desktop Vulkan/DX12 all
        // have it; GLES (ARM SBCs) generally doesn't and keeps the CPU path.
        let hdr_capable = !gpu_tonemap_env_off()
            && adapter
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
        let _ = GPU_TONEMAP_OK.set(hdr_capable);
        tracing::info!(
            "GPU HDR tonemap: {}",
            if hdr_capable {
                "available (16-bit textures)"
            } else if gpu_tonemap_env_off() {
                "disabled by TRIREME_GPU_TONEMAP=0 (CPU zscale fallback)"
            } else {
                "unavailable on this device (CPU zscale fallback)"
            }
        );
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("trireme"),
                    required_features: if hdr_capable {
                        wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
                    } else {
                        wgpu::Features::empty()
                    },
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .context("request_device")?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let want = present.present_mode();
        let present_mode = if caps.present_modes.contains(&want) {
            want
        } else {
            tracing::warn!("present mode {want:?} unsupported; falling back to Fifo");
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: if matches!(present, PaceMode::LowLatency) {
                1
            } else {
                2
            },
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        tracing::info!(
            "surface {}x{} {format:?} present={present_mode:?}",
            config.width,
            config.height
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trireme-bgl"),
            entries: &[
                tex_entry(0), // Y plane (R8)
                tex_entry(1), // CbCr plane (Rg8)
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(format.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // HDR pipeline: same fullscreen blit over the same bind group layout
        // (16-bit plane views still sample as filterable float), but the
        // fragment shader does the whole HDR10→SDR tonemap (issue #15). The
        // hable normalization peak is baked into the shader source at startup.
        let hdr_pipeline = hdr_capable.then(|| {
            let src = HDR_SHADER.replace("{PEAK}", &format!("{:?}", hdr_tonemap_peak()));
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("blit-hdr"),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("blit-hdr"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_main",
                    targets: &[Some(format.into())],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        });

        let source_gpus = sources
            .iter()
            .map(|s| SourceGpu::new(&device, &queue, &bgl, &sampler, s.width, s.height, false))
            .collect();

        // --- active-cell glow overlay ---------------------------------------
        let glow_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glow-uniform"),
            size: 48, // 3 × vec4<f32>
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glow_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glow-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let glow_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glow-bg"),
            layout: &glow_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: glow_buf.as_entire_binding(),
            }],
        });
        let glow_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glow"),
            source: wgpu::ShaderSource::Wgsl(GLOW_SHADER.into()),
        });
        let glow_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&glow_bgl],
            push_constant_ranges: &[],
        });
        let glow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glow"),
            layout: Some(&glow_layout),
            vertex: wgpu::VertexState {
                module: &glow_shader,
                entry_point: "vs",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &glow_shader,
                entry_point: "fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            hdr_pipeline,
            sources: source_gpus,
            sampler,
            bgl,
            glow_pipeline,
            glow_buf,
            glow_bg,
        })
    }

    /// Upload one decoded frame: the Y plane to the full-res texture and the
    /// interleaved CbCr plane to the half-res texture (NV12 = 1 byte/sample,
    /// P010 = 2 bytes/sample). If the frame's resolution OR colorimetry differs
    /// from the current textures (a native source changed video mode, or the
    /// decode chain flipped SDR↔HDR), the planes are re-created first. The
    /// YUV→RGB conversion happens later in the fragment shader.
    fn upload(&mut self, idx: usize, frame: &Frame) {
        if frame.hdr && self.hdr_pipeline.is_none() {
            // Can't happen while the decode chain respects `tonemap_chain()`,
            // but never feed P010 bytes to a device without 16-bit textures.
            tracing::warn!("source[{idx}] dropped an HDR frame (no GPU tonemap support)");
            return;
        }
        if self.sources[idx].width != frame.w
            || self.sources[idx].height != frame.h
            || self.sources[idx].hdr != frame.hdr
        {
            tracing::info!(
                "source[{idx}] gpu textures → {}x{} ({})",
                frame.w,
                frame.h,
                if frame.hdr { "P010" } else { "NV12" }
            );
            self.sources[idx] = SourceGpu::new(
                &self.device,
                &self.queue,
                &self.bgl,
                &self.sampler,
                frame.w,
                frame.h,
                frame.hdr,
            );
        }
        let s = &self.sources[idx];
        let bps = if s.hdr { 2 } else { 1 };
        let frame = &frame.data;
        let y_size = (s.width * s.height) as usize * bps;
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &s.y_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame[..y_size.min(frame.len())],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(s.width * bps as u32),
                rows_per_image: Some(s.height),
            },
            wgpu::Extent3d {
                width: s.width,
                height: s.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &s.uv_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame[y_size.min(frame.len())..],
            wgpu::ImageDataLayout {
                offset: 0,
                // (w/2) CbCr texels × 2 channels × bps = w*bps bytes per row.
                bytes_per_row: Some(s.width * bps as u32),
                rows_per_image: Some(s.height / 2),
            },
            wgpu::Extent3d {
                width: s.width / 2,
                height: s.height / 2,
                depth_or_array_layers: 1,
            },
        );
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn present(
        &self,
        cells: &[Cell],
        glow: Option<([f32; 4], [f32; 3], f32)>,
    ) -> Result<(), wgpu::SurfaceError> {
        // Update the highlight uniform (rect + colour + thickness) before encoding.
        if let Some(([x, y, w, h], [r, g, b], thickness)) = glow {
            let u: [f32; 12] = [
                x, y, w, h, // rect (framebuffer px)
                r, g, b, 1.0, // sRGB colour + alpha (border is opaque)
                thickness, 0.0, 0.0, 0.0, // params: x = thickness px
            ];
            let bytes = unsafe {
                std::slice::from_raw_parts(u.as_ptr().cast::<u8>(), std::mem::size_of_val(&u))
            };
            self.queue.write_buffer(&self.glow_buf, 0, bytes);
        }
        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            for cell in cells {
                // Per-cell pipeline: HDR sources (P010 planes) tonemap in the
                // shader; SDR sources take the plain BT.709 blit. Mixed grids
                // just switch pipelines between draws.
                let src = &self.sources[cell.source];
                match (src.hdr, &self.hdr_pipeline) {
                    (true, Some(hdr)) => rp.set_pipeline(hdr),
                    _ => rp.set_pipeline(&self.pipeline),
                }
                rp.set_viewport(cell.x, cell.y, cell.w, cell.h, 0.0, 1.0);
                rp.set_bind_group(0, &src.bind_group, &[]);
                rp.draw(0..3, 0..1);
            }
            // Highlight overlay: clip a fullscreen triangle to the active cell
            // and alpha-blend the border on top of the video.
            if let Some(([x, y, w, h], _, _)) = glow {
                rp.set_pipeline(&self.glow_pipeline);
                rp.set_viewport(x, y, w, h, 0.0, 1.0);
                rp.set_bind_group(0, &self.glow_bg, &[]);
                rp.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        frame.present();
        Ok(())
    }
}

const SHADER: &str = r#"
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var out: VsOut;
    let xy = p[i];
    out.pos = vec4(xy, 0.0, 1.0);
    out.uv = vec2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return select(lo, hi, c > vec3(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // NV12, BT.709 limited ("tv") range → gamma-encoded R'G'B'. Y is full-res;
    // CbCr is half-res and the linear sampler upsamples it. The coefficients
    // fold in the 219/224 limited-range scaling.
    let y  = (textureSample(y_tex, samp, in.uv).r - 0.0627451) * 1.1643836;
    let c  = textureSample(uv_tex, samp, in.uv).rg - vec2(0.5019608, 0.5019608);
    let cb = c.x;
    let cr = c.y;
    let rgb = clamp(vec3(
        y + 1.7927411 * cr,
        y - 0.2132486 * cb - 0.5329093 * cr,
        y + 2.1124018 * cb,
    ), vec3(0.0), vec3(1.0));
    // The swapchain is sRGB and re-encodes on store; sRGB-decode here so the
    // round-trip is a net no-op and the gamma-encoded video reaches the
    // framebuffer unchanged — matching the previous swscale-RGBA look.
    return vec4(srgb_to_linear(rgb), 1.0);
}
"#;

/// The hable normalization peak baked into the HDR shader: the tonemap divides
/// by `hable(PEAK)` so this is the signal level that maps to full white.
///
/// The default 10.0 reproduces the CPU chain's look: ffmpeg's `tonemap=hable`
/// reads the peak from the stream's mastering metadata as
/// `max_luminance / REFERENCE_WHITE` = 1000 / 100 = 10.0 (both Pharos hosts
/// attach 1000-nit MDCV SEI), and applies it to a signal that zscale's
/// `npl=203` normalized to SDR-white=1.0 — a unit mismatch, but it IS the look
/// the tonemap has always had, so we replicate it rather than "fix" it.
/// `TRIREME_HDR_PEAK` overrides for A/B tuning (lower = brighter midtones).
fn hdr_tonemap_peak() -> f32 {
    std::env::var("TRIREME_HDR_PEAK")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|p| p.is_finite() && *p > 0.0)
        .unwrap_or(10.0)
}

/// GPU HDR→SDR tonemap (issue #15): the fragment-shader port of the CPU chain
/// `zscale=t=linear:npl=203,tonemap=hable,zscale=p=bt709:t=bt709:m=bt709:r=tv`,
/// fed 10-bit P010 planes straight from the decoder instead of CPU-tonemapped
/// NV12 — the zimg path topped out at ~19 fps at 4K where the decoder itself
/// was fine, so the whole transform moves onto the GPU. Stages, in the CPU
/// chain's exact order:
///   1. P010 → 10-bit codes (samples are left-justified in 16 bits).
///   2. Limited-range BT.2020-NCL YCbCr → non-linear R'G'B'.
///   3. PQ EOTF (ST-2084; constants match the host encoder's inverse in
///      `compositor/src/multi.rs` / `pharos-win/src/zerocopy.rs`), normalized
///      to SDR white = 203 nits (zscale's `npl=203`).
///   4. hable tonemap with ffmpeg's default desaturation (desat=2.0), still in
///      linear BT.2020, exactly where `tonemap=hable` runs. NOTE the desat
///      "luma" is r+g+b with UNITY coefficients: zscale hands vf_tonemap an
///      RGB-colorspace frame, so ffmpeg's `av_csp_luma_coeffs_from_avcsp`
///      yields unity weights and every pixel with r+g+b > 2 desaturates toward
///      gray. A quirk, but it IS the shipped look — verified pixel-exact
///      against the reference chain (worst 2.33/255 over a 40-patch sweep on
///      ffmpeg n8.1.2), so we replicate it deliberately.
///   5. BT.2020 → BT.709 linear gamut map (the exact inverse of the host's
///      BT709_TO_BT2020 matrix).
///   6. Inverse BT.1886 encode (pure gamma 1/2.4 — zimg's display-referred
///      `t=bt709`, NOT the scene OETF; also verified against the reference),
///      then the same sRGB-decode-before-sRGB-store no-op as the SDR shader so
///      the encoded video reaches the framebuffer as-is.
///
/// `{PEAK}` is replaced with [`hdr_tonemap_peak`] at pipeline creation.
const HDR_SHADER: &str = r#"
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var out: VsOut;
    let xy = p[i];
    out.pos = vec4(xy, 0.0, 1.0);
    out.uv = vec2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

// PQ EOTF (ST-2084): non-linear signal → nits. The inverse of the host
// encoder's nits_to_pq.
fn pq_to_nits(e: vec3<f32>) -> vec3<f32> {
    let em = pow(max(e, vec3(0.0)), vec3(1.0 / PQ_M2));
    let num = max(em - vec3(PQ_C1), vec3(0.0));
    let den = max(vec3(PQ_C2) - PQ_C3 * em, vec3(1e-6));
    return 10000.0 * pow(num / den, vec3(1.0 / PQ_M1));
}

// Uncharted-2 hable curve, ffmpeg vf_tonemap's constants.
fn hable(x: f32) -> f32 {
    return (x * (x * 0.15 + 0.05) + 0.004) / (x * (x * 0.15 + 0.5) + 0.06) - 1.0 / 15.0;
}

// BT.2020 → BT.709 linear RGB: the exact inverse of the host's
// BT709_TO_BT2020 (multi.rs / zerocopy.rs), column-major.
const BT2020_TO_BT709: mat3x3<f32> = mat3x3<f32>(
    vec3(1.660491002, -0.124550474, -0.018150764),
    vec3(-0.587641138, 1.132899897, -0.100578898),
    vec3(-0.072849864, -0.008349423, 1.118729662),
);

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return select(lo, hi, c > vec3(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // P010: 10-bit codes left-justified in 16-bit words, so an R16Unorm sample
    // is code*64/65535; rescale to code/1023.
    let scale = 65535.0 / 65472.0;
    let yv = textureSample(y_tex, samp, in.uv).r * scale;
    let cv = textureSample(uv_tex, samp, in.uv).rg * scale;
    // 10-bit limited range: Y 64..940, chroma 64..960 centered on 512.
    let y  = (yv * 1023.0 - 64.0) / 876.0;
    let cb = (cv.x * 1023.0 - 512.0) / 896.0;
    let cr = (cv.y * 1023.0 - 512.0) / 896.0;
    // BT.2020 non-constant-luminance YCbCr → R'G'B' (Kr=0.2627, Kb=0.0593).
    let rgb_pq = clamp(vec3(
        y + 1.4746 * cr,
        y - 0.16455313 * cb - 0.57135313 * cr,
        y + 1.8814 * cb,
    ), vec3(0.0), vec3(1.0));
    // PQ → linear light, normalized so SDR white (203 nits) = 1.0 (npl=203).
    var lin = pq_to_nits(rgb_pq) / 203.0;
    // hable tonemap in linear BT.2020, matching ffmpeg vf_tonemap: desaturate
    // overbrights toward "luma" (desat=2.0 default) — with UNITY coefficients,
    // the CPU chain's csp=gbr quirk (see the Rust doc comment above) — then
    // scale by the max component through the curve, normalized to the peak.
    let luma = lin.r + lin.g + lin.b;
    let overbright = max(luma - 2.0, 1e-6) / max(luma, 1e-6);
    lin = mix(lin, vec3(luma), overbright);
    let sig = max(max(lin.r, lin.g), max(lin.b, 1e-6));
    lin = lin * (hable(sig) / hable({PEAK}) / sig);
    // Gamut map to BT.709, then encode like the CPU chain's final zscale:
    // inverse BT.1886 (display-referred pure gamma 1/2.4).
    let lin709 = clamp(BT2020_TO_BT709 * lin, vec3(0.0), vec3(1.0));
    let enc = pow(lin709, vec3(1.0 / 2.4));
    // Same sRGB-store no-op dance as the SDR shader.
    return vec4(srgb_to_linear(enc), 1.0);
}
"#;

// --- Source runtime ---------------------------------------------------------

/// What a cell streams from. Chosen by `SourceCfg.protocol`. Pharos is the
/// default; VNC and SPICE are native console protocols decoded client-side and
/// bridged into the same NV12 render path via ffmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Pharos,
    Vnc,
    Spice,
}

impl SourceKind {
    /// Map a `protocol` string to a kind. Unknown non-empty values fall back to
    /// Pharos with a warning (rather than silently), so a typo is visible.
    fn from_protocol(protocol: Option<&str>, source_name: &str) -> Self {
        match protocol {
            None | Some("") | Some("pharos") => SourceKind::Pharos,
            Some("vnc") => SourceKind::Vnc,
            Some("spice") => SourceKind::Spice,
            Some(other) => {
                tracing::warn!(
                    "source '{source_name}': unknown protocol {other:?}; treating as pharos"
                );
                SourceKind::Pharos
            }
        }
    }
}

struct Source {
    name: String,
    /// Candidate addresses for this host (`[addr] + alt_addrs`). The reconnect
    /// loop rotates through them so a dead host NIC fails over to another path.
    addrs: Vec<SocketAddr>,
    /// Index into `addrs` of the address used for the current/next connection.
    addr_idx: usize,
    pace: PaceMode,
    width: u32,
    height: u32,
    /// Which kind of source this is (Pharos QUIC/HEVC, native VNC, or native
    /// SPICE). Selected by `protocol` in the config; decides which decode
    /// pipeline `spawn_source` starts.
    kind: SourceKind,
    /// Auth password for a VNC/SPICE source that requires one.
    password: Option<String>,
    /// Pinned host pubkey for this source. Derived from the registry at
    /// build time; passed to `transport::run_video_client`.
    expected_pubkey: Option<[u8; 32]>,
    queue: Arc<FrameQueue>,
    /// Channel into the per-source input-send task. `None` until
    /// `spawn_decode` runs — populated then so the App's WindowEvent handler
    /// can forward classified events.
    input_tx: Option<tokio::sync::mpsc::UnboundedSender<core_proto::InputEvent>>,
    /// Lock-free decode/present latency taps, shared with this source's QUIC
    /// transport task so the host-triggered live benchmark can read them.
    /// `Some` only for Pharos QUIC sources (the decode reader records decode
    /// timing; `render` records present timing). `None` for VNC/SPICE.
    taps: Option<Arc<transport::LatencyTaps>>,
    presents: u64,
    win_start: Instant,
}

impl Source {
    fn from_cfg(cfg: &SourceCfg) -> Self {
        let pace = cfg
            .pace
            .as_deref()
            .map(PaceMode::parse)
            .unwrap_or(PaceMode::LowLatency);
        let width = cfg.width.unwrap_or(DEFAULT_WIDTH);
        let height = cfg.height.unwrap_or(DEFAULT_HEIGHT);
        let name = cfg.name.clone().unwrap_or_else(|| cfg.addr.to_string());
        let kind = SourceKind::from_protocol(cfg.protocol.as_deref(), &name);
        let expected_pubkey = cfg
            .host_pubkey
            .as_deref()
            .and_then(|h| hex::decode(h).ok())
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
        if cfg.host_pubkey.is_some() && expected_pubkey.is_none() {
            tracing::warn!(
                "source '{}' has malformed host_pubkey; falling back to TOFU",
                name
            );
        }
        // Candidate addresses: the primary first, then any alternates, deduped.
        let mut addrs = vec![cfg.addr];
        for a in &cfg.alt_addrs {
            if !addrs.contains(a) {
                addrs.push(*a);
            }
        }
        Self {
            name,
            addrs,
            addr_idx: 0,
            pace,
            width,
            height,
            kind,
            password: cfg.password.clone(),
            expected_pubkey,
            queue: Arc::new(FrameQueue {
                inner: Mutex::new(VecDeque::new()),
                depth: pace.depth(),
            }),
            input_tx: None,
            taps: None,
            presents: 0,
            win_start: Instant::now(),
        }
    }

    /// The address for the current/next connection attempt.
    fn current_addr(&self) -> SocketAddr {
        self.addrs[self.addr_idx % self.addrs.len()]
    }
}

/// How long to wait after a dropped stream before reconnecting. Long enough not
/// to hammer a host that's mid-reboot / NIC-flap, short enough to feel transient.
const RECONNECT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1500);

/// Backoff for a host that REFUSED us because its primary client holds the
/// slot (issue #19). Much longer than [`RECONNECT_BACKOFF`]: every retry will
/// keep being refused until the primary leaves, so redialing at blip speed is
/// just noise — but do keep retrying, so this client picks the slot back up
/// within ~15s of the primary disconnecting.
const RESERVED_BACKOFF: std::time::Duration = std::time::Duration::from_secs(15);

/// Why a host deliberately refused this client (issue #19) — mapped from the
/// stable transport error messages, with distinct retry policies.
#[derive(Debug, Clone, Copy)]
enum StreamRefusal {
    /// The host's primary client holds the slot; retry on [`RESERVED_BACKOFF`].
    ReservedByPrimary,
    /// The host unpaired (forgot) this device; retrying is pointless until the
    /// user pairs it again, so reconnection stops.
    Unpaired,
}

#[derive(Debug)]
#[allow(dead_code)] // index isn't read today; kept for per-source trace logs.
enum UserEvent {
    FrameReady(usize),
    StreamEnded(usize),
    /// The host deliberately refused this source (issue #19) — unlike
    /// [`UserEvent::StreamEnded`] this must NOT take the ordinary fast
    /// reconnect: reserved-by-primary retries slowly, unpaired stops.
    StreamRefused(usize, StreamRefusal),
    /// Time to (re)connect source `usize` — fired after a backoff once a stream
    /// drops, so the player rotates to the next address and comes back.
    Reconnect(usize),
    /// A native (VNC/SPICE) source reported its native resolution `(w, h)`. The
    /// player adopts it as the source's aspect so the cell letterboxes correctly.
    SourceSize(usize, u32, u32),
    /// The host game's cursor-capture state changed (from the `DG_CURSOR_MODE`
    /// announce, decoded in the reader thread): `true` = CAPTURED (the game grabbed
    /// the mouse for relative mouselook), `false` = FREE (a menu/desktop absolute
    /// pointer). The player mirrors it into its own OS cursor grab so a windowed FPS
    /// auto-locks the client cursor during gameplay and frees it in menus, with no
    /// manual toggle. Sent only on a CHANGE (not every frame).
    CursorMode(bool),
}

#[derive(Debug, Clone, Copy)]
struct Cell {
    source: usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Letterbox the source's `src_w × src_h` aspect into the cell rectangle so
/// content is never stretched. Returns the inner viewport rect.
fn letterbox(
    cell_x: f32,
    cell_y: f32,
    cell_w: f32,
    cell_h: f32,
    src_w: u32,
    src_h: u32,
) -> (f32, f32, f32, f32) {
    let src_ar = src_w as f32 / src_h as f32;
    let cell_ar = cell_w / cell_h;
    if cell_ar > src_ar {
        let w = cell_h * src_ar;
        let x = cell_x + (cell_w - w) * 0.5;
        (x, cell_y, w, cell_h)
    } else {
        let h = cell_w / src_ar;
        let y = cell_y + (cell_h - h) * 0.5;
        (cell_x, y, cell_w, h)
    }
}

/// Mean aspect ratio (w/h) of the sources, used as the target cell aspect for
/// the flex grid. Defaults to 16:9 if there are no sources.
fn avg_source_aspect(sources: &[Source]) -> f32 {
    if sources.is_empty() {
        return 16.0 / 9.0;
    }
    let sum: f32 = sources
        .iter()
        .map(|s| s.width as f32 / s.height.max(1) as f32)
        .sum();
    sum / sources.len() as f32
}

/// Choose a `(cols, rows)` grid covering `n` sources that maximises the fraction
/// of the window actually showing source pixels — trading letterbox bars (cell
/// aspect vs `src_aspect`) against empty cells. As the window aspect changes the
/// winner changes, so a wide window flows toward a row, a tall one toward a
/// column, and a ~square window toward a balanced grid. Recomputed on every
/// resize so the layout re-flows live. Always returns `cols*rows >= n`.
fn flex_grid(n: usize, win_w: u32, win_h: u32, src_aspect: f32) -> (u32, u32) {
    let n = n.max(1) as u32;
    if n == 1 {
        return (1, 1);
    }
    let (w, h) = (win_w.max(1) as f32, win_h.max(1) as f32);
    let src = if src_aspect.is_finite() && src_aspect > 0.0 {
        src_aspect
    } else {
        16.0 / 9.0
    };
    let mut best = (n, 1u32);
    let mut best_score = f32::NEG_INFINITY;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cell_aspect = (w / cols as f32) / (h / rows as f32);
        // Fraction of a cell a letterboxed source fills (1.0 = aspect match).
        let fill = (cell_aspect / src).min(src / cell_aspect);
        // Fraction of cells that are non-empty.
        let used = n as f32 / (cols * rows) as f32;
        // Maximise displayed area; gently tie-break toward a square arrangement.
        let score = fill * used - 0.001 * (cols as f32 - rows as f32).abs();
        if score > best_score {
            best_score = score;
            best = (cols, rows);
        }
    }
    best
}

fn compute_cells(cols: u32, rows: u32, win_w: u32, win_h: u32, sources: &[Source]) -> Vec<Cell> {
    let cell_w = win_w as f32 / cols as f32;
    let cell_h = win_h as f32 / rows as f32;
    let mut cells = Vec::with_capacity(sources.len());
    for (i, s) in sources.iter().enumerate() {
        let col = (i as u32) % cols;
        let row = (i as u32) / cols;
        let cx = col as f32 * cell_w;
        let cy = row as f32 * cell_h;
        let (x, y, w, h) = letterbox(cx, cy, cell_w, cell_h, s.width, s.height);
        cells.push(Cell {
            source: i,
            x,
            y,
            w,
            h,
        });
    }
    cells
}

struct App {
    cfg: PlayerCfg,
    sources: Vec<Source>,
    /// The client's long-term identity, presented on each video connect so an
    /// enforcing host can authorize us against its paired-peers list.
    identity: Arc<core_crypto::Identity>,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    cells: Vec<Cell>,
    started: bool,
    // Input state -------------------------------------------------------------
    /// Modifier state, updated from winit's `ModifiersChanged` event so we can
    /// stamp every Key with the right modifier set even when keys arrive out
    /// of order (autorepeat, IME).
    mods: core_proto::Modifiers,
    /// True when the Trireme window has keyboard focus. Used by `focus` mode.
    focused: bool,
    /// Last cursor position (in window-logical pixels). Used to find which
    /// source cell the pointer is over and to project absolute motion.
    cursor: Option<(f32, f32)>,
    /// Source index the cursor is currently over, for routing pointer events.
    cursor_cell: Option<usize>,
    /// `toggle` mode: latched on/off state, flipped by the `toggle_capture` hotkey.
    capture_toggle_on: bool,
    /// Whether the player is currently fullscreen (toggled by `toggle_fullscreen`).
    fullscreen: bool,
    /// `true` while the window is minimized/occluded — rendering is skipped so we
    /// don't churn the GPU presenting frames nobody can see.
    suspended: bool,
    /// Reconnect-safe target for the desktop gamepad capture thread (issue #14).
    /// The capture thread (spawned once in `resumed`) writes canonical Gamepad*
    /// events here; we keep it pointed at the ACTIVE source's `input_tx`. It is a
    /// shared `Option<Sender>` and not a bare `Sender` because `input_tx` is
    /// re-created on every (re)connect — handing the thread a `Sender` by value
    /// would leave pads dead after the first blip. `None` means "don't forward":
    /// either no source is connected, or `CaptureMode::Off` has muted input.
    gamepad_sink: crate::gamepad::GamepadSink,
    /// Last input device that deliberately drove the stream (kbd/mouse vs pad),
    /// shared with the gamepad capture thread. Used to mute bare mouse-motion
    /// forwarding while a controller is in use, so a free cursor drifting to the
    /// window edge stops fighting the pad for the game's active-device focus.
    input_source: crate::gamepad::InputSource,
    /// Keys/pointer-buttons we have forwarded a press for and not yet a release,
    /// so we can release them all when the window loses focus. Without this an
    /// alt-tab loses the key-up events (they fire while unfocused, or the OS eats
    /// the alt-tab chord), leaving the host's virtual keyboard with a stuck key
    /// (e.g. Alt) so the next focus session reads every keystroke as Alt+key.
    held_keys: std::collections::HashSet<u32>,
    held_buttons: std::collections::HashSet<u8>,
    /// The user's cursor-grab intent (issue #5) — flipped by the `toggle_grab`
    /// hotkey. Persists across focus loss so alt-tab-and-back resumes mouselook.
    grab_wanted: bool,
    /// Whether the OS cursor is *actually* grabbed right now. Only true while
    /// `grab_wanted && focused` — [`Self::apply_grab`] reconciles the two. When on,
    /// the cursor is hidden + locked and raw motion becomes `PointerRel` (instead of
    /// `PointerAbs` from `CursorMoved`), for FPS-style mouselook.
    grabbed: bool,
}

impl App {
    /// Re-flow the grid for a window of `w × h` and rebuild the cell rectangles.
    /// Called on first show and on every resize, so the layout adapts live to the
    /// window's aspect ratio (see [`flex_grid`]). A zero dimension (minimize) is
    /// ignored — the previous layout is kept until the window is restored.
    fn relayout(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let (cols, rows) = flex_grid(self.sources.len(), w, h, avg_source_aspect(&self.sources));
        self.cells = compute_cells(cols, rows, w, h, &self.sources);
        if self.sources.len() > 1 {
            if let Some(win) = &self.window {
                win.set_title(&format!(
                    "Trireme — {} sources ({cols}×{rows})",
                    self.sources.len()
                ));
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Deliver raw `DeviceEvent::MouseMotion` while focused — the source of
        // relative deltas for cursor-grab mode (issue #5). winit doesn't emit
        // device events by default.
        el.listen_device_events(winit::event_loop::DeviceEvents::WhenFocused);
        let title = if self.sources.len() == 1 {
            format!("Trireme — {}", self.sources[0].name)
        } else {
            format!(
                "Trireme — {} sources ({}×{})",
                self.sources.len(),
                self.cfg.layout.cols,
                self.cfg.layout.rows
            )
        };
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title(title)
                    .with_inner_size(LogicalSize::new(1280.0, 720.0)),
            )
            .expect("create window"),
        );
        match pollster::block_on(Gpu::new(
            window.clone(),
            self.cfg.present_pace(),
            &self.sources,
        )) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(e) => {
                tracing::error!("gpu init failed: {e:?}");
                el.exit();
                return;
            }
        }
        let size = window.inner_size();
        self.window = Some(window);
        self.relayout(size.width, size.height);
        if !self.started {
            self.started = true;
            for (i, s) in self.sources.iter_mut().enumerate() {
                match spawn_source(
                    i,
                    s.current_addr(),
                    s.expected_pubkey,
                    s.width,
                    s.height,
                    s.queue.clone(),
                    self.proxy.clone(),
                    self.identity.clone(),
                    s.kind,
                    s.password.clone(),
                ) {
                    Ok((tx, taps)) => {
                        s.input_tx = tx;
                        s.taps = taps;
                    }
                    Err(e) => {
                        tracing::error!("decode start failed for source[{i}] ({}): {e:?}", s.name);
                        el.exit();
                        return;
                    }
                }
            }
            // Point the gamepad sink at the freshly-connected active source, then
            // spawn the capture thread ONCE (issue #14). The thread outlives every
            // reconnect; only the shared sink is re-pointed as sources cycle.
            self.refresh_gamepad_sink();
            crate::gamepad::run_gamepad_capture(
                self.gamepad_sink.clone(),
                self.input_source.clone(),
            );
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::FrameReady(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            // A native source reported its true resolution → adopt it as this
            // source's aspect and re-flow so the cell letterboxes correctly.
            UserEvent::SourceSize(i, w, h) => {
                let changed = self
                    .sources
                    .get(i)
                    .is_some_and(|s| (s.width, s.height) != (w, h));
                if changed {
                    if let Some(s) = self.sources.get_mut(i) {
                        s.width = w;
                        s.height = h;
                    }
                    if let Some(win) = self.window.clone() {
                        let sz = win.inner_size();
                        self.relayout(sz.width, sz.height);
                        win.request_redraw();
                    }
                }
            }
            UserEvent::StreamEnded(i) => {
                // The connection dropped (host NIC down, reboot, network blip).
                // Rotate to the next candidate address and reconnect after a
                // backoff — a host-NIC failure becomes a transient blip, and a
                // single-homed host just auto-reconnects.
                if let Some(s) = self.sources.get_mut(i) {
                    s.input_tx = None; // old back-channel is dead until we reconnect
                    if s.addrs.len() > 1 {
                        s.addr_idx = s.addr_idx.wrapping_add(1);
                    }
                    tracing::info!(
                        "source[{i}] ({}) dropped; reconnecting to {} in {:?}",
                        s.name,
                        s.current_addr(),
                        RECONNECT_BACKOFF
                    );
                }
                self.refresh_gamepad_sink(); // pads mute until the source is back
                                             // Free the cursor if mouselook-grabbed — don't trap it during the
                                             // reconnect window; the user re-grabs after reconnect.
                self.grab_wanted = false;
                self.apply_grab();
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(RECONNECT_BACKOFF);
                    let _ = proxy.send_event(UserEvent::Reconnect(i));
                });
            }
            UserEvent::StreamRefused(i, refusal) => {
                // The host said no ON PURPOSE (issue #19) — honest copy + the
                // right retry policy instead of the ordinary blip reconnect.
                if let Some(s) = self.sources.get_mut(i) {
                    s.input_tx = None; // back-channel is dead either way
                }
                self.refresh_gamepad_sink(); // pads mute while refused
                                             // Free the cursor if mouselook-grabbed (same as a plain drop).
                self.grab_wanted = false;
                self.apply_grab();
                let name = self
                    .sources
                    .get(i)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                match refusal {
                    StreamRefusal::ReservedByPrimary => {
                        // Same address on purpose: the host is healthy, just
                        // reserved — rotating NICs wouldn't change the answer.
                        tracing::warn!(
                            "source[{i}] ({name}) refused: host reserved by its primary \
                             client; retrying in {RESERVED_BACKOFF:?}"
                        );
                        let proxy = self.proxy.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(RESERVED_BACKOFF);
                            let _ = proxy.send_event(UserEvent::Reconnect(i));
                        });
                    }
                    StreamRefusal::Unpaired => {
                        // Every future connect fails authorization too — stop
                        // redialing instead of hammering a host that forgot us.
                        tracing::error!(
                            "source[{i}] ({name}) unpaired by the host — pair this device \
                             again to reconnect (auto-retry stopped)"
                        );
                    }
                }
            }
            UserEvent::Reconnect(i) => {
                // Re-spawn the source pipeline on its current address. Snapshot the
                // params first to avoid holding a borrow across spawn_decode.
                let params = self.sources.get(i).map(|s| {
                    (
                        s.current_addr(),
                        s.expected_pubkey,
                        s.width,
                        s.height,
                        s.queue.clone(),
                        s.name.clone(),
                        s.kind,
                        s.password.clone(),
                    )
                });
                let Some((addr, pubkey, w, h, queue, name, kind, password)) = params else {
                    return;
                };
                match spawn_source(
                    i,
                    addr,
                    pubkey,
                    w,
                    h,
                    queue,
                    self.proxy.clone(),
                    self.identity.clone(),
                    kind,
                    password,
                ) {
                    Ok((tx, taps)) => {
                        if let Some(s) = self.sources.get_mut(i) {
                            s.input_tx = tx;
                            s.taps = taps;
                        }
                        self.refresh_gamepad_sink(); // re-point pads at the new channel
                        tracing::info!("source[{i}] ({name}) reconnecting on {addr}");
                    }
                    Err(e) => {
                        tracing::error!("source[{i}] reconnect spawn failed: {e:?}; retrying");
                        let proxy = self.proxy.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(RECONNECT_BACKOFF);
                            let _ = proxy.send_event(UserEvent::Reconnect(i));
                        });
                    }
                }
            }
            // The host game's cursor-capture state changed: mirror it into our OS
            // cursor grab so a windowed FPS auto-locks the cursor (relative mouselook)
            // during gameplay and frees it (absolute) in menus, no manual toggle.
            // `grab_wanted` is the SAME intent the Ctrl+Alt+Home `toggle_grab` hotkey
            // drives; setting it here means the manual toggle stays a MANUAL OVERRIDE:
            // it flips the intent as before, and the NEXT host CursorMode change simply
            // re-syncs it. Acceptable for v1. Gate on the user's mute: if input capture
            // is CaptureMode::Off we never auto-grab (respect the mute); `apply_grab`
            // itself further gates on window focus. If the host never announces (older
            // host, or the not-yet-done Linux host), no CursorMode ever arrives and the
            // client behaves exactly as today (manual grab only).
            UserEvent::CursorMode(captured) => {
                use crate::input::CaptureMode;
                if matches!(self.cfg.input.mode, CaptureMode::Off) {
                    // Muted: don't auto-grab. (A later un-mute + host re-announce, or
                    // the manual hotkey, still engages it.)
                    return;
                }
                if self.grab_wanted != captured {
                    self.grab_wanted = captured;
                    self.apply_grab();
                    tracing::info!(
                        "host cursor {} → client grab_wanted={captured}",
                        if captured { "CAPTURED" } else { "FREE" }
                    );
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        use winit::event::WindowEvent as WE;
        match ev {
            WE::CloseRequested => el.exit(),
            WE::Resized(s) => {
                // A zero dimension means minimized — suspend rendering until the
                // window comes back (configuring a 0-sized surface is invalid).
                if s.width == 0 || s.height == 0 {
                    self.suspended = true;
                } else {
                    self.suspended = false;
                    if let Some(g) = &mut self.gpu {
                        g.resize(s.width, s.height);
                    }
                    self.relayout(s.width, s.height);
                    // Re-flow immediately even if no source frame is in flight, so
                    // the picture tracks the drag instead of stretching the last
                    // frame until the next decode tick.
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            // Fully hidden (minimized / behind another window on some compositors)
            // → stop presenting; resume + redraw when shown again.
            WE::Occluded(hidden) => {
                self.suspended = hidden;
                if !hidden {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WE::RedrawRequested => self.render(),
            WE::Focused(f) => {
                self.focused = f;
                // On focus loss, release every key/button we still hold on the
                // host: their up events fire while unfocused (or the OS ate the
                // alt-tab chord), so without this the host keeps them pressed and
                // the next focus session is stuck-key broken (a held Alt turns
                // every keystroke into Alt+key). Also clear the local modifier
                // state so a stuck Shift/Alt isn't carried over.
                if !f {
                    self.release_all_held();
                    self.mods = core_proto::Modifiers::default();
                }
                // Reconcile the grab: release when we lose focus (don't trap the
                // cursor on alt-tab), re-grab when we regain it if the user still
                // wants mouselook.
                self.apply_grab();
                self.reconcile_cursor();
            }
            WE::ModifiersChanged(m) => {
                let st = m.state();
                self.mods = core_proto::Modifiers {
                    ctrl: st.control_key(),
                    shift: st.shift_key(),
                    alt: st.alt_key(),
                    meta: st.super_key(),
                };
            }
            WE::KeyboardInput { event, .. } => self.handle_key(el, event),
            WE::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                self.cursor = Some((x, y));
                let new_cell = self
                    .cells
                    .iter()
                    .find(|c| x >= c.x && x < c.x + c.w && y >= c.y && y < c.y + c.h)
                    .map(|c| c.source);
                let cell_changed = new_cell != self.cursor_cell;
                self.cursor_cell = new_cell;
                self.handle_pointer_move();
                // Only on enter/leave a cell (not every move): toggle the local
                // cursor so it's hidden over the video (the host cursor shows
                // there) and visible over the client's chrome.
                if cell_changed {
                    self.reconcile_cursor();
                }
            }
            WE::CursorLeft { .. } => {
                self.cursor = None;
                self.cursor_cell = None;
                self.reconcile_cursor();
            }
            WE::MouseInput { state, button, .. } => self.handle_mouse_button(state, button),
            WE::MouseWheel { delta, .. } => self.handle_mouse_wheel(delta),
            _ => {}
        }
    }

    /// Raw device motion — the source of relative mouse deltas for cursor-grab
    /// mode (issue #5). `WindowEvent::CursorMoved` is OS-accelerated and clamps at
    /// the screen edge; `DeviceEvent::MouseMotion` is the unaccelerated delta a
    /// mouselook game wants. Only forwarded while grabbed + capturing.
    fn device_event(
        &mut self,
        _el: &ActiveEventLoop,
        _id: winit::event::DeviceId,
        ev: winit::event::DeviceEvent,
    ) {
        if let winit::event::DeviceEvent::MouseMotion { delta: (dx, dy) } = ev {
            // `grabbed` already implies the window is focused (apply_grab), and an
            // explicit grab is the user's capture intent — so it does NOT gate on
            // capture_active() (which in Focus mode needs a cursor_cell that Locked
            // grab can never refresh). Only the mouse class gate applies.
            if self.grabbed && self.cfg.input.forward_mouse {
                self.send_to_active(core_proto::InputEvent::PointerRel {
                    dx: dx as f32,
                    dy: dy as f32,
                });
            }
        }
    }
}

impl App {
    /// True when input forwarding should actually fire — combines the configured
    /// capture mode with the live `focused` / `cursor_cell` / `capture_toggle_on`
    /// state.
    fn capture_active(&self) -> bool {
        use crate::input::CaptureMode;
        match self.cfg.input.mode {
            CaptureMode::Off => false,
            CaptureMode::Always => true,
            CaptureMode::Toggle => self.capture_toggle_on,
            CaptureMode::Focus => self.focused && self.cursor_cell.is_some(),
        }
    }

    /// Re-point the gamepad capture thread's sink at the source it should drive,
    /// or clear it (issue #14). Called at every site that (re)assigns or clears a
    /// source's `input_tx` — spawn, reconnect, stream-end/refuse — so a pad keeps
    /// working across reconnects even though the underlying channel is recreated.
    ///
    /// Pads forward whenever a source is CONNECTED regardless of window focus
    /// (matching Android and player expectation), so this deliberately does NOT
    /// consult `capture_active()`'s focus/cursor logic — only the hard
    /// `CaptureMode::Off` mute, which clears the sink to `None`. The single-stream
    /// case uses source 0 (the same fallback `send_to_active` uses); its channel
    /// is the one the pads should reach.
    fn refresh_gamepad_sink(&self) {
        use crate::input::CaptureMode;
        let muted = matches!(self.cfg.input.mode, CaptureMode::Off);
        let tx = if muted {
            None
        } else {
            self.sources.first().and_then(|s| s.input_tx.clone())
        };
        if let Ok(mut guard) = self.gamepad_sink.lock() {
            *guard = tx;
        }
    }

    /// Route an InputEvent to one source. Default: source 0 (today's tests
    /// have one source). Multi-source routing follows the pointer: the source
    /// the cursor is currently over receives the events, falling back to 0.
    fn send_to_active(&self, ev: core_proto::InputEvent) {
        use crate::gamepad::SRC_KBDMOUSE;
        use core_proto::InputEvent as IE;
        use std::sync::atomic::Ordering;
        // Last-used-device arbitration (matches the pad side in `gamepad.rs`):
        // bare pointer motion is the flood a free cursor generates (edge-escape,
        // drift), so forward it ONLY while kbd/mouse is the active source — a
        // controller in use must not be fought by a stray mouse. Motion never
        // *switches* the active source (idle drift must not steal focus back from
        // the pad). A deliberate key/click/scroll (a real press) claims kbd/mouse;
        // key/button *releases* pass through without claiming, so a release that
        // lands mid-gamepad-use can't un-mute the drifting cursor.
        match &ev {
            IE::PointerAbs { .. } | IE::PointerRel { .. } => {
                if self.input_source.load(Ordering::Relaxed) != SRC_KBDMOUSE {
                    return;
                }
            }
            IE::Key { pressed: true, .. }
            | IE::PointerButton { pressed: true, .. }
            | IE::Scroll { .. } => {
                self.input_source.store(SRC_KBDMOUSE, Ordering::Relaxed);
            }
            _ => {}
        }
        let idx = self.cursor_cell.unwrap_or(0);
        if let Some(tx) = self.sources.get(idx).and_then(|s| s.input_tx.as_ref()) {
            let _ = tx.send(ev);
        }
    }

    fn handle_key(&mut self, el: &ActiveEventLoop, ev: winit::event::KeyEvent) {
        use crate::input::{classify_key, KeyDecision};
        use winit::keyboard::PhysicalKey;
        let PhysicalKey::Code(code) = ev.physical_key else {
            return;
        };
        let decision = classify_key(
            &self.cfg.input,
            self.capture_active(),
            ev.state,
            code,
            self.mods,
        );
        match decision {
            KeyDecision::Forward(input_ev) => {
                // Track the held set so a focus loss can release it (stuck-key fix).
                if let core_proto::InputEvent::Key {
                    scancode, pressed, ..
                } = input_ev
                {
                    if pressed {
                        self.held_keys.insert(scancode);
                    } else {
                        self.held_keys.remove(&scancode);
                    }
                }
                self.send_to_active(input_ev);
            }
            KeyDecision::Hotkey(name) => self.handle_hotkey(el, name),
            KeyDecision::Local | KeyDecision::Drop => {
                // Local: let winit's default take over (which on Windows means
                // the OS handles it — no action needed on our side).
                // Drop: capture-off or unmapped key — do nothing.
            }
        }
    }

    fn handle_hotkey(&mut self, el: &ActiveEventLoop, name: &str) {
        match name {
            "toggle_capture" => {
                self.capture_toggle_on = !self.capture_toggle_on;
                tracing::info!("capture toggle = {}", self.capture_toggle_on);
                self.reconcile_cursor();
            }
            "toggle_fullscreen" => {
                self.fullscreen = !self.fullscreen;
                if let Some(w) = &self.window {
                    let fs = if self.fullscreen {
                        Some(winit::window::Fullscreen::Borderless(None))
                    } else {
                        None
                    };
                    w.set_fullscreen(fs);
                }
            }
            "disconnect" => {
                tracing::info!("disconnect hotkey — exiting");
                el.exit();
            }
            "toggle_grab" => {
                self.grab_wanted = !self.grab_wanted;
                self.apply_grab();
            }
            _ => {}
        }
    }

    /// Reconcile the OS cursor grab with `grab_wanted && focused` (issue #5). When
    /// grabbed, the cursor is hidden + locked so raw motion (via `device_event`)
    /// becomes `PointerRel` for mouselook. `Locked` is true pointer lock; fall back
    /// to `Confined` where a platform/backend doesn't support it. Idempotent — safe
    /// to call on every focus change / toggle. An explicit grab IS the capture
    /// intent, so it works regardless of `CaptureMode`.
    fn apply_grab(&mut self) {
        use winit::window::CursorGrabMode;
        let want = self.grab_wanted && self.focused;
        if want == self.grabbed {
            return;
        }
        self.grabbed = want;
        if let Some(win) = &self.window {
            if want {
                if win.set_cursor_grab(CursorGrabMode::Locked).is_err() {
                    let _ = win.set_cursor_grab(CursorGrabMode::Confined);
                }
            } else {
                let _ = win.set_cursor_grab(CursorGrabMode::None);
            }
        }
        self.reconcile_cursor();
        tracing::info!("cursor grab (relative mouse) = {want}");
    }

    /// Send a release for every key/pointer-button with an outstanding press
    /// ([`Self::held_keys`]/[`Self::held_buttons`]), then clear both. Called on
    /// focus loss so an alt-tab can never strand a key/button held down on the
    /// host's virtual input.
    fn release_all_held(&mut self) {
        let keys: Vec<u32> = self.held_keys.drain().collect();
        for scancode in keys {
            self.send_to_active(core_proto::InputEvent::Key {
                scancode,
                pressed: false,
                mods: core_proto::Modifiers::default(),
            });
        }
        let buttons: Vec<u8> = self.held_buttons.drain().collect();
        for button in buttons {
            self.send_to_active(core_proto::InputEvent::PointerButton {
                button,
                pressed: false,
            });
        }
    }

    /// Hide the local OS cursor when the host's own cursor is the one that
    /// matters: while grabbed (relative/FPS), or while forwarding absolute
    /// pointer motion over a source cell. Otherwise both the local cursor and the
    /// host cursor rendered into the video show at once (a double pointer). Stays
    /// visible over the client's own chrome, when capture is off, and in view-only
    /// mode. Cheap and idempotent; call on any state transition.
    fn reconcile_cursor(&self) {
        if let Some(win) = &self.window {
            let hide = self.grabbed
                || (self.capture_active()
                    && self.cfg.input.forward_mouse
                    && self.cursor_cell.is_some());
            win.set_cursor_visible(!hide);
        }
    }

    fn handle_pointer_move(&mut self) {
        if self.grabbed {
            // Grabbed: relative motion flows via `device_event` → PointerRel; the
            // (edge-pinned, on the Confined fallback) absolute cursor is ignored.
            return;
        }
        if !self.capture_active() || !self.cfg.input.forward_mouse {
            return;
        }
        let Some((cx, cy)) = self.cursor else { return };
        let Some(idx) = self.cursor_cell else { return };
        let Some(cell) = self.cells.iter().find(|c| c.source == idx) else {
            return;
        };
        // Cell-local position in source's letterboxed rectangle, normalised 0..=1.
        let nx = ((cx - cell.x) / cell.w).clamp(0.0, 1.0);
        let ny = ((cy - cell.y) / cell.h).clamp(0.0, 1.0);
        self.send_to_active(core_proto::InputEvent::PointerAbs { x: nx, y: ny });
    }

    fn handle_mouse_button(
        &mut self,
        state: winit::event::ElementState,
        button: winit::event::MouseButton,
    ) {
        if !self.capture_active() {
            return;
        }
        if let Some(ev) = crate::input::classify_mouse_button(&self.cfg.input, true, button, state)
        {
            // Track held buttons so a focus loss releases them (stuck-button fix).
            if let core_proto::InputEvent::PointerButton { button, pressed } = ev {
                if pressed {
                    self.held_buttons.insert(button);
                } else {
                    self.held_buttons.remove(&button);
                }
            }
            self.send_to_active(ev);
        }
    }

    fn handle_mouse_wheel(&mut self, delta: winit::event::MouseScrollDelta) {
        if !self.capture_active() || !self.cfg.input.forward_scroll {
            return;
        }
        let (dx, dy) = match delta {
            winit::event::MouseScrollDelta::LineDelta(x, y) => (x, y),
            winit::event::MouseScrollDelta::PixelDelta(p) => {
                // ~1 line per ~120 px on Windows / typical hi-DPI.
                (p.x as f32 / 120.0, p.y as f32 / 120.0)
            }
        };
        self.send_to_active(core_proto::InputEvent::Scroll { dx, dy });
    }

    /// The active cell's highlight: `(rect [x,y,w,h] px, sRGB colour, thickness
    /// px)`, or `None` when highlighting is off (`highlight_px == 0`) or no cell
    /// is capturing input.
    fn active_highlight(&self) -> Option<([f32; 4], [f32; 3], f32)> {
        // Always off for a single stream (one cell) — the border only disambiguates
        // which cell has input focus in a multi-source grid.
        if self.cfg.highlight_px == 0 || self.cells.len() <= 1 || !self.capture_active() {
            return None;
        }
        // Mirror send_to_active: the input target is the hovered cell, else 0.
        let idx = self.cursor_cell.unwrap_or(0);
        let c = self.cells.iter().find(|c| c.source == idx)?;
        Some((
            [c.x, c.y, c.w, c.h],
            parse_hex_color(&self.cfg.highlight_color),
            self.cfg.highlight_px as f32,
        ))
    }

    fn render(&mut self) {
        if self.suspended {
            return;
        }
        // Compute the highlight first — needs &self, before borrowing gpu mut.
        let glow = self.active_highlight();
        let Some(gpu) = &mut self.gpu else { return };
        for (i, s) in self.sources.iter().enumerate() {
            if let Some(frame) = s.queue.take(!s.pace.in_order()) {
                // Present stage of the live latency benchmark: time from this
                // frame becoming available (decoder output) until it's uploaded
                // for this present. One relaxed-atomic EMA fold; no-op when no
                // bench is running (Pharos sources only carry taps).
                if let Some(taps) = &s.taps {
                    taps.record_present_ms(frame.produced.elapsed().as_secs_f32() * 1000.0);
                }
                gpu.upload(i, &frame);
            }
        }
        match gpu.present(&self.cells, glow) {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let (w, h) = (gpu.config.width, gpu.config.height);
                gpu.resize(w, h);
            }
            Err(e) => tracing::error!("present: {e:?}"),
        }
        for (i, s) in self.sources.iter_mut().enumerate() {
            s.presents += 1;
            let dt = s.win_start.elapsed();
            if dt.as_secs() >= 1 {
                tracing::info!(
                    "source[{i}] ({}) present cadence: {:.1} fps ({:?})",
                    s.name,
                    s.presents as f64 / dt.as_secs_f64(),
                    s.pace
                );
                s.presents = 0;
                s.win_start = Instant::now();
            }
        }
    }
}

/// Resolve the ffmpeg executable. On Windows we also check the WinGet "Links"
/// dir directly, because `winget install Gyan.FFmpeg` adds its alias dir to
/// the user PATH but already-running shells (and processes spawned by them
/// like Explorer's children) don't see the new PATH until next login.
pub fn ffmpeg_program() -> std::ffi::OsString {
    #[cfg(target_os = "windows")]
    {
        if let Some(la) = std::env::var_os("LOCALAPPDATA") {
            let cand = std::path::Path::new(&la)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("ffmpeg.exe");
            if cand.exists() {
                return cand.into_os_string();
            }
        }
    }
    std::ffi::OsString::from("ffmpeg")
}

/// Back-channel into a source's input-send task (the App forwards classified
/// `WindowEvent`s here).
type InputSender = tokio::sync::mpsc::UnboundedSender<core_proto::InputEvent>;

/// What a spawned source hands back: its input back-channel and (Pharos QUIC
/// sources only) the live latency-benchmark taps to stash on the `Source`.
type SpawnedSource = (Option<InputSender>, Option<Arc<transport::LatencyTaps>>);

/// The HDR→SDR tonemap `-vf` chain: 10-bit HDR10 (BT.2020/PQ) → 8-bit SDR
/// (BT.709), via zscale/tonemap (zimg is in the standard ffmpeg builds). Output is
/// NV12 so it drops straight into the renderer's existing 8-bit path. Run ONLY when
/// the stream is truly HDR: tonemapping an SDR source fails zscale with `no path
/// between colorspaces` and blacks the window (issue #16).
const HDR_TONEMAP_VF: &str =
    "zscale=t=linear:npl=203,tonemap=hable,zscale=p=bt709:t=bt709:m=bt709:r=tv,format=nv12";

/// The decode ffmpeg's output arrangement — what happens to an HDR stream
/// between the decoder and the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeChain {
    /// Plain 8-bit NV12 decode, no tonemap. The only chain an SDR stream may
    /// carry (issue #16).
    Plain,
    /// CPU HDR→SDR tonemap: the zscale/zimg `-vf` chain, NV12 out. Correct but
    /// slow — tops out ~19 fps at 4K10 (issue #15) — so it is only the
    /// fallback for devices without 16-bit GPU textures (GLES/ARM).
    TonemapCpu,
    /// GPU tonemap: decode straight to 10-bit P010 and let the renderer's HDR
    /// fragment shader do PQ→hable→BT.709 (issue #15). No CPU filtering at
    /// all, so the decoder's full rate reaches the screen.
    TonemapGpu,
}

impl DecodeChain {
    /// Bytes per raw sample on ffmpeg's stdout: P010 is 16-bit, NV12 8-bit.
    fn bytes_per_sample(self) -> usize {
        match self {
            DecodeChain::TonemapGpu => 2,
            _ => 1,
        }
    }
    /// Whether this chain treats the incoming stream as HDR.
    fn is_tonemap(self) -> bool {
        self != DecodeChain::Plain
    }
    /// Raw frame size on ffmpeg's stdout for a `w × h` 4:2:0 frame.
    fn frame_bytes(self, w: u32, h: u32) -> usize {
        (w as usize) * (h as usize) * 3 / 2 * self.bytes_per_sample()
    }
    fn label(self) -> &'static str {
        match self {
            DecodeChain::Plain => "plain",
            DecodeChain::TonemapCpu => "tonemap-cpu",
            DecodeChain::TonemapGpu => "tonemap-gpu",
        }
    }
}

/// Decide whether the decode chain should treat the stream as HDR at all.
///
/// `wants_hdr` is the client's CAPABILITY opt-in (`TRIREME_HDR`): it asks the host
/// for a 10-bit stream, but does NOT by itself force the tonemap. `announced` is the
/// host's `DG_STREAM_HDR` colorimetry: `Some(true)` = HDR, `Some(false)` = SDR,
/// `None` = not yet learned.
///
/// The tonemap runs only when we opted in AND the stream is confirmed HDR. When the
/// announce is SDR or not-yet-known we default to PLAIN DECODE: forcing the tonemap
/// onto an SDR stream is the crash (zscale `no path between colorspaces` → black
/// window), whereas plain-decoding a genuinely-HDR stream merely looks washed-out
/// and self-corrects on the next rebuild once the announce lands. So the safe
/// default is always plain decode (issue #16).
fn should_tonemap_hdr(wants_hdr: bool, announced: Option<bool>) -> bool {
    wants_hdr && announced == Some(true)
}

/// What the decode reader should do about the current ffmpeg chain's colorimetry
/// (issue #16). The reader owns the ffmpeg child and can restart it IN PLACE on the
/// same QUIC connection, so a colorimetry flip never reconnects (no host preempt, no
/// storm). This is the pure decision it makes each time it needs to (re)evaluate:
///
/// - `wants_hdr`: the client's `TRIREME_HDR` capability opt-in.
/// - `chain_tonemap`: whether the CURRENTLY RUNNING ffmpeg chain has the HDR→SDR
///   tonemap `-vf` inserted.
/// - `announced`: the host's `DG_STREAM_HDR` colorimetry (`None` = not-yet-known).
/// - `ffmpeg_alive`: whether the current decode ffmpeg is still emitting frames.
///   When it has died after producing nothing on an HDR-built chain, that is itself
///   evidence the stream is really SDR (zscale `no path between colorspaces`), so we
///   force a rebuild to plain even if the announce hasn't (yet) said SDR.
///
/// The rule is: rebuild only when the chain we WANT (`should_tonemap_hdr`) actually
/// differs from the one we HAVE, or when a tonemap chain has died on us (the
/// SDR-masquerading-as-HDR case). Otherwise keep the chain. Because a rebuild always
/// moves the chain to exactly what the (stable) stream needs, re-evaluating after the
/// rebuild returns `Keep`: the decision is a fixed point, so it cannot loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainAction {
    /// The chain matches the stream; do nothing.
    Keep,
    /// Restart ffmpeg as the given chain.
    Rebuild(DecodeChain),
}

/// `tonemap` is which HDR chain THIS machine builds when the stream is
/// confirmed HDR ([`tonemap_chain`]: GPU when 16-bit textures exist, else the
/// CPU zscale fallback). Passed in, not read from the global, so the decision
/// stays pure and unit-testable.
fn chain_action(
    wants_hdr: bool,
    chain: DecodeChain,
    announced: Option<bool>,
    ffmpeg_alive: bool,
    tonemap: DecodeChain,
) -> ChainAction {
    // A never-opted-in (SDR) client must NEVER carry a tonemap chain — forcing the
    // tonemap onto an SDR stream is the original black-window crash (#16). This is the
    // one case where we rebuild a still-alive chain: back to plain, unconditionally.
    if !wants_hdr {
        return if chain.is_tonemap() {
            ChainAction::Rebuild(DecodeChain::Plain)
        } else {
            ChainAction::Keep
        };
    }
    // From here the client opted into HDR. We rebuild only on POSITIVE contrary
    // evidence, never merely because the announce hasn't landed — otherwise the
    // request-based optimistic tonemap chain would be torn down on the first idle
    // iterations and rebuilt when the HDR announce arrived, i.e. a spurious flip on the
    // happy path.
    //
    // A tonemap chain that DIED without ever confirming HDR is such evidence: the
    // stream was really SDR, zscale had `no path between colorspaces`, and ffmpeg
    // exited (the request/stream mismatch — BUG 3). Force it back to plain. (The GPU
    // chain converts SDR input without dying, so for it this arm is just belt and
    // braces; the announce arm below is what corrects a wrong P010 guess.)
    if !ffmpeg_alive && chain.is_tonemap() && announced != Some(true) {
        return ChainAction::Rebuild(DecodeChain::Plain);
    }
    // With no confirmed announce yet, hold an alive chain as-is (the storm-free
    // branch). Once the announce lands, `should_tonemap_hdr` is the canonical target
    // chain for the (now steady) stream; rebuild toward it if we differ, else keep.
    // This is a FIXED POINT: after the rebuild the chain equals the target, so a
    // re-evaluation returns Keep and the reader cannot loop.
    let Some(_) = announced else {
        return ChainAction::Keep;
    };
    let target = if should_tonemap_hdr(wants_hdr, announced) {
        tonemap
    } else {
        DecodeChain::Plain
    };
    if chain != target {
        ChainAction::Rebuild(target)
    } else {
        ChainAction::Keep
    }
}

/// The take/put reinsertion rule for the on_frame writer (issue #16 deadlock fix).
/// After writing an AU with the stdin Mutex RELEASED, the writer wants to put the
/// ChildStdin back, but only if the reader's rebuild path hasn't installed a fresh
/// child's stdin in the meantime. `slot_empty` is whether the shared slot is `None`
/// at re-lock time: put back iff it is still empty, otherwise drop our now-stale stdin
/// so we never clobber the new chain the reader just swapped in. Pulled out as a pure
/// predicate purely so the race decision is unit-testable.
fn reinsert_stdin(slot_empty: bool) -> bool {
    slot_empty
}

/// `StreamCodec` → `AtomicU8` encoding for the sniffed-codec cell (0 = unknown).
fn codec_to_u8(c: media_decode::StreamCodec) -> u8 {
    match c {
        media_decode::StreamCodec::Hevc => 1,
        media_decode::StreamCodec::Av1 => 2,
        media_decode::StreamCodec::H264 => 3,
        media_decode::StreamCodec::Vvc => 4,
    }
}

/// Inverse of [`codec_to_u8`]; `None` for 0 (codec not yet sniffed).
fn codec_from_u8(v: u8) -> Option<media_decode::StreamCodec> {
    Some(match v {
        1 => media_decode::StreamCodec::Hevc,
        2 => media_decode::StreamCodec::Av1,
        3 => media_decode::StreamCodec::H264,
        4 => media_decode::StreamCodec::Vvc,
        _ => return None,
    })
}

/// Build the full ffmpeg argument vector for the compressed → raw decode as the
/// given chain: plain NV12, CPU-tonemapped NV12 (the zscale `-vf`), or P010
/// passthrough for the GPU tonemap (no filtering — the shader does the color
/// transform). `codec` is the stream's codec (first-AU sniff or TRIREME_CODEC
/// override — see the reader's codec wait). Factored out of [`spawn_decode`] so
/// the reader thread can rebuild the child IN PLACE with another chain on a
/// mid-session colorimetry flip (issue #16), reusing the exact same
/// decoder/codec/loglevel setup rather than reconnecting.
fn decode_ffmpeg_args(chain: DecodeChain, codec: media_decode::StreamCodec) -> Vec<String> {
    let ff_debug = std::env::var_os("TRIREME_FFMPEG_DEBUG").is_some();
    let lead: &[&str] = if ff_debug {
        &["-hide_banner", "-loglevel", "info", "-stats"]
    } else {
        &["-hide_banner", "-loglevel", "warning"]
    };
    let mut args: Vec<String> = lead.iter().map(|s| s.to_string()).collect();
    args.extend(media_decode::input_args_for(codec));
    args.push("-f".into());
    args.push(codec.ffmpeg_format());
    let tail: &[&str] = match chain {
        DecodeChain::Plain => &[
            "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt", "nv12", "pipe:1",
        ],
        DecodeChain::TonemapCpu => &[
            "-i",
            "pipe:0",
            "-vf",
            HDR_TONEMAP_VF,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "pipe:1",
        ],
        // 10-bit HEVC decodes natively to P010; no `-vf` means no zimg on the
        // CPU at all — the renderer's HDR shader takes it from here.
        DecodeChain::TonemapGpu => &[
            "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt", "p010le", "pipe:1",
        ],
    };
    args.extend(tail.iter().map(|s| s.to_string()));
    args
}

/// Spawn the decode ffmpeg for the given chain and return the child plus its stdin
/// and stdout handles. The reader thread calls this both for the initial chain and
/// for every in-place rebuild on a colorimetry flip (issue #16); each rebuild reaps
/// nothing here (the caller kills-and-reaps the old child first), it just swaps in a
/// freshly-spawned child on the SAME QUIC connection.
fn spawn_decode_ffmpeg(
    chain: DecodeChain,
    codec: media_decode::StreamCodec,
) -> Result<(
    std::process::Child,
    std::process::ChildStdin,
    std::process::ChildStdout,
)> {
    let args = decode_ffmpeg_args(chain, codec);
    let mut ff = Command::new(ffmpeg_program())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn ffmpeg (video decode)")?;
    let stdin = ff.stdin.take().context("ffmpeg stdin")?;
    let stdout = ff.stdout.take().context("ffmpeg stdout")?;
    // Fatten the raw-frame pipes (Linux): a 4K P010 frame is ~25MB and the
    // default 64KB pipe costs ~380 read syscalls per frame in the reader hot
    // loop — a real slice of the ~52fps reader ceiling that backpressured the
    // whole HDR path (2026-07-09). 1MB is the unprivileged max
    // (/proc/sys/fs/pipe-max-size). Best-effort: on failure the default size
    // still works, just slower.
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        for fd in [stdin.as_raw_fd(), stdout.as_raw_fd()] {
            unsafe {
                libc::fcntl(fd, libc::F_SETPIPE_SZ, 1024 * 1024);
            }
        }
    }
    Ok((ff, stdin, stdout))
}

/// Spawn the right pipeline for a source and return its input back-channel.
/// Pharos sources (the default) get a QUIC/HEVC decode pipeline; VNC and SPICE
/// sources decode client-side and bridge full RGBA frames through a shared
/// ffmpeg RGBA→NV12 scaler ([`spawn_rgba_decode`]). **All** return a live
/// `InputEvent` sender (VNC/SPICE translate it to native pointer/key messages)
/// and feed the same NV12 frame queue, so the renderer, input routing, and
/// reconnect machinery are identical regardless of kind.
#[allow(clippy::too_many_arguments)]
fn spawn_source(
    idx: usize,
    addr: SocketAddr,
    expected_pubkey: Option<[u8; 32]>,
    width: u32,
    height: u32,
    frames: Arc<FrameQueue>,
    proxy: EventLoopProxy<UserEvent>,
    client_identity: Arc<core_crypto::Identity>,
    kind: SourceKind,
    password: Option<String>,
) -> Result<SpawnedSource> {
    let (tx, taps) = match kind {
        SourceKind::Pharos => {
            let (tx, taps) = spawn_decode(
                idx,
                addr,
                expected_pubkey,
                width,
                height,
                frames,
                proxy,
                client_identity,
            )?;
            (tx, Some(taps))
        }
        // VNC/SPICE decode client-side (no HEVC AU stream), so the live decode/
        // present benchmark — which profiles the Pharos QUIC path — has no taps.
        SourceKind::Vnc => (
            spawn_rgba_decode(idx, "vnc", frames, proxy, move |rx, mut sink| {
                crate::vncsrc::run(addr, password.as_deref(), Some(rx), move |vw, vh, px| {
                    sink.write(vw, vh, px)
                })
            })?,
            None,
        ),
        SourceKind::Spice => (
            spawn_spice_source(idx, addr, frames, proxy, password)?,
            None,
        ),
    };
    Ok((Some(tx), taps))
}

/// Start a native SPICE source (`protocol = "spice"`). Compiled only with the
/// `spice` feature (which links `libspice-client-glib-2.0` via `media-spice`).
#[cfg(feature = "spice")]
fn spawn_spice_source(
    idx: usize,
    addr: SocketAddr,
    frames: Arc<FrameQueue>,
    proxy: EventLoopProxy<UserEvent>,
    password: Option<String>,
) -> Result<tokio::sync::mpsc::UnboundedSender<core_proto::InputEvent>> {
    spawn_rgba_decode(idx, "spice", frames, proxy, move |rx, mut sink| {
        media_spice::run(addr, password.as_deref(), Some(rx), move |vw, vh, px| {
            sink.write(vw, vh, px)
        })
    })
}

/// Without the `spice` feature, a `protocol = "spice"` source fails with a clear
/// message instead of silently doing nothing.
#[cfg(not(feature = "spice"))]
fn spawn_spice_source(
    idx: usize,
    _addr: SocketAddr,
    _frames: Arc<FrameQueue>,
    _proxy: EventLoopProxy<UserEvent>,
    _password: Option<String>,
) -> Result<tokio::sync::mpsc::UnboundedSender<core_proto::InputEvent>> {
    anyhow::bail!(
        "source[{idx}] uses protocol=\"spice\" but this Trireme build has no SPICE \
         support — rebuild with `--features spice` (needs the spice-gtk library)"
    )
}

/// Converts full RGBA frames from a native source (VNC/SPICE) into the cell's
/// NV12 by piping them through an ffmpeg scaler. The scaler is sized to the
/// source's native resolution, which isn't known until the first frame — so it's
/// spawned lazily, and **respawned whenever the source resolution changes**
/// (a SPICE guest changes mode several times during boot; VNC ends the run on
/// resize, so for it this only fires once). Without respawn, post-resize frames
/// would be fed to an ffmpeg still configured for the old width → a sheared,
/// wrong-stride picture. One per source thread; not `Sync`.
struct RgbaToNv12 {
    idx: usize,
    /// Short tag for logs/thread names (`"vnc"` / `"spice"`).
    label: &'static str,
    frames: Arc<FrameQueue>,
    proxy: EventLoopProxy<UserEvent>,
    ff_stdin: Option<std::process::ChildStdin>,
    /// Native source size the current scaler is configured for; respawn on change.
    src_dims: (u16, u16),
    scaler_failed: bool,
}

impl RgbaToNv12 {
    /// Feed one native-resolution RGBA frame (`vw × vh`) into the scaler. We
    /// scale to the source's **native** resolution (only forced to even dims for
    /// NV12), not a fixed cell size — so each cell is as sharp as the guest
    /// renders, and the GPU does the final fit to the on-screen cell. The scaler
    /// is respawned whenever the native size changes (e.g. a guest switching
    /// video mode during boot); otherwise stale frames would feed an ffmpeg
    /// configured for the old width → a sheared picture.
    fn write(&mut self, vw: u16, vh: u16, rgba: &[u8]) {
        if self.ff_stdin.is_some() && (vw, vh) != self.src_dims {
            tracing::info!(
                "source[{}] {} resized {}x{} → {vw}x{vh}; respawning scaler",
                self.idx,
                self.label,
                self.src_dims.0,
                self.src_dims.1
            );
            self.ff_stdin = None;
        }
        if self.ff_stdin.is_none() && !self.scaler_failed {
            // NV12 needs even dimensions; render at the native size, evened down.
            let (ow, oh) = ((vw & !1) as u32, (vh & !1) as u32);
            match spawn_rgba_ffmpeg(
                self.idx,
                self.label,
                vw,
                vh,
                ow,
                oh,
                self.frames.clone(),
                self.proxy.clone(),
            ) {
                Ok(s) => {
                    self.ff_stdin = Some(s);
                    self.src_dims = (vw, vh);
                    // Tell the player the source's native size so the cell
                    // letterboxes to the right aspect.
                    let _ = self
                        .proxy
                        .send_event(UserEvent::SourceSize(self.idx, ow, oh));
                }
                Err(e) => {
                    tracing::error!(
                        "source[{}] {} scaler spawn failed: {e:?}",
                        self.idx,
                        self.label
                    );
                    self.scaler_failed = true;
                }
            }
        }
        if let Some(stdin) = self.ff_stdin.as_mut() {
            let _ = stdin.write_all(rgba);
        }
    }
}

/// Shared machinery for the RGBA-emitting sources (VNC, SPICE). A worker thread
/// runs `run_source` — a native client (RFB / spice-glib) that emits full RGBA
/// frames into the supplied [`RgbaToNv12`] sink and drains the input channel.
/// When the source returns (clean close, resize, or drop) the thread fires
/// `StreamEnded`, which the App turns into a reconnect after a backoff — the
/// same path a Pharos stream takes. Returns the input sender.
fn spawn_rgba_decode<R>(
    idx: usize,
    label: &'static str,
    frames: Arc<FrameQueue>,
    proxy: EventLoopProxy<UserEvent>,
    run_source: R,
) -> Result<tokio::sync::mpsc::UnboundedSender<core_proto::InputEvent>>
where
    R: FnOnce(
            tokio::sync::mpsc::UnboundedReceiver<core_proto::InputEvent>,
            RgbaToNv12,
        ) -> Result<()>
        + Send
        + 'static,
{
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::InputEvent>();
    let proxy_end = proxy.clone();
    std::thread::Builder::new()
        .name(format!("trireme-{label}-{idx}"))
        .spawn(move || {
            let sink = RgbaToNv12 {
                idx,
                label,
                frames,
                proxy,
                ff_stdin: None,
                src_dims: (0, 0),
                scaler_failed: false,
            };
            // When `run_source` returns, the sink (and its ffmpeg stdin) drops,
            // closing the scaler + its reader thread.
            match run_source(input_rx, sink) {
                Ok(()) => tracing::info!("source[{idx}] {label} source ended (resize/close)"),
                Err(e) => tracing::warn!("source[{idx}] {label} source dropped: {e:?}"),
            }
            let _ = proxy_end.send_event(UserEvent::StreamEnded(idx));
        })?;
    Ok(input_tx)
}

/// Spawn the RGBA→NV12 scaler for a native source (VNC/SPICE): ffmpeg reads raw
/// RGBA frames at the source's native `src_w × src_h` on stdin, scales to the
/// cell's `width × height`, and emits NV12 on stdout. A reader thread drains NV12
/// frames into the shared queue exactly like the HEVC path, so the renderer is
/// unchanged. `label` tags logs/threads. Returns ffmpeg's stdin for the caller
/// to write RGBA frames into.
#[allow(clippy::too_many_arguments)]
fn spawn_rgba_ffmpeg(
    idx: usize,
    label: &'static str,
    src_w: u16,
    src_h: u16,
    width: u32,
    height: u32,
    frames: Arc<FrameQueue>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<std::process::ChildStdin> {
    let ff_debug = std::env::var_os("TRIREME_FFMPEG_DEBUG").is_some();
    let mut args: Vec<String> = if ff_debug {
        ["-hide_banner", "-loglevel", "info", "-stats"]
    } else {
        ["-hide_banner", "-loglevel", "warning", ""]
    }
    .iter()
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string())
    .collect();
    args.extend(
        [
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{src_w}x{src_h}"),
            "-i",
            "pipe:0",
            // Fit the desktop into the cell preserving aspect (a 4:3 console
            // in a 16:9 cell gets black bars, never stretched), then pad to
            // exactly width×height so the NV12 frame always matches the texture.
            "-vf",
            &format!(
                "scale={width}:{height}:flags=bilinear:force_original_aspect_ratio=decrease\
                 :force_divisible_by=2,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2"
            ),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "pipe:1",
        ]
        .into_iter()
        .map(str::to_string),
    );
    let mut ff = Command::new(ffmpeg_program())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn ffmpeg (rgba→nv12)")?;
    let stdin = ff.stdin.take().context("ffmpeg stdin")?;
    let mut stdout = ff.stdout.take().context("ffmpeg stdout")?;
    let frame_bytes = (width as usize) * (height as usize) * 3 / 2;

    let frames_r = frames.clone();
    let proxy_r = proxy.clone();
    std::thread::Builder::new()
        .name(format!("trireme-{label}-raw-{idx}"))
        .spawn(move || {
            let mut buf = vec![0u8; frame_bytes];
            let mut count: u64 = 0;
            while stdout.read_exact(&mut buf).is_ok() {
                if count == 0 {
                    tracing::info!("source[{idx}] first {label} frame scaled ({width}x{height})");
                }
                count += 1;
                frames_r.push(Frame {
                    data: buf.clone(),
                    w: width,
                    h: height,
                    hdr: false,
                    produced: Instant::now(),
                });
                let _ = proxy_r.send_event(UserEvent::FrameReady(idx));
            }
            tracing::info!("source[{idx}] {label} scaler reader exited after {count} frames");
        })?;
    std::thread::spawn(move || {
        let _ = ff.wait();
    });
    Ok(stdin)
}

/// Spawn one source's decode pipeline: ffmpeg HW-decode subprocess (the decoder
/// is chosen at runtime by `media_decode` — NVDEC / Quick Sync / Rockchip MPP /
/// V4L2 stateless / software) + a network thread feeding it AUs and a reader
/// thread draining NV12 frames into the queue.
/// Returns a sender for InputEvents — the App's WindowEvent handler pushes
/// classified events into it, the QUIC client task forwards them to Pharos.
#[allow(clippy::too_many_arguments)]
fn spawn_decode(
    idx: usize,
    addr: SocketAddr,
    expected_pubkey: Option<[u8; 32]>,
    width: u32,
    height: u32,
    frames: Arc<FrameQueue>,
    proxy: EventLoopProxy<UserEvent>,
    client_identity: Arc<core_crypto::Identity>,
) -> Result<(InputSender, Arc<transport::LatencyTaps>)> {
    // The HW decoder is chosen at runtime by `media_decode` (probe + test-decode,
    // logged once; override with `TRIREME_DECODER`). Hard-won decoder facts the
    // probe relies on:
    //   - Linux dev rig (RTX 4070): NVDEC via `hevc_cuvid`, now with
    //     `-flags low_delay` (added in `media_decode`'s LOW_DELAY_SAFE set). The
    //     old "green-frame bug" was specifically `-fflags nobuffer`, NOT
    //     `low_delay`: on ffmpeg n8.1.2 a 60-frame decode is MD5-identical to
    //     baseline with `low_delay` (corruption only appears once `nobuffer` is
    //     added), while it ~halves the decode-side in-flight latency
    //     (99.8→51.4ms here — NVDEC holds fewer pictures before output). Probe
    //     limits (`-probesize/-analyzeduration`) stay OFF (they starve the QSV
    //     SPS parser → `Picture size 0x0`). NVDEC init is transiently flaky under
    //     GPU contention, which is why the probe retries + has a listing-based net.
    //   - Windows (Intel iGPU): Quick Sync via the standalone `hevc_qsv`
    //     decoder. Confirmed on the T14 (i5-1235U, Iris Xe, oneVPL runtime 2.15):
    //     decodes 1080p120 → RGBA at ~160 fps, comfortably above the 120 fps
    //     line rate, so the QUIC/pipe backlog that caused the ~7 s latency with
    //     software decode can no longer grow.
    //   - ARM SBCs: `hevc_rkmpp` (RK3588) or `-hwaccel drm` (Pi 5 V4L2 stateless);
    //     `-pix_fmt nv12` below downloads to system memory so the render path is
    //     unchanged (zero-copy DMA-BUF is a separate, future optimization).
    //
    // Earlier QSV dead-ends on the T14, for the record:
    //   - The `Picture size 0x0` once attributed to a "missing Media SDK runtime"
    //     was a red herring — oneVPL is present and a bare `hevc_qsv` decode
    //     works. The 0x0 came from `-probesize 32 -analyzeduration 0` starving
    //     the SPS parser; do NOT add probe limits.
    //   - `-hwaccel qsv -hwaccel_output_format nv12` (auto GPU→sys download)
    //     fails with `Error synchronizing the operation: -3`; the standalone
    //     decoder, which emits system-memory NV12 itself, sidesteps it.
    //   - `-hwaccel d3d11va` + `-fflags nobuffer -flags low_delay` discards the
    //     opening VPS/SPS/PPS+IDR, then loops `Could not find ref with POC …`.
    // Output is NV12 (the decoder's native format), converted to RGB in the
    // fragment shader — so there's no swscale on the CPU at all. Measured on the
    // T14: decode→NV12 sustains ~520fps vs ~260fps for decode→RGBA, and utime
    // drops ~7×. Remaining latency lever if ever needed: `-async_depth 1`.
    // `TRIREME_FFMPEG_DEBUG=1` surfaces the decoder's own truth: stream mapping,
    // pixel format, and a periodic `frame= N fps=…` line on stderr (ffmpeg
    // flushes stats, so they survive even a force-kill). Indispensable for
    // confirming HW decode actually emits frames at line rate.
    // The HW decoder is chosen at runtime by `media_decode` (probe + test-decode,
    // logged once; override with `TRIREME_DECODER`). Output is forced to
    // system-memory NV12, so whichever decoder wins, its frames arrive in exactly the
    // format the renderer's R8+Rg8 textures and BT.709 shader already consume — the
    // rest of the pipeline is decoder-agnostic. Codec: HEVC by default; AV1 when the
    // host streams it (`TRIREME_CODEC=av1`). The full ffmpeg arg vector (with or
    // without the HDR→SDR tonemap `-vf`) is built by `decode_ffmpeg_args`, so the
    // reader thread can rebuild the child IN PLACE with the other chain on a flip.

    // Live-benchmark latency taps, shared across the threads below. The encoder is
    // IPPP (1 AU in → 1 NV12 out, in order), so a plain FIFO of submit instants
    // matches each decoded frame to the AU that produced it. Also carries the host's
    // DG_STREAM_HDR colorimetry announce, which the reader watches to rebuild the
    // chain. Created here (fresh per spawn), fed by the QUIC thread once it connects.
    let taps = Arc::new(transport::LatencyTaps::new());
    // Submit instants pushed by the on_frame closure (QUIC thread) as it writes
    // each AU to ffmpeg stdin, popped oldest-first by the reader thread when a
    // decoded frame emerges. A tiny deque; never blocks the render path.
    let submit_times: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));

    // HDR (TRIREME_HDR): the flag is a CAPABILITY opt-in — it asks the host for a
    // 10-bit HEVC Main10 (BT.2020/PQ) stream (the `wants_hdr` handshake below). A
    // genuinely-HDR stream must be tonemapped HDR10 → SDR BT.709 — on the GPU when
    // the device can (P010 planes + fragment-shader tonemap, issue #15), else via
    // ffmpeg's zscale chain; an SDR stream must NOT be (zscale fails `no path
    // between colorspaces` and blacks the window — issue #16).
    let wants_hdr = std::env::var_os("TRIREME_HDR").is_some();
    // Which tonemap chain this machine builds when the stream is HDR. Resolved once
    // per spawn: `resumed()` created the GPU (and set the capability) before any
    // source spawns, so this is stable for the reader's whole life.
    let tonemap = tonemap_chain();

    // Initial chain from the CLIENT REQUEST, not a host announce (issue #16). The
    // old code waited up to 500ms for `taps.stream_hdr()` HERE, but nothing feeds the
    // taps until the QUIC thread (spawned further down) connects — so that wait ALWAYS
    // timed out to plain, and against a real HDR host the reader instantly detected a
    // flip and reconnected, which the single-client host preempts → a ~1.5s
    // reconnect/preempt storm. Instead: if we opted into HDR we assume the stream we
    // ASKED for is HDR and tonemap optimistically. The happy path (HDR client + HDR
    // host) then builds the correct chain FIRST → no flip, no storm. If the host
    // actually serves SDR, the reader rebuilds to plain in place (same QUIC
    // connection) the moment ffmpeg dies with 0 frames or the announce says SDR.
    let initial_chain = if wants_hdr {
        tonemap
    } else {
        DecodeChain::Plain
    };
    if wants_hdr {
        tracing::info!(
            "source[{idx}] HDR requested; building {} chain (adapts if the stream is \
             actually SDR)",
            initial_chain.label()
        );
    }
    // The stream's codec, published as a codec_to_u8 value by the QUIC thread's
    // first-AU sniff (0 = not yet known). An explicit TRIREME_CODEC override
    // pre-fills the cell, so the sniff never runs and the env var keeps its old
    // meaning. The reader waits on this before spawning the FIRST decode child —
    // that ordering (sniff, THEN spawn) is what lets the desktop client follow
    // whatever codec the host streams (HEVC/AV1/AVC/VVC) with no configuration,
    // the same way the Android client does. Sniffed once per source: a host
    // codec switch always relaunches the host, which ends this session, and the
    // re-dial re-sniffs.
    let sniffed_codec = Arc::new(std::sync::atomic::AtomicU8::new(
        media_decode::codec_override_from_env().map_or(0, codec_to_u8),
    ));
    // The stdin the on_frame closure writes each AU to. Wrapped so the reader thread
    // can SWAP it for a freshly-rebuilt child's stdin on a colorimetry flip WITHOUT
    // reconnecting: the QUIC connection (and this closure) stays alive across the
    // rebuild, so the host never sees a drop and cannot preempt us. `None` until the
    // reader has learned the codec and spawned the first child, and briefly while it
    // is mid-rebuild; writes in those windows are dropped (the key-AU gate plus the
    // host's IDR re-sync recover the fresh chain).
    let shared_stdin: Arc<Mutex<Option<std::process::ChildStdin>>> = Arc::new(Mutex::new(None));
    // Set by the QUIC thread when the session ends (drop / host refusal / error) so the
    // reader can tell a genuine end-of-session from a colorimetry-rebuild window: once
    // the session is over there is nothing feeding a new decode child, so the reader
    // must EXIT rather than rebuild (which would otherwise spawn an unfed child).
    let quic_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // While set, the on_frame closure DROPS every AU that can't start a fresh
    // decode (no HEVC IRAP / AV1 key frame). A decoder fed mid-GOP emits
    // zero-initialised concealment frames — the green flash of issue #32 — so a
    // freshly spawned child (initial connect AND every colorimetry rebuild) waits
    // for a key AU; the renderer meanwhile keeps presenting the last uploaded
    // frame, turning the host's HDR↔SDR respawn into a brief freeze. Set here for
    // the initial child and by the reader's rebuild path BEFORE it installs the
    // new child's stdin (so the QUIC thread can never slip a mid-GOP AU past it).
    let await_key = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // One-shot client→host IDR request (DG_REQUEST_IDR, flushed within ~100ms by
    // the transport loop). The rebuild path sets it so the wait for a key AU ends
    // in ~1 RTT + encode instead of up to a full host IDR period.
    let request_idr = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let frames_r = frames.clone();
    let proxy_r = proxy.clone();
    let taps_dec = taps.clone();
    let submit_dec = submit_times.clone();
    let stdin_dec = shared_stdin.clone();
    let quic_done_dec = quic_done.clone();
    let await_key_dec = await_key.clone();
    let request_idr_dec = request_idr.clone();
    let sniffed_dec = sniffed_codec.clone();
    std::thread::Builder::new()
        .name(format!("trireme-raw-{idx}"))
        .spawn(move || {
            // Colorimetry the CURRENTLY RUNNING ffmpeg chain was built for. Starts at
            // the request-based initial choice and moves only when THIS thread rebuilds
            // the child, so it always reflects the live arrangement.
            let mut chain = initial_chain;
            // Size the NV12 reader to the host's TRUE geometry. The host announces
            // it over the wire (DG_STREAM_GEOMETRY → stashed in the taps); wait
            // briefly so the very first read is aligned — the host sends it before
            // any AU, and ffmpeg can't emit a frame until it has decoded an IDR
            // anyway, so this resolves in well under the timeout. Falling back to the
            // configured size only happens for hosts that don't announce geometry.
            // NV12 = full-res Y plane (w*h) + half-res interleaved CbCr (w*h/2) =
            // w*h*3/2 bytes; P010 is the same layout at 2 bytes/sample (see
            // `DecodeChain::frame_bytes`). Slicing by the WRONG size shears every
            // frame (the green bug); this is the fix.
            let (mut fw, mut fh) = {
                let mut g = taps_dec.geometry();
                let mut waited_ms = 0u32;
                while g.is_none() && waited_ms < 2000 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    waited_ms += 10;
                    g = taps_dec.geometry();
                }
                g.unwrap_or((width, height))
            };
            if (fw, fh) != (width, height) {
                tracing::info!(
                    "source[{idx}] host geometry {fw}x{fh} (config assumed {width}x{height})"
                );
            }
            let mut buf = vec![0u8; chain.frame_bytes(fw, fh)];
            let mut count: u64 = 0;
            // Frames THIS ffmpeg child has emitted. Reset on every rebuild so a chain
            // that dies without producing anything is recognisable as the
            // wrong-colorimetry case (`chain_action`'s `ffmpeg_alive=false` path).
            let mut frames_this_chain: u64 = 0;
            // The current decode child, held in an Option so a rebuild can `take()` it
            // for reaping without leaving `ff` moved-out for the borrow checker.
            // `None` until the codec wait below spawns the first child.
            let mut ff: Option<std::process::Child> = None;
            // Rebuild the decode ffmpeg IN PLACE with `to_tonemap` (issue #16). KILLS
            // and reaps the old child, spawns the new one, re-points the shared stdin
            // the on_frame closure writes to, and returns its stdout for the reader to
            // switch to. The SAME QUIC connection carries on, so the host never sees a
            // drop and cannot preempt us — a colorimetry flip can therefore never
            // reconnect, which is what makes the old ~1.5s storm impossible. Returns the
            // new (child, stdout) or None if the respawn failed (reader then exits).
            //
            // ORDERING INVARIANT (deadlock fix): the old child is KILLED before we ever
            // reach for the shared stdin Mutex. Here is the cycle that reorder breaks.
            // A single 4K IDR AU can exceed the ~64KB pipe buffer, and while the reader
            // is in this rebuild path it is NOT draining ffmpeg's stdout, so a frame can
            // fill stdout, ffmpeg stops reading stdin, ffmpeg's stdin pipe fills, and the
            // on_frame closure's `write_all` then BLOCKS while still holding the stdin
            // Mutex. If we tried to `lock()` the stdin here first we would wait forever on
            // a guard the writer can't release (it is wedged on a full pipe whose reader,
            // ffmpeg, is itself wedged on a full stdout only WE drain). Killing the old
            // child first (SIGKILL + closing its pipe fds) makes that blocked `write_all`
            // fail with BrokenPipe/EPIPE; it is a `let _ = …` so it returns harmlessly and
            // DROPS the guard, after which our lock below succeeds. Merely setting the
            // Option to None can NOT free a child blocked on its STDOUT, so the kill (not
            // an EOF on stdin) is what actually unwedges everything.
            let rebuild = |old_ff: Option<std::process::Child>, to_chain: DecodeChain| {
                // 1. Kill the old child BEFORE any lock acquisition. This unblocks a
                //    concurrent on_frame `write_all` that is stuck on the old child's full
                //    stdin pipe (it gets EPIPE and releases the guard), and it lets us reap
                //    the child fast below instead of `wait()`-ing on a possibly-wedged live
                //    process. Kill is idempotent-safe: an already-exited child just errors.
                if let Some(mut old) = old_ff {
                    let _ = old.kill();
                    // 2. Reap in place; the child is dead now, so this returns promptly and
                    //    leaves no zombie. (Doing this before the lock also means the guard
                    //    is free by the time we take it in step 3/4.)
                    let _ = old.wait();
                }
                // If the QUIC session has already ended there is nothing to feed a new
                // child — refuse to rebuild so the reader exits instead of spawning an
                // unfed ffmpeg (which would never read a frame). Returning None makes the
                // caller break.
                if quic_done_dec.load(std::sync::atomic::Ordering::Relaxed) {
                    *stdin_dec.lock().unwrap() = None;
                    return None;
                }
                // 3. Now the writer's guard is free, so clearing the shared stdin cannot
                //    block. (The old child is already dead; this just drops the stale
                //    ChildStdin. A few AUs may fall in the None window; the host's ~1×/s
                //    IDR re-sync papers over that on the new chain.)
                *stdin_dec.lock().unwrap() = None;
                // The codec is whatever the sniff/override published — the codec
                // wait below guarantees it is known before the first spawn, and
                // it never changes within a session (a host codec switch always
                // relaunches the host, which ends this session).
                let codec = codec_from_u8(sniffed_dec.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(media_decode::StreamCodec::Hevc);
                match spawn_decode_ffmpeg(to_chain, codec) {
                    Ok((new_ff, new_stdin, new_stdout)) => {
                        // Arm the key-AU gate BEFORE the fresh stdin becomes reachable:
                        // a mid-GOP AU fed to the new child would decode as green
                        // concealment frames (issue #32). Ordering matters — the QUIC
                        // thread takes the stdin under the same lock, so setting the
                        // gate first means no write can precede it.
                        await_key_dec.store(true, std::sync::atomic::Ordering::Relaxed);
                        // 4. Install the new child's stdin under the lock so the next
                        //    on_frame feeds the fresh chain.
                        *stdin_dec.lock().unwrap() = Some(new_stdin);
                        // Ask the host for an on-demand IDR so the gate opens in ~1 RTT
                        // instead of waiting out the periodic IDR (the freeze the user
                        // sees is exactly this window).
                        request_idr_dec.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some((new_ff, new_stdout))
                    }
                    Err(e) => {
                        tracing::error!("source[{idx}] decode-chain rebuild failed: {e:#}");
                        None
                    }
                }
            };
            // Wait for the stream's codec before spawning the FIRST decode child:
            // a TRIREME_CODEC override resolved instantly (the cell is pre-filled);
            // otherwise the QUIC thread publishes the first AU's sniff. No
            // timeout — with no AU there is nothing to decode, and a session that
            // ends first breaks the wait via quic_done. This ordering (learn the
            // codec, THEN pick the demuxer/decoder) is what lets the desktop
            // client follow a host codec switch with zero configuration.
            let codec = loop {
                if let Some(c) =
                    codec_from_u8(sniffed_dec.load(std::sync::atomic::Ordering::Relaxed))
                {
                    break c;
                }
                if quic_done_dec.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(
                        "source[{idx}] decode reader exited (session ended before any AU)"
                    );
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            tracing::info!(
                "source[{idx}] decode chain: {} ({}-colorimetry)",
                codec.label(),
                chain.label()
            );
            let mut stdout = match rebuild(ff.take(), chain) {
                Some((new_ff, new_stdout)) => {
                    ff = Some(new_ff);
                    new_stdout
                }
                None => {
                    tracing::info!("source[{idx}] decode reader exited (no decode chain)");
                    return;
                }
            };
            // Last host-announced cursor-capture state we forwarded to the app (from
            // DG_CURSOR_MODE, stashed in the taps). Starts unset so the FIRST announce
            // — including the connect-time one — is always forwarded; thereafter only a
            // CHANGE fires a UserEvent, so we never spam the event loop each frame.
            let mut last_cursor: Option<bool> = None;
            loop {
                // Cursor-mode follow: mirror the host game's own cursor-capture state
                // into the client's OS cursor grab. On a CHANGE (or the first announce)
                // hand it to the app via the proxy; `user_event` reconciles it into the
                // real grab (game CAPTURED → locked+relative, FREE → released+absolute).
                // Only on change — never every frame.
                let cur = taps_dec.cursor_captured();
                if cur.is_some() && cur != last_cursor {
                    last_cursor = cur;
                    if let Some(captured) = cur {
                        let _ = proxy_r.send_event(UserEvent::CursorMode(captured));
                    }
                }
                // Colorimetry flip (issue #16): the host toggled HDR↔SDR (e.g. a wgc-zc
                // pipeline respawn as the other colorimetry) and re-announced via
                // DG_STREAM_HDR, OR the initial request-based guess was wrong. If the
                // chain we WANT differs from the one running, rebuild it in place. The
                // decision is a fixed point (`chain_action` returns Keep once the chain
                // matches the stream), so this cannot thrash.
                match chain_action(wants_hdr, chain, taps_dec.stream_hdr(), true, tonemap) {
                    ChainAction::Keep => {}
                    ChainAction::Rebuild(to_chain) => {
                        tracing::info!(
                            "source[{idx}] colorimetry: rebuilding decode chain ({} → {})",
                            chain.label(),
                            to_chain.label()
                        );
                        let Some((new_ff, new_stdout)) = rebuild(ff.take(), to_chain) else {
                            break;
                        };
                        ff = Some(new_ff);
                        stdout = new_stdout;
                        chain = to_chain;
                        buf = vec![0u8; chain.frame_bytes(fw, fh)];
                        frames_this_chain = 0;
                        continue;
                    }
                }
                // Re-derive the frame size if the host (re-)announced geometry that
                // differs from what we're slicing at — covers the resolution actuator's
                // live changes, a late/lost connect-time datagram, and a host
                // generation respawn at a new display mode. RESPAWN the decode child
                // rather than resizing `buf` in place: by the time the announce is
                // seen here, the OLD child has usually already emitted frames at the
                // new size into its stdout, so this reader's fixed-size `read_exact`
                // is permanently mid-frame — phase loss no later size fix can heal
                // (the locked, sheared, green-cast screen seen live 2026-07-08 after
                // an HDR toggle bounced the display mode). A fresh child = a fresh
                // pipe = guaranteed frame alignment, and the rebuild arms the key-AU
                // gate + requests an on-demand IDR, so the new size starts clean in
                // ~1 RTT. The renderer reallocates textures when Frame.w/h move.
                if let Some((gw, gh)) = taps_dec.geometry() {
                    if (gw, gh) != (fw, fh) {
                        tracing::info!(
                            "source[{idx}] geometry → {gw}x{gh} (was {fw}x{fh}); respawning \
                             decode chain (phase-safe)"
                        );
                        let Some((new_ff, new_stdout)) = rebuild(ff.take(), chain) else {
                            break;
                        };
                        ff = Some(new_ff);
                        stdout = new_stdout;
                        fw = gw;
                        fh = gh;
                        buf = vec![0u8; chain.frame_bytes(fw, fh)];
                        frames_this_chain = 0;
                        continue;
                    }
                }
                if stdout.read_exact(&mut buf).is_err() {
                    // The current ffmpeg died. If it was a tonemap chain that produced
                    // nothing, the stream was really SDR (zscale `no path between
                    // colorspaces`) — the request/stream mismatch or an HDR→SDR flip we
                    // hadn't yet seen the announce for. `chain_action` with
                    // `ffmpeg_alive=false` folds that into a RebuildToPlain even when the
                    // announce is still unknown, so we rebuild to plain IN PLACE rather
                    // than leaving a permanently black window (issue #16 BUG 3).
                    // Otherwise (a genuine EOF/decoder crash on an already-correct chain)
                    // fall through to the plain break and let the session end normally.
                    let alive = frames_this_chain > 0;
                    match chain_action(wants_hdr, chain, taps_dec.stream_hdr(), alive, tonemap) {
                        ChainAction::Keep => break,
                        ChainAction::Rebuild(to_chain) => {
                            tracing::info!(
                                "source[{idx}] decode ffmpeg exited after {frames_this_chain} \
                                 frames on a {}-colorimetry chain; rebuilding to {}",
                                chain.label(),
                                to_chain.label()
                            );
                            let Some((new_ff, new_stdout)) = rebuild(ff.take(), to_chain) else {
                                break;
                            };
                            ff = Some(new_ff);
                            stdout = new_stdout;
                            chain = to_chain;
                            buf = vec![0u8; chain.frame_bytes(fw, fh)];
                            frames_this_chain = 0;
                            continue;
                        }
                    }
                }
                // Decode stage: this frame corresponds to the oldest AU still
                // outstanding (IPPP, in order). Pop it and fold the elapsed time
                // into the EMA. If the deque is momentarily empty (e.g. decoder
                // emitted before the submit instant landed), skip — never block.
                if let Some(submit) = submit_dec.lock().unwrap().pop_front() {
                    taps_dec.record_decode_ms(submit.elapsed().as_secs_f32() * 1000.0);
                }
                if count == 0 {
                    tracing::info!("source[{idx}] first decoded frame ({fw}x{fh})");
                }
                count += 1;
                frames_this_chain += 1;
                if count.is_multiple_of(120) {
                    tracing::info!("source[{idx}] decoded {count} frames");
                }
                // Hand the buffer over instead of cloning it. A 4K P010 frame is
                // ~25MB; at 60fps the old `buf.clone()` was 1.5GB/s of pure memcpy
                // in the loop that must outrun the decoder — the measured ~52fps
                // reader ceiling that backpressured ffmpeg → the client's QUIC
                // reads → the host's flow-control credit → multi-second host
                // `write_all` stalls (the 2026-07-09 HDR input lag). The
                // replacement Vec comes from mmap'd zero pages (no memset for
                // allocations this size), so this removes the copy outright.
                let data = std::mem::replace(&mut buf, vec![0u8; chain.frame_bytes(fw, fh)]);
                frames_r.push(Frame {
                    data,
                    w: fw,
                    h: fh,
                    hdr: chain == DecodeChain::TonemapGpu,
                    produced: Instant::now(),
                });
                let _ = proxy_r.send_event(UserEvent::FrameReady(idx));
            }
            // Reap whatever child is current when the reader exits (normal EOF; a
            // failed rebuild already `take()`-consumed and reaped its old child).
            if let Some(mut ff) = ff {
                std::thread::spawn(move || {
                    let _ = ff.wait();
                });
            }
            tracing::info!("source[{idx}] decode reader exited after {count} frames");
        })?;

    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::InputEvent>();
    let proxy_n = proxy.clone();
    // Audio: decode Opus datagrams → PCM → pacat (default sink). Best-effort —
    // video plays regardless if playback can't start (or off-Linux).
    let on_audio: Option<Box<dyn FnMut(Vec<u8>) + Send>> = match make_audio_player() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("source[{idx}] audio playback off: {e:#}");
            None
        }
    };
    let taps_quic = taps.clone();
    // The on_frame closure writes each AU to whichever decode child is CURRENTLY
    // installed (the reader swaps this on a colorimetry rebuild — issue #16). During
    // the brief rebuild window it is `None` and the AU is dropped; the host's ~1×/s
    // IDR re-sync recovers the new chain, and no reconnect happens so the connection
    // survives the swap.
    //
    // The blocking `write_all` runs WITHOUT the Mutex held (take/put): we TAKE the
    // ChildStdin out of the Option under the lock, write with the lock RELEASED, then
    // put it back ONLY IF the slot is still empty. This is the second half of the
    // deadlock fix; holding the guard across a full-pipe `write_all` is exactly what
    // let the reader's rebuild wedge on `lock()`. on_frame runs inline on the single
    // threaded QUIC task, so there is never a concurrent on_frame; the take/put pair is
    // therefore atomic in practice and can only race the reader's swap, which we handle
    // by NOT clobbering a stdin the reader installed while our write was in flight.
    let stdin_quic = shared_stdin.clone();
    let await_key_quic = await_key.clone();
    let request_idr_quic = request_idr.clone();
    let sniffed_quic = sniffed_codec.clone();
    std::thread::Builder::new()
        .name(format!("trireme-quic-{idx}"))
        .spawn(move || {
            let res = transport::run_video_client_with_input(
                vec![addr],
                expected_pubkey,
                Some(client_identity.as_ref()),
                |au| {
                    // The codec whose bitstream the key-AU gate parses: the cell
                    // holds the TRIREME_CODEC override or a prior sniff; while it
                    // is still empty (un-overridden session start), sniff each AU
                    // and publish on the first CLASSIFIABLE one — the reader is
                    // waiting on exactly this to spawn the right ffmpeg. A
                    // mid-stream join's first AUs are usually unclassifiable
                    // P-slices (sniff = None): drop those and nudge the host for
                    // an IDR, whose parameter sets classify. Once published it's
                    // one atomic load per AU (the closure is hot).
                    let stream_codec = match codec_from_u8(
                        sniffed_quic.load(std::sync::atomic::Ordering::Relaxed),
                    ) {
                        Some(c) => c,
                        None => match media_decode::sniff_codec(&au) {
                            Some(c) => {
                                sniffed_quic
                                    .store(codec_to_u8(c), std::sync::atomic::Ordering::Relaxed);
                                tracing::info!("source[{idx}] stream codec sniffed: {}", c.label());
                                c
                            }
                            None => {
                                request_idr_quic.store(true, std::sync::atomic::Ordering::Relaxed);
                                return;
                            }
                        },
                    };
                    // TAKE the stdin out under the lock, then release before writing so a
                    // full-pipe `write_all` never blocks the reader's rebuild `lock()`.
                    // The key-AU gate (issue #32) is checked under the SAME guard: the
                    // rebuild path arms it before installing a fresh child's stdin, so
                    // holding the lock across check+take means a mid-GOP AU can never
                    // slip into a decoder that hasn't seen its first IRAP/key frame yet
                    // (that's what decoded to the green concealment frames). Gated AUs
                    // are dropped whole — no submit stamp, no write.
                    let taken = {
                        let mut slot = stdin_quic.lock().unwrap();
                        if await_key_quic.load(std::sync::atomic::Ordering::Relaxed) {
                            if !media_decode::is_keyframe_au(stream_codec, &au) {
                                // Still waiting — keep nudging the host. The transport
                                // tick flushes at most one DG_REQUEST_IDR per ~100ms
                                // and the host coalesces within its min-IDR interval,
                                // so this is self-limiting; without the re-arm, a
                                // single request the host rate-limited away (e.g. it
                                // landed right after the respawn's leading IDR, which
                                // the OLD chain consumed) would leave the gate closed
                                // until the next periodic IDR.
                                request_idr_quic.store(true, std::sync::atomic::Ordering::Relaxed);
                                return;
                            }
                            // A key AU but no decode child to feed yet (the reader
                            // is still in its codec wait / mid-spawn): keep the
                            // gate ARMED so the next key AU opens it for the fresh
                            // child, instead of this one disarming into a `None`
                            // slot and the child then starting mid-GOP (green
                            // concealment frames).
                            if slot.is_none() {
                                request_idr_quic.store(true, std::sync::atomic::Ordering::Relaxed);
                                return;
                            }
                            await_key_quic.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        slot.take()
                    };
                    if let Some(mut stdin) = taken {
                        // Stamp the submit instant just before handing the AU to the
                        // decoder; the reader thread pops it when the matching NV12
                        // frame comes out (decode-stage latency). Only for AUs that
                        // actually reach ffmpeg — a stamped-but-dropped AU would pair
                        // later frames with earlier instants and inflate the EMA.
                        submit_times.lock().unwrap().push_back(Instant::now());
                        let _ = stdin.write_all(&au);
                        // PUT it back only if the reader hasn't installed a new child in
                        // the meantime. If the slot is now Some (rebuild swapped in the
                        // fresh chain), drop this stale stdin instead of clobbering it.
                        // Hold ONE guard across the check AND the set: with two separate
                        // locks a rebuild's swap-in could land between them and be
                        // clobbered by our stale handle (a TOCTOU that would starve the
                        // new child and stall the stream).
                        let mut slot = stdin_quic.lock().unwrap();
                        if reinsert_stdin(slot.is_none()) {
                            *slot = Some(stdin);
                        }
                    }
                },
                Some(input_rx),
                on_audio,
                None,            // desktop player: no gyro calibration telemetry
                None,            // no external cancel; ends with the player
                Some(taps_quic), // feed the host-triggered live latency benchmark
                None,            // desktop player: no app-catalog relay yet
                None,            // desktop player: no app-notice toast relay yet
                None,            // desktop player: no app-launch relay yet
                false, // desktop player: no controller gyro (no IMU on the desktop client)
                // wants_hdr: the CAPABILITY opt-in (TRIREME_HDR). It asks the host for
                // a 10-bit stream; the tonemap `-vf` is decided separately, from the
                // host's DG_STREAM_HDR announce (issue #16). Keep sending it so an
                // HDR-capable host still streams HDR. Off by default (SDR).
                wants_hdr,
                Some(Box::new({
                    // Log link-health edges. The desktop player has no on-screen
                    // overlay facility yet, so this is log-only (the Android client
                    // draws the yellow indicator); recovery itself is host-driven.
                    let mut last = false;
                    move |degraded| {
                        if degraded != last {
                            last = degraded;
                            if degraded {
                                tracing::warn!(
                                    "source[{idx}] host link DEGRADED (dropping frames)"
                                );
                            } else {
                                tracing::info!("source[{idx}] host link recovered");
                            }
                        }
                    }
                })),
                // request_idr: set by the reader's colorimetry rebuild so the fresh
                // decode child gets its key AU in ~1 RTT (issue #32); the transport
                // loop flushes it as DG_REQUEST_IDR within ~100ms.
                Some(request_idr),
                core_proto::StreamMode::default(), // requested_mode: desktop sends Auto for now
                None, // res_pref: desktop player has no resolution selector yet
                None, // bitrate_pref: ditto
                None, // fps_pref: ditto
                None, // transport_hint: desktop transport detection is a follow-up
                None, // on_clipboard: desktop player clipboard sync is a follow-up
                None, // clipboard_rx: ditto
            );
            // Mark the session done BEFORE closing stdin so a reader that wakes on the
            // resulting EOF sees the flag and exits instead of trying to rebuild an
            // unfed decode child (issue #16). Then close stdin so ffmpeg sees EOF and
            // the reader's `read_exact` returns; the reader reaps the child.
            quic_done.store(true, std::sync::atomic::Ordering::Relaxed);
            *stdin_quic.lock().unwrap() = None;
            if let Err(e) = res {
                tracing::error!("source[{idx}] video client: {e:?}");
                // Deliberate host refusals (issue #19) carry stable messages —
                // route them to their own retry policy instead of the ordinary
                // fast reconnect (which would hammer a host that meant "no").
                let msg = format!("{e:#}");
                if msg.contains(transport::UNPAIRED_MSG) {
                    let _ =
                        proxy_n.send_event(UserEvent::StreamRefused(idx, StreamRefusal::Unpaired));
                    return;
                }
                if msg.contains(transport::RESERVED_BY_PRIMARY_MSG) {
                    let _ = proxy_n.send_event(UserEvent::StreamRefused(
                        idx,
                        StreamRefusal::ReservedByPrimary,
                    ));
                    return;
                }
            }
            let _ = proxy_n.send_event(UserEvent::StreamEnded(idx));
        })?;

    Ok((input_tx, taps))
}

/// Spawn a `pacat` playback process and return a callback that decodes each
/// Opus packet (delivered as a QUIC datagram) and writes the PCM to it.
/// Linux/PipeWire only for now — Android plays via MediaCodec, the Windows
/// desktop client's Opus output comes later.
#[cfg(target_os = "linux")]
fn make_audio_player() -> Result<Box<dyn FnMut(Vec<u8>) + Send>> {
    let mut child = Command::new("pacat")
        .args([
            "--format=s16le",
            "--rate=48000",
            "--channels=2",
            "--latency-msec=40",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn pacat (audio playback)")?;
    let mut stdin = child.stdin.take().context("pacat stdin")?;
    // `child` drops when this fn returns; pacat keeps running off `stdin` until
    // it sees EOF (when this callback is dropped at end of session).
    let mut dec = media_audio::OpusDecoder::new()?;
    Ok(Box::new(move |pkt: Vec<u8>| match dec.decode_frame(&pkt) {
        Ok(pcm) => {
            let mut bytes = Vec::with_capacity(pcm.len() * 2);
            for s in pcm {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            let _ = stdin.write_all(&bytes);
        }
        Err(e) => tracing::debug!("opus decode: {e}"),
    }))
}

#[cfg(not(target_os = "linux"))]
fn make_audio_player() -> Result<Box<dyn FnMut(Vec<u8>) + Send>> {
    anyhow::bail!("audio playback not implemented on this platform yet")
}

// --- Entry points -----------------------------------------------------------

/// Legacy CLI form: one source on one address, 1×1 grid.
pub fn run_legacy(addr: SocketAddr, pace: PaceMode) -> Result<()> {
    run(PlayerCfg::single(addr, pace))
}

/// Multi-source player entry point. Validates the config, builds the window +
/// swapchain, and runs the event loop until every stream ends or the window
/// is closed.
pub fn run(cfg: PlayerCfg) -> Result<()> {
    cfg.validate()?;
    let sources: Vec<Source> = cfg.sources.iter().map(Source::from_cfg).collect();
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
    tracing::info!(
        "trireme player: {} source(s), {}×{} grid, swapchain={:?}",
        sources.len(),
        cfg.layout.cols,
        cfg.layout.rows,
        cfg.present_pace(),
    );
    for (i, s) in sources.iter().enumerate() {
        tracing::info!(
            "  source[{i}] '{}' {} ({} addr(s)) {}x{} pace={:?}",
            s.name,
            s.current_addr(),
            s.addrs.len(),
            s.width,
            s.height,
            s.pace
        );
    }
    let identity = Arc::new(
        core_crypto::Identity::load_or_create(&crate::servers::identity_path()?)
            .context("load client identity")?,
    );
    tracing::info!("trireme identity {}", identity.public_hex());
    let mut app = App {
        cfg,
        sources,
        identity,
        proxy: event_loop.create_proxy(),
        window: None,
        gpu: None,
        cells: Vec::new(),
        started: false,
        mods: core_proto::Modifiers::default(),
        focused: false,
        cursor: None,
        cursor_cell: None,
        capture_toggle_on: false,
        fullscreen: false,
        suspended: false,
        gamepad_sink: Arc::new(Mutex::new(None)),
        input_source: Arc::new(std::sync::atomic::AtomicU8::new(
            crate::gamepad::SRC_KBDMOUSE,
        )),
        held_keys: std::collections::HashSet::new(),
        held_buttons: std::collections::HashSet::new(),
        grab_wanted: false,
        grabbed: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `-vf` branch decision (issue #16): the HDR→SDR tonemap runs ONLY when the
    /// client opted into HDR AND the host's announce CONFIRMS the stream is HDR.
    /// Every other combination is plain decode — critically the crash-prone ones
    /// (opted-in but stream SDR, or announce not-yet-known) default safely to plain
    /// decode, never forcing the tonemap onto an SDR stream.
    #[test]
    fn tonemap_only_on_confirmed_hdr() {
        // Opted in + confirmed HDR → tonemap. The one true case.
        assert!(should_tonemap_hdr(true, Some(true)));
        // Opted in but the stream is SDR → plain decode (this is the black-window
        // crash the whole fix prevents).
        assert!(!should_tonemap_hdr(true, Some(false)));
        // Opted in but the announce hasn't arrived → plain decode (crash-safe
        // default; a real HDR stream self-corrects on the next rebuild).
        assert!(!should_tonemap_hdr(true, None));
        // Not opted in → plain decode regardless of what the host announces.
        assert!(!should_tonemap_hdr(false, Some(true)));
        assert!(!should_tonemap_hdr(false, Some(false)));
        assert!(!should_tonemap_hdr(false, None));
    }

    /// The reader's in-place rebuild decision (issue #16). The three bugs all stem
    /// from mis-handling a colorimetry mismatch; `chain_action` is the single pure
    /// rule the reader uses, and its key property is that it is a FIXED POINT — once
    /// the chain matches the stream it returns `Keep`, so the reader cannot loop
    /// (the old reconnect/preempt storm is structurally impossible here).
    /// Parameterized over BOTH tonemap kinds (GPU / CPU-fallback, issue #15): the
    /// safety rules are identical whichever chain does the tonemapping.
    #[test]
    fn chain_action_adapts_without_storming() {
        use ChainAction::*;
        use DecodeChain::*;
        for &tm in &[TonemapGpu, TonemapCpu] {
            // HAPPY PATH (the storm case, now fixed): HDR client, tonemap chain built
            // optimistically from the request, host really is HDR, ffmpeg alive →
            // KEEP. No rebuild means no reconnect means no preempt means no storm.
            assert_eq!(chain_action(true, tm, Some(true), true, tm), Keep);

            // Initial guess was plain (or host flipped SDR→HDR mid-session): HDR
            // client, plain chain, host announces HDR → rebuild to THIS machine's
            // tonemap chain, exactly once.
            assert_eq!(chain_action(true, Plain, Some(true), true, tm), Rebuild(tm));

            // HDR→SDR flip with the announce landed: tonemap chain but host now SDR →
            // rebuild to plain (whether ffmpeg is still alive or already died).
            assert_eq!(
                chain_action(true, tm, Some(false), true, tm),
                Rebuild(Plain)
            );
            assert_eq!(
                chain_action(true, tm, Some(false), false, tm),
                Rebuild(Plain)
            );

            // BUG 3 core: request/stream mismatch. HDR client, tonemap chain, the
            // announce hasn't said anything yet, but ffmpeg DIED with no frames
            // (zscale `no path between colorspaces`) → treat as SDR and rebuild to
            // plain, no black window.
            assert_eq!(chain_action(true, tm, None, false, tm), Rebuild(Plain));
            // ...but a tonemap chain that is STILL ALIVE with the announce merely
            // unknown must be kept — we don't tear down a working HDR stream on a
            // missing announce.
            assert_eq!(chain_action(true, tm, None, true, tm), Keep);

            // SDR client never tonemaps: a plain chain is always kept regardless of
            // what the (ignored) announce says (preserves the #16 safety win).
            assert_eq!(chain_action(false, Plain, Some(true), true, tm), Keep);
            assert_eq!(chain_action(false, Plain, None, true, tm), Keep);
            assert_eq!(chain_action(false, Plain, Some(false), true, tm), Keep);
            // And an SDR client that somehow has a tonemap chain rebuilds to plain.
            assert_eq!(
                chain_action(false, tm, Some(true), true, tm),
                Rebuild(Plain)
            );
        }
    }

    /// Idempotence / anti-loop proof: against a STEADY stream (a live ffmpeg and a
    /// confirmed announce) `chain_action` converges to `Keep` in at most one rebuild
    /// and then stays there forever. This is the property that makes the reconnect
    /// storm impossible; encode it so a future refactor can't reintroduce it.
    #[test]
    fn chain_action_converges_to_keep() {
        use DecodeChain::*;
        for &tm in &[TonemapGpu, TonemapCpu] {
            for &wants_hdr in &[false, true] {
                // Only CONFIRMED announces have a definite target chain. (An unknown
                // announce deliberately holds an alive chain as-is; covered
                // separately.)
                for &stream_hdr in &[false, true] {
                    let announced = Some(stream_hdr);
                    let target = if should_tonemap_hdr(wants_hdr, announced) {
                        tm
                    } else {
                        Plain
                    };
                    // Start deliberately on the wrong chain, ffmpeg alive, iterate.
                    let mut chain = if target == Plain { tm } else { Plain };
                    let mut rebuilds = 0;
                    for _ in 0..8 {
                        match chain_action(wants_hdr, chain, announced, true, tm) {
                            ChainAction::Keep => break,
                            ChainAction::Rebuild(to) => {
                                chain = to;
                                rebuilds += 1;
                            }
                        }
                    }
                    assert!(
                        rebuilds <= 1,
                        "tm={tm:?} wants_hdr={wants_hdr} announced={announced:?}: \
                         {rebuilds} rebuilds (want ≤1)"
                    );
                    assert_eq!(chain, target);
                    // Fixed point: re-evaluating the settled chain returns Keep.
                    assert_eq!(
                        chain_action(wants_hdr, chain, announced, true, tm),
                        ChainAction::Keep
                    );
                }
            }
        }
    }

    /// The happy path is storm-free specifically because an unknown announce never
    /// tears down an ALIVE, request-built tonemap chain: both chains are fixed points
    /// while the announce is `None` and ffmpeg is running, so nothing rebuilds until
    /// real evidence (a confirmed announce, or ffmpeg dying) arrives.
    #[test]
    fn chain_action_holds_alive_chain_until_announce() {
        use DecodeChain::*;
        for &tm in &[TonemapGpu, TonemapCpu] {
            // HDR client, optimistic tonemap chain, announce not arrived, alive → Keep.
            assert_eq!(
                chain_action(true, tm, None, true, tm),
                ChainAction::Keep,
                "an alive optimistic tonemap chain must not be rebuilt before the announce"
            );
            // HDR client, plain chain, announce not yet arrived, alive → Keep (we don't
            // speculatively upgrade to tonemap without a confirmed HDR announce).
            assert_eq!(chain_action(true, Plain, None, true, tm), ChainAction::Keep);
        }
    }

    /// The GPU tonemap decodes to 16-bit P010 (issue #15) — twice the bytes of the
    /// 8-bit NV12 chains. The reader sizes its `read_exact` buffer from this; a
    /// wrong size shears every frame, so pin the arithmetic.
    #[test]
    fn chain_frame_bytes_track_sample_depth() {
        use DecodeChain::*;
        // 4K: NV12 = 3840*2160*1.5, P010 doubles it.
        assert_eq!(Plain.frame_bytes(3840, 2160), 12_441_600);
        assert_eq!(TonemapCpu.frame_bytes(3840, 2160), 12_441_600);
        assert_eq!(TonemapGpu.frame_bytes(3840, 2160), 24_883_200);
        // Only the GPU chain produces HDR (P010) frames for the renderer.
        assert!(TonemapGpu.is_tonemap() && TonemapCpu.is_tonemap());
        assert!(!Plain.is_tonemap());
    }

    /// The on_frame writer's take/put race decision (issue #16 deadlock fix). After a
    /// lock-released `write_all`, put the stdin back ONLY if the slot is still empty; if
    /// the reader's rebuild swapped a fresh child's stdin in while we wrote, the slot is
    /// now Some and we must drop our stale stdin rather than clobber the new chain.
    #[test]
    fn reinsert_stdin_never_clobbers_a_swapped_in_child() {
        // Slot still empty (the common case): re-insert our stdin.
        assert!(reinsert_stdin(true));
        // Reader installed a new child's stdin while we wrote: do NOT re-insert (drop
        // the stale one). This is what keeps a mid-write colorimetry rebuild correct.
        assert!(!reinsert_stdin(false));
    }

    /// Compile both blit shaders (SDR + the issue-#15 HDR tonemap) on a real
    /// headless device so a WGSL error can't hide until a live stream. Needs a
    /// GPU with 16-bit-norm texture support, hence ignored by default — run
    /// with `cargo test -p trireme -- --ignored hdr_shader` on a dev box.
    #[test]
    #[ignore]
    fn hdr_shader_compiles_on_this_gpu() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no adapter");
        assert!(
            adapter
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM),
            "adapter lacks 16-bit norm textures; HDR path would fall back to CPU"
        );
        let (device, _queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("hdr-shader-test"),
                required_features: wgpu::Features::TEXTURE_FORMAT_16BIT_NORM,
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .expect("device");
        // Fail the test on any validation error instead of wgpu's default panic-log.
        device.on_uncaptured_error(Box::new(|e| panic!("wgpu validation: {e}")));
        for (label, src) in [
            ("sdr", SHADER.to_string()),
            (
                "hdr",
                HDR_SHADER.replace("{PEAK}", &format!("{:?}", hdr_tonemap_peak())),
            ),
        ] {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: None, // auto layout is fine for a compile check
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: "vs_main",
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: "fs_main",
                    targets: &[Some(wgpu::TextureFormat::Rgba8UnormSrgb.into())],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
        }
        device.poll(wgpu::Maintain::Wait);
    }

    #[test]
    fn source_rotates_through_candidate_addresses() {
        let cfg = SourceCfg {
            name: None,
            addr: "127.0.0.1:1".parse().unwrap(),
            protocol: None,
            password: None,
            // includes a duplicate of the primary, which should be dropped
            alt_addrs: vec![
                "127.0.0.1:2".parse().unwrap(),
                "127.0.0.1:1".parse().unwrap(),
            ],
            pace: None,
            width: None,
            height: None,
            host_pubkey: None,
        };
        let mut s = Source::from_cfg(&cfg);
        assert_eq!(s.addrs.len(), 2, "primary + one unique alt");
        assert_eq!(s.current_addr().port(), 1);
        s.addr_idx += 1;
        assert_eq!(s.current_addr().port(), 2, "rotates to the alt");
        s.addr_idx += 1;
        assert_eq!(s.current_addr().port(), 1, "wraps back to the primary");
    }

    #[test]
    fn hex_colors_parse() {
        // #RRGGBB, #RGB shorthand, and a missing-# form all work.
        assert_eq!(parse_hex_color("#FF0000"), [1.0, 0.0, 0.0]);
        assert_eq!(parse_hex_color("00FF00"), [0.0, 1.0, 0.0]);
        assert_eq!(parse_hex_color("#00F"), [0.0, 0.0, 1.0]);
        assert_eq!(parse_hex_color("#fff"), [1.0, 1.0, 1.0]);
        // Malformed → green fallback, never panics.
        assert_eq!(parse_hex_color("nope"), [0.2, 1.0, 0.4]);
        assert_eq!(parse_hex_color("#12"), [0.2, 1.0, 0.4]);
    }

    #[test]
    fn flex_grid_adapts_to_window_aspect() {
        let sixteen_nine = 16.0 / 9.0;
        // A ~square window with four 16:9 sources → balanced 2×2.
        assert_eq!(flex_grid(4, 1920, 1080, sixteen_nine), (2, 2));
        // A very wide window → a single row.
        assert_eq!(flex_grid(4, 6000, 800, sixteen_nine), (4, 1));
        // A tall/portrait window → a single column.
        assert_eq!(flex_grid(4, 600, 3000, sixteen_nine), (1, 4));
        // One source is always 1×1 regardless of aspect.
        assert_eq!(flex_grid(1, 6000, 200, sixteen_nine), (1, 1));
    }

    #[test]
    fn flex_grid_always_covers_all_sources() {
        for n in 1..=9usize {
            for (w, h) in [(1920, 1080), (3840, 1080), (1080, 1920), (1000, 1000)] {
                let (c, r) = flex_grid(n, w, h, 16.0 / 9.0);
                assert!(
                    (c * r) as usize >= n,
                    "grid {c}×{r} too small for {n} sources at {w}x{h}"
                );
                assert!(c >= 1 && r >= 1);
            }
        }
    }

    #[test]
    fn flex_grid_degenerate_dims_dont_panic() {
        // Zero/!finite inputs must not panic or divide-by-zero.
        assert!(flex_grid(4, 0, 0, 16.0 / 9.0).0 >= 1);
        assert!(flex_grid(4, 1920, 1080, f32::NAN).0 >= 1);
        assert!(flex_grid(4, 1920, 1080, 0.0).0 >= 1);
    }

    #[test]
    fn parses_two_source_grid() {
        let toml_text = r#"
present = "low-latency"
layout  = { cols = 2, rows = 1 }

[[source]]
name = "desktop"
addr = "127.0.0.1:4433"
pace = "low-latency"

[[source]]
name = "console"
addr = "127.0.0.1:4435"
pace = "vrr"
width  = 1920
height = 1080
"#;
        let cfg: PlayerCfg = toml::from_str(toml_text).expect("parse");
        cfg.validate().expect("validate");
        assert_eq!(cfg.sources.len(), 2);
        assert_eq!(cfg.layout.cols, 2);
        assert_eq!(cfg.sources[0].addr, "127.0.0.1:4433".parse().unwrap());
        assert_eq!(cfg.sources[1].name.as_deref(), Some("console"));
    }

    #[test]
    fn rejects_overflow_grid() {
        let toml_text = r#"
layout  = { cols = 1, rows = 1 }
[[source]]
addr = "127.0.0.1:4433"
[[source]]
addr = "127.0.0.1:4434"
"#;
        let cfg: PlayerCfg = toml::from_str(toml_text).expect("parse");
        assert!(cfg.validate().is_err(), "two sources in a 1x1 must fail");
    }

    #[test]
    fn letterboxes_wide_source_in_square_cell() {
        // 16:9 source in a 1:1 cell → black bars top + bottom (letterbox).
        let (x, y, w, h) = letterbox(0.0, 0.0, 1080.0, 1080.0, 1920, 1080);
        assert!(
            (w - 1080.0).abs() < 0.5,
            "w should fill cell width, got {w}"
        );
        assert!(h < 1080.0, "h should shrink, got {h}");
        assert!(y > 0.0, "y should offset down, got {y}");
        assert!((x - 0.0).abs() < 0.5, "x should be 0, got {x}");
    }

    #[test]
    fn pillarboxes_square_source_in_wide_cell() {
        // 1:1 source in a 16:9 cell → black bars left + right (pillarbox).
        let (x, y, w, h) = letterbox(0.0, 0.0, 1920.0, 1080.0, 1080, 1080);
        assert!(
            (h - 1080.0).abs() < 0.5,
            "h should fill cell height, got {h}"
        );
        assert!(w < 1920.0, "w should shrink, got {w}");
        assert!(x > 0.0, "x should offset right, got {x}");
        assert!((y - 0.0).abs() < 0.5, "y should be 0, got {y}");
    }
}
