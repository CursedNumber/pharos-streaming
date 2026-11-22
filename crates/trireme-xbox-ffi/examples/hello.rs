// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Phase-0 "hello core" smoke test: link the shim and print the protocol
//! version, proving the C ABI executes on the target.
//!
//!   cargo run -p trireme-xbox-ffi --example hello --target x86_64-pc-windows-msvc
//!
//! (Also runs on the host toolchain for a quick local sanity check.)

fn main() {
    let v = trireme_xbox::trireme_protocol_version();
    println!("trireme-xbox-ffi OK: wire protocol version = {v}");
}
