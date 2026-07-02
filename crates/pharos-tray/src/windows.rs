//! Windows tray backend: the Win32 notification area via `tray-icon`, driven by
//! a `tao` event loop (the message pump that owns the tray's HWND — Win32
//! requires the icon to live on a thread with a running message loop). A poller
//! thread feeds state over a channel; the loop polls that channel and
//! tray-icon's global menu-event receiver each ~100 ms tick.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::{detect_state, find_panel, open_panel, State, POLL_INTERVAL};

/// Build a `tray-icon` icon from the shared RGBA raster at a tray-friendly size.
fn icon_for(state: State) -> Result<Icon> {
    let img = crate::rasterize_rgba(state.svg(), 32).context("rasterize SVG")?;
    Icon::from_rgba(img.rgba, img.width, img.height).context("build tray icon")
}

pub fn run() -> Result<()> {
    let panel_path = find_panel();
    info!(
        "pharos-tray starting (panel_path={:?})",
        panel_path.as_deref()
    );

    let event_loop = EventLoopBuilder::new().build();

    // Build the menu: "Open Panel" (only when a panel binary is resolvable) +
    // "Exit Pharos". The MenuIds are cloned out so we can match clicks after the
    // menu itself is moved into the tray.
    let menu = Menu::new();
    let open_id: Option<MenuId> = if panel_path.is_some() {
        let it = MenuItem::new("Open Panel", true, None);
        menu.append(&it).context("append Open Panel")?;
        menu.append(&PredefinedMenuItem::separator()).ok();
        Some(it.id().clone())
    } else {
        None
    };
    let exit = MenuItem::new("Exit Pharos", true, None);
    menu.append(&exit).context("append Exit")?;
    let exit_id = exit.id().clone();

    // Background poller -> state channel (send only on change).
    let (state_tx, state_rx) = mpsc::channel::<State>();
    std::thread::Builder::new()
        .name("pharos-tray-poll".into())
        .spawn(move || {
            let mut last = State::Offline;
            loop {
                let s = detect_state();
                if s != last {
                    let _ = state_tx.send(s);
                    last = s;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .ok();

    // tray-icon delivers menu clicks on a process-global channel.
    let menu_rx = MenuEvent::receiver();

    let mut menu_slot = Some(menu);
    let mut tray: Option<TrayIcon> = None;
    let mut last_state = State::Offline;

    event_loop.run(move |event, _target, control_flow| {
        // Wake ~10×/s to poll the state + menu channels; cheap, and clicks feel
        // instant. Real Win32 messages wake the loop sooner anyway.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));

        // The tray HWND must be created on the event-loop thread.
        if let Event::NewEvents(StartCause::Init) = event {
            if let Some(menu) = menu_slot.take() {
                match icon_for(State::Offline) {
                    Ok(icon) => {
                        tray = TrayIconBuilder::new()
                            .with_menu(Box::new(menu))
                            .with_tooltip(State::Offline.tooltip())
                            .with_icon(icon)
                            .build()
                            .map_err(|e| warn!("build tray: {e}"))
                            .ok();
                        if tray.is_some() {
                            info!("registered with the Windows notification area");
                        }
                    }
                    Err(e) => warn!("idle icon: {e:#}"),
                }
            }
        }

        // Apply any pending state change to the live icon + tooltip.
        while let Ok(s) = state_rx.try_recv() {
            if s != last_state {
                info!("state {:?} -> {:?}", last_state, s);
                last_state = s;
                if let Some(t) = &tray {
                    if let Ok(icon) = icon_for(s) {
                        let _ = t.set_icon(Some(icon));
                    }
                    let _ = t.set_tooltip(Some(s.tooltip()));
                }
            }
        }

        // Handle menu clicks.
        while let Ok(ev) = menu_rx.try_recv() {
            if Some(&ev.id) == open_id.as_ref() {
                if let Some(p) = &panel_path {
                    open_panel(p);
                }
            } else if ev.id == exit_id {
                shutdown_host();
                *control_flow = ControlFlow::Exit;
            }
        }
    });
}

/// Stop the host on Windows: no systemd here — terminate the host process.
fn shutdown_host() {
    info!("stopping pharos-host");
    let _ = std::process::Command::new("taskkill")
        .args(["/IM", "pharos-host.exe", "/F"])
        .status();
}
