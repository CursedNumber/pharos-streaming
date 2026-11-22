// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Frame capture → HEVC. M1 step 2 (first cut).
//!
//! The compositor renders its output into a CPU buffer (software/pixman path)
//! and we hand the frames to `ffmpeg`'s `hevc_nvenc` encoder over a pipe. This
//! proves the render → encode chain end to end on the GPU's NVENC block. The
//! zero-copy GLES → DMA-BUF → NVENC path is the follow-up optimization.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

/// An HEVC encoder backed by a child `ffmpeg` process using `hevc_nvenc`.
/// Frames are written as tightly-packed BGRA (matching pixman `Argb8888`).
pub struct HevcEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl HevcEncoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        pix_fmt: &str,
        out: &str,
    ) -> anyhow::Result<Self> {
        let size = format!("{width}x{height}");
        let fps = fps.to_string();
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                // raw input: packed BGRA frames on stdin
                "-f",
                "rawvideo",
                "-pixel_format",
                pix_fmt,
                "-video_size",
                &size,
                "-framerate",
                &fps,
                "-i",
                "pipe:0",
                // NVENC HEVC, low-latency tuning
                "-c:v",
                "hevc_nvenc",
                "-preset",
                "p4",
                "-tune",
                "ll",
                "-pix_fmt",
                "yuv420p",
                out,
            ])
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn ffmpeg (is it on PATH?): {e}"))?;
        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    /// Submit one tightly-packed BGRA frame (`width * height * 4` bytes).
    pub fn submit(&mut self, frame_bgra: &[u8]) -> anyhow::Result<()> {
        self.stdin
            .as_mut()
            .expect("encoder already finished")
            .write_all(frame_bgra)?;
        Ok(())
    }

    /// Close the pipe and wait for ffmpeg to flush and exit.
    pub fn finish(mut self) -> anyhow::Result<()> {
        self.stdin.take(); // drop stdin → EOF → ffmpeg flushes
        let status = self.child.wait()?;
        anyhow::ensure!(status.success(), "ffmpeg exited with status {status}");
        Ok(())
    }
}
