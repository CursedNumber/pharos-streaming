// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! End-to-end pairing enforcement: a real `peers.toml` gates real QUIC clients.
//! A client whose identity is listed is admitted; one that isn't is rejected.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn enforces_peers_toml_over_real_transport() {
    let host_id = Arc::new(core_crypto::Identity::generate());
    let client_a = core_crypto::Identity::generate(); // listed  → admitted
    let client_b = core_crypto::Identity::generate(); // unlisted → rejected

    // A peers.toml that lists only client A.
    let dir = std::env::temp_dir().join(format!("pharos-enforce-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let peers = dir.join("peers.toml");
    let mut f = std::fs::File::create(&peers).unwrap();
    writeln!(f, "[[peer]]\npubkey = \"{}\"", client_a.public_hex()).unwrap();
    drop(f);

    let authorizer = host_core::pair::peers_authorizer_at(peers.clone());

    // Free UDP port → streamer gated by that authorizer.
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();
    let streamer = Arc::new(
        transport::QuicStreamer::start_with_authorizer(
            addr,
            host_id,
            authorizer,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap(),
    );

    // Keep video flowing so an admitted client receives something.
    let stop = Arc::new(AtomicBool::new(false));
    let pusher = {
        let s = streamer.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                s.send(vec![1, 2, 3, 4]);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    // Client B (not in peers.toml) is rejected — run to completion first.
    let res_b = std::thread::spawn(move || {
        transport::run_video_client(addr, None, Some(&client_b), |_| {}, None, None, None)
    })
    .join()
    .unwrap();
    assert!(
        res_b.is_err(),
        "unpaired client must be rejected by peers.toml"
    );

    // Client A (in peers.toml) is admitted and receives a frame.
    let got = Arc::new(AtomicBool::new(false));
    {
        let got = got.clone();
        std::thread::spawn(move || {
            let _ = transport::run_video_client(
                addr,
                None,
                Some(&client_a),
                move |_| got.store(true, Ordering::Release),
                None,
                None,
                None,
            );
        });
    }
    let mut admitted = false;
    for _ in 0..300 {
        if got.load(Ordering::Acquire) {
            admitted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = pusher.join();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(admitted, "paired client (in peers.toml) must be admitted");
}
