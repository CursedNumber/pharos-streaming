// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Audio for Pharos/Trireme (BATTLE_PLAN §7): capture a PipeWire sink monitor
//! (Linux) / WASAPI loopback (Windows, M3) → **Opus**, low-latency, in the
//! shared core from M2 so host (encode) and desktop client (decode) use one
//! implementation.
//!
//! This slice provides the codec + Linux capture. Transport (an audio stream
//! alongside video) and client playback are the next slice. Android decodes
//! Opus via `MediaCodec`, so the libopus binding here is Linux-gated for now;
//! the Windows host's Opus path lands with M3.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackend {
    Pipewire,
    Wasapi,
}

pub fn default_backend() -> AudioBackend {
    if cfg!(windows) {
        AudioBackend::Wasapi
    } else {
        AudioBackend::Pipewire
    }
}

/// Opus is locked to 48 kHz stereo with 20 ms frames (960 samples/channel) —
/// the low-latency sweet spot and a valid Opus frame size. Both ends agree on
/// these so a packet is self-describing by position.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples *per channel* in one 20 ms frame.
pub const FRAME_SAMPLES: usize = 960;
/// Interleaved i16 samples in one frame (both channels).
pub const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS;
/// Default target bitrate — transparent enough for desktop + game audio.
pub const DEFAULT_BITRATE: i32 = 96_000;

#[cfg(any(target_os = "linux", target_os = "windows"))]
pub use codec::{OpusDecoder, OpusEncoder};

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod codec {
    use super::{CHANNELS, FRAME_INTERLEAVED, SAMPLE_RATE};
    use anyhow::{anyhow, Result};

    /// Opus encoder fixed to our 48 kHz / stereo / low-delay profile.
    pub struct OpusEncoder {
        enc: opus::Encoder,
    }

    impl OpusEncoder {
        pub fn new(bitrate: i32) -> Result<Self> {
            let mut enc = opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            )
            .map_err(|e| anyhow!("opus encoder: {e}"))?;
            enc.set_bitrate(opus::Bitrate::Bits(bitrate))
                .map_err(|e| anyhow!("opus bitrate: {e}"))?;
            Ok(Self { enc })
        }

        /// Encode one 20 ms frame (`FRAME_INTERLEAVED` interleaved i16) into an
        /// Opus packet.
        pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
            anyhow::ensure!(
                pcm.len() == FRAME_INTERLEAVED,
                "frame must be {FRAME_INTERLEAVED} interleaved i16, got {}",
                pcm.len()
            );
            // 4000 is comfortably above any single 20 ms stereo packet.
            self.enc
                .encode_vec(pcm, 4000)
                .map_err(|e| anyhow!("opus encode: {e}"))
        }
    }

    /// Opus decoder, the mirror of [`OpusEncoder`].
    pub struct OpusDecoder {
        dec: opus::Decoder,
    }

    impl OpusDecoder {
        pub fn new() -> Result<Self> {
            let dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                .map_err(|e| anyhow!("opus decoder: {e}"))?;
            Ok(Self { dec })
        }

        /// Decode one Opus packet into interleaved i16 (one frame).
        pub fn decode_frame(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
            let mut out = vec![0i16; FRAME_INTERLEAVED];
            let per_ch = self
                .dec
                .decode(packet, &mut out, false)
                .map_err(|e| anyhow!("opus decode: {e}"))?;
            out.truncate(per_ch * CHANNELS);
            Ok(out)
        }
    }
}

// ---- Linux capture: a PipeWire sink monitor via `parec` (raw s16le) --------

/// The default sink's monitor source — capturing it gives us the system mix
/// (everything playing on the host), the desktop/game audio we want to stream.
#[cfg(target_os = "linux")]
pub fn default_monitor() -> anyhow::Result<String> {
    use anyhow::Context;
    let out = std::process::Command::new("pactl")
        .arg("get-default-sink")
        .output()
        .context("run `pactl get-default-sink` (is PipeWire/Pulse running?)")?;
    anyhow::ensure!(out.status.success(), "pactl get-default-sink failed");
    let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(!sink.is_empty(), "no default sink");
    Ok(format!("{sink}.monitor"))
}

/// A per-session PipeWire **null sink**. Apps launched into the virtual display
/// are routed here (via `PULSE_SINK`), so the stream captures *their* audio off
/// this sink's monitor — not the host's desktop default sink. That fixes the
/// twin failure of capturing the default monitor: silent game audio (the game
/// was routed elsewhere) and a desktop leak (a YouTube tab on the default sink
/// reaching the stream). It also keeps a couch game off the host's speakers.
///
/// RAII: the module is unloaded on drop, so the session's sink disappears when
/// streaming ends.
#[cfg(target_os = "linux")]
pub struct NullSink {
    module_id: String,
    sink_name: String,
}

#[cfg(target_os = "linux")]
impl NullSink {
    /// Load `module-null-sink` named `name`, returning a guard that unloads it on
    /// drop. Any stale module of the same `sink_name` (left by a crashed prior
    /// run) is unloaded first so the name is free to reuse.
    pub fn create(name: &str) -> anyhow::Result<Self> {
        use anyhow::Context;
        unload_existing_null_sink(name);
        let out = std::process::Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                "sink_properties=device.description=Pharos-Stream",
            ])
            .output()
            .context("run `pactl load-module module-null-sink` (PipeWire/Pulse running?)")?;
        anyhow::ensure!(
            out.status.success(),
            "pactl load-module module-null-sink failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let module_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        anyhow::ensure!(!module_id.is_empty(), "pactl returned an empty module id");
        Ok(Self {
            module_id,
            sink_name: name.to_string(),
        })
    }

    /// The sink name apps should target via `PULSE_SINK`.
    pub fn sink_name(&self) -> &str {
        &self.sink_name
    }

    /// The monitor source to capture (what this sink is "playing").
    pub fn monitor(&self) -> String {
        format!("{}.monitor", self.sink_name)
    }
}

#[cfg(target_os = "linux")]
impl Drop for NullSink {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pactl")
            .args(["unload-module", &self.module_id])
            .output();
    }
}

/// Best-effort: unload any existing `module-null-sink` whose argument names
/// `sink_name=<name>` — a stale sink from a crashed run would block re-creating
/// it under the same name.
#[cfg(target_os = "linux")]
fn unload_existing_null_sink(name: &str) {
    let Ok(out) = std::process::Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
    else {
        return;
    };
    let needle = format!("sink_name={name}");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // `<id>\tmodule-null-sink\t<args>`
        let mut cols = line.split('\t');
        let (Some(id), Some(module), Some(args)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        if module == "module-null-sink" && args.contains(&needle) {
            let _ = std::process::Command::new("pactl")
                .args(["unload-module", id])
                .output();
        }
    }
}

/// Move every sink-input that belongs to an app rendering into the virtual
/// display (`WAYLAND_DISPLAY=<socket>` in its environment) onto `sink`. This
/// catches game audio that bypassed `PULSE_SINK` — most importantly Steam
/// titles, whose real game process is spawned by an already-running Steam and so
/// never inherits the launch env. Best-effort and idempotent: moving an input
/// already on `sink` is a harmless no-op. One pass; the caller loops it.
#[cfg(target_os = "linux")]
pub fn reconcile_virtual_display_audio(socket: &str, sink: &str) {
    let Ok(out) = std::process::Command::new("pactl")
        .args(["list", "sink-inputs"])
        .output()
    else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let want = format!("WAYLAND_DISPLAY={socket}");
    for block in text.split("Sink Input #").skip(1) {
        // The index is the first token of the block header.
        let Some(id) = block.split_whitespace().next() else {
            continue;
        };
        // `application.process.id = "1234"`
        let pid = block
            .lines()
            .find_map(|l| l.trim().strip_prefix("application.process.id = "))
            .map(|v| v.trim().trim_matches('"'));
        let Some(pid) = pid else { continue };
        if process_targets_display(pid, &want) {
            let _ = std::process::Command::new("pactl")
                .args(["move-sink-input", id, sink])
                .output();
        }
    }
}

/// Does process `pid` have `WAYLAND_DISPLAY=<socket>` in its environment? Reads
/// `/proc/<pid>/environ` (NUL-separated); any failure (gone, no perms) = false.
#[cfg(target_os = "linux")]
fn process_targets_display(pid: &str, want: &str) -> bool {
    let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    environ.split(|&b| b == 0).any(|e| e == want.as_bytes())
}

/// Capture system audio (a sink monitor) as 20 ms stereo frames, Opus-encode
/// each, and hand the packet to `on_packet`. Blocking; spawns `parec` for raw
/// s16le PCM. Stops after `max_frames` (None = until the stream ends/errors).
/// Returns the number of frames emitted.
///
/// `device` defaults to [`default_monitor`]. The subprocess capture mirrors the
/// video path's ffmpeg pattern; an in-process PipeWire client is a later
/// refinement.
#[cfg(target_os = "linux")]
pub fn capture_opus(
    device: Option<&str>,
    bitrate: i32,
    max_frames: Option<usize>,
    mut on_packet: impl FnMut(Vec<u8>),
) -> anyhow::Result<usize> {
    use anyhow::Context;
    use std::io::Read;

    let device = match device {
        Some(d) => d.to_string(),
        None => default_monitor()?,
    };
    // Capture with `parec`: targeting a sink's `.monitor` source is first-class
    // and reliable — parec binds to EXACTLY the device we name (verified: it lands
    // on `pharos_stream.monitor`, source idx matches).
    //
    // We previously tried `pw-record -P "{ stream.capture.sink=true … }"` to get a
    // prettier "sink capture" tray label, but `stream.capture.sink=true` makes
    // WirePlumber auto-route the capture to the DEFAULT sink's monitor and ignore
    // `--target` — so it silently captured the host's *desktop* (idle ⇒ silence on
    // the wire, plus the leak risk this null-sink design exists to prevent) instead
    // of the session sink. Correct capture beats a nicer label; reverted. The
    // descriptive name rides on `--client-name`/`--stream-name` instead.
    //
    // Output is raw interleaved s16le on stdout — the exact bytes the frame loop reads.
    //
    // `--latency-msec` is ESSENTIAL: without it parec uses PulseAudio's default record
    // buffer, which on pipewire-pulse is ~1.8s — the DOMINANT audio-lag source (measured:
    // beep→stdout 1841ms by default vs 49ms at 30ms). PHAROS_AUDIO_CAPTURE_MS tunes the
    // buffer (default 15ms; measured clean — 5–10ms also captured cleanly idle but leave
    // no headroom under load). `pactl`'s "Buffer Latency: 0" does NOT reflect this
    // fragment buffer — measure end-to-end, don't trust it.
    let cap_ms: u32 = std::env::var("PHAROS_AUDIO_CAPTURE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(15);
    let device_arg = format!("--device={device}");
    let latency_arg = format!("--latency-msec={cap_ms}");
    let mut child = std::process::Command::new("parec")
        .args([
            device_arg.as_str(),
            latency_arg.as_str(),
            "--format=s16le",
            "--rate=48000",
            "--channels=2",
            "--client-name=Pharos audio capture",
            "--stream-name=Pharos stream audio",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn `parec` (needs pipewire-pulse / pulseaudio)")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("parec stdout missing"))?;

    let mut enc = OpusEncoder::new(bitrate)?;
    let mut raw = vec![0u8; FRAME_INTERLEAVED * 2]; // i16 = 2 bytes
    let mut pcm = vec![0i16; FRAME_INTERLEAVED];
    let mut frames = 0usize;

    loop {
        if max_frames.is_some_and(|m| frames >= m) {
            break;
        }
        if stdout.read_exact(&mut raw).is_err() {
            break; // stream ended
        }
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = i16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]);
        }
        match enc.encode_frame(&pcm) {
            Ok(pkt) => on_packet(pkt),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        frames += 1;
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(frames)
}

// ---- Windows capture: WASAPI loopback of the default render endpoint ---------

/// Capture the system mix (everything playing on the default output device) via
/// **WASAPI loopback**, resample/convert to 48 kHz stereo i16, Opus-encode each
/// 20 ms frame, and hand the packet to `on_packet`. Blocking; runs on the calling
/// thread (the host spawns one). Stops after `max_frames` (None = until error).
/// `device` is ignored (always the default render endpoint, like the Linux monitor).
///
/// Loopback delivers data only while the endpoint is *active* (something playing);
/// on silence it yields nothing, so the stream naturally pauses and resumes — the
/// client treats a gap as silence. (A silent keep-alive render stream to force
/// continuous delivery is a later refinement.)
#[cfg(target_os = "windows")]
pub fn capture_opus(
    _device: Option<&str>,
    bitrate: i32,
    max_frames: Option<usize>,
    mut on_packet: impl FnMut(Vec<u8>),
) -> anyhow::Result<usize> {
    use std::slice;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // WASAPI's `AUDCLNT_BUFFERFLAGS_SILENT` = 0x2 — the buffer is silence and its
    // memory needn't be read (we synthesize zeros instead).
    const BUFFERFLAGS_SILENT: u32 = 0x2;
    #[inline]
    fn f32_to_i16(s: f32) -> i16 {
        (s.clamp(-1.0, 1.0) * 32767.0) as i16
    }

    unsafe {
        // WinRT/COM apartment on this thread, or interface calls fail.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| anyhow::anyhow!("MMDeviceEnumerator: {e}"))?;
        // Default *render* endpoint; loopback captures what it plays — the system mix.
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| anyhow::anyhow!("GetDefaultAudioEndpoint(render): {e}"))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| anyhow::anyhow!("Activate IAudioClient: {e}"))?;

        let pwfx = client
            .GetMixFormat()
            .map_err(|e| anyhow::anyhow!("GetMixFormat: {e}"))?;
        let wfx = *pwfx; // copy the WAVEFORMATEX header before Initialize/free
        let rate = wfx.nSamplesPerSec;
        let channels = wfx.nChannels as usize;
        let bits = wfx.wBitsPerSample as usize;
        anyhow::ensure!(channels >= 1, "WASAPI reported 0 channels");
        anyhow::ensure!(
            rate == SAMPLE_RATE,
            "WASAPI mix rate is {rate} Hz; Pharos needs {SAMPLE_RATE} Hz \
             (set the Windows output device sample rate to 48000)"
        );
        // Shared-mode mix is 32-bit float; 16 means PCM. (No subtype-GUID dance —
        // bit depth is a reliable enough discriminator for the mix format.)
        let is_float = bits == 32;
        anyhow::ensure!(
            is_float || bits == 16,
            "unsupported WASAPI bit depth {bits} (expected 32-bit float or 16-bit PCM)"
        );

        // 200 ms loopback buffer (units of 100 ns). Periodicity 0 = default.
        let init = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            200 * 10_000,
            0,
            pwfx,
            None,
        );
        CoTaskMemFree(Some(pwfx as *const _));
        init.map_err(|e| anyhow::anyhow!("IAudioClient::Initialize(loopback): {e}"))?;

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| anyhow::anyhow!("GetService IAudioCaptureClient: {e}"))?;
        client
            .Start()
            .map_err(|e| anyhow::anyhow!("IAudioClient::Start: {e}"))?;

        let mut enc = OpusEncoder::new(bitrate)?;
        let mut acc: Vec<i16> = Vec::with_capacity(FRAME_INTERLEAVED * 2);
        let mut frames = 0usize;

        let result = 'cap: loop {
            if max_frames.is_some_and(|m| frames >= m) {
                break 'cap Ok(frames);
            }
            match capture.GetNextPacketSize() {
                Ok(0) | Err(_) => {
                    // Idle endpoint delivers nothing; poll (loopback is change-driven).
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Ok(_) => {
                    let mut pdata: *mut u8 = std::ptr::null_mut();
                    let mut nframes: u32 = 0;
                    let mut flags: u32 = 0;
                    if let Err(e) =
                        capture.GetBuffer(&mut pdata, &mut nframes, &mut flags, None, None)
                    {
                        break 'cap Err(anyhow::anyhow!("GetBuffer: {e}"));
                    }
                    let n = nframes as usize;
                    if n > 0 {
                        if flags & BUFFERFLAGS_SILENT != 0 {
                            acc.resize(acc.len() + n * CHANNELS, 0i16);
                        } else if is_float {
                            let s = slice::from_raw_parts(pdata as *const f32, n * channels);
                            for f in 0..n {
                                let base = f * channels;
                                let l = s[base];
                                let r = if channels >= 2 { s[base + 1] } else { l };
                                acc.push(f32_to_i16(l));
                                acc.push(f32_to_i16(r));
                            }
                        } else {
                            let s = slice::from_raw_parts(pdata as *const i16, n * channels);
                            for f in 0..n {
                                let base = f * channels;
                                let l = s[base];
                                let r = if channels >= 2 { s[base + 1] } else { l };
                                acc.push(l);
                                acc.push(r);
                            }
                        }
                    }
                    let _ = capture.ReleaseBuffer(nframes);
                }
            }

            // Emit every complete 20 ms frame we've accumulated.
            while acc.len() >= FRAME_INTERLEAVED {
                let frame: Vec<i16> = acc.drain(..FRAME_INTERLEAVED).collect();
                match enc.encode_frame(&frame) {
                    Ok(pkt) => on_packet(pkt),
                    Err(e) => break 'cap Err(e),
                }
                frames += 1;
            }
        };

        let _ = client.Stop();
        result
    }
}

/// Capture isn't wired up on this platform (only Linux PipeWire + Windows WASAPI).
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_opus(
    _device: Option<&str>,
    _bitrate: i32,
    _max_frames: Option<usize>,
    _on_packet: impl FnMut(Vec<u8>),
) -> anyhow::Result<usize> {
    anyhow::bail!("audio capture not implemented on this platform")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// One 20 ms stereo frame of a sine wave, advancing `phase`.
    fn sine_frame(freq: f32, phase: &mut f32) -> Vec<i16> {
        let mut v = vec![0i16; FRAME_INTERLEAVED];
        let step = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE as f32;
        for s in 0..FRAME_SAMPLES {
            let val = (phase.sin() * 8000.0) as i16;
            v[s * 2] = val;
            v[s * 2 + 1] = val;
            *phase += step;
        }
        v
    }

    #[test]
    fn opus_round_trip_preserves_frame_and_carries_signal() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let mut phase = 0.0f32;
        let mut total_energy: u64 = 0;
        // Opus has encoder lookahead, so energy appears after a frame or two —
        // summing across several frames is the robust check.
        for _ in 0..15 {
            let pcm = sine_frame(440.0, &mut phase);
            let pkt = enc.encode_frame(&pcm).unwrap();
            assert!(
                !pkt.is_empty() && pkt.len() < 4000,
                "sane opus packet size: {}",
                pkt.len()
            );
            let out = dec.decode_frame(&pkt).unwrap();
            assert_eq!(out.len(), FRAME_INTERLEAVED, "decodes to one full frame");
            total_energy += out
                .iter()
                .map(|&s| (s as i64 * s as i64) as u64)
                .sum::<u64>();
        }
        assert!(
            total_energy > 0,
            "decoded audio must carry the tone, not silence"
        );
    }

    #[test]
    fn encode_rejects_wrong_frame_size() {
        let mut enc = OpusEncoder::new(DEFAULT_BITRATE).unwrap();
        assert!(enc.encode_frame(&[0i16; 100]).is_err());
    }

    /// Needs a live PipeWire/Pulse server, so it's ignored in CI. Run locally:
    /// `cargo test -p media-audio -- --ignored`.
    #[test]
    #[ignore]
    fn capture_smoke() {
        let n = capture_opus(None, DEFAULT_BITRATE, Some(25), |pkt| {
            assert!(!pkt.is_empty())
        })
        .expect("capture from the default monitor");
        assert_eq!(n, 25, "captured the requested number of frames");
    }
}
