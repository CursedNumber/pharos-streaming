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
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6, DXGI_OUTPUT_DESC,
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
