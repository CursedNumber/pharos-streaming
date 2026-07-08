// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Zero-copy WGC → NVENC HEVC pipeline (issue #12), with HDR10 (issue #4).
//!
//! Replaces the ffmpeg subprocess on the WGC path: the capture pool's D3D11
//! texture is GPU-copied into one persistent registered texture and encoded
//! in-process by NVENC through the vendored SDK (D3D11 device session), so no
//! frame bytes ever cross the CPU. The 4K cost that sank the old path (33 MB
//! staging readback + 33 MB pipe write + CPU BGRA→NV12 per frame) disappears;
//! the only per-frame work is one GPU CopyResource (plus, in HDR, one
//! fullscreen shader pass) and the encode itself.
//!
//! HDR10 mode (client sent `wants_hdr` and the captured output has Windows
//! HDR active): the pool captures the DWM's fp16 scRGB composition, a pixel
//! shader ports the Linux compositor's scRGB→PQ/BT.2020 math (multi.rs
//! `SCRGB_TO_PQ_SHADER`) into a 10-bit R10G10B10A2 target registered with
//! NVENC as ABGR10, and the session encodes HEVC **Main10** with PQ/BT.2020
//! VUI plus Mastering-Display + Content-Light-Level SEI on every IDR.
//!
//! Threading mirrors `wgc.rs`: ONE dedicated thread owns every COM object
//! (WGC pool, D3D11 device, NVENC session — none are Send) plus the encoder,
//! and paces itself at `fps` the way the old pacer did: a tick with no fresh
//! WGC frame re-encodes the held texture, so the change-driven source still
//! yields the continuous cadence clients expect. Only finished access units
//! cross the channel.
//!
//! Fault model: any non-transient NVENC/D3D error records a typed reason and
//! ends the thread; the channel disconnect surfaces in the supervisor's pump
//! as a device-loss fault (never as idle — the same tri-state rule as
//! `wgc::Recv`), and the P6 ladder rebuilds the whole pipeline with a fresh
//! device and session next generation.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT::{NV_ENC_BUFFER_FORMAT_ABGR10, NV_ENC_BUFFER_FORMAT_ARGB},
    NV_ENC_CODEC_HEVC_GUID, NV_ENC_HEVC_PROFILE_MAIN10_GUID, NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_PARAMS_HEVC, NV_ENC_PIC_TYPE, NV_ENC_PRESET_P4_GUID,
    NV_ENC_SEI_PAYLOAD, NV_ENC_TUNING_INFO, NV_ENC_VUI_COLOR_PRIMARIES, NV_ENC_VUI_MATRIX_COEFFS,
    NV_ENC_VUI_TRANSFER_CHARACTERISTIC,
};
use nvidia_video_codec_sdk::{
    CodecPictureParams, EncodePictureParams, Encoder, EncoderInitParams, Session,
};
use windows::core::{ComInterface, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;

use crate::{display, wgc};

/// One encoded access unit. NVENC's lock-bitstream returns exactly one AU per
/// encoded picture, so no Annex-B splitting is needed on this path; the
/// keyframe flag (IDR or I) feeds `QuicStreamer::send_frame`'s reserve.
pub struct Au {
    pub bytes: Vec<u8>,
    pub keyframe: bool,
}

/// Shared bitrate-control cell between the ~1 s adaptive loop (the sole writer of
/// `target_kbps`) and the encode thread (the sole writer of `ceiling_kbps`, and the
/// reader of `target_kbps`). All values are kbps; 0 = unset. (issue #30)
#[derive(Clone, Default)]
pub struct BitrateCtl {
    /// The CBR target the adaptive loop currently wants. 0 until the loop first
    /// pushes a value; the encode thread treats 0 as "use the build ceiling".
    pub target_kbps: Arc<AtomicU32>,
    /// The build/ceiling bitrate of the CURRENT generation (the resolution+HDR
    /// `bitrate_kbps`); the adaptive loop reads it as its upper bound and waits
    /// while it is 0 (no pipeline built yet).
    pub ceiling_kbps: Arc<AtomicU32>,
    /// Bumped once per pipeline build (colorimetry flip, display-mode change,
    /// fault recovery). The control loop watches it and RE-ARMS at the ceiling
    /// with a short grace window instead of inheriting a brake earned on the
    /// respawn transient — a Windows display-mode switch spikes RTT and drops
    /// packets for a few seconds (DPC storms), which reads as severe congestion
    /// and otherwise parks the whole session at the floor.
    pub generation: Arc<AtomicU32>,
}

/// Lower bound the adaptive-bitrate loop (issue #30) will drop to, kbps — mirrors the
/// Linux compositor's NET_FLOOR_KBPS. Clamped to the ceiling when the ceiling is lower.
pub const MIN_ADAPTIVE_KBPS: u32 = 2000;

/// Handle the supervisor owns: AUs come out of `recv_timeout`, faults surface
/// as [`ZcRecv::Dead`] with a reason in `fault_reason`, and `stop` tears the
/// encoder thread down without any process to kill (the in-process analogue
/// of the drop(enc)-kills-ffmpeg contract).
pub struct ZcPipeline {
    pub size: (u32, u32),
    /// True when this session is actually encoding HEVC Main10 BT.2020/PQ.
    pub hdr: bool,
    au_rx: Option<Receiver<Au>>,
    stop: Arc<AtomicBool>,
    /// One-shot on-demand keyframe request (client `DG_REQUEST_IDR`, forwarded
    /// by the supervisor pump). The encode loop swaps it false and forces the
    /// next picture to an IDR, rate-limited like the Linux host.
    idr_req: Arc<AtomicBool>,
    /// One-shot drop-recovery keyframe request (the transport queue shed an AU).
    /// Coalesced much harder than `idr_req` (~1s vs ~150ms): under sustained
    /// congestion every frame drops, and honouring each at the client-request
    /// rate is a 4-6Hz forced-IDR storm whose oversized keyframes deepen the
    /// congestion (the live "quality pulse" on the 4K HDR path).
    drop_idr_req: Arc<AtomicBool>,
    fault: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The off-encode-thread HDR-active poll (issue #29), if this generation watches
    /// for a mid-session flip. Joined in `stop()`; exits within ~100ms of `stop`.
    poll_thread: Option<std::thread::JoinHandle<()>>,
}

/// Tri-state receive, same contract as `wgc::Recv`: `Dead` is a pipeline
/// fault, never idle.
pub enum ZcRecv {
    Au(Au),
    Empty,
    Dead,
}

impl ZcPipeline {
    /// Start capture + in-process encode for `monitor_idx` at `fps`.
    /// `hdr_requested` is the client's `wants_hdr`; it engages only when the
    /// captured output actually has Windows HDR active (or `PHAROS_HDR=1`
    /// forces it for bring-up on SDR rigs). Blocks until the capture item and
    /// NVENC session are up (so `size` is the item's true size) or reports
    /// the init error.
    pub fn start(
        monitor_idx: usize,
        fps: u32,
        hdr_requested: bool,
        ctl: BitrateCtl,
    ) -> Result<Self> {
        let hmon = wgc::monitor_by_index(monitor_idx)?;
        let force = std::env::var("PHAROS_HDR").is_ok_and(|v| v.trim() == "1");
        let hdr = if force {
            tracing::warn!("PHAROS_HDR=1: forcing the HDR pipeline regardless of display state");
            true
        } else if hdr_requested {
            match display::monitor_hdr_active(hmon) {
                Ok(true) => true,
                Ok(false) => {
                    tracing::info!(
                        "client wants HDR but Windows HDR is off on the captured output; \
                         streaming SDR"
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!("HDR probe failed: {e:#}; streaming SDR");
                    false
                }
            }
        } else {
            false
        };
        // Watch for a mid-session Windows HDR toggle only when it can change the encode:
        // the client asked for HDR and we're not force-pinning it (PHAROS_HDR=1). A force
        // or SDR-only session never repolls (its colorimetry doesn't depend on the
        // desktop's HDR state).
        let watch_hdr = hdr_requested && !force;

        let stop = Arc::new(AtomicBool::new(false));
        let idr_req = Arc::new(AtomicBool::new(false));
        let drop_idr_req = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        // Latched by the poll thread when the monitor's HDR-active state diverges from
        // what this generation built; zc_loop reads it cheaply each tick and faults so
        // the supervisor respawns at the new colorimetry (issue #29).
        let hdr_changed = Arc::new(AtomicBool::new(false));
        let (au_tx, au_rx) = sync_channel::<Au>(8);
        let (init_tx, init_rx) =
            std::sync::mpsc::channel::<std::result::Result<(u32, u32), String>>();
        let (stop_t, fault_t) = (stop.clone(), fault.clone());
        let hdr_changed_z = hdr_changed.clone();
        let ctl_z = ctl.clone();
        let idr_t = idr_req.clone();
        let drop_idr_t = drop_idr_req.clone();
        // Captured for the poll thread below, before `hmon`/`hdr` move into the encode
        // closure (HMONITOR travels as its raw isize; `hdr` is Copy).
        let (hmon_isize, built_hdr) = (hmon.0, hdr);
        let thread = std::thread::Builder::new()
            .name("wgc-zc".into())
            .spawn(move || {
                if let Err(e) = zc_loop(
                    hmon,
                    fps,
                    hdr,
                    &hdr_changed_z,
                    &stop_t,
                    &idr_t,
                    &drop_idr_t,
                    &ctl_z,
                    init_tx.clone(),
                    au_tx,
                ) {
                    let msg = format!("{e:#}");
                    tracing::error!("wgc-zc pipeline ended: {msg}");
                    *fault_t.lock().unwrap() = Some(msg.clone());
                    // If init never completed, surface the error to start().
                    let _ = init_tx.send(Err(msg));
                }
            })?;
        let size = init_rx
            .recv()
            .map_err(|_| anyhow!("wgc-zc thread died during init"))?
            .map_err(|e| anyhow!("wgc-zc init: {e}"))?;
        // The HDR-active poll runs on its OWN thread (issue #29 follow-up) so the DXGI
        // enumeration in monitor_hdr_active never stalls the encode hot path. Spawned
        // AFTER a successful init (so an init failure can't leak it); polls ~1x/s,
        // latches `hdr_changed` on a real change, and exits within ~100ms of stop.
        let poll_thread = if watch_hdr {
            let (stop_p, changed_p) = (stop.clone(), hdr_changed.clone());
            Some(
                std::thread::Builder::new()
                    .name("wgc-hdr-poll".into())
                    .spawn(move || {
                        let hmon = windows::Win32::Graphics::Gdi::HMONITOR(hmon_isize);
                        'poll: while !stop_p.load(Ordering::Relaxed) {
                            for _ in 0..10 {
                                if stop_p.load(Ordering::Relaxed) {
                                    break 'poll;
                                }
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            match display::monitor_hdr_active(hmon) {
                                Ok(now) if now != built_hdr => {
                                    tracing::info!(
                                        "wgc-zc: Windows HDR toggled on the captured output \
                                         (was {built_hdr}, now {now}); requesting respawn (issue #29)"
                                    );
                                    changed_p.store(true, Ordering::Relaxed);
                                    break 'poll;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::debug!("wgc-zc HDR poll failed: {e:#} (ignored)")
                                }
                            }
                        }
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            size,
            hdr,
            au_rx: Some(au_rx),
            stop,
            idr_req,
            drop_idr_req,
            fault,
            thread: Some(thread),
            poll_thread,
        })
    }

    /// Ask the encode loop to make the next picture an IDR (on-demand keyframe,
    /// e.g. a client resyncing after a decode-chain rebuild — issue #32). The
    /// loop coalesces requests within its min-IDR interval (~150ms).
    pub fn request_idr(&self) {
        self.idr_req.store(true, Ordering::Relaxed);
    }

    /// Ask for a drop-recovery IDR (the transport queue shed an AU and the
    /// client will smear until a keyframe). Coalesced far harder (~1s) than
    /// [`Self::request_idr`] so sustained congestion cannot IDR-storm.
    pub fn request_idr_after_drop(&self) {
        self.drop_idr_req.store(true, Ordering::Relaxed);
    }

    /// Next AU, waiting up to `dur`.
    pub fn recv_timeout(&self, dur: Duration) -> ZcRecv {
        let rx = self.au_rx.as_ref().expect("au_rx taken only in stop()");
        match rx.recv_timeout(dur) {
            Ok(au) => ZcRecv::Au(au),
            Err(RecvTimeoutError::Timeout) => ZcRecv::Empty,
            Err(RecvTimeoutError::Disconnected) => ZcRecv::Dead,
        }
    }

    /// The typed reason the encoder thread recorded before dying, if any.
    pub fn fault_reason(&self) -> Option<String> {
        self.fault.lock().ok().and_then(|g| g.clone())
    }

    /// Ordered teardown: signal stop, unblock a send-blocked encoder thread by
    /// dropping the receiver, then join. The supervisor calls this in the slot
    /// where the ffmpeg path does drop(enc) + joins.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.au_rx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.poll_thread.take() {
            let _ = t.join();
        }
    }
}

/// Transient NVENC errors: skip the frame and keep streaming (the Linux
/// host's taxonomy — everything else is fatal for the generation).
fn is_transient(msg: &str) -> bool {
    msg.contains("NeedMoreInput") || msg.contains("EncoderBusy") || msg.contains("LockBusy")
}

/// Periodic IDR cadence in frames (mirrors multi.rs's `idr_period_frames`).
/// Used to be hardcoded to `fps` (1s) for fast mid-stream-join resync; that
/// requirement no longer holds, so it's env-tunable via `PHAROS_IDR_PERIOD_SEC`
/// (default 4s) — spacing IDRs out means the encoder eats the IDR-sized
/// rate-control spike less often.
fn idr_period_frames(fps: u32) -> u32 {
    let secs = std::env::var("PHAROS_IDR_PERIOD_SEC")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4);
    fps.max(1).saturating_mul(secs)
}

/// VBV buffer depth, in frames of average bitrate (mirrors multi.rs's
/// `vbv_buffer_frames`). Used to be hardcoded to 1 frame, which an IDR (2-4x a
/// P-frame) overflows — see the `bitrate_kbps` doc comment on the visible
/// pulse this caused on the Bravia. Env-tunable via `PHAROS_VBV_FRAMES`
/// (default 4).
fn vbv_buffer_frames() -> u32 {
    std::env::var("PHAROS_VBV_FRAMES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4)
}

/// VBV buffer size in BITS for a given CBR `kbps` at `fps` — the value NVENC wants in
/// `rcParams.vbvBufferSize`. Factored so the initial config and a live
/// `reconfigure_bitrate` (issue #30) size the VBV identically.
fn vbv_bits(kbps: u32, fps: u32) -> u32 {
    (kbps.saturating_mul(1000) / fps.max(1)).saturating_mul(vbv_buffer_frames())
}

/// CBR bitrate for the session, kbps: `PHAROS_BITRATE_KBPS` if set, else a
/// pixel-rate + depth aware default (LAN-tuned), clamped to 15..60 Mbps.
/// A flat 20 Mbps starves 4K60 Main10 badly enough that the 1s IDR refresh
/// pulses visibly (the quality snap between the keyframe and the starved
/// P-frames); 45 Mbps eliminated it in live verification on the Bravia, and
/// the 0.13/0.09 bits-per-pixel targets bracket that data point.
fn bitrate_kbps(w: u32, h: u32, fps: u32, hdr: bool) -> u32 {
    if let Some(v) = std::env::var("PHAROS_BITRATE_KBPS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return v;
    }
    let pixel_rate = u64::from(w) * u64::from(h) * u64::from(fps.max(1));
    let bpp_x100: u64 = if hdr { 13 } else { 9 };
    (pixel_rate * bpp_x100 / 100_000).clamp(15_000, 60_000) as u32
}

/// Static HDR10 metadata SEI payloads, byte-for-byte the Linux host's
/// (`compositor/src/nvenc.rs::hdr10_sei_payloads`): Mastering Display Colour
/// Volume (type 137, BT.2020 primaries + D65, 1000/0.0001 cd/m2) and Content
/// Light Level (type 144, MaxCLL 1000 / MaxFALL 400). Placeholder mastering
/// values; a later refinement sources them from `IDXGIOutput6::GetDesc1`.
fn hdr10_sei_payloads() -> (Vec<u8>, Vec<u8>) {
    let mut mdcv = Vec::with_capacity(24);
    for &(x, y) in &[
        (8500u16, 39850u16), // G
        (6550, 2300),        // B
        (35400, 14600),      // R
        (15635, 16450),      // white D65
    ] {
        mdcv.extend_from_slice(&x.to_be_bytes());
        mdcv.extend_from_slice(&y.to_be_bytes());
    }
    mdcv.extend_from_slice(&10_000_000u32.to_be_bytes());
    mdcv.extend_from_slice(&1u32.to_be_bytes());
    let mut cll = Vec::with_capacity(4);
    cll.extend_from_slice(&1000u16.to_be_bytes());
    cll.extend_from_slice(&400u16.to_be_bytes());
    (mdcv, cll)
}

/// Main10 + BT.2020/PQ VUI on a fresh HEVC preset config: the D3D11 port of
/// the Linux host's `apply_hdr10` (multi.rs). `cfg` must be the preset
/// returned for the HEVC GUID so the union is the HEVC variant.
fn apply_hdr10(cfg: &mut nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_CONFIG) {
    cfg.profileGUID = NV_ENC_HEVC_PROFILE_MAIN10_GUID;
    // SAFETY: the union is the HEVC variant (preset fetched for the HEVC GUID).
    unsafe {
        let hevc = &mut cfg.encodeCodecConfig.hevcConfig;
        hevc.set_pixelBitDepthMinus8(2); // 10-bit encoded output
        let vui = &mut hevc.hevcVUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFullRangeFlag = 0; // limited range: standard HDR10 signal
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
        vui.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
        vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
    }
}

/// scRGB (fp16, linear BT.709 light, 1.0 = 80 nits — what the DWM composites
/// on an HDR desktop) → PQ (ST-2084) / BT.2020, the HLSL port of the Linux
/// compositor's `SCRGB_TO_PQ_SHADER` (multi.rs): same PQ constants, same
/// BT.709→BT.2020 matrix (transposed here — GLSL's mat3 constructor is
/// column-major, HLSL's initializer is row-major). Fullscreen triangle from
/// `SV_VertexID`, source sampled by pixel Load (same-size textures).
const SCRGB_TO_PQ_HLSL: &str = r#"
Texture2D<float4> src : register(t0);

struct VSOut { float4 pos : SV_Position; };

VSOut vs_main(uint id : SV_VertexID) {
    float2 uv = float2((id << 1) & 2, id & 2);
    VSOut o;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

static const float PQ_M1 = 0.1593017578125;
static const float PQ_M2 = 78.84375;
static const float PQ_C1 = 0.8359375;
static const float PQ_C2 = 18.8515625;
static const float PQ_C3 = 18.6875;

float3 nits_to_pq(float3 nits) {
    float3 y = clamp(nits / 10000.0, 0.0, 1.0);
    float3 ym1 = pow(y, PQ_M1);
    float3 num = PQ_C1 + PQ_C2 * ym1;
    float3 den = 1.0 + PQ_C3 * ym1;
    return pow(num / den, PQ_M2);
}

static const float3x3 BT709_TO_BT2020 = {
    0.627403896, 0.329283038, 0.043313066,
    0.069097289, 0.919540395, 0.011362316,
    0.016391439, 0.088013308, 0.895595253
};

float4 ps_main(VSOut i) : SV_Target {
    float4 c = src.Load(int3(int2(i.pos.xy), 0));
    float3 lin709 = max(c.rgb, 0.0);
    float3 nits = mul(BT709_TO_BT2020, lin709 * 80.0);
    return float4(nits_to_pq(nits), 1.0);
}
"#;

/// The compiled scRGB→PQ pass plus its persistent GPU objects.
struct PqPass {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    srv: ID3D11ShaderResourceView,
    rtv: ID3D11RenderTargetView,
    viewport: D3D11_VIEWPORT,
}

impl PqPass {
    /// Build the pass: `src` is the persistent fp16 capture texture, `dst`
    /// the 10-bit NVENC-registered target.
    fn new(
        device: &ID3D11Device,
        src: &ID3D11Texture2D,
        dst: &ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Result<Self> {
        let vs_blob = compile_hlsl(SCRGB_TO_PQ_HLSL, "vs_main", "vs_5_0")?;
        let ps_blob = compile_hlsl(SCRGB_TO_PQ_HLSL, "ps_main", "ps_5_0")?;
        let (mut vs, mut ps) = (None, None);
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
                .context("CreateVertexShader")?;
            device
                .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))
                .context("CreatePixelShader")?;
        }
        let mut srv = None;
        unsafe { device.CreateShaderResourceView(src, None, Some(&mut srv)) }
            .context("CreateShaderResourceView")?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(dst, None, Some(&mut rtv)) }
            .context("CreateRenderTargetView")?;
        Ok(Self {
            vs: vs.ok_or_else(|| anyhow!("no vertex shader"))?,
            ps: ps.ok_or_else(|| anyhow!("no pixel shader"))?,
            srv: srv.ok_or_else(|| anyhow!("no SRV"))?,
            rtv: rtv.ok_or_else(|| anyhow!("no RTV"))?,
            viewport: D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            },
        })
    }

    /// Run the fullscreen conversion; unbinds views afterwards so the next
    /// tick's CopyResource into the source texture has no hazard.
    fn run(&self, context: &ID3D11DeviceContext) {
        unsafe {
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(&self.ps, None);
            context.PSSetShaderResources(0, Some(&[Some(self.srv.clone())]));
            context.RSSetViewports(Some(&[self.viewport]));
            context.OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            context.Draw(3, 0);
            context.OMSetRenderTargets(None, None);
            context.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

fn compile_hlsl(src: &str, entry: &str, target: &str) -> Result<ID3DBlob> {
    let entry_c = std::ffi::CString::new(entry).unwrap();
    let target_c = std::ffi::CString::new(target).unwrap();
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            src.as_ptr().cast(),
            src.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry_c.as_ptr().cast()),
            PCSTR(target_c.as_ptr().cast()),
            0,
            0,
            &mut blob,
            Some(&mut errors),
        )
    };
    if let Err(e) = hr {
        let log = errors
            .map(|b| String::from_utf8_lossy(blob_bytes(&b)).into_owned())
            .unwrap_or_default();
        return Err(anyhow!("D3DCompile({entry}): {e} {log}"));
    }
    blob.ok_or_else(|| anyhow!("D3DCompile returned no blob"))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

/// The capture+encode thread body. Owns every COM/NVENC object; returns Err
/// only for init or fatal runtime faults (recorded by the spawn wrapper).
#[allow(clippy::too_many_arguments)]
fn zc_loop(
    hmon: windows::Win32::Graphics::Gdi::HMONITOR,
    fps: u32,
    hdr: bool,
    // Latched true by the pipeline's off-thread HDR-active poll when the monitor's HDR
    // state diverges from what this generation built (issue #29); checked cheaply each
    // tick below so a mid-session flip faults into a colorimetry respawn.
    hdr_changed: &AtomicBool,
    stop: &AtomicBool,
    // One-shot on-demand keyframe request (client `DG_REQUEST_IDR`, issue #32):
    // swapped false each tick; forces the next picture to an IDR, coalesced at
    // ~150ms (client requests deserve speed — a decode-chain rebuild is waiting).
    idr_req: &AtomicBool,
    // One-shot drop-recovery keyframe request (the transport queue shed an AU):
    // swapped false each tick; coalesced at ~1s so sustained congestion cannot
    // become a 4-6Hz forced-IDR storm that feeds itself.
    drop_idr_req: &AtomicBool,
    // Adaptive-bitrate cell (issue #30): read `target_kbps` each tick and reconfigure
    // the live NVENC session in place; publish `ceiling_kbps` at build (in the config
    // block above).
    ctl: &BitrateCtl,
    init_tx: std::sync::mpsc::Sender<std::result::Result<(u32, u32), String>>,
    au_tx: SyncSender<Au>,
) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let (device, context) = wgc::create_d3d11_device()?;
    let rt_device = wgc::create_rt_device(&device)?;
    let item = wgc::create_item_for_monitor(hmon)?;
    let size = item.Size()?;
    let (w, h) = (size.Width.max(0) as u32, size.Height.max(0) as u32);
    if w == 0 || h == 0 {
        return Err(anyhow!("capture item reports {w}x{h}"));
    }

    let pool_format = if hdr {
        // The DWM composites an HDR desktop in fp16 scRGB; capture it losslessly.
        windows::Graphics::DirectX::DirectXPixelFormat::R16G16B16A16Float
    } else {
        windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized
    };
    let pool = windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
        &rt_device,
        pool_format,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    if std::env::var("PHAROS_DRAW_MOUSE").is_ok_and(|v| v.trim() == "0") {
        session.SetIsCursorCaptureEnabled(false)?;
        tracing::info!("wgc-zc: cursor capture disabled (PHAROS_DRAW_MOUSE=0)");
    }
    session.StartCapture()?;

    // NVENC session on the SAME device the pool fills textures on: the whole
    // point — every per-frame hop below is GPU→GPU on one device. SAFETY:
    // `device` outlives encoder/session/reg (single scope, declared first).
    let encoder = unsafe { Encoder::initialize_with_directx(device.as_raw()) }
        .map_err(|e| anyhow!("NVENC D3D11 session: {e:?}"))?;

    let encode_guid = NV_ENC_CODEC_HEVC_GUID;
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| anyhow!("preset config: {e:?}"))?;

    // The Linux host's low-latency recipe: periodic GOP/IDR (see
    // `idr_period_frames`) with VPS/SPS/PPS repeated on every keyframe, plus a
    // multi-frame VBV CBR (multi.rs's shape) so an IDR-sized spike doesn't
    // overflow the buffer and force a QP clamp.
    let cfg = &mut preset_config.presetCfg;
    let idr = idr_period_frames(fps);
    cfg.gopLength = idr;
    cfg.frameIntervalP = 1;
    cfg.encodeCodecConfig.hevcConfig.idrPeriod = idr;
    // SAFETY: the union is the HEVC variant (preset fetched for the HEVC GUID).
    unsafe {
        cfg.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1);
    }
    // Adaptive bitrate (issue #30): publish this generation's ceiling so the control
    // loop knows the upper bound, and START at the loop's current target if it has one
    // (continuity across a respawn) — else the ceiling. The generation bump tells the
    // loop to re-arm at the ceiling + grace rather than inherit a transient brake.
    let ceiling = bitrate_kbps(w, h, fps, hdr);
    ctl.ceiling_kbps.store(ceiling, Ordering::Relaxed);
    ctl.generation.fetch_add(1, Ordering::Relaxed);
    let kbps = match ctl.target_kbps.load(Ordering::Relaxed) {
        0 => ceiling,
        t => t.clamp(MIN_ADAPTIVE_KBPS.min(ceiling), ceiling),
    };
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    cfg.rcParams.averageBitRate = kbps.saturating_mul(1000);
    cfg.rcParams.maxBitRate = kbps.saturating_mul(1000);
    cfg.rcParams.vbvBufferSize = vbv_bits(kbps, fps);
    cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
    if hdr {
        apply_hdr10(cfg);
    }

    let buffer_format = if hdr {
        NV_ENC_BUFFER_FORMAT_ABGR10 // DXGI R10G10B10A2 little-endian
    } else {
        NV_ENC_BUFFER_FORMAT_ARGB // DXGI B8G8R8A8 little-endian
    };
    let mut init = EncoderInitParams::new(encode_guid, w, h);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps.max(1), 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);
    let nv_session = encoder
        .start_session(buffer_format, init)
        .map_err(|e| anyhow!("start session: {e:?}"))?;

    // Textures. SDR: pool frame → CopyResource → enc_tex (BGRA, registered).
    // HDR: pool frame → CopyResource → cap_tex (fp16) → PQ pass → enc_tex
    // (R10G10B10A2, registered). Pool frames are Closed immediately after the
    // copy, keeping the 2-buffer pool healthy; a tick with no fresh frame
    // re-encodes the previous enc_tex contents (the pacer's hold-frame
    // semantics without any CPU bytes).
    let enc_format = if hdr {
        DXGI_FORMAT_R10G10B10A2_UNORM
    } else {
        DXGI_FORMAT_B8G8R8A8_UNORM
    };
    let enc_tex = create_texture(&device, w, h, enc_format, D3D11_BIND_RENDER_TARGET.0 as u32)?;
    let hdr_pass = if hdr {
        let cap_tex = create_texture(
            &device,
            w,
            h,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
        )?;
        let pass = PqPass::new(&device, &cap_tex, &enc_tex, w, h)?;
        Some((cap_tex, pass))
    } else {
        None
    };

    let mut reg = nv_session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            enc_tex.as_raw(),
            0,
        )
        .map_err(|e| anyhow!("register D3D11 texture: {e:?}"))?;

    let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
        + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
        .max(1) as usize;
    let mut outputs: Vec<_> = (0..num_bufs)
        .map(|_| nv_session.create_output_bitstream())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow!("create bitstream: {e:?}"))?;

    let _ = init_tx.send(Ok((w, h)));
    tracing::info!(
        "wgc-zc: {w}x{h}@{fps} HEVC{} in-process (CBR {kbps} kbps, gop {}); zero CPU copies",
        if hdr { " Main10 BT.2020/PQ" } else { "" },
        fps.max(1)
    );

    let (mdcv, cll) = hdr10_sei_payloads();
    let sei = sei_array(&mdcv, &cll);
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let mut next = Instant::now() + interval;
    let mut got_first = false;
    let mut frame_no: u64 = 0;
    // #27 per-stage timing: accumulate each stage's per-frame cost and log the
    // averages every ~2s, so a per-frame sync stall (vs raw NVENC throughput) is
    // visible in the host log — capture drain, PQ pass, encode submit, and the
    // blocking bitstream lock are timed separately.
    let (mut t_cap, mut t_pq, mut t_enc, mut t_lock) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let mut stage_frames: u64 = 0;
    // Count of fresh WGC frames actually drained this window (the true capture
    // rate) vs stage_frames (the tick grid) — the two together separate a
    // capture-limited stall (fresh << ticks) from an encode-limited one (ticks drop).
    let mut fresh_frames: u64 = 0;
    let mut last_stage_log = Instant::now();
    // Adaptive bitrate (issue #30): the CBR value NVENC is currently configured for;
    // reconfigured in place below when the control loop moves `ctl.target_kbps`.
    let mut applied_kbps = kbps;
    // On-demand IDR coalescing: honour a client `DG_REQUEST_IDR` by forcing the
    // next picture to an IDR, but drop requests landing within the min interval
    // of the last keyframe (periodic or forced) — a fresh IDR already gave a
    // clean reference. Same env lever as Linux. Drop-recovery requests get a
    // much LONGER interval: under sustained congestion every queued frame
    // drops, and honouring each at the client rate is a 4-6Hz forced-IDR storm
    // whose oversized keyframes deepen the very congestion that caused the
    // drops (the live 4K HDR "quality pulse"). 1s still beats the 4s periodic.
    let min_idr_interval = Duration::from_millis(
        std::env::var("PHAROS_MIN_IDR_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150),
    );
    let drop_idr_interval = Duration::from_millis(
        std::env::var("PHAROS_DROP_IDR_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000),
    );
    let mut last_idr_at: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Mid-session HDR flip (issue #29): the off-thread poll latched a colorimetry
        // change, so end this generation with a typed fault — the supervisor respawns at
        // the new colorimetry and re-announces (a reconfigure, not a fault: no backoff).
        if hdr_changed.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "Windows HDR toggled on the captured output; respawning to re-encode at \
                 the new colorimetry (issue #29)"
            ));
        }
        // Drain the pool to the newest frame; GPU-copy it and return the pool
        // buffer immediately.
        let cap0 = Instant::now();
        let mut fresh = false;
        loop {
            let frame = match pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(e) if e.code().is_ok() => break, // empty — change-driven idle
                Err(e) => return Err(anyhow!("TryGetNextFrame: {e:?} (capture session dead)")),
            };
            // WGC does NOT fault on a display-mode change (unlike DDA): it
            // keeps delivering frames at the stale pool size, silently
            // cropping the new desktop to the top-left corner (found live: a
            // 1080p session kept streaming the top-left quarter of the
            // rewoken 4K desktop). Detect the change and make it a typed
            // fault; the P6 respawn rebuilds everything at the new size.
            let cs = frame.ContentSize()?;
            let (cw, ch) = (cs.Width.max(0) as u32, cs.Height.max(0) as u32);
            if (cw, ch) != (w, h) {
                let _ = frame.Close();
                return Err(anyhow!(
                    "capture size changed {w}x{h} → {cw}x{ch} (display mode change); respawning"
                ));
            }
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let copy_dst = match &hdr_pass {
                Some((cap_tex, _)) => cap_tex,
                None => &enc_tex,
            };
            unsafe { context.CopyResource(copy_dst, &texture) };
            let _ = frame.Close();
            got_first = true;
            fresh = true;
            fresh_frames += 1;
        }
        t_cap += cap0.elapsed();
        if fresh {
            if let Some((_, pass)) = &hdr_pass {
                let pq0 = Instant::now();
                pass.run(&context);
                t_pq += pq0.elapsed();
            }
        }

        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += interval;
        // If encode fell behind the tick grid (transient stall), resync
        // rather than bursting to catch up.
        if next < Instant::now() {
            next = Instant::now() + interval;
        }

        if !got_first {
            continue; // nothing captured yet — nothing to encode
        }

        // HDR10 static metadata rides every IDR; the GOP is aligned by forcing
        // the IDR at each period boundary, which also matches idrPeriod. An
        // on-demand request (client resync, issue #32) forces one off-period,
        // rate-limited; the swap always consumes the request (coalesce, don't
        // queue). HDR attaches the SEI to forced IDRs too, so a mid-GOP joiner
        // still gets the mastering/CLL metadata.
        let periodic = frame_no.is_multiple_of(u64::from(idr_period_frames(fps)));
        // Both swaps run unconditionally (consume = coalesce, never queue).
        let (client_req, drop_req) = (
            idr_req.swap(false, Ordering::Relaxed),
            drop_idr_req.swap(false, Ordering::Relaxed),
        );
        let idr_ok = |min: Duration| last_idr_at.is_none_or(|t| t.elapsed() >= min);
        let forced = !periodic
            && ((client_req && idr_ok(min_idr_interval))
                || (drop_req && idr_ok(drop_idr_interval)));
        let params = if hdr && (periodic || forced) {
            sei_idr_params(&sei)
        } else if forced {
            EncodePictureParams {
                force_idr: true,
                ..Default::default()
            }
        } else {
            EncodePictureParams::default()
        };
        if forced {
            // The observable for validating on-demand resync (issue #32): one
            // line per honoured request, absent when coalesced.
            let why = if client_req && idr_ok(min_idr_interval) {
                "client DG_REQUEST_IDR"
            } else {
                "drop recovery"
            };
            tracing::info!("wgc-zc: forced IDR at frame {frame_no} ({why})");
        }
        if periodic || forced {
            last_idr_at = Some(Instant::now());
        }

        let ob = &mut outputs[(frame_no as usize) % num_bufs];
        let enc0 = Instant::now();
        if let Err(e) = nv_session.encode_picture(&mut reg, ob, params) {
            let msg = format!("encode_picture: {e:?}");
            if is_transient(&msg) {
                tracing::debug!("wgc-zc transient: {msg}");
                continue;
            }
            return Err(anyhow!(msg));
        }
        t_enc += enc0.elapsed();
        let lock0 = Instant::now();
        let lock = match ob.lock() {
            Ok(l) => l,
            Err(e) => {
                let msg = format!("bitstream lock: {e:?}");
                if is_transient(&msg) {
                    continue;
                }
                return Err(anyhow!(msg));
            }
        };
        let keyframe = matches!(
            lock.picture_type(),
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        );
        let bytes = lock.data().to_vec();
        drop(lock);
        t_lock += lock0.elapsed();
        frame_no += 1;

        // #27: emit per-stage averages every ~2s. A stage near the ~16.7ms frame
        // budget is the bottleneck; a large `lock` specifically = the encode→retrieve
        // is serialized (submit N, then block for N) rather than pipelined.
        stage_frames += 1;
        if last_stage_log.elapsed() >= Duration::from_secs(2) {
            let win = last_stage_log.elapsed().as_secs_f64().max(1e-9);
            let n = stage_frames.max(1) as f64;
            tracing::info!(
                "wgc-zc over {win:.1}s (hdr={hdr}): fresh_capture={:.1}fps encode_ticks={:.1}fps \
                 | per-frame capture={:.1}ms pq={:.1}ms encode={:.1}ms lock={:.1}ms",
                fresh_frames as f64 / win,
                stage_frames as f64 / win,
                t_cap.as_secs_f64() * 1000.0 / n,
                t_pq.as_secs_f64() * 1000.0 / n,
                t_enc.as_secs_f64() * 1000.0 / n,
                t_lock.as_secs_f64() * 1000.0 / n,
            );
            t_cap = Duration::ZERO;
            t_pq = Duration::ZERO;
            t_enc = Duration::ZERO;
            t_lock = Duration::ZERO;
            stage_frames = 0;
            fresh_frames = 0;
            last_stage_log = Instant::now();
        }

        // Adaptive bitrate (issue #30): apply the control loop's target in place via an
        // NVENC RC-only reconfigure (no session reset) when it has moved. Force an IDR on
        // a DROP so the client re-syncs cleanly at the lower rate; a raise needs none.
        {
            let ceil = ctl.ceiling_kbps.load(Ordering::Relaxed).max(1);
            let want = match ctl.target_kbps.load(Ordering::Relaxed) {
                0 => applied_kbps,
                t => t.clamp(MIN_ADAPTIVE_KBPS.min(ceil), ceil),
            };
            if want != applied_kbps {
                let bits = want.saturating_mul(1000);
                match nv_session.reconfigure_bitrate(
                    NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR,
                    bits,
                    bits,
                    vbv_bits(want, fps),
                    want < applied_kbps,
                ) {
                    Ok(()) => {
                        tracing::info!(
                            "wgc-zc: bitrate {applied_kbps} -> {want} kbps (idr={})",
                            want < applied_kbps
                        );
                        applied_kbps = want;
                    }
                    Err(e) => {
                        tracing::warn!("wgc-zc: reconfigure_bitrate to {want} kbps failed: {e:?}")
                    }
                }
            }
        }

        // Blocking send = natural backpressure (the ffmpeg pipe's role). The
        // supervisor unblocks a stuck send by dropping the receiver in stop().
        if au_tx.send(Au { bytes, keyframe }).is_err() {
            break; // supervisor went away — clean teardown
        }
    }

    // Ordered teardown: bitstreams, then the registered resource, then the
    // NVENC session/encoder (EOS on session drop), then WGC. The borrow
    // structure enforces most of it; be explicit anyway.
    drop(outputs.drain(..));
    drop(reg);
    let _ = session.Close();
    Ok(())
}

/// The MDCV + CLL SEI array, built ONCE per session over payload buffers that
/// outlive every encode call. The array must stay alive across each
/// `encode_picture` that references it: NVENC reads `seiPayloadArray` during
/// the call, so pointing it at a helper's stack local is an access violation
/// (learned the hard way; the Linux original keeps the array in the caller's
/// frame).
fn sei_array(mdcv: &[u8], cll: &[u8]) -> [NV_ENC_SEI_PAYLOAD; 2] {
    [
        NV_ENC_SEI_PAYLOAD {
            payloadSize: mdcv.len() as u32,
            payloadType: 137, // mastering_display_colour_volume
            payload: mdcv.as_ptr() as *mut u8,
        },
        NV_ENC_SEI_PAYLOAD {
            payloadSize: cll.len() as u32,
            payloadType: 144, // content_light_level_info
            payload: cll.as_ptr() as *mut u8,
        },
    ]
}

/// Force-IDR params carrying `sei` (the Linux HDR spike's per-IDR attachment).
/// `sei` must outlive the `encode_picture` call these params feed.
fn sei_idr_params(sei: &[NV_ENC_SEI_PAYLOAD; 2]) -> EncodePictureParams {
    // SAFETY: POD zero-init; the caller keeps `sei` (and the payload Vecs it
    // points into) alive across the encode call.
    let mut hevc_pic: NV_ENC_PIC_PARAMS_HEVC = unsafe { std::mem::zeroed() };
    hevc_pic.seiPayloadArrayCnt = sei.len() as u32;
    hevc_pic.seiPayloadArray = sei.as_ptr() as *mut NV_ENC_SEI_PAYLOAD;
    EncodePictureParams {
        force_idr: true,
        codec_params: Some(CodecPictureParams::Hevc(hevc_pic)),
        ..Default::default()
    }
}

/// One default-usage texture with the given format and bind flags.
fn create_texture(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D")?;
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))
}

/// Smallest-correct f32 → IEEE half bits for the HDR test fill (normal range
/// only; the ramp values are 0..~12.5, well inside it).
fn f32_to_f16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let frac = bits & 0x007f_ffff;
    if v == 0.0 {
        return sign;
    }
    if exp <= 0 {
        return sign; // flush tiny to zero — fine for the test ramp
    }
    if exp >= 0x1f {
        return sign | 0x7c00; // inf
    }
    sign | ((exp as u16) << 10) | ((frac >> 13) as u16)
}

/// Shared encode-session bring-up for the test-pattern subcommands. Consumes
/// the encoder (`start_session` takes it by value; the session owns it).
fn test_session(
    encoder: Encoder,
    width: u32,
    height: u32,
    fps: u32,
    hdr: bool,
) -> Result<(Session, usize)> {
    let encode_guid = NV_ENC_CODEC_HEVC_GUID;
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| anyhow!("preset config: {e:?}"))?;
    preset_config.presetCfg.gopLength = fps.max(1);
    preset_config.presetCfg.frameIntervalP = 1;
    preset_config
        .presetCfg
        .encodeCodecConfig
        .hevcConfig
        .idrPeriod = fps.max(1);
    // SAFETY: HEVC preset — the union is the HEVC variant.
    unsafe {
        preset_config
            .presetCfg
            .encodeCodecConfig
            .hevcConfig
            .set_repeatSPSPPS(1);
    }
    if hdr {
        apply_hdr10(&mut preset_config.presetCfg);
    }
    let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
        + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
        .max(1) as usize;

    let mut init = EncoderInitParams::new(encode_guid, width, height);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps.max(1), 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);
    let session = encoder
        .start_session(
            if hdr {
                NV_ENC_BUFFER_FORMAT_ABGR10
            } else {
                NV_ENC_BUFFER_FORMAT_ARGB
            },
            init,
        )
        .map_err(|e| anyhow!("start session: {e:?}"))?;
    Ok((session, num_bufs))
}

/// `pharos-host nvenc-test`: encode an animated BGRA test pattern through the
/// D3D11 NVENC session to a raw HEVC file — proves driver loading, session
/// open, texture registration, and encode with no capture and no client (the
/// Windows sibling of the compositor's `nvenc-test` stage C).
pub fn encode_test_pattern(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    out_path: &str,
) -> Result<()> {
    use std::io::Write;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let (device, context) = wgc::create_d3d11_device()?;
    let encoder = unsafe { Encoder::initialize_with_directx(device.as_raw()) }
        .map_err(|e| anyhow!("NVENC D3D11 session: {e:?}"))?;
    let (session, num_bufs) = test_session(encoder, width, height, fps, false)?;

    let tex = create_texture(
        &device,
        width,
        height,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    )?;
    let mut reg = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            tex.as_raw(),
            0,
        )
        .map_err(|e| anyhow!("register D3D11 texture: {e:?}"))?;
    let mut outputs: Vec<_> = (0..num_bufs)
        .map(|_| session.create_output_bitstream())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow!("create bitstream: {e:?}"))?;

    let pitch = (width * 4) as usize;
    let mut host = vec![0u8; pitch * height as usize];
    let mut out = std::fs::File::create(out_path)?;
    tracing::info!("nvenc-test: {frames} frames {width}x{height} HEVC → {out_path}");

    // #27 probe: time only the GPU encode + bitstream-retrieve stages (the CPU
    // ramp fill and UpdateSubresource are outside this region), so the summary is
    // a clean read of the encode/retrieve throughput independent of capture.
    let (mut t_enc, mut t_lock) = (std::time::Duration::ZERO, std::time::Duration::ZERO);

    for i in 0..frames {
        // BGRA gradient sweep, animated so successive frames differ.
        let t = (255 * i / frames.max(1)) as u8;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let px = y * pitch + x * 4;
                host[px] = t; // B
                host[px + 1] = (255 * y / height.max(1) as usize) as u8; // G
                host[px + 2] = (255 * x / width.max(1) as usize) as u8; // R
                host[px + 3] = 255; // A
            }
        }
        unsafe {
            context.UpdateSubresource(&tex, 0, None, host.as_ptr().cast(), pitch as u32, 0);
        }
        let ob = &mut outputs[i as usize % num_bufs];
        let e0 = std::time::Instant::now();
        session
            .encode_picture(&mut reg, ob, Default::default())
            .map_err(|e| anyhow!("encode_picture: {e:?}"))?;
        t_enc += e0.elapsed();
        let l0 = std::time::Instant::now();
        let lock = ob.lock().map_err(|e| anyhow!("bitstream lock: {e:?}"))?;
        t_lock += l0.elapsed();
        out.write_all(lock.data())?;
    }
    drop(outputs.drain(..));
    drop(reg);
    let region = (t_enc + t_lock).as_secs_f64();
    tracing::info!(
        "nvenc-test timing @ {width}x{height}: encode={:.2}ms/f lock={:.2}ms/f → {:.1} fps \
         (encode+lock region, SDR 8-bit)",
        t_enc.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        t_lock.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        frames as f64 / region.max(1e-9),
    );
    tracing::info!("nvenc-test complete: {out_path}");
    Ok(())
}

/// `pharos-host nvenc-test-hdr`: drive the FULL HDR leg with no HDR display
/// and no client — fill an fp16 texture with a known scRGB luminance ramp on
/// the CPU, run the real scRGB→PQ shader pass into the 10-bit registered
/// texture, and encode HEVC Main10 with PQ/BT.2020 VUI + MDCV/CLL SEI on the
/// IDRs. ffprobe on the output verifies profile/colorimetry/side data; a
/// smooth decoded ramp verifies genuine 10-bit (banding = something truncated
/// to 8-bit — the compositor HDR spike's diagnostic).
pub fn encode_test_pattern_hdr(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    out_path: &str,
) -> Result<()> {
    use std::io::Write;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let (device, context) = wgc::create_d3d11_device()?;
    let encoder = unsafe { Encoder::initialize_with_directx(device.as_raw()) }
        .map_err(|e| anyhow!("NVENC D3D11 session: {e:?}"))?;
    let (session, num_bufs) = test_session(encoder, width, height, fps, true)?;

    let cap_tex = create_texture(
        &device,
        width,
        height,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
    )?;
    let enc_tex = create_texture(
        &device,
        width,
        height,
        DXGI_FORMAT_R10G10B10A2_UNORM,
        D3D11_BIND_RENDER_TARGET.0 as u32,
    )?;
    let pass = PqPass::new(&device, &cap_tex, &enc_tex, width, height)?;
    let mut reg = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            enc_tex.as_raw(),
            0,
        )
        .map_err(|e| anyhow!("register D3D11 texture: {e:?}"))?;
    let mut outputs: Vec<_> = (0..num_bufs)
        .map(|_| session.create_output_bitstream())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow!("create bitstream: {e:?}"))?;

    let (mdcv, cll) = hdr10_sei_payloads();
    let sei = sei_array(&mdcv, &cll);
    let pitch = (width * 8) as usize; // fp16 RGBA = 8 bytes/px
    let mut host = vec![0u8; pitch * height as usize];
    let mut out = std::fs::File::create(out_path)?;
    tracing::info!(
        "nvenc-test-hdr: {frames} frames {width}x{height} HEVC Main10 BT.2020/PQ → {out_path}"
    );

    // #27 probe: time the scRGB→PQ pass + GPU encode + bitstream retrieve (CPU ramp
    // fill and UpdateSubresource excluded) — the HDR counterpart to nvenc-test, so
    // the two summaries A/B the 10-bit vs 8-bit encode/retrieve cost with no capture.
    let (mut t_pq, mut t_enc, mut t_lock) = (
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
    );

    for i in 0..frames {
        // Horizontal scRGB luminance ramp 0..~10.0 (0..800 nits), animated.
        let phase = i as f32 / frames.max(1) as f32;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let lum = (x as f32 / width.max(1) as f32) * 10.0 * (0.5 + 0.5 * phase);
                let bits = f32_to_f16_bits(lum);
                let one = f32_to_f16_bits(1.0);
                let px = y * pitch + x * 8;
                host[px..px + 2].copy_from_slice(&bits.to_le_bytes());
                host[px + 2..px + 4].copy_from_slice(&bits.to_le_bytes());
                host[px + 4..px + 6].copy_from_slice(&bits.to_le_bytes());
                host[px + 6..px + 8].copy_from_slice(&one.to_le_bytes());
            }
        }
        unsafe {
            context.UpdateSubresource(&cap_tex, 0, None, host.as_ptr().cast(), pitch as u32, 0);
        }
        let p0 = std::time::Instant::now();
        pass.run(&context);
        t_pq += p0.elapsed();

        let params = if i.is_multiple_of(fps.max(1)) {
            sei_idr_params(&sei)
        } else {
            EncodePictureParams::default()
        };
        let ob = &mut outputs[i as usize % num_bufs];
        let e0 = std::time::Instant::now();
        session
            .encode_picture(&mut reg, ob, params)
            .map_err(|e| anyhow!("encode_picture: {e:?}"))?;
        t_enc += e0.elapsed();
        let l0 = std::time::Instant::now();
        let lock = ob.lock().map_err(|e| anyhow!("bitstream lock: {e:?}"))?;
        t_lock += l0.elapsed();
        out.write_all(lock.data())?;
    }
    drop(outputs.drain(..));
    drop(reg);
    let region = (t_pq + t_enc + t_lock).as_secs_f64();
    tracing::info!(
        "nvenc-test-hdr timing @ {width}x{height}: pq={:.2}ms/f encode={:.2}ms/f lock={:.2}ms/f \
         → {:.1} fps (pq+encode+lock region, HDR Main10 10-bit)",
        t_pq.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        t_enc.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        t_lock.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        frames as f64 / region.max(1e-9),
    );
    tracing::info!("nvenc-test-hdr complete: {out_path}");
    Ok(())
}
