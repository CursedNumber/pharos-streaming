//! Latency-benchmark wire types for Pharos ↔ Trireme.
//!
//! These ride over QUIC as **postcard-encoded datagram bodies**. The exchange is
//! *client-initiated*: the host opens a bench window by tagging outgoing frames
//! with [`FrameStageMeta`] (compose / encode-start / encode-done / wire-send
//! stamps), and that first meta is the client's signal to probe the clock with
//! [`ClockPing`]/[`ClockPong`]
//! (an NTP-style 4-timestamp exchange — the **client** sends the ping, the **host**
//! responds). The client then computes per-stage attribution and sends back a
//! [`WireLatencyReport`].
//!
//! Timestamp convention is direction-agnostic: `t0` = initiator (client) send,
//! `t1`/`t2` = responder (host) receive/send, `t3` = initiator (client) receive.
//! All cross-machine timestamps are **UNIX-epoch nanoseconds** (`SystemTime`), not
//! a monotonic `Instant` — the two ends need a shared epoch to compute the offset.
//!
//! All types are plain `serde` structs — deliberately **not** UniFFI-exported.
//! Encode/decode mirror the `encode_input`/`decode_input` pattern in
//! [`crate`] (same [`WireError`], same shape).

use serde::{Deserialize, Serialize};

use crate::WireError;

/// Client → host: a clock-sync probe (the client is the initiator). `t0_nanos` is
/// the client's send timestamp (UNIX-epoch nanos). `seq` correlates the matching
/// [`ClockPong`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClockPing {
    pub seq: u32,
    pub t0_nanos: u64,
}

/// Host → client: the reply to a [`ClockPing`] (the host is the responder).
/// Carries the original `t0_nanos` plus the host's receive (`t1_nanos`) and send
/// (`t2_nanos`) timestamps, so the client can compute RTT and clock offset
/// NTP-style (with its own `t3` = pong-receive instant). All UNIX-epoch nanos.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClockPong {
    pub seq: u32,
    pub t0_nanos: u64,
    pub t1_nanos: u64,
    pub t2_nanos: u64,
}

/// Host → client: per-frame stage timestamps, **all host-clock UNIX-epoch
/// nanoseconds**. Every stage on the host is now stamped explicitly so the client
/// attributes compose/encode/host-send as pure host-side subtractions (no
/// cross-clock arithmetic — those are offset-free). The four host stamps:
///   * `capture_ns`      — render/compose start.
///   * `encode_start_ns` — just before NVENC `encode_picture`.
///   * `encode_done_ns`  — just after the NVENC bitstream lock.
///   * `wire_send_ns`    — just before the transport's `write_all` (filled IN THE
///     TRANSPORT, immediately before the AU hits the wire, not by the encoder).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FrameStageMeta {
    pub frame_idx: u64,
    pub capture_ns: u64,
    pub encode_start_ns: u64,
    pub encode_done_ns: u64,
    pub wire_send_ns: u64,
}

/// Client → host: the client-computed per-stage latency attribution (all stages
/// in **ms** unless noted), plus its clock estimate. The host surfaces this in the
/// panel's latency report.
///
/// The host stages (`compose`/`encode`/`host_send`) are pure host-clock
/// subtractions of [`FrameStageMeta`] stamps, averaged over the window. The wire
/// stage `nexus_ms` is `rtt_ms / 2` (pure router/link time, RTT-derived — no
/// frame join, no clock offset); `nexus_uncertainty_ms` is half the RTT spread
/// over the clock burst. The client-local stages come from the player taps:
/// `decode_ms` is the *minimum* decode time seen (≈ pure decode), `buffer_ms` is
/// the mean-minus-min decode residency (in-flight / jitter), and `present_ms` is
/// the mean present time. `clock_offset_ms` is now **diagnostic only** — no stage
/// is computed from it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireLatencyReport {
    /// Codec four-CC, e.g. `*b"hevc"`.
    pub codec_fourcc: [u8; 4],
    pub samples: u32,
    pub rtt_ms: f32,
    pub clock_offset_ms: f32,
    pub compose_ms: f32,
    pub encode_ms: f32,
    pub host_send_ms: f32,
    pub nexus_ms: f32,
    pub nexus_uncertainty_ms: f32,
    pub buffer_ms: f32,
    pub decode_ms: f32,
    pub present_ms: f32,
    pub total_ms: f32,
}

/// Client → host: a lightweight, **continuous** quality-of-service report (NOT
/// bench-gated — sent ~1×/s for the whole session). It tells the host the
/// *delivered* experience — how many frames actually arrived per second, and how
/// long they take to decode on the client — so the host can adapt its send rate
/// to the path instead of flooding it open-loop. Rides a `DG_CLIENT_QOS`
/// datagram; both the Rust and (via the shared transport) Android clients emit it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClientQos {
    /// Frames received over the last reporting window, per second. The key
    /// congestion signal: if this sits well below the host's send rate, the path
    /// is shedding frames and the host should back off.
    pub recv_fps: f32,
    /// Recent mean client-side decode time (ms); `0.0` when unknown.
    pub decode_ms: f32,
}

/// Encode a [`ClientQos`] to bytes (postcard).
pub fn encode_client_qos(q: &ClientQos) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(q).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClientQos`] from bytes.
pub fn decode_client_qos(bytes: &[u8]) -> std::result::Result<ClientQos, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Client → host: the client's desired resolution range for the adaptive ladder
/// (Phase 2). `target_h` is the ceiling coded height it wants the host to uphold
/// (`0` = Auto / native); `floor_h` is the minimum it may auto-downscale to
/// (`0` = no floor / lowest rung). The host maps these per-output (aspect-
/// preserving) and adapts within `[floor_h, target_h]`. Sent on a
/// `DG_CLIENT_RESPREF` datagram ~1×/s (continuous, like [`ClientQos`]) so a lost
/// one self-heals; the host applies it idempotently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientResPref {
    /// Target/ceiling coded height in px; `0` = Auto (native).
    pub target_h: u32,
    /// Minimum coded height the ladder may drop to; `0` = no floor (lowest rung).
    pub floor_h: u32,
}

/// Encode a [`ClientResPref`] to bytes (postcard).
pub fn encode_client_res_pref(p: &ClientResPref) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(p).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClientResPref`] from bytes.
pub fn decode_client_res_pref(bytes: &[u8]) -> std::result::Result<ClientResPref, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Client → host: the client's desired bitrate range for the adaptive controller
/// (Phase 3) — the PRIMARY auto-recover lever. `min_kbps` is the floor the host
/// never auto-drops below; `max_kbps` is the requested ceiling, with `0` meaning
/// "uncapped" (the host owns the top via its own rate-control / loss controller,
/// still bounded by the QUIC flow window — NOT unbounded send). The controller
/// adapts within `[min_kbps, max_kbps]` and only drops resolution once bitrate is
/// at the floor. Sent on a `DG_CLIENT_BITRATEPREF` datagram ~1×/s (continuous,
/// like [`ClientQos`]); the host applies it idempotently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientBitratePref {
    /// Floor bitrate in kbit/s the controller never auto-drops below; `0` = host default.
    pub min_kbps: u32,
    /// Ceiling bitrate in kbit/s; `0` = uncapped (host owns the top).
    pub max_kbps: u32,
}

/// Encode a [`ClientBitratePref`] to bytes (postcard).
pub fn encode_client_bitrate_pref(
    p: &ClientBitratePref,
) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(p).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClientBitratePref`] from bytes.
pub fn decode_client_bitrate_pref(
    bytes: &[u8],
) -> std::result::Result<ClientBitratePref, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Client → host: the client's desired framerate range (Phase 3). `max_fps` is the
/// target ceiling the host paces the encode (and, for direct-launched apps, the
/// game render) to — bounded to the panel's physical refresh either way; `0` =
/// Auto (host uses the panel refresh / its configured ceiling). `min_fps` is the
/// adaptation floor when fps is the active lever (preset RC); `0` = host default
/// (24). Lets the user cap fps below the panel rate to save GPU/heat/bandwidth.
/// Sent on a `DG_CLIENT_FPSPREF` datagram ~1×/s (continuous, like [`ClientQos`]);
/// the host applies it idempotently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientFpsPref {
    /// Adaptation floor in fps the controller never auto-drops below; `0` = host default (24).
    pub min_fps: u16,
    /// Target ceiling in fps (clamped to the panel refresh); `0` = Auto (panel refresh).
    pub max_fps: u16,
}

/// Encode a [`ClientFpsPref`] to bytes (postcard).
pub fn encode_client_fps_pref(p: &ClientFpsPref) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(p).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClientFpsPref`] from bytes.
pub fn decode_client_fps_pref(bytes: &[u8]) -> std::result::Result<ClientFpsPref, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Encode a [`ClockPing`] to bytes (postcard).
pub fn encode_clock_ping(p: &ClockPing) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(p).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClockPing`] from bytes.
pub fn decode_clock_ping(bytes: &[u8]) -> std::result::Result<ClockPing, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Encode a [`ClockPong`] to bytes (postcard).
pub fn encode_clock_pong(p: &ClockPong) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(p).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClockPong`] from bytes.
pub fn decode_clock_pong(bytes: &[u8]) -> std::result::Result<ClockPong, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Encode a [`FrameStageMeta`] to bytes (postcard).
pub fn encode_frame_meta(m: &FrameStageMeta) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(m).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`FrameStageMeta`] from bytes.
pub fn decode_frame_meta(bytes: &[u8]) -> std::result::Result<FrameStageMeta, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Encode a [`WireLatencyReport`] to bytes (postcard).
pub fn encode_wire_latency(r: &WireLatencyReport) -> std::result::Result<Vec<u8>, WireError> {
    postcard::to_stdvec(r).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`WireLatencyReport`] from bytes.
pub fn decode_wire_latency(bytes: &[u8]) -> std::result::Result<WireLatencyReport, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_ping_roundtrip() {
        let p = ClockPing {
            seq: 7,
            t0_nanos: 123_456_789,
        };
        let bytes = encode_clock_ping(&p).unwrap();
        let back = decode_clock_ping(&bytes).unwrap();
        assert_eq!(back.seq, p.seq);
        assert_eq!(back.t0_nanos, p.t0_nanos);
    }

    #[test]
    fn clock_pong_roundtrip() {
        let p = ClockPong {
            seq: 7,
            t0_nanos: 100,
            t1_nanos: 250,
            t2_nanos: 260,
        };
        let bytes = encode_clock_pong(&p).unwrap();
        let back = decode_clock_pong(&bytes).unwrap();
        assert_eq!(back.seq, p.seq);
        assert_eq!(back.t0_nanos, p.t0_nanos);
        assert_eq!(back.t1_nanos, p.t1_nanos);
        assert_eq!(back.t2_nanos, p.t2_nanos);
    }

    #[test]
    fn frame_meta_roundtrip() {
        let m = FrameStageMeta {
            frame_idx: 4096,
            capture_ns: 1_000,
            encode_start_ns: 1_200,
            encode_done_ns: 3_500,
            wire_send_ns: 3_800,
        };
        let bytes = encode_frame_meta(&m).unwrap();
        let back = decode_frame_meta(&bytes).unwrap();
        assert_eq!(back.frame_idx, m.frame_idx);
        assert_eq!(back.capture_ns, m.capture_ns);
        assert_eq!(back.encode_start_ns, m.encode_start_ns);
        assert_eq!(back.encode_done_ns, m.encode_done_ns);
        assert_eq!(back.wire_send_ns, m.wire_send_ns);
    }

    #[test]
    fn client_qos_roundtrip() {
        let q = ClientQos {
            recv_fps: 58.5,
            decode_ms: 6.25,
        };
        let bytes = encode_client_qos(&q).unwrap();
        let back = decode_client_qos(&bytes).unwrap();
        assert_eq!(back, q);
    }

    #[test]
    fn wire_latency_roundtrip() {
        let r = WireLatencyReport {
            codec_fourcc: *b"hevc",
            samples: 120,
            rtt_ms: 4.2,
            clock_offset_ms: -1.5,
            compose_ms: 0.8,
            encode_ms: 2.1,
            host_send_ms: 0.3,
            nexus_ms: 1.9,
            nexus_uncertainty_ms: 0.5,
            buffer_ms: 0.7,
            decode_ms: 0.4,
            present_ms: 1.2,
            total_ms: 8.9,
        };
        let bytes = encode_wire_latency(&r).unwrap();
        let back = decode_wire_latency(&bytes).unwrap();
        assert_eq!(back.codec_fourcc, r.codec_fourcc);
        assert_eq!(back.samples, r.samples);
        assert_eq!(back.rtt_ms, r.rtt_ms);
        assert_eq!(back.clock_offset_ms, r.clock_offset_ms);
        assert_eq!(back.compose_ms, r.compose_ms);
        assert_eq!(back.encode_ms, r.encode_ms);
        assert_eq!(back.host_send_ms, r.host_send_ms);
        assert_eq!(back.nexus_ms, r.nexus_ms);
        assert_eq!(back.nexus_uncertainty_ms, r.nexus_uncertainty_ms);
        assert_eq!(back.buffer_ms, r.buffer_ms);
        assert_eq!(back.decode_ms, r.decode_ms);
        assert_eq!(back.present_ms, r.present_ms);
        assert_eq!(back.total_ms, r.total_ms);
    }
}
