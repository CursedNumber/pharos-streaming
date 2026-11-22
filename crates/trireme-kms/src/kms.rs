// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! DRM/KMS plumbing shared by the scanout backends.
//!
//! [`Target`] selects the connected connector / CRTC / primary plane / mode and
//! caches the property handles. [`modeset_req`] / [`flip_req`] build the atomic
//! requests. Two presenters consume this: the dumb-buffer [`Display`] (CPU-mapped
//! XRGB8888 — simple, but nvidia-drm does a per-flip linear→tiled blit so it caps
//! at ~60 Hz) and the GL presenter in `gl_present` (renders into a tiled GBM
//! surface → full-rate 120 Hz). Atomic is mandatory here: fbdev blocks and legacy
//! page-flips error on this driver.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::io::{AsFd, BorrowedFd};

use anyhow::{anyhow, Context, Result};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::dumbbuffer::{DumbBuffer, DumbMapping};
use drm::control::{
    atomic, connector, crtc, framebuffer, plane, property, AtomicCommitFlags,
    Device as ControlDevice, Mode, PlaneType, ResourceHandles,
};
use drm::Device as BasicDevice;

/// A DRM device node wrapper. Implementing `AsFd` unlocks the `drm` device traits.
pub struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl BasicDevice for Card {}
impl ControlDevice for Card {}

impl Card {
    /// Open `path` read/write and claim the atomic + universal-planes caps.
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {path}"))?;
        let card = Card(file);
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .context("request UniversalPlanes")?;
        card.set_client_capability(drm::ClientCapability::Atomic, true)
            .context("request Atomic capability")?;
        Ok(card)
    }

    /// A dup of the underlying fd (shares the open file description, hence DRM
    /// master) for handing to GBM, which only needs to allocate, not modeset.
    pub fn dup_file(&self) -> Result<File> {
        self.0.try_clone().context("dup DRM fd for GBM")
    }
}

/// Requested output mode: width, height, optional refresh (Hz).
#[derive(Clone, Copy, Debug)]
pub struct ModePref {
    pub w: u16,
    pub h: u16,
    pub refresh: Option<u32>,
}

impl ModePref {
    /// Parse `WxH` or `WxH@R` (e.g. `1920x1080@120`).
    pub fn parse(s: &str) -> Result<Self> {
        let (wh, refresh) = match s.split_once('@') {
            Some((wh, r)) => (wh, Some(r.parse::<u32>().context("parse refresh")?)),
            None => (s, None),
        };
        let (w, h) = wh
            .split_once('x')
            .ok_or_else(|| anyhow!("mode must be WxH[@R]"))?;
        Ok(Self {
            w: w.parse().context("parse width")?,
            h: h.parse().context("parse height")?,
            refresh,
        })
    }
}

/// The selected scanout target: connector/CRTC/primary-plane/mode plus the
/// property handles needed to build atomic requests.
pub struct Target {
    pub conn: connector::Handle,
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
    pub mode: Mode,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    // Cached property handles.
    pub plane_fb_id: property::Handle,
    conn_crtc_id: property::Handle,
    crtc_mode_id: property::Handle,
    crtc_active: property::Handle,
    plane_crtc_id: property::Handle,
    plane_src_x: property::Handle,
    plane_src_y: property::Handle,
    plane_src_w: property::Handle,
    plane_src_h: property::Handle,
    plane_crtc_x: property::Handle,
    plane_crtc_y: property::Handle,
    plane_crtc_w: property::Handle,
    plane_crtc_h: property::Handle,
}

/// Pick the connected connector (optionally matching `pref`), a CRTC and primary
/// plane that can drive it, and cache the property handles.
pub fn select_target(card: &Card, pref: Option<ModePref>) -> Result<Target> {
    let res = card.resource_handles().context("resource handles")?;

    let conn = res
        .connectors()
        .iter()
        .filter_map(|&h| card.get_connector(h, true).ok())
        .find(|c| c.state() == connector::State::Connected)
        .ok_or_else(|| anyhow!("no connected connector"))?;
    tracing::info!(
        "connector {}-{} connected, {} modes",
        conn.interface().as_str(),
        conn.interface_id(),
        conn.modes().len()
    );

    let mode = pick_mode(conn.modes(), pref).ok_or_else(|| anyhow!("no usable mode"))?;
    let (width, height) = mode.size();
    tracing::info!(
        "mode: {}x{}@{} ({})",
        width,
        height,
        mode.vrefresh(),
        mode.name().to_string_lossy()
    );

    let crtc = select_crtc(card, &conn, &res).ok_or_else(|| anyhow!("no CRTC for connector"))?;
    let plane =
        select_primary_plane(card, &res, crtc).ok_or_else(|| anyhow!("no primary plane"))?;
    tracing::info!("crtc {:?}, primary plane {:?}", crtc, plane);

    let conn_props = card.get_properties(conn.handle())?.as_hashmap(card)?;
    let crtc_props = card.get_properties(crtc)?.as_hashmap(card)?;
    let plane_props = card.get_properties(plane)?.as_hashmap(card)?;
    let ph = |m: &HashMap<String, property::Info>, k: &str| -> Result<property::Handle> {
        m.get(k)
            .map(|i| i.handle())
            .ok_or_else(|| anyhow!("missing property {k}"))
    };

    Ok(Target {
        conn: conn.handle(),
        crtc,
        plane,
        mode,
        width: width as u32,
        height: height as u32,
        refresh: mode.vrefresh(),
        plane_fb_id: ph(&plane_props, "FB_ID")?,
        conn_crtc_id: ph(&conn_props, "CRTC_ID")?,
        crtc_mode_id: ph(&crtc_props, "MODE_ID")?,
        crtc_active: ph(&crtc_props, "ACTIVE")?,
        plane_crtc_id: ph(&plane_props, "CRTC_ID")?,
        plane_src_x: ph(&plane_props, "SRC_X")?,
        plane_src_y: ph(&plane_props, "SRC_Y")?,
        plane_src_w: ph(&plane_props, "SRC_W")?,
        plane_src_h: ph(&plane_props, "SRC_H")?,
        plane_crtc_x: ph(&plane_props, "CRTC_X")?,
        plane_crtc_y: ph(&plane_props, "CRTC_Y")?,
        plane_crtc_w: ph(&plane_props, "CRTC_W")?,
        plane_crtc_h: ph(&plane_props, "CRTC_H")?,
    })
}

/// A full modeset request: connector→CRTC, the mode blob, plane→fb + rectangles.
pub fn modeset_req(
    card: &Card,
    t: &Target,
    fb: framebuffer::Handle,
) -> Result<atomic::AtomicModeReq> {
    let blob = card.create_property_blob(&t.mode).context("mode blob")?;
    let mut req = atomic::AtomicModeReq::new();
    req.add_property(t.conn, t.conn_crtc_id, property::Value::CRTC(Some(t.crtc)));
    req.add_property(t.crtc, t.crtc_mode_id, blob);
    req.add_property(t.crtc, t.crtc_active, property::Value::Boolean(true));
    req.add_property(
        t.plane,
        t.plane_fb_id,
        property::Value::Framebuffer(Some(fb)),
    );
    req.add_property(
        t.plane,
        t.plane_crtc_id,
        property::Value::CRTC(Some(t.crtc)),
    );
    req.add_property(t.plane, t.plane_src_x, property::Value::UnsignedRange(0));
    req.add_property(t.plane, t.plane_src_y, property::Value::UnsignedRange(0));
    req.add_property(
        t.plane,
        t.plane_src_w,
        property::Value::UnsignedRange((t.width as u64) << 16),
    );
    req.add_property(
        t.plane,
        t.plane_src_h,
        property::Value::UnsignedRange((t.height as u64) << 16),
    );
    req.add_property(t.plane, t.plane_crtc_x, property::Value::SignedRange(0));
    req.add_property(t.plane, t.plane_crtc_y, property::Value::SignedRange(0));
    req.add_property(
        t.plane,
        t.plane_crtc_w,
        property::Value::UnsignedRange(t.width as u64),
    );
    req.add_property(
        t.plane,
        t.plane_crtc_h,
        property::Value::UnsignedRange(t.height as u64),
    );
    Ok(req)
}

/// A page-flip request: just swap the plane's framebuffer.
pub fn flip_req(t: &Target, fb: framebuffer::Handle) -> atomic::AtomicModeReq {
    let mut req = atomic::AtomicModeReq::new();
    req.add_property(
        t.plane,
        t.plane_fb_id,
        property::Value::Framebuffer(Some(fb)),
    );
    req
}

/// Block until a page-flip-complete event arrives on the DRM fd.
pub fn wait_flip(card: &Card) -> Result<()> {
    loop {
        for ev in card.receive_events().context("receive DRM events")? {
            if let drm::control::Event::PageFlip(_) = ev {
                return Ok(());
            }
        }
    }
}

// ---- dumb-buffer presenter (the ~60 Hz fallback / atomic-path proof) ----------

/// One scanout buffer: a dumb XRGB8888 buffer, its framebuffer handle, and a
/// persistent CPU mapping (mapped once, written every flip).
struct Surface {
    db: DumbBuffer,
    fb: framebuffer::Handle,
    // Lifetime laundered to `'static`: the mmap outlives the `&mut DumbBuffer`
    // borrow `map_dumb_buffer` took. `DumbBuffer` is `Copy`, so the stored `db`
    // does not alias this mapping.
    map: DumbMapping<'static>,
}

/// CPU-mapped dumb-buffer presenter. Simple but ~60 Hz on nvidia-drm.
pub struct Display {
    card: Card,
    target: Target,
    surfaces: Vec<Surface>,
}

impl Display {
    pub fn open(card_path: &str, pref: Option<ModePref>, nbuf: usize) -> Result<Self> {
        let card = Card::open(card_path)?;
        let target = select_target(&card, pref)?;
        let mut surfaces = Vec::with_capacity(nbuf);
        for _ in 0..nbuf {
            surfaces.push(make_surface(&card, target.width, target.height)?);
        }
        Ok(Self {
            card,
            target,
            surfaces,
        })
    }

    pub fn width(&self) -> u32 {
        self.target.width
    }
    pub fn height(&self) -> u32 {
        self.target.height
    }
    pub fn refresh(&self) -> u32 {
        self.target.refresh
    }
    pub fn pitch(&self) -> u32 {
        self.surfaces[0].db.pitch()
    }

    /// Vblank-locked flip loop. `fill(buf, pitch, frame)` writes the next frame
    /// into the back buffer (XRGB8888, `pitch` bytes/row); return `false` to stop.
    pub fn run<F>(&mut self, mut fill: F) -> Result<u64>
    where
        F: FnMut(&mut [u8], u32, u64) -> bool,
    {
        let pitch = self.pitch();
        if !fill(self.surfaces[0].map.as_mut(), pitch, 0) {
            return Ok(0);
        }
        let req = modeset_req(&self.card, &self.target, self.surfaces[0].fb)?;
        self.card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .context("atomic modeset commit")?;

        let mut front = 0usize;
        let mut frame = 1u64;
        let nbuf = self.surfaces.len();
        loop {
            let mut back = frame as usize % nbuf;
            if back == front {
                back = (back + 1) % nbuf;
            }
            if !fill(self.surfaces[back].map.as_mut(), pitch, frame) {
                break;
            }
            let req = flip_req(&self.target, self.surfaces[back].fb);
            self.card
                .atomic_commit(
                    AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
                    req,
                )
                .context("atomic page-flip commit")?;
            wait_flip(&self.card)?;
            front = back;
            frame += 1;
        }
        Ok(frame)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        for s in self.surfaces.drain(..) {
            let _ = self.card.destroy_framebuffer(s.fb);
            drop(s.map);
            let _ = self.card.destroy_dumb_buffer(s.db);
        }
    }
}

fn make_surface(card: &Card, w: u32, h: u32) -> Result<Surface> {
    let mut db = card
        .create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)
        .context("create dumb buffer")?;
    let fb = card
        .add_framebuffer(&db, 24, 32)
        .context("add framebuffer")?;
    let map = card.map_dumb_buffer(&mut db).context("map dumb buffer")?;
    let map: DumbMapping<'static> = unsafe { std::mem::transmute(map) };
    Ok(Surface { db, fb, map })
}

// ---- selection helpers --------------------------------------------------------

/// Prefer an exact `WxH[@R]` match; otherwise the highest-refresh 1920x1080;
/// otherwise the connector's first (preferred) mode.
fn pick_mode(modes: &[Mode], pref: Option<ModePref>) -> Option<Mode> {
    if let Some(p) = pref {
        return modes
            .iter()
            .filter(|m| m.size() == (p.w, p.h))
            .max_by_key(|m| {
                let exact = matches!(p.refresh, Some(r) if m.vrefresh() == r) as u32;
                (exact, m.vrefresh())
            })
            .copied();
    }
    modes
        .iter()
        .filter(|m| m.size() == (1920, 1080))
        .max_by_key(|m| m.vrefresh())
        .or_else(|| modes.first())
        .copied()
}

fn select_crtc(card: &Card, conn: &connector::Info, res: &ResourceHandles) -> Option<crtc::Handle> {
    if let Some(enc) = conn
        .current_encoder()
        .and_then(|e| card.get_encoder(e).ok())
    {
        if let Some(c) = enc.crtc() {
            return Some(c);
        }
    }
    for &eh in conn.encoders() {
        if let Ok(enc) = card.get_encoder(eh) {
            if let Some(&c) = res.filter_crtcs(enc.possible_crtcs()).first() {
                return Some(c);
            }
        }
    }
    res.crtcs().first().copied()
}

fn select_primary_plane(
    card: &Card,
    res: &ResourceHandles,
    crtc: crtc::Handle,
) -> Option<plane::Handle> {
    let planes = card.plane_handles().ok()?;
    planes.into_iter().find(|&p| {
        let drives_crtc = card
            .get_plane(p)
            .map(|info| res.filter_crtcs(info.possible_crtcs()).contains(&crtc))
            .unwrap_or(false);
        drives_crtc && plane_type(card, p) == Some(PlaneType::Primary)
    })
}

fn plane_type(card: &Card, p: plane::Handle) -> Option<PlaneType> {
    let props = card.get_properties(p).ok()?;
    for (&id, &val) in props.iter() {
        if let Ok(info) = card.get_property(id) {
            if info.name().to_str().ok() == Some("type") {
                return match val {
                    x if x == PlaneType::Primary as u64 => Some(PlaneType::Primary),
                    x if x == PlaneType::Overlay as u64 => Some(PlaneType::Overlay),
                    x if x == PlaneType::Cursor as u64 => Some(PlaneType::Cursor),
                    _ => None,
                };
            }
        }
    }
    None
}
