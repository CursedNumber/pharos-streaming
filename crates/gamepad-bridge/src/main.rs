// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! `gamepad-bridge` — lowest-latency gamepad forwarding for couch streaming.
//!
//! The controller is paired to the *couch* Pi (the Trireme client). The game
//! runs on the Pharos host. This carries the controller's input the one network
//! hop from Pi → host as **unreliable UDP datagrams** (one per controller
//! report), and replays it through a **uinput** virtual device the game sees as
//! a native pad. Fire-and-forget beats USB/IP here: a dropped sample over WiFi
//! is better than a retransmit stall, and there's no relay-reset jitter.
//!
//!   gamepad-bridge send <host:port> [event-device]   # Pi: read pad, forward
//!   gamepad-bridge recv <bind:port> [--peer <ip>]     # host: inject via uinput
//!
//! Wire format (little-endian, one datagram each):
//!   config: [0x01][bustype,vendor,product,version : u16×4][name_len:u8][name]
//!           [n_keys:u16][key:u16…][n_abs:u16][(code:u16,val,min,max,fuzz,flat,res : i32×6)…]
//!   frame:  [0x02][n:u16][(type:u16, code:u16, value:i32)…]   (one report, incl. SYN)
//!
//! v1 is input-only; rumble (host→pad force-feedback) is a future back-channel.
//!
//! Linux-only by nature (evdev in, uinput out); other platforms get a stub.

#[cfg(target_os = "linux")]
mod bridge;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    bridge::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("gamepad-bridge is Linux-only (evdev capture + uinput replay).");
    std::process::exit(1);
}
