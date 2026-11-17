// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Capture + video encode via an ffmpeg subprocess.
//!
//! Bring-up uses ffmpeg's `ddagrab` (DXGI Desktop Duplication, D3D11) to capture
//! a monitor by index and `hevc_qsv` / `hevc_nvenc` (or their `h264_*` siblings,
//! or software `libvvenc` for H.266) to encode it — the same "ffmpeg owns the
//! codec" idiom as the original Linux `capture.rs` and the Trireme client's
//! decoder. The exact filter/encoder flags are the most likely thing to need
//! tuning on real hardware, so `PHAROS_FFMPEG_CMD` can override the whole
//! argument list at runtime (`%IDX%` / `%FPS%` placeholders). Output is a raw
//! Annex-B stream on stdout, split into access units by
//! [`crate::au::AnnexBSplitter`].
//!
//! Codec selection is `PHAROS_CODEC` (`hevc` default | `h264`/`avc` | `vvc`) —
//! see [`WinCodec::from_env`]. AV1 is not offered here: the AU splitter cuts on
//! Annex-B AUDs, which AV1's OBU stream doesn't have.

use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

#[derive(Debug, Clone, Copy)]
pub enum Encoder {
    /// Intel Quick Sync (`hevc_qsv`) — the T14 bring-up target.
    Qsv,
    /// NVIDIA NVENC (`hevc_nvenc`) — the GTX 1080 rig (Pascal: I/P only, no AV1).
    Nvenc,
    /// AMD AMF (`hevc_amf`) — the RX 570 `.201` rig (Polaris/VCE 3.4: I/P only, no AV1).
    Amf,
}

/// The codec this Windows host encodes. HEVC is the default (unchanged
/// behavior); H.264/AVC is the universal-compatibility option (~+40% bitrate
/// for equal quality; encoder engine flags are shared with HEVC); VVC/H.266 is
/// experimental SOFTWARE encode via ffmpeg's libvvenc — no GPU encodes it, so
/// the engine selection is ignored and the CPU does the work (realtime only at
/// modest resolutions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinCodec {
    Hevc,
    H264,
    Vvc,
}

impl WinCodec {
    /// Parse `PHAROS_CODEC` (`hevc`/`h265` | `h264`/`avc` | `vvc`/`h266`).
    /// Unset → HEVC. An unrecognised value (incl. `av1` — the AU splitter can't
    /// cut OBU streams) is an error so a typo doesn't silently stream HEVC.
    pub fn from_env() -> anyhow::Result<Self> {
        let Ok(raw) = std::env::var("PHAROS_CODEC") else {
            return Ok(WinCodec::Hevc);
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "hevc" | "h265" => Ok(WinCodec::Hevc),
            "h264" | "avc" => Ok(WinCodec::H264),
            "vvc" | "h266" => Ok(WinCodec::Vvc),
            other => anyhow::bail!(
                "PHAROS_CODEC={other} not supported on the Windows host \
                 (hevc | h264/avc | vvc/h266)"
            ),
        }
    }

    /// The ffmpeg encoder token for this codec on `enc` (`hevc_qsv`,
    /// `h264_nvenc`, …). VVC is engine-independent (software libvvenc).
    pub fn token(self, enc: Encoder) -> String {
        let engine = match enc {
            Encoder::Qsv => "qsv",
            Encoder::Nvenc => "nvenc",
            Encoder::Amf => "amf",
        };
        match self {
            WinCodec::Hevc => format!("hevc_{engine}"),
            WinCodec::H264 => format!("h264_{engine}"),
            WinCodec::Vvc => "libvvenc".to_string(),
        }
    }

    /// The `crate::au` splitter flavour for this codec's Annex-B output.
    pub fn au_codec(self) -> crate::au::AuCodec {
        match self {
            WinCodec::Hevc => crate::au::AuCodec::Hevc,
            WinCodec::H264 => crate::au::AuCodec::H264,
            WinCodec::Vvc => crate::au::AuCodec::Vvc,
        }
    }

    /// Short uppercase codec abbreviation for status/display.
    pub fn label(self) -> &'static str {
        match self {
            WinCodec::Hevc => "HEVC",
            WinCodec::H264 => "AVC",
            WinCodec::Vvc => "VVC",
        }
    }

    /// The encoder pixel format. The HW encoders take NV12; libvvenc consumes
    /// 10-bit planar (ffmpeg does the CSC either way).
    fn pix_fmt(self) -> &'static str {
        match self {
            WinCodec::Hevc | WinCodec::H264 => "nv12",
            WinCodec::Vvc => "yuv420p10le",
        }
    }
}

/// Resolve the ffmpeg executable: an `ffmpeg.exe` placed next to
/// `pharos-host.exe` wins (no PATH edit needed for a manual install), then the
/// WinGet "Links" shim dir that already-open shells don't have on PATH
/// (mirrors Trireme's `ffmpeg_program`), then plain PATH lookup.
pub fn ffmpeg_program() -> std::ffi::OsString {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    {
        let cand = dir.join("ffmpeg.exe");
        if cand.exists() {
            return cand.into_os_string();
        }
    }
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
    std::ffi::OsString::from("ffmpeg")
}

/// What to tell the user when ffmpeg can't be found. The ddagrab/wgc capture
/// paths (and every Intel/AMD encode) run through it, and the installer does
/// not ship it, so this lands in the tray/panel fault reason verbatim.
pub const FFMPEG_MISSING: &str = "ffmpeg not found: install it with `winget install Gyan.FFmpeg` \
     (or put ffmpeg.exe next to pharos-host.exe), then restart Pharos";

/// Whether [`ffmpeg_program`] actually runs (`ffmpeg -version`).
pub fn ffmpeg_available() -> bool {
    Command::new(ffmpeg_program())
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|st| st.success())
}

/// Turn an ffmpeg spawn failure into an error, with the install hint when the
/// executable simply isn't there.
fn spawn_error(prog: &std::ffi::OsStr, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!("{FFMPEG_MISSING}")
    } else {
        anyhow::anyhow!("spawn ffmpeg ({}): {e}", prog.to_string_lossy())
    }
}

pub struct CaptureEncoder {
    child: Child,
    stdout: Option<ChildStdout>,
    stdin: Option<ChildStdin>,
    stderr: Option<ChildStderr>,
}

impl CaptureEncoder {
    /// ddagrab path: ffmpeg both captures (DXGI Desktop Duplication) and encodes.
    /// No stdin — the capture source lives inside ffmpeg.
    pub fn spawn(output_idx: u32, fps: u32, enc: Encoder, codec: WinCodec) -> anyhow::Result<Self> {
        let args = build_args(output_idx, fps, enc, codec);
        let prog = ffmpeg_program();
        tracing::info!("spawning {} {}", prog.to_string_lossy(), args.join(" "));
        let mut child = Command::new(&prog)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Pipe stderr so the P6 recovery ladder can scan it for device-loss/TDR
            // strings; a reader thread re-echoes every line so ffmpeg's own
            // diagnostics (stream mapping, encoder init, `frame= N`) stay visible.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_error(&prog, e))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            stdout,
            stdin: None,
            stderr,
        })
    }

    /// WGC path: ffmpeg encodes a raw BGRA stream the host pushes to its stdin
    /// (the host captures via Windows.Graphics.Capture and paces the frames). The
    /// `width`/`height` must match the BGRA frames written, or the encoder
    /// desyncs. Returns the encoder with both stdin (feed) and stdout (AUs) piped.
    pub fn spawn_wgc(
        width: u32,
        height: u32,
        fps: u32,
        enc: Encoder,
        codec: WinCodec,
    ) -> anyhow::Result<Self> {
        let args = build_rawvideo_args(width, height, fps, enc, codec);
        let prog = ffmpeg_program();
        tracing::info!("spawning {} {}", prog.to_string_lossy(), args.join(" "));
        let mut child = Command::new(&prog)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped (not inherited) so the recovery ladder can scan it; a reader
            // thread re-echoes every line to keep ffmpeg's diagnostics visible.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| spawn_error(&prog, e))?;
        let stdout = child.stdout.take();
        let stdin = child.stdin.take();
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            stdout,
            stdin,
            stderr,
        })
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// The ffmpeg stderr pipe — the P6 recovery ladder spawns a scanner thread on
    /// it (device-loss detection + line echo). `None` after the first take.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    /// The rawvideo input pipe (WGC path only; `None` for ddagrab).
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// Block until ffmpeg exits. Unused by the P6 supervisor (which reaps via
    /// `drop`), but kept as a natural handle API.
    #[allow(dead_code)]
    pub fn wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        Ok(self.child.wait()?)
    }
}

impl Drop for CaptureEncoder {
    fn drop(&mut self) {
        // Don't leave ffmpeg holding the capture device if we bail early.
        let _ = self.child.kill();
    }
}

fn build_args(output_idx: u32, fps: u32, enc: Encoder, codec: WinCodec) -> Vec<String> {
    // Live-tuning hook: a full override so the ffmpeg incantation can be iterated
    // on the test rig without a rebuild. Tokens are whitespace-split.
    if let Ok(tmpl) = std::env::var("PHAROS_FFMPEG_CMD") {
        return tmpl
            .split_whitespace()
            .map(|s| {
                s.replace("%IDX%", &output_idx.to_string())
                    .replace("%FPS%", &fps.to_string())
            })
            .collect();
    }

    let debug = std::env::var_os("PHAROS_FFMPEG_DEBUG").is_some();
    let loglevel = if debug { "info" } else { "error" };
    let fps_s = fps.to_string();
    let gop = fps.max(1).to_string();
    // Default: composite the Windows cursor into captured frames (TV/phone
    // clients render no local pointer, so the baked-in one is all they get).
    // PHAROS_DRAW_MOUSE=0 captures cursor-free for desktop-KVM use, where the
    // client draws its own instant pointer and the baked-in one lags and
    // doubles (issue #9).
    let draw_mouse = std::env::var("PHAROS_DRAW_MOUSE").map_or(true, |v| v.trim() != "0");
    let grab = format!(
        "ddagrab=output_idx={output_idx}:framerate={fps_s}:draw_mouse={}",
        u8::from(draw_mouse)
    );

    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        loglevel.into(),
        "-stats".into(),
        // ddagrab needs a D3D11 device for DXGI Desktop Duplication.
        "-init_hw_device".into(),
        "d3d11va".into(),
        "-filter_complex".into(),
        // D3D11 BGRA → system memory; `-pix_fmt nv12` does the CSC for the encoder.
        format!("{grab},hwdownload,format=bgra"),
        "-an".into(),
        "-pix_fmt".into(),
        codec.pix_fmt().into(),
    ];
    a.extend(encoder_flags(enc, codec, &gop, fps));
    a.extend(annexb_output(codec));
    a
}

/// ffmpeg args for the **WGC** path: encode a raw BGRA stream arriving on stdin
/// (the host captures via Windows.Graphics.Capture and paces frames at `fps`).
/// `PHAROS_FFMPEG_CMD` overrides everything, with `%W%`/`%H%`/`%FPS%` tokens.
fn build_rawvideo_args(
    width: u32,
    height: u32,
    fps: u32,
    enc: Encoder,
    codec: WinCodec,
) -> Vec<String> {
    if let Ok(tmpl) = std::env::var("PHAROS_FFMPEG_CMD") {
        return tmpl
            .split_whitespace()
            .map(|s| {
                s.replace("%W%", &width.to_string())
                    .replace("%H%", &height.to_string())
                    .replace("%FPS%", &fps.to_string())
            })
            .collect();
    }

    let debug = std::env::var_os("PHAROS_FFMPEG_DEBUG").is_some();
    let loglevel = if debug { "info" } else { "error" };
    let gop = fps.max(1).to_string();

    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        loglevel.into(),
        "-stats".into(),
        // Raw BGRA frames, exactly width*height*4 bytes each, paced at `fps` by
        // the host; `-framerate` sets the nominal rate so PTS track wall-clock.
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgra".into(),
        "-video_size".into(),
        format!("{width}x{height}"),
        "-framerate".into(),
        fps.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-an".into(),
        "-pix_fmt".into(),
        codec.pix_fmt().into(),
    ];
    a.extend(encoder_flags(enc, codec, &gop, fps));
    a.extend(annexb_output(codec));
    a
}

/// Encoder selection + low-latency tuning, shared by the ddagrab and WGC paths.
/// The HEVC and H.264 engine flags are identical (the `-c:v` token differs);
/// VVC ignores the engine and software-encodes on libvvenc.
fn encoder_flags(enc: Encoder, codec: WinCodec, gop: &str, fps: u32) -> Vec<String> {
    if codec == WinCodec::Vvc {
        // vvenc rejects an intra period that isn't a multiple of its GOP size
        // (32 for every current preset), so round the 1s cadence up.
        let idr = fps.max(1).div_ceil(32) * 32;
        let preset = std::env::var("PHAROS_VVENC_PRESET").unwrap_or_else(|_| "faster".to_string());
        let mut a = vec![
            "-c:v".into(),
            "libvvenc".into(),
            "-preset".into(),
            preset,
            "-g".into(),
            idr.to_string(),
        ];
        // Expert lever, passed verbatim (e.g. `gopsize=1:decodingrefreshtype=idr`).
        if let Ok(params) = std::env::var("PHAROS_VVENC_PARAMS") {
            if !params.trim().is_empty() {
                a.extend(["-vvenc-params".into(), params]);
            }
        }
        return a;
    }
    let token = codec.token(enc);
    match enc {
        Encoder::Qsv => vec![
            "-c:v".into(),
            token,
            "-preset".into(),
            "veryfast".into(),
            "-bf".into(),
            "0".into(),
            "-g".into(),
            gop.into(),
            "-low_power".into(),
            "0".into(),
        ],
        Encoder::Nvenc => vec![
            "-c:v".into(),
            token,
            "-preset".into(),
            "p4".into(),
            "-tune".into(),
            "ll".into(),
            "-bf".into(),
            "0".into(),
            "-g".into(),
            gop.into(),
            "-rc".into(),
            "cbr".into(),
        ],
        // AMD AMF (Polaris VCE 3.4): `ultralowlatency` usage picks a low-latency
        // rate control internally; `speed` quality for the frame budget. AMD HEVC
        // is I/P only (no B-frames), so no `-bf`.
        Encoder::Amf => vec![
            "-c:v".into(),
            token,
            "-usage".into(),
            "ultralowlatency".into(),
            "-quality".into(),
            "speed".into(),
            "-g".into(),
            gop.into(),
        ],
    }
}

/// Insert access-unit delimiters so the splitter can cut one AU per frame, then
/// emit a raw Annex-B elementary stream on stdout.
fn annexb_output(codec: WinCodec) -> Vec<String> {
    let (bsf, fmt) = match codec {
        WinCodec::Hevc => ("hevc_metadata=aud=insert", "hevc"),
        WinCodec::H264 => ("h264_metadata=aud=insert", "h264"),
        WinCodec::Vvc => ("vvc_metadata=aud=insert", "vvc"),
    };
    vec![
        "-bsf:v".into(),
        bsf.into(),
        "-f".into(),
        fmt.into(),
        "pipe:1".into(),
    ]
}
