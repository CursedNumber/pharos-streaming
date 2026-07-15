// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Native screen capture via **Windows.Graphics.Capture** (WGC).
//!
//! Replaces ffmpeg's `ddagrab` as the capture source: WGC grabs a monitor (by
//! `HMONITOR`, including an IddCx virtual display) into a D3D11 texture; we copy
//! it to a CPU-readable staging texture and hand tightly-packed BGRA frames to a
//! channel. The host feeds those to ffmpeg's rawvideo input for HEVC encode.
//! Benefits over ddagrab: the modern capture API (cleaner indirect-display
//! handling), per-window capture later, and a path to zero-copy D3D11 → encoder.
//!
//! Threading: a `FreeThreaded` frame pool is **polled** on a dedicated thread
//! that owns all the (non-Send) COM objects — only the finished `Vec<u8>` BGRA
//! frame crosses the channel, so we avoid wrapping COM handles in `Send`.

use std::sync::mpsc::{
    sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::core::ComInterface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{BOOL, HMODULE, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// One captured frame: tightly-packed BGRA, `width * height * 4` bytes.
// The encoder is sized from `WgcCapturer::start`'s returned (w, h), not these
// fields; only `bgra` is consumed downstream, so width/height stay unused.
#[allow(dead_code)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Result of polling the capturer: a fresh frame, nothing new (WGC is
/// change-driven, so a static screen is LEGITIMATE idle), or `Dead` — the
/// capture thread exited (device/session fault) and no frame will ever come
/// again. Callers must treat `Dead` as a pipeline fault: conflating it with
/// idle is how a wedged capture turns into a silent 60 fps freeze-frame (the
/// pacer would re-emit the held frame forever and nothing downstream notices).
pub enum Recv {
    Frame(Frame),
    Empty,
    Dead,
}

pub struct WgcCapturer {
    rx: Receiver<Frame>,
    _thread: std::thread::JoinHandle<()>,
}

impl WgcCapturer {
    /// Start capturing the monitor at `monitor_idx` (in `EnumDisplayMonitors`
    /// order). Returns the capture `(width, height)` and the capturer. Blocks
    /// until the capture thread has initialized (so the size is the item's true
    /// size) or reports an init error.
    pub fn start(monitor_idx: usize) -> Result<(u32, u32, Self)> {
        let hmon = monitor_by_index(monitor_idx)?;
        let (frame_tx, frame_rx) = sync_channel::<Frame>(2);
        let (size_tx, size_rx) =
            std::sync::mpsc::channel::<std::result::Result<(u32, u32), String>>();
        let thread = std::thread::Builder::new()
            .name("wgc-capture".into())
            .spawn(move || capture_loop(hmon, size_tx, frame_tx))?;
        let (w, h) = size_rx
            .recv()
            .map_err(|_| anyhow!("WGC capture thread died during init"))?
            .map_err(|e| anyhow!("WGC init: {e}"))?;
        Ok((
            w,
            h,
            Self {
                rx: frame_rx,
                _thread: thread,
            },
        ))
    }

    /// Block for the next frame, or [`Recv::Dead`] once the capture thread has
    /// stopped.
    #[allow(dead_code)] // the host loop will use the blocking form
    pub fn recv(&self) -> Recv {
        match self.rx.recv() {
            Ok(f) => Recv::Frame(f),
            Err(_) => Recv::Dead,
        }
    }

    /// Like [`recv`], but gives up after `dur` (WGC is change-driven, so a static
    /// screen produces no new frames — callers pace/hold the last frame).
    pub fn recv_timeout(&self, dur: Duration) -> Recv {
        match self.rx.recv_timeout(dur) {
            Ok(f) => Recv::Frame(f),
            Err(RecvTimeoutError::Timeout) => Recv::Empty,
            Err(RecvTimeoutError::Disconnected) => Recv::Dead,
        }
    }

    /// Non-blocking: the next queued frame if one is ready. The pacer drains with
    /// this to keep only the freshest frame (the bounded channel holds at most a
    /// couple, so stale frames don't accumulate).
    pub fn try_recv(&self) -> Recv {
        match self.rx.try_recv() {
            Ok(f) => Recv::Frame(f),
            Err(TryRecvError::Empty) => Recv::Empty,
            Err(TryRecvError::Disconnected) => Recv::Dead,
        }
    }
}

fn capture_loop(
    hmon: HMONITOR,
    size_tx: Sender<std::result::Result<(u32, u32), String>>,
    frame_tx: SyncSender<Frame>,
) {
    let run = || -> Result<()> {
        // WinRT (WGC) needs the thread in a COM apartment, or frame delivery
        // never starts even though setup succeeds. MTA suits a FreeThreaded pool.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let (device, context) = create_d3d11_device()?;
        let rt_device = create_rt_device(&device)?;
        let item = create_item_for_monitor(hmon)?;
        let size = item.Size()?;
        let (w, h) = (size.Width.max(0) as u32, size.Height.max(0) as u32);

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &rt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Same knob as the ddagrab path (issue #9): PHAROS_DRAW_MOUSE=0
        // captures cursor-free for clients that render their own pointer.
        if std::env::var("PHAROS_DRAW_MOUSE").is_ok_and(|v| v.trim() == "0") {
            session.SetIsCursorCaptureEnabled(false)?;
            tracing::info!("WGC: cursor capture disabled (PHAROS_DRAW_MOUSE=0)");
        }
        session.StartCapture()?;
        let _ = size_tx.send(Ok((w, h)));

        let row_bytes = (w as usize) * 4;
        let mut staging: Option<ID3D11Texture2D> = None;
        let mut got_first = false;
        let mut gap_start: Option<std::time::Instant> = None;
        let mut gap_logged = false;
        loop {
            let frame = match pool.TryGetNextFrame() {
                Ok(f) => f,
                // windows-rs surfaces WinRT's "no frame available" null return
                // as Err with an OK code; anything else is a real device or
                // session fault. Treating faults as idle is the silent-freeze
                // bug: the thread would poll forever while the pacer re-emits
                // the last frame as a perfectly healthy-looking stream.
                Err(e) if e.code().is_ok() => {
                    let start = *gap_start.get_or_insert_with(std::time::Instant::now);
                    if !gap_logged && start.elapsed() > Duration::from_secs(2) {
                        gap_logged = true;
                        tracing::debug!("WGC: no frames for 2s (change-driven idle)");
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(e) => return Err(anyhow!("TryGetNextFrame: {e:?} (capture session dead)")),
            };
            if let Some(start) = gap_start.take() {
                if gap_logged {
                    tracing::info!(
                        "WGC: frames resumed after a {:.1}s gap",
                        start.elapsed().as_secs_f64()
                    );
                }
                gap_logged = false;
            }
            if !got_first {
                tracing::info!("WGC: first frame retrieved");
                got_first = true;
            }
            // WGC does NOT fault on a display-mode change: it keeps delivering
            // frames at the stale pool size, silently cropping the new desktop
            // to the top-left corner. Make it a fault (thread exit → channel
            // Dead → pacer stops → supervisor respawns at the new size).
            let cs = frame.ContentSize()?;
            let (cw, ch) = (cs.Width.max(0) as u32, cs.Height.max(0) as u32);
            if (cw, ch) != (w, h) {
                let _ = frame.Close();
                return Err(anyhow!(
                    "capture size changed {w}x{h} → {cw}x{ch} (display mode change)"
                ));
            }
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

            if staging.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { texture.GetDesc(&mut desc) };
                let sdesc = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: desc.SampleDesc,
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut tex = None;
                unsafe { device.CreateTexture2D(&sdesc, None, Some(&mut tex))? };
                staging = tex;
            }
            let staging_ref = staging.as_ref().unwrap();

            unsafe { context.CopyResource(staging_ref, &texture) };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe { context.Map(staging_ref, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };

            let mut bgra = vec![0u8; row_bytes * (h as usize)];
            unsafe {
                let src = mapped.pData as *const u8;
                let pitch = mapped.RowPitch as usize;
                for y in 0..(h as usize) {
                    std::ptr::copy_nonoverlapping(
                        src.add(y * pitch),
                        bgra.as_mut_ptr().add(y * row_bytes),
                        row_bytes,
                    );
                }
                context.Unmap(staging_ref, 0);
            }
            // Return the frame's buffer to the pool, or it starves after
            // `numberofbuffers` frames (Rust's drop releases the COM ref but
            // does not call IClosable::Close).
            let _ = frame.Close();

            match frame_tx.try_send(Frame {
                width: w,
                height: h,
                bgra,
            }) {
                Ok(()) => {}
                // Full → drop this frame to keep latency low.
                Err(TrySendError::Full(_)) => {}
                // Receiver gone → the host stopped; end the capture.
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        let _ = session.Close();
        Ok(())
    };

    if let Err(e) = run() {
        // If init failed before sending the size, surface it to `start`.
        let _ = size_tx.send(Err(format!("{e:?}")));
        tracing::error!("WGC capture loop ended: {e:?}");
    }
}

pub(crate) fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?,
        context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?,
    ))
}

pub(crate) fn create_rt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    Ok(inspectable.cast()?)
}

pub(crate) fn create_item_for_monitor(hmon: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    Ok(unsafe { interop.CreateForMonitor(hmon)? })
}

pub(crate) fn monitor_by_index(idx: usize) -> Result<HMONITOR> {
    let mons = enum_monitors();
    mons.get(idx)
        .copied()
        .ok_or_else(|| anyhow!("monitor index {idx} out of range ({} present)", mons.len()))
}

fn enum_monitors() -> Vec<HMONITOR> {
    let mut v: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_proc),
            LPARAM(&mut v as *mut _ as isize),
        );
    }
    v
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let v = &mut *(lparam.0 as *mut Vec<HMONITOR>);
    v.push(hmon);
    TRUE
}

/// Monitor count + each monitor's pixel rect, for the `wgc-test` listing.
pub fn list_monitors() -> Vec<(u32, u32)> {
    enum_monitors()
        .into_iter()
        .map(|m| monitor_size(m).unwrap_or((0, 0)))
        .collect()
}

fn monitor_size(hmon: HMONITOR) -> Result<(u32, u32)> {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetMonitorInfoW(hmon, &mut mi) };
    if !ok.as_bool() {
        return Err(anyhow!("GetMonitorInfoW failed"));
    }
    Ok((
        (mi.rcMonitor.right - mi.rcMonitor.left) as u32,
        (mi.rcMonitor.bottom - mi.rcMonitor.top) as u32,
    ))
}
