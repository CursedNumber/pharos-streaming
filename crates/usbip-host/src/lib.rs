//! USB/IP import side (BATTLE_PLAN §8): Linux vhci-hcd / Windows signed bridge.
//! The host side that materializes a client-exported device into a virtual hub.
//! M0 stub — lands at M5 (Linux) / M3 (Windows bridge).

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
