// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

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

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConfigureNotifyEvent, ConfigureRequestEvent,
    ConfigureWindowAux, ConnectionExt as _, EventMask, InputFocus, StackMode, Window,
    CONFIGURE_NOTIFY_EVENT,
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
    /// Windows owed a resize "nudge" (see [`Wm::on_configure`]): when the next
    /// step is due, and whether the 1px shrink has happened yet.
    nudges: RefCell<HashMap<Window, (Instant, bool)>>,
}

/// Delay before a nudge starts, and between its shrink and restore steps.
/// Qt folds two back-to-back size changes into one and doesn't relayout; a
/// ~100ms gap between real ConfigureNotifys is what it reliably follows.
const NUDGE_STEP: Duration = Duration::from_millis(100);

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
        nudges: RefCell::new(HashMap::new()),
        conn,
        root,
        width,
        height,
    };
    tracing::info!("xwm :{dpy}: managing windows, fullscreen {width}x{height}");
    wm.conn.flush()?;

    loop {
        // Block on events unless a nudge is pending; then poll so it fires on time.
        let event = if wm.nudges.borrow().is_empty() {
            Some(wm.conn.wait_for_event()?)
        } else {
            let ev = wm.conn.poll_for_event()?;
            if ev.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            ev
        };
        match event {
            Some(Event::MapRequest(e)) => wm.on_map(e.window)?,
            Some(Event::ConfigureRequest(e)) => wm.on_configure(&e)?,
            Some(Event::DestroyNotify(e)) => {
                wm.nudges.borrow_mut().remove(&e.window);
            }
            _ => {}
        }
        wm.run_nudges()?;
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

    /// Geometry `(x, y, w, h)` for a window: full screen, or centred at its own
    /// `(w, h)` for a dialog.
    fn placement(&self, dialog: bool, w: u16, h: u16) -> (i16, i16, u16, u16) {
        if dialog {
            let (w, h) = (w.min(self.width), h.min(self.height));
            (
                ((self.width - w) / 2) as i16,
                ((self.height - h) / 2) as i16,
                w,
                h,
            )
        } else {
            (0, 0, self.width, self.height)
        }
    }

    /// Apply `geo` and tell the client where it really is.
    ///
    /// The synthetic `ConfigureNotify` is ICCCM 4.1.5's rule for a WM that
    /// overrides a request: when the forced geometry equals the current one the
    /// server sends nothing, so a toolkit that asked for another size (Qt
    /// restoring a saved 1280x778, say) keeps believing its request and lays
    /// its contents out for a window it doesn't have.
    fn place(
        &self,
        w: Window,
        geo: (i16, i16, u16, u16),
        stack: Option<StackMode>,
    ) -> anyhow::Result<()> {
        let (x, y, width, height) = geo;
        let mut aux = ConfigureWindowAux::new()
            .x(i32::from(x))
            .y(i32::from(y))
            .width(u32::from(width))
            .height(u32::from(height))
            .border_width(0);
        if let Some(mode) = stack {
            aux = aux.stack_mode(mode);
        }
        self.conn.configure_window(w, &aux)?;
        let notify = ConfigureNotifyEvent {
            response_type: CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: w,
            window: w,
            above_sibling: x11rb::NONE,
            x,
            y,
            width,
            height,
            border_width: 0,
            override_redirect: false,
        };
        self.conn
            .send_event(false, w, EventMask::STRUCTURE_NOTIFY, notify)?;
        Ok(())
    }

    fn on_map(&self, w: Window) -> anyhow::Result<()> {
        let dialog = self.is_dialog(w).unwrap_or(false);
        let geo = self.conn.get_geometry(w)?.reply()?;
        self.place(w, self.placement(dialog, geo.width, geo.height), None)?;
        self.conn.map_window(w)?;
        self.conn
            .set_input_focus(InputFocus::POINTER_ROOT, w, CURRENT_TIME)?;
        Ok(())
    }

    /// Answer a resize/move request. Normal windows stay full screen whatever
    /// they ask for; dialogs get their requested size, kept centred. Stacking
    /// requests pass through so raise/lower still works.
    ///
    /// A refused resize to a normal window also schedules a "nudge": 1px
    /// narrower, then back, [`NUDGE_STEP`] apart. The synthetic
    /// `ConfigureNotify` from [`Wm::place`] isn't always enough: Qt resizes its
    /// widget tree optimistically on its own request (Eden/Lichii asks for
    /// 1920x1138 when a game boots, sizing the window to the game's
    /// resolution), and a notify for the unchanged geometry didn't re-run the
    /// layout, leaving the game view 1920 wide in the top-left corner of a
    /// 2712-wide window, even after F11. Two real, spaced size changes from
    /// the server do.
    fn on_configure(&self, e: &ConfigureRequestEvent) -> anyhow::Result<()> {
        use x11rb::protocol::xproto::ConfigWindow;
        if e.parent != self.root {
            return Ok(());
        }
        let dialog = self.is_dialog(e.window).unwrap_or(false);
        let stack = e
            .value_mask
            .contains(ConfigWindow::STACK_MODE)
            .then_some(e.stack_mode);
        let geo = self.placement(dialog, e.width, e.height);
        let asked_resize = e
            .value_mask
            .intersects(ConfigWindow::WIDTH | ConfigWindow::HEIGHT);
        if !dialog && asked_resize && (e.width, e.height) != (geo.2, geo.3) {
            // (Re)arm; a request mid-nudge restarts it, so one nudge always
            // lands after the client's last request.
            self.nudges
                .borrow_mut()
                .insert(e.window, (Instant::now() + NUDGE_STEP, false));
        }
        self.place(e.window, geo, stack)
    }

    /// Advance due nudges: shrink by 1px, then restore full screen.
    fn run_nudges(&self) -> anyhow::Result<()> {
        let now = Instant::now();
        let mut nudges = self.nudges.borrow_mut();
        let due: Vec<(Window, bool)> = nudges
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(w, (_, shrunk))| (*w, *shrunk))
            .collect();
        for (w, shrunk) in due {
            if shrunk {
                nudges.remove(&w);
                self.place(w, (0, 0, self.width, self.height), None)?;
            } else {
                nudges.insert(w, (now + NUDGE_STEP, true));
                self.conn.configure_window(
                    w,
                    &ConfigureWindowAux::new().width(u32::from(self.width.saturating_sub(1))),
                )?;
            }
        }
        Ok(())
    }
}
