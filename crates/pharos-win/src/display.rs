// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! DXGI output geometry for the `ddagrab` capture path (issue #1).
//!
//! ffmpeg's `ddagrab=output_idx=N` duplicates output `N` of the default D3D11
//! adapter, so the stream geometry we announce must come from the same DXGI
//! enumeration; `EnumDisplayMonitors` order can differ on multi-adapter or
//! multi-monitor rigs.

use anyhow::{anyhow, Context, Result};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_OUTPUT_DESC};

/// Pixel size of DXGI output `output_idx` on adapter 0: the monitor ffmpeg's
/// `ddagrab` captures, and therefore the encoded stream size (the ddagrab
/// pipeline encodes at native size; nothing scales).
pub fn dda_output_size(output_idx: u32) -> Result<(u32, u32)> {
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }.context("CreateDXGIFactory1")?;
    let adapter = unsafe { factory.EnumAdapters1(0) }.context("DXGI adapter 0")?;
    let output = unsafe { adapter.EnumOutputs(output_idx) }
        .with_context(|| format!("DXGI output {output_idx} on adapter 0"))?;
    let mut desc = DXGI_OUTPUT_DESC::default();
    unsafe { output.GetDesc(&mut desc) }.context("DXGI output GetDesc")?;
    let r = desc.DesktopCoordinates;
    let (w, h) = (
        (r.right - r.left).max(0) as u32,
        (r.bottom - r.top).max(0) as u32,
    );
    if w == 0 || h == 0 {
        return Err(anyhow!(
            "DXGI output {output_idx} reports {w}x{h} (detached or disabled?)"
        ));
    }
    Ok((w, h))
}
