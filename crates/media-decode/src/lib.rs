// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! HEVC hardware-decoder capability probe.
//!
//! Trireme decodes the Pharos HEVC stream by piping it into an `ffmpeg`
//! subprocess. Which HW decoder to ask for is platform-specific — `hevc_cuvid`
//! (NVDEC) on the dev rig, `hevc_qsv` (Quick Sync) on Intel, `hevc_rkmpp` on a
//! Rockchip RK3588, `-hwaccel drm` (V4L2 stateless) on a Raspberry Pi 5, and so
//! on. Rather than hard-code one per `cfg!(target_os)`, this crate **probes the
//! running box** and picks the best one, and exposes the full per-method result
//! so a settings UI can show the menu and grey out what isn't usable.
//!
//! ## Why a real test-decode, not just `ffmpeg -decoders`
//!
//! Listing what ffmpeg was *built* with is misleading: a generic Linux ffmpeg
//! on an NVIDIA box advertises `hevc_qsv`, `hevc_v4l2m2m`, `hevc_amf` and the
//! `vaapi`/`drm` hwaccels even though none of them have a working device. So we
//! actually **decode a tiny embedded HEVC sample** through each candidate and
//! keep only the ones that emit a frame. That is the honest "gauge".
//!
//! ## Why a listing-based safety net
//!
//! NVDEC init is observably flaky under GPU contention — a `hevc_cuvid`
//! test-decode can transiently fail at startup even on a box that streams fine.
//! If a transient failure silently dropped us to software decode we'd reintro a
//! latency regression. So: if the test-decode confirms a HW decoder, use it; but
//! if *every* HW test-decode fails while a HW decoder is nonetheless compiled in,
//! fall back to the highest-priority compiled-in HW decoder (with a loud warning)
//! rather than silently to software. `TRIREME_DECODER` overrides everything.
//!
//! The selected decoder is always logged at startup (`tracing::info!`).

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A few-frame all-IDR 256×256 HEVC elementary stream (Annex-B), used to
/// test-decode each candidate. Tiny (~5 KiB) and embedded in the binary so the
/// probe needs no files on the target.
const SAMPLE: &[u8] = include_bytes!("sample.hevc");

/// How a candidate is selected on the ffmpeg command line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A standalone decoder: `-c:v <token>` (e.g. `hevc_cuvid`, `hevc_rkmpp`).
    Decoder,
    /// A hwaccel on the generic `hevc` decoder: `-hwaccel <token>` (e.g. `drm`).
    Hwaccel,
    /// ffmpeg's built-in software decoder — no extra args, always available.
    Software,
}

/// A decode method the probe knows about (a static catalogue entry).
struct Candidate {
    /// Short stable id used by `TRIREME_DECODER` and the UI (`cuvid`, `rkmpp`…).
    id: &'static str,
    /// Human label for the UI.
    label: &'static str,
    /// ffmpeg decoder/hwaccel name (`""` for software).
    token: &'static str,
    kind: Kind,
}

/// The catalogue, in **selection-priority order** (best first). Only the
/// candidate(s) actually compiled into *and* usable by the local ffmpeg matter;
/// on any given board most of these are absent, so the ordering only breaks ties
/// when several genuinely work at once.
static CATALOGUE: &[Candidate] = &[
    Candidate {
        id: "cuvid",
        label: "NVIDIA NVDEC",
        token: "hevc_cuvid",
        kind: Kind::Decoder,
    },
    Candidate {
        id: "rkmpp",
        label: "Rockchip MPP (RK3588…)",
        token: "hevc_rkmpp",
        kind: Kind::Decoder,
    },
    Candidate {
        id: "qsv",
        label: "Intel Quick Sync",
        token: "hevc_qsv",
        kind: Kind::Decoder,
    },
    Candidate {
        id: "vaapi",
        label: "VA-API (Intel/AMD)",
        token: "vaapi",
        kind: Kind::Hwaccel,
    },
    Candidate {
        id: "amf",
        label: "AMD AMF",
        token: "hevc_amf",
        kind: Kind::Decoder,
    },
    Candidate {
        id: "drm",
        label: "V4L2 stateless (Raspberry Pi 5…)",
        token: "drm",
        kind: Kind::Hwaccel,
    },
    Candidate {
        id: "v4l2m2m",
        label: "V4L2 M2M (Raspberry Pi 4…)",
        token: "hevc_v4l2m2m",
        kind: Kind::Decoder,
    },
    Candidate {
        id: "software",
        label: "Software (CPU)",
        token: "",
        kind: Kind::Software,
    },
];

/// The probed status of one decode method — the row a settings UI renders.
#[derive(Clone, Debug)]
pub struct Capability {
    pub id: String,
    pub label: String,
    pub kind: Kind,
    /// The ffmpeg args this method injects before `-f hevc -i pipe:0`.
    pub input_args: Vec<String>,
    /// Built into the local ffmpeg at all.
    pub compiled_in: bool,
    /// Confirmed to actually decode (software is always `true`; methods not
    /// compiled in are always `false`). This is the "grey it out" flag.
    pub usable: bool,
    /// Human-readable reason, for a log line or a UI tooltip.
    pub detail: String,
}

/// The full probe result.
#[derive(Clone, Debug)]
pub struct Capabilities {
    /// Every catalogue method, in priority order.
    pub methods: Vec<Capability>,
    /// Index into `methods` of the chosen decoder.
    pub selected: usize,
    /// Why that one was chosen (override / test-decode / listing fallback).
    pub selected_reason: String,
}

impl Capabilities {
    /// The chosen method.
    pub fn chosen(&self) -> &Capability {
        &self.methods[self.selected]
    }
    /// ffmpeg input args for the chosen method (to splice before `-f hevc`).
    pub fn input_args(&self) -> Vec<String> {
        self.chosen().input_args.clone()
    }
}

/// Decoders verified **bit-exact** with `-flags low_delay` on this project's
/// ffmpeg, where it ~halves the decode-side in-flight latency (NVDEC holds fewer
/// pictures before output) with byte-identical frames. NOT applied blanket: the
/// trireme `play.rs` lore records that `-probesize/-analyzeduration` starve the
/// QSV SPS parser (`Picture size 0x0`) and `nobuffer`+`low_delay` broke d3d11va.
/// `nobuffer` alone also changes cuvid output (the real "green-frame" flag). Add
/// an id here ONLY after MD5-verifying `low_delay` on that decoder.
///   - cuvid: ffmpeg n8.1.2 / RTX 4070 — MD5-identical to baseline, 99.8→51.4ms.
const LOW_DELAY_SAFE: &[&str] = &["cuvid"];

impl Candidate {
    fn input_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if LOW_DELAY_SAFE.contains(&self.id) {
            a.push("-flags".into());
            a.push("low_delay".into());
        }
        match self.kind {
            Kind::Decoder => a.extend(["-c:v".into(), self.token.into()]),
            Kind::Hwaccel => a.extend(["-hwaccel".into(), self.token.into()]),
            Kind::Software => {}
        }
        a
    }
}

/// Resolve the ffmpeg executable. Mirrors the client's own resolver: on Windows
/// the WinGet "Links" alias dir isn't on a pre-existing shell's PATH, so check
/// it directly. (Duplicated rather than shared to keep this crate dependency-free
/// and usable from both `trireme` and `trireme-kms`.)
pub fn ffmpeg_program() -> OsString {
    #[cfg(target_os = "windows")]
    {
        if let Some(la) = std::env::var_os("LOCALAPPDATA") {
            let cand = std::path::Path::new(&la)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("ffmpeg.exe");
            if cand.exists() {
                return cand.into_os_string();
            }
        }
    }
    OsString::from("ffmpeg")
}

/// Resolve the ffprobe executable — the same WinGet-Links-aware lookup as
/// [`ffmpeg_program`] (ffprobe ships alongside ffmpeg in that alias dir, which
/// isn't on a pre-existing shell's PATH on Windows). Used by the panel's
/// media-track picker.
pub fn ffprobe_program() -> OsString {
    #[cfg(target_os = "windows")]
    {
        if let Some(la) = std::env::var_os("LOCALAPPDATA") {
            let cand = std::path::Path::new(&la)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("ffprobe.exe");
            if cand.exists() {
                return cand.into_os_string();
            }
        }
    }
    OsString::from("ffprobe")
}

/// Run the capability probe (test-decode each catalogue method, choose, log).
/// Shared by the cached [`capabilities`] and the uncached [`reprobe`].
fn probe() -> Capabilities {
    let caps = detect(&ffmpeg_program());
    log_capabilities(&caps);
    caps
}

/// Probe the box once and cache the result for the process. The first call runs
/// the test-decodes (a few hundred ms to ~2 s) and logs the outcome.
pub fn capabilities() -> &'static Capabilities {
    static CELL: OnceLock<Capabilities> = OnceLock::new();
    CELL.get_or_init(probe)
}

/// Re-run the probe, **bypassing** the [`capabilities`] process cache, and return
/// a fresh owned [`Capabilities`]. Lets a caller (e.g. the panel's "Re-probe"
/// action) reflect a GPU plugged/unplugged *after* startup without a restart. The
/// cached [`capabilities`] result is left untouched, so `input_args` /
/// `selected_label` stay stable for an already-running decode.
pub fn reprobe() -> Capabilities {
    probe()
}

/// Convenience: the ffmpeg input args for the chosen decoder (cached probe).
pub fn input_args() -> Vec<String> {
    capabilities().input_args()
}

/// Convenience: a short label for the chosen decoder (e.g. for a status line).
pub fn selected_label() -> &'static str {
    let c = capabilities();
    &c.methods[c.selected].label
}

// ---------------------------------------------------------------------------
// Codec-aware decode (HEVC default; AV1/AVC/VVC support)
// ---------------------------------------------------------------------------

/// The video codec a stream carries. HEVC is the project default; AV1 is the
/// opt-in the host can encode on Ada+ NVENC; H264 (AVC) is the universal
/// legacy-compatibility codec; Vvc (H.266) is the experimental successor the
/// host software-encodes via libvvenc.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamCodec {
    Hevc,
    Av1,
    H264,
    Vvc,
}

impl StreamCodec {
    /// The ffmpeg input demuxer (`-f`). HEVC/H.264/VVC = their raw Annex-B
    /// demuxers (`hevc`/`h264`/`vvc`). AV1 = `obu` (NVENC emits the
    /// low-overhead OBU stream, which is also what `sniff_codec` matches);
    /// override with `TRIREME_AV1_FORMAT` (`obu`|`av1`) if a host ever produces
    /// Annex-B AV1 instead. This is the one bit worth confirming against a live
    /// NVENC AV1 stream.
    pub fn ffmpeg_format(self) -> String {
        match self {
            StreamCodec::Hevc => "hevc".to_string(),
            StreamCodec::H264 => "h264".to_string(),
            StreamCodec::Vvc => "vvc".to_string(),
            StreamCodec::Av1 => std::env::var("TRIREME_AV1_FORMAT")
                .ok()
                .filter(|s| matches!(s.as_str(), "obu" | "av1"))
                .unwrap_or_else(|| "obu".to_string()),
        }
    }
    /// Short uppercase codec abbreviation.
    pub fn label(self) -> &'static str {
        match self {
            StreamCodec::Hevc => "HEVC",
            StreamCodec::Av1 => "AV1",
            StreamCodec::H264 => "AVC",
            StreamCodec::Vvc => "VVC",
        }
    }
    /// Parse a codec name (case-insensitive `hevc`/`h265` / `av1` /
    /// `avc`/`h264` / `vvc`/`h266`); `None` if unrecognised.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hevc" | "h265" | "h.265" => Some(StreamCodec::Hevc),
            "av1" => Some(StreamCodec::Av1),
            "avc" | "h264" | "h.264" => Some(StreamCodec::H264),
            "vvc" | "h266" | "h.266" => Some(StreamCodec::Vvc),
            _ => None,
        }
    }
}

/// The explicit `TRIREME_CODEC` override, if set to a recognised codec name
/// (mirrors `TRIREME_DECODER`). Unlike [`codec_from_env`] this distinguishes
/// "unset" (`None` — the caller may sniff the stream instead) from an explicit
/// user choice that must never be second-guessed by a sniff.
pub fn codec_override_from_env() -> Option<StreamCodec> {
    std::env::var("TRIREME_CODEC")
        .ok()
        .and_then(|s| StreamCodec::parse(&s))
}

/// The codec the client should decode: the `TRIREME_CODEC` env override if set,
/// else `default`. Callers that can see the bitstream should prefer
/// [`codec_override_from_env`] + [`sniff_codec`] on the first AU, which follows
/// a host codec switch with no env var at all.
pub fn codec_from_env(default: StreamCodec) -> StreamCodec {
    codec_override_from_env().unwrap_or(default)
}

/// Sniff the codec from the first bytes of an access unit, or `None` when the
/// AU can't be classified. AV1 is easy — a low-overhead OBU stream has no NAL
/// start codes, and every NVENC temporal unit opens with a temporal delimiter,
/// so AV1 classifies on any AU. H.264, HEVC and VVC are ALL Annex-B
/// (`00 00 [00] 01` start codes), told apart by the first NAL header — but
/// ONLY on "stream opener" NALs (parameter sets / IRAP / AUD / SEI), which
/// every host encoder attaches to each IDR via parameter-set repetition:
///
/// * VVC's two-byte header is `layer-id byte` + `type(5)|tid+1(3)`: base-layer
///   parameter sets / AUD / picture-header / IRAP give `0x00` then a type-coded
///   second byte — no other codec starts a NAL with `0x00` + those types.
/// * HEVC's two-byte header is `type(6)<<1` + `layer/tid`: layer-0 gives a
///   second byte of exactly `tid+1` (1..=7), and NVENC opens with VPS (0x40).
/// * H.264's one-byte header (`ref_idc(2)|type(5)`) opens with SPS/AUD/SEI/IDR
///   — types that don't parse as plausible HEVC or VVC openers.
///
/// Mid-GOP P-slices MUST return `None`, never a guess: a mid-stream joiner's
/// first AU is usually one, and they are mutually ambiguous across the NAL
/// codecs (`0x61` is an H.264 non-IDR slice; HEVC TRAIL_N and VVC TRAIL share
/// their first two bytes exactly). Callers skip unclassifiable AUs and re-try
/// on the next one — the host's periodic IDR (≤1s) always classifies. Guessing
/// here is what put an HEVC decoder on a live AVC stream (black screen,
/// 2026-07-14).
pub fn sniff_codec(head: &[u8]) -> Option<StreamCodec> {
    let nal = if head.starts_with(&[0, 0, 0, 1]) {
        Some(&head[4..])
    } else if head.starts_with(&[0, 0, 1]) {
        Some(&head[3..])
    } else {
        None
    };
    if let Some(nal) = nal {
        let (Some(&b0), Some(&b1)) = (nal.first(), nal.get(1)) else {
            return None;
        };
        // VVC: base layer (b0 == 0x00), second byte carries type + tid.
        // Types: IRAP 7..=9, VPS 14, SPS 15, PPS 16, prefix-APS 17, PH 19,
        // AUD 20, prefix-SEI 23. tid+1 must be non-zero.
        if b0 == 0x00 && (b1 & 0x07) != 0 && matches!(b1 >> 3, 7..=9 | 14..=17 | 19 | 20 | 23) {
            return Some(StreamCodec::Vvc);
        }
        // HEVC: layer-0 second byte is tid+1 (1..=7); opener types:
        // IRAP 16..=23, VPS/SPS/PPS 32..=34, AUD 35, prefix-SEI 39.
        let hevc_type = (b0 >> 1) & 0x3f;
        if (b0 & 0x81) == 0 && (1..=7).contains(&b1) && matches!(hevc_type, 16..=23 | 32..=35 | 39)
        {
            return Some(StreamCodec::Hevc);
        }
        // H.264: forbidden bit clear; opener types IDR 5, SEI 6, SPS 7,
        // PPS 8, AUD 9.
        if (b0 & 0x80) == 0 && matches!(b0 & 0x1f, 5..=9) {
            return Some(StreamCodec::H264);
        }
        // Anything else (P/B slices, suffix NALs…) is not classifiable.
        return None;
    }
    if let Some(&b) = head.first() {
        let forbidden = b & 0x80;
        let obu_type = (b >> 3) & 0x0f;
        // 1 = sequence header, 2 = temporal delimiter, 6 = frame OBU.
        if forbidden == 0 && matches!(obu_type, 1 | 2 | 6) {
            return Some(StreamCodec::Av1);
        }
    }
    None
}

/// Whether an access unit can start a fresh decode: it contains an HEVC IRAP
/// NAL (IDR/CRA/BLA), an H.264 IDR, a VVC IRAP (IDR/CRA), or an AV1 key frame.
/// The client's decode-chain rebuild (issue #32) gates what it feeds a freshly
/// spawned ffmpeg on this — feeding a new decoder mid-GOP makes it emit
/// zero-initialised concealment frames, which render as the green flash. False
/// negatives would freeze the stream, so the parsers err permissive: the NAL
/// codecs key on the NAL type alone, AV1 accepts either a sequence-header OBU
/// (NVENC repeats it on every key frame, mirroring repeatSPSPPS) or an explicit
/// KEY_FRAME frame header.
pub fn is_keyframe_au(codec: StreamCodec, au: &[u8]) -> bool {
    match codec {
        StreamCodec::Hevc => hevc_has_irap(au),
        StreamCodec::Av1 => av1_has_key(au),
        StreamCodec::H264 => h264_has_idr(au),
        StreamCodec::Vvc => vvc_has_irap(au),
    }
}

/// Scan an Annex-B AU for an IRAP NAL. HEVC `nal_unit_type` lives in bits 6..1
/// of the first byte after the start code; types 16..=23 are the IRAP class
/// (BLA_W_LP..RSV_IRAP_VCL23), any of which starts a decodable chain.
fn hevc_has_irap(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        // Both 3- and 4-byte start codes appear in NVENC output.
        if au[i] == 0 && au[i + 1] == 0 {
            let hdr = if au[i + 2] == 1 {
                i + 3
            } else if au[i + 2] == 0 && au.get(i + 3) == Some(&1) {
                i + 4
            } else {
                i += 1;
                continue;
            };
            if let Some(&b) = au.get(hdr) {
                if (16..=23).contains(&((b >> 1) & 0x3f)) {
                    return true;
                }
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    false
}

/// Scan an Annex-B AU for an H.264 IDR slice (`nal_unit_type` in the low 5
/// bits of the header byte; type 5 = IDR). NVENC's repeatSPSPPS attaches
/// SPS/PPS to every IDR AU, so keying on the IDR alone is sufficient.
fn h264_has_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 {
            let hdr = if au[i + 2] == 1 {
                i + 3
            } else if au[i + 2] == 0 && au.get(i + 3) == Some(&1) {
                i + 4
            } else {
                i += 1;
                continue;
            };
            if let Some(&b) = au.get(hdr) {
                if (b & 0x80) == 0 && (b & 0x1f) == 5 {
                    return true;
                }
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    false
}

/// Scan an Annex-B AU for a VVC IRAP NAL. The two-byte VVC header is
/// `forbidden(1) reserved(1) layer_id(6)` + `nal_type(5) tid+1(3)`; IRAP types
/// are IDR_W_RADL 7, IDR_N_LP 8, CRA 9, any of which starts a decodable chain.
fn vvc_has_irap(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 {
            let hdr = if au[i + 2] == 1 {
                i + 3
            } else if au[i + 2] == 0 && au.get(i + 3) == Some(&1) {
                i + 4
            } else {
                i += 1;
                continue;
            };
            if let (Some(&b0), Some(&b1)) = (au.get(hdr), au.get(hdr + 1)) {
                if (b0 & 0xC0) == 0 && (7..=9).contains(&(b1 >> 3)) {
                    return true;
                }
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    false
}

/// Walk a low-overhead AV1 OBU stream for key-frame evidence: a sequence-header
/// OBU (type 1), or a frame / frame-header OBU (6 / 3) whose first bits decode
/// to `show_existing_frame=0, frame_type=KEY_FRAME`. Malformed input just ends
/// the scan (→ not a key frame).
fn av1_has_key(au: &[u8]) -> bool {
    let mut i = 0;
    while i < au.len() {
        let b = au[i];
        if b & 0x80 != 0 {
            return false; // forbidden bit — not an OBU boundary; bail
        }
        let obu_type = (b >> 3) & 0x0f;
        let has_ext = b & 0x04 != 0;
        let has_size = b & 0x02 != 0;
        let mut p = i + 1 + usize::from(has_ext);
        let size = if has_size {
            // leb128, ≤8 bytes per spec.
            let mut v: u64 = 0;
            let mut ok = false;
            for k in 0..8 {
                let Some(&sb) = au.get(p) else { return false };
                p += 1;
                v |= u64::from(sb & 0x7f) << (7 * k);
                if sb & 0x80 == 0 {
                    ok = true;
                    break;
                }
            }
            if !ok {
                return false;
            }
            v as usize
        } else {
            au.len() - p // size-less OBU extends to the end of the AU
        };
        if obu_type == 1 {
            return true; // sequence header rides key frames only
        }
        if matches!(obu_type, 3 | 6) {
            // frame_header: show_existing_frame f(1); if 0, frame_type f(2)
            // (KEY_FRAME = 0). Top 3 bits of the first payload byte.
            if let Some(&fb) = au.get(p) {
                if fb & 0xe0 == 0 {
                    return true;
                }
            }
        }
        i = p.saturating_add(size);
        if size == 0 && obu_type != 2 {
            return false; // only a temporal delimiter is legitimately empty
        }
    }
    false
}

/// ffmpeg input args for the chosen decoder, retargeted to `codec`. HEVC uses
/// the probed decoder verbatim; the other codecs map the probed HW engine to
/// their sibling decoder (`hevc_cuvid`→`av1_cuvid`/`h264_cuvid`, …) when one
/// exists on that engine; codec-agnostic hwaccels (`-hwaccel vaapi/drm`) pass
/// through (ffmpeg falls back to software by itself when the accel lacks the
/// codec); anything without a known HW sibling drops `-c:v` so ffmpeg picks its
/// default (software) decoder for that codec. VVC hardware decode barely exists
/// (Intel Lunar Lake `vvc_qsv` only) — most boxes land on ffmpeg's native
/// software VVC decoder.
pub fn input_args_for(codec: StreamCodec) -> Vec<String> {
    retarget(codec, &input_args())
}

/// Retarget the probed HEVC decoder's ffmpeg args to `codec`
/// (see [`input_args_for`]).
fn retarget(codec: StreamCodec, base: &[String]) -> Vec<String> {
    // Known hardware sibling decoders per codec (ffmpeg `-decoders` names).
    let (sub, hw): (&str, &[&str]) = match codec {
        StreamCodec::Hevc => return base.to_vec(),
        StreamCodec::Av1 => ("av1", &["av1_cuvid", "av1_qsv", "av1_rkmpp"]),
        StreamCodec::H264 => (
            "h264",
            &["h264_cuvid", "h264_qsv", "h264_rkmpp", "h264_v4l2m2m"],
        ),
        StreamCodec::Vvc => ("vvc", &["vvc_qsv"]),
    };
    let mut out = Vec::with_capacity(base.len());
    let mut i = 0;
    while i < base.len() {
        if base[i] == "-c:v" && i + 1 < base.len() {
            let sibling = base[i + 1].replace("hevc", sub);
            if hw.contains(&sibling.as_str()) {
                out.push("-c:v".to_string());
                out.push(sibling);
            }
            // else: omit `-c:v` → ffmpeg picks its default decoder for `codec`.
            i += 2;
        } else {
            out.push(base[i].clone());
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn sniff_hevc_annexb() {
        assert_eq!(
            sniff_codec(&[0, 0, 0, 1, 0x40, 0x01]),
            Some(StreamCodec::Hevc)
        );
        assert_eq!(sniff_codec(&[0, 0, 1, 0x40, 0x01]), Some(StreamCodec::Hevc));
    }

    #[test]
    fn sniff_av1_obu() {
        // Temporal-delimiter OBU (type 2, has_size=1) = 0x12 — present on
        // EVERY NVENC temporal unit, so AV1 classifies mid-GOP too.
        assert_eq!(sniff_codec(&[0x12, 0x00]), Some(StreamCodec::Av1));
        // Sequence-header OBU (type 1) = 0x0a.
        assert_eq!(sniff_codec(&[0x0a, 0x0e]), Some(StreamCodec::Av1));
    }

    #[test]
    fn sniff_mid_gop_is_unclassified() {
        // The live 2026-07-14 black-screen bytes: an H.264 non-IDR slice
        // (0x61 = ref_idc 3, type 1) from a mid-stream join. Must be None —
        // guessing HEVC here rebuilt the wrong decoder on a live AVC stream.
        assert_eq!(sniff_codec(&[0, 0, 0, 1, 0x61, 0xe0, 0x82, 0xbf]), None);
        // HEVC TRAIL_R P-slice (type 1 → 0x02 0x01): also not classifiable.
        assert_eq!(sniff_codec(&[0, 0, 1, 0x02, 0x01, 0xd0]), None);
        // HEVC TRAIL_N / VVC TRAIL share 0x00 0x01 exactly — never guess.
        assert_eq!(sniff_codec(&[0, 0, 1, 0x00, 0x01, 0xd0]), None);
        // Truncated AU (start code + one header byte): not classifiable.
        assert_eq!(sniff_codec(&[0, 0, 0, 1, 0x40]), None);
        // Garbage with no start code and no plausible OBU header.
        assert_eq!(sniff_codec(&[0xff, 0xfe]), None);
        assert_eq!(sniff_codec(&[]), None);
    }

    #[test]
    fn keyframe_hevc_sample_is_irap() {
        // The bundled probe sample opens VPS/SPS/PPS + IDR — a key AU.
        assert!(is_keyframe_au(StreamCodec::Hevc, SAMPLE));
    }

    #[test]
    fn keyframe_hevc_p_slice_is_not() {
        // TRAIL_R slice (type 1): header byte = 1 << 1 = 0x02.
        assert!(!is_keyframe_au(
            StreamCodec::Hevc,
            &[0, 0, 0, 1, 0x02, 0x01, 0xd0]
        ));
        // Parameter sets alone (VPS 32 = 0x40) don't make a key AU either.
        assert!(!is_keyframe_au(
            StreamCodec::Hevc,
            &[0, 0, 0, 1, 0x40, 0x01]
        ));
    }

    #[test]
    fn keyframe_hevc_idr_after_params() {
        // VPS then IDR_W_RADL (19): header byte = 19 << 1 = 0x26.
        assert!(is_keyframe_au(
            StreamCodec::Hevc,
            &[0, 0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x26, 0x01]
        ));
        // CRA (21) counts too: 21 << 1 = 0x2a.
        assert!(is_keyframe_au(StreamCodec::Hevc, &[0, 0, 1, 0x2a, 0x01]));
    }

    #[test]
    fn keyframe_av1_sequence_header() {
        // Temporal delimiter (type 2, has_size, size 0) then sequence header
        // (type 1, has_size): 0x12 0x00, 0x0a <len> …
        assert!(is_keyframe_au(
            StreamCodec::Av1,
            &[0x12, 0x00, 0x0a, 0x02, 0x00, 0x00]
        ));
    }

    #[test]
    fn keyframe_av1_key_frame_header() {
        // Frame OBU (type 6, has_size): first payload byte 0b000….. =
        // show_existing_frame=0, frame_type=KEY.
        assert!(is_keyframe_au(StreamCodec::Av1, &[0x32, 0x02, 0x10, 0x00]));
        // Inter frame (frame_type=1 → bits 001…): not a key AU.
        assert!(!is_keyframe_au(StreamCodec::Av1, &[0x32, 0x02, 0x30, 0x00]));
    }

    #[test]
    fn parse_roundtrip() {
        assert_eq!(StreamCodec::parse("AV1"), Some(StreamCodec::Av1));
        assert_eq!(StreamCodec::parse("hevc"), Some(StreamCodec::Hevc));
        assert_eq!(StreamCodec::parse("h264"), Some(StreamCodec::H264));
        assert_eq!(StreamCodec::parse("AVC"), Some(StreamCodec::H264));
        assert_eq!(StreamCodec::parse("vvc"), Some(StreamCodec::Vvc));
        assert_eq!(StreamCodec::parse("H.266"), Some(StreamCodec::Vvc));
        assert_eq!(StreamCodec::parse("vp9"), None);
    }

    #[test]
    fn sniff_h264_annexb() {
        // NVENC H.264 opens with SPS (0x67, High profile 0x64 follows).
        assert_eq!(
            sniff_codec(&[0, 0, 0, 1, 0x67, 0x64]),
            Some(StreamCodec::H264)
        );
        // AUD-first (0x09) also reads as H.264.
        assert_eq!(sniff_codec(&[0, 0, 1, 0x09, 0xF0]), Some(StreamCodec::H264));
        // IDR-first (0x65) — the on-demand IDR a joiner requested.
        assert_eq!(sniff_codec(&[0, 0, 1, 0x65, 0x88]), Some(StreamCodec::H264));
    }

    #[test]
    fn sniff_vvc_annexb() {
        // vvenc + aud=insert opens with AUD (type 20): 0x00 0xA1.
        assert_eq!(
            sniff_codec(&[0, 0, 0, 1, 0x00, 0xA1]),
            Some(StreamCodec::Vvc)
        );
        // SPS-first (type 15): 0x00 0x79.
        assert_eq!(sniff_codec(&[0, 0, 1, 0x00, 0x79]), Some(StreamCodec::Vvc));
        // IDR_W_RADL-first (type 7): 0x00 0x39.
        assert_eq!(sniff_codec(&[0, 0, 1, 0x00, 0x39]), Some(StreamCodec::Vvc));
    }

    #[test]
    fn sniff_hevc_not_confused_by_new_codecs() {
        // HEVC IDR_W_RADL (type 19 → 0x26 0x01).
        assert_eq!(sniff_codec(&[0, 0, 1, 0x26, 0x01]), Some(StreamCodec::Hevc));
        // HEVC CRA (type 21 → 0x2A 0x01).
        assert_eq!(sniff_codec(&[0, 0, 1, 0x2A, 0x01]), Some(StreamCodec::Hevc));
    }

    #[test]
    fn keyframe_h264_idr() {
        // SPS + PPS + IDR = key AU.
        assert!(is_keyframe_au(
            StreamCodec::H264,
            &[0, 0, 0, 1, 0x67, 0x64, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x65, 0x88]
        ));
        // Non-IDR slice (type 1) is not.
        assert!(!is_keyframe_au(
            StreamCodec::H264,
            &[0, 0, 0, 1, 0x41, 0x9A]
        ));
        // Parameter sets alone don't make a key AU.
        assert!(!is_keyframe_au(
            StreamCodec::H264,
            &[0, 0, 0, 1, 0x67, 0x64]
        ));
    }

    #[test]
    fn keyframe_vvc_irap() {
        // AUD + SPS + IDR_W_RADL (type 7 → 0x00 0x39) = key AU.
        assert!(is_keyframe_au(
            StreamCodec::Vvc,
            &[0, 0, 1, 0x00, 0xA1, 0x50, 0, 0, 1, 0x00, 0x79, 0x02, 0, 0, 1, 0x00, 0x39, 0x01]
        ));
        // CRA (type 9 → 0x00 0x49) counts too.
        assert!(is_keyframe_au(
            StreamCodec::Vvc,
            &[0, 0, 0, 1, 0x00, 0x49, 0x01]
        ));
        // TRAIL (type 0 → 0x00 0x01) does not.
        assert!(!is_keyframe_au(
            StreamCodec::Vvc,
            &[0, 0, 0, 1, 0x00, 0x01, 0xD0]
        ));
    }

    #[test]
    fn retarget_maps_known_hw_and_passes_hwaccel() {
        // NVDEC: hevc_cuvid → av1_cuvid, low_delay flag preserved.
        let nvdec = vec![
            "-flags".into(),
            "low_delay".into(),
            "-c:v".into(),
            "hevc_cuvid".into(),
        ];
        assert_eq!(
            retarget(StreamCodec::Av1, &nvdec),
            vec!["-flags", "low_delay", "-c:v", "av1_cuvid"]
        );
        // hwaccel passes through unchanged (vaapi/drm decode AV1 too).
        let vaapi = vec!["-hwaccel".into(), "vaapi".into()];
        assert_eq!(
            retarget(StreamCodec::Av1, &vaapi),
            vec!["-hwaccel", "vaapi"]
        );
        // Software (no -c:v) stays empty → ffmpeg's default AV1 decoder.
        assert!(retarget(StreamCodec::Av1, &[]).is_empty());
        // A HW HEVC decoder with no AV1 sibling drops -c:v (software fallback).
        let amf = vec!["-c:v".into(), "hevc_amf".into()];
        assert!(retarget(StreamCodec::Av1, &amf).is_empty());
    }

    #[test]
    fn retarget_h264_and_vvc() {
        let nvdec = vec!["-c:v".into(), "hevc_cuvid".into()];
        // Every probed HW engine has an H.264 sibling.
        assert_eq!(
            retarget(StreamCodec::H264, &nvdec),
            vec!["-c:v", "h264_cuvid"]
        );
        let pi4 = vec!["-c:v".into(), "hevc_v4l2m2m".into()];
        assert_eq!(
            retarget(StreamCodec::H264, &pi4),
            vec!["-c:v", "h264_v4l2m2m"]
        );
        // VVC: only vvc_qsv exists in hardware; NVDEC drops to software.
        assert!(retarget(StreamCodec::Vvc, &nvdec).is_empty());
        let qsv = vec!["-c:v".into(), "hevc_qsv".into()];
        assert_eq!(retarget(StreamCodec::Vvc, &qsv), vec!["-c:v", "vvc_qsv"]);
        // HEVC passes through verbatim.
        assert_eq!(
            retarget(StreamCodec::Hevc, &nvdec),
            vec!["-c:v", "hevc_cuvid"]
        );
    }
}

// --- detection --------------------------------------------------------------

fn detect(ffmpeg: &OsStr) -> Capabilities {
    if let Some(ov) = std::env::var_os("TRIREME_DECODER") {
        return detect_override(ffmpeg, &ov.to_string_lossy());
    }
    let decoders = ffmpeg_word_set(ffmpeg, "-decoders");
    let hwaccels = ffmpeg_word_set(ffmpeg, "-hwaccels");

    let mut methods: Vec<Capability> = CATALOGUE
        .iter()
        .map(|c| {
            let compiled_in = match c.kind {
                Kind::Software => true,
                Kind::Decoder => decoders.iter().any(|w| w == c.token),
                Kind::Hwaccel => hwaccels.iter().any(|w| w == c.token),
            };
            // Test-decode every compiled-in HW candidate (software is a given).
            let (usable, detail) = if c.kind == Kind::Software {
                (true, "always available".to_string())
            } else if !compiled_in {
                (false, "not built into this ffmpeg".to_string())
            } else if test_decode(ffmpeg, &c.input_args()) {
                (true, "test-decode ok".to_string())
            } else {
                (
                    false,
                    "built in but no working device (test-decode failed)".to_string(),
                )
            };
            Capability {
                id: c.id.to_string(),
                label: c.label.to_string(),
                kind: c.kind,
                input_args: c.input_args(),
                compiled_in,
                usable,
                detail,
            }
        })
        .collect();

    let (selected, reason) = choose(&methods);
    // Annotate the chosen row if it was a listing-only safety-net pick.
    if reason.starts_with("WARNING") {
        methods[selected].detail = reason.clone();
    }
    Capabilities {
        methods,
        selected,
        selected_reason: reason,
    }
}

/// Honour `TRIREME_DECODER`. Accepts a catalogue id (`rkmpp`), an ffmpeg token
/// (`hevc_rkmpp`), `software`/`none`, or any other string (treated as a literal
/// `-c:v <value>` decoder). No test-decode — the user is trusted.
fn detect_override(ffmpeg: &OsStr, raw: &str) -> Capabilities {
    let val = raw.trim();
    let decoders = ffmpeg_word_set(ffmpeg, "-decoders");
    let hwaccels = ffmpeg_word_set(ffmpeg, "-hwaccels");

    // Build the normal listing (no test-decode) so the UI still has context.
    let mut methods: Vec<Capability> = CATALOGUE
        .iter()
        .map(|c| {
            let compiled_in = match c.kind {
                Kind::Software => true,
                Kind::Decoder => decoders.iter().any(|w| w == c.token),
                Kind::Hwaccel => hwaccels.iter().any(|w| w == c.token),
            };
            Capability {
                id: c.id.to_string(),
                label: c.label.to_string(),
                kind: c.kind,
                input_args: c.input_args(),
                compiled_in,
                usable: compiled_in,
                detail: if compiled_in {
                    "compiled in (not test-decoded; override set)".into()
                } else {
                    "not built into this ffmpeg".into()
                },
            }
        })
        .collect();

    let lc = val.to_ascii_lowercase();
    // Match a catalogue id or token.
    if let Some(i) = CATALOGUE.iter().position(|c| c.id == lc || c.token == val) {
        methods[i].detail = "forced via TRIREME_DECODER".into();
        return Capabilities {
            methods,
            selected: i,
            selected_reason: format!("forced via TRIREME_DECODER={val}"),
        };
    }
    if lc == "none" || lc == "sw" {
        let i = CATALOGUE
            .iter()
            .position(|c| c.kind == Kind::Software)
            .unwrap();
        methods[i].detail = "forced via TRIREME_DECODER".into();
        return Capabilities {
            methods,
            selected: i,
            selected_reason: format!("forced via TRIREME_DECODER={val}"),
        };
    }
    // Anything else: a literal decoder name we don't have in the catalogue.
    methods.push(Capability {
        id: "custom".into(),
        label: format!("custom (-c:v {val})"),
        kind: Kind::Decoder,
        input_args: vec!["-c:v".into(), val.into()],
        compiled_in: decoders.iter().any(|w| w == val),
        usable: true,
        detail: "forced via TRIREME_DECODER (literal decoder name)".into(),
    });
    let i = methods.len() - 1;
    Capabilities {
        methods,
        selected: i,
        selected_reason: format!("forced via TRIREME_DECODER={val} (literal)"),
    }
}

/// Pure selection policy (no I/O — unit-tested). Prefer the highest-priority
/// test-confirmed HW decoder; else fall back to the highest-priority compiled-in
/// HW decoder with a warning (never silently to software while HW exists); else
/// software.
fn choose(methods: &[Capability]) -> (usize, String) {
    if let Some(i) = methods
        .iter()
        .position(|m| m.kind != Kind::Software && m.usable)
    {
        return (i, "test-decode confirmed".to_string());
    }
    if let Some(i) = methods
        .iter()
        .position(|m| m.kind != Kind::Software && m.compiled_in)
    {
        return (
            i,
            "WARNING: no HW decoder passed the test-decode; selected on listing \
             only — if playback is slow/black, set TRIREME_DECODER to override"
                .to_string(),
        );
    }
    let i = methods
        .iter()
        .position(|m| m.kind == Kind::Software)
        .unwrap_or(methods.len() - 1);
    (
        i,
        "no hardware decoder available — using software".to_string(),
    )
}

// --- ffmpeg plumbing --------------------------------------------------------

/// Run `ffmpeg <flag>` (e.g. `-decoders`) and collect its stdout as a set of
/// whitespace words, for membership tests against decoder/hwaccel names.
fn ffmpeg_word_set(ffmpeg: &OsStr, flag: &str) -> Vec<String> {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", flag])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
        Err(e) => {
            tracing::warn!("ffmpeg {flag} probe failed ({e}); assuming software-only");
            Vec::new()
        }
    }
}

/// Up to two attempts (NVDEC init is transiently flaky under GPU contention).
fn test_decode(ffmpeg: &OsStr, input_args: &[String]) -> bool {
    for _ in 0..2 {
        if test_decode_once(ffmpeg, input_args, Duration::from_secs(4)) {
            return true;
        }
    }
    false
}

/// Feed the embedded sample through `ffmpeg <input_args> -f hevc -i pipe:0
/// -frames:v 1 -f null -` and report whether it exited cleanly within `timeout`.
fn test_decode_once(ffmpeg: &OsStr, input_args: &[String], timeout: Duration) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    cmd.args(input_args);
    cmd.args([
        "-f",
        "hevc",
        "-i",
        "pipe:0",
        "-frames:v",
        "1",
        "-f",
        "null",
        "-",
    ]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    // The sample (~5 KiB) fits the pipe buffer, so this won't block; then EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(SAMPLE);
        // stdin drops here → EOF.
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return false,
        }
    }
}

fn log_capabilities(caps: &Capabilities) {
    tracing::info!("HEVC decoder probe — {} candidate(s):", caps.methods.len());
    for (i, m) in caps.methods.iter().enumerate() {
        let mark = if !m.compiled_in {
            "–"
        } else if m.usable {
            "✓"
        } else {
            "✗"
        };
        let sel = if i == caps.selected {
            "  <= selected"
        } else {
            ""
        };
        tracing::info!("  [{mark}] {:8} {} — {}{sel}", m.id, m.label, m.detail);
    }
    let chosen = caps.chosen();
    if caps.selected_reason.starts_with("WARNING") {
        tracing::warn!(
            "HEVC decode: using '{}' ({}) — {}",
            chosen.id,
            chosen.label,
            caps.selected_reason
        );
    } else {
        tracing::info!(
            "HEVC decode: using '{}' ({}) — {}",
            chosen.id,
            chosen.label,
            caps.selected_reason
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(id: &str, kind: Kind, compiled_in: bool, usable: bool) -> Capability {
        let c = CATALOGUE.iter().find(|c| c.id == id).unwrap();
        Capability {
            id: id.into(),
            label: c.label.into(),
            kind,
            input_args: c.input_args(),
            compiled_in,
            usable,
            detail: String::new(),
        }
    }

    #[test]
    fn input_args_per_kind() {
        // cuvid is in LOW_DELAY_SAFE, so its args are prefixed with -flags low_delay.
        let decoder = CATALOGUE.iter().find(|c| c.id == "cuvid").unwrap();
        assert_eq!(
            decoder.input_args(),
            vec!["-flags", "low_delay", "-c:v", "hevc_cuvid"]
        );
        // A decoder NOT in the safe-set gets the bare selector (no low_delay).
        let qsv = CATALOGUE.iter().find(|c| c.id == "qsv").unwrap();
        assert_eq!(qsv.input_args(), vec!["-c:v", "hevc_qsv"]);
        let hwaccel = CATALOGUE.iter().find(|c| c.id == "drm").unwrap();
        assert_eq!(hwaccel.input_args(), vec!["-hwaccel", "drm"]);
        let sw = CATALOGUE.iter().find(|c| c.id == "software").unwrap();
        assert!(sw.input_args().is_empty());
    }

    #[test]
    fn catalogue_ends_in_software() {
        // The fallback must always be last so `choose` can find it.
        assert_eq!(CATALOGUE.last().unwrap().kind, Kind::Software);
        // ids are unique.
        for (i, a) in CATALOGUE.iter().enumerate() {
            for b in &CATALOGUE[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate id {}", a.id);
            }
        }
    }

    #[test]
    fn choose_prefers_confirmed_hw_in_priority_order() {
        // cuvid usable beats qsv usable (priority).
        let methods = vec![
            cap("cuvid", Kind::Decoder, true, true),
            cap("qsv", Kind::Decoder, true, true),
            cap("software", Kind::Software, true, true),
        ];
        let (i, why) = choose(&methods);
        assert_eq!(methods[i].id, "cuvid");
        assert_eq!(why, "test-decode confirmed");
    }

    #[test]
    fn choose_skips_unusable_hw() {
        // qsv compiled-but-broken, vaapi works → vaapi.
        let methods = vec![
            cap("qsv", Kind::Decoder, true, false),
            cap("vaapi", Kind::Hwaccel, true, true),
            cap("software", Kind::Software, true, true),
        ];
        let (i, _) = choose(&methods);
        assert_eq!(methods[i].id, "vaapi");
    }

    #[test]
    fn choose_safety_net_when_all_hw_tests_fail() {
        // cuvid compiled-in but test transiently failed; no other HW works.
        // Must pick cuvid (listing) with a warning, NOT silently software.
        let methods = vec![
            cap("cuvid", Kind::Decoder, true, false),
            cap("software", Kind::Software, true, true),
        ];
        let (i, why) = choose(&methods);
        assert_eq!(methods[i].id, "cuvid");
        assert!(why.starts_with("WARNING"), "got: {why}");
    }

    #[test]
    fn choose_software_when_no_hw_compiled_in() {
        let methods = vec![
            cap("cuvid", Kind::Decoder, false, false),
            cap("software", Kind::Software, true, true),
        ];
        let (i, why) = choose(&methods);
        assert_eq!(methods[i].id, "software");
        assert!(why.contains("software"));
    }
}
