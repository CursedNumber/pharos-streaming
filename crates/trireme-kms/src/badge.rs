// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! The idle "badge": a crescent-moon + star + pearl mark with a "Ready to
//! connect" label to its left, composited (on black) into one RGBA texture at
//! startup. The presenter then draws this badge at a slowly-drifting position
//! so the idle screen says "waiting, alive" without burn-in. The visual is a
//! legacy mark from the prior brand; a Trireme-themed redesign is a follow-up.
//!
//! The logo is rasterized procedurally (resolution-independent, no asset file);
//! the text is rasterized from a system **sans** font via `ab_glyph`.

use ab_glyph::{Font, FontVec, Glyph, GlyphId, PxScale, ScaleFont};

/// A composited badge image (RGBA8, premultiplied on black).
pub struct Badge {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

// --- palette (0..1) ---------------------------------------------------------
const SILVER: [f32; 3] = [0.80, 0.84, 0.93];
const NAVY: [f32; 3] = [0.09, 0.13, 0.30];
const STAR: [f32; 3] = [0.92, 0.96, 1.00];
const TEXT: [f32; 3] = [0.50, 0.50, 0.46]; // half-bright ivory

const LABEL: &str = "Ready to connect";

/// Build the badge. `logo_px` sets the logo (and badge) height in pixels.
pub fn build(logo_px: u32) -> Badge {
    let l = logo_px as usize;
    let (text_w, text_h, text_cov) = rasterize_label(l as f32 * 0.42);
    let gap = l / 8;
    let bw = text_w + gap + l;
    let bh = l;
    let mut rgba = vec![0u8; bw * bh * 4];

    // Text: vertically centred in the left region.
    let ty = (bh.saturating_sub(text_h)) / 2;
    for y in 0..text_h {
        for x in 0..text_w {
            let cov = text_cov[y * text_w + x] as f32 / 255.0;
            if cov > 0.0 {
                put(&mut rgba, bw, x, ty + y, TEXT, cov);
            }
        }
    }

    // Logo: square region on the right.
    let lx = text_w + gap;
    rasterize_logo(&mut rgba, bw, lx, 0, l);

    Badge {
        w: bw as u32,
        h: bh as u32,
        rgba,
    }
}

/// Alpha-over a colour onto the black-backed RGBA buffer.
fn put(buf: &mut [u8], stride_px: usize, x: usize, y: usize, col: [f32; 3], a: f32) {
    let i = (y * stride_px + x) * 4;
    for c in 0..3 {
        let bg = buf[i + c] as f32 / 255.0;
        let v = bg * (1.0 - a) + col[c] * a;
        buf[i + c] = (v.clamp(0.0, 1.0) * 255.0) as u8;
    }
    buf[i + 3] = 255;
}

/// Procedurally rasterize the crescent + star + pearl into the square logo
/// region at (`ox`,`oy`), side `l`.
fn rasterize_logo(buf: &mut [u8], stride_px: usize, ox: usize, oy: usize, l: usize) {
    let px = 2.0 / l as f32; // normalized units per pixel
    let edge = px * 1.2; // ~1px anti-alias

    // Crescent: main disc minus a disc offset left → opening faces left.
    let main_c = [-0.04f32, 0.0];
    let main_r = 0.86f32;
    let carve_c = [-0.50f32, -0.06];
    let carve_r = 0.78f32;

    // The 8-point star sits in the (left) opening.
    let star_c = [-0.36f32, -0.18];
    let star_r = 0.20f32;

    for y in 0..l {
        for x in 0..l {
            let n = [
                (x as f32 + 0.5) / l as f32 * 2.0 - 1.0,
                (y as f32 + 0.5) / l as f32 * 2.0 - 1.0,
            ];
            let mut col = [0.0f32; 3];
            let mut a = 0.0f32;

            // Crescent.
            let d1 = dist(n, main_c) - main_r;
            let d2 = dist(n, carve_c) - carve_r;
            let sdf = d1.max(-d2);
            let cres = smoothstep(edge, -edge, sdf);
            if cres > 0.0 {
                // Silver on the outer (right) convex edge → navy toward the inner.
                let g = (0.5 + (n[0] - main_c[0]) / (1.6 * main_r)).clamp(0.0, 1.0);
                let c = mix(NAVY, SILVER, g);
                col = mix(col, c, cres);
                a = a.max(cres);
            }

            // Star: bright core + four long + four short spikes.
            let s = star(sub(n, star_c), star_r);
            if s > 0.0 {
                col = add(col, STAR, s);
                a = (a + s).clamp(0.0, 1.0);
            }

            if a > 0.0 {
                put(buf, stride_px, ox + x, oy + y, col, a.min(1.0));
            }
        }
    }
}

fn star(d: [f32; 2], r: f32) -> f32 {
    let q = [d[0] / r, d[1] / r];
    let rr = (q[0] * q[0] + q[1] * q[1]).sqrt();
    let core = (1.0 - smoothstep(0.0, 0.45, rr)).max(0.0);
    // Axis spikes (long, thin).
    let spike_h =
        (1.0 - smoothstep(0.0, 2.6, q[0].abs())) * (1.0 - smoothstep(0.0, 0.16, q[1].abs()));
    let spike_v =
        (1.0 - smoothstep(0.0, 2.6, q[1].abs())) * (1.0 - smoothstep(0.0, 0.16, q[0].abs()));
    // Diagonal spikes (shorter), q rotated 45°.
    let r2 = std::f32::consts::FRAC_1_SQRT_2;
    let e = [(q[0] + q[1]) * r2, (q[0] - q[1]) * r2];
    let spike_d1 =
        (1.0 - smoothstep(0.0, 1.5, e[0].abs())) * (1.0 - smoothstep(0.0, 0.12, e[1].abs()));
    let spike_d2 =
        (1.0 - smoothstep(0.0, 1.5, e[1].abs())) * (1.0 - smoothstep(0.0, 0.12, e[0].abs()));
    (core + 0.75 * (spike_h + spike_v) + 0.45 * (spike_d1 + spike_d2)).clamp(0.0, 1.0)
}

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}
fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}
fn add(a: [f32; 3], b: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] + b[0] * s, a[1] + b[1] * s, a[2] + b[2] * s]
}
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Rasterize the label at `px` height → (width, height, coverage bytes).
fn rasterize_label(px: f32) -> (usize, usize, Vec<u8>) {
    let font = match load_sans() {
        Some(f) => f,
        None => return (0, 0, Vec::new()),
    };
    let scale = PxScale::from(px);
    let sf = font.as_scaled(scale);

    // Lay glyphs out on a baseline.
    let mut caret_x = 2.0f32;
    let mut prev: Option<GlyphId> = None;
    let mut glyphs: Vec<Glyph> = Vec::new();
    for ch in LABEL.chars() {
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
    let mut cov = vec![0u8; w * h];
    for g in glyphs {
        if let Some(o) = font.outline_glyph(g) {
            let bb = o.px_bounds();
            o.draw(|gx, gy, c| {
                let x = gx as i32 + bb.min.x as i32;
                let y = gy as i32 + bb.min.y as i32;
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    let i = y as usize * w + x as usize;
                    cov[i] = cov[i].max((c * 255.0) as u8);
                }
            });
        }
    }
    (w, h, cov)
}

/// Load a system sans-serif TTF (DejaVu/Noto/Liberation), trying common paths.
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
                tracing::info!("idle label font: {p}");
                return Some(f);
            }
        }
    }
    tracing::warn!("no system sans font found; idle label disabled");
    None
}
