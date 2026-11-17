// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! A minimal "fullscreen everything" X11 window manager for the per-app
//! Xwayland (`compositor = "xwayland"`).
//!
//! That Xwayland is a rootful, fullscreen server holding exactly one app, and
//! without a window manager every window opens at whatever size it asked for
//! in the top-left corner (Lichii: a 1808x813 window on a 2712x1220 stream).
//! This runs on its own thread, claims the WM role on the server, and:
//!
//! * fills the screen with every normal top-level window (and keeps it filled:
//!   later resize/move requests are answered with the full-screen geometry);
//! * centres dialogs (`WM_TRANSIENT_FOR`, or `_NET_WM_WINDOW_TYPE_DIALOG`) at
//!   their own size, so a settings window or file picker stays usable;
//! * gives keyboard focus to whatever was mapped last.
//!
//! Override-redirect windows (menus, tooltips) bypass a WM by design and are
//! untouched. The thread ends when the server goes away (the app exited and
//! `-terminate` shut Xwayland down), so there's nothing to clean up.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConfigureRequestEvent, ConfigureWindowAux,
    ConnectionExt as _, EventMask, InputFocus, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

/// Start the window manager for X display `:display` on a background thread.
/// Best-effort: failures (another WM already running, server gone) only log.
pub fn spawn(dpy: u32) {
    let spawned = std::thread::Builder::new()
        .name(format!("xwm-{dpy}"))
        .spawn(move || {
            if let Err(e) = run(dpy) {
                tracing::debug!("xwm :{dpy}: ended ({e})");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("xwm :{dpy}: couldn't spawn thread: {e}");
    }
}

struct Wm {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    transient_for: Atom,
    window_type: Atom,
    type_dialog: Atom,
}

fn run(dpy: u32) -> anyhow::Result<()> {
    let (conn, screen_num) = RustConnection::connect(Some(&format!(":{dpy}")))?;
    let screen = &conn.setup().roots[screen_num];
    let (root, width, height) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);

    // Becoming the WM = selecting SubstructureRedirect on the root. Only one
    // client may; an error here means something else already manages windows.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new()
            .event_mask(EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY),
    )?
    .check()
    .map_err(|e| anyhow::anyhow!("another window manager is running: {e:?}"))?;

    let atom =
        |name: &[u8]| -> anyhow::Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
    let wm = Wm {
        transient_for: AtomEnum::WM_TRANSIENT_FOR.into(),
        window_type: atom(b"_NET_WM_WINDOW_TYPE")?,
        type_dialog: atom(b"_NET_WM_WINDOW_TYPE_DIALOG")?,
        conn,
        root,
        width,
        height,
    };
    tracing::info!("xwm :{dpy}: managing windows, fullscreen {width}x{height}");
    wm.conn.flush()?;

    loop {
        match wm.conn.wait_for_event()? {
            Event::MapRequest(e) => wm.on_map(e.window)?,
            Event::ConfigureRequest(e) => wm.on_configure(&e)?,
            _ => {}
        }
        wm.conn.flush()?;
    }
}

impl Wm {
    fn is_dialog(&self, w: Window) -> anyhow::Result<bool> {
        let transient = self
            .conn
            .get_property(false, w, self.transient_for, AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        if transient.value_len > 0 {
            return Ok(true);
        }
        let types = self
            .conn
            .get_property(false, w, self.window_type, AtomEnum::ATOM, 0, 16)?
            .reply()?;
        Ok(types
            .value32()
            .is_some_and(|mut it| it.any(|t| t == self.type_dialog)))
    }

    /// Geometry for a window: full screen, or centred at `(w, h)` for a dialog.
    fn placement(&self, dialog: bool, w: u16, h: u16) -> ConfigureWindowAux {
        if dialog {
            let (w, h) = (w.min(self.width), h.min(self.height));
            ConfigureWindowAux::new()
                .x(i32::from((self.width - w) / 2))
                .y(i32::from((self.height - h) / 2))
                .width(u32::from(w))
                .height(u32::from(h))
        } else {
            ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(self.width))
                .height(u32::from(self.height))
                .border_width(0)
        }
    }

    fn on_map(&self, w: Window) -> anyhow::Result<()> {
        let dialog = self.is_dialog(w).unwrap_or(false);
        let geo = self.conn.get_geometry(w)?.reply()?;
        self.conn
            .configure_window(w, &self.placement(dialog, geo.width, geo.height))?;
        self.conn.map_window(w)?;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, w, CURRENT_TIME)?;
        Ok(())
    }

    /// Answer a resize/move request. Normal windows stay full screen whatever
    /// they ask for; dialogs get their requested size, kept centred. Stacking
    /// requests pass through so raise/lower still works.
    fn on_configure(&self, e: &ConfigureRequestEvent) -> anyhow::Result<()> {
        if e.parent != self.root {
            return Ok(());
        }
        let dialog = self.is_dialog(e.window).unwrap_or(false);
        let mut aux = self.placement(dialog, e.width, e.height);
        if e.value_mask
            .contains(x11rb::protocol::xproto::ConfigWindow::STACK_MODE)
        {
            aux = aux.stack_mode(e.stack_mode);
        }
        self.conn.configure_window(e.window, &aux)?;
        Ok(())
    }
}
