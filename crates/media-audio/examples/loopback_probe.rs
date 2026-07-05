// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Manual loopback capture check: is real audio (not silence) reaching the
//! Opus encoder? Captures the default output's system mix for 500 frames
//! (~10 seconds of ACTIVE audio; silent gaps don't count on Windows, where
//! loopback is change-driven), decodes its own Opus packets back to PCM, and
//! prints per-second RMS and peak levels. Play something audible while it
//! runs; rms near 0 with the peak stuck at 0 means the encoder is being fed
//! silence and the capture path is broken.
//!
//!   cargo run -p media-audio --example loopback_probe

fn main() -> anyhow::Result<()> {
    let mut dec = media_audio::OpusDecoder::new()?;
    let mut frames = 0usize;
    let mut sum_sq = 0f64;
    let mut samples = 0usize;
    let mut peak = 0i32;
    let mut second = 0usize;
    eprintln!("capturing 500 Opus frames from the default output's loopback; play something…");
    let n = media_audio::capture_opus(None, media_audio::DEFAULT_BITRATE, Some(500), |pkt| {
        if let Ok(pcm) = dec.decode_frame(&pkt) {
            for &s in &pcm {
                let a = i32::from(s).abs();
                sum_sq += f64::from(a) * f64::from(a);
                peak = peak.max(a);
            }
            samples += pcm.len();
        }
        frames += 1;
        if frames.is_multiple_of(50) {
            second += 1;
            let rms = if samples > 0 {
                (sum_sq / samples as f64).sqrt()
            } else {
                0.0
            };
            println!("t={second:>2}s  rms={rms:>7.0}  peak={peak:>5}  ({samples} samples)");
            sum_sq = 0.0;
            samples = 0;
            peak = 0;
        }
    })?;
    println!("captured {n} Opus frames total");
    Ok(())
}
