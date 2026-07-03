// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Pharos host daemon — M0 skeleton.
//!
//! At M0 this just proves the workspace wires together: it runs the proto
//! round-trip self-test and reports which PAL backends this platform will use.
//! The real daemon (capture -> encode -> QUIC, web console) lands at M1+.

use core_proto::{decode, encode, ControlMsg, PROTOCOL_VERSION};

fn main() {
    println!(
        "Pharos host daemon  (v{}, M0 skeleton)",
        env!("CARGO_PKG_VERSION")
    );
    println!("protocol version:   {PROTOCOL_VERSION}");
    println!(
        "platform:           {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("capture backend:    {:?}", media_capture::default_backend());
    println!("encode backend:     {:?}", media_encode::default_backend());
    println!("audio backend:      {:?}", media_audio::default_backend());
    println!("input backend:      {:?}", input_inject::default_backend());

    // Proto round-trip self-test (the same contract Trireme speaks).
    let hello = ControlMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        agent: format!("pharos/{}", env!("CARGO_PKG_VERSION")),
    };
    let bytes = encode(&hello).expect("encode");
    let back = decode(&bytes).expect("decode");
    assert_eq!(hello, back);
    println!("proto round-trip:   OK ({} bytes)", bytes.len());

    println!();
    println!("Pharos host self-test passed. See README.md.");
}
