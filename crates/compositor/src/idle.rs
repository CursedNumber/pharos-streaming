// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Idle "no app running" screen for the streamed output.
//!
//! When NO client window is mapped, the compositor would otherwise stream a flat
//! dark fill — indistinguishable from a game that has gone to a black screen. So
//! we paint the Pharos lighthouse + a status message instead: a black stream then
//! unambiguously means "the app itself went black", while the logo means "you're
//! on the empty desktop, nothing is running".
//!
//! Cost: the logo + text are rasterized + composited into ONE output-sized buffer
//! ONCE at pipeline build (pure CPU, cached). The caller renders this only when
//! no window overlaps that output (its per-output element list is empty); while
//! any app window is mapped the idle path is skipped entirely, so it adds
//! nothing to the in-game render. The per-frame work when idle is just one
//! textured quad at an animated alpha (no re-rasterization).

use anyhow::{Context, Result};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::Transform;

use ab_glyph::{Font, FontVec, Glyph, GlyphId, PxScale, ScaleFont};

/// The streamed idle screen: the logo + status baked into one output-sized buffer.
pub struct IdleScreen {
    buffer: MemoryRenderBuffer,
}

/// Background fill behind the idle logo — a touch darker than the desktop fill so
/// the lighthouse reads as the focal point.
pub const BG: [f32; 4] = [0.02, 0.03, 0.06, 1.0];

impl IdleScreen {
    /// Bake the idle screen for a `w`×`h` output. Pure CPU (rasterize + composite
    /// once); cache the result on the pipeline. The logo always renders; the text
    /// is best-effort (omitted if no system sans font is found).
    pub fn build(w: u32, h: u32) -> Result<Self> {
        let (w, h) = (w.max(1), h.max(1));
        // Transparent premultiplied-RGBA canvas (byte order R,G,B,A == Abgr8888).
        let mut canvas = vec![0u8; (w as usize) * (h as usize) * 4];

        // Watercolour lighthouse (the Linux app icon): centred, sitting a little
        // above centre. Its transparent background composites over the dark fill.
        let logo_px = (w.min(h) / 2).clamp(128, 768);
        // `text_top` = where the status text begins: just under the logo, or a
        // sensible default if the logo couldn't be decoded.
        let mut text_top = h / 2;
        if let Some((lw, lh, lrgba)) = load_logo(logo_px) {
            let lx = w.saturating_sub(lw) / 2;
            let ly = (h.saturating_sub(lh) / 2).saturating_sub(h / 10);
            blit_premul(&mut canvas, w, h, lx, ly, &lrgba, lw, lh);
            text_top = (ly + lh).saturating_add(h / 30);
        }

        // Status text below the logo.
        if let Some(font) = load_sans() {
            let mut y = text_top;
            let title_px = (h as f32 * 0.06).clamp(28.0, 96.0);
            let (tw, th, cov) = rasterize_text(&font, "Pharos", title_px);
            if tw > 0 {
                let x = (w as usize).saturating_sub(tw) as u32 / 2;
                blit_text(&mut canvas, w, h, x, y, &cov, tw, th, [0.93, 0.95, 0.98]);
                y += th as u32 + (title_px * 0.3) as u32;
            }
            let sub_px = (h as f32 * 0.032).clamp(16.0, 48.0);
            let (sw, sh, scov) = rasterize_text(&font, "No app is running", sub_px);
            if sw > 0 {
                let x = (w as usize).saturating_sub(sw) as u32 / 2;
                blit_text(&mut canvas, w, h, x, y, &scov, sw, sh, [0.60, 0.69, 0.78]);
            }
        }

        let buffer = MemoryRenderBuffer::from_slice(
            &canvas,
            Fourcc::Abgr8888,
            (w as i32, h as i32),
            1,
            Transform::Normal,
            None,
        );
        Ok(Self { buffer })
    }

    /// The full-screen idle render element at `alpha` (drives the pulse). Drawn at
    /// (0,0); the buffer is already output-sized so no scaling.
    pub fn element(
        &self,
        renderer: &mut GlesRenderer,
        alpha: f32,
    ) -> Result<MemoryRenderBufferRenderElement<GlesRenderer>> {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (0.0, 0.0),
            &self.buffer,
            Some(alpha),
            None,
            None,
            Kind::Unspecified,
        )
        .context("idle render element")
    }
}

/// Gentle "breathing" glow alpha (the lighthouse lantern), wall-clock driven so
/// it's frame-rate independent. ~4s period, ~0.6..1.0 so text stays readable.
pub fn pulse(t_secs: f32) -> f32 {
    0.8 + 0.2 * (t_secs * std::f32::consts::TAU / 4.0).sin()
}

/// Alpha-"over" a premultiplied-RGBA `src` onto the (premultiplied) canvas.
#[allow(clippy::too_many_arguments)]
fn blit_premul(canvas: &mut [u8], cw: u32, ch: u32, x: u32, y: u32, src: &[u8], sw: u32, sh: u32) {
    for sy in 0..sh {
        let dy = y + sy;
        if dy >= ch {
            break;
        }
        for sx in 0..sw {
            let dx = x + sx;
            if dx >= cw {
                break;
            }
            let si = ((sy * sw + sx) * 4) as usize;
            let di = ((dy * cw + dx) * 4) as usize;
            let inv = 255 - src[si + 3] as u32;
            for c in 0..4 {
                let out = src[si + c] as u32 + canvas[di + c] as u32 * inv / 255;
                canvas[di + c] = out.min(255) as u8;
            }
        }
    }
}

/// Composite glyph `coverage` (0..255) as premultiplied coloured text onto the canvas.
#[allow(clippy::too_many_arguments)]
fn blit_text(
    canvas: &mut [u8],
    cw: u32,
    ch: u32,
    x: u32,
    y: u32,
    cov: &[u8],
    tw: usize,
    th: usize,
    color: [f32; 3],
) {
    for ty in 0..th {
        let dy = y + ty as u32;
        if dy >= ch {
            break;
        }
        for tx in 0..tw {
            let dx = x + tx as u32;
            if dx >= cw {
                break;
            }
            let a = cov[ty * tw + tx] as u32;
            if a == 0 {
                continue;
            }
            let di = ((dy * cw + dx) * 4) as usize;
            let inv = 255 - a;
            let src = [
                (color[0] * a as f32) as u32,
                (color[1] * a as f32) as u32,
                (color[2] * a as f32) as u32,
                a,
            ];
            for c in 0..4 {
                let out = src[c] + canvas[di + c] as u32 * inv / 255;
                canvas[di + c] = out.min(255) as u8;
            }
        }
    }
}

/// Rasterize `text` at `px` into a tight coverage bitmap `(w, h, coverage)`.
/// Mirrors the proven `trireme-kms` badge text path.
fn rasterize_text(font: &FontVec, text: &str, px: f32) -> (usize, usize, Vec<u8>) {
    let scale = PxScale::from(px);
    let sf = font.as_scaled(scale);
    let mut caret_x = 2.0f32;
    let mut prev: Option<GlyphId> = None;
    let mut glyphs: Vec<Glyph> = Vec::new();
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            caret_x += sf.kern(p, id);
        }
        glyphs.push(id.with_scale_and_position(scale, ab_glyph::point(caret_x, sf.ascent())));
        caret_x += sf.h_advance(id);
        prev = Some(id);
    }
    let w = (caret_x + 2.0).ceil() as usize;
    let h = (sf.ascent() - sf.descent()).ceil() as usize;
    if w == 0 || h == 0 {
        return (0, 0, Vec::new());
    }
    let mut cov = vec![0u8; w * h];
    for g in glyphs {
        if let Some(o) = font.outline_glyph(g) {
            let bb = o.px_bounds();
            o.draw(|gx, gy, c| {
                let px = gx as i32 + bb.min.x as i32;
                let py = gy as i32 + bb.min.y as i32;
                if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                    let i = py as usize * w + px as usize;
                    cov[i] = cov[i].max((c * 255.0) as u8);
                }
            });
        }
    }
    (w, h, cov)
}

/// Decode the watercolour app icon (a transparent PNG), scale it to `px`×`px`,
/// and PREMULTIPLY it (the `image` crate yields straight alpha; the canvas +
/// blit are premultiplied). Returns `(w, h, rgba)` or `None` if decode fails.
fn load_logo(px: u32) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(pharos_assets::PHAROS_APP)
        .map_err(|e| tracing::warn!("idle logo decode: {e}"))
        .ok()?;
    // Triangle = fast bilinear; this is a one-time startup resize. resize_exact
    // keeps it square (the source is 1024×1024).
    let mut rgba = img
        .resize_exact(px, px, image::imageops::FilterType::Triangle)
        .to_rgba8()
        .into_raw();
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        p[0] = (p[0] as u32 * a / 255) as u8;
        p[1] = (p[1] as u32 * a / 255) as u8;
        p[2] = (p[2] as u32 * a / 255) as u8;
    }
    Some((px, px, rgba))
}

/// Load a system sans-serif TTF (DejaVu/Liberation/Noto), trying common paths.
fn load_sans() -> Option<FontVec> {
    const PATHS: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/NotoSans-Regular.ttf",
    ];
    for p in PATHS {
        if let Ok(bytes) = std::fs::read(p) {
            if let Ok(f) = FontVec::try_from_vec(bytes) {
                tracing::info!("idle screen font: {p}");
                return Some(f);
            }
        }
    }
    tracing::warn!("no system sans font found; idle screen text disabled");
    None
}
