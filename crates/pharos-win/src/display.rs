// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! DXGI output geometry for the `ddagrab` capture path (issue #1).
//!
//! ffmpeg's `ddagrab=output_idx=N` duplicates output `N` of the default D3D11
//! adapter, so the stream geometry we announce must come from the same DXGI
//! enumeration; `EnumDisplayMonitors` order can differ on multi-adapter or
//! multi-monitor rigs.

use anyhow::{anyhow, Context, Result};
use windows::core::{ComInterface, PCWSTR};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6, DXGI_ADAPTER_DESC1, DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS, HMONITOR,
};

/// Pixel size of DXGI output `output_idx` on adapter 0: the monitor ffmpeg's
/// `ddagrab` captures, and therefore the encoded stream size (the ddagrab
/// pipeline encodes at native size; nothing scales).
///
/// The size comes from the output's CURRENT DISPLAY MODE, not its desktop
/// coordinates: `DesktopCoordinates` (like `GetMonitorInfo`) is DPI-virtualized
/// for a non-DPI-aware process (a 3840x2160 desktop at 300% scale reads
/// 1280x720), while Desktop Duplication frames are always physical pixels.
/// main() opts the process into per-monitor DPI awareness (issue #8), which
/// also fixes those queries, but `EnumDisplaySettings` reports the mode and is
/// correct under EVERY awareness state; keep it as the authoritative source.
pub fn dda_output_size(output_idx: u32) -> Result<(u32, u32)> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.context("CreateDXGIFactory1")?;
    let adapter = unsafe { factory.EnumAdapters1(0) }.context("DXGI adapter 0")?;
    let output = unsafe { adapter.EnumOutputs(output_idx) }
        .with_context(|| format!("DXGI output {output_idx} on adapter 0"))?;
    let mut desc = DXGI_OUTPUT_DESC::default();
    unsafe { output.GetDesc(&mut desc) }.context("DXGI output GetDesc")?;

    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(desc.DeviceName.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
    };
    if !ok.as_bool() {
        return Err(anyhow!(
            "EnumDisplaySettingsW({}) failed",
            String::from_utf16_lossy(&desc.DeviceName)
        ));
    }
    let (w, h) = (dm.dmPelsWidth, dm.dmPelsHeight);
    if w == 0 || h == 0 {
        return Err(anyhow!(
            "DXGI output {output_idx} reports mode {w}x{h} (detached or disabled?)"
        ));
    }
    Ok((w, h))
}

/// PCI vendor ids as DXGI adapter descriptors report them.
pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;

/// The GPU vendor whose hardware encoder this machine should default to:
/// walk every DXGI adapter and prefer NVIDIA (NVENC) over AMD (AMF) over
/// Intel (Quick Sync), so a discrete GPU beats the iGPU regardless of
/// adapter enumeration order. Software adapters (WARP) are skipped. `None`
/// when no known hardware vendor is present.
pub fn preferred_encoder_vendor() -> Option<u32> {
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;
    let rank = |v: u32| match v {
        VENDOR_NVIDIA => 3,
        VENDOR_AMD => 2,
        VENDOR_INTEL => 1,
        _ => 0,
    };
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.ok()?;
    let mut best: Option<u32> = None;
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        let mut desc = DXGI_ADAPTER_DESC1::default();
        if unsafe { adapter.GetDesc1(&mut desc) }.is_err() {
            continue;
        }
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0 {
            continue;
        }
        if rank(desc.VendorId) > best.map_or(0, rank) {
            best = Some(desc.VendorId);
        }
    }
    best.filter(|&v| rank(v) > 0)
}

/// Is Windows HDR active on the output that owns `hmon`? Walks every DXGI
/// adapter's outputs to find the matching monitor and asks `IDXGIOutput6::
/// GetDesc1`: HDR-on shows as the RGB_FULL_G2084_NONE_P2020 (PQ/BT.2020)
/// color space. `Ok(false)` for SDR outputs; `Err` if the monitor cannot be
/// resolved (the caller should treat that as SDR and say why).
pub fn monitor_hdr_active(hmon: HMONITOR) -> Result<bool> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.context("CreateDXGIFactory1")?;
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(ad) => ad,
            Err(_) => break,
        };
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(out) => out,
                Err(_) => break,
            };
            let mut desc = DXGI_OUTPUT_DESC::default();
            if unsafe { output.GetDesc(&mut desc) }.is_err() || desc.Monitor != hmon {
                continue;
            }
            let out6: IDXGIOutput6 = output.cast().context("IDXGIOutput6 (needs Win10 1703+)")?;
            let mut d1 = Default::default();
            unsafe { out6.GetDesc1(&mut d1) }.context("GetDesc1")?;
            return Ok(d1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
        }
    }
    Err(anyhow!("monitor not found among DXGI outputs"))
}
