// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Client-side pairing driver — opens a QUIC pair link to Pharos, runs the
//! SPAKE2 handshake from `core-crypto`, and reports the host's pinned
//! pubkey on success.
//!
//! Used by both the GUI Pair modal (`menu`) and the headless CLI
//! (`trireme pair`). The actual crypto and the wire shape live in
//! `core-crypto` and `transport::pair_proto` so this file just orchestrates.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use core_crypto::{Identity, PairingRole, PairingSession, PAIRING_PROTOCOL_VERSION};
use transport::pair_proto::PairMsg;

/// What we learned about the host after a successful pairing.
#[derive(Debug, Clone)]
pub struct PairOutcome {
    pub host_pubkey_hex: String,
    pub host_alias: Option<String>,
}

/// Run the handshake to completion. Blocking — spins up a small tokio
/// runtime, so it's safe to call from sync code (the menu's "Pair" button).
pub fn run_pair(
    addr: SocketAddr,
    pin: &str,
    our_identity: &Identity,
    our_alias: Option<String>,
) -> Result<PairOutcome> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for pair")?;
    rt.block_on(run_pair_async(addr, pin, our_identity, our_alias))
}

async fn run_pair_async(
    addr: SocketAddr,
    pin: &str,
    our_identity: &Identity,
    our_alias: Option<String>,
) -> Result<PairOutcome> {
    let mut link = transport::pair_connect(&[addr])
        .await
        .with_context(|| format!("connect pair {addr}"))?;

    // Step 1: send HelloPair.
    link.send_msg(&PairMsg::HelloPair {
        version: PAIRING_PROTOCOL_VERSION,
        client_pubkey: our_identity.public_bytes(),
        alias: our_alias,
    })
    .await?;

    // Step 2: receive HelloAck.
    let ack = link.recv_msg().await?;
    let (host_pub, spake2_msg_b, host_alias) = match ack {
        PairMsg::HelloAck {
            host_pubkey,
            spake2_msg_b,
            host_alias,
        } => (host_pubkey, spake2_msg_b, host_alias),
        PairMsg::Abort { reason } => anyhow::bail!("host aborted: {reason}"),
        other => anyhow::bail!("expected HelloAck, got {other:?}"),
    };

    // Step 3: build our SPAKE2 message, send it.
    let mut session = PairingSession::new(
        PairingRole::Client,
        pin.to_string(),
        our_identity.public_bytes(),
    );
    session.set_peer_pubkey(host_pub);
    let spake2_msg_a = session
        .start()
        .map_err(|e| anyhow::anyhow!("spake start: {e}"))?;
    link.send_msg(&PairMsg::Spake2Reply { spake2_msg_a })
        .await?;

    // Compute the shared secret using the host's message.
    session
        .finish(&spake2_msg_b)
        .map_err(|e| anyhow::anyhow!("spake finish: {e}"))?;

    // Step 4: send ClientConfirm.
    let client_mac = session
        .our_mac()
        .map_err(|e| anyhow::anyhow!("derive client mac: {e}"))?;
    link.send_msg(&PairMsg::ClientConfirm { mac: client_mac })
        .await?;

    // Step 5: receive HostConfirm + verify.
    let confirm = link.recv_msg().await?;
    let host_mac = match confirm {
        PairMsg::HostConfirm { mac } => mac,
        PairMsg::Abort { reason } => anyhow::bail!("host aborted: {reason}"),
        other => anyhow::bail!("expected HostConfirm, got {other:?}"),
    };
    session
        .verify_peer_mac(&host_mac)
        .map_err(|e| anyhow::anyhow!("host confirm verify: {e}"))?;

    // Optional close — give the peer a moment to receive ClientConfirm
    // before we tear the connection down on the client side.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), link.close()).await;

    Ok(PairOutcome {
        host_pubkey_hex: hex::encode(host_pub),
        host_alias,
    })
}
