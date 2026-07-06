// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! USB/IP import side (BATTLE_PLAN §8): Linux vhci-hcd / Windows signed bridge.
//! The host side that materializes a client-exported device into a virtual hub.
//! Both backends live in `bin/pharos-usbimport.rs`: Linux hands vhci_hcd a
//! socketpair fd via sysfs; Windows drives the usbip-win2 vhci driver through
//! a loopback USB/IP shim (issue #3).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportBackend {
    /// Linux in-kernel virtual host controller (vhci-hcd).
    LinuxVhci,
    /// Windows signed bridge driver (lean on usbipd-win / usbip-win2).
    WindowsBridge,
}

pub fn default_backend() -> ImportBackend {
    if cfg!(windows) {
        ImportBackend::WindowsBridge
    } else {
        ImportBackend::LinuxVhci
    }
}
