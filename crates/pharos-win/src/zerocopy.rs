// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Zero-copy WGC → NVENC HEVC pipeline (issue #12).
//!
//! Replaces the ffmpeg subprocess on the WGC path: the capture pool's D3D11
//! texture is GPU-copied into one persistent registered texture and encoded
//! in-process by NVENC through the vendored SDK (D3D11 device session), so no
//! frame bytes ever cross the CPU. The 4K cost that sank the old path (33 MB
//! staging readback + 33 MB pipe write + CPU BGRA→NV12 per frame) disappears;
//! the only per-frame work is one GPU CopyResource and the encode itself.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB, NV_ENC_CODEC_HEVC_GUID,
    NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_TYPE, NV_ENC_PRESET_P4_GUID,
    NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{Encoder, EncoderInitParams};
use windows::core::{ComInterface, Interface};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;

use crate::wgc;

/// One encoded access unit. NVENC's lock-bitstream returns exactly one AU per
/// encoded picture, so no Annex-B splitting is needed on this path; the
/// keyframe flag (IDR or I) feeds `QuicStreamer::send_frame`'s reserve.
pub struct Au {
    pub bytes: Vec<u8>,
    pub keyframe: bool,
}

/// Handle the supervisor owns: AUs come out of `recv`, faults surface as
/// [`ZcRecv::Dead`] with a reason in `fault_reason`, and `stop` tears the
/// encoder thread down without any process to kill (the in-process analogue
/// of the drop(enc)-kills-ffmpeg contract).
pub struct ZcPipeline {
    pub size: (u32, u32),
    au_rx: Option<Receiver<Au>>,
    stop: Arc<AtomicBool>,
    fault: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Tri-state receive, same contract as `wgc::Recv`: `Dead` is a pipeline
/// fault, never idle.
pub enum ZcRecv {
    Au(Au),
    Empty,
    Dead,
}

impl ZcPipeline {
    /// Start capture + in-process encode for `monitor_idx` at `fps`. Blocks
    /// until the capture item and NVENC session are up (so `size` is the
    /// item's true size) or reports the init error.
    pub fn start(monitor_idx: usize, fps: u32) -> Result<Self> {
        let hmon = wgc::monitor_by_index(monitor_idx)?;
        let stop = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        let (au_tx, au_rx) = sync_channel::<Au>(8);
        let (init_tx, init_rx) =
            std::sync::mpsc::channel::<std::result::Result<(u32, u32), String>>();
        let (stop_t, fault_t) = (stop.clone(), fault.clone());
        let thread = std::thread::Builder::new()
            .name("wgc-zc".into())
            .spawn(move || {
                if let Err(e) = zc_loop(hmon, fps, &stop_t, init_tx.clone(), au_tx) {
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
        Ok(Self {
            size,
            au_rx: Some(au_rx),
            stop,
            fault,
            thread: Some(thread),
        })
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
    }
}

/// Transient NVENC errors: skip the frame and keep streaming (the Linux
/// host's taxonomy — everything else is fatal for the generation).
fn is_transient(msg: &str) -> bool {
    msg.contains("NeedMoreInput") || msg.contains("EncoderBusy") || msg.contains("LockBusy")
}

/// CBR bitrate for the session, kbps. Matches the ffmpeg path's `-rc cbr`
/// spirit with an explicit budget: 20 Mbps default (comfortable 4K60 on a
/// LAN), `PHAROS_BITRATE_KBPS` to tune without a rebuild.
fn bitrate_kbps() -> u32 {
    std::env::var("PHAROS_BITRATE_KBPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000)
}

/// The capture+encode thread body. Owns every COM/NVENC object; returns Err
/// only for init or fatal runtime faults (recorded by the spawn wrapper).
fn zc_loop(
    hmon: windows::Win32::Graphics::Gdi::HMONITOR,
    fps: u32,
    stop: &AtomicBool,
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

    let pool = windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
        &rt_device,
        windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized,
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
    // point — CopyResource below is GPU→GPU on one device, nothing crosses
    // the CPU. SAFETY: `device` outlives encoder/session/reg (single scope,
    // declared first; rustc enforces the borrow chain on the NVENC side).
    let encoder = unsafe { Encoder::initialize_with_directx(device.as_raw()) }
        .map_err(|e| anyhow!("NVENC D3D11 session: {e:?}"))?;

    let encode_guid = NV_ENC_CODEC_HEVC_GUID;
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| anyhow!("preset config: {e:?}"))?;

    // The Linux host's low-latency recipe: 1s GOP with an IDR each period and
    // VPS/SPS/PPS repeated on every keyframe, so a mid-stream joiner resyncs
    // in <=1s; plus explicit 1-frame-VBV CBR (multi.rs's shape).
    let cfg = &mut preset_config.presetCfg;
    cfg.gopLength = fps.max(1);
    cfg.frameIntervalP = 1;
    cfg.encodeCodecConfig.hevcConfig.idrPeriod = fps.max(1);
    // SAFETY: the union is the HEVC variant (preset fetched for the HEVC GUID).
    unsafe {
        cfg.encodeCodecConfig.hevcConfig.set_repeatSPSPPS(1);
    }
    let kbps = bitrate_kbps();
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    cfg.rcParams.averageBitRate = kbps.saturating_mul(1000);
    cfg.rcParams.maxBitRate = kbps.saturating_mul(1000);
    cfg.rcParams.vbvBufferSize = kbps.saturating_mul(1000) / fps.max(1);
    cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;

    let mut init = EncoderInitParams::new(encode_guid, w, h);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps.max(1), 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);
    let nv_session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT_ARGB, init)
        .map_err(|e| anyhow!("start session: {e:?}"))?;

    // One persistent BGRA texture, registered with NVENC once. Each tick the
    // newest pool frame is CopyResource'd into it (and the pool frame Closed
    // immediately, keeping the 2-buffer pool healthy); a tick with no fresh
    // frame just re-encodes the previous contents — the pacer's hold-frame
    // semantics without any CPU bytes.
    let enc_tex = create_bgra_texture(&device, w, h)?;
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
        "wgc-zc: {w}x{h}@{fps} HEVC in-process (CBR {kbps} kbps, gop {}); zero CPU copies",
        fps.max(1)
    );

    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let mut next = Instant::now() + interval;
    let mut got_first = false;
    let mut frame_no: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Drain the pool to the newest frame; GPU-copy it into the registered
        // texture and return the pool buffer immediately.
        loop {
            let frame = match pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(e) if e.code().is_ok() => break, // empty — change-driven idle
                Err(e) => return Err(anyhow!("TryGetNextFrame: {e:?} (capture session dead)")),
            };
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
            unsafe { context.CopyResource(&enc_tex, &texture) };
            let _ = frame.Close();
            got_first = true;
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

        let ob = &mut outputs[(frame_no as usize) % num_bufs];
        if let Err(e) = nv_session.encode_picture(&mut reg, ob, Default::default()) {
            let msg = format!("encode_picture: {e:?}");
            if is_transient(&msg) {
                tracing::debug!("wgc-zc transient: {msg}");
                continue;
            }
            return Err(anyhow!(msg));
        }
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
        frame_no += 1;

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
    let _ = &mut reg;
    drop(reg);
    let _ = session.Close();
    Ok(())
}

/// One default-usage BGRA render-target texture (NVENC registration wants a
/// renderable surface).
fn create_bgra_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .context("CreateTexture2D (encoder input)")?;
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))
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

    let mut init = EncoderInitParams::new(encode_guid, width, height);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps.max(1), 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT_ARGB, init)
        .map_err(|e| anyhow!("start session: {e:?}"))?;

    let tex = create_bgra_texture(&device, width, height)?;
    let mut reg = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            tex.as_raw(),
            0,
        )
        .map_err(|e| anyhow!("register D3D11 texture: {e:?}"))?;

    let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
        + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
        .max(1) as usize;
    let mut outputs: Vec<_> = (0..num_bufs)
        .map(|_| session.create_output_bitstream())
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow!("create bitstream: {e:?}"))?;

    let pitch = (width * 4) as usize;
    let mut host = vec![0u8; pitch * height as usize];
    let mut out = std::fs::File::create(out_path)?;
    tracing::info!("nvenc-test: {frames} frames {width}x{height} HEVC → {out_path}");

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
        session
            .encode_picture(&mut reg, ob, Default::default())
            .map_err(|e| anyhow!("encode_picture: {e:?}"))?;
        let lock = ob.lock().map_err(|e| anyhow!("bitstream lock: {e:?}"))?;
        out.write_all(lock.data())?;
    }
    drop(outputs.drain(..));
    drop(reg);
    tracing::info!("nvenc-test complete: {out_path}");
    Ok(())
}
