// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Live pipeline: QUIC recv → ffmpeg HW decode (probed: NVDEC/Rockchip MPP/V4L2)
//! → GL atomic scanout.
//!
//! Three threads. T1 runs the shared `transport` video client and writes each
//! HEVC access unit to ffmpeg's stdin. T2 reads decoded NV12 frames off ffmpeg's
//! stdout into a single "latest frame" slot (drop-old: a slow display never
//! backs up the decoder). The main thread runs the GL presenter's vblank-locked
//! flip loop, uploading the newest frame's Y/CbCr planes and drawing (YUV→RGB in
//! the shader) each vblank, re-presenting the last frame when none is newer.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::gl_present::GlPresenter;
use crate::kms::{Display, ModePref};

/// Optional `trireme-kms.toml`. Probed (in order) at `$TRIREME_KMS_CONFIG`,
/// `./trireme-kms.toml`, then `/etc/trireme-kms.toml`; defaults if none is found.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    /// Show the idle badge between streams; `false` = plain black.
    idle_badge: bool,
    /// Hex-encoded 32-byte Ed25519 pubkey of the Pharos host. When set, the
    /// connect refuses any host that doesn't sign for this key (mirrors the
    /// desktop client's per-server `host_pubkey`). Empty/missing ⇒ TOFU.
    host_pubkey: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_badge: true,
            host_pubkey: None,
        }
    }
}

/// Parse the hex pubkey (if any) into a 32-byte array. Logs and falls back
/// to `None` on malformed input — pinning would silently break otherwise.
fn pin_from_cfg(cfg: &Config) -> Option<[u8; 32]> {
    let hex_s = cfg.host_pubkey.as_deref().filter(|s| !s.is_empty())?;
    match hex::decode(hex_s.trim()) {
        Ok(b) => match <[u8; 32]>::try_from(b.as_slice()) {
            Ok(arr) => Some(arr),
            Err(_) => {
                tracing::warn!("host_pubkey must be 32 bytes (64 hex chars); ignoring");
                None
            }
        },
        Err(e) => {
            tracing::warn!("host_pubkey hex decode failed: {e}; ignoring");
            None
        }
    }
}

fn load_config() -> Config {
    let mut paths: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("TRIREME_KMS_CONFIG") {
        paths.push(p);
    }
    paths.push("trireme-kms.toml".into());
    paths.push("/etc/trireme-kms.toml".into());
    for p in &paths {
        if let Ok(text) = std::fs::read_to_string(p) {
            match toml::from_str::<Config>(&text) {
                Ok(cfg) => {
                    tracing::info!("loaded config {p}: {cfg:?}");
                    return cfg;
                }
                Err(e) => tracing::warn!("parse {p}: {e}; using defaults"),
            }
        }
    }
    Config::default()
}

/// The most recent decoded frame, plus a generation counter so the presenter
/// can tell "new frame since last vblank" from "same frame".
#[derive(Default)]
struct LatestFrame {
    data: Vec<u8>,
    gen: u64,
    done: bool,
}

struct FrameSlot {
    inner: Mutex<LatestFrame>,
}

/// The running decoder: the frame slot it fills and the feeder thread. The
/// feeder owns the ffmpeg child + reader thread (spawned lazily on first frame).
struct Decoder {
    slot: Arc<FrameSlot>,
    feeder: JoinHandle<()>,
}

/// Connect to Pharos and, on the first received access unit, spawn the ffmpeg
/// decoder and start feeding decoded NV12 frames into a drop-old slot (NV12 = `w*h` Y +
/// `w*h/2` interleaved CbCr). Spawning ffmpeg lazily means a failed connect (no
/// host) costs nothing — no decoder churn while the resident client idles.
fn start_decode(
    addr: SocketAddr,
    w: u32,
    h: u32,
    expected_pubkey: Option<[u8; 32]>,
    client_identity: Arc<core_crypto::Identity>,
) -> Result<Decoder> {
    let frame_bytes = (w * h + w * h / 2) as usize;
    let slot = Arc::new(FrameSlot {
        inner: Mutex::new(LatestFrame::default()),
    });

    let feed_slot = slot.clone();
    let feeder = std::thread::Builder::new()
        .name("quic-feed".into())
        .spawn(move || {
            let mut pipe: Option<(Child, std::process::ChildStdin, JoinHandle<()>)> = None;
            let res = transport::run_video_client(
                addr,
                expected_pubkey,
                Some(client_identity.as_ref()),
                |au| {
                    if pipe.is_none() {
                        // Codec: a TRIREME_CODEC override wins; otherwise sniff
                        // THIS first AU (NAL-header / OBU discrimination) so the
                        // resident client follows whatever codec the host streams
                        // (HEVC/AV1/AVC/VVC) with no configuration. The lazy
                        // spawn-on-first-AU already gives the sniff its bytes.
                        let codec = media_decode::codec_override_from_env()
                            .unwrap_or_else(|| media_decode::sniff_codec(&au));
                        tracing::info!("stream codec: {} — spawning decoder", codec.label());
                        match spawn_decoder(w, h, codec) {
                            Ok(mut child) => {
                                let stdin = child.stdin.take().expect("ffmpeg stdin");
                                let stdout = child.stdout.take().expect("ffmpeg stdout");
                                let reader = spawn_reader(stdout, feed_slot.clone(), frame_bytes);
                                pipe = Some((child, stdin, reader));
                            }
                            Err(e) => tracing::error!("spawn ffmpeg: {e}"),
                        }
                    }
                    if let Some((_, stdin, _)) = pipe.as_mut() {
                        if let Err(e) = stdin.write_all(&au) {
                            tracing::error!("ffmpeg stdin write failed: {e}");
                        }
                    }
                },
                None, // res_pref: the KMS client has no resolution selector
                None, // bitrate_pref: ditto
                None, // fps_pref: ditto
            );
            if let Err(e) = res {
                tracing::debug!("video client ended: {e:?}");
            }
            match pipe {
                Some((mut child, stdin, reader)) => {
                    drop(stdin); // EOF so ffmpeg flushes and exits
                    let _ = child.wait();
                    let _ = reader.join();
                }
                None => {
                    // Never connected — mark done so the presenter stops waiting.
                    let mut g = feed_slot.inner.lock().unwrap();
                    g.done = true;
                }
            }
        })?;

    Ok(Decoder { slot, feeder })
}

/// Read fixed-size NV12 frames off ffmpeg's stdout into the drop-old slot.
fn spawn_reader(
    mut stdout: std::process::ChildStdout,
    slot: Arc<FrameSlot>,
    frame_bytes: usize,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("nvdec-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; frame_bytes];
            let mut count = 0u64;
            loop {
                match stdout.read_exact(&mut buf) {
                    Ok(()) => {
                        count += 1;
                        let mut g = slot.inner.lock().unwrap();
                        g.data.clear();
                        g.data.extend_from_slice(&buf);
                        g.gen += 1;
                    }
                    Err(_) => {
                        let mut g = slot.inner.lock().unwrap();
                        g.done = true;
                        break;
                    }
                }
            }
            tracing::info!("decoder produced {count} frames");
        })
        .expect("spawn nvdec-read thread")
}

/// Live play: a resident client. Holds the screen **black** while idle, streams
/// at the connector's full refresh while Pharos is connected, and reconnects when
/// a stream ends — so the output is never a stale decoded frame or the leftover
/// console fb. Runs until killed.
/// Where the client identity lives: `$TRIREME_KMS_IDENTITY`, else
/// `~/.config/trireme-kms/identity.toml`, else a CWD-relative fallback.
fn identity_path() -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Ok(p) = std::env::var("TRIREME_KMS_IDENTITY") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config/trireme-kms/identity.toml");
    }
    PathBuf::from("trireme-kms-identity.toml")
}

pub fn run(addr: SocketAddr, card_path: &str, pref: Option<ModePref>) -> Result<()> {
    let cfg = load_config();
    let pin = pin_from_cfg(&cfg);
    if let Some(p) = pin.as_ref() {
        tracing::info!("pinning host pubkey {}", hex::encode(p));
    } else {
        tracing::info!("no host_pubkey pinned (TOFU)");
    }
    // Present our long-term identity on connect so an enforcing host authorizes
    // us. trireme-kms has no pairing UI, so log the pubkey: add it to the host's
    // peers.toml (or run Pharos with PHAROS_ALLOW_UNPAIRED=1).
    let id_path = identity_path();
    let identity = Arc::new(
        core_crypto::Identity::load_or_create(&id_path)
            .with_context(|| format!("identity at {}", id_path.display()))?,
    );
    tracing::info!(
        "trireme-kms identity {} — add to the host's peers.toml (or PHAROS_ALLOW_UNPAIRED=1)",
        identity.public_hex()
    );
    let mut presenter =
        GlPresenter::open(card_path, pref, cfg.idle_badge).context("open GL presenter")?;
    let (w, h) = (presenter.width(), presenter.height());
    tracing::info!(
        "GL scanout {}x{}@{}; idle until Pharos streams from {addr}",
        w,
        h,
        presenter.refresh()
    );

    loop {
        // Start a connect attempt (ffmpeg + QUIC client on a worker thread), and
        // animate the idle screen until it connects or the attempt fails.
        let dec = start_decode(addr, w, h, pin, identity.clone())?;
        if wait_connect_animating(&mut presenter, &dec) {
            tracing::info!("Pharos connected — streaming");
            if let Err(e) = stream(&mut presenter, &dec, w, h) {
                tracing::warn!("stream error: {e:?}");
            }
        }
        join_decoder(dec);
    }
}

/// Animate the idle screen (drifting logo) until the decoder delivers its first
/// frame (→ `true`, go stream) or the connection closes with none (→ `false`).
fn wait_connect_animating(presenter: &mut GlPresenter, dec: &Decoder) -> bool {
    loop {
        {
            let g = dec.slot.inner.lock().unwrap();
            if g.gen > 0 {
                return true;
            }
            if g.done {
                return false;
            }
        }
        if let Err(e) = presenter.show_idle() {
            tracing::warn!("show_idle: {e:?}");
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

/// Stream until the host finishes, presenting each frame. Returns the number of
/// unique frames shown.
fn stream(presenter: &mut GlPresenter, dec: &Decoder, w: u32, h: u32) -> Result<u64> {
    let present_slot = dec.slot.clone();
    let mut last_gen = 0u64;
    let mut uploaded = 0u64;
    let mut t0: Option<Instant> = None;
    let presented = presenter
        .present_loop(|r, _frame| {
            let g = present_slot.inner.lock().unwrap();
            if g.done && g.gen == last_gen {
                return false; // stream ended and nothing new to show
            }
            if g.gen != last_gen {
                last_gen = g.gen;
                r.upload_nv12(w as i32, h as i32, &g.data);
                uploaded += 1;
            }
            drop(g);
            r.draw();
            t0.get_or_insert_with(Instant::now);
            true
        })
        .unwrap_or(0);

    // Total frames the decoder actually delivered (gen counts every decoded
    // frame, including ones superseded before a vblank by a newer one).
    let decoded = dec.slot.inner.lock().unwrap().gen;
    if let Some(t0) = t0 {
        let secs = t0.elapsed().as_secs_f64();
        if secs > 0.0 {
            tracing::info!(
                "{:.1}s: {:.1} fps decode ({decoded} frames), {:.1} fps present ({presented} flips), {uploaded} unique shown ({} coalesced)",
                secs,
                decoded as f64 / secs,
                presented as f64 / secs,
                decoded.saturating_sub(uploaded),
            );
        }
    }

    Ok(uploaded)
}

fn join_decoder(dec: Decoder) {
    let _ = dec.feeder.join();
}

/// Hold the screen black (resident) until killed — parks the output in a clean
/// state. Useful to clear a leftover frame when nothing is streaming.
pub fn run_blank(card_path: &str, pref: Option<ModePref>) -> Result<()> {
    let p = GlPresenter::open(card_path, pref, false).context("open GL presenter")?;
    tracing::info!(
        "holding black on {}x{}@{}; Ctrl-C to exit",
        p.width(),
        p.height(),
        p.refresh()
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// GLES rate test: animated clear colour through the full GBM/EGL/atomic path,
/// proving the tiled-scanout path holds the connector's refresh (120 Hz).
pub fn run_gl_test(card_path: &str, pref: Option<ModePref>, frames: u64) -> Result<()> {
    let mut p = GlPresenter::open(card_path, pref, false).context("open GL presenter")?;
    tracing::info!(
        "GL test {}x{}@{} for {frames} frames",
        p.width(),
        p.height(),
        p.refresh()
    );
    let t0 = Instant::now();
    let mut last = Instant::now();
    let mut jitter_max = 0f64;
    let presented = p.present_loop(|r, frame| {
        if frame >= frames {
            return false;
        }
        let t = frame as f32 * 0.04;
        r.clear(
            0.5 + 0.5 * t.sin(),
            0.5 + 0.5 * (t * 1.7).sin(),
            0.5 + 0.5 * (t * 0.9).sin(),
        );
        if frame > 0 {
            jitter_max = jitter_max.max(last.elapsed().as_secs_f64() * 1000.0);
        }
        last = Instant::now();
        true
    })?;
    let secs = t0.elapsed().as_secs_f64();
    tracing::info!(
        "presented {presented} frames in {:.2}s = {:.1} fps (worst inter-flip {:.1} ms)",
        secs,
        presented as f64 / secs,
        jitter_max
    );
    Ok(())
}

/// Run a self-generated colour-bar/animation test pattern (no decode) to prove
/// the atomic scanout path at the target refresh.
pub fn run_test_pattern(card_path: &str, pref: Option<ModePref>, frames: u64) -> Result<()> {
    let mut display = Display::open(card_path, pref, 3).context("open DRM display")?;
    let (w, h) = (display.width(), display.height());
    let refresh = display.refresh();
    tracing::info!("test pattern {}x{}@{} for {frames} frames", w, h, refresh);

    // Precompute a double-width gradient strip once, then scroll a window of it
    // per frame via plain memcpy — same per-frame cost as the real decode path
    // (a few ms), so this measures the *scanout* rate, not a slow pixel loop.
    let row_bytes = w as usize * 4;
    let strip_w = w as usize * 2;
    let strip_row = strip_w * 4;
    let mut strip = vec![0u8; strip_row * h as usize];
    for y in 0..h as usize {
        for x in 0..strip_w {
            let i = y * strip_row + x * 4;
            strip[i] = ((x + y) & 0xff) as u8; // B  (XRGB8888 LE: B,G,R,X)
            strip[i + 1] = (y & 0xff) as u8; // G
            strip[i + 2] = (x & 0xff) as u8; // R
            strip[i + 3] = 0; // X
        }
    }

    let t0 = Instant::now();
    let mut last = Instant::now();
    let mut jitter_max = 0f64;
    let presented = display.run(|dst, pitch, frame| {
        if frame >= frames {
            return false;
        }
        let off = (frame as usize * 8) % w as usize; // scroll 8 px/frame
        for y in 0..h as usize {
            let s = y * strip_row + off * 4;
            let d = y * pitch as usize;
            dst[d..d + row_bytes].copy_from_slice(&strip[s..s + row_bytes]);
        }
        if frame > 0 {
            let dt = last.elapsed().as_secs_f64() * 1000.0;
            jitter_max = jitter_max.max(dt);
        }
        last = Instant::now();
        true
    })?;
    let secs = t0.elapsed().as_secs_f64();
    tracing::info!(
        "presented {presented} frames in {:.2}s = {:.1} fps (worst inter-flip {:.1} ms)",
        secs,
        presented as f64 / secs,
        jitter_max
    );
    Ok(())
}

fn spawn_decoder(w: u32, h: u32, codec: media_decode::StreamCodec) -> Result<Child> {
    // The HW decoder is chosen at runtime by `media_decode` (NVDEC `hevc_cuvid`,
    // Rockchip MPP `hevc_rkmpp`, V4L2 stateless `-hwaccel drm` on a Pi 5, …;
    // override with TRIREME_DECODER). Whatever wins, we force system-memory NV12
    // out (no -hwaccel_output_format), so there's no CPU swscale CSC — YUV→RGB
    // happens on the GPU in the shader. NV12 is also 1.5 B/px vs 4 for RGBA.
    // NB: do NOT add `-flags low_delay -fflags nobuffer` — on the nvidia driver
    // they make hevc_cuvid emit undecoded GREEN frames, and our stream has no
    // B-frames (ULTRA_LOW_LATENCY) so they'd buy nothing.
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "warning".into()];
    // Codec: sniffed from the stream's first AU by the caller (TRIREME_CODEC
    // overrides). `input_args_for` retargets the probed HW decoder to that
    // codec's sibling.
    let input_args = media_decode::input_args_for(codec);
    // `-flags low_delay -fflags nobuffer` make hevc_cuvid (NVIDIA) emit GREEN frames
    // (see the note above), so gate them to a non-cuvid decoder — the Pi's V4L2 drm
    // path is fine and needs them: without low-latency input, ffmpeg's default
    // probesize (5MB) / analyzeduration (5s) buffer ~seconds of input before the
    // first frame and never drain at real-time decode rate → a sustained ~1.5s lag.
    let is_cuvid = input_args.iter().any(|a| a.contains("cuvid"));
    args.extend(input_args);
    // Tiny probe is ample: the stream repeats SPS/PPS with every IDR (≤1s apart).
    args.extend(
        ["-probesize", "200000", "-analyzeduration", "200000"]
            .iter()
            .map(|s| s.to_string()),
    );
    if !is_cuvid {
        args.extend(
            ["-fflags", "nobuffer", "-flags", "low_delay"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    args.push("-f".into());
    args.push(codec.ffmpeg_format());
    args.extend(["-i", "pipe:0", "-vf"].iter().map(|s| s.to_string()));
    args.push(format!("scale={w}:{h}"));
    args.extend(
        ["-pix_fmt", "nv12", "-f", "rawvideo", "pipe:1"]
            .iter()
            .map(|s| s.to_string()),
    );
    let child = Command::new(media_decode::ffmpeg_program())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Null (not inherit): a resident client reconnects often, and a failed
        // connect makes ffmpeg exit noisily on empty input. Decode problems show
        // up as missing frames instead.
        .stderr(Stdio::null())
        .spawn()
        .context("spawn ffmpeg (is ffmpeg present with a working HEVC decoder?)")?;
    Ok(child)
}
