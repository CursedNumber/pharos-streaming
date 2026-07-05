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
//! PipeWire stream thread, SHM frame delivery, and DMA-BUF plane collection
//! (fds duplicated so they outlive the pool buffer). Not yet implemented:
//! DMA-BUF modifier negotiation in the format pod (SHM formats only for now),
//! the EGLImage -> GL import ([`import_frame_gl`] bails), and the libei
//! bridge behind [`MirrorSession::connect_input`] (returns the raw fd; the
//! `reis` protocol handling is the wiring pass). `pharos-compositor mirror`
//! is the smoke test: negotiate, stream, print what arrives.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::mpsc::{SyncSender, TrySendError};

use anyhow::{Context as _, Result};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use ashpd::enumflags2::BitFlags;

// ---------------------------------------------------------------------------
// Portal session
// ---------------------------------------------------------------------------

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

impl MirrorSession {
    /// Negotiate the combined portal session: ask for keyboard+pointer remote
    /// control and monitor screen-cast sources, then start it (the user sees
    /// one permission/selection dialog, or none with a valid restore token).
    pub async fn open(restore_token: Option<&str>) -> Result<Self> {
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
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
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
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
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
    /// (`ConnectToEIS`). The wiring pass turns this fd into an EI sender
    /// (the `reis` crate) fed from the existing `transport` input rx, giving
    /// full KVM control of the real desktop through the SAME permission the
    /// video came from. Until then the fd is returned raw so callers can
    /// already structure around it. Fallback for portals without EIS v2:
    /// `RemoteDesktop::notify_pointer_motion` / `notify_keyboard_keycode`.
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

/// Import a [`MirrorFrame`] into the compositor's GL context as a texture,
/// ready for the existing GLES -> CUDA -> NVENC path (the same route headless
/// output frames take today).
///
/// NOT YET IMPLEMENTED; this signature is the seam the wiring pass fills in:
/// DMA-BUF planes become an `EGLImage` (`eglCreateImageKHR` with the
/// `EXT_image_dma_buf_import` attribs from format/modifier/planes) bound to a
/// texture; SHM bytes become a plain `glTexImage2D` upload. Callers should
/// treat an `Err` as "mirror source unavailable", not fatal.
pub fn import_frame_gl(
    _renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    frame: &MirrorFrame,
) -> Result<smithay::backend::renderer::gles::GlesTexture> {
    anyhow::bail!(
        "portal-mirror: GL import of a {} frame is not wired yet (scaffold); see \
         portal_mirror module docs",
        frame.kind()
    )
}
