// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Headless SPICE display + inputs sessions driven by a **single shared GLib
//! main loop** over raw `spice-client-glib-2.0` FFI.
//!
//! Contract of [`run`] (identical to `trireme::vncsrc::run`): connect to `addr`,
//! emit full top-down RGBA frames (`w*h*4`) to `on_frame` on each coalesced
//! refresh, drain `input_rx` and translate to SPICE pointer/key messages, block
//! on the caller's thread, and return `Ok` on a clean/server-side close or `Err`
//! on a connection error / connect timeout (so the player reconnects).
//!
//! ## Why one shared loop, not one-per-source
//! spice-client-glib binds every channel's socket I/O to the **global-default**
//! `GMainContext` (not the thread-default one). Only one thread may run the
//! global-default context at a time, so N sources cannot each run their own loop.
//! Instead a single daemon thread (`media-spice-loop`) runs the global-default
//! loop and hosts *all* sessions; each `run()` call registers a session on it and
//! blocks until that session ends. Sessions are independent `State` blobs, so
//! they coexist on the one loop. `on_frame`/`input_rx` are therefore invoked on
//! the shared loop thread (both are `Send`).
//!
//! ## Mouse modes
//! Absolute positioning (`spice_inputs_channel_position`) only works once the
//! guest agent negotiates *client* mouse mode (needs `spice-vdagent`). A bare
//! QEMU/Proxmox console stays in *server* mode, where only relative `_motion` is
//! honoured. We default to server mode, track the live mode via
//! `main-mouse-update`, and translate absolute pointer input into relative deltas
//! while in server mode.
//!
//! ## TODO — guest resolution (banked design directive, not yet built)
//! We currently render at the guest's *native* resolution and let the GPU fit it
//! to the cell. A future option is to ask the guest to render at a chosen
//! resolution via SPICE monitor-config (`spice_main_set_display` /
//! `..._update_display_enabled`; needs `spice-vdagent` in the guest). When we do:
//! default to a resolution whose **aspect matches Trireme's display surface** —
//! the in-window cell or the fullscreen display — so the guest fills the cell with
//! no letterbox bars. Must be configurable (the player computes the target from
//! the cell/window aspect and hands it down). Settings UI deferred to the
//! all-source-variants UI pass.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::raw::{c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use core_proto::InputEvent;
use tokio::sync::mpsc::UnboundedReceiver;

use glib_sys::{gboolean, gpointer, GSource, G_SOURCE_CONTINUE, G_SOURCE_REMOVE};
use gobject_sys::{GObject, GValue, G_TYPE_INT, G_TYPE_STRING};

use crate::ffi;
use crate::input_map::{
    abs_to_pixel, apply_button, SPICE_MOUSE_BUTTON_DOWN, SPICE_MOUSE_BUTTON_UP,
};
use crate::keymap::evdev_to_xt;
use crate::pixels::bgrx_to_rgba;

/// A sink for decoded frames: `(width, height, top-down RGBA)`. `Send` because it
/// is invoked on the shared loop thread, not the caller's.
type FrameSink = Box<dyn FnMut(u16, u16, &[u8]) + Send>;

/// Frame-readout cadence (~60 Hz). Invalidate/mark only flag `dirty`; this timer
/// does the surface read + RGBA convert + `on_frame`, coalescing bursts.
const FRAME_INTERVAL_MS: u32 = 16;
/// Input-drain cadence — tighter than frames so typing/clicks feel responsive.
const INPUT_INTERVAL_MS: u32 = 4;
/// How often the shared loop checks for newly-registered sessions.
const REGISTER_POLL_MS: u32 = 20;
/// Give up (with an error, so the player reconnects) if no usable primary
/// surface appears within this window — rather than hanging on a silent drop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A session to create on the shared loop thread, handed over from a `run()`
/// caller. All fields are `Send`.
struct SessionRequest {
    host: String,
    port: String,
    password: Option<String>,
    input_rx: Option<UnboundedReceiver<InputEvent>>,
    on_frame: FrameSink,
    /// Signalled with the session's terminal result; unblocks the `run()` caller.
    done: mpsc::Sender<Result<()>>,
}

/// Per-session state, leaked to a raw pointer and threaded through every signal/
/// timer as `user_data`. Lives on (and is only touched by) the shared loop thread.
struct State {
    session: *mut ffi::SpiceSession,
    /// This session's GLib timer sources (frame + input), destroyed on finish.
    timers: Vec<*mut GSource>,
    /// Signals the `run()` caller; taken (→ `None`) once fired.
    done: Option<mpsc::Sender<Result<()>>>,
    finished: bool,
    /// The display channel (for `get_primary`); null until it arrives.
    display: *mut ffi::SpiceChannel,
    /// The inputs channel (for pointer/key sends); null until it arrives.
    inputs: *mut ffi::SpiceInputsChannel,
    /// `true` once the inputs channel reaches OPENED — sending before that trips
    /// a `state UNCONNECTED` assertion in spice-glib.
    inputs_ready: bool,
    /// Current negotiated mouse mode (`SPICE_MOUSE_MODE_*`).
    mouse_mode: c_int,
    on_frame: FrameSink,
    rgba: Vec<u8>,
    last_dims: (c_int, c_int),
    /// Last pointer position in surface pixels (for server-mode relative deltas).
    last_pos: (c_int, c_int),
    have_pos: bool,
    /// Held-button mask (SPICE button_state); see `crate::input_map`.
    btn_mask: c_int,
    primary_ready: bool,
    dirty: bool,
    /// One-shot guard so an unhandled pixel format is logged once, not per tick.
    warned_format: bool,
    started: Instant,
    input_rx: Option<UnboundedReceiver<InputEvent>>,
}

/// Queue of sessions awaiting creation on the shared loop. `OnceLock` doubles as
/// the "is the shared loop thread started?" latch.
static PENDING: OnceLock<Mutex<Vec<SessionRequest>>> = OnceLock::new();

/// Lazily start the shared loop thread and return the pending-session queue.
fn runtime() -> &'static Mutex<Vec<SessionRequest>> {
    PENDING.get_or_init(|| {
        std::thread::Builder::new()
            .name("media-spice-loop".into())
            .spawn(|| unsafe {
                // Run the GLOBAL-default context — the one spice binds channels to.
                let ctx = glib_sys::g_main_context_default();
                let main_loop = glib_sys::g_main_loop_new(ctx, glib_sys::GFALSE);
                // Poll for newly-registered sessions and create them here.
                let poll = glib_sys::g_timeout_source_new(REGISTER_POLL_MS);
                glib_sys::g_source_set_callback(
                    poll,
                    Some(drain_pending),
                    std::ptr::null_mut(),
                    None,
                );
                glib_sys::g_source_attach(poll, ctx);
                glib_sys::g_main_loop_run(main_loop);
            })
            .expect("spawn media-spice loop thread");
        Mutex::new(Vec::new())
    })
}

pub fn run(
    addr: SocketAddr,
    password: Option<&str>,
    input_rx: Option<UnboundedReceiver<InputEvent>>,
    on_frame: impl FnMut(u16, u16, &[u8]) + Send + 'static,
) -> Result<()> {
    let (done_tx, done_rx) = mpsc::channel();
    let req = SessionRequest {
        host: addr.ip().to_string(),
        port: addr.port().to_string(),
        password: password.map(str::to_string),
        input_rx,
        on_frame: Box::new(on_frame),
        done: done_tx,
    };
    runtime().lock().unwrap().push(req);
    // Block until the shared loop creates, runs, and finishes this session.
    done_rx
        .recv()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("media-spice loop thread gone")))
}

/// Shared-loop timer: create any sessions queued by `run()` callers.
unsafe extern "C" fn drain_pending(_data: gpointer) -> gboolean {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let reqs: Vec<SessionRequest> = {
            let Some(q) = PENDING.get() else {
                return;
            };
            std::mem::take(&mut *q.lock().unwrap())
        };
        for req in reqs {
            start_session(req);
        }
    }));
    G_SOURCE_CONTINUE
}

/// Connect a GObject signal to an `extern "C"` handler. `$sig` is a C-string
/// literal; `$ty` is the handler's *exact* C signature (also documenting the
/// verified prototype) — the fn item is coerced to that pointer type, then
/// reinterpreted as GLib's type-erased `GCallback`.
macro_rules! connect_signal {
    ($obj:expr, $sig:expr, $handler:expr, $ty:ty, $data:expr) => {{
        let cb: gobject_sys::GCallback = ::std::option::Option::Some(::std::mem::transmute::<
            $ty,
            unsafe extern "C" fn(),
        >($handler));
        gobject_sys::g_signal_connect_data($obj, $sig.as_ptr(), cb, $data, None, 0);
    }};
}

/// Create + wire up one SPICE session on the shared loop thread.
unsafe fn start_session(req: SessionRequest) {
    let session = ffi::spice_session_new();
    set_str_prop(session, c"host", &req.host);
    set_str_prop(session, c"port", &req.port);
    if let Some(pw) = req.password.as_deref() {
        set_str_prop(session, c"password", pw);
    }

    let state = Box::new(State {
        session,
        timers: Vec::new(),
        done: Some(req.done),
        finished: false,
        display: std::ptr::null_mut(),
        inputs: std::ptr::null_mut(),
        inputs_ready: false,
        mouse_mode: ffi::SPICE_MOUSE_MODE_SERVER,
        on_frame: req.on_frame,
        rgba: Vec::new(),
        last_dims: (0, 0),
        last_pos: (0, 0),
        have_pos: false,
        btn_mask: 0,
        primary_ready: false,
        dirty: false,
        warned_format: false,
        started: Instant::now(),
        input_rx: req.input_rx,
    });
    let data = Box::into_raw(state);
    let dp = data.cast::<c_void>();

    connect_signal!(
        session,
        c"channel-new",
        on_channel_new,
        unsafe extern "C" fn(*mut GObject, *mut GObject, gpointer),
        dp
    );

    let frame_src = attach_timer(FRAME_INTERVAL_MS, frame_tick, dp);
    let input_src = attach_timer(INPUT_INTERVAL_MS, input_tick, dp);
    (*data).timers = vec![frame_src, input_src];

    let host = req.host;
    let ok = ffi::spice_session_connect(session);
    tracing::info!("spice: session_connect({host}) -> {ok}");
    if ok == glib_sys::GFALSE {
        finish_session(
            data,
            Err(anyhow::anyhow!("spice_session_connect failed to initiate")),
        );
    }
}

/// Tear down one session (destroy its timers, disconnect + unref the session) and
/// unblock its `run()` caller with `result`. Idempotent; frees the `State` box.
/// Does NOT touch the shared loop — other sessions keep running.
unsafe fn finish_session(data: *mut State, result: Result<()>) {
    let st = &mut *data;
    if st.finished {
        return;
    }
    st.finished = true;
    for t in st.timers.drain(..) {
        glib_sys::g_source_destroy(t);
        glib_sys::g_source_unref(t);
    }
    if !st.session.is_null() {
        ffi::spice_session_disconnect(st.session);
        gobject_sys::g_object_unref(st.session.cast());
    }
    if let Some(tx) = st.done.take() {
        let _ = tx.send(result);
    }
    drop(Box::from_raw(data));
}

/// Create a repeating timeout `GSource` and attach it to the global-default
/// context (the one the shared loop runs). Returns the owned source for teardown.
unsafe fn attach_timer(
    interval_ms: u32,
    cb: unsafe extern "C" fn(gpointer) -> gboolean,
    data: gpointer,
) -> *mut GSource {
    let src = glib_sys::g_timeout_source_new(interval_ms);
    glib_sys::g_source_set_callback(src, Some(cb), data, None);
    glib_sys::g_source_attach(src, glib_sys::g_main_context_default());
    src
}

/// Set a `gchar*` GObject property from a Rust `&str` via a `GValue` — never
/// `g_object_set` (varargs across FFI is the likeliest silent ABI mismatch).
unsafe fn set_str_prop(obj: *mut GObject, name: &std::ffi::CStr, val: &str) {
    let cval = CString::new(val).unwrap_or_default();
    let mut gv: GValue = std::mem::zeroed();
    gobject_sys::g_value_init(&mut gv, G_TYPE_STRING);
    gobject_sys::g_value_set_string(&mut gv, cval.as_ptr());
    gobject_sys::g_object_set_property(obj, name.as_ptr(), &gv);
    gobject_sys::g_value_unset(&mut gv);
}

/// Read an `gint` GObject property (used for `mouse-mode`).
unsafe fn get_int_prop(obj: *mut GObject, name: &std::ffi::CStr) -> c_int {
    let mut gv: GValue = std::mem::zeroed();
    gobject_sys::g_value_init(&mut gv, G_TYPE_INT);
    gobject_sys::g_object_get_property(obj, name.as_ptr(), &mut gv);
    let v = gobject_sys::g_value_get_int(&gv);
    gobject_sys::g_value_unset(&mut gv);
    v
}

// --- Signal handlers (signatures verified against GIR; user_data always last) ---

/// `SpiceSession::channel-new` — route the new channel by its GType.
unsafe extern "C" fn on_channel_new(_session: *mut GObject, channel: *mut GObject, data: gpointer) {
    let st = &mut *(data as *mut State);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let inst = channel.cast::<gobject_sys::GTypeInstance>();
        let is_display =
            gobject_sys::g_type_check_instance_is_a(inst, ffi::spice_display_channel_get_type())
                != 0;
        let is_inputs =
            gobject_sys::g_type_check_instance_is_a(inst, ffi::spice_inputs_channel_get_type())
                != 0;
        let is_main =
            gobject_sys::g_type_check_instance_is_a(inst, ffi::spice_main_channel_get_type()) != 0;
        tracing::debug!(
            "spice: channel-new display={is_display} inputs={is_inputs} main={is_main}"
        );
        // Secondary channels (display/inputs/cursor) are created but NOT auto-
        // connected by the session — we must connect each one ourselves. (The
        // main channel is already connecting from spice_session_connect.)
        if !is_main {
            ffi::spice_channel_connect(channel.cast());
        }
        if is_display {
            st.display = channel;
            connect_signal!(
                channel,
                c"display-primary-create",
                on_primary_create,
                unsafe extern "C" fn(
                    *mut GObject,
                    c_int,
                    c_int,
                    c_int,
                    c_int,
                    c_int,
                    gpointer,
                    gpointer,
                ),
                data
            );
            connect_signal!(
                channel,
                c"display-primary-destroy",
                on_primary_destroy,
                unsafe extern "C" fn(*mut GObject, gpointer),
                data
            );
            connect_signal!(
                channel,
                c"display-invalidate",
                on_invalidate,
                unsafe extern "C" fn(*mut GObject, c_int, c_int, c_int, c_int, gpointer),
                data
            );
            connect_signal!(
                channel,
                c"display-mark",
                on_mark,
                unsafe extern "C" fn(*mut GObject, c_int, gpointer),
                data
            );
            connect_signal!(
                channel,
                c"channel-event",
                on_channel_event,
                unsafe extern "C" fn(*mut GObject, c_int, gpointer),
                data
            );
        } else if is_inputs {
            st.inputs = channel;
            connect_signal!(
                channel,
                c"channel-event",
                on_channel_event,
                unsafe extern "C" fn(*mut GObject, c_int, gpointer),
                data
            );
        } else if is_main {
            connect_signal!(
                channel,
                c"main-mouse-update",
                on_mouse_update,
                unsafe extern "C" fn(*mut GObject, gpointer),
                data
            );
            connect_signal!(
                channel,
                c"channel-event",
                on_channel_event,
                unsafe extern "C" fn(*mut GObject, c_int, gpointer),
                data
            );
            // Seed the initial mode (the agent may already be in client mode).
            st.mouse_mode = get_int_prop(channel, c"mouse-mode");
        }
        // The library auto-connects each channel; we never call spice_channel_connect.
    }));
}

/// `SpiceChannel::channel-event` — the connect/close/error arbiter.
unsafe extern "C" fn on_channel_event(channel: *mut GObject, event: c_int, data: gpointer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        tracing::debug!("spice: channel-event {event}");
        let st = &mut *(data as *mut State);
        // Mark the inputs channel ready once it opens (gate sends until then).
        if event == ffi::SPICE_CHANNEL_OPENED && channel == st.inputs {
            st.inputs_ready = true;
        }
        // Decide whether this event terminates the session, then drop the `st`
        // borrow before finish_session (which reconstructs &mut State + frees it).
        let terminal: Option<Result<()>> = match event {
            ffi::SPICE_CHANNEL_CLOSED | ffi::SPICE_CHANNEL_SWITCHING => Some(Ok(())),
            ffi::SPICE_CHANNEL_ERROR_CONNECT
            | ffi::SPICE_CHANNEL_ERROR_TLS
            | ffi::SPICE_CHANNEL_ERROR_LINK
            | ffi::SPICE_CHANNEL_ERROR_AUTH
            | ffi::SPICE_CHANNEL_ERROR_IO => Some(Err(anyhow::anyhow!(
                "spice channel error {event}: {}",
                channel_error_message(channel)
            ))),
            _ => None, // OPENED, NONE, etc.
        };
        if let Some(result) = terminal {
            finish_session(data as *mut State, result);
        }
    }));
}

/// Read the channel's last `GError` message (borrowed, channel-owned — copy out;
/// do not free).
unsafe fn channel_error_message(channel: *mut GObject) -> String {
    let gerr = ffi::spice_channel_get_error(channel.cast());
    if gerr.is_null() || (*gerr).message.is_null() {
        return "connection error".to_string();
    }
    std::ffi::CStr::from_ptr((*gerr).message)
        .to_string_lossy()
        .into_owned()
}

/// `SpiceDisplayChannel::display-primary-create` — new primary surface geometry.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_primary_create(
    _channel: *mut GObject,
    format: c_int,
    width: c_int,
    height: c_int,
    _stride: c_int,
    _shmid: c_int,
    _imgdata: gpointer,
    data: gpointer,
) {
    let st = &mut *(data as *mut State);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        tracing::info!("spice: primary-create {width}x{height} fmt={format}");
        st.last_dims = (width, height);
        st.primary_ready = true;
        st.dirty = true;
    }));
}

/// `SpiceDisplayChannel::display-primary-destroy`.
unsafe extern "C" fn on_primary_destroy(_channel: *mut GObject, data: gpointer) {
    let st = &mut *(data as *mut State);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        st.primary_ready = false;
    }));
}

/// `SpiceDisplayChannel::display-invalidate` — a region changed; coalesce.
unsafe extern "C" fn on_invalidate(
    _channel: *mut GObject,
    _x: c_int,
    _y: c_int,
    _w: c_int,
    _h: c_int,
    data: gpointer,
) {
    let st = &mut *(data as *mut State);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        tracing::trace!("spice: invalidate");
        st.dirty = true;
    }));
}

/// `SpiceDisplayChannel::display-mark` — surface ready to display (`mark` is a
/// `gint` per GIR, not a gboolean as the header comment implies).
unsafe extern "C" fn on_mark(_channel: *mut GObject, mark: c_int, data: gpointer) {
    let st = &mut *(data as *mut State);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if mark != 0 {
            st.dirty = true;
        }
    }));
}

/// `SpiceMainChannel::main-mouse-update` — the negotiated mouse mode changed.
unsafe extern "C" fn on_mouse_update(channel: *mut GObject, data: gpointer) {
    let st = &mut *(data as *mut State);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        st.mouse_mode = get_int_prop(channel, c"mouse-mode");
        // Re-anchor relative motion against the next absolute position.
        st.have_pos = false;
    }));
}

// --- Periodic workers -------------------------------------------------------

/// Coalescing frame readout: convert the live primary surface to RGBA and hand
/// it to `on_frame`, at most once per tick and only when something changed.
unsafe extern "C" fn frame_tick(data: gpointer) -> gboolean {
    match catch_unwind(AssertUnwindSafe(|| frame_tick_inner(data as *mut State))) {
        Ok(ret) => ret,
        Err(_) => {
            finish_session(
                data as *mut State,
                Err(anyhow::anyhow!("panic in spice frame_tick")),
            );
            G_SOURCE_REMOVE
        }
    }
}

unsafe fn frame_tick_inner(data: *mut State) -> gboolean {
    let st = &mut *data;
    // We poll the primary surface directly rather than waiting for the
    // `display-primary-create` signal — that signal doesn't fire for a QXL
    // device in VGA-compat mode (SeaBIOS / no guest driver), but `get_primary`
    // still returns the surface. The connect-timeout is keyed off having ever
    // obtained a valid primary.
    if st.display.is_null() {
        if st.started.elapsed() > CONNECT_TIMEOUT {
            finish_session(
                data,
                Err(anyhow::anyhow!(
                    "spice: no display channel within {CONNECT_TIMEOUT:?}"
                )),
            );
            return G_SOURCE_REMOVE;
        }
        return G_SOURCE_CONTINUE;
    }
    let mut p = MaybeUninit::<ffi::SpiceDisplayPrimary>::zeroed();
    let got =
        ffi::spice_display_channel_get_primary(st.display, 0, p.as_mut_ptr()) != glib_sys::GFALSE;
    let p = p.assume_init();
    if !got || p.data.is_null() || p.width <= 0 || p.height <= 0 {
        if !st.primary_ready && st.started.elapsed() > CONNECT_TIMEOUT {
            finish_session(
                data,
                Err(anyhow::anyhow!(
                    "spice: no display surface within {CONNECT_TIMEOUT:?}"
                )),
            );
            return G_SOURCE_REMOVE;
        }
        return G_SOURCE_CONTINUE;
    }
    // First valid primary → mark ready (clears the timeout) and force a render.
    if !st.primary_ready {
        st.primary_ready = true;
        st.dirty = true;
        tracing::info!(
            "spice: first primary {}x{} fmt={}",
            p.width,
            p.height,
            p.format
        );
    }
    if !st.dirty {
        return G_SOURCE_CONTINUE;
    }
    // v1 handles the 32-bit formats QEMU/Proxmox actually use; anything else is
    // logged once and skipped (a 16-bit primary would otherwise render garbage).
    if p.format != ffi::SPICE_SURFACE_FMT_32_xRGB && p.format != ffi::SPICE_SURFACE_FMT_32_ARGB {
        if !st.warned_format {
            tracing::warn!("spice: unsupported primary format {} — skipping", p.format);
            st.warned_format = true;
        }
        st.dirty = false;
        return G_SOURCE_CONTINUE;
    }
    let (w, h, stride) = (p.width as usize, p.height as usize, p.stride as usize);
    let src = std::slice::from_raw_parts(p.data, stride * h);
    bgrx_to_rgba(src, w, h, stride, &mut st.rgba);
    st.last_dims = (p.width, p.height);
    (st.on_frame)(w as u16, h as u16, &st.rgba);
    st.dirty = false;
    G_SOURCE_CONTINUE
}

/// Drain the player's input channel and translate to SPICE input messages.
unsafe extern "C" fn input_tick(data: gpointer) -> gboolean {
    match catch_unwind(AssertUnwindSafe(|| input_tick_inner(data as *mut State))) {
        Ok(ret) => ret,
        Err(_) => {
            finish_session(
                data as *mut State,
                Err(anyhow::anyhow!("panic in spice input_tick")),
            );
            G_SOURCE_REMOVE
        }
    }
}

unsafe fn input_tick_inner(data: *mut State) -> gboolean {
    let st = &mut *data;
    if st.inputs.is_null() || !st.inputs_ready {
        // Not connected yet — discard any queued input so we don't replay stale
        // motion once it opens (and don't trip the UNCONNECTED assertion).
        if let Some(rx) = st.input_rx.as_mut() {
            while rx.try_recv().is_ok() {}
        }
        return G_SOURCE_CONTINUE;
    }
    let inp = st.inputs;
    // Take the receiver out to avoid aliasing `st` while we mutate its fields.
    let mut rx = match st.input_rx.take() {
        Some(r) => r,
        None => return G_SOURCE_CONTINUE,
    };
    while let Ok(ev) = rx.try_recv() {
        match ev {
            InputEvent::Key {
                scancode, pressed, ..
            } => {
                if let Some(xt) = evdev_to_xt(scancode) {
                    if pressed {
                        ffi::spice_inputs_channel_key_press(inp, xt);
                    } else {
                        ffi::spice_inputs_channel_key_release(inp, xt);
                    }
                }
            }
            InputEvent::PointerAbs { x, y } => {
                let (w, h) = st.last_dims;
                let px = abs_to_pixel(x, w);
                let py = abs_to_pixel(y, h);
                if st.mouse_mode == ffi::SPICE_MOUSE_MODE_CLIENT {
                    ffi::spice_inputs_channel_position(inp, px, py, 0, st.btn_mask);
                } else if st.have_pos {
                    // Server mode: only relative motion is honoured.
                    let (lx, ly) = st.last_pos;
                    ffi::spice_inputs_channel_motion(inp, px - lx, py - ly, st.btn_mask);
                }
                st.last_pos = (px, py);
                st.have_pos = true;
            }
            InputEvent::PointerRel { dx, dy } => {
                ffi::spice_inputs_channel_motion(
                    inp,
                    dx.round() as c_int,
                    dy.round() as c_int,
                    st.btn_mask,
                );
            }
            InputEvent::PointerButton { button, pressed } => {
                if let Some((new_mask, spice_btn)) = apply_button(st.btn_mask, button, pressed) {
                    st.btn_mask = new_mask;
                    if pressed {
                        ffi::spice_inputs_channel_button_press(inp, spice_btn, st.btn_mask);
                    } else {
                        ffi::spice_inputs_channel_button_release(inp, spice_btn, st.btn_mask);
                    }
                }
            }
            InputEvent::Scroll { dx: _, dy } => {
                // SPICE models the wheel as momentary press+release of UP/DOWN.
                let (btn, notches) = if dy >= 0.0 {
                    (SPICE_MOUSE_BUTTON_UP, dy)
                } else {
                    (SPICE_MOUSE_BUTTON_DOWN, -dy)
                };
                for _ in 0..(notches.round() as i32).clamp(0, 8) {
                    ffi::spice_inputs_channel_button_press(inp, btn, st.btn_mask);
                    ffi::spice_inputs_channel_button_release(inp, btn, st.btn_mask);
                }
            }
            // Gamepad input has no SPICE equivalent — the host's virtual pad is local.
            InputEvent::GamepadButton { .. }
            | InputEvent::GamepadAxis { .. }
            | InputEvent::GamepadConnect { .. }
            | InputEvent::GamepadDisconnect { .. }
            | InputEvent::GamepadMotion { .. }
            | InputEvent::ControllerTuning { .. } => {}
        }
    }
    st.input_rx = Some(rx);
    G_SOURCE_CONTINUE
}
