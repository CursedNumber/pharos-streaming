//! Shared Pharos branding assets + a tiny SVG→RGBA rasterizer.
//!
//! The lighthouse art lives in `rig/icons/` (the canonical source); this crate
//! is its *code* home so the tray ([`pharos-tray`]), the compositor's idle
//! screen, and any future surface all render the SAME mark from one place
//! instead of each `include_bytes!`-ing + rasterizing its own copy.
//!
//! A leaf crate: it depends on nothing in the workspace, so anything can depend
//! on it without risking a cycle.

use anyhow::{Context, Result};

/// The full-colour watercolour lighthouse — the Linux app icon (1024×1024 RGBA
/// PNG, transparent background). The richest Pharos mark; used for the
/// compositor's idle/splash background. Decode it with an image codec (it's a
/// raster, not an SVG), e.g. the `image` crate.
pub const PHAROS_APP: &[u8] = include_bytes!("../../../rig/icons/pharos-app.png");
/// The line-art lighthouse SVG that the tray icons derive from. Kept for any
/// surface wanting a vector mark; the idle screen uses [`PHAROS_APP`] instead.
pub const PHAROS_LIGHTHOUSE: &[u8] = include_bytes!("../../../rig/icons/lighthouse-source.svg");
/// Grey, unlit — the host isn't running.
pub const PHAROS_OFFLINE: &[u8] = include_bytes!("../../../rig/icons/pharos-tray-offline.svg");
/// Green lantern + grey beams — host up, idle.
pub const PHAROS_IDLE: &[u8] = include_bytes!("../../../rig/icons/pharos-tray-idle.svg");
/// Green lantern + green beams — streaming.
pub const PHAROS_STREAMING: &[u8] = include_bytes!("../../../rig/icons/pharos-tray-streaming.svg");
/// Green + red — serving but the GPU is strained.
pub const PHAROS_DEGRADED: &[u8] = include_bytes!("../../../rig/icons/pharos-tray-degraded.svg");
/// Sticky red — the GPU faulted.
pub const PHAROS_FAULTED: &[u8] = include_bytes!("../../../rig/icons/pharos-tray-faulted.svg");

/// A rasterized image: `width`×`height`, premultiplied RGBA8 (byte order
/// R,G,B,A — i.e. DRM `Abgr8888`). Premultiplied is the GL/tiny-skia norm;
/// callers that need straight ARGB (e.g. the Linux SNI tray) byte-swap it.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Rasterize an SVG into a `width`×`height` premultiplied-RGBA buffer, scaling
/// the art uniformly to *fit* the box and centring it (so non-square boxes letterbox
/// rather than distort). Honours the SVG's own viewBox.
pub fn rasterize(svg: &[u8], width: u32, height: u32) -> Result<Rgba> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg, &opt).context("parse SVG")?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height).context("alloc pixmap")?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    let scale = (width as f32 / sw).min(height as f32 / sh);
    let tx = (width as f32 - sw * scale) / 2.0;
    let ty = (height as f32 - sh * scale) / 2.0;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(Rgba {
        width,
        height,
        rgba: pixmap.take(),
    })
}

/// Square convenience — the shape the tray icons want.
pub fn rasterize_square(svg: &[u8], size: u32) -> Result<Rgba> {
    rasterize(svg, size, size)
}
