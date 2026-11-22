// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! `trireme` — the Linux reference client for the Pharos stream.
//!
//! This file is the client CLI entry point: it dispatches the `menu`, `servers`,
//! `pair`, `decode`, `play`, `usb-list`, and `wake` subcommands. Each streaming
//! path connects to the Pharos QUIC streamer, receives the encoded access units
//! over the shared Rust `transport`, and decodes them with ffmpeg using the best
//! HW decoder for the box (probed at runtime by `media_decode`: NVDEC
//! `hevc_cuvid` on the dev rig, Quick Sync / Rockchip MPP / V4L2 stateless
//! elsewhere). Windowed low-latency playback (winit + GPU present) and VRR are
//! shipped via `play`; the `decode` subcommand saves periodic PNG snapshots so
//! the decode stays verifiable end to end.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

mod discover;
mod gamepad;
mod input;
mod menu;
mod pair;
mod play;
mod servers;
mod usb;
mod vncsrc;
mod wol;

fn main() -> Result<()> {
    // RUST_LOG wins; otherwise default to `info` with wgpu muted. wgpu_core logs
    // `Device::maintain: waiting for submission index …` at INFO every frame, and
    // writing that to a terminal synchronously throttles the render loop — which
    // matters here because the player runs at 60–120 fps. wgpu_hal=error also hides
    // the benign "Unrecognized present mode" WARN newer Vulkan drivers advertise.
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "info,wgpu_core=warn,wgpu_hal=error,naga=warn",
            ))
            .init();
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // No subcommand → show the server selector (the first screen).
        None => run_menu_then_player(),
        // `trireme menu` — explicitly show the selector.
        Some("menu") => run_menu_then_player(),
        // `trireme servers [add|list|remove] ...` — registry from the CLI.
        Some("servers") => run_servers_cmd(&args[2..]),
        // `trireme pair <host:pair_port> <PIN>` — run the SPAKE2 handshake
        // headlessly; same code path as the menu's Pair button.
        Some("pair") => run_pair_cmd(&args[2..]),
        // `trireme decode <host:port> [out_dir]` — decode the live stream (HW
        // decoder probed at runtime), saving a PNG snapshot ~once a second.
        Some("decode") => {
            let addr: SocketAddr = match args.get(2) {
                Some(a) => a.parse().context("parse <host:port>")?,
                None => (std::net::Ipv4Addr::LOCALHOST, core_proto::ports::STREAM).into(),
            };
            let out = args
                .get(3)
                .map(String::as_str)
                .unwrap_or("/tmp/trireme-frames");
            // Optional fourth arg pins the host pubkey (hex). Useful for
            // headless verification that mismatched keys are rejected.
            let expected = args
                .get(4)
                .and_then(|h| hex::decode(h).ok())
                .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
            if args.get(4).is_some() && expected.is_none() {
                anyhow::bail!("pubkey arg must be 64 hex chars");
            }
            run_decode(addr, out, expected)
        }
        // Windowed player. Three forms:
        //   trireme play                       — load default config, else 1×1 fallback
        //   trireme play <host:port> [pace]     — legacy single-source
        //   trireme play <config.toml>          — explicit multi-source config
        Some("play") => match args.get(2).map(String::as_str) {
            None => {
                let cfg = play::load_default_config()?.unwrap_or_else(|| {
                    let addr: SocketAddr =
                        (std::net::Ipv4Addr::LOCALHOST, core_proto::ports::STREAM).into();
                    tracing::info!("no sources config found; default single-source 1×1 @ {addr}");
                    play::PlayerCfg::single(addr, play::PaceMode::LowLatency)
                });
                play::run(cfg)
            }
            Some(s) => {
                // SocketAddr ⇒ legacy single-source; anything else ⇒ config file path.
                if let Ok(addr) = s.parse::<SocketAddr>() {
                    let pace = args
                        .get(3)
                        .map(|p| play::PaceMode::parse(p))
                        .unwrap_or(play::PaceMode::LowLatency);
                    play::run_legacy(addr, pace)
                } else {
                    let cfg = play::load_config_path(Path::new(s))?;
                    play::run(cfg)
                }
            }
        },
        // `trireme usb-list` — enumerate USB devices to pick one to relay.
        Some("usb-list") => usb::list(),
        // `trireme wake <host|mac>` — Wake-on-LAN a configured host (opt-in).
        Some("wake") => run_wake_cmd(&args[2..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 trireme                              (server selector)\n  \
                 trireme menu                         (same)\n  \
                 trireme servers [list|add|remove ...]\n  \
                 trireme pair <host:pair_port> <PIN>\n  \
                 trireme decode <host:port> [out_dir]\n  \
                 trireme play\n  \
                 trireme play <host:port> [pace]\n  \
                 trireme play <config.toml>\n  \
                 trireme wake <host|mac>\n  \
                 trireme usb-list"
            );
            std::process::exit(2);
        }
    }
}

/// Show the server selector. When the user clicks Connect, build a
/// [`play::PlayerCfg`] from the selected entries and hand off to the player.
fn run_menu_then_player() -> Result<()> {
    let registry = servers::load().context("load servers registry")?;
    let id_path = servers::identity_path()?;
    let identity = std::sync::Arc::new(
        core_crypto::Identity::load_or_create(&id_path)
            .with_context(|| format!("identity at {}", id_path.display()))?,
    );
    tracing::info!("trireme identity {}", identity.public_hex());
    let (final_registry, outcome) = menu::run(registry, identity)?;
    // Persist any final state — the menu also writes incrementally, but a
    // last save covers the close-window-without-clicking case.
    let _ = servers::save(&final_registry);
    match outcome {
        menu::MenuOutcome::Quit => Ok(()),
        menu::MenuOutcome::Connect(entries) => {
            let cfg = build_player_cfg(&entries)?;
            play::run(cfg)
        }
    }
}

/// Pack the selected servers into a `PlayerCfg` with an auto-sized grid.
/// N=1 → 1×1, N=2 → 2×1, N=3..=4 → 2×2, N=5..=6 → 3×2, otherwise 3×ceil(N/3).
fn build_player_cfg(entries: &[servers::ServerEntry]) -> Result<play::PlayerCfg> {
    anyhow::ensure!(!entries.is_empty(), "no servers selected");
    let n = entries.len();
    let (cols, rows) = match n {
        1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        5 | 6 => (3, 2),
        _ => {
            let cols = 3u32;
            let rows = (n as u32).div_ceil(cols);
            (cols, rows)
        }
    };
    let sources: Vec<play::SourceCfg> = entries
        .iter()
        .map(|s| play::SourceCfg {
            name: Some(s.friendly_name()),
            addr: s.socket_addr().unwrap_or_else(|_| {
                // Last-ditch fallback so a stale entry still produces a
                // valid SocketAddr; the network thread will surface the
                // real error.
                "0.0.0.0:0".parse().unwrap()
            }),
            protocol: s.protocol.clone(),
            password: s.password.clone(),
            alt_addrs: s.alt_socket_addrs(),
            pace: s.pace.clone(),
            width: None,
            height: None,
            host_pubkey: s.host_pubkey.clone(),
        })
        .collect();
    Ok(play::PlayerCfg {
        present: Some("low-latency".into()),
        layout: play::LayoutCfg { cols, rows },
        sources,
        input: crate::input::InputCfg::default(),
        // Off by default (and always suppressed for a single cell). Opt in to the
        // active-cell border for a multi-source grid via `highlight_px` in sources.toml.
        highlight_px: 0,
        highlight_color: "#33FF66".to_string(),
    })
}

/// `trireme pair <host:pair_port> <PIN>` — run the SPAKE2 handshake against
/// a Pharos pair listener. On success, find the matching server entry by
/// host (or create a new one) and stash the host's pubkey + flip paired=true.
fn run_pair_cmd(args: &[String]) -> Result<()> {
    let addr_s = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: pair <host:pair_port> <PIN>"))?;
    let pin = args
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("usage: pair <host:pair_port> <PIN>"))?;
    let addr: SocketAddr = addr_s.parse().context("parse <host:pair_port>")?;
    let id_path = servers::identity_path()?;
    let identity = core_crypto::Identity::load_or_create(&id_path)
        .with_context(|| format!("identity at {}", id_path.display()))?;
    let alias = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty());
    tracing::info!("trireme identity {}", identity.public_hex());
    tracing::info!("pairing with {addr} (pin redacted)…");
    let outcome = pair::run_pair(addr, pin, &identity, alias)?;
    println!(
        "paired with host {}{}",
        outcome.host_pubkey_hex,
        outcome
            .host_alias
            .as_deref()
            .map(|a| format!(" alias={a}"))
            .unwrap_or_default()
    );

    // Update / add the matching server entry so the menu sees `paired`.
    let mut registry = servers::load()?;
    let host_str = addr.ip().to_string();
    let entry_id = registry
        .servers
        .iter()
        .find(|s| s.host == host_str)
        .map(|s| s.id.clone());
    let id = match entry_id {
        Some(id) => id,
        None => {
            // No matching server known — store the *pair_port* host as a new
            // entry; the user can fix up the video port later in the menu.
            registry.upsert(host_str.clone(), addr.port(), None)
        }
    };
    if let Some(s) = registry.find_mut(&id) {
        s.paired = true;
        s.host_pubkey = Some(outcome.host_pubkey_hex.clone());
        s.pair_port = Some(addr.port());
    }
    servers::save(&registry)?;
    println!("saved to {}", servers::servers_path()?.display());
    Ok(())
}

/// `trireme wake <host|mac>` — Wake-on-LAN, fully opt-in. If the argument is a
/// MAC, the magic packet goes straight to it; otherwise we look up a known
/// server by host/name and use its configured `mac`. Errors (rather than
/// guessing) if no MAC is known — Trireme never assumes the LAN topology.
fn run_wake_cmd(args: &[String]) -> Result<()> {
    let target = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: wake <host|mac>"))?;
    if wol::parse_mac(target).is_ok() {
        wol::wake(target)?;
        println!("Wake-on-LAN magic packet sent to {target}");
        return Ok(());
    }
    let reg = servers::load()?;
    let entry = reg
        .servers
        .iter()
        .find(|s| s.host == *target || s.friendly_name() == *target)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no server matching {target:?}, and it isn't a MAC — pass a MAC or add the server"
            )
        })?;
    let mac = entry.mac.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "server {:?} has no `mac` set — Wake-on-LAN is opt-in; add the host's MAC to its \
             entry in servers.toml (e.g. mac = \"AA:BB:CC:DD:EE:FF\")",
            entry.friendly_name()
        )
    })?;
    wol::wake(mac)?;
    println!(
        "Wake-on-LAN magic packet sent to {} ({mac})",
        entry.friendly_name()
    );
    Ok(())
}

/// `trireme servers [list|add|remove ...]` — CLI to the same registry the
/// menu uses, so the user can preseed entries from a shell.
fn run_servers_cmd(args: &[String]) -> Result<()> {
    let mut registry = servers::load()?;
    match args.first().map(String::as_str) {
        None | Some("list") => {
            if registry.servers.is_empty() {
                println!("(no servers — add one with `trireme servers add <host> <port> [alias]`)");
                return Ok(());
            }
            for s in &registry.servers {
                println!(
                    "{:24} {:22} [{}]{}",
                    s.friendly_name(),
                    s.address_label(),
                    if s.paired { "paired" } else { "unpaired" },
                    s.alias
                        .as_deref()
                        .filter(|a| !a.is_empty())
                        .map(|a| format!("  alias={a}"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        Some("add") => {
            let host = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: servers add <host> <port> [alias]"))?
                .clone();
            let port: u16 = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: servers add <host> <port> [alias]"))?
                .parse()
                .context("parse port")?;
            let alias = args.get(3).cloned();
            let id = registry.upsert(host.clone(), port, alias);
            servers::save(&registry)?;
            println!("added {id} → {host}:{port}");
            Ok(())
        }
        Some("remove") | Some("rm") => {
            let id = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: servers remove <id|host>"))?;
            // Accept either an id or a host substring — registry ids are
            // a sanitized form of host_port so this almost always matches.
            let target = registry
                .servers
                .iter()
                .find(|s| s.id == *id || s.host == *id)
                .map(|s| s.id.clone());
            match target {
                Some(t) => {
                    registry.remove(&t);
                    servers::save(&registry)?;
                    println!("removed {t}");
                    Ok(())
                }
                None => {
                    println!("(no match for {id})");
                    Ok(())
                }
            }
        }
        Some(other) => anyhow::bail!("unknown subcommand `{other}`; try list|add|remove"),
    }
}

/// Connect to Pharos, pipe the received HEVC into ffmpeg (HW decoder probed at
/// runtime), and write one PNG snapshot per ~60 frames. Proves: stream received
/// + HW decode + pixel output.
fn run_decode(addr: SocketAddr, out_dir: &str, expected_pubkey: Option<[u8; 32]>) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("create {out_dir}"))?;
    let pattern = format!("{out_dir}/frame_%04d.png");

    // select=every-60th keeps the snapshot rate sane; -vsync 0 = take frames as-is.
    // The HW decoder is probed at runtime by `media_decode` (NVDEC on the dev rig,
    // Quick Sync / Rockchip MPP / V4L2 stateless elsewhere; override with
    // TRIREME_DECODER); falls back to software.
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-stats"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let codec = media_decode::codec_from_env(media_decode::StreamCodec::Hevc);
    args.extend(media_decode::input_args_for(codec));
    args.push("-f".into());
    args.push(codec.ffmpeg_format());
    args.extend(
        [
            "-i",
            "pipe:0",
            "-vf",
            "select=not(mod(n\\,60))",
            "-vsync",
            "0",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let mut ff = Command::new(media_decode::ffmpeg_program())
        .args(&args)
        .arg(&pattern)
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn ffmpeg ({} decode)", codec.label()))?;
    let mut stdin = ff.stdin.take().context("take ffmpeg stdin")?;

    let dec = media_decode::selected_label();
    tracing::info!("trireme: connecting to {addr}; decoding via {dec} → {out_dir}/");
    let mut aus = 0u64;
    let mut bytes = 0u64;
    // Optionally present our long-term identity so an *enforcing* host (one that
    // requires peers.toml membership) authorizes us. Off by default to keep
    // `decode` a pure anonymous probe; TRIREME_PRESENT_IDENTITY=1 turns it on so
    // the full pair→stream happy path is testable headlessly.
    let identity = if std::env::var_os("TRIREME_PRESENT_IDENTITY").is_some() {
        let id_path = servers::identity_path()?;
        let id = core_crypto::Identity::load_or_create(&id_path)
            .with_context(|| format!("identity at {}", id_path.display()))?;
        tracing::info!(
            "presenting identity {} (an enforcing host must have this in peers.toml)",
            id.public_hex()
        );
        Some(id)
    } else {
        None
    };
    // Phase 2 live-test hook: TRIREME_RES_TARGET / TRIREME_RES_FLOOR (coded heights
    // in px; unset/0 = Auto / no floor) make the headless probe send a one-shot
    // resolution preference after auth, so the client→host ladder is testable
    // end-to-end without the Android UI.
    let res_pref = {
        let env_h = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
        };
        let (target_h, floor_h) = (env_h("TRIREME_RES_TARGET"), env_h("TRIREME_RES_FLOOR"));
        if target_h != 0 || floor_h != 0 {
            tracing::info!("res pref: target={target_h}p floor={floor_h}p");
            Some(core_proto::ClientResPref { target_h, floor_h })
        } else {
            None
        }
    };
    // Phase 3 live-test hook: TRIREME_BITRATE_MIN / TRIREME_BITRATE_MAX (kbps;
    // max=0 = uncapped) — one-shot bitrate range for the headless path.
    let bitrate_pref = {
        let env_k = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
        };
        let (min_kbps, max_kbps) = (env_k("TRIREME_BITRATE_MIN"), env_k("TRIREME_BITRATE_MAX"));
        if min_kbps != 0 || max_kbps != 0 {
            tracing::info!("bitrate pref: min={min_kbps} max={max_kbps} kbps");
            Some(core_proto::ClientBitratePref { min_kbps, max_kbps })
        } else {
            None
        }
    };
    // Phase 3 live-test hook: TRIREME_FPS_MIN / TRIREME_FPS_MAX (fps; max=0 = Auto/
    // panel) — one-shot framerate range for the headless path.
    let fps_pref = {
        let env_f = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
                .unwrap_or(0)
        };
        let (min_fps, max_fps) = (env_f("TRIREME_FPS_MIN"), env_f("TRIREME_FPS_MAX"));
        if min_fps != 0 || max_fps != 0 {
            tracing::info!("fps pref: min={min_fps} max={max_fps}");
            Some(core_proto::ClientFpsPref { min_fps, max_fps })
        } else {
            None
        }
    };
    let res = {
        let (caus, cbytes, cstdin) = (&mut aus, &mut bytes, &mut stdin);
        transport::run_video_client(
            addr,
            expected_pubkey,
            identity.as_ref(),
            move |au| {
                *caus += 1;
                *cbytes += au.len() as u64;
                if let Err(e) = cstdin.write_all(&au) {
                    tracing::error!("ffmpeg stdin closed: {e}");
                }
            },
            res_pref,
            bitrate_pref,
            fps_pref,
        )
    };
    drop(stdin); // EOF so ffmpeg flushes and exits
    let status = ff.wait().context("wait for ffmpeg")?;

    tracing::info!(
        "stream ended ({res:?}); received {aus} AUs ({bytes} bytes); ffmpeg {dec} exit={status}"
    );
    res
}
