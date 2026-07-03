// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Capture + HEVC encode via an ffmpeg subprocess.
//!
//! Bring-up uses ffmpeg's `ddagrab` (DXGI Desktop Duplication, D3D11) to capture
//! a monitor by index and `hevc_qsv` / `hevc_nvenc` to encode it — the same
//! "ffmpeg owns the codec" idiom as the original Linux `capture.rs` and the
//! Trireme client's decoder. The exact filter/encoder flags are the most likely
//! thing to need tuning on real hardware, so `PHAROS_FFMPEG_CMD` can override
//! the whole argument list at runtime (`%IDX%` / `%FPS%` placeholders). Output
//! is a raw Annex-B HEVC stream on stdout, split into access units by
//! [`crate::au::AnnexBSplitter`].

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

/// Resolve the ffmpeg executable, checking the WinGet "Links" shim dir that
/// already-open shells don't have on PATH (mirrors Trireme's `ffmpeg_program`).
pub fn ffmpeg_program() -> std::ffi::OsString {
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

pub struct CaptureEncoder {
    child: Child,
    stdout: Option<ChildStdout>,
    stdin: Option<ChildStdin>,
    stderr: Option<ChildStderr>,
}

impl CaptureEncoder {
    /// ddagrab path: ffmpeg both captures (DXGI Desktop Duplication) and encodes.
    /// No stdin — the capture source lives inside ffmpeg.
    pub fn spawn(output_idx: u32, fps: u32, enc: Encoder) -> anyhow::Result<Self> {
        let args = build_args(output_idx, fps, enc);
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
            .map_err(|e| anyhow::anyhow!("spawn ffmpeg ({}): {e}", prog.to_string_lossy()))?;
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
    pub fn spawn_wgc(width: u32, height: u32, fps: u32, enc: Encoder) -> anyhow::Result<Self> {
        let args = build_rawvideo_args(width, height, fps, enc);
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
            .map_err(|e| anyhow::anyhow!("spawn ffmpeg ({}): {e}", prog.to_string_lossy()))?;
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

fn build_args(output_idx: u32, fps: u32, enc: Encoder) -> Vec<String> {
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
    let grab = format!("ddagrab=output_idx={output_idx}:framerate={fps_s}");

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
        "nv12".into(),
    ];
    a.extend(encoder_flags(enc, &gop));
    a.extend(annexb_output());
    a
}

/// ffmpeg args for the **WGC** path: encode a raw BGRA stream arriving on stdin
/// (the host captures via Windows.Graphics.Capture and paces frames at `fps`).
/// `PHAROS_FFMPEG_CMD` overrides everything, with `%W%`/`%H%`/`%FPS%` tokens.
fn build_rawvideo_args(width: u32, height: u32, fps: u32, enc: Encoder) -> Vec<String> {
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
        "nv12".into(),
    ];
    a.extend(encoder_flags(enc, &gop));
    a.extend(annexb_output());
    a
}

/// Encoder selection + low-latency tuning, shared by the ddagrab and WGC paths.
fn encoder_flags(enc: Encoder, gop: &str) -> Vec<String> {
    match enc {
        Encoder::Qsv => vec![
            "-c:v".into(),
            "hevc_qsv".into(),
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
            "hevc_nvenc".into(),
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
            "hevc_amf".into(),
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
/// emit a raw Annex-B HEVC elementary stream on stdout.
fn annexb_output() -> Vec<String> {
    vec![
        "-bsf:v".into(),
        "hevc_metadata=aud=insert".into(),
        "-f".into(),
        "hevc".into(),
        "pipe:1".into(),
    ]
}
