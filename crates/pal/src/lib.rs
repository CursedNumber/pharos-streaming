//! Platform Abstraction Layer (PAL) for Pharos.
//!
//! These traits are the seam that lets the Windows and Linux host backends be
//! co-equal (BATTLE_PLAN.md §3, §6). Concrete backends live in the
//! `media-capture`, `media-encode`, `media-audio`, `input-inject`, and
//! `usbip-host` crates and implement these traits.
//!
//! Everything here is deliberately backend-agnostic: no DMA-BUF fds, no D3D11
//! pointers leak into the signatures yet. The skeleton favours clarity; the
//! zero-copy specifics get wired in behind [`GpuFrame`] at M1.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PalError {
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("unsupported mode: {0:?}")]
    UnsupportedMode(Mode),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = core::result::Result<T, PalError>;

/// A display mode the virtual display advertises. We synthesize this ourselves
/// (no real EDID), which is precisely why the EVDI-class EDID bugs don't apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz (120000 = 120.000 Hz).
    pub refresh_mhz: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
    Nv12,
    P010,
}

/// Whether a captured frame lives in GPU memory (zero-copy path) or was copied
/// to a CPU buffer (the EVDI-era fallback lane — kept only as a safety net).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLocation {
    Gpu,
    Cpu,
}

/// An opaque captured frame. At M0 this carries only metadata; the platform
/// handle (DMA-BUF fd / D3D11 texture) is added at M1.
#[derive(Debug, Clone)]
pub struct GpuFrame {
    pub mode: Mode,
    pub format: PixelFormat,
    pub location: FrameLocation,
    /// Monotonic capture timestamp in nanoseconds (for pacing / deadlines).
    pub timestamp_ns: u64,
}

/// Encoded output handed to the packetizer.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub timestamp_ns: u64,
    pub is_keyframe: bool,
}

/// Input flowing client -> host. Note relative *and* absolute pointer plus
/// analog gamepad axes from day one — game-readiness invariant #2.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key { scancode: u32, pressed: bool },
    PointerMotionRelative { dx: f64, dy: f64 },
    PointerMotionAbsolute { x: f64, y: f64 },
    PointerButton { button: u32, pressed: bool },
    Scroll { dx: f64, dy: f64 },
    GamepadButton { pad: u8, button: u16, pressed: bool },
    GamepadAxis { pad: u8, axis: u8, value: f32 },
}

/// Owns a headless output the desktop/game renders into. Linux: hand-rolled
/// Smithay compositor output. Windows: IddCx virtual display.
pub trait VirtualDisplay {
    type Capture: CaptureSource;
    fn create(&mut self, mode: Mode, hdr: bool) -> Result<Self::Capture>;
    /// Live mode/refresh re-negotiation (games switch resolution) — invariant #3.
    fn set_mode(&mut self, mode: Mode) -> Result<()>;
}

/// Produces frames from a virtual display (or, later, a mirrored real session).
pub trait CaptureSource {
    fn next_frame(&mut self) -> Result<GpuFrame>;
}

/// Encodes captured frames. HEVC/NVENC first; AV1/VAAPI/SW behind the same trait.
pub trait Encoder {
    fn submit(&mut self, frame: GpuFrame) -> Result<EncodedPacket>;
}

/// Low-latency audio capture (PipeWire virtual sink / WASAPI loopback) -> Opus.
pub trait AudioSource {
    fn next_chunk(&mut self) -> Result<Vec<u8>>;
}

/// Injects input on the host (uinput / SendInput + ViGEm).
pub trait InputSink {
    fn inject(&mut self, event: InputEvent) -> Result<()>;
}
