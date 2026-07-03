// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! `quic-recv [server_addr] [out_file] [max_mb]` — connect to a Pharos QUIC
//! streamer and save the received HEVC stream. For verifying the host transport.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let addr = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("127.0.0.1:4433")
        .parse()?;
    let out = args.get(2).map(String::as_str).unwrap_or("streamed.hevc");
    let max_mb = args
        .get(3)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(256);

    let data = transport::fetch_stream(addr, max_mb * 1024 * 1024)?;
    std::fs::write(out, &data)?;
    eprintln!("received {} bytes → {out}", data.len());
    Ok(())
}
