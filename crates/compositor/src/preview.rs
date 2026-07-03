// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Live-preview JPEG writer.
//!
//! The render loop hands a captured RGBA frame here (opt-in + 20s-capped, ~2fps,
//! so only short bursts), and we downscale + JPEG it and atomically write it to
//! [`control_ipc::preview_path`] for the panel to display. Best-effort: any
//! error is logged at debug and dropped — a preview hiccup must never disturb
//! the stream.

use std::io::Write;

/// Max preview width (height follows the source aspect). Small + lightweight.
pub const PREVIEW_MAX_W: u32 = 480;

/// Downscale a (possibly row-padded) RGBA frame and write it as a JPEG to the
/// shared preview path. `stride` is the source bytes-per-row (≥ `src_w*4`).
pub fn write_preview(rgba: &[u8], src_w: u32, src_h: u32, stride: usize) {
    if let Err(e) = try_write(rgba, src_w, src_h, stride) {
        tracing::debug!("preview write skipped: {e:#}");
    }
}

fn try_write(rgba: &[u8], src_w: u32, src_h: u32, stride: usize) -> anyhow::Result<()> {
    let (sw, sh) = (src_w as usize, src_h as usize);
    let row = sw * 4;
    anyhow::ensure!(
        sw > 0 && sh > 0 && stride >= row && rgba.len() >= stride * sh,
        "short/empty preview buffer"
    );
    // Repack to tightly-packed RGBA, dropping any GL row padding.
    let packed: Vec<u8> = if stride == row {
        rgba[..row * sh].to_vec()
    } else {
        let mut p = Vec::with_capacity(row * sh);
        for y in 0..sh {
            p.extend_from_slice(&rgba[y * stride..y * stride + row]);
        }
        p
    };
    let img = image::RgbaImage::from_raw(src_w, src_h, packed)
        .ok_or_else(|| anyhow::anyhow!("RgbaImage::from_raw"))?;

    // Downscale to PREVIEW_MAX_W wide, keeping aspect (never upscale).
    let dst_w = PREVIEW_MAX_W.min(src_w);
    let dst_h = ((dst_w as u64 * src_h as u64) / sw as u64).max(1) as u32;
    let small = image::imageops::resize(&img, dst_w, dst_h, image::imageops::FilterType::Triangle);

    // JPEG has no alpha — flatten to RGB, encode at a modest quality.
    let rgb = image::DynamicImage::ImageRgba8(small).to_rgb8();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 80).encode_image(&rgb)?;

    // Atomic write: tmp + rename so the panel never reads a half-written file.
    let path = control_ipc::preview_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("jpg.tmp");
    std::fs::File::create(&tmp)?.write_all(&buf)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
