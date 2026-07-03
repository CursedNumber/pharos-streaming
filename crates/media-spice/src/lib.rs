// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Native **SPICE** source for split-screen cells — e.g. a Proxmox/QEMU VM
//! console reached over `protocol = "spice"`. Mirrors `trireme::vncsrc`: a
//! [`run`] that connects, emits full top-down RGBA frames on each refresh, and
//! drains an `InputEvent` channel forwarding pointer/keys, so the player bridges
//! it into the existing NV12 render path via ffmpeg unchanged.
//!
//! Internally this drives a per-thread GLib main loop over raw
//! `spice-client-glib-2.0` FFI (display + inputs channels). That FFI — and the
//! glib-sys/gobject-sys deps + the spice-gtk system library it links — lives
//! behind the `ffi` feature. The pure-logic pieces (scancode map, pixel
//! conversion, button accounting) compile and are unit-tested unconditionally.

// The pure-logic modules exist to support the FFI session loop *and* to be
// unit-tested without the `.so`. When `ffi` is off their only non-test consumer
// (the `session` module) is compiled out, so they're legitimately dead outside
// `cfg(test)` — allow that rather than littering each item.
#[cfg_attr(not(feature = "ffi"), allow(dead_code))]
mod input_map;
#[cfg_attr(not(feature = "ffi"), allow(dead_code))]
mod keymap;
#[cfg_attr(not(feature = "ffi"), allow(dead_code))]
mod pixels;

#[cfg(feature = "ffi")]
mod ffi;
#[cfg(feature = "ffi")]
mod session;

#[cfg(feature = "ffi")]
pub use session::run;

/// Stub used when the crate is built without the `ffi` feature (no spice-gtk).
/// Returns an error so the player reports a clear reason instead of a silent
/// black cell.
#[cfg(not(feature = "ffi"))]
pub fn run(
    _addr: std::net::SocketAddr,
    _password: Option<&str>,
    _input_rx: Option<tokio::sync::mpsc::UnboundedReceiver<core_proto::InputEvent>>,
    _on_frame: impl FnMut(u16, u16, &[u8]) + Send + 'static,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "SPICE support not compiled in (media-spice built without the `ffi` \
         feature) — rebuild the Trireme client with `--features spice` on a host \
         that has the spice-gtk library installed"
    )
}
