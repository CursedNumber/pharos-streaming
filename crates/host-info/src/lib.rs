// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Host identity helpers (hostname + NIC MACs/IPs) for telemetry, Wake-on-LAN,
//! and MAC-driven socket binding.
//!
//! A lean leaf crate: just cross-platform NIC enumeration via `network-interface`
//! (no tokio/rumqttc), so it cross-compiles cleanly to `windows-msvc` and can be
//! shared by both the host (`integration-mqtt`, `pharos-win`) and the
//! cross-platform `pharos-panel`. The logic here was extracted from
//! `integration-mqtt`'s `host` module, which now re-exports it unchanged.

use network_interface::{NetworkInterface, NetworkInterfaceConfig};

/// Best-effort hostname (env-based, cross-platform).
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pharos".into())
}

/// Max NICs we advertise. A host can have *hundreds* of (virtual) adapters;
/// we surface only a handful of active physical ones. This is the lever to
/// bump if we later expose more interfaces for WAN transport selection.
pub const MAX_NICS: usize = 4;

/// Active, physical NIC MACs to advertise — at most [`MAX_NICS`], lowercase
/// `aa:bb:cc:dd:ee:ff`, deduped. Excludes **inactive** NICs (no routable
/// IPv4 — down/unconfigured), **virtual** adapters (locally-administered MAC:
/// WiFi-Direct, hosted networks, VPN/VM bridges), and the zero MAC. Order is
/// the OS enumeration order of the active set — a true "last-connected" sort
/// needs per-NIC timestamps (OS-specific; deferred), but dead NICs are gone.
pub fn advertised_macs() -> Vec<String> {
    let Ok(ifaces) = NetworkInterface::show() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for ifa in ifaces {
        let Some(mac) = ifa.mac_addr else { continue };
        let mac = mac.to_ascii_lowercase();
        if mac.is_empty() || mac == "00:00:00:00:00:00" {
            continue;
        }
        // Locally-administered (bit 0x02 of octet 0) = virtual adapter.
        let virtual_nic = mac
            .split(':')
            .next()
            .and_then(|o| u8::from_str_radix(o, 16).ok())
            .is_some_and(|first| first & 0x02 != 0);
        if virtual_nic {
            continue;
        }
        // Active = carries a routable IPv4 (not loopback / link-local). A
        // down or unconfigured NIC has none and is skipped.
        let active = ifa.addr.iter().any(|a| match a.ip() {
            std::net::IpAddr::V4(v) => !v.is_loopback() && !v.is_link_local(),
            std::net::IpAddr::V6(_) => false,
        });
        if !active {
            continue;
        }
        if !out.contains(&mac) {
            out.push(mac);
        }
        if out.len() >= MAX_NICS {
            break;
        }
    }
    out
}

/// The first advertised MAC — the conventional "primary".
pub fn primary_mac() -> Option<String> {
    advertised_macs().into_iter().next()
}

/// The IP (IPv4 preferred) of the interface whose MAC matches `target`. Lets a
/// multi-homed host bind the QUIC listener to the operator-designated NIC
/// instead of the wildcard address. Case-insensitive; `:`/`-` not normalized
/// beyond case (callers pass the same form they advertised).
pub fn ip_for_mac(target: &str) -> Option<std::net::IpAddr> {
    let target = target.to_ascii_lowercase();
    let ifaces = NetworkInterface::show().ok()?;
    for ifa in ifaces {
        if ifa.mac_addr.as_deref().map(str::to_ascii_lowercase) != Some(target.clone()) {
            continue;
        }
        if let Some(ip) = ifa
            .addr
            .iter()
            .map(|a| a.ip())
            .find(std::net::IpAddr::is_ipv4)
        {
            return Some(ip);
        }
        if let Some(a) = ifa.addr.first() {
            return Some(a.ip());
        }
    }
    None
}
