// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Linux tray backend: the freedesktop StatusNotifierItem protocol via `ksni`
//! (pure-Rust D-Bus, KDE Plasma's tray / GNOME with the AppIndicator
//! extension), driven by a tokio runtime. Converts the shared RGBA icons into
//! SNI's ARGB. On a desktop with no SNI-capable tray, `tray.spawn()` simply logs
//! and the daemon no-ops cleanly.

use anyhow::{Context, Result};
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Handle, Icon, MenuItem, Tray, TrayMethods};
use std::path::PathBuf;
use std::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    detect_pairing, detect_state, find_panel, host_launcher, open_panel, rasterize_rgba,
    PairingHint, State, POLL_INTERVAL,
};

/// SNI lets the host pick the closest pixmap; ship a tray-sized 22px and a 2×
/// retina 44px so the tray renders crisply at either DPI.
const ICON_SIZES: &[u32] = &[22, 44];

/// Rasterize an SVG into the SNI pixmap fan for one state. SNI wants ARGB32 in
/// network byte order; tiny-skia gives premultiplied RGBA little-endian, so each
/// 4-byte pixel `rotate_right(1)` lands as `[A, R, G, B]`.
fn ksni_icons(svg: &[u8]) -> Result<Vec<Icon>> {
    ICON_SIZES
        .iter()
        .map(|&size| {
            let img = rasterize_rgba(svg, size).context("rasterize SVG")?;
            let mut data = img.rgba;
            for px in data.chunks_mut(4) {
                px.rotate_right(1);
            }
            Ok(Icon {
                width: img.width as i32,
                height: img.height as i32,
                data,
            })
        })
        .collect()
}

/// Pre-rasterized pixmap fans, one per state. Cloned on each SNI re-render —
/// `Icon` is just `(w, h, Vec<u8>)`, ~8 KB total per icon, so a clone is cheap.
struct StateIcons {
    offline: Vec<Icon>,
    idle: Vec<Icon>,
    streaming: Vec<Icon>,
    degraded: Vec<Icon>,
    faulted: Vec<Icon>,
}

impl StateIcons {
    fn build() -> Result<Self> {
        Ok(Self {
            offline: ksni_icons(State::Offline.svg())?,
            idle: ksni_icons(State::Idle.svg())?,
            streaming: ksni_icons(State::Streaming.svg())?,
            degraded: ksni_icons(State::Degraded.svg())?,
            faulted: ksni_icons(State::Faulted.svg())?,
        })
    }
    fn for_state(&self, s: State) -> Vec<Icon> {
        match s {
            State::Offline => self.offline.clone(),
            State::Idle => self.idle.clone(),
            State::Streaming => self.streaming.clone(),
            State::Degraded => self.degraded.clone(),
            State::Faulted => self.faulted.clone(),
        }
    }
}

/// Menu actions raised on the ksni D-Bus task. The `activate` closures must not
/// block; they push these on an mpsc and the main task does the actual work.
enum Action {
    OpenPanel,
    ToggleAutostart,
    Exit,
}

struct LighthouseTray {
    state: State,
    /// Current pairing line (issue #17): the live PIN, a lockout note, or `None`
    /// when there's nothing pairing-related to show. Rendered as a menu label.
    pairing: Option<PairingHint>,
    icons: StateIcons,
    /// `None` = no panel binary found → omit the "Open Panel" entry.
    panel_path: Option<PathBuf>,
    tx: mpsc::UnboundedSender<Action>,
}

impl Tray for LighthouseTray {
    /// Stable per-app id so KDE's tray remembers icon placement across restarts.
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").to_string()
    }
    fn title(&self) -> String {
        "Pharos".to_string()
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.for_state(self.state)
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.state.tooltip().to_string(),
            ..Default::default()
        }
    }
    /// Left-click → open the control panel. (SNI has no double-click; the menu is
    /// still right-click.) No-op if no panel binary was found.
    fn activate(&mut self, _x: i32, _y: i32) {
        if self.panel_path.is_some() {
            let _ = self.tx.send(Action::OpenPanel);
        }
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        // Pairing line (issue #17): a non-clickable label showing the live PIN
        // (or a lockout note) so a device can be paired straight from the tray.
        if let Some(hint) = &self.pairing {
            items.push(
                StandardItem {
                    label: hint.label(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }
        if self.panel_path.is_some() {
            items.push(
                StandardItem {
                    label: "Open Panel".into(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.tx.send(Action::OpenPanel);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }
        items.push(
            CheckmarkItem {
                label: "Start on boot".into(),
                checked: control_ipc::autostart_enabled(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Action::ToggleAutostart);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Exit Pharos".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Action::Exit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let icons = StateIcons::build()?;
    let panel_path = find_panel();
    info!(
        "pharos-tray starting (panel_path={:?})",
        panel_path.as_deref()
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
    let tray = LighthouseTray {
        state: State::Offline,
        pairing: None,
        icons,
        panel_path: panel_path.clone(),
        tx,
    };

    let handle: Handle<LighthouseTray> = tray
        .spawn()
        .await
        .context("register tray with StatusNotifierWatcher")?;
    info!("registered with the system tray");

    // Background poller — push updates only on a change so SNI doesn't re-emit
    // signals each tick.
    let poll_handle = handle.clone();
    let poller = tokio::spawn(async move {
        // Track both the icon state and the pairing line so a PIN rotation (or a
        // lockout) re-renders the menu even when the icon state is unchanged.
        let mut last: (State, Option<PairingHint>) = (State::Offline, None);
        loop {
            let snap = tokio::task::spawn_blocking(|| (detect_state(), detect_pairing()))
                .await
                .unwrap_or((State::Offline, None));
            if snap != last {
                if snap.0 != last.0 {
                    info!("state {:?} -> {:?}", last.0, snap.0);
                }
                let s = snap.clone();
                let _ = poll_handle
                    .update(move |t: &mut LighthouseTray| {
                        t.state = s.0;
                        t.pairing = s.1;
                    })
                    .await;
                last = snap;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });

    // Action receiver — driven by the menu's `activate` closures.
    while let Some(act) = rx.recv().await {
        match act {
            Action::OpenPanel => {
                if let Some(p) = &panel_path {
                    open_panel(p);
                }
            }
            Action::ToggleAutostart => {
                let enable = !control_ipc::autostart_enabled();
                if let Err(e) = control_ipc::set_autostart(enable, &host_launcher()) {
                    warn!("toggle start-on-boot: {e}");
                }
                // Refresh the menu so the checkbox reflects the new state.
                let _ = handle.update(|_t: &mut LighthouseTray| {}).await;
            }
            Action::Exit => {
                shutdown_host();
                break;
            }
        }
    }

    poller.abort();
    handle.shutdown().await;
    Ok(())
}

/// Stop the user-level compositor service AND the system-level USB-import bridge
/// (both transient `systemd-run` units, so "stop" is graceful teardown).
/// Best-effort: the bridge needs passwordless sudo; if absent we don't prompt.
fn shutdown_host() {
    info!("stopping pharos-host + pharos-usbimport");
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "pharos-host.service"])
        .status();
    let _ = Command::new("sudo")
        .args(["-n", "systemctl", "stop", "pharos-usbimport.service"])
        .status();
}
