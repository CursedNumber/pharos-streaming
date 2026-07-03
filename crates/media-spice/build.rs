// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Link `libspice-client-glib-2.0` — but only when the `ffi` feature is on.
//!
//! With the feature off (the workspace default) this is a no-op, so the crate
//! builds and its pure-logic tests run on hosts without the `spice-gtk` dev
//! package. With `ffi` on, pkg-config emits the link flags
//! (`-lspice-client-glib-2.0 -lgio-2.0 -lgobject-2.0 -lglib-2.0`) and the right
//! search paths; glib-sys/gobject-sys emit their own glib/gobject directives.
//! We never hardcode `/usr/lib` or the `.8` soname.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_FFI").is_some() {
        pkg_config::Config::new()
            .atleast_version("0.38")
            .probe("spice-client-glib-2.0")
            .expect(
                "spice-client-glib-2.0 not found via pkg-config — install the \
                 `spice-gtk` package (it provides libspice-client-glib-2.0), or \
                 build without the `spice` feature",
            );
    }
}
