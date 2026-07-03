// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Server-side `wp_color_manager_v1` (color-management-v1 staging, version 2).
//!
//! Smithay ships no handler for this protocol — only the generated Wire bindings
//! (via its `staging` feature). So this is a hand-rolled `GlobalDispatch`/`Dispatch`
//! implementation on [`crate::State`], mirroring Smithay's own `alpha_modifier` /
//! `content_type` handlers (a manager global + per-surface double-buffered state).
//!
//! Purpose (Stage A of the HDR color-management work, see `HDR_COLOR_MGMT_PLAN.md`):
//! accept a client's HDR image description and store it on the surface's
//! double-buffered state so the capture/compositing path can stop flattening that
//! surface to 8-bit sRGB. Verified ground truth: `proton-cachyos` winewayland binds
//! this protocol at v2 and tags its surface via `create_windows_scrgb` (scRGB FP16,
//! BT.709, extended-linear, 1.0 = 80 nits) — the PQ path is gated on protocol v3
//! which nothing ships. Every request is logged so a live `WAYLAND_DEBUG` trace
//! (mpv / a real game) confirms exactly which path the client takes.
//!
//! This module is the negotiation half only. The scRGB→PQ compositing shader
//! (Stage B) and driving the capture format from the stored description (Stage C)
//! are separate.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{
        self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
    },
    wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_v1::{self, Cause, WpImageDescriptionV1},
};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::wl_surface::WlSurface,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak,
};
use smithay::wayland::compositor::{self, Cacheable};
use tracing::{info, warn};

use crate::State;

/// The interface version we advertise. v2 unlocks `ready2` (64-bit identity) and
/// `create_windows_scrgb` — the path winewayland uses.
const CM_VERSION: u32 = 2;

/// Register the `wp_color_manager_v1` global. Hold the returned id for the run's
/// lifetime; dropping it withdraws the global.
pub fn init(dh: &DisplayHandle) -> GlobalId {
    dh.create_global::<State, WpColorManagerV1, _>(CM_VERSION, ())
}

thread_local! {
    /// `wp_image_description_info_v1.done` is a *destructor* event; firing it in
    /// the dispatch that created the object crashes wayland-backend (it assigns
    /// the object's user-data after the handler, on an object that no longer
    /// exists). So we queue the info objects here and fire their `done()` on a
    /// LATER dispatch — [`flush_pending_info_done`], called from the render loop —
    /// exactly how Smithay's presentation-time sends its destructor events.
    static PENDING_INFO_DONE: RefCell<Vec<WpImageDescriptionInfoV1>> =
        const { RefCell::new(Vec::new()) };
}

/// Fire the deferred `done()` on image-description-info objects created since the
/// last flush. Call once per render-loop iteration (a different dispatch than the
/// `get_information` that created them), so the destructor never runs in the
/// creating dispatch. Cheap: usually empty.
pub fn flush_pending_info_done() {
    PENDING_INFO_DONE.with(|q| {
        for info in q.borrow_mut().drain(..) {
            info.done();
        }
    });
}

/// Monotonic, never-recycled, non-zero image-description identity for `ready2`.
fn next_identity() -> (u32, u32) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let v = NEXT.fetch_add(1, Ordering::Relaxed);
    ((v >> 32) as u32, v as u32)
}

/// Emit the version-correct terminal "ready" event on a finished image
/// description. `ready2` (64-bit identity) exists only from interface version 2;
/// sending it to a v1-bound object is an unknown-opcode protocol error on the
/// client proxy, which tears the object down and re-binds the manager — precisely
/// the ~15/s spin loop winewayland/NVIDIA produced. A v1-bound object gets the
/// deprecated `ready` (32-bit identity) instead. Every created description must
/// reach exactly one terminal event (`ready`/`ready2`/`failed`) or the client's
/// swapchain setup hangs, so this is the single funnel for the success path.
fn send_ready(d: &WpImageDescriptionV1) {
    let (hi, lo) = next_identity();
    if d.version() >= 2 {
        d.ready2(hi, lo);
    } else {
        d.ready(lo);
    }
}

// ---------------------------------------------------------------------------
// Compositor-native parsed image description (the bit Stage B/C consume).
// ---------------------------------------------------------------------------

/// Transfer function, reduced to what the capture path cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// SMPTE ST-2084 (PQ).
    Pq,
    /// Hybrid Log-Gamma.
    Hlg,
    /// Extended-linear (scRGB): linear light, values may exceed 1.0.
    ExtLinear,
    /// An SDR gamma/sRGB-ish curve.
    Sdr,
}

/// Colour gamut / primaries, reduced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gamut {
    /// ITU-R BT.2020.
    Bt2020,
    /// sRGB / BT.709.
    Srgb,
    /// Display-P3.
    DisplayP3,
}

/// A parsed, compositor-native image description.
#[derive(Clone, Copy, Debug)]
pub struct ImageDesc {
    /// Transfer function of the surface's content.
    pub transfer: Transfer,
    /// Primaries / gamut of the surface's content.
    pub gamut: Gamut,
    /// Maximum content light level (cd/m²), if declared.
    pub max_cll: Option<u32>,
    /// Maximum frame-average light level (cd/m²), if declared.
    pub max_fall: Option<u32>,
    /// True when produced by `create_windows_scrgb` (the winewayland/DXVK path).
    pub scrgb: bool,
}

impl ImageDesc {
    /// Windows-scRGB: BT.709 primaries, extended-linear TF, 1.0 = 80 nits.
    fn windows_scrgb() -> Self {
        Self {
            transfer: Transfer::ExtLinear,
            gamut: Gamut::Srgb,
            max_cll: None,
            max_fall: None,
            scrgb: true,
        }
    }

    /// The virtual output's advertised preferred description (PQ / BT.2020) — what
    /// the compositor tells clients the display is, so they enable HDR.
    fn output_preferred() -> Self {
        Self {
            transfer: Transfer::Pq,
            gamut: Gamut::Bt2020,
            max_cll: Some(1000),
            max_fall: Some(400),
            scrgb: false,
        }
    }

    /// Should the capture path treat a surface carrying this description as HDR
    /// (10-bit target + Main10 encode)?
    pub fn is_hdr(&self) -> bool {
        self.scrgb || matches!(self.transfer, Transfer::Pq | Transfer::Hlg)
    }
}

fn map_transfer(tf: TransferFunction) -> Transfer {
    match tf {
        TransferFunction::St2084Pq => Transfer::Pq,
        TransferFunction::Hlg => Transfer::Hlg,
        TransferFunction::ExtLinear => Transfer::ExtLinear,
        _ => Transfer::Sdr,
    }
}

fn map_gamut(p: Primaries) -> Gamut {
    match p {
        Primaries::Bt2020 => Gamut::Bt2020,
        Primaries::DisplayP3 => Gamut::DisplayP3,
        _ => Gamut::Srgb,
    }
}

/// Emit the full `wp_image_description_info_v1` sequence for a parametric image
/// description, per the info-interface contract (XML 1518-1528): a parametric
/// description MUST deliver `primaries`, `primaries_named` (when named), a
/// transfer function, `luminances`, and `target_luminance`, and MAY deliver
/// `target_max_cll` / `target_max_fall`; `target_primaries` is sent only when the
/// target volume differs from the primary volume (ours does not, so it is
/// omitted). The terminal `done()` is a *destructor* and is NOT sent here — the
/// caller defers it (see [`PENDING_INFO_DONE`]).
///
/// This completeness is load-bearing: a client that only receives
/// `primaries_named` + `tf_named` treats the description as unusable and abandons
/// the v2 color manager.
fn send_image_desc_info(info: &WpImageDescriptionInfoV1, desc: &ImageDesc) {
    // CIE 1931 xy chromaticities × 1e6 (D65 white). BT.2020 / sRGB(BT.709) /
    // Display-P3 red, green, blue.
    let (named, (rx, ry, gx, gy, bx, by)) = match desc.gamut {
        Gamut::Bt2020 => (
            Primaries::Bt2020,
            (708_000, 292_000, 170_000, 797_000, 131_000, 46_000),
        ),
        Gamut::DisplayP3 => (
            Primaries::DisplayP3,
            (680_000, 320_000, 265_000, 690_000, 150_000, 60_000),
        ),
        Gamut::Srgb => (
            Primaries::Srgb,
            (640_000, 330_000, 300_000, 600_000, 150_000, 60_000),
        ),
    };
    let (wx, wy) = (312_700, 329_000); // D65
    info.primaries(rx, ry, gx, gy, bx, by, wx, wy);
    info.primaries_named(named);

    let tf = match desc.transfer {
        Transfer::Pq => TransferFunction::St2084Pq,
        Transfer::Hlg => TransferFunction::Hlg,
        Transfer::ExtLinear => TransferFunction::ExtLinear,
        Transfer::Sdr => TransferFunction::Gamma22,
    };
    info.tf_named(tf);

    // Primary-colour-volume luminance range + reference white. `min_lum` is
    // scaled ×10000; `max_lum` / `reference_lum` are unscaled cd/m². PQ implies a
    // 0.005–10000 cd/m² swing with a 203 cd/m² reference (XML 350-359).
    let (min_lum_e4, max_lum, ref_lum): (u32, u32, u32) = match desc.transfer {
        Transfer::Pq => (50, 10_000, 203),
        Transfer::Hlg => (50, 1_000, 203),
        Transfer::ExtLinear => (0, 10_000, 203),
        Transfer::Sdr => (2_000, 80, 80), // sRGB defaults: 0.2 / 80 / 80
    };
    info.luminances(min_lum_e4, max_lum, ref_lum);

    // Target (display / mastering) luminance. Peak = declared max_cll when known,
    // else the primary-volume max. target_primaries is omitted (target gamut ==
    // primary gamut here — XML 1642-1644 says omit when equal).
    let target_max = desc.max_cll.unwrap_or(max_lum);
    info.target_luminance(min_lum_e4, target_max);
    if let Some(cll) = desc.max_cll {
        info.target_max_cll(cll);
    }
    if let Some(fall) = desc.max_fall {
        info.target_max_fall(fall);
    }
}

// ---------------------------------------------------------------------------
// Per-surface double-buffered state (read back at compositing time).
// ---------------------------------------------------------------------------

/// The committed colour state of a surface. Read during compositing via
/// `with_states(surface, |s| s.cached_state.get::<ColorSurfaceCachedState>())`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColorSurfaceCachedState {
    /// The image description set by the client, applied on `wl_surface.commit`.
    pub image_desc: Option<ImageDesc>,
}

impl Cacheable for ColorSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }
    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Per-surface marker enforcing one color-management surface object per surface.
#[derive(Default)]
struct ColorSurfaceData {
    attached: AtomicBool,
}

// ---------------------------------------------------------------------------
// Per-object UserData.
// ---------------------------------------------------------------------------

/// UserData of a `wp_color_management_surface_v1`: a weak handle to its surface.
pub struct ColorSurfaceUserData(Mutex<Weak<WlSurface>>);

impl ColorSurfaceUserData {
    fn new(surface: WlSurface) -> Self {
        Self(Mutex::new(surface.downgrade()))
    }
    fn wl_surface(&self) -> Option<WlSurface> {
        self.0.lock().unwrap().upgrade().ok()
    }
}

/// UserData of a `wp_image_description_creator_params_v1`: accumulates setters.
#[derive(Default)]
pub struct ParamsUserData(Mutex<PartialParams>);

#[derive(Default)]
struct PartialParams {
    transfer: Option<Transfer>,
    gamut: Option<Gamut>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
}

/// UserData of a finished `wp_image_description_v1`: the immutable parsed record
/// plus whether `get_information` is permitted on it.
pub struct ImageDescData {
    desc: ImageDesc,
    /// Per the protocol, `get_information` is allowed ONLY on compositor-originated
    /// descriptions (from an output / surface-feedback / reference) and FORBIDDEN
    /// (must raise `no_information`) on client-authored ones (`create_windows_scrgb`,
    /// parametric `create`, ICC `create`). Answering a forbidden query with a bogus
    /// info set makes a conformance-checking client tear the object down and re-bind
    /// the manager — one of the observed spin-bind drivers.
    allow_info: bool,
}

impl ImageDescData {
    /// A compositor-originated description the client may introspect via
    /// `get_information` (output / preferred / reference).
    fn introspectable(desc: ImageDesc) -> Self {
        Self {
            desc,
            allow_info: true,
        }
    }
    /// A client-authored / opaque description whose `get_information` is forbidden.
    fn opaque(desc: ImageDesc) -> Self {
        Self {
            desc,
            allow_info: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Manager: wp_color_manager_v1
// ---------------------------------------------------------------------------

impl GlobalDispatch<WpColorManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        let mgr = data_init.init(resource, ());
        // Advertise ONLY what we consume — the client is contractually limited to
        // the advertised set, so this bounds which requests we can receive.
        mgr.supported_intent(RenderIntent::Perceptual);
        mgr.supported_feature(Feature::Parametric);
        // `windows_scrgb` (feature 7) and its request carry no `since` in our
        // bundled v2 XML — they exist from interface v1. Advertise it at every
        // bound version: a client that falls back to a v1 manager still needs a
        // path to declare its scRGB HDR surface (winewayland's actual path).
        mgr.supported_feature(Feature::WindowsScrgb);
        mgr.supported_feature(Feature::SetLuminances);
        mgr.supported_feature(Feature::SetMasteringDisplayPrimaries);
        // winewayland gates its output `supports_hdr` on ALL of
        // extended_target_volume(6) && windows_scrgb(7) && st2084_pq(11) being
        // advertised (wayland_output.c: supports_extended_volume && supports_pq &&
        // supports_scrgb). We advertised 7 + 11 but not 6, so a real game saw the
        // display as SDR (hdr=0) and never requested an HDR swapchain. Feature 6 is
        // legal at v2 (no `since`) and its prerequisite — set_mastering_display_
        // primaries(5) above — is already advertised. This single line flips
        // winewayland's HDR detection on.
        mgr.supported_feature(Feature::ExtendedTargetVolume);
        // Transfer functions: HDR (PQ/HLG/ext-linear) + a couple of SDR curves.
        mgr.supported_tf_named(TransferFunction::St2084Pq);
        mgr.supported_tf_named(TransferFunction::Hlg);
        mgr.supported_tf_named(TransferFunction::ExtLinear);
        mgr.supported_tf_named(TransferFunction::Bt1886);
        mgr.supported_tf_named(TransferFunction::Gamma22);
        // Primaries: BT.2020 (HDR) + sRGB + Display-P3.
        mgr.supported_primaries_named(Primaries::Bt2020);
        mgr.supported_primaries_named(Primaries::Srgb);
        mgr.supported_primaries_named(Primaries::DisplayP3);
        mgr.done();
        info!(target: "compositor::color", "wp_color_manager_v1 bound (v{})", mgr.version());
    }
}

impl Dispatch<WpColorManagerV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        manager: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_manager_v1::Request;
        match request {
            Request::GetSurface { id, surface } => {
                let already = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(ColorSurfaceData::default);
                    states
                        .data_map
                        .get::<ColorSurfaceData>()
                        .unwrap()
                        .attached
                        .swap(true, Ordering::AcqRel)
                });
                if already {
                    manager.post_error(
                        wp_color_manager_v1::Error::SurfaceExists,
                        "surface already has a color-management surface",
                    );
                } else {
                    data_init.init(id, ColorSurfaceUserData::new(surface));
                    info!(target: "compositor::color", "get_surface");
                }
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(obj, ParamsUserData::default());
                info!(target: "compositor::color", "create_parametric_creator");
            }
            Request::CreateWindowsScrgb { image_description } => {
                let d = data_init.init(
                    image_description,
                    ImageDescData::opaque(ImageDesc::windows_scrgb()),
                );
                send_ready(&d);
                info!(target: "compositor::color", "create_windows_scrgb → ready HDR(scRGB) description");
            }
            Request::GetSurfaceFeedback { id, surface: _ } => {
                data_init.init(id, ());
                info!(target: "compositor::color", "get_surface_feedback");
            }
            Request::GetOutput { id, output: _ } => {
                data_init.init(id, ());
                info!(target: "compositor::color", "get_output");
            }
            Request::CreateIccCreator { obj } => {
                // We don't advertise icc_v2_v4, so a conformant client won't call
                // this — but init the object anyway so a non-conformant one that
                // does gets a clean `failed` at create time, not a dropped id.
                data_init.init(obj, ());
                warn!(target: "compositor::color", "create_icc_creator (ICC unsupported)");
            }
            Request::GetImageDescription {
                image_description,
                reference: _,
            } => {
                // v2 reference-based lookup; winewayland/mpv don't use it. Hand
                // back the output-preferred description so the id isn't left inert.
                let d = data_init.init(
                    image_description,
                    ImageDescData::introspectable(ImageDesc::output_preferred()),
                );
                send_ready(&d);
                warn!(target: "compositor::color", "get_image_description (reference) → output-preferred");
            }
            Request::Destroy => {}
            _ => warn!(target: "compositor::color", "unhandled wp_color_manager_v1 request"),
        }
    }
}

// ---------------------------------------------------------------------------
// Surface: wp_color_management_surface_v1
// ---------------------------------------------------------------------------

impl Dispatch<WpColorManagementSurfaceV1, ColorSurfaceUserData> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        obj: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        data: &ColorSurfaceUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_surface_v1::Request;
        match request {
            Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let intent = match render_intent {
                    WEnum::Value(v) => v,
                    _ => {
                        obj.post_error(
                            wp_color_management_surface_v1::Error::RenderIntent,
                            "unknown render intent",
                        );
                        return;
                    }
                };
                let desc = image_description.data::<ImageDescData>().map(|d| d.desc);
                let Some(surface) = data.wl_surface() else {
                    obj.post_error(
                        wp_color_management_surface_v1::Error::Inert,
                        "surface destroyed",
                    );
                    return;
                };
                compositor::with_states(&surface, |states| {
                    states
                        .cached_state
                        .get::<ColorSurfaceCachedState>()
                        .pending()
                        .image_desc = desc;
                });
                info!(
                    target: "compositor::color",
                    "set_image_description intent={:?} desc={:?}", intent, desc
                );
            }
            Request::UnsetImageDescription => {
                if let Some(surface) = data.wl_surface() {
                    compositor::with_states(&surface, |states| {
                        states
                            .cached_state
                            .get::<ColorSurfaceCachedState>()
                            .pending()
                            .image_desc = None;
                    });
                }
                info!(target: "compositor::color", "unset_image_description");
            }
            Request::Destroy => {
                if let Some(surface) = data.wl_surface() {
                    compositor::with_states(&surface, |states| {
                        if let Some(m) = states.data_map.get::<ColorSurfaceData>() {
                            m.attached.store(false, Ordering::Release);
                        }
                        states
                            .cached_state
                            .get::<ColorSurfaceCachedState>()
                            .pending()
                            .image_desc = None;
                    });
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Surface feedback: wp_color_management_surface_feedback_v1
// ---------------------------------------------------------------------------

impl Dispatch<WpColorManagementSurfaceFeedbackV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _obj: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_surface_feedback_v1::Request;
        match request {
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => {
                let d = data_init.init(
                    image_description,
                    ImageDescData::introspectable(ImageDesc::output_preferred()),
                );
                send_ready(&d);
                info!(target: "compositor::color", "feedback get_preferred → output-preferred (PQ/BT.2020)");
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Output: wp_color_management_output_v1
// ---------------------------------------------------------------------------

impl Dispatch<WpColorManagementOutputV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _obj: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_output_v1::Request;
        match request {
            Request::GetImageDescription { image_description } => {
                let d = data_init.init(
                    image_description,
                    ImageDescData::introspectable(ImageDesc::output_preferred()),
                );
                send_ready(&d);
                info!(target: "compositor::color", "output get_image_description → PQ/BT.2020");
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Parametric creator: wp_image_description_creator_params_v1
// ---------------------------------------------------------------------------

impl Dispatch<WpImageDescriptionCreatorParamsV1, ParamsUserData> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        obj: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        data: &ParamsUserData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};
        match request {
            Request::SetTfNamed { tf } => {
                let WEnum::Value(tf) = tf else { return };
                let mut p = data.0.lock().unwrap();
                if p.transfer.is_some() {
                    obj.post_error(Error::AlreadySet, "tf already set");
                    return;
                }
                p.transfer = Some(map_transfer(tf));
            }
            Request::SetPrimariesNamed { primaries } => {
                let WEnum::Value(primaries) = primaries else {
                    return;
                };
                let mut p = data.0.lock().unwrap();
                if p.gamut.is_some() {
                    obj.post_error(Error::AlreadySet, "primaries already set");
                    return;
                }
                p.gamut = Some(map_gamut(primaries));
            }
            Request::SetMaxCll { max_cll } => {
                data.0.lock().unwrap().max_cll = Some(max_cll);
            }
            Request::SetMaxFall { max_fall } => {
                data.0.lock().unwrap().max_fall = Some(max_fall);
            }
            // Advertised features we accept but don't yet act on (values folded in
            // later when the shader/SEI want them).
            Request::SetLuminances { .. }
            | Request::SetMasteringDisplayPrimaries { .. }
            | Request::SetMasteringLuminance { .. }
            | Request::SetPrimaries { .. }
            | Request::SetTfPower { .. } => {}
            Request::Create { image_description } => {
                let p = data.0.lock().unwrap();
                let (Some(transfer), Some(gamut)) = (p.transfer, p.gamut) else {
                    obj.post_error(Error::IncompleteSet, "tf and primaries are required");
                    return;
                };
                let desc = ImageDesc {
                    transfer,
                    gamut,
                    max_cll: p.max_cll,
                    max_fall: p.max_fall,
                    scrgb: false,
                };
                drop(p);
                // Parametric `create` descriptions forbid get_information (XML).
                let d = data_init.init(image_description, ImageDescData::opaque(desc));
                send_ready(&d);
                info!(target: "compositor::color", "parametric create → ready description {:?}", desc);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ICC creator: wp_image_description_creator_icc_v1 (unsupported — fail cleanly).
// ---------------------------------------------------------------------------

impl Dispatch<WpImageDescriptionCreatorIccV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _obj: &WpImageDescriptionCreatorIccV1,
        request: wp_image_description_creator_icc_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_creator_icc_v1::Request;
        if let Request::Create { image_description } = request {
            let d = data_init.init(
                image_description,
                ImageDescData::opaque(ImageDesc::output_preferred()),
            );
            d.failed(
                Cause::Unsupported,
                "ICC image descriptions are not supported".into(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Image description: wp_image_description_v1
// ---------------------------------------------------------------------------

impl Dispatch<WpImageDescriptionV1, ImageDescData> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        obj: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        data: &ImageDescData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_v1::Request;
        match request {
            Request::GetInformation { information } => {
                if !data.allow_info {
                    // scRGB / parametric-create / ICC descriptions forbid
                    // get_information; the client must never call it, and if it
                    // does the protocol says raise `no_information`. (Same
                    // no-init-on-error pattern as get_surface's surface_exists.)
                    obj.post_error(
                        wp_image_description_v1::Error::NoInformation,
                        "get_information is not allowed on this image description",
                    );
                    return;
                }
                // Emit the FULL conformant info set the client needs to learn the
                // display's HDR colorimetry + luminance. Sending only
                // primaries_named + tf_named (the old behaviour) leaves the query
                // incomplete: winewayland/libplacebo read the partial caps, decide
                // the manager is unusable, drop to a v1 manager and spin-bind
                // ~30×/s — never declaring an HDR surface. `done()` is a destructor
                // and is deferred (see PENDING_INFO_DONE / flush_pending_info_done).
                let info: WpImageDescriptionInfoV1 = data_init.init(information, ());
                send_image_desc_info(&info, &data.desc);
                PENDING_INFO_DONE.with(|q| q.borrow_mut().push(info));
                info!(target: "compositor::color", "image_description get_information → full info set sent, done() deferred");
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// wp_image_description_info_v1 has no requests; a trivial Dispatch is still needed
// so the object can be created via `data_init.init`.
impl Dispatch<WpImageDescriptionInfoV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _obj: &WpImageDescriptionInfoV1,
        _request: <WpImageDescriptionInfoV1 as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }

    fn destroyed(
        _state: &mut State,
        _client: ClientId,
        _obj: &WpImageDescriptionInfoV1,
        _data: &(),
    ) {
    }
}
