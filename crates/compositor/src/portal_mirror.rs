// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Experimental session-mirror capture (feature `portal-mirror`): stream the
//! user's REAL desktop session (KDE/GNOME on Wayland) instead of the headless
//! Pharos session. This is BATTLE_PLAN §6's optional "session-mirror" source
//! and `media_capture::CaptureBackend::PipewirePortal`, scaffolded now so the
//! structure is settled before the full implementation lands.
//!
//! Pipeline shape; these are the load-bearing decisions, fixed up front:
//!
//! 1. **One combined portal session** (RemoteDesktop + ScreenCast sources),
//!    not a bare ScreenCast session. The single session yields BOTH the
//!    PipeWire video node (this file) and, later, the input-injection handle
//!    ([`MirrorSession::connect_input`] -> libei), so adding remote control
//!    reuses the same user permission dialog instead of adding a second
//!    portal round; that would be the late structural change this scaffold
//!    exists to avoid.
//! 2. **Frames cross threads through a bounded channel** as [`MirrorFrame`].
//!    The PipeWire loop runs on its own thread ([`spawn_frame_stream`]); the
//!    render loop must never block on the portal side (the render-loop
//!    no-block rule). Drop policy is drop-newest on a full channel, matching
//!    the transport's video queue semantics.
//! 3. **[`MirrorFrame`] carries either DMA-BUF planes or copied SHM bytes.**
//!    Compositors may refuse DMA-BUF, so both variants exist from day one
//!    and the encode seam ([`import_frame_gl`]) accepts both; its signature
//!    never has to change when the fast path lands.
//!
//! Implemented today: portal negotiation (with restore-token plumbing), the
//! PipeWire stream thread, SHM frame delivery, DMA-BUF plane collection (fds
//! duplicated so they outlive the pool buffer), the SHM -> [`MemoryRenderBuffer`]
//! import ([`MirrorImporter`], feeding the same damage-tracked memory-import
//! path the idle screen uses — see `idle.rs`), and the libei input bridge
//! (`mirror_input.rs`, driven off [`MirrorSession::connect_input`]'s fd). Not
//! yet implemented: DMA-BUF modifier negotiation in the format pod (SHM
//! formats only for now) and the DMA-BUF -> EGLImage zero-copy import (a
//! deferred perf pass; [`MirrorImporter::element`] returns `Err` for a
//! `DmaBuf` frame today). `pharos-compositor mirror` is the smoke test:
//! negotiate, stream, print what arrives.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc::{SyncSender, TrySendError};

use anyhow::{Context as _, Result};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Rectangle, Transform};

// ---------------------------------------------------------------------------
// Portal session
// ---------------------------------------------------------------------------

/// The stable application id Pharos registers with xdg-desktop-portal.
///
/// The host runs as a bare `systemd-run` unit (`pharos-host.service`), so the
/// portal can't derive an app id from its cgroup and sees an anonymous caller.
/// Registering one gives KDE something to key a pre-authorization on: with
/// `kde-authorized` / `remote-desktop` / `io.github.cursednumber.Pharos` = `yes` in the
/// permission store, `xdg-desktop-portal-kde` skips the "Share your screen?"
/// dialog entirely (see `rig/authorize-remote-desktop.sh`).
pub const PORTAL_APP_ID: &str = "io.github.cursednumber.Pharos";

/// Register [`PORTAL_APP_ID`] for this process's portal D-Bus connection.
///
/// Must run before the first portal call on the connection; a later call is
/// refused (the connection is already bound to an id), which is harmless, so
/// failures only log. Best-effort: older portals (<1.19) lack the Registry and
/// simply fall back to the dialog.
async fn register_app_id() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let id = match PORTAL_APP_ID.parse::<ashpd::AppID>() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("portal-mirror: invalid app id {PORTAL_APP_ID}: {e}");
            return;
        }
    };
    match ashpd::register_host_app(id).await {
        Ok(()) => tracing::info!("portal-mirror: registered portal app id {PORTAL_APP_ID}"),
        Err(e) => tracing::warn!(
            "portal-mirror: couldn't register portal app id {PORTAL_APP_ID} ({e}); \
             KDE pre-authorization won't apply, the share dialog will show"
        ),
    }
}

/// One selected monitor/stream from the portal.
#[derive(Debug, Clone)]
pub struct MirrorStreamInfo {
    /// PipeWire node id to connect a video stream to.
    pub node_id: u32,
    /// Logical size in the source session, when the compositor reports it.
    pub size: Option<(i32, i32)>,
    /// Position in the source session's layout (multi-monitor), if reported.
    pub position: Option<(i32, i32)>,
}

/// An open RemoteDesktop+ScreenCast portal session.
///
/// Keep this alive for as long as frames should flow: dropping it releases
/// the D-Bus session object, and the portal then tears the cast down.
pub struct MirrorSession {
    remote: RemoteDesktop,
    session: Session<RemoteDesktop>,
    /// The granted video streams (one per selected monitor).
    pub streams: Vec<MirrorStreamInfo>,
    /// Pass this back into [`MirrorSession::open`] on the next run to skip
    /// the permission dialog (persistence wiring: store it under the Pharos
    /// config dir next to `multi.toml`; not yet written anywhere).
    pub restore_token: Option<String>,
}

/// Detect the KDE-specific "a RemoteDesktop session cannot persist" portal
/// rejection by message text (ashpd/ D-Bus don't expose a distinct error
/// variant for it — it surfaces as a generic `InvalidArgument`). Matched
/// loosely (case-insensitive "persist") so a differently-worded backend
/// hitting the same limitation still falls back instead of hard-failing.
fn is_persist_unsupported(e: &anyhow::Error) -> bool {
    format!("{e:#}").to_ascii_lowercase().contains("persist")
}

impl MirrorSession {
    /// Negotiate the combined portal session: ask for keyboard+pointer remote
    /// control and monitor screen-cast sources, then start it (the user sees
    /// one permission/selection dialog, or none with a valid restore token).
    ///
    /// **Live-validated finding (2026-07-09, KDE Plasma/`xdg-desktop-portal-kde`
    /// on Wayland):** this backend rejects `PersistMode::ExplicitlyRevoked`
    /// outright for a session that also selects RemoteDesktop devices —
    /// `SelectSources` fails immediately with `InvalidArgument: "Remote
    /// desktop sessions cannot persist"`, before any dialog is even shown.
    /// So the happy path tries persistence first (some backends DO support
    /// it, and it's the whole point of the restore token), then falls back
    /// to a fresh, non-persistent session (dialog every launch, no restore
    /// token) on that specific error — rather than failing desktop-mirror
    /// entirely on backends like this one.
    pub async fn open(restore_token: Option<&str>) -> Result<Self> {
        register_app_id().await;
        match Self::try_open(restore_token, PersistMode::ExplicitlyRevoked).await {
            Ok(s) => Ok(s),
            Err(e) if is_persist_unsupported(&e) => {
                tracing::warn!(
                    "portal-mirror: this portal backend doesn't support persisting a \
                     RemoteDesktop session ({e:#}); falling back to a non-persistent \
                     session (you'll see the permission dialog every time)"
                );
                Self::try_open(None, PersistMode::DoNot).await
            }
            Err(e) => Err(e),
        }
    }

    async fn try_open(restore_token: Option<&str>, persist: PersistMode) -> Result<Self> {
        let remote = RemoteDesktop::new()
            .await
            .context("connect to the RemoteDesktop portal (is xdg-desktop-portal running?)")?;
        let screencast = Screencast::new()
            .await
            .context("connect to the ScreenCast portal")?;

        let session = remote
            .create_session(CreateSessionOptions::default())
            .await
            .context("create the portal session")?;

        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                    .set_persist_mode(persist)
                    .set_restore_token(restore_token),
            )
            .await
            .context("SelectDevices")?
            .response()
            .context("SelectDevices response")?;

        // Embedded cursor: burned into the frames, so the client needs no
        // cursor plumbing. Metadata mode (client-side cursor, lower latency
        // feel) is a later refinement on the same session.
        screencast
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_multiple(true)
                    .set_sources(BitFlags::from(SourceType::Monitor))
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_persist_mode(persist)
                    .set_restore_token(restore_token),
            )
            .await
            .context("SelectSources")?
            .response()
            .context("SelectSources response")?;

        let selected = remote
            .start(&session, None, StartOptions::default())
            .await
            .context("start the portal session")?
            .response()
            .context("portal start response (dialog cancelled?)")?;

        let streams = selected
            .streams()
            .iter()
            .map(|s| MirrorStreamInfo {
                node_id: s.pipe_wire_node_id(),
                size: s.size(),
                position: s.position(),
            })
            .collect();

        Ok(Self {
            remote,
            session,
            streams,
            restore_token: selected.restore_token().map(String::from),
        })
    }

    /// Input-injection seam: hand back the libei socket for this session
    /// (`ConnectToEIS`). Callers pass the returned fd to
    /// [`crate::mirror_input::MirrorInput::spawn`], which speaks the libei
    /// wire protocol over it (the `reis` crate) and is fed from the existing
    /// `transport` input rx — full KVM control of the real desktop through
    /// the SAME permission the video came from. Fallback for portals without
    /// EIS v2 (not implemented): `RemoteDesktop::notify_pointer_motion` /
    /// `notify_keyboard_keycode`.
    pub async fn connect_input(&self) -> Result<OwnedFd> {
        let fd = self
            .remote
            .connect_to_eis(&self.session, ConnectToEISOptions::default())
            .await
            .context("ConnectToEIS (portal too old? needs RemoteDesktop v2)")?;
        Ok(fd)
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Pixel format of a mirror frame, decoupled from libspa types so the encode
/// seam does not depend on PipeWire. All four are 8-bit 4-byte formats; the
/// GL import maps them onto the matching GL/DRM formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorPixelFormat {
    Bgrx,
    Rgbx,
    Bgra,
    Rgba,
    /// Raw SPA `VideoFormat` id we did not map; the GL import rejects it.
    Unknown(u32),
}

/// One plane of a DMA-BUF frame. The fd is duplicated from the pool buffer,
/// so it stays valid after the PipeWire buffer is requeued.
#[derive(Debug)]
pub struct DmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A frame delivered from the portal stream. Either zero-copy DMA-BUF planes
/// (import via EGLImage) or copied SHM bytes (upload path). Both variants are
/// first-class; which one arrives depends on what the source compositor and
/// the format negotiation agreed on.
#[derive(Debug)]
pub enum MirrorFrame {
    DmaBuf {
        width: u32,
        height: u32,
        format: MirrorPixelFormat,
        /// DRM format modifier from the negotiated video info (0 = linear).
        modifier: u64,
        planes: Vec<DmaBufPlane>,
    },
    Shm {
        width: u32,
        height: u32,
        format: MirrorPixelFormat,
        stride: u32,
        data: Vec<u8>,
    },
}

impl MirrorFrame {
    pub fn kind(&self) -> &'static str {
        match self {
            MirrorFrame::DmaBuf { .. } => "dmabuf",
            MirrorFrame::Shm { .. } => "shm",
        }
    }

    pub fn size(&self) -> (u32, u32) {
        match self {
            MirrorFrame::DmaBuf { width, height, .. } | MirrorFrame::Shm { width, height, .. } => {
                (*width, *height)
            }
        }
    }
}

fn map_format(f: pipewire::spa::param::video::VideoFormat) -> MirrorPixelFormat {
    use pipewire::spa::param::video::VideoFormat as VF;
    match f {
        VF::BGRx => MirrorPixelFormat::Bgrx,
        VF::RGBx => MirrorPixelFormat::Rgbx,
        VF::BGRA => MirrorPixelFormat::Bgra,
        VF::RGBA => MirrorPixelFormat::Rgba,
        other => MirrorPixelFormat::Unknown(other.as_raw()),
    }
}

// ---------------------------------------------------------------------------
// PipeWire stream thread
// ---------------------------------------------------------------------------

/// Handle to a running frame stream. Stops (and joins) the PipeWire loop
/// thread on [`StreamHandle::stop`] or drop.
pub struct StreamHandle {
    quit: Option<pipewire::channel::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl StreamHandle {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Per-stream state living on the PipeWire loop thread.
struct StreamState {
    format: pipewire::spa::param::video::VideoInfoRaw,
    have_format: bool,
    tx: SyncSender<MirrorFrame>,
    dropped: u64,
}

/// Connect a video stream to `node_id` (from [`MirrorStreamInfo`]) on a
/// dedicated PipeWire loop thread and deliver frames into `tx`.
///
/// Format negotiation currently offers the common 4-byte SHM formats only;
/// offering DMA-BUF (a second EnumFormat pod carrying a modifier choice) is
/// the wiring pass. The process path already handles both buffer types, so
/// enabling DMA-BUF later changes only the pod built here.
pub fn spawn_frame_stream(node_id: u32, tx: SyncSender<MirrorFrame>) -> Result<StreamHandle> {
    let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    let join = std::thread::Builder::new()
        .name(format!("pharos-mirror-pw-{node_id}"))
        .spawn(move || {
            if let Err(e) = run_pw_loop(node_id, tx, quit_rx, &ready_tx) {
                // If setup already succeeded the ready channel is spent;
                // late errors only get logged.
                let msg = format!("{e:#}");
                tracing::error!("portal-mirror pipewire loop: {msg}");
                let _ = ready_tx.try_send(Err(msg));
            }
        })
        .context("spawn the pipewire loop thread")?;

    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(StreamHandle {
            quit: Some(quit_tx),
            join: Some(join),
        }),
        Ok(Err(e)) => {
            let _ = join.join();
            anyhow::bail!("pipewire stream setup failed: {e}")
        }
        Err(_) => {
            let _ = quit_tx.send(());
            let _ = join.join();
            anyhow::bail!("pipewire stream setup timed out")
        }
    }
}

fn run_pw_loop(
    node_id: u32,
    tx: SyncSender<MirrorFrame>,
    quit_rx: pipewire::channel::Receiver<()>,
    ready_tx: &SyncSender<Result<(), String>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::spa;
    use pw::spa::pod::Pod;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pipewire mainloop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pipewire context")?;
    let core = context.connect_rc(None).context("connect to pipewire")?;

    // Cross-thread stop: StreamHandle sends () and the attached receiver
    // quits the loop from inside the loop thread (MainLoop is not Send).
    let loop_quit = mainloop.clone();
    let _quit_attached = quit_rx.attach(mainloop.loop_(), move |_| loop_quit.quit());

    let state = StreamState {
        format: Default::default(),
        have_format: false,
        tx,
        dropped: 0,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "pharos-mirror",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("create pipewire stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_stream, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if state.format.parse(param).is_ok() {
                state.have_format = true;
                tracing::info!(
                    "portal-mirror: negotiated {:?} {}x{} @ {}/{}",
                    state.format.format(),
                    state.format.size().width,
                    state.format.size().height,
                    state.format.framerate().num,
                    state.format.framerate().denom,
                );
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            if !state.have_format {
                return;
            }
            let width = state.format.size().width;
            let height = state.format.size().height;
            let format = map_format(state.format.format());
            let modifier = state.format.modifier();

            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }

            let frame = if datas[0].type_() == spa::buffer::DataType::DmaBuf {
                // Duplicate every plane fd: the pool buffer is requeued as
                // soon as this callback returns, but the dmabuf itself lives
                // until the last fd closes.
                let mut planes = Vec::with_capacity(datas.len());
                for d in datas.iter() {
                    let raw = unsafe { libc::fcntl(d.fd(), libc::F_DUPFD_CLOEXEC, 0) };
                    if raw < 0 {
                        tracing::warn!("portal-mirror: dup dmabuf fd failed; dropping frame");
                        return;
                    }
                    planes.push(DmaBufPlane {
                        fd: unsafe { OwnedFd::from_raw_fd(raw) },
                        offset: d.chunk().offset(),
                        stride: d.chunk().stride() as u32,
                    });
                }
                MirrorFrame::DmaBuf {
                    width,
                    height,
                    format,
                    modifier,
                    planes,
                }
            } else {
                // SHM/MemFd: MAP_BUFFERS mapped it; copy out the used range.
                let chunk_offset = datas[0].chunk().offset() as usize;
                let chunk_size = datas[0].chunk().size() as usize;
                let stride = datas[0].chunk().stride() as u32;
                let Some(map) = datas[0].data() else { return };
                let end = chunk_offset.saturating_add(chunk_size).min(map.len());
                let start = chunk_offset.min(end);
                MirrorFrame::Shm {
                    width,
                    height,
                    format,
                    stride,
                    data: map[start..end].to_vec(),
                }
            };

            // Drop-newest on backpressure: stale desktop frames are worthless.
            match state.tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    state.dropped += 1;
                    if state.dropped.is_power_of_two() {
                        tracing::debug!(
                            "portal-mirror: consumer slow; {} frame(s) dropped",
                            state.dropped
                        );
                    }
                }
                Err(TrySendError::Disconnected(_)) => {
                    // Consumer gone; nothing useful left to do until quit.
                }
            }
        })
        .register()
        .context("register stream listener")?;

    // Offer the common 4-byte formats. DMA-BUF negotiation (modifier choice
    // in a second pod) lands with the GL import; see the module docs.
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .context("serialize format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("format pod bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connect stream to the portal node")?;

    let _ = ready_tx.try_send(Ok(()));
    mainloop.run();
    Ok(())
}

// ---------------------------------------------------------------------------
// Encode seam
// ---------------------------------------------------------------------------

/// Map a [`MirrorPixelFormat`] onto the matching `smithay`/DRM [`Fourcc`] by
/// **memory byte order**, not by name. DRM fourcc names spell channels
/// MSB->LSB for a pixel packed as a little-endian 32-bit word, so e.g.
/// `Fourcc::Abgr8888` is memory bytes `R,G,B,A` — the SAME convention already
/// established (and review-verified) for the idle screen canvas; see the byte
/// order comment on `idle.rs`'s `IdleScreen::build`. PipeWire/SPA instead
/// names its 32-bit formats by literal memory byte order (`Bgrx` = bytes
/// `B,G,R,X`), so the memory-equivalent pairs are:
///
/// | SPA format (memory order) | DRM Fourcc (same memory order) |
/// |----------------------------|----------------------------------|
/// | `Bgrx` (`B,G,R,X`)         | `Xrgb8888`                        |
/// | `Rgbx` (`R,G,B,X`)         | `Xbgr8888`                        |
/// | `Bgra` (`B,G,R,A`)         | `Argb8888`                        |
/// | `Rgba` (`R,G,B,A`)         | `Abgr8888`                        |
fn mirror_format_to_fourcc(f: MirrorPixelFormat) -> Option<Fourcc> {
    match f {
        MirrorPixelFormat::Bgrx => Some(Fourcc::Xrgb8888),
        MirrorPixelFormat::Rgbx => Some(Fourcc::Xbgr8888),
        MirrorPixelFormat::Bgra => Some(Fourcc::Argb8888),
        MirrorPixelFormat::Rgba => Some(Fourcc::Abgr8888),
        MirrorPixelFormat::Unknown(_) => None,
    }
}

/// Feeds successive [`MirrorFrame`]s into ONE persistent
/// [`MemoryRenderBuffer`], reusing smithay's damage-tracked memory-import
/// path (`element()` on the idle screen does the same, but bakes its content
/// once; here the content changes every call). Keep one `MirrorImporter` per
/// mirrored output for its lifetime — recreating it would defeat the
/// texture-reuse damage tracking does for you.
#[derive(Default)]
pub struct MirrorImporter {
    buffer: Option<MemoryRenderBuffer>,
    format: Option<MirrorPixelFormat>,
}

impl MirrorImporter {
    /// Copy `frame`'s pixels into the backing buffer (creating/resizing/
    /// reformatting it as needed) and return a render element for THIS
    /// frame, ready to hand to `render_output` alongside (or instead of) the
    /// app-window elements.
    ///
    /// `dst` is the target output's logical size. The element is **best-fit
    /// scaled** into it, preserving aspect ratio and centered — so a wide host
    /// desktop (e.g. 5120×1440) letterboxes into a 16:9 stream instead of being
    /// cropped to its middle band. The uncovered bars are left to the caller's
    /// clear color (`idle::BG`).
    ///
    /// SHM frames only for now — a `DmaBuf` frame returns `Err`; its
    /// zero-copy EGLImage import is the deferred perf pass (see the module
    /// docs). Treat an `Err` as "mirror source unavailable this frame", not
    /// fatal — the caller should fall back to the idle screen / a flat fill
    /// rather than propagate.
    pub fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        frame: &MirrorFrame,
        dst: smithay::utils::Size<i32, smithay::utils::Logical>,
    ) -> Result<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let MirrorFrame::Shm {
            width,
            height,
            format,
            stride,
            data,
        } = frame
        else {
            anyhow::bail!(
                "portal-mirror: import of a {} frame is not wired yet (DMA-BUF zero-copy \
                 is a deferred perf pass); see portal_mirror module docs",
                frame.kind()
            );
        };
        let fourcc = mirror_format_to_fourcc(*format)
            .with_context(|| format!("portal-mirror: unsupported pixel format {format:?}"))?;

        let row_bytes = (*width as usize) * 4;
        let src_stride = (*stride as usize).max(row_bytes);
        anyhow::ensure!(
            row_bytes > 0 && *height > 0 && !data.is_empty(),
            "portal-mirror: empty/degenerate SHM frame ({width}x{height}, stride {stride}, {} bytes)",
            data.len()
        );

        // (Re)create the buffer if this is the first frame or the format
        // changed (a resolution change alone is handled by `ctx.resize`
        // below without losing the damage-tracked texture cache).
        let need_new = self.buffer.is_none() || self.format != Some(*format);
        if need_new {
            self.buffer = Some(MemoryRenderBuffer::new(
                fourcc,
                (*width as i32, *height as i32),
                1,
                Transform::Normal,
                None,
            ));
            self.format = Some(*format);
        }
        let buffer = self.buffer.as_mut().expect("just created above");

        {
            let mut ctx = buffer.render();
            ctx.resize((*width as i32, *height as i32));
            ctx.draw(|mem| {
                if src_stride == row_bytes {
                    let n = data.len().min(mem.len());
                    mem[..n].copy_from_slice(&data[..n]);
                } else {
                    // De-stride: PipeWire's row stride may exceed width*4
                    // (padding); MemoryBuffer assumes tightly packed rows.
                    for y in 0..*height as usize {
                        let src_off = y * src_stride;
                        let dst_off = y * row_bytes;
                        if src_off + row_bytes > data.len() || dst_off + row_bytes > mem.len() {
                            break;
                        }
                        mem[dst_off..dst_off + row_bytes]
                            .copy_from_slice(&data[src_off..src_off + row_bytes]);
                    }
                }
                let size: smithay::utils::Size<i32, smithay::utils::Buffer> =
                    (*width as i32, *height as i32).into();
                Ok::<_, std::convert::Infallible>(vec![Rectangle::from_size(size)])
            })
            .unwrap_or_else(|e: std::convert::Infallible| match e {});
        }

        // Best-fit: scale the source into `dst` preserving aspect ratio, then
        // center it (letterbox/pillarbox on the short axis). `size = Some(..)`
        // on the element scales the full buffer to that logical size; `location`
        // is the top-left of the fitted rect. Degenerate dst (0×0, before the
        // first real output geometry) falls back to a 1:1 draw at the origin.
        let (location, size) = if dst.w > 0 && dst.h > 0 {
            let scale =
                (f64::from(dst.w) / f64::from(*width)).min(f64::from(dst.h) / f64::from(*height));
            let fit_w = ((f64::from(*width) * scale).round() as i32).max(1);
            let fit_h = ((f64::from(*height) * scale).round() as i32).max(1);
            let off_x = f64::from(dst.w - fit_w) / 2.0;
            let off_y = f64::from(dst.h - fit_h) / 2.0;
            (
                (off_x.max(0.0), off_y.max(0.0)),
                Some(smithay::utils::Size::from((fit_w, fit_h))),
            )
        } else {
            ((0.0, 0.0), None)
        };

        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            buffer,
            None,
            None,
            size,
            Kind::Unspecified,
        )
        .map_err(|e| anyhow::anyhow!("portal-mirror: import mirror frame into GL: {e:?}"))
    }
}
