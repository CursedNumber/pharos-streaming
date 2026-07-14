// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Rig-only end-to-end audio check: real PipeWire capture → Opus encode → QUIC
//! datagram → Opus decode, asserting the decoded PCM actually carries signal.
//! Needs a live PipeWire/Pulse server + ffmpeg, so it is `#[ignore]`'d (CI has
//! no audio device). Run on the host:
//!   cargo test -p trireme --test audio_e2e -- --ignored

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
#[ignore]
fn audio_end_to_end_carries_tone() {
    let host_id = Arc::new(core_crypto::Identity::generate());

    // Bind the streamer to a free UDP port.
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

    let streamer = Arc::new(transport::QuicStreamer::start(addr, host_id).unwrap());

    // Host: real capture of the default sink monitor → Opus → datagrams, only
    // while a client is connected (mirrors the compositor's audio thread).
    {
        let h = streamer.audio_handle();
        std::thread::spawn(move || {
            let _ = media_audio::capture_opus(None, media_audio::DEFAULT_BITRATE, None, |pkt| {
                if h.is_connected() {
                    h.send(pkt);
                }
            });
        });
    }

    // Play a tone into the default sink so the monitor capture isn't silent.
    let mut tone = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=5",
            "-f",
            "pulse",
            "default",
        ])
        .spawn()
        .expect("play tone (needs ffmpeg + pulse)");

    // Client: decode each Opus datagram, accumulate frame count + signal energy.
    let frames = Arc::new(AtomicU64::new(0));
    let energy = Arc::new(AtomicU64::new(0));
    {
        let (frames, energy) = (frames.clone(), energy.clone());
        std::thread::spawn(move || {
            let mut dec = media_audio::OpusDecoder::new().unwrap();
            let on_audio: Box<dyn FnMut(Vec<u8>) + Send> = Box::new(move |pkt| {
                if let Ok(pcm) = dec.decode_frame(&pkt) {
                    assert_eq!(pcm.len(), media_audio::FRAME_INTERLEAVED);
                    let e: u64 = pcm.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
                    energy.fetch_add(e, Ordering::Relaxed);
                    frames.fetch_add(1, Ordering::Relaxed);
                }
            });
            let _ = transport::run_video_client_with_input(
                vec![addr],
                None,
                None,
                |_| {},
                None,
                Some(on_audio),
                None,
                None,
                None,
                None,
                None,
                false,
                false, // wants_hdr
                None,
                None,
                core_proto::StreamMode::default(),
                None,
                None,
                None,
                None, // transport_hint
                None, // on_clipboard
                None, // clipboard_rx
            );
        });
    }

    // Wait for real audio to flow through the whole chain.
    let mut ok = false;
    for _ in 0..500 {
        if frames.load(Ordering::Relaxed) > 10 && energy.load(Ordering::Relaxed) > 0 {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = tone.kill();
    let _ = tone.wait();
    assert!(
        ok,
        "expected decoded tone end-to-end; got {} frames, energy {}",
        frames.load(Ordering::Relaxed),
        energy.load(Ordering::Relaxed)
    );
}
