// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! USB export side (Trireme). Step 1: enumerate devices so we can pick one to
//! relay. Claiming the device + servicing USB/IP URBs against it (the actual
//! export) builds on this. Backed by libusb via `rusb`.

use anyhow::Result;

/// Human-readable name for a USB base-class code (enough to spot HID/storage).
fn class_name(code: u8) -> &'static str {
    match code {
        0x00 => "per-interface",
        0x01 => "audio",
        0x03 => "HID",
        0x08 => "mass-storage",
        0x09 => "hub",
        0x0e => "video",
        0xff => "vendor",
        _ => "other",
    }
}

/// List attached USB devices with their interface classes — like `lsusb`, but
/// flags the ones we can relay in v1 (HID / audio / mass-storage).
pub fn list() -> Result<()> {
    for dev in rusb::devices()?.iter() {
        let dd = match dev.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        // The relayable class lives on the interface when bDeviceClass is 0.
        let mut iface_classes = Vec::new();
        if let Ok(cfg) = dev.active_config_descriptor() {
            for iface in cfg.interfaces() {
                for desc in iface.descriptors() {
                    iface_classes.push(desc.class_code());
                }
            }
        }
        let relayable = iface_classes
            .iter()
            .any(|&c| matches!(c, 0x03 | 0x01 | 0x08));
        let ifaces: Vec<String> = iface_classes
            .iter()
            .map(|&c| format!("{:02x}/{}", c, class_name(c)))
            .collect();
        println!(
            "bus {:03} addr {:03}  {:04x}:{:04x}  dev-class {:02x}/{:<13}  ifaces [{}]{}",
            dev.bus_number(),
            dev.address(),
            dd.vendor_id(),
            dd.product_id(),
            dd.class_code(),
            class_name(dd.class_code()),
            ifaces.join(", "),
            if relayable { "  <- relayable" } else { "" },
        );
    }
    Ok(())
}
