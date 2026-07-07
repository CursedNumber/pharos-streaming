// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! UniFFI surface — the single boundary the Kotlin (Trireme) client crosses to
//! reach the shared Rust core. This is the "hello core" round-trip target for M0
//! (BATTLE_PLAN §10).
//!
//! Regenerate Kotlin bindings (library mode):
//!   cargo build -p core-ffi
//!   cargo run -p core-ffi --bin uniffi-bindgen -- \
//!       generate --library target/debug/libcore_ffi.so \
//!       --language kotlin --out-dir client/shared/generated/kotlin
//!
//! The generated package namespace is `core_ffi` (the crate name).

uniffi::setup_scaffolding!();

use tokio_util::sync::CancellationToken;

/// Structured info handed to the client on connect — proves a record crosses
/// the FFI boundary, not just a string.
#[derive(uniffi::Record)]
pub struct CoreInfo {
    pub agent: String,
    pub core_version: String,
    pub protocol_version: u32,
}

/// The canonical "hello core" round-trip.
#[uniffi::export]
pub fn core_info() -> CoreInfo {
    CoreInfo {
        agent: "pharos-core".to_string(),
        core_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: core_proto::PROTOCOL_VERSION,
    }
}

/// Simple greeting, used by the Android smoke test.
#[uniffi::export]
pub fn greet(name: String) -> String {
    format!("Trireme says hello to {name}, served by Pharos core.")
}

/// Sink for decoded-stream bytes, implemented on the Kotlin side. Each
/// `on_frame` carries one HEVC access unit to hand to MediaCodec; `on_end`
/// fires when the stream closes (with an error message on failure).
#[uniffi::export(callback_interface)]
pub trait FrameSink: Send + Sync {
    fn on_frame(&self, data: Vec<u8>);
    fn on_end(&self, error: Option<String>);
    /// Host link-health heartbeat (~2×/s): `true` while the host is shedding video
    /// frames (link slower than the encode rate → visible corruption), `false` when
    /// clean. Drive a subtle "degraded" indicator from it; never block (QUIC loop).
    fn on_link_health(&self, degraded: bool);
}

/// Sink for Opus audio packets (one 48 kHz / stereo / 20 ms frame per call),
/// carried as unreliable QUIC datagrams alongside the video. The host always
/// captures + sends audio while a client is connected; passing `None` for the
/// audio sink to [`start_video_stream`] simply drops it. The platform shell
/// decodes Opus natively (Android `MediaCodec("audio/opus")` → `AudioTrack`),
/// so the core never links libopus.
#[uniffi::export(callback_interface)]
pub trait AudioSink: Send + Sync {
    /// One Opus packet. Decode + play; never block (called from the QUIC loop).
    fn on_packet(&self, opus: Vec<u8>);
}

/// Sink for live gyro calibration telemetry: the host reads the controller's
/// IMU (over USB/IP or Bluetooth — the client can't see it) and streams the raw
/// per-axis angular velocity here so the client can draw a calibration
/// visualizer and validate gyro mode. `(gx, gy, gz)` are deg/s for the raw
/// `rx`/`ry`/`rz` gyro axes; passing `None` to [`start_video_stream`] drops it.
#[uniffi::export(callback_interface)]
pub trait GyroSink: Send + Sync {
    /// One gyro sample (deg/s, raw axes). Never block (called from the QUIC loop).
    fn on_sample(&self, gx: f32, gy: f32, gz: f32);
}

/// One launchable application as the client renders it — mirrors
/// [`core_proto::AppInfo`]. `kind` follows [`core_proto::apps::app_kind`]
/// (0=app, 1=game, 2=desktop, 3=media); `art` is PNG/JPEG box-art bytes (empty
/// = draw a placeholder).
#[derive(uniffi::Record)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub kind: u8,
    pub art: Vec<u8>,
    /// True if this app is currently running on the host. The client floats
    /// running apps to the top, highlights them, and offers Switch-To / End-process.
    pub running: bool,
}

/// Sink for the host's app catalog: the host pushes the launchable apps (with box
/// art) so the client can draw a tile grid. `on_catalog` replaces the previous
/// list; passing `None` for the catalog sink to [`start_video_stream`] simply
/// doesn't request it. The client still gets a [`GamepadInput::launch_app`] to
/// send a chosen id back.
#[uniffi::export(callback_interface)]
pub trait AppCatalogSink: Send + Sync {
    /// The full catalog (replaces any previous). Never block (QUIC loop).
    fn on_catalog(&self, apps: Vec<AppInfo>);
}

/// Sink for the host's clipboard: fires when a host app copied text, so the client
/// mirrors it into its OS clipboard (Android `ClipboardManager`). An empty string
/// means the host cleared its clipboard. Passing `None` to [`start_video_stream`]
/// drops host→client sync. The client sends its own copies back with
/// [`GamepadInput::set_clipboard`].
///
/// The shell MUST suppress the OS clipboard-changed listener it would otherwise
/// use to drive `set_clipboard` while applying this text, or record the applied
/// text and skip re-sending it — else host→client→host echoes.
#[uniffi::export(callback_interface)]
pub trait ClipboardSink: Send + Sync {
    /// Host clipboard text (empty = cleared). Never block (called from the QUIC loop).
    fn on_clipboard(&self, text: String);
}

/// Handle for forwarding the client's gamepad to the host, returned by
/// [`start_video_stream`]. The platform shell captures its local controller
/// (Android `KeyEvent`/`MotionEvent`, …), maps each to the **canonical** index
/// space ([`core_proto::gamepad`]), and calls these. The host replays them onto
/// a uinput virtual Xbox 360 pad the game reads via evdev — the low-latency
/// couch path. Calls are cheap, non-blocking, and no-op once the stream ends.
#[derive(uniffi::Object)]
pub struct GamepadInput {
    tx: tokio::sync::mpsc::UnboundedSender<core_proto::InputEvent>,
    /// Client→host app actions (launch / switch-to / end-process). Drained by the
    /// QUIC client task and written on the app relay stream. Exposed via
    /// [`GamepadInput::launch_app`] / [`switch_to`] / [`end_process`].
    app_tx: tokio::sync::mpsc::UnboundedSender<core_proto::AppMsg>,
    /// Client→host clipboard: the shell pushes text the user copied locally here;
    /// the QUIC client task writes it on the clipboard relay stream and the host
    /// becomes the Wayland selection owner. Exposed via [`GamepadInput::set_clipboard`].
    clipboard_tx: tokio::sync::mpsc::UnboundedSender<core_proto::ClipboardMsg>,
    /// Flip to tear down the underlying connection (e.g. a short-lived gyro
    /// calibration session). Exposed via [`GamepadInput::stop`].
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by [`GamepadInput::request_keyframe`] when the client's decoder hits a
    /// corrupt-stream error; the QUIC client loop drains it and sends one
    /// `DG_REQUEST_IDR` so the host emits a fresh keyframe to resync.
    idr_request: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The client's requested resolution range (Phase 2), packed
    /// `(target_h<<32)|floor_h`. Set by [`GamepadInput::set_resolution_pref`] from
    /// the settings UI; the QUIC client loop re-sends it ~1×/s as
    /// `DG_CLIENT_RESPREF` so the host adapts the ladder within `[floor, target]`.
    res_pref: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The client's requested bitrate range (Phase 3), packed
    /// `(min_kbps<<32)|max_kbps` (max=0 = uncapped). Set by
    /// [`GamepadInput::set_bitrate_pref`]; the QUIC loop re-sends it ~1×/s as
    /// `DG_CLIENT_BITRATEPREF` — the host's primary adaptive lever.
    bitrate_pref: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The client's requested framerate range (Phase 3), packed
    /// `(min_fps<<32)|max_fps` (max=0 = Auto/panel). Set by
    /// [`GamepadInput::set_fps_pref`]; the QUIC loop re-sends it ~1×/s as
    /// `DG_CLIENT_FPSPREF` — caps the host encode pace + adaptation floor.
    fps_pref: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[uniffi::export]
impl GamepadInput {
    /// A controller appeared on `slot`; the host classifies it and brings up a
    /// virtual pad. Call once when a controller connects.
    pub fn connect(&self, slot: u8, vendor: u16, product: u16) {
        let _ = self.tx.send(core_proto::InputEvent::GamepadConnect {
            slot,
            vendor,
            product,
        });
    }

    /// A controller on `slot` went away; the host drops its pad.
    pub fn disconnect(&self, slot: u8) {
        let _ = self
            .tx
            .send(core_proto::InputEvent::GamepadDisconnect { slot });
    }

    /// A keyboard key transition. `scancode` is the Linux **evdev** code (the
    /// host adds the xkb `+8`); the shell maps its native key → evdev before
    /// calling this. The host derives modifiers from the key sequence, so `mods`
    /// is left empty. Reaches the focused Wayland surface on the host (e.g. the
    /// media player). No-op once the stream ends.
    pub fn key(&self, scancode: u32, pressed: bool) {
        let _ = self.tx.send(core_proto::InputEvent::Key {
            scancode,
            pressed,
            mods: core_proto::Modifiers::default(),
        });
    }

    /// Button transition. `button` is the canonical index: 0=A 1=B 2=X 3=Y
    /// 4=LB 5=RB 6=Back 7=Start 8=Guide 9=LStick 10=RStick.
    pub fn button(&self, slot: u8, button: u8, pressed: bool) {
        let _ = self.tx.send(core_proto::InputEvent::GamepadButton {
            slot,
            button,
            pressed,
        });
    }

    /// Analog axis. `axis` is the canonical index: 0=LeftX 1=LeftY 2=RightX
    /// 3=RightY 4=LeftTrigger 5=RightTrigger 6=HatX 7=HatY. `value` is -1..=1
    /// for sticks/hat (down/right positive), 0..=1 for triggers.
    pub fn axis(&self, slot: u8, axis: u8, value: f32) {
        let _ = self
            .tx
            .send(core_proto::InputEvent::GamepadAxis { slot, axis, value });
    }

    /// Motion-sensor sample for gyro-aim: gyroscope (rad/s) + accelerometer (g).
    /// The host turns gyro into relative mouse motion.
    #[allow(clippy::too_many_arguments)]
    pub fn motion(&self, slot: u8, gx: f32, gy: f32, gz: f32, ax: f32, ay: f32, az: f32) {
        let _ = self.tx.send(core_proto::InputEvent::GamepadMotion {
            slot,
            gx,
            gy,
            gz,
            ax,
            ay,
            az,
        });
    }

    /// Relay a controller's tuning (face swaps + gyro feel) from the client's
    /// settings UI to the host, which applies it live and persists it keyed by
    /// `vendor:product`. Call when the user edits a slider/toggle, and once on
    /// connect so the host re-asserts the client's saved feel.
    pub fn set_tuning(&self, slot: u8, vendor: u16, product: u16, profile: ControllerProfile) {
        let _ = self.tx.send(core_proto::InputEvent::ControllerTuning {
            slot,
            vendor,
            product,
            profile: profile.into(),
        });
    }

    /// Launch a host app by its catalog id (the slug from an [`AppInfo`]). Sent
    /// to the host on the app relay stream; the host spawns it into the virtual
    /// display. Call when the user selects a tile in the Apps grid.
    pub fn launch_app(&self, id: String) {
        let _ = self.app_tx.send(core_proto::AppMsg::Launch { id });
    }

    /// Switch to (raise + focus) a running host app by its catalog id. Call from
    /// the Apps grid's context menu on a running tile.
    pub fn switch_to(&self, id: String) {
        let _ = self.app_tx.send(core_proto::AppMsg::SwitchTo { id });
    }

    /// End (kill the whole process tree of) a running host app by its catalog id.
    /// Call from the Apps grid's context menu on a running tile.
    pub fn end_process(&self, id: String) {
        let _ = self.app_tx.send(core_proto::AppMsg::EndProcess { id });
    }

    /// Push text the user copied on the client to the host clipboard (empty string
    /// clears it). The host becomes the Wayland selection owner so host apps paste
    /// it. Drive from the OS clipboard-changed listener; to avoid a host→client→host
    /// echo, skip calling this for text the client just applied from [`ClipboardSink`].
    /// Cheap, non-blocking; no-op once the stream ends. Text is capped at
    /// [`core_proto::MAX_CLIPBOARD_BYTES`] (truncated on a char boundary) so an
    /// oversized OS clipboard can't produce a frame that trips the host reader.
    pub fn set_clipboard(&self, mut text: String) {
        if text.len() > core_proto::MAX_CLIPBOARD_BYTES {
            let mut end = core_proto::MAX_CLIPBOARD_BYTES;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let _ = self.clipboard_tx.send(core_proto::ClipboardMsg::Text(text));
    }

    /// Tear down the connection this handle drives. Used by the gyro calibration
    /// overlay to end its short-lived telemetry session when dismissed.
    pub fn stop(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Ask the host for an immediate keyframe (IDR). Call when the client's decoder
    /// reports a corrupt-stream error so it resyncs in ~1 RTT instead of waiting for
    /// the next periodic IDR. Cheap, non-blocking; safe to call from the decode
    /// thread. A backstop to the host's own drop-driven IDR.
    pub fn request_keyframe(&self) {
        self.idr_request
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the desired resolution range (Phase 2). `target_h` is the ceiling coded
    /// height the host should uphold (`0` = Auto / native); `floor_h` is the
    /// minimum it may auto-downscale to (`0` = no floor / lowest rung). The host
    /// maps these per-output (aspect-preserving) and adapts within the range. Call
    /// when the user moves the resolution slider, and once on connect to re-assert
    /// the saved choice. Cheap, non-blocking; takes effect within ~1s.
    pub fn set_resolution_pref(&self, target_h: u32, floor_h: u32) {
        let packed = (u64::from(target_h) << 32) | u64::from(floor_h);
        self.res_pref
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the desired bitrate range (Phase 3) — the host's primary adaptive
    /// lever. `min_kbps` is the floor it never auto-drops below; `max_kbps` is the
    /// ceiling, with `0` = uncapped (host owns the top, still flow-window bounded).
    /// Call when the user moves the bitrate slider, and once on connect to
    /// re-assert. Cheap, non-blocking; takes effect within ~1s.
    pub fn set_bitrate_pref(&self, min_kbps: u32, max_kbps: u32) {
        let packed = (u64::from(min_kbps) << 32) | u64::from(max_kbps);
        self.bitrate_pref
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the desired framerate range (Phase 3). `min_fps` is the adaptation floor
    /// the host won't auto-drop below (`0` = host default 24); `max_fps` is the
    /// target ceiling the host paces the encode (and direct-launched game render)
    /// to, clamped to the panel refresh (`0` = Auto/panel). Call when the user moves
    /// the fps slider, and once on connect to re-assert. Cheap, non-blocking.
    pub fn set_fps_pref(&self, min_fps: u32, max_fps: u32) {
        let packed = (u64::from(min_fps) << 32) | u64::from(max_fps);
        self.fps_pref
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Gyro→mouse tuning for the client UI — mirrors [`core_proto::GyroTuning`]
/// (UniFFI can't derive `Record` on a foreign type, so we mirror + convert).
#[derive(uniffi::Record)]
pub struct GyroTuning {
    pub enabled: bool,
    pub sens_x: f32,
    pub sens_y: f32,
    pub accel: f32,
    pub accel_pow: f32,
    pub accel_ref: f32,
    pub accel_max: f32,
    pub invert_x: bool,
    pub invert_y: bool,
    /// 0=rx, 1=ry, 2=rz — which raw gyro axis drives horizontal / vertical.
    pub axis_h: u8,
    pub axis_v: u8,
}

/// Per-controller profile for the client UI — mirrors [`core_proto::ControllerProfile`].
#[derive(uniffi::Record)]
pub struct ControllerProfile {
    pub swap_ab: bool,
    pub swap_xy: bool,
    pub gyro: GyroTuning,
}

impl From<ControllerProfile> for core_proto::ControllerProfile {
    fn from(p: ControllerProfile) -> Self {
        core_proto::ControllerProfile {
            // The client side has no alias surface yet (host renames live in
            // panel + relay); leave it `None` so a relayed tuning doesn't
            // accidentally clobber a host-set alias.
            alias: None,
            swap_ab: p.swap_ab,
            swap_xy: p.swap_xy,
            gyro: core_proto::GyroTuning {
                enabled: p.gyro.enabled,
                sens_x: p.gyro.sens_x,
                sens_y: p.gyro.sens_y,
                accel: p.gyro.accel,
                accel_pow: p.gyro.accel_pow,
                accel_ref: p.gyro.accel_ref,
                accel_max: p.gyro.accel_max,
                invert_x: p.gyro.invert_x,
                invert_y: p.gyro.invert_y,
                axis_h: p.gyro.axis_h,
                axis_v: p.gyro.axis_v,
            },
        }
    }
}

/// The built-in default feel, exposed so the client UI can seed a controller's
/// sliders to match the host's out-of-the-box defaults (single source of truth).
#[uniffi::export]
pub fn default_controller_profile() -> ControllerProfile {
    let d = core_proto::ControllerProfile::default();
    ControllerProfile {
        swap_ab: d.swap_ab,
        swap_xy: d.swap_xy,
        gyro: GyroTuning {
            enabled: d.gyro.enabled,
            sens_x: d.gyro.sens_x,
            sens_y: d.gyro.sens_y,
            accel: d.gyro.accel,
            accel_pow: d.gyro.accel_pow,
            accel_ref: d.gyro.accel_ref,
            accel_max: d.gyro.accel_max,
            invert_x: d.gyro.invert_x,
            invert_y: d.gyro.invert_y,
            axis_h: d.gyro.axis_h,
            axis_v: d.gyro.axis_v,
        },
    }
}

/// Recognised controller family (mirrors `core_proto::controllers::ControllerType`).
#[derive(uniffi::Enum)]
pub enum ControllerKind {
    Xbox360,
    XboxOne,
    XboxElite,
    DualShock3,
    DualShock4,
    DualSense,
    SwitchPro,
    JoyCon,
    EightBitDo,
    Flydigi,
    Stadia,
    SteamController,
    Generic,
}

/// Classification + capabilities for the client's controller selector UI.
#[derive(uniffi::Record)]
pub struct ControllerInfo {
    pub kind: ControllerKind,
    pub label: String,
    pub gyro: bool,
    pub touchpad: bool,
    pub paddles: bool,
    pub rumble: bool,
}

/// Identify a controller from its USB VID:PID + name (the shared classifier).
/// Drives the Android selector's detected-model + capability display.
#[uniffi::export]
pub fn classify_controller(vendor: u16, product: u16, name: String) -> ControllerInfo {
    use core_proto::controllers::ControllerType as T;
    let info = core_proto::controllers::classify(vendor, product, &name);
    let kind = match info.kind {
        T::Xbox360 => ControllerKind::Xbox360,
        T::XboxOne => ControllerKind::XboxOne,
        T::XboxElite => ControllerKind::XboxElite,
        T::DualShock3 => ControllerKind::DualShock3,
        T::DualShock4 => ControllerKind::DualShock4,
        T::DualSense => ControllerKind::DualSense,
        T::SwitchPro => ControllerKind::SwitchPro,
        T::JoyCon => ControllerKind::JoyCon,
        T::EightBitDo => ControllerKind::EightBitDo,
        T::Flydigi => ControllerKind::Flydigi,
        T::Stadia => ControllerKind::Stadia,
        T::SteamController => ControllerKind::SteamController,
        T::Generic => ControllerKind::Generic,
    };
    ControllerInfo {
        kind,
        label: info.kind.label().to_string(),
        gyro: info.caps.gyro,
        touchpad: info.caps.touchpad,
        paddles: info.caps.paddles,
        rumble: info.caps.rumble,
    }
}

/// Connect to a Pharos QUIC streamer at `addr` (e.g. `192.168.1.100:4433`) and
/// deliver HEVC access units to `sink` until the stream ends. Returns
/// immediately; the QUIC client runs on its own thread.
///
/// `expected_pubkey_hex` is the 64-char hex of the host's long-term Ed25519
/// pubkey (saved at pairing time). When non-empty, the per-connection auth
/// challenge rejects any host that doesn't sign for that exact key. Empty
/// (or `None`) means TOFU — accept any pubkey the host presents.
///
/// `identity_path` is the device's `identity.toml` (the same path passed to
/// [`client_pair`], e.g. `Context.getFilesDir()`). When set, we present and
/// prove this identity so an enforcing host can authorize us against its
/// paired-peers list. Empty/`None` connects anonymously.
/// `wants_gyro` opts this client in to the host's IMU gyro relay during the
/// stream auth handshake (v2 wire). Default = `false`: the host then skips the
/// per-frame gyro drain entirely, leaving the gyro reader thread idle. Set
/// `true` to receive raw gyro samples through `gyro_sink` AND to let the host
/// integrate gyro→mouse for the active app (when the app's `gyro_mode`
/// allows). The Trireme client binds this to a SharedPreferences toggle the
/// user controls in Settings; an opt-in design so old/light clients never pay
/// the IMU cost. The flag is sent once at connect; toggling it on the client
/// takes effect on the NEXT dial.
/// Client-requested stream mode passed to [`start_video_stream`]. `width`/`height`
/// of 0 = **Auto** (the host keeps its native canvas resolution). `refresh_mhz` =
/// the panel's refresh in millihertz (60 Hz = 60_000). The host caps its encode
/// fps to the refresh and (Tier-3) targets width×height.
#[derive(uniffi::Record)]
pub struct StreamMode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub fn start_video_stream(
    addr: String,
    alt_addrs: Vec<String>,
    expected_pubkey_hex: Option<String>,
    identity_path: Option<String>,
    sink: Box<dyn FrameSink>,
    audio_sink: Option<Box<dyn AudioSink>>,
    gyro_sink: Option<Box<dyn GyroSink>>,
    app_catalog_sink: Option<Box<dyn AppCatalogSink>>,
    clipboard_sink: Option<Box<dyn ClipboardSink>>,
    wants_gyro: bool,
    requested_mode: StreamMode,
) -> std::sync::Arc<GamepadInput> {
    // Client→host input channel (gamepad today). The handle we return holds the
    // sender; the QUIC client task drains the receiver and opens the input bidi
    // stream. Idle if the shell never forwards an event (no controller).
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::InputEvent>();
    // Client→host app-launch channel (tile taps). Always created so a tile can be
    // launched even without a catalog sink; the QUIC task accepts the host's app
    // relay stream and writes these.
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::AppMsg>();
    // Client→host clipboard channel. Always created so [`GamepadInput::set_clipboard`]
    // works; the QUIC task writes these on the clipboard relay stream. The client
    // syncs clipboard only when it registered a `clipboard_sink` (host→client) — but
    // it can still SEND copies regardless, so we always keep the sender.
    let (clipboard_tx, clipboard_rx) =
        tokio::sync::mpsc::unbounded_channel::<core_proto::ClipboardMsg>();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_thread = cancel.clone();
    // Client→host IDR request flag: the handle's request_keyframe() sets it; the
    // QUIC client loop drains it into a DG_REQUEST_IDR.
    let idr_request = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let idr_request_thread = idr_request.clone();
    // Client→host resolution range (Phase 2): set_resolution_pref() packs it here;
    // the QUIC client loop re-sends it ~1×/s as DG_CLIENT_RESPREF.
    let res_pref = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let res_pref_thread = res_pref.clone();
    // Client→host bitrate range (Phase 3): set_bitrate_pref() packs it here.
    let bitrate_pref = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bitrate_pref_thread = bitrate_pref.clone();
    // Client→host framerate range (Phase 3): set_fps_pref() packs it here.
    let fps_pref = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fps_pref_thread = fps_pref.clone();
    std::thread::spawn(move || {
        // LAN first, then any fallbacks (e.g. a WAN/DDNS address). IDNA-aware so
        // Unicode domains like `bücher.example` resolve; unresolvable candidates drop.
        let mut candidates = vec![addr];
        candidates.extend(alt_addrs);
        let server_addrs = transport::resolve_candidates(&candidates);
        if server_addrs.is_empty() {
            sink.on_end(Some(format!("no resolvable address in {candidates:?}")));
            return;
        }
        let expected = match expected_pubkey_hex.as_deref().filter(|s| !s.is_empty()) {
            Some(hex_s) => match decode_pubkey(hex_s) {
                Ok(b) => Some(b),
                Err(e) => {
                    sink.on_end(Some(format!("invalid expected_pubkey_hex: {e}")));
                    return;
                }
            },
            None => None,
        };
        // Present our paired identity (if we have one) so an enforcing host can
        // authorize us. Reuses the same identity.toml as pairing.
        let identity = match identity_path.as_deref().filter(|s| !s.is_empty()) {
            Some(p) => match core_crypto::Identity::load_or_create(std::path::Path::new(p)) {
                Ok(id) => Some(id),
                Err(e) => {
                    sink.on_end(Some(format!("identity at {p}: {e}")));
                    return;
                }
            },
            None => None,
        };
        // Route Opus datagrams to the platform audio sink (decoded natively
        // there).
        let on_audio: Option<transport::AudioSink> = audio_sink.map(|a| {
            let cb: transport::AudioSink = Box::new(move |pkt| a.on_packet(pkt));
            cb
        });
        // Live gyro telemetry → calibration visualizer (when a sink is provided).
        let on_gyro: Option<transport::GyroSink> = gyro_sink.map(|g| {
            let cb: transport::GyroSink = Box::new(move |gx, gy, gz| g.on_sample(gx, gy, gz));
            cb
        });
        // Host app catalog → tile grid (when a sink is provided). Map the wire
        // record to the FFI record across the boundary.
        let on_catalog: Option<Box<dyn FnMut(Vec<core_proto::AppInfo>) + Send>> = app_catalog_sink
            .map(|s| {
                let cb: Box<dyn FnMut(Vec<core_proto::AppInfo>) + Send> = Box::new(move |apps| {
                    let mapped = apps
                        .into_iter()
                        .map(|a| AppInfo {
                            id: a.id,
                            name: a.name,
                            kind: a.kind,
                            art: a.art,
                            running: a.running,
                        })
                        .collect();
                    s.on_catalog(mapped);
                });
                cb
            });
        // Host clipboard → the platform OS clipboard (when a sink is provided).
        let on_clipboard: Option<Box<dyn FnMut(String) + Send>> = clipboard_sink.map(|s| {
            let cb: Box<dyn FnMut(String) + Send> = Box::new(move |text| s.on_clipboard(text));
            cb
        });
        // Share the FrameSink across the per-frame and link-health callbacks (both
        // take &self): an Arc lets each closure own a clone — a closure that borrowed
        // `sink` couldn't be the 'static Box<dyn FnMut> the transport wants for
        // on_link_health.
        let sink: std::sync::Arc<dyn FrameSink> = sink.into();
        let frame_sink = sink.clone();
        let health_sink = sink.clone();
        let result = transport::run_video_client_with_input(
            server_addrs,
            expected,
            identity.as_ref(),
            move |frame| frame_sink.on_frame(frame),
            Some(input_rx),
            on_audio,
            on_gyro,
            Some(cancel_thread),
            None, // latency taps: no on-device decode/present wiring yet
            on_catalog,
            Some(app_rx),
            wants_gyro,
            // wants_hdr: false for now — the wire is v4 and always carries the
            // byte, but the client HDR toggle isn't exposed through the FFI yet
            // (task 6). Wiring it is a new start_video_stream param + UniFFI regen.
            false,
            Some(Box::new(move |degraded| {
                health_sink.on_link_health(degraded)
            })),
            Some(idr_request_thread),
            core_proto::StreamMode {
                width: requested_mode.width,
                height: requested_mode.height,
                refresh_mhz: requested_mode.refresh_mhz,
            },
            Some(res_pref_thread),
            Some(bitrate_pref_thread),
            Some(fps_pref_thread),
            on_clipboard,
            Some(clipboard_rx),
        );
        sink.on_end(result.err().map(|e| e.to_string()));
    });
    std::sync::Arc::new(GamepadInput {
        tx: input_tx,
        app_tx,
        clipboard_tx,
        cancel,
        idr_request,
        res_pref,
        bitrate_pref,
        fps_pref,
    })
}

fn decode_pubkey(hex_s: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_s.trim()).map_err(|e| e.to_string())?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| "pubkey must be 32 bytes".to_string())
}

// ---------------------------------------------------------------------------
// Client-side pairing handshake — same orchestration as the desktop
// `trireme::pair`, exposed across the FFI so Android/iOS share the proven
// flow. Identity is stored at `identity_path` (the caller passes its
// platform-specific app-data path; on Android, `Context.getFilesDir()`).
// ---------------------------------------------------------------------------

/// What the client learned about the host after a successful pair.
#[derive(uniffi::Record, Debug, Clone)]
pub struct PairResult {
    pub host_pubkey_hex: String,
    pub host_alias: Option<String>,
}

#[derive(uniffi::Error, Debug, thiserror::Error)]
pub enum PairError {
    #[error("invalid address: {reason}")]
    BadAddress { reason: String },
    #[error("identity error: {reason}")]
    Identity { reason: String },
    #[error("network: {reason}")]
    Network { reason: String },
    #[error("protocol violation: {reason}")]
    Protocol { reason: String },
    #[error("PIN or identity mismatch")]
    PinMismatch,
    #[error("host aborted: {reason}")]
    HostAborted { reason: String },
}

/// Drive one client-side pairing attempt. Blocks the calling thread (Kotlin
/// callers should invoke from a coroutine context). On success, the host's
/// long-term pubkey is the pinning value to stash in the local registry.
#[uniffi::export]
pub fn client_pair(
    addr: String,
    alt_addrs: Vec<String>,
    pin: String,
    identity_path: String,
    our_alias: Option<String>,
) -> Result<PairResult, PairError> {
    // LAN first, then any fallbacks (WAN/DDNS). IDNA-aware (`bücher.example` works).
    let mut candidates = vec![addr];
    candidates.extend(alt_addrs);
    let parsed = transport::resolve_candidates(&candidates);
    if parsed.is_empty() {
        return Err(PairError::BadAddress {
            reason: format!("no resolvable address in {candidates:?}"),
        });
    }

    let id_path = std::path::PathBuf::from(&identity_path);
    let identity =
        core_crypto::Identity::load_or_create(&id_path).map_err(|e| PairError::Identity {
            reason: format!("{}: {e}", id_path.display()),
        })?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PairError::Network {
            reason: format!("tokio: {e}"),
        })?;
    rt.block_on(do_pair(parsed, pin, identity, our_alias))
}

async fn do_pair(
    addrs: Vec<std::net::SocketAddr>,
    pin: String,
    identity: core_crypto::Identity,
    our_alias: Option<String>,
) -> Result<PairResult, PairError> {
    use transport::pair_proto::PairMsg;
    let mut link = transport::pair_connect(&addrs)
        .await
        .map_err(|e| PairError::Network {
            reason: format!("connect: {e}"),
        })?;

    link.send_msg(&PairMsg::HelloPair {
        version: core_crypto::PAIRING_PROTOCOL_VERSION,
        client_pubkey: identity.public_bytes(),
        alias: our_alias,
    })
    .await
    .map_err(|e| PairError::Network {
        reason: format!("send HelloPair: {e}"),
    })?;

    let ack = link.recv_msg().await.map_err(|e| PairError::Network {
        reason: format!("recv HelloAck: {e}"),
    })?;
    let (host_pub, spake2_msg_b, host_alias) = match ack {
        PairMsg::HelloAck {
            host_pubkey,
            spake2_msg_b,
            host_alias,
        } => (host_pubkey, spake2_msg_b, host_alias),
        PairMsg::Abort { reason } => return Err(PairError::HostAborted { reason }),
        other => {
            return Err(PairError::Protocol {
                reason: format!("expected HelloAck, got {other:?}"),
            })
        }
    };

    let mut session = core_crypto::PairingSession::new(
        core_crypto::PairingRole::Client,
        pin,
        identity.public_bytes(),
    );
    session.set_peer_pubkey(host_pub);
    let spake2_msg_a = session.start().map_err(|e| PairError::Protocol {
        reason: format!("spake start: {e}"),
    })?;
    link.send_msg(&PairMsg::Spake2Reply { spake2_msg_a })
        .await
        .map_err(|e| PairError::Network {
            reason: format!("send Spake2Reply: {e}"),
        })?;

    session
        .finish(&spake2_msg_b)
        .map_err(|e| PairError::Protocol {
            reason: format!("spake finish: {e}"),
        })?;

    let client_mac = session.our_mac().map_err(|e| PairError::Protocol {
        reason: format!("derive client mac: {e}"),
    })?;
    link.send_msg(&PairMsg::ClientConfirm { mac: client_mac })
        .await
        .map_err(|e| PairError::Network {
            reason: format!("send ClientConfirm: {e}"),
        })?;

    let confirm = link.recv_msg().await.map_err(|e| PairError::Network {
        reason: format!("recv HostConfirm: {e}"),
    })?;
    let host_mac = match confirm {
        PairMsg::HostConfirm { mac } => mac,
        PairMsg::Abort { reason } => {
            // Map the host's "PIN/identity mismatch" abort to PinMismatch
            // so the Kotlin UI can show a clean error.
            return if reason.contains("PIN/identity mismatch") {
                Err(PairError::PinMismatch)
            } else {
                Err(PairError::HostAborted { reason })
            };
        }
        other => {
            return Err(PairError::Protocol {
                reason: format!("expected HostConfirm, got {other:?}"),
            })
        }
    };
    session
        .verify_peer_mac(&host_mac)
        .map_err(|_| PairError::PinMismatch)?;

    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), link.close()).await;
    Ok(PairResult {
        host_pubkey_hex: hex::encode(host_pub),
        host_alias,
    })
}

// ---------------------------------------------------------------------------
// USB/IP export (Slice 3): Trireme shares a local USB device with Pharos. The
// Rust core owns the QUIC relay + USB/IP URB protocol; the Kotlin side performs
// the actual hardware transfers via `UsbDeviceConnection` (so no rusb/libusb on
// Android, and no permission pop-up when paired with a manifest device-filter).
// ---------------------------------------------------------------------------

/// One interface's class triple, as the v2 hello carries it.
#[derive(uniffi::Record, Clone)]
pub struct UsbInterfaceInfo {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// USB descriptor fields for the v2 exporter hello — everything a Windows
/// import bridge must echo into OP_REP_IMPORT (the Linux importer ignores
/// them). Built on the Kotlin side from `android.hardware.usb.UsbDevice`.
#[derive(uniffi::Record, Clone)]
pub struct UsbDeviceInfo {
    pub busnum: u32,
    pub devnum: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    /// BCD-coded device release (`bcdDevice`), e.g. 0x0100 for 1.00.
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub interfaces: Vec<UsbInterfaceInfo>,
}

impl From<&UsbDeviceInfo> for core_usbip::hello::DeviceDesc {
    fn from(i: &UsbDeviceInfo) -> Self {
        core_usbip::hello::DeviceDesc {
            busnum: i.busnum,
            devnum: i.devnum,
            id_vendor: i.vendor_id,
            id_product: i.product_id,
            bcd_device: i.bcd_device,
            device_class: i.device_class,
            device_subclass: i.device_subclass,
            device_protocol: i.device_protocol,
            configuration_value: i.configuration_value,
            num_configurations: i.num_configurations,
            interfaces: i
                .interfaces
                .iter()
                .map(|f| core_usbip::op::Interface {
                    class: f.class,
                    subclass: f.subclass,
                    protocol: f.protocol,
                })
                .collect(),
        }
    }
}

/// Result of one USB transfer performed by the Kotlin side.
#[derive(uniffi::Record)]
pub struct UsbTransferResult {
    /// 0 = success, negative errno on error (e.g. -110 timeout, -32 stall).
    pub status: i32,
    /// Bytes actually transferred (read for IN, written for OUT).
    pub actual_length: i32,
    /// IN payload (the bytes read); empty for an OUT transfer.
    pub data: Vec<u8>,
}

/// Hardware access for the exported device, implemented in Kotlin over
/// `UsbDeviceConnection`. The core calls this once per URB.
#[uniffi::export(callback_interface)]
pub trait UsbTransfer: Send + Sync {
    /// Perform one transfer. `ep == 0` → control (use the 8-byte `setup`);
    /// otherwise bulk/interrupt on endpoint `ep`. `dir_in` = device→host.
    /// `length` = max bytes to read (IN); `out_data` = payload to write (OUT).
    fn transfer(
        &self,
        ep: u8,
        dir_in: bool,
        setup: Vec<u8>,
        out_data: Vec<u8>,
        length: u32,
    ) -> UsbTransferResult;
    /// The relay ended (error message on failure).
    fn on_end(&self, error: Option<String>);
}

/// A handle to a running export. Call `cancel()` (from Kotlin, on toggle-off or
/// device unplug) to stop the relay promptly instead of waiting for Pharos to
/// drop the QUIC stream. Pair it with closing the `UsbDeviceConnection`, which
/// unblocks any in-flight transfer.
#[derive(uniffi::Object)]
pub struct UsbExportHandle {
    cancel: CancellationToken,
}

#[uniffi::export]
impl UsbExportHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Export a local USB device to Pharos over the standalone USB relay. `name` is
/// the friendly label shown on Pharos; `devid`/`speed` are the USB/IP attach
/// params (speed: 2=full, 3=high, 5=super). Returns a handle to cancel it; the
/// relay runs on its own thread and drives `sink` per URB until cancelled or
/// Pharos drops the stream.
#[uniffi::export]
pub fn start_usb_export(
    pharos: String,
    name: String,
    devid: u32,
    speed: u32,
    info: Option<UsbDeviceInfo>,
    sink: Box<dyn UsbTransfer>,
) -> std::sync::Arc<UsbExportHandle> {
    let cancel = CancellationToken::new();
    let handle = std::sync::Arc::new(UsbExportHandle {
        cancel: cancel.clone(),
    });
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                sink.on_end(Some(format!("runtime: {e}")));
                return;
            }
        };
        let _ = rt.block_on(usb_export_loop(
            &pharos,
            &name,
            devid,
            speed,
            info,
            sink.into(),
            cancel,
        ));
    });
    handle
}

/// Thin wrapper around [`usb_export_run`]: its only job is to guarantee that
/// `sink.on_end(...)` fires exactly once, on every exit path. Without this funnel
/// an early `?`-bail (bad address, QUIC connect failure, handshake write error)
/// would silently drop the Result in [`start_usb_export`], the Kotlin engine
/// would never see the export end, and its `active` map would keep a dead handle
/// — wedging the row at "Share enabled" forever, with no retry.
async fn usb_export_loop(
    pharos: &str,
    name: &str,
    devid: u32,
    speed: u32,
    info: Option<UsbDeviceInfo>,
    sink: std::sync::Arc<dyn UsbTransfer>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let result = usb_export_run(pharos, name, devid, speed, info, sink.clone(), cancel).await;
    sink.on_end(result.as_ref().err().map(|e| e.to_string()));
    result
}

async fn usb_export_run(
    pharos: &str,
    name: &str,
    devid: u32,
    speed: u32,
    info: Option<UsbDeviceInfo>,
    sink: std::sync::Arc<dyn UsbTransfer>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    use core_usbip::proto::*;
    use std::collections::HashSet;

    let addr = pharos.parse()?;
    let mut link = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        r = transport::usb_export_connect(addr) => r?,
    };
    // Hello: v2 (descriptor block — what a Windows import bridge needs) when
    // the importer advertised it AND the caller supplied the descriptors;
    // otherwise the legacy v1 triplet.
    let hello = core_usbip::hello::Hello {
        name: name.to_string(),
        devid,
        speed,
        desc: info.as_ref().map(Into::into),
    };
    let hello_bytes = if link.importer_v2() && hello.desc.is_some() {
        hello.to_bytes_v2()
    } else {
        hello.to_bytes_v1()
    };
    link.send.write_all(&hello_bytes).await?;

    // Pipeline URBs: each is serviced on a blocking thread so a pending
    // interrupt-IN (e.g. an HID gamepad waiting for input, or HCI events) never
    // blocks other URBs (control commands, OUT/rumble). A single writer task
    // serializes the replies back onto the stream.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let mut send = link.send;
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if send.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // Guard against the "stale RET_SUBMIT" disconnect.
    //
    // When vhci sends CMD_UNLINK (seqnum N) it immediately frees the URB for
    // the original CMD_SUBMIT (target_seqnum M). If our blocking transfer thread
    // is still running for M and later sends RET_SUBMIT(M), vhci sees a reply
    // for a URB it already freed → "cannot find a urb of seqnum M" → device
    // reset. Fix: track which CMD_SUBMITs are still in-flight; on CMD_UNLINK
    // mark the target seqnum so the blocking thread drops its reply.
    //
    // `in_flight`: seqnums of CMD_SUBMITs whose blocking task is still running.
    // `unlinked`:  subset of in_flight that received CMD_UNLINK while running.
    struct SeqState {
        in_flight: HashSet<u32>,
        unlinked: HashSet<u32>,
    }
    let seq_state = std::sync::Arc::new(std::sync::Mutex::new(SeqState {
        in_flight: HashSet::new(),
        unlinked: HashSet::new(),
    }));

    let mut recv = link.recv;
    let result = loop {
        let mut hdr = [0u8; HEADER_LEN];
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            r = recv.read_exact(&mut hdr) => if r.is_err() { break Ok(()) },
        }
        match command_of(&hdr) {
            USBIP_CMD_SUBMIT => {
                let cmd = CmdSubmit::parse(&hdr).expect("checked command");
                let mut out = vec![0u8; cmd.out_data_len()];
                if !out.is_empty() && recv.read_exact(&mut out).await.is_err() {
                    break Ok(());
                }
                if cmd.is_iso() {
                    // Drain the iso descriptor array (keeps the stream framed)
                    // and reject every packet cleanly. Iso servicing is deferred
                    // on Android — UsbDeviceConnection has no usable iso path.
                    let mut iso = vec![0u8; cmd.iso_descriptor_len()];
                    if !iso.is_empty() && recv.read_exact(&mut iso).await.is_err() {
                        break Ok(());
                    }
                    let _ = tx.send(RetSubmit::iso_reject(&cmd, -22).to_bytes()).await;
                    continue;
                }
                let seqnum = cmd.basic.seqnum;
                seq_state.lock().unwrap().in_flight.insert(seqnum);
                let sink = sink.clone();
                let txc = tx.clone();
                let ss = seq_state.clone();
                tokio::task::spawn_blocking(move || {
                    let mut ret = RetSubmit::for_request(&cmd);
                    let r = sink.transfer(
                        cmd.basic.ep as u8,
                        cmd.is_in(),
                        cmd.setup.to_vec(),
                        out,
                        cmd.transfer_buffer_length.max(0) as u32,
                    );
                    ret.status = r.status;
                    ret.actual_length = r.actual_length;
                    ret.data = r.data;
                    // Remove from in_flight; if it was CMD_UNLINKed while running,
                    // drop the reply — vhci already freed the URB slot.
                    let mut s = ss.lock().unwrap();
                    s.in_flight.remove(&seqnum);
                    let was_unlinked = s.unlinked.remove(&seqnum);
                    drop(s);
                    if !was_unlinked {
                        let _ = txc.blocking_send(ret.to_bytes());
                    }
                });
            }
            USBIP_CMD_UNLINK => {
                let u = CmdUnlink::parse(&hdr).expect("checked command");
                {
                    let mut s = seq_state.lock().unwrap();
                    // Only suppress if the submit is still in flight; if it
                    // already finished and sent RET_SUBMIT, the late CMD_UNLINK
                    // is harmless (vhci treats it as ENOENT and moves on).
                    if s.in_flight.contains(&u.unlink_seqnum) {
                        s.unlinked.insert(u.unlink_seqnum);
                    }
                }
                let _ = tx.send(ret_unlink(&u, 0)).await;
            }
            other => break Err(anyhow::anyhow!("unexpected USB/IP command {other}")),
        }
    };
    drop(tx);
    let _ = writer.await;
    result
}
