// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Pharos headless Wayland compositor (Smithay).
//!
//! M1, step 1: a hand-rolled headless compositor that advertises a *synthesized*
//! virtual output (no DRM connector, no real EDID — so the EVDI-class
//! connector/EDID bugs simply cannot occur) and accepts Wayland clients.
//! Rendering of client surfaces and DMA-BUF capture → NVENC come in step 2.
//!
//! Structure follows Smithay 0.7's `smallvil` example, with the winit backend
//! and the interactive move/resize grabs removed (a streaming host drives the
//! output itself and doesn't need pointer-driven window management).

#![cfg(target_os = "linux")]

use std::{ffi::OsString, sync::Arc, time::Duration};

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::output::Mode as OutputMode;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{PopupKind, PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatHandler, SeatState},
    output::{Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopSignal, Mode as CalloopMode, PostAction,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface},
            Client, Display, DisplayHandle, Resource,
        },
    },
    utils::{Logical, Point, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
            CompositorState,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
    },
};
use tracing::info;

mod capture;
use capture::HevcEncoder;

pub mod control;
pub mod dsu;
pub mod flydigi;
pub mod gamepad;
pub mod gyro_imu;
pub mod host_input;
pub mod idle;

pub mod color_mgmt;
pub mod cuegl;
pub mod multi;
// Desktop-mirror: keyboard/pointer injection into the real desktop session
// via libei, fed from `portal_mirror::MirrorSession::connect_input`.
#[cfg(feature = "portal-mirror")]
pub mod mirror_input;
pub mod nvenc;
pub mod pairing;
// Software encode via an ffmpeg subprocess (VVC/libvvenc — no consumer GPU
// encodes H.266, so `video_codec = "vvc"` bypasses NVENC entirely).
pub mod swenc;
// Desktop-mirror capture (mirror the user's real desktop via the
// xdg-desktop-portal + PipeWire); on by default on Linux (issue #6).
#[cfg(feature = "portal-mirror")]
pub mod portal_mirror;
pub mod preview;
pub mod usbkbd;
// Pairing + identity glue now lives in the cross-platform `host-core` crate
// (shared with the Windows host). Re-exported so `compositor::pair::run` and
// the internal `crate::pair::identity_path()` call sites keep resolving.
pub use host_core::pair;

// Software (pixman) rendering + CPU readback for the capture path.
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
use smithay::utils::{Rectangle, Size};

// linux-dmabuf + real client compositing.
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::delegate_dmabuf;
use smithay::delegate_viewporter;
use smithay::desktop::space::space_render_elements;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::viewporter::ViewporterState;

// wp_presentation + relative-pointer + pointer-constraints: the three globals
// gamescope's Wayland backend hard-requires (issue #38 — its init aborts with
// "Couldn't create Wayland objects." when any is missing).
use smithay::input::pointer::PointerHandle;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler};
use smithay::{delegate_pointer_constraints, delegate_presentation, delegate_relative_pointer};

/// Event-loop shared data: the compositor state plus a display handle.
pub struct CalloopData {
    pub state: State,
    pub display_handle: DisplayHandle,
}

/// How long after a launch a newly-mapped toplevel is associated with that app's
/// slug (for Switch-To). Generous because Steam can take many seconds to bring a
/// game window up after the URI handoff.
const APP_CLAIM_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);

/// The compositor. Holds every Wayland global's server-side state.
pub struct State {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    /// How many toplevels we've already placed. For a multi-output `extend`
    /// layout we put the Nth new toplevel on the Nth output (clamped to the last)
    /// so distinct apps land on distinct streams. For mirror/single — overlapping
    /// or single output — every output sits at the same origin, so this still
    /// resolves to the same (0,0)-anchored placement as before.
    pub placed_toplevels: usize,
    /// Set by the launch path just before spawning an app (slug + launch instant):
    /// every toplevel that maps within [`APP_CLAIM_WINDOW`] of the launch claims
    /// this slug, LAST-one-wins — so a Steam splash / winewayland throwaway window
    /// that maps first is overwritten by the real game window that maps a moment
    /// later. Launch-order association sidesteps the XWayland pid problem (a Proton
    /// game's surface belongs to Xwayland, not the game). Stale dead surfaces are
    /// swept each frame (see the render loop's `app_surfaces.retain`).
    pub pending_app_launch: Option<(String, std::time::Instant)>,
    /// slug → the toplevel surface most recently launched for it. Used by
    /// [`State::switch_to_app`] to raise + focus a running app on request. Stale
    /// entries (surface gone) are pruned on lookup.
    pub app_surfaces: std::collections::HashMap<String, WlSurface>,

    // --- clipboard sync (host ⇄ client) ---
    /// Master gate for clipboard sync (both directions). `PHAROS_CLIPBOARD=0`
    /// disables it; default on. Clipboard can carry secrets, so this is the kill
    /// switch until a per-direction/per-device policy lands. Read by
    /// [`SelectionHandler::new_selection`] and the render-loop drain.
    pub clipboard_enabled: bool,
    /// Mime flavors offered by a just-changed CLIENT clipboard selection (a host app
    /// copied), stashed by [`SelectionHandler::new_selection`] for the render loop
    /// to read on a LATER turn — smithay commits the new selection AFTER the
    /// callback, so we must not read it synchronously. `Some(vec)` = a read is
    /// pending; `Some(empty)` = the host CLEARED its clipboard (propagate an empty
    /// text to the client). The render loop `take()`s it. The host→client sender,
    /// echo-guard hashes, and pipe-read plumbing all live as render-loop locals.
    pub pending_clipboard_read: Option<Vec<String>>,

    // --- host-drawn cursor (see [`State::host_cursor_wanted`]) ---
    /// What the focused client last asked the cursor to look like. Only the
    /// `Hidden` case matters: an app that hides its pointer (most games) gets
    /// no overlay arrow.
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    /// When a client last moved the host pointer (mouse or controller-as-
    /// cursor; gyro aiming deliberately doesn't count). Drives the auto-hide.
    pub last_pointer_input: Option<std::time::Instant>,
}

/// How long the overlay cursor lingers after the last pointer movement over a
/// fullscreen app, so a pointer that isn't in use stays out of the picture.
pub const CURSOR_IDLE_HIDE: std::time::Duration = std::time::Duration::from_secs(3);

impl State {
    pub fn new(event_loop: &mut EventLoop<CalloopData>, display: Display<Self>) -> Self {
        let start_time = std::time::Instant::now();
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let dmabuf_state = DmabufState::new();
        let popups = PopupManager::default();

        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        // Assume keyboard + pointer are always present (host injects input later).
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();

        let space = Space::default();
        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();

        Self {
            start_time,
            socket_name,
            display_handle: dh,
            space,
            loop_signal,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            dmabuf_state,
            popups,
            seat,
            placed_toplevels: 0,
            pending_app_launch: None,
            app_surfaces: std::collections::HashMap::new(),
            clipboard_enabled: std::env::var("PHAROS_CLIPBOARD")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("off"))
                .unwrap_or(true),
            pending_clipboard_read: None,
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            last_pointer_input: None,
        }
    }

    /// Raise + focus the toplevel most recently launched for `slug` (Switch-To).
    /// Returns the focused surface (so the render loop can point pointer/touch/gyro
    /// dispatch at it too — keyboard focus alone leaves those routed at the old
    /// window). `None` if no live window is associated; prunes a stale mapping.
    pub fn switch_to_app(&mut self, slug: &str) -> Option<WlSurface> {
        let surface = self.app_surfaces.get(slug).cloned()?;
        let win = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &surface))
            .cloned();
        match win {
            Some(w) => {
                self.space.raise_element(&w, true);
                if let Some(kbd) = self.seat.get_keyboard() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    kbd.set_focus(self, Some(surface.clone()), serial);
                }
                Some(surface)
            }
            None => {
                // The window is gone — drop the stale mapping.
                self.app_surfaces.remove(slug);
                None
            }
        }
    }

    /// Drop `app_surfaces` entries whose toplevel is no longer mapped, so a later
    /// Switch-To can't resolve to a dead surface and a winewayland throwaway window
    /// (mapped then torn down) doesn't linger as a bogus association. Called once
    /// per frame after `space.refresh()`.
    pub fn sweep_app_surfaces(&mut self) {
        if self.app_surfaces.is_empty() {
            return;
        }
        // A Vec (not a HashSet) — WlSurface has interior mutability, so it's a
        // poor hash key (clippy::mutable_key_type), and app_surfaces is tiny.
        let live: Vec<WlSurface> = self
            .space
            .elements()
            .filter_map(|w| w.toplevel().map(|t| t.wl_surface().clone()))
            .collect();
        self.app_surfaces.retain(|_, s| live.contains(s));
    }

    /// Center each mapped toplevel on its output when it renders SMALLER than the
    /// output — a fixed-size dialog (e.g. Cyberpunk's REDprelauncher) that ignored
    /// the fullscreen configure would otherwise sit in the output's top-left corner
    /// (`new_toplevel` maps at the output origin). A fullscreen app whose geometry
    /// equals the output size gets a zero offset, so this is a no-op for the common
    /// case. Called once per frame after `space.refresh()`; re-mapping to the same
    /// location is cheap, so it's idempotent and self-correcting as the dialog
    /// resizes. Per-output so an `extend` layout centers each stream independently.
    pub fn center_windows(&mut self) {
        let outputs: Vec<(i32, i32, i32, i32)> = self
            .space
            .outputs()
            .filter_map(|o| {
                self.space
                    .output_geometry(o)
                    .map(|g| (g.loc.x, g.loc.y, g.size.w, g.size.h))
            })
            .collect();
        if outputs.is_empty() {
            return;
        }
        // Snapshot (window, current loc, rendered size) so we don't borrow the
        // space while re-mapping below.
        let windows: Vec<(Window, i32, i32, i32, i32)> = self
            .space
            .elements()
            .filter_map(|w| {
                let loc = self.space.element_location(w)?;
                let sz = w.geometry().size;
                Some((w.clone(), loc.x, loc.y, sz.w, sz.h))
            })
            .collect();
        for (w, lx, ly, sw, sh) in windows {
            if sw <= 0 || sh <= 0 {
                continue; // not presenting a real buffer yet
            }
            // The output this window currently sits on (its loc falls within),
            // else the first output.
            let (ox, oy, ow, oh) = outputs
                .iter()
                .copied()
                .find(|&(x, y, w2, h2)| lx >= x && lx < x + w2 && ly >= y && ly < y + h2)
                .unwrap_or(outputs[0]);
            // Center, clamped so an oversized window never pushes off the top-left.
            let cx = ox + ((ow - sw) / 2).max(0);
            let cy = oy + ((oh - sh) / 2).max(0);
            if (cx, cy) != (lx, ly) {
                self.space.map_element(w, (cx, cy), false);
            }
        }
    }

    /// True when a mapped toplevel is smaller than the output it sits on, i.e. a
    /// windowed dialog / launcher (the same shape `center_windows` re-centers) is
    /// present. That's exactly when a streamed client (a TV with a mouse or a
    /// controller-as-cursor) needs to SEE a host cursor to click; a fullscreen
    /// app (size == output) manages its own cursor, so this stays false in-game.
    pub fn windowed_dialog_present(&self) -> bool {
        let outputs: Vec<(i32, i32)> = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o).map(|g| (g.size.w, g.size.h)))
            .collect();
        if outputs.is_empty() {
            return false;
        }
        self.space.elements().any(|w| {
            let sz = w.geometry().size;
            if sz.w <= 0 || sz.h <= 0 {
                return false;
            }
            // Smaller than EVERY output ⇒ genuinely windowed (not just smaller
            // than a mismatched secondary output in an extend layout).
            outputs.iter().all(|&(ow, oh)| sz.w < ow || sz.h < oh)
        })
    }

    /// Whether to paint the overlay arrow. The compositor never renders the
    /// cursor images clients set, so without this a fullscreen app with a
    /// normal pointer (an emulator's game list, a launcher filling the screen)
    /// shows no cursor on the stream at all. Rules, in order:
    /// - never while the focused surface holds a pointer lock (FPS aiming);
    /// - always over a windowed dialog (the original case: launchers that set
    ///   no cursor at all);
    /// - otherwise only if the app hasn't hidden its cursor AND the pointer
    ///   moved within [`CURSOR_IDLE_HIDE`].
    pub fn host_cursor_wanted(&self) -> bool {
        if self.pointer_locked() {
            return false;
        }
        if self.windowed_dialog_present() {
            return true;
        }
        !matches!(
            self.cursor_status,
            smithay::input::pointer::CursorImageStatus::Hidden
        ) && self
            .last_pointer_input
            .is_some_and(|t| t.elapsed() < CURSOR_IDLE_HIDE)
    }

    /// True when the surface under the pointer holds an ACTIVE pointer lock.
    fn pointer_locked(&self) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(surface) = pointer.current_focus() else {
            return false;
        };
        with_pointer_constraint(&surface, &pointer, |constraint| {
            constraint.is_some_and(|c| {
                c.is_active()
                    && matches!(
                        &*c,
                        smithay::wayland::pointer_constraints::PointerConstraint::Locked(_)
                    )
            })
        })
    }

    /// The host pointer's current absolute location in compositor-logical pixels
    /// (== physical at scale 1.0), for drawing the overlay cursor. `None` if no
    /// pointer is bound yet.
    pub fn pointer_location(&self) -> Option<(f64, f64)> {
        self.seat
            .get_pointer()
            .map(|p| p.current_location())
            .map(|l| (l.x, l.y))
    }

    fn init_wayland_listener(
        display: Display<State>,
        event_loop: &mut EventLoop<CalloopData>,
    ) -> OsString {
        // Auto-pick the next free `wayland-N` socket (won't clash with the host's).
        let listening_socket = ListeningSocketSource::new_auto().unwrap();
        let socket_name = listening_socket.socket_name().to_os_string();

        // INVARIANT (screenshot/portal safety): Pharos is a *headless, isolated*
        // compositor — its `wl_output`/`xdg_output` globals live only on this
        // socket, so the host's `xdg-desktop-portal-kde` / Spectacle / KWin
        // screenshots cannot see them *unless* `WAYLAND_DISPLAY=<this socket>`
        // leaks into the user session or the D-Bus/systemd activation
        // environment. If it does, the portal binds to Pharos instead of KWin
        // and screen capture goes dark (the historical "Spectacle dark-screen").
        // This socket must stay private to clients we launch (they get it via a
        // per-process `Command::env`, never a session-wide export). The future
        // extend-physical path is the *inbound* mirror — Pharos connecting to
        // the host's `wayland-0` as a client — and is unaffected by this rule.
        // See rig/WORKAROUNDS.md (Spectacle section). Cheap startup tripwire:
        match std::env::var_os("WAYLAND_DISPLAY") {
            Some(session_wd) if session_wd == socket_name => {
                tracing::error!(
                    "WAYLAND_DISPLAY={socket_name:?} is Pharos's OWN virtual-display \
                     socket — the host desktop portal / Spectacle / KWin screenshots \
                     will bind to Pharos and screen capture will break. Never export \
                     this socket session-wide; see rig/WORKAROUNDS.md."
                );
            }
            Some(session_wd) => {
                tracing::info!(
                    "virtual-display socket {socket_name:?} (private to launched \
                     clients); host session WAYLAND_DISPLAY={session_wd:?} — portal / \
                     Spectacle / KWin screenshots stay on the host compositor"
                );
            }
            None => {
                tracing::info!(
                    "virtual-display socket {socket_name:?} (private to launched \
                     clients); no host WAYLAND_DISPLAY in env"
                );
            }
        }

        let handle = event_loop.handle();

        handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("failed to init the wayland event source");

        handle
            .insert_source(
                Generic::new(display, Interest::READ, CalloopMode::Level),
                |_, display, state| {
                    // SAFETY: we never drop the display.
                    unsafe {
                        display
                            .get_mut()
                            .dispatch_clients(&mut state.state)
                            .unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

// --- Compositor / Buffer / Shm ---

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == &root)
            {
                window.on_commit();
            }
        }
        handle_commit(&mut self.popups, &self.space, surface);
    }
}
impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_compositor!(State);
delegate_shm!(State);

// --- Seat / DataDevice / Output ---

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // Not rendered (see `host_cursor_wanted`); remembered so an app that
        // hides its pointer doesn't get the overlay arrow drawn over it.
        self.cursor_status = image;
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}
delegate_seat!(State);

// --- Pointer constraints / relative pointer / presentation ---
//
// Advertised for nested-compositor clients (gamescope hosting the Steam
// session, issue #38) and pointer-locked games. Relative motion was already
// dispatched by the input path (`dispatch_input_event` / `dispatch_gyro_mouse`);
// the `zwp_relative_pointer_manager_v1` global is what lets clients bind to
// receive it. `wp_presentation` feedback is answered by the multi render loop
// at the frame-callback cadence (the encode tick is the "vblank" here).

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Activate immediately when the surface already holds pointer focus —
        // the streamed seat has no local cursor UX to arbitrate, so a focused
        // client asking to lock/confine gets it. A constraint created BEFORE
        // focus lands is picked up by `activate_pointer_constraint_on` on the
        // next injected motion.
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(c) = constraint {
                    c.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
        // Headless output: there is no host cursor to warp; the hint is
        // advisory and safely ignored.
    }
}
delegate_pointer_constraints!(State);
delegate_relative_pointer!(State);
delegate_presentation!(State);

/// Activate any not-yet-active pointer constraint on the focused `surface`.
/// Called from the input dispatch after each injected motion: a game usually
/// requests its lock BEFORE the seat ever focuses it (at startup), so the
/// `new_constraint` fast-path misses and this sweep catches it on the first
/// real pointer event. Cheap — one surface-data lookup per event.
fn activate_pointer_constraint_on(state: &mut State, surface: &WlSurface) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    with_pointer_constraint(surface, &pointer, |constraint| {
        if let Some(c) = constraint {
            if !c.is_active() {
                c.activate();
            }
        }
    });
}

impl SelectionHandler for State {
    /// A compositor-owned clipboard carries its bytes here so [`Self::send_selection`]
    /// can hand them to a pasting host app. `Arc<Vec<u8>>` is `Clone + Send + Sync`.
    type SelectionUserData = std::sync::Arc<Vec<u8>>;

    /// A focused client set (or cleared) the CLIPBOARD selection — i.e. a host app
    /// copied something. We only *record* it here; the actual pipe read happens on
    /// the next render-loop turn ([`crate::multi`] drains `pending_clipboard_read`),
    /// because smithay commits the new selection AFTER this callback returns, so a
    /// synchronous read would see the previous selection.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard || !self.clipboard_enabled {
            return;
        }
        match source {
            // A host app owns a new selection: stash its offered mime flavors; the
            // render loop requests the data after the commit.
            Some(src) => self.pending_clipboard_read = Some(src.mime_types()),
            // The host app cleared the clipboard: propagate an empty text. An empty
            // mime list is our sentinel for "clear" (no read needed).
            None => self.pending_clipboard_read = Some(Vec::new()),
        }
    }

    /// A host app is pasting from a COMPOSITOR-owned selection (one we set from the
    /// client's clipboard). Write the stored bytes to the client's pipe `fd`. The
    /// write can block if the pasting app reads slowly, so hand it to a detached
    /// thread — this callback runs on the calloop/render thread.
    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let bytes = user_data.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::File::from(fd);
            let _ = f.write_all(&bytes);
            // Drop closes the pipe → the pasting app sees EOF.
        });
    }
}

/// Cheap content hash for clipboard echo de-dup (not security-sensitive).
pub(crate) fn clipboard_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}
impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}
delegate_data_device!(State);

impl OutputHandler for State {}
delegate_output!(State);

// --- linux-dmabuf: lets EGL/GPU clients (e.g. alacritty) submit GPU buffers ---

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Optimistically accept; the GLES renderer imports the buffer at render
        // time when it builds the surface's render element.
        let _ = notifier.successful::<State>();
    }
}
delegate_dmabuf!(State);

// --- Xdg Shell (no interactive grabs in a headless host) ---

impl State {
    /// Configure a toplevel to the single virtual output's size, activated and
    /// fullscreen. This headless capture host always presents one app filling the
    /// 1920x1080 virtual output, so every toplevel gets a real size up front.
    /// Clients that wait for the compositor to tell them a size before rendering
    /// — games via `winewayland`, SDL/gamescope fullscreen — otherwise sit on the
    /// bare 0x0 "you decide" initial configure and never present a visible buffer
    /// (the symptom: a live, GPU-busy game but a black captured output). Sets
    /// pending state only; the configure is sent by the caller or, for the first
    /// one, by the initial-commit path in [`handle_commit`].
    fn configure_fullscreen(&self, surface: &ToplevelSurface) {
        self.configure_fullscreen_on(surface, self.space.outputs().next());
    }

    /// Like [`Self::configure_fullscreen`] but sized to a *specific* output (for
    /// the `extend` layout, where the Nth toplevel fills the Nth output). Falls
    /// back to the first output's size when `output` is `None`.
    fn configure_fullscreen_on(&self, surface: &ToplevelSurface, output: Option<&Output>) {
        let size = output
            .or_else(|| self.space.outputs().next())
            .and_then(|o| o.current_mode())
            .map(|m| (m.size.w, m.size.h));
        surface.with_pending_state(|state| {
            if let Some((w, h)) = size {
                state.size = Some((w, h).into());
            }
            state.states.set(xdg_toplevel::State::Activated);
            state.states.set(xdg_toplevel::State::Fullscreen);
        });
    }
}

/// A short human label for a toplevel — its xdg `app_id` + `title` — for
/// diagnostics. Either may be empty before the client sets it on first commit.
fn xdg_ident(surface: &ToplevelSurface) -> String {
    with_states(surface.wl_surface(), |states| {
        match states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
        {
            Some(a) => format!(
                "app_id={:?} title={:?}",
                a.app_id.as_deref().unwrap_or(""),
                a.title.as_deref().unwrap_or(""),
            ),
            None => "app_id=? title=?".to_string(),
        }
    })
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Window placement. In a MIRROR/single layout (one output, or several
        // sharing the same origin) every new app maps to that shared origin so
        // they stack cleanly on one z-order — no alternating between overlapping
        // outputs (which made the Bravia render two apps with the newest on top
        // and no way to switch). In an `extend` layout (outputs at distinct
        // origins) we keep the Nth-toplevel-on-Nth-output spread so distinct apps
        // land on distinct streams.
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mirror = outputs.len() <= 1 || {
            let first = self.space.output_geometry(&outputs[0]).map(|g| g.loc);
            outputs
                .iter()
                .all(|o| self.space.output_geometry(o).map(|g| g.loc) == first)
        };
        let target = if outputs.is_empty() {
            None
        } else if mirror {
            Some(outputs[0].clone())
        } else {
            Some(outputs[self.placed_toplevels.min(outputs.len() - 1)].clone())
        };
        self.configure_fullscreen_on(&surface, target.as_ref());
        let origin = target
            .as_ref()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| (g.loc.x, g.loc.y))
            .unwrap_or((0, 0));
        // Claim a pending launch so Switch-To can later raise this app by slug.
        // Last-one-wins within the launch window: a Steam splash or winewayland
        // throwaway window may map first, but the real game window maps a moment
        // later and overwrites the association (the throwaway is swept once gone).
        let wl = surface.wl_surface().clone();
        match &self.pending_app_launch {
            Some((slug, at)) if at.elapsed() < APP_CLAIM_WINDOW => {
                self.app_surfaces.insert(slug.clone(), wl);
            }
            Some(_) => self.pending_app_launch = None, // window elapsed; stop claiming
            None => {}
        }
        let window = Window::new_wayland_window(surface);
        // activate=true: raise the freshly-launched app to the top of the stack and
        // mark it activated (launch-to-front). The render loop's focus-on-count-
        // change then routes keyboard input to it (it's now the topmost element).
        // Diagnostic: name every toplevel pharos receives, so app-spawned auxiliary
        // windows (Steam's "launching game" / shader-processing / Input onboarding)
        // are visible in the log even when the render policy hides them — the first
        // step to surfacing them in-stream. See `xdg_ident`.
        tracing::info!(
            "new toplevel #{}: {} (mapped fullscreen at {origin:?}, now {} window(s) in space)",
            self.placed_toplevels,
            xdg_ident(window.toplevel().unwrap()),
            self.space.elements().count() + 1,
        );
        self.space.map_element(window, origin, true);
        self.placed_toplevels = self.placed_toplevels.saturating_add(1);
    }
    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::info!(
            "new popup (parent surface present={})",
            get_parent(surface.wl_surface()).is_some(),
        );
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Log + unmap so a departed auxiliary window (e.g. Steam's launch splash)
        // doesn't linger in the space as a stale fullscreen element on top of the
        // game. Find the matching Window and drop it.
        tracing::info!("toplevel destroyed: {}", xdg_ident(&surface));
        let victim = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface()) == Some(surface.wl_surface()))
            .cloned();
        if let Some(w) = victim {
            self.space.unmap_elem(&w);
        }
    }
    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        // A client (e.g. a game going fullscreen) asked for it — re-assert the
        // output size + fullscreen state and ack, so it sizes its swapchain.
        self.configure_fullscreen(&surface);
        surface.send_configure();
    }
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.configure_fullscreen(&surface);
        surface.send_configure();
    }
    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
    }
    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
    }
    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}
delegate_xdg_shell!(State);
delegate_viewporter!(State);

/// Send the initial xdg configure on a surface's first commit (per protocol),
/// for both toplevels and popups. Called from [`CompositorHandler::commit`].
pub fn handle_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().unwrap().wl_surface() == surface)
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(_) => {}
        }
    }
}

/// Run the headless compositor at the given mode until the process is killed.
///
/// `refresh_mhz` is millihertz (60_000 = 60.000 Hz), matching `pal::Mode`.
pub fn run_headless(width: u32, height: u32, refresh_mhz: u32) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();
    let mut state = State::new(&mut event_loop, display);

    // The synthesized virtual output. We own the mode, so there is no EDID to
    // misreport and no DRM connector to crash on.
    let output = Output::new(
        "HEADLESS-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Pharos".into(),
            model: "Virtual".into(),
        },
    );
    let _global = output.create_global::<State>(&dh);
    let mode = OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: refresh_mhz as i32,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    let socket = state.socket_name.clone();
    info!(
        "Pharos headless compositor up — WAYLAND_DISPLAY={:?}, virtual output {}x{}@{:.3}Hz",
        socket,
        width,
        height,
        refresh_mhz as f64 / 1000.0
    );

    let mut data = CalloopData {
        state,
        display_handle: dh,
    };
    event_loop.run(Some(Duration::from_millis(16)), &mut data, |data| {
        data.state.space.refresh();
        data.state.popups.cleanup();
        let _ = data.display_handle.flush_clients();
    })?;
    Ok(())
}

/// Render `frames` frames of the compositor output and encode them to `out_path`
/// as HEVC via `hevc_nvenc`. M1 step 2 (first cut): proves the render → encode
/// chain on NVENC. Frame content is an animated clear colour (no shm clients are
/// installed to composite yet); real client surfaces arrive with the GLES +
/// dmabuf path.
pub fn run_capture(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    out_path: &str,
) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();
    let mut state = State::new(&mut event_loop, display);

    let output = Output::new(
        "HEADLESS-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Pharos".into(),
            model: "Virtual".into(),
        },
    );
    let _global = output.create_global::<State>(&dh);
    let mode = OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: (fps * 1000) as i32,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    // Software renderer + an offscreen CPU buffer we read back each frame.
    let mut renderer = PixmanRenderer::new()?;
    let mut buffer =
        renderer.create_buffer(Fourcc::Argb8888, (width as i32, height as i32).into())?;
    let mut damage = OutputDamageTracker::from_output(&output);
    let no_elements: Vec<SolidColorRenderElement> = Vec::new();

    let mut encoder = HevcEncoder::new(width, height, fps, "bgra", out_path)?;
    info!(
        "capturing {} frames at {}x{} → {} via hevc_nvenc",
        frames, width, height, out_path
    );

    let mut data = CalloopData {
        state,
        display_handle: dh,
    };
    let row = (width * 4) as usize;

    for i in 0..frames {
        // Service any connected Wayland clients.
        event_loop.dispatch(Some(Duration::from_millis(1)), &mut data)?;
        data.state.space.refresh();

        // Animated clear colour so the encoded stream has real motion.
        let t = (i as f32 / frames.max(1) as f32) * std::f32::consts::TAU;
        let clear = [
            0.5 + 0.5 * t.sin(),
            0.5 + 0.5 * (t + 2.094).sin(),
            0.5 + 0.5 * (t + 4.188).sin(),
            1.0,
        ];

        let mut fb = renderer.bind(&mut buffer)?;
        damage.render_output(&mut renderer, &mut fb, 0, &no_elements, clear)?;
        let region = Rectangle::new((0, 0).into(), Size::from((width as i32, height as i32)));
        let mapping = renderer.copy_framebuffer(&fb, region, Fourcc::Argb8888)?;
        let bytes = renderer.map_texture(&mapping)?;

        // Repack to tightly-packed BGRA, tolerating any row padding.
        let stride = bytes.len() / height as usize;
        if stride == row {
            encoder.submit(bytes)?;
        } else {
            let mut packed = Vec::with_capacity(row * height as usize);
            for y in 0..height as usize {
                packed.extend_from_slice(&bytes[y * stride..y * stride + row]);
            }
            encoder.submit(&packed)?;
        }

        drop(fb);
        let _ = data.display_handle.flush_clients();
    }

    encoder.finish()?;
    info!("capture complete: {}", out_path);
    Ok(())
}

/// GLES variant of [`run_capture`]: renders via a headless EGL/GLES context off
/// a DRM render node. This is the vendor-neutral GPU path (Mesa on AMD/Intel,
/// NVIDIA's EGL on NVIDIA) that will later import client dmabufs and feed NVENC
/// zero-copy. For now it reads back via `glReadPixels` → `hevc_nvenc`.
pub fn run_capture_gl(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    out_path: &str,
    launch: Option<&str>,
) -> anyhow::Result<()> {
    use smithay::backend::allocator::gbm::GbmDevice;
    use smithay::backend::egl::{EGLContext, EGLDisplay};
    use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};

    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();
    let mut state = State::new(&mut event_loop, display);

    let output = Output::new(
        "HEADLESS-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Pharos".into(),
            model: "Virtual".into(),
        },
    );
    let _global = output.create_global::<State>(&dh);
    let mode = OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: (fps * 1000) as i32,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    // Headless EGL/GLES off a DRM render node — vendor-neutral GPU path.
    let node = "/dev/dri/renderD128";
    let drm_fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| anyhow::anyhow!("open {node}: {e}"))?;
    let gbm = GbmDevice::new(drm_fd)?;
    let egl_display = unsafe { EGLDisplay::new(gbm)? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
    info!("GLES renderer initialized on {node}");

    let mut target: GlesRenderbuffer =
        renderer.create_buffer(Fourcc::Abgr8888, (width as i32, height as i32).into())?;
    let mut damage = OutputDamageTracker::from_output(&output);

    // Advertise linux-dmabuf with the renderer's formats so EGL/GPU clients
    // (alacritty) can submit GPU buffers, imported at render time.
    let formats: Vec<_> = renderer.dmabuf_formats().into_iter().collect();
    let _dmabuf_global = advertise_dmabuf(&mut state.dmabuf_state, &dh, node, formats);
    // winewayland (and gamescope) require wp_viewporter to set up their surfaces;
    // without it the client creates a toplevel then immediately tears it down (the
    // symptom: `new_toplevel` fires, then the window count drops back to 0).
    let _viewporter = ViewporterState::new::<State>(&dh);
    // Advertise wp_color_manager_v1 (HDR color-management negotiation), matching the
    // multi path. OFF by default (opt in with PHAROS_ENABLE_COLOR_MGMT): the
    // handshake is incomplete and stalls real games (see the multi path + HANDOFF.md).
    let _color_mgmt = if std::env::var_os("PHAROS_ENABLE_COLOR_MGMT").is_some() {
        Some(crate::color_mgmt::init(&dh))
    } else {
        None
    };

    let socket_name = state.socket_name.clone();
    let mut encoder = HevcEncoder::new(width, height, fps, "rgba", out_path)?;
    info!(
        "capturing {} GLES frames at {}x{} → {} via hevc_nvenc",
        frames, width, height, out_path
    );

    let mut data = CalloopData {
        state,
        display_handle: dh,
    };

    // Optionally launch a client into our compositor.
    let child = match launch {
        Some(cmd) => {
            info!("launching client `{cmd}` on {socket_name:?}");
            let mut parts = cmd.split_whitespace();
            let program = parts.next().unwrap_or(cmd);
            Some(
                std::process::Command::new(program)
                    .args(parts)
                    .env("WAYLAND_DISPLAY", &socket_name)
                    .env_remove("DISPLAY")
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("spawn {cmd}: {e}"))?,
            )
        }
        None => None,
    };

    // Warm up: let the client connect, map a window, and draw before capturing.
    let warmup = std::time::Instant::now();
    while warmup.elapsed() < Duration::from_millis(2000) {
        event_loop.dispatch(Some(Duration::from_millis(16)), &mut data)?;
        data.state.space.refresh();
        let _ = data.display_handle.flush_clients();
    }

    let row = (width * 4) as usize;
    for _ in 0..frames {
        event_loop.dispatch(Some(Duration::from_millis(1)), &mut data)?;
        data.state.space.refresh();

        // Render elements for the mapped client windows over a dark background.
        let elements = space_render_elements(&mut renderer, [&data.state.space], &output, 1.0)
            .map_err(|_| anyhow::anyhow!("output has no mode"))?;

        let mut fb = renderer.bind(&mut target)?;
        damage.render_output(
            &mut renderer,
            &mut fb,
            0,
            &elements,
            [0.05, 0.05, 0.08, 1.0],
        )?;
        let region = Rectangle::new((0, 0).into(), Size::from((width as i32, height as i32)));
        let mapping = renderer.copy_framebuffer(&fb, region, Fourcc::Abgr8888)?;
        let bytes = renderer.map_texture(&mapping)?;

        let stride = bytes.len() / height as usize;
        if stride == row {
            encoder.submit(bytes)?;
        } else {
            let mut packed = Vec::with_capacity(row * height as usize);
            for y in 0..height as usize {
                packed.extend_from_slice(&bytes[y * stride..y * stride + row]);
            }
            encoder.submit(&packed)?;
        }
        drop(fb);

        // Ask clients to produce the next frame.
        let now = data.state.start_time.elapsed();
        for window in data.state.space.elements() {
            window.send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        let _ = data.display_handle.flush_clients();
    }

    encoder.finish()?;
    if let Some(mut c) = child {
        let _ = c.kill();
    }
    info!("GLES capture complete: {}", out_path);
    Ok(())
}

/// UNIX-epoch nanoseconds from the wall clock — the shared clock domain every
/// latency timestamp that crosses the link uses (matches the client's reads and
/// transport's `FrameStageMeta` stamps). A monotonic `Instant` has no shared
/// epoch across two machines, so the bench must read `SystemTime`.
pub(crate) fn unix_nanos_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Translate one Trireme InputEvent into a smithay seat dispatch.
///
/// Trireme's wire scancode is a Linux evdev code (+ 0 — the host adds the xkb
/// `+8` offset here). PointerAbs is normalised 0..=1 over the source
/// dimensions; we scale to compositor pixels. PointerRel applies as a delta
/// from the cached last absolute position; we don't try to teleport when only
/// relative motion arrives (the cursor stays where it last was).
fn dispatch_input(
    data: &mut CalloopData,
    width: u32,
    height: u32,
    focus_surface: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    ev: core_proto::InputEvent,
) {
    use smithay::backend::input::KeyState;
    use smithay::input::keyboard::{FilterResult, Keycode};
    use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
    use smithay::utils::{Point, SERIAL_COUNTER};

    let time = data.state.start_time.elapsed().as_millis() as u32;
    match ev {
        // Modifiers are tracked client-side; the host's xkb state derives mods
        // from the key sequence we feed it, so we don't need `mods` here. (We
        // forward the scancode and the press/release; that's enough.)
        core_proto::InputEvent::Key {
            scancode, pressed, ..
        } => {
            let Some(keyboard) = data.state.seat.get_keyboard() else {
                return;
            };
            let state = if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            keyboard.input::<(), _>(
                &mut data.state,
                Keycode::new(scancode + 8),
                state,
                SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| FilterResult::Forward,
            );
        }
        core_proto::InputEvent::PointerAbs { x, y } => {
            let Some(pointer) = data.state.seat.get_pointer() else {
                return;
            };
            data.state.last_pointer_input = Some(std::time::Instant::now());
            let loc = Point::from((x as f64 * width as f64, y as f64 * height as f64));
            // Focus the surface ACTUALLY under the cursor (with its real map
            // location), not the keyboard-focused surface assumed at (0,0): a
            // centered dialog (e.g. Cyberpunk's REDprelauncher) sits at an offset,
            // so a (0,0) assumption maps the click outside its bounds and it misses
            // the button. A fullscreen app fills the output → same result as before.
            let focus = data.state.surface_under(loc);
            pointer.motion(
                &mut data.state,
                focus,
                &MotionEvent {
                    location: loc,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.frame(&mut data.state);
            if let Some(s) = focus_surface {
                activate_pointer_constraint_on(&mut data.state, s);
            }
        }
        core_proto::InputEvent::PointerRel { dx, dy } => {
            let Some(pointer) = data.state.seat.get_pointer() else {
                return;
            };
            data.state.last_pointer_input = Some(std::time::Instant::now());
            // Advance the current absolute location by the delta (clamped to the
            // output) AND emit relative motion, so both desktop apps and
            // pointer-locked games (which read `wl_relative_pointer`) follow —
            // the same primitive the gyro-aim path uses (see `dispatch_gyro_mouse`).
            let cur = pointer.current_location();
            let loc = Point::from((
                (cur.x + dx as f64).clamp(0.0, f64::from(width)),
                (cur.y + dy as f64).clamp(0.0, f64::from(height)),
            ));
            // Same as PointerAbs: target the surface under the new location (real
            // map offset), so a relative cursor (gamepad/gyro) lands on a centered
            // dialog too, not just a fullscreen app at (0,0).
            let focus = data.state.surface_under(loc);
            pointer.motion(
                &mut data.state,
                focus.clone(),
                &MotionEvent {
                    location: loc,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.relative_motion(
                &mut data.state,
                focus,
                &RelativeMotionEvent {
                    delta: Point::from((dx as f64, dy as f64)),
                    delta_unaccel: Point::from((dx as f64, dy as f64)),
                    utime: u64::from(time) * 1000,
                },
            );
            pointer.frame(&mut data.state);
            if let Some(s) = focus_surface {
                activate_pointer_constraint_on(&mut data.state, s);
            }
        }
        core_proto::InputEvent::PointerButton { button, pressed } => {
            let Some(pointer) = data.state.seat.get_pointer() else {
                return;
            };
            // evdev button codes: 0x110 = BTN_LEFT, 0x111 = BTN_RIGHT,
            // 0x112 = BTN_MIDDLE, 0x113 = BTN_SIDE, 0x114 = BTN_EXTRA.
            let evdev_button = 0x110u32 + button as u32;
            let state = if pressed {
                smithay::backend::input::ButtonState::Pressed
            } else {
                smithay::backend::input::ButtonState::Released
            };
            pointer.button(
                &mut data.state,
                &ButtonEvent {
                    button: evdev_button,
                    state,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.frame(&mut data.state);
        }
        core_proto::InputEvent::Scroll { dx, dy } => {
            let Some(pointer) = data.state.seat.get_pointer() else {
                return;
            };
            let mut frame = AxisFrame::new(time).source(smithay::backend::input::AxisSource::Wheel);
            if dy != 0.0 {
                // Wayland wl_pointer axis is "discrete" units (notches) ↔ 15° per
                // notch / 10 logical units; we hand it 10 * dy for one-notch-per-1.0.
                frame = frame
                    .value(smithay::backend::input::Axis::Vertical, -dy as f64 * 10.0)
                    .v120(
                        smithay::backend::input::Axis::Vertical,
                        -(dy * 120.0) as i32,
                    );
            }
            if dx != 0.0 {
                frame = frame
                    .value(smithay::backend::input::Axis::Horizontal, dx as f64 * 10.0)
                    .v120(
                        smithay::backend::input::Axis::Horizontal,
                        (dx * 120.0) as i32,
                    );
            }
            pointer.axis(&mut data.state, frame);
            pointer.frame(&mut data.state);
        }
        // Controller input is routed to the virtual pads / gyro→mouse in the
        // input drain loop before reaching here; arms kept for exhaustiveness.
        core_proto::InputEvent::GamepadButton { .. }
        | core_proto::InputEvent::GamepadAxis { .. }
        | core_proto::InputEvent::GamepadConnect { .. }
        | core_proto::InputEvent::GamepadDisconnect { .. }
        | core_proto::InputEvent::GamepadMotion { .. }
        | core_proto::InputEvent::ControllerTuning { .. } => {}
    }
}

/// Inject a gyro-aim mouse delta: advance an absolute cursor (clamped to the
/// output) AND emit relative motion, so both desktop apps and pointer-locked
/// games (which read `wl_relative_pointer`) follow the gyro.
fn dispatch_gyro_mouse(
    data: &mut CalloopData,
    width: u32,
    height: u32,
    focus_surface: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    cursor: &mut (f64, f64),
    dx: f64,
    dy: f64,
) {
    use smithay::input::pointer::{MotionEvent, RelativeMotionEvent};
    use smithay::utils::{Point, SERIAL_COUNTER};
    let Some(pointer) = data.state.seat.get_pointer() else {
        return;
    };
    let time = data.state.start_time.elapsed().as_millis() as u32;
    cursor.0 = (cursor.0 + dx).clamp(0.0, f64::from(width));
    cursor.1 = (cursor.1 + dy).clamp(0.0, f64::from(height));
    let focus = focus_surface.map(|s| (s.clone(), Point::from((0.0, 0.0))));
    pointer.motion(
        &mut data.state,
        focus.clone(),
        &MotionEvent {
            location: Point::from((cursor.0, cursor.1)),
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.relative_motion(
        &mut data.state,
        focus,
        &RelativeMotionEvent {
            delta: Point::from((dx, dy)),
            delta_unaccel: Point::from((dx, dy)),
            utime: u64::from(time) * 1000,
        },
    );
    pointer.frame(&mut data.state);
    if let Some(s) = focus_surface {
        activate_pointer_constraint_on(&mut data.state, s);
    }
}

/// Advertise `zwp_linux_dmabuf_v1` **v4+ with default feedback** naming `node`
/// as the main device, falling back to the plain v3 global if the feedback
/// can't be built.
///
/// v4 matters: Xwayland's glamor (GPU) path finds its render node only through
/// the feedback's main device (we expose no `wl_drm`). With bare v3 it logs
/// "GBM Wayland interfaces not available" and falls back to software
/// rendering, which crippled X11 apps on the per-app Xwayland.
pub(crate) fn advertise_dmabuf(
    dmabuf_state: &mut DmabufState,
    dh: &smithay::reexports::wayland_server::DisplayHandle,
    node: &str,
    formats: Vec<smithay::backend::allocator::Format>,
) -> DmabufGlobal {
    use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
    use std::os::unix::fs::MetadataExt;

    let feedback = std::fs::metadata(node)
        .map_err(|e| e.to_string())
        .and_then(|m| {
            DmabufFeedbackBuilder::new(m.rdev() as libc::dev_t, formats.clone())
                .build()
                .map_err(|e| e.to_string())
        });
    match feedback {
        Ok(fb) => {
            info!("linux-dmabuf v4+ advertised with main device {node}");
            dmabuf_state.create_global_with_default_feedback::<State>(dh, &fb)
        }
        Err(e) => {
            tracing::warn!("dmabuf feedback for {node} unavailable ({e}); advertising v3 only");
            dmabuf_state.create_global::<State>(dh, formats)
        }
    }
}
