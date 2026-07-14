// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Software video encode via an ffmpeg subprocess — the VVC (H.266) path.
//!
//! No consumer GPU has a VVC hardware encoder (NVENC tops out at AV1), so
//! `video_codec = "vvc"` routes the per-output pipeline here instead of NVENC:
//! the compositor's GL→CUDA blit still fills the per-output linear CUDA buffer,
//! then each frame is copied device→host and piped as rawvideo RGBA into
//! `ffmpeg -c:v libvvenc`, whose Annex-B output is split back into access units
//! and handed to the same QUIC streamer the NVENC path feeds. This is the same
//! "ffmpeg owns the codec" idiom as pharos-win and the original capture.rs.
//!
//! ## Latency contract (lesson-render-loop-no-block)
//!
//! The render loop must NEVER block on the encoder. Feeding is a single-slot
//! mailbox: `submit_from_cuda` only does the device→host copy when the slot is
//! empty (i.e. the encoder is ready for a frame) and returns immediately either
//! way. A slow encoder therefore drops frames at the mailbox — the stream's
//! effective fps degrades to what libvvenc sustains, and the render loop never
//! stalls. A detached writer thread moves mailbox frames into ffmpeg's stdin; a
//! reader thread splits stdout into AUs and queues them for the render loop to
//! drain (`drain_aus`) on its next tick.
//!
//! ## Known limitations vs the NVENC path (logged at spawn)
//!
//! * No on-demand IDR: an ffmpeg pipe has no mid-stream control channel, so
//!   `DG_REQUEST_IDR` waits for the next periodic IDR (`-g`, ≤ the configured
//!   IDR period) instead of resyncing in ~1 frame.
//! * No live reconfigure: the panel's live bitrate/resolution actuators no-op
//!   for a software session (a codec/bitrate change relaunches the host, which
//!   rebuilds this encoder with the new settings — same UX as a codec switch).
//! * SDR only: the readback is 8-bit RGBA; `PHAROS_HDR` stays HEVC-only.
//!
//! Tuning levers: `PHAROS_VVENC_PRESET` (`faster` default, `fast`, `medium`…)
//! and `PHAROS_VVENC_PARAMS` (passed verbatim as `-vvenc-params`, vvenc expert
//! options, e.g. `gopsize=1:decodingrefreshtype=idr` for a low-delay GOP).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};

/// One encoded access unit plus its "starts a decodable chain" flag (VVC IRAP),
/// which the transport's bounded send queue uses for its keyframe reserve.
pub struct EncodedAu {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// A software encoder session: one ffmpeg child per output, fed RGBA frames,
/// emitting Annex-B VVC access units.
pub struct SoftEncoder {
    child: Child,
    /// Single-slot frame mailbox (see module docs). `None` = writer is ready
    /// for the next frame.
    slot: Arc<(Mutex<Option<Vec<u8>>>, Condvar)>,
    /// Encoded AUs from the reader thread, drained by the render loop.
    au_rx: Receiver<EncodedAu>,
    /// Spent frame buffers returned by the writer thread for reuse, so the
    /// steady state does one device→host copy and zero allocations per frame.
    spare_rx: Receiver<Vec<u8>>,
    /// Set by the writer/reader threads when the child pipe breaks.
    dead: Arc<AtomicBool>,
    /// Rate-limits the "encoder died" reminder log.
    last_dead_log: std::time::Instant,
    frame_len: usize,
}

/// What the local ffmpeg must provide for the VVC path. Probed once at spawn so
/// a missing piece fails the pipeline build with an actionable message instead
/// of a silent black stream.
pub fn preflight() -> Result<(), String> {
    let need = [
        ("-encoders", "libvvenc", "the libvvenc (VVenC) encoder"),
        ("-muxers", "vvc", "the raw H.266/VVC muxer"),
        ("-bsfs", "vvc_metadata", "the vvc_metadata bitstream filter"),
    ];
    for (flag, token, what) in need {
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", flag])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("VVC needs ffmpeg on PATH, but running it failed: {e}"))?;
        let listed = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|w| w == token);
        if !listed {
            return Err(format!(
                "this ffmpeg lacks {what} (`ffmpeg {flag}` has no '{token}'); VVC host \
                 encode needs ffmpeg ≥ 7.1 built with --enable-libvvenc"
            ));
        }
    }
    Ok(())
}

impl SoftEncoder {
    /// Spawn the libvvenc child for a `width`×`height`@`fps` RGBA feed.
    /// `bitrate` is the resolved `(avg_kbps, max_kbps)` rate-control target, if
    /// configured; without one libvvenc runs its default (constant-QP-ish) RC.
    pub fn spawn(
        width: u32,
        height: u32,
        fps: u32,
        idr_period_frames: u32,
        bitrate: Option<(u32, u32)>,
    ) -> anyhow::Result<Self> {
        preflight().map_err(|e| anyhow::anyhow!(e))?;

        // vvenc rejects an intra period that isn't a multiple of its GOP size
        // (32 for every current preset), so round the configured cadence up.
        let idr = idr_period_frames.max(1).div_ceil(32) * 32;
        let preset = std::env::var("PHAROS_VVENC_PRESET").unwrap_or_else(|_| "faster".to_string());

        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            // Raw RGBA frames, one per mailbox hand-off. `-framerate` sets the
            // nominal rate; when libvvenc can't keep up the mailbox drops
            // frames, so the true cadence is encoder-paced.
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "rgba".into(),
            "-video_size".into(),
            format!("{width}x{height}"),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            "pipe:0".into(),
            "-an".into(),
            // libvvenc consumes 10-bit planar; ffmpeg does the RGBA→YUV CSC.
            "-pix_fmt".into(),
            "yuv420p10le".into(),
            "-c:v".into(),
            "libvvenc".into(),
            "-preset".into(),
            preset.clone(),
            "-g".into(),
            idr.to_string(),
        ];
        if let Some((avg, max)) = bitrate {
            args.extend(["-b:v".into(), format!("{avg}k")]);
            if max > avg {
                args.extend(["-maxrate".into(), format!("{max}k")]);
            }
        }
        if let Ok(params) = std::env::var("PHAROS_VVENC_PARAMS") {
            if !params.trim().is_empty() {
                args.extend(["-vvenc-params".into(), params]);
            }
        }
        // AUDs delimit access units so the splitter can cut one AU per send —
        // the same trick as pharos-win's `hevc_metadata=aud=insert`.
        args.extend([
            "-bsf:v".into(),
            "vvc_metadata=aud=insert".into(),
            "-f".into(),
            "vvc".into(),
            "pipe:1".into(),
        ]);

        tracing::info!(
            target: "compositor::swenc",
            "spawning software VVC encoder: ffmpeg {} (preset={preset}, idr={idr} frames); \
             on-demand IDR + live bitrate/resolution reconfigure are unavailable on this path",
            args.join(" ")
        );
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn ffmpeg (libvvenc): {e}"))?;

        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let slot: Arc<(Mutex<Option<Vec<u8>>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let (au_tx, au_rx) = std::sync::mpsc::channel::<EncodedAu>();
        let (spare_tx, spare_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        // Writer: mailbox → ffmpeg stdin. Exits when the encoder is dropped
        // (slot poisoned/dead flag) or the pipe breaks.
        {
            let slot = slot.clone();
            let dead = dead.clone();
            std::thread::Builder::new()
                .name("swenc-writer".into())
                .spawn(move || {
                    let (lock, cvar) = &*slot;
                    loop {
                        let frame = {
                            let mut guard = lock.lock().unwrap();
                            loop {
                                if dead.load(Ordering::Relaxed) {
                                    return;
                                }
                                if let Some(f) = guard.take() {
                                    break f;
                                }
                                let (g, _timeout) = cvar
                                    .wait_timeout(guard, std::time::Duration::from_millis(250))
                                    .unwrap();
                                guard = g;
                            }
                        };
                        if let Err(e) = stdin.write_all(&frame) {
                            tracing::error!(
                                target: "compositor::swenc",
                                "libvvenc stdin write failed ({e}); encoder is gone"
                            );
                            dead.store(true, Ordering::Relaxed);
                            return;
                        }
                        // Hand the buffer back for reuse; if the session is
                        // being dropped the send just fails, which is fine.
                        let _ = spare_tx.send(frame);
                    }
                })
                .map_err(|e| anyhow::anyhow!("spawn swenc-writer: {e}"))?;
        }

        // Reader: ffmpeg stdout → AU splitter → au_tx.
        {
            let dead = dead.clone();
            std::thread::Builder::new()
                .name("swenc-reader".into())
                .spawn(move || {
                    let mut rd = BufReader::with_capacity(1 << 16, stdout);
                    let mut splitter = VvcAuSplitter::new();
                    let mut chunk = [0u8; 1 << 16];
                    loop {
                        match rd.read(&mut chunk) {
                            Ok(0) | Err(_) => {
                                dead.store(true, Ordering::Relaxed);
                                return;
                            }
                            Ok(n) => {
                                for au in splitter.push(&chunk[..n]) {
                                    let keyframe = vvc_au_has_irap(&au);
                                    if au_tx.send(EncodedAu { data: au, keyframe }).is_err() {
                                        return; // encoder dropped
                                    }
                                }
                            }
                        }
                    }
                })
                .map_err(|e| anyhow::anyhow!("spawn swenc-reader: {e}"))?;
        }

        // Stderr: surface libvvenc/ffmpeg diagnostics in the journal.
        std::thread::Builder::new()
            .name("swenc-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    tracing::warn!(target: "compositor::swenc", "ffmpeg: {line}");
                }
            })
            .map_err(|e| anyhow::anyhow!("spawn swenc-stderr: {e}"))?;

        Ok(Self {
            child,
            slot,
            au_rx,
            spare_rx,
            dead,
            last_dead_log: std::time::Instant::now(),
            frame_len: (width as usize) * (height as usize) * 4,
        })
    }

    /// Offer the freshly-blitted frame in the per-output CUDA linear buffer to
    /// the encoder. Non-blocking: when the mailbox is still occupied (encoder
    /// busy) this returns immediately WITHOUT the device→host copy, so a slow
    /// encode costs the render loop nothing.
    ///
    /// # Safety
    /// `dptr` must be a live CUDA linear allocation of at least
    /// `width * height * 4` bytes on the context bound to this thread.
    pub unsafe fn submit_from_cuda(&mut self, dptr: u64) {
        if self.dead.load(Ordering::Relaxed) {
            if self.last_dead_log.elapsed() >= std::time::Duration::from_secs(5) {
                self.last_dead_log = std::time::Instant::now();
                tracing::error!(
                    target: "compositor::swenc",
                    "software VVC encoder is dead (see earlier ffmpeg errors); \
                     the stream is stalled — switch codecs in the panel or fix ffmpeg"
                );
            }
            return;
        }
        let (lock, cvar) = &*self.slot;
        {
            let guard = lock.lock().unwrap();
            if guard.is_some() {
                return; // encoder still chewing on the previous frame
            }
        }
        // Slot free: reuse a spent buffer if the writer returned one.
        let mut buf = self.spare_rx.try_recv().unwrap_or_default();
        buf.resize(self.frame_len, 0);
        // SAFETY: caller upholds dptr validity/size; buf is exactly frame_len.
        if let Err(e) = unsafe { cudarc::driver::result::memcpy_dtoh_sync(&mut buf, dptr) } {
            tracing::error!(target: "compositor::swenc", "cuMemcpyDtoH: {e:?}");
            return;
        }
        let mut guard = lock.lock().unwrap();
        *guard = Some(buf);
        cvar.notify_one();
    }

    /// Every access unit the encoder finished since the last call. Never blocks.
    pub fn drain_aus(&mut self) -> Vec<EncodedAu> {
        self.au_rx.try_iter().collect()
    }
}

impl Drop for SoftEncoder {
    fn drop(&mut self) {
        // Unblock + stop the writer, then reap ffmpeg so it can't outlive the
        // pipeline holding its feed.
        self.dead.store(true, Ordering::Relaxed);
        self.slot.1.notify_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Annex-B VVC byte logic (pure functions — unit-tested below)
// ---------------------------------------------------------------------------

/// Splits a raw Annex-B H.266 stream into access units, cutting at each
/// access-unit-delimiter NAL (AUD_NUT, type 20) — guaranteed present by
/// `vvc_metadata=aud=insert`. The VVC NAL header is two bytes:
/// `forbidden(1) reserved(1) layer_id(6)` then `nal_type(5) tid+1(3)`, so a
/// base-layer AUD after a start code reads `0x00 0xA1`.
struct VvcAuSplitter {
    buf: Vec<u8>,
}

impl VvcAuSplitter {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append freshly-read bytes and return any now-complete access units.
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let starts = vvc_aud_starts(&self.buf);
        let mut out = Vec::new();
        if starts.len() < 2 {
            return out; // 0 or 1 AUD seen → nothing provably complete yet
        }
        for w in starts.windows(2) {
            out.push(self.buf[w[0]..w[1]].to_vec());
        }
        let keep_from = *starts.last().unwrap();
        self.buf.drain(..keep_from);
        out
    }
}

/// Byte offsets at which an AUD NAL's start code begins (including the leading
/// `00` of a 4-byte start code).
fn vvc_aud_starts(buf: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let n = buf.len();
    let mut i = 0usize;
    while i + 4 < n {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            // Two-byte VVC NAL header: base layer (0x00), type from byte 2.
            if buf[i + 3] == 0x00 && (buf[i + 4] >> 3) == 20 {
                let start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
                starts.push(start);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
}

/// Whether a VVC access unit contains an IRAP VCL NAL (IDR_W_RADL 7,
/// IDR_N_LP 8, CRA 9) — i.e. can start a fresh decode. Mirrors the HEVC/AV1
/// keyframe classifiers in `media-decode`.
fn vvc_au_has_irap(au: &[u8]) -> bool {
    let n = au.len();
    let mut i = 0usize;
    while i + 4 < n {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0xC0 == 0 {
                let nal_type = au[i + 4] >> 3;
                if (7..=9).contains(&nal_type) {
                    return true;
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(type << 3) | tid+1` second header byte for a base-layer NAL.
    fn nal(t: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0x00, (t << 3) | 1];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn splitter_cuts_on_aud() {
        let mut s = VvcAuSplitter::new();
        let mut stream = Vec::new();
        stream.extend(nal(20, &[0x50])); // AUD
        stream.extend(nal(15, &[1, 2])); // SPS
        stream.extend(nal(7, &[3, 4])); // IDR slice
        stream.extend(nal(20, &[0x50])); // next AUD
        stream.extend(nal(0, &[5])); // TRAIL
        let aus = s.push(&stream);
        assert_eq!(aus.len(), 1, "one complete AU between the two AUDs");
        assert!(vvc_au_has_irap(&aus[0]));
        // The trailing partial AU stays buffered until the next AUD arrives.
        let aus2 = s.push(&nal(20, &[0x50]));
        assert_eq!(aus2.len(), 1);
        assert!(!vvc_au_has_irap(&aus2[0]));
    }

    #[test]
    fn splitter_handles_split_reads() {
        let mut s = VvcAuSplitter::new();
        let mut stream = Vec::new();
        stream.extend(nal(20, &[0x50]));
        stream.extend(nal(8, &[9, 9]));
        stream.extend(nal(20, &[0x50]));
        // Feed byte-by-byte: boundaries must survive arbitrary chunking.
        let mut got = Vec::new();
        for b in stream {
            got.extend(s.push(&[b]));
        }
        assert_eq!(got.len(), 1);
        assert!(vvc_au_has_irap(&got[0]));
    }

    #[test]
    fn irap_detection_by_type() {
        for t in [7u8, 8, 9] {
            assert!(vvc_au_has_irap(&nal(t, &[0])), "type {t} is IRAP");
        }
        for t in [0u8, 1, 10, 14, 15, 19, 20] {
            assert!(!vvc_au_has_irap(&nal(t, &[0])), "type {t} is not IRAP");
        }
    }

    #[test]
    fn aud_from_nonzero_layer_is_not_a_boundary() {
        // layer_id != 0 → first header byte != 0x00 → not our cut point.
        let mut s = VvcAuSplitter::new();
        let mut stream = vec![0, 0, 1, 0x01, (20 << 3) | 1, 0x50];
        stream.extend(nal(20, &[0x50]));
        stream.extend(nal(20, &[0x50]));
        let aus = s.push(&stream);
        assert_eq!(aus.len(), 1);
    }
}
