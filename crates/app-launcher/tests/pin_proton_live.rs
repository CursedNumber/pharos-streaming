// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Opt-in live end-to-end test of the Proton auto-pin against a REAL Steam
//! install on the host: it copies the machine's `config.vdf` into a throwaway
//! root, runs the actual `RedirectProvider::pin_proton` (real compat-tool
//! discovery under the system `compatibilitytools.d`, the Steam-closed write, the
//! backup, and the idempotency re-check), and asserts a clean result — WITHOUT
//! ever touching the user's live config.
//!
//! Enable with `PHAROS_TEST_LIVE_PIN=1`; it also needs Steam CLOSED (the write is
//! guarded otherwise). Silent no-op when the env is unset or no real config.vdf is
//! present, so it's safe in CI.
//!
//!   PHAROS_TEST_LIVE_PIN=1 cargo test -p app-launcher --test pin_proton_live -- --nocapture

use std::path::PathBuf;

use app_launcher::env::SteamInstall;
use app_launcher::redirect::{self, PinOutcome, RedirectId};

fn real_config_vdf() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    for rel in [".local/share/Steam", ".steam/steam", ".steam/root"] {
        let p = home.join(rel).join("config").join("config.vdf");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[test]
fn live_pin_against_real_config() {
    if std::env::var_os("PHAROS_TEST_LIVE_PIN").is_none() {
        eprintln!("live_pin: set PHAROS_TEST_LIVE_PIN=1 to run");
        return;
    }
    let Some(src) = real_config_vdf() else {
        eprintln!("live_pin: no real config.vdf found — skipping");
        return;
    };

    // A throwaway root: <tmp>/pharos-pinlive-<pid>/config/config.vdf, seeded from
    // the real file. Discovery still reads the system compatibilitytools.d, so a
    // genuine installed Proton is what gets chosen.
    let root = std::env::temp_dir().join(format!("pharos-pinlive-{}", std::process::id()));
    let cfgdir = root.join("config");
    std::fs::create_dir_all(&cfgdir).unwrap();
    let cfg = cfgdir.join("config.vdf");
    std::fs::copy(&src, &cfg).unwrap();
    let original = std::fs::read_to_string(&cfg).unwrap();

    // Pick an appid unlikely to already be mapped so we exercise the INSERT path.
    let appid: u32 = std::env::var("PHAROS_TEST_APPID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(220);

    let provider = redirect::provider();
    let install = SteamInstall::Native { root: root.clone() };

    let first = provider
        .pin_proton(&install, RedirectId { appid })
        .expect("pin_proton failed");
    eprintln!("live_pin: appid {appid} first pass → {first:?}");

    match &first {
        PinOutcome::Pinned(tool) => {
            let after = std::fs::read_to_string(&cfg).unwrap();
            assert!(
                after.contains(&format!("\"{appid}\"")),
                "appid block present after pin"
            );
            assert!(after.contains(tool), "pinned tool name present: {tool}");
            assert_eq!(
                after.matches('{').count(),
                after.matches('}').count(),
                "braces balanced"
            );
            // A one-time backup was taken next to the config.
            assert!(
                cfg.with_extension("vdf.pharos.bak").is_file(),
                "backup written"
            );
            // Idempotency: a second pass must be a no-op.
            let second = provider
                .pin_proton(&install, RedirectId { appid })
                .expect("second pin_proton failed");
            eprintln!("live_pin: appid {appid} second pass → {second:?}");
            assert_eq!(second, PinOutcome::AlreadyPinned, "idempotent");
        }
        PinOutcome::AlreadyPinned => {
            // The appid was already mapped to the chosen tool — fine, nothing to do.
            assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        }
        PinOutcome::Deferred => {
            eprintln!("live_pin: Steam is running — write deferred (expected when Steam is up)");
        }
        PinOutcome::NoSuitableProton => {
            eprintln!("live_pin: no winewayland-capable Proton installed — nothing to pin");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }

    std::fs::remove_dir_all(&root).ok();
}
