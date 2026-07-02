//! Raw `spice-client-glib-2.0` FFI: the `spice_*` entry points + constants the
//! headless display/inputs session needs. GLib/GObject FFI itself comes from
//! `glib-sys`/`gobject-sys`; only the spice symbols are declared here.
//!
//! Every signature and constant was verified against **spice-gtk 0.42** —
//! headers (`/usr/include/spice-client-glib-2.0`, `/usr/include/spice-1`),
//! GObject-introspection (`SpiceClientGLib-2.0.gir`), and exported symbols
//! (`nm -D libspice-client-glib-2.0.so.8`). Getting a struct layout or a
//! callback signature wrong here corrupts the stack at signal-emit time, so the
//! constants below cite their source. Linking is handled by `build.rs`
//! (pkg-config emits `-lspice-client-glib-2.0`).
// Full protocol constant tables are kept even where not every value is
// referenced — they document the verified enums and guard against drift.
#![allow(non_camel_case_types, non_upper_case_globals, dead_code)]

use glib_sys::{gboolean, GError, GType};
use gobject_sys::GObject;
use std::os::raw::{c_int, c_uint};

// Every Spice* object is GObject-derived; at the ABI level they are all GObject.
// These aliases exist purely for call-site readability.
pub type SpiceSession = GObject;
pub type SpiceChannel = GObject;
pub type SpiceInputsChannel = GObject;

/// `SpiceDisplayPrimary` — verified field order/types from `channel-display.h`.
/// `data` points at a channel-owned linear framebuffer (we never free it).
/// `format` is a C `enum` (int-width); compare against the `SPICE_SURFACE_FMT_*`
/// constants rather than matching an FFI enum.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpiceDisplayPrimary {
    pub format: c_int,
    pub width: c_int,
    pub height: c_int,
    pub stride: c_int,
    pub shmid: c_int,
    pub data: *mut u8,
    pub marked: gboolean,
}

// --- SpiceSurfaceFmt (spice/enums.h:292-302) -------------------------------
// 32-bit primaries; in-memory byte order is [B, G, R, X|A] little-endian.
pub const SPICE_SURFACE_FMT_32_xRGB: c_int = 32;
pub const SPICE_SURFACE_FMT_32_ARGB: c_int = 96;

// --- SpiceChannelType (spice/enums.h:407-411) ------------------------------
pub const SPICE_CHANNEL_MAIN: c_int = 1;
pub const SPICE_CHANNEL_DISPLAY: c_int = 2;
pub const SPICE_CHANNEL_INPUTS: c_int = 3;

// --- SpiceChannelEvent (spice-channel.h) — 11/12 are implicit-sequence ------
pub const SPICE_CHANNEL_OPENED: c_int = 10;
pub const SPICE_CHANNEL_SWITCHING: c_int = 11;
pub const SPICE_CHANNEL_CLOSED: c_int = 12;
pub const SPICE_CHANNEL_ERROR_CONNECT: c_int = 20;
pub const SPICE_CHANNEL_ERROR_TLS: c_int = 21;
pub const SPICE_CHANNEL_ERROR_LINK: c_int = 22;
pub const SPICE_CHANNEL_ERROR_AUTH: c_int = 23;
pub const SPICE_CHANNEL_ERROR_IO: c_int = 24;

// --- SpiceMouseMode (spice/enums.h:117-122) --------------------------------
pub const SPICE_MOUSE_MODE_SERVER: c_int = 1 << 0;
pub const SPICE_MOUSE_MODE_CLIENT: c_int = 1 << 1;

extern "C" {
    // ===== SpiceSession =====
    pub fn spice_session_new() -> *mut SpiceSession;
    pub fn spice_session_connect(session: *mut SpiceSession) -> gboolean;
    pub fn spice_session_disconnect(session: *mut SpiceSession);

    // ===== SpiceChannel base =====
    pub fn spice_channel_connect(channel: *mut SpiceChannel) -> gboolean;
    pub fn spice_channel_get_error(channel: *mut SpiceChannel) -> *const GError;

    // ===== Type getters for runtime channel dispatch =====
    pub fn spice_display_channel_get_type() -> GType;
    pub fn spice_inputs_channel_get_type() -> GType;
    pub fn spice_main_channel_get_type() -> GType;

    // ===== SpiceDisplayChannel =====
    // `surface_id` is guint32; `primary` is caller-allocated and filled in.
    pub fn spice_display_channel_get_primary(
        channel: *mut SpiceChannel,
        surface_id: c_uint,
        primary: *mut SpiceDisplayPrimary,
    ) -> gboolean;

    // ===== SpiceInputsChannel ===== (button_state = post-event held-button mask)
    pub fn spice_inputs_channel_position(
        channel: *mut SpiceInputsChannel,
        x: c_int,
        y: c_int,
        display: c_int,
        button_state: c_int,
    );
    pub fn spice_inputs_channel_motion(
        channel: *mut SpiceInputsChannel,
        dx: c_int,
        dy: c_int,
        button_state: c_int,
    );
    pub fn spice_inputs_channel_button_press(
        channel: *mut SpiceInputsChannel,
        button: c_int,
        button_state: c_int,
    );
    pub fn spice_inputs_channel_button_release(
        channel: *mut SpiceInputsChannel,
        button: c_int,
        button_state: c_int,
    );
    // `scancode` is the XT set-1 make code; 0xE0-prefixed keys are sent as
    // (low_byte | 0x100). See `crate::keymap`.
    pub fn spice_inputs_channel_key_press(channel: *mut SpiceInputsChannel, scancode: c_uint);
    pub fn spice_inputs_channel_key_release(channel: *mut SpiceInputsChannel, scancode: c_uint);
}
