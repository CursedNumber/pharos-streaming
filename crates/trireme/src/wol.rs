// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Wake-on-LAN — a Trireme client feature, fully opt-in.
//!
//! Trireme only ever sends a magic packet when the user has supplied the target
//! host's MAC (per-server in `servers.toml`, or passed directly to
//! `trireme wake`). No assumptions are made about the user's network. WoWLAN
//! (wake from wireless) rarely works from a powered-off machine; wired WoL needs
//! the host NIC's "Wake on Magic Packet" + the BIOS setting enabled, and Fast
//! Startup disabled on Windows.

use std::net::UdpSocket;

use anyhow::{Context, Result};

/// Parse `AA:BB:CC:DD:EE:FF` (or `-` separated) into 6 bytes.
pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    anyhow::ensure!(
        parts.len() == 6,
        "MAC must be 6 colon/dash-separated octets, got {s:?}"
    );
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16).with_context(|| format!("bad MAC octet {p:?}"))?;
    }
    Ok(mac)
}

/// The 102-byte magic packet: `FF`×6 then the MAC repeated 16×.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut pkt = [0u8; 102];
    pkt[..6].fill(0xFF);
    for i in 0..16 {
        pkt[6 + i * 6..6 + i * 6 + 6].copy_from_slice(&mac);
    }
    pkt
}

/// Broadcast a magic packet to wake `mac_str`. Sends to the LAN broadcast on the
/// two conventional WoL ports (9 and 7).
pub fn wake(mac_str: &str) -> Result<()> {
    let mac = parse_mac(mac_str)?;
    let pkt = magic_packet(mac);
    let sock = UdpSocket::bind("0.0.0.0:0").context("bind WoL socket")?;
    sock.set_broadcast(true).context("enable UDP broadcast")?;
    let mut sent = 0;
    for port in [9u16, 7] {
        if sock.send_to(&pkt, ("255.255.255.255", port)).is_ok() {
            sent += 1;
        }
    }
    anyhow::ensure!(sent > 0, "failed to send magic packet to the LAN broadcast");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_separators() {
        assert_eq!(
            parse_mac("0A:1b:2C:3d:4E:5f").unwrap(),
            [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f]
        );
        assert_eq!(
            parse_mac("aa-bb-cc-dd-ee-ff").unwrap(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
    }

    #[test]
    fn rejects_bad_macs() {
        assert!(parse_mac("aa:bb:cc:dd:ee").is_err()); // 5 octets
        assert!(parse_mac("zz:bb:cc:dd:ee:ff").is_err()); // non-hex
    }

    #[test]
    fn magic_packet_shape() {
        let p = magic_packet([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[6..12], &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        assert_eq!(&p[96..102], &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
    }
}
