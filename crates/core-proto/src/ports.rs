//! Default wire ports for the Pharos ⇄ Trireme transports.
//!
//! These were previously repeated as bare string literals (`"0.0.0.0:4432"`,
//! `"127.0.0.1:15777"`, …) across the compositor, the Windows host, the USB/IP
//! import binary, and both clients. Centralizing them here means a re-number is
//! a one-line change instead of a cross-crate grep. The bind *host* (`0.0.0.0`
//! vs `127.0.0.1`) stays at each call site — only the port magic lives here.

/// Pairing (SPAKE2) listener — `pharos-compositor pair` / `pharos-host pair`.
pub const PAIR: u16 = 4432;

/// Legacy stream port (pre-WAN). **Deprecated**: the live stream moved to
/// [`STREAM`]; this lingers only in older example configs and tests.
pub const STREAM_LEGACY: u16 = 4433;

/// USB/IP import listener (`pharos-usbimport`); the client's export target.
pub const USBIP: u16 = 4434;

/// Windows host stream (`pharos-host stream`).
pub const WIN_STREAM: u16 = 4435;

/// Current QUIC stream port — the single deliberate LAN+WAN port.
pub const STREAM: u16 = 15777;

/// TCP control plane (panel ⟷ host) for hosts reached over the network — the
/// Windows host, or a remote Linux host. The local Linux compositor still uses
/// a Unix-domain socket; this is the cross-machine equivalent.
pub const CONTROL: u16 = 15778;
