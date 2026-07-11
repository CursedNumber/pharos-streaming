// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.stream

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import java.util.concurrent.atomic.AtomicInteger
import no.aasle.trireme.ControllerManager
import no.aasle.trireme.GyroTelemetry
import no.aasle.trireme.OpusAudioPlayer
import no.aasle.trireme.ServerEntry
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.core_ffi.AppCatalogSink
import uniffi.core_ffi.AppInfo
import uniffi.core_ffi.AudioSink
import uniffi.core_ffi.ClipboardSink
import uniffi.core_ffi.FrameSink
import uniffi.core_ffi.GamepadInput
import uniffi.core_ffi.GyroSink
import uniffi.core_ffi.StreamMode
import uniffi.core_ffi.startVideoStream

private const val TAG = "TriremeHub"

// Bitrate cap auto-applied when the slider is untouched and the active network is
// wired Ethernet (see [StreamHub.wiredAutoMaxKbps]). Matches a slider stop so the
// Settings caption can name it.
private const val WIRED_AUTO_MAX_KBPS = 150_000

// Last-deliberately-used input source, for pad-vs-pointer arbitration (mirrors the
// desktop client's gamepad.rs/play.rs SRC_KBDMOUSE/SRC_GAMEPAD). The host turns
// both touch pointer motion AND controller gyro into MOUSE events, so a touch (or
// gyro) flood fights the virtual gamepad for the game's "active device" focus and
// the game starts ignoring the pad. We forward bare touch pointer motion ONLY while
// TOUCH is the active source: real pad use (a button or a past-deadzone stick)
// claims [SRC_GAMEPAD] and a deliberate touch gesture claims [SRC_TOUCH]. A pad
// merely connecting does not claim, and bare motion never switches the source
// (idle motion can't steal focus back).
private const val SRC_TOUCH = 0
private const val SRC_GAMEPAD = 1

/// One host's app catalog, surfaced for the pooled Apps pane.
data class HostApps(val hostId: String, val hostName: String, val apps: List<AppInfo>)

/// Lifecycle of a single host session, as the UI needs to render it.
/// `Preempted` = the host handed our single streaming slot to another device, so
/// we YIELDED (no auto-reconnect); the user reclaims by re-selecting the host.
enum class ConnStatus { Connecting, Live, Reconnecting, Failed, Disconnected, Preempted }

/// One row in the Hosts pane: a known/connected session and whether it is the
/// one currently bound to the decoder/Surface.
data class HostConn(val hostId: String, val name: String, val status: ConnStatus, val active: Boolean)

/**
 * Process-lifetime hub for every Pharos host session.
 *
 * Multi-host model (DECIDED — see the redesign contract §"Multi-host model"):
 * each connected host is ONE [startVideoStream] (a full QUIC session). Only the
 * **active** session's frames feed the single [HevcDecoder]/Surface; inactive
 * sessions stay fully connected so their app catalog stays live and a launch can
 * be routed instantly — but their `FrameSink.onFrame` bytes are dropped. This
 * costs inactive-stream bandwidth on the LAN (a few couch hosts is fine); a
 * future catalog-only FFI mode would remove it.
 *
 * Survives navbar navigation AND activity recreate: the Surface is re-attachable
 * across destroy→recreate, and a session that died while we were away reconnects
 * with backoff. The host emits a 1s IDR cadence + repeatSPSPPS, so a fresh re-dial
 * resyncs picture in ≤1s — this is what makes reconnect "graceful".
 *
 * Audio ([OpusAudioPlayer]) and [ControllerManager] are owned per-active-session
 * here, so they persist while the navbar is open and survive a switch.
 *
 * Thread-safety: all mutable state is guarded by [lock]; StateFlow writes happen
 * on the main thread (FFI callbacks arrive off-main and are marshalled via [main]).
 */
object StreamHub {
    // --- Process context, set once by TriremeApp.init() ---
    private lateinit var appContext: Context
    private lateinit var identityPath: String
    @Volatile private var initialized = false

    // Main-thread marshaller for the off-main FFI callbacks (FrameSink/AudioSink/
    // AppCatalogSink fire on the QUIC client's threads).
    private val main = Handler(Looper.getMainLooper())

    // --- Clipboard sync (host ⇄ client) ---
    // The OS clipboard + the two last-seen texts that break the copy↔paste echo:
    // `lastAppliedClip` is what the HOST pushed to us (skip re-sending it); the
    // primary-clip listener only fires while the app has focus (Android's own
    // restriction), which is exactly when a streaming session is foregrounded.
    private var clipboard: ClipboardManager? = null
    @Volatile private var lastAppliedClip: String? = null
    @Volatile private var lastSentClip: String? = null
    private val clipListener = ClipboardManager.OnPrimaryClipChangedListener { onLocalClipChanged() }

    // The single lock guarding [sessions], the live Surface/caps, and the decoder.
    private val lock = Any()

    // --- Public StateFlows (Compose collectAsState) ---
    private val _connections = MutableStateFlow<List<HostConn>>(emptyList())
    val connections: StateFlow<List<HostConn>> = _connections.asStateFlow()

    private val _pooledApps = MutableStateFlow<List<HostApps>>(emptyList())
    val pooledApps: StateFlow<List<HostApps>> = _pooledApps.asStateFlow()

    // Per-app last-used time (epoch millis, keyed by app id), persisted across
    // launches in `trireme.app_recency`. The Apps grid floats running apps first,
    // then most-recently-used — so an app you just played stays near the top even
    // after it stops, instead of dropping back into catalog order. Updated on
    // launch + switch-to; seeded from prefs in [init].
    private val _appRecency = MutableStateFlow<Map<String, Long>>(emptyMap())
    val appRecency: StateFlow<Map<String, Long>> = _appRecency.asStateFlow()

    private val _activeHostId = MutableStateFlow<String?>(null)
    val activeHostId: StateFlow<String?> = _activeHostId.asStateFlow()

    private val _activeStatus = MutableStateFlow(ConnStatus.Disconnected)
    val activeStatus: StateFlow<ConnStatus> = _activeStatus.asStateFlow()

    // --- Live render target, owned by the shell and handed in here ---
    @Volatile private var surface: Surface? = null
    @Volatile private var caps: DisplayCaps = DisplayCaps.NONE

    // Panel refresh capability (from the shell's configureRefresh) so UI can gate
    // refresh-dependent options — e.g. hide VRR on a fixed-refresh panel. Set once a
    // Surface is available, which is at shell start, BEFORE any stream connects.
    private val _displayCaps = MutableStateFlow(DisplayCaps.NONE)
    val displayCaps: StateFlow<DisplayCaps> = _displayCaps.asStateFlow()

    // Live decode stats for the on-screen debug HUD (debug variant only). Fed by
    // the active decoder's onStats callback ~1/s; null when no decoder is running.
    // Only WIRED under BuildConfig.DEBUG (see startDecoderLocked), so release never
    // pays for it and the flow stays null.
    private val _debugMetrics = MutableStateFlow<DecoderStats?>(null)
    val debugMetrics: StateFlow<DecoderStats?> = _debugMetrics.asStateFlow()

    // User toggle for the on-screen debug HUD overlay (debug builds only — the
    // metrics flow is BuildConfig.DEBUG-gated). Default ON to preserve the current
    // behaviour; Settings flips it live and the shell observes this flow. Seeded
    // from prefs in [init].
    private val _debugHud = MutableStateFlow(true)
    val debugHud: StateFlow<Boolean> = _debugHud.asStateFlow()

    // True video aspect ratio (width/height) of the active stream, from the decoder's
    // output format. The shell letterboxes to THIS instead of a hardcoded 16:9, so a
    // 20:9 (2712x1220) stream fills a 20:9 panel. Defaults to 16:9 until a frame's
    // format lands; wired in release (not debug-gated).
    private val _videoAspect = MutableStateFlow(16f / 9f)
    val videoAspect: StateFlow<Float> = _videoAspect.asStateFlow()

    // Host link-health: true while the host reports it's shedding video frames
    // (the DG_LINK_HEALTH heartbeat, ~2×/s). Drives the subtle yellow "degraded"
    // indicator. [applyLinkHealth] adds hysteresis — it holds true for ~1.5s after
    // the last degraded beat so an intermittent single-window drop doesn't flicker.
    private val _linkDegraded = MutableStateFlow(false)
    val linkDegraded: StateFlow<Boolean> = _linkDegraded.asStateFlow()
    private val linkDegradedHoldMs = 1500L
    private val clearLinkDegraded = Runnable { _linkDegraded.value = false }

    // Global pace preset chosen in Settings (null = use the per-host ServerEntry.pace).
    // Applied live to the active decoder via [setPace]; seeded from prefs in [init].
    @Volatile private var paceOverride: PaceMode? = null

    // Stream-time gyro opt-in chosen in Settings. Default false: the host then
    // skips its per-frame gyro drain entirely (and the IMU reader thread idles)
    // because nothing on the wire asks for it. Flipped live via [setGyroEnabled];
    // takes effect on the NEXT dial (the v2 auth handshake sends the flag once).
    @Volatile private var gyroEnabled: Boolean = false
    // Touch-pointer mode. true = absolute (tap-to-position), false = trackpad
    // (relative drag → PointerRel). Seeded in [init]; flipped live via
    // [setTouchAbsolute] (takes effect immediately, no re-dial).
    @Volatile private var touchAbsolute: Boolean = true
    // Which device most recently DELIBERATELY drove input (SRC_TOUCH / SRC_GAMEPAD).
    // Written from the input threads (ControllerManager) and the main thread (touch),
    // read on every pointer send — an atomic keeps that lock-free and consistent.
    // Starts on TOUCH so a touch-only session works before any pad activity.
    private val inputSource = AtomicInteger(SRC_TOUCH)
    // Controller-handling master switch (Settings / Controllers pane). Default
    // false: pads are forwarded RAW (host default profile, no gyro/swaps). When
    // on, ControllerManager applies the per-controller profile. Seeded in [init];
    // flipped live via [setControllerHandling], which rebuilds the manager.
    @Volatile private var controllerHandling: Boolean = false
    // Resolution range chosen in Settings (Phase 2). Coded heights in px;
    // `0` = Auto target / no floor. Sent to the host LIVE via the GamepadInput
    // handle (DG_CLIENT_RESPREF); the host adapts the ladder within [floor, target]
    // and re-asserts on each (re)connect. Seeded from prefs in [init].
    @Volatile private var resTargetH: Int = 0
    @Volatile private var resFloorH: Int = 0
    // Bitrate range chosen in Settings (Phase 3) — the PRIMARY auto-recover lever.
    // kbps; `min=0` = host floor (2 Mbps), `max=0` = uncapped (host owns the top).
    // Sent to the host LIVE via the GamepadInput handle (DG_CLIENT_BITRATEPREF); the
    // host adapts bitrate within [min, max] and re-asserts on each (re)connect.
    // Seeded from prefs in [init].
    @Volatile private var bitrateMinKbps: Int = 0
    @Volatile private var bitrateMaxKbps: Int = 0
    // Framerate range chosen in Settings (Phase 3), derived from the panel refresh.
    // fps; `min=0` = host floor (24), `max=0` = Auto (panel refresh). Sent to the
    // host LIVE via DG_CLIENT_FPSPREF; caps the encode pace + adaptation floor (and,
    // for direct-launched apps, the game render). Seeded from prefs in [init].
    @Volatile private var fpsMin: Int = 0
    @Volatile private var fpsMax: Int = 0
    // Last-known coded stream size, from the decoder's INFO_OUTPUT_FORMAT_CHANGED
    // (via onGeometry). The next decoder configures at this size directly, so a
    // geometry-triggered rebuild converges in one step. Cleared on host switch —
    // the new host's stream size is unrelated.
    @Volatile private var lastVideoSize: Pair<Int, Int>? = null
    // The single decoder. Exists only while there is a Surface AND a Live active
    // session; recreated on (re)attach, destroyed on surface-destroyed/switch.
    @Volatile private var decoder: HevcDecoder? = null
    // The stream's video MIME, sniffed from the first access unit of the active
    // session (HEVC vs AV1). Remembered so a reconnect builds the right decoder
    // directly; `sniffed` re-arms per decoder so a host that switched codec is
    // re-detected. Default HEVC keeps the existing path byte-for-byte unchanged.
    @Volatile private var streamMime: String = "video/hevc"
    @Volatile private var sniffed: Boolean = false

    // --- Per-host session bookkeeping ---
    /// One connected host. `gamepad` is the FFI back-channel; `active` flips the
    /// frame routing; `userStopped` distinguishes a deliberate disconnect (no
    /// reconnect) from an unexpected `onEnd` (reconnect with backoff).
    private class Session(
        val entry: ServerEntry,
        val hostId: String,
        val name: String,
        val pace: PaceMode,
    ) {
        @Volatile var gamepad: GamepadInput? = null
        @Volatile var status: ConnStatus = ConnStatus.Connecting
        @Volatile var active: Boolean = false
        @Volatile var apps: List<AppInfo> = emptyList()
        // Kind of the app this client last launched/switched-to on this host
        // (core_proto::apps::app_kind: 1=game, 2=desktop, 3=media, else app).
        // Drives desktop-only touch gestures (long-press right-click, double-tap
        // drag) — those are enabled only for a desktop stream. Null until the
        // user launches something (fresh connect / idle → treated as non-desktop).
        @Volatile var launchedKind: Int? = null
        @Volatile var userStopped: Boolean = false
        @Volatile var backoffMs: Long = INITIAL_BACKOFF_MS
        // Generation guards stale callbacks: a re-dial bumps this, so a late
        // onEnd from a torn-down GamepadInput is ignored.
        @Volatile var generation: Int = 0
    }

    private val sessions = LinkedHashMap<String, Session>() // guarded by [lock]

    // Audio + controllers follow the ACTIVE session.
    @Volatile private var audio: OpusAudioPlayer? = null
    @Volatile private var controllers: ControllerManager? = null

    /// The GamepadInput handle `controllers` is currently bound to. A reconnect /
    /// re-dial swaps `Session.gamepad` for a fresh handle; if the manager isn't
    /// rebuilt against it, its sink.button/axis calls go to the dead old handle and
    /// vanish (onKey returns true, but nothing reaches the host). Tracked so
    /// [startAudioAndControllersLocked] can detect the change by reference identity.
    private var controllersGamepad: GamepadInput? = null

    // ============================================================
    // Lifecycle
    // ============================================================

    /** Idempotent; stores the app context + identity path. Call from [TriremeApp]. */
    fun init(appContext: Context) {
        if (initialized) return
        synchronized(lock) {
            if (initialized) return
            this.appContext = appContext.applicationContext
            this.identityPath = this.appContext.filesDir.resolve("identity.toml").absolutePath
            paceOverride = readPacePref(this.appContext)
            gyroEnabled = readGyroEnabledPref(this.appContext)
            touchAbsolute = readTouchAbsolutePref(this.appContext)
            controllerHandling = readControllerHandlingPref(this.appContext)
            val sp = this.appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            resTargetH = sp.getInt("res_target_h", 0)
            resFloorH = sp.getInt("res_floor_h", 0)
            bitrateMinKbps = sp.getInt("bitrate_min_kbps", 0)
            bitrateMaxKbps = sp.getInt("bitrate_max_kbps", 0)
            fpsMin = sp.getInt("fps_min", 0)
            fpsMax = sp.getInt("fps_max", 0)
            _debugHud.value = sp.getBoolean("debug_hud", true)
            // Seed the app-recency map (appId → epoch millis) from its prefs file.
            val rp = this.appContext.getSharedPreferences("trireme.app_recency", Context.MODE_PRIVATE)
            _appRecency.value = rp.all.entries
                .mapNotNull { (k, v) -> (v as? Long)?.let { k to it } }
                .toMap()
            // Clipboard: mirror the host clipboard into ours (host→client) and push
            // our local copies to the host (client→host). The listener fires only
            // while the app is focused (Android policy), i.e. during a live session.
            clipboard = runCatching {
                (this.appContext.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager)
                    ?.also { it.addPrimaryClipChangedListener(clipListener) }
            }.getOrNull()
            initialized = true
        }
        Log.i(TAG, "StreamHub initialized (identity=$identityPath, gyroEnabled=$gyroEnabled)")
    }

    /** Apply text the host copied to our OS clipboard (host→client). Records it so
     *  the resulting primary-clip-changed callback isn't echoed back to the host.
     *  Empty text = the host cleared its clipboard. Main thread. */
    private fun applyHostClipboard(text: String) {
        val cb = clipboard ?: return
        Log.i(TAG, "clipboard host→client: applying ${text.length} chars")
        lastAppliedClip = text
        // The host clipboard just changed ours; forget the last text WE sent so the
        // user re-copying an earlier value still propagates it back to the host.
        lastSentClip = null
        runCatching {
            if (text.isEmpty()) {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                    cb.clearPrimaryClip()
                } else {
                    cb.setPrimaryClip(ClipData.newPlainText("", ""))
                }
            } else {
                cb.setPrimaryClip(ClipData.newPlainText("pharos", text))
            }
        }.onFailure { Log.w(TAG, "applyHostClipboard failed: ${it.message}") }
    }

    /** Our OS clipboard changed: push the new text to the host (client→host) unless
     *  it's the text the host just gave us (echo) or one we already sent. */
    private fun onLocalClipChanged() {
        val cb = clipboard ?: return
        val text = runCatching {
            cb.primaryClip
                ?.takeIf { it.itemCount > 0 }
                ?.getItemAt(0)
                ?.coerceToText(appContext)
                ?.toString()
        }.getOrNull() ?: return
        if (text == lastAppliedClip || text == lastSentClip) return
        lastSentClip = text
        Log.i(TAG, "clipboard client→host: sending ${text.length} chars")
        activeGamepad()?.setClipboard(text)
    }

    /** Re-read the OS clipboard and push it to the host when the stream regains
     *  focus. The change listener only fires while Trireme is focused, so a copy the
     *  user made in another app (with Trireme backgrounded) would otherwise be
     *  missed — reading on focus-gain covers the "copy elsewhere, return to stream,
     *  paste on host" flow. Echo-guarded via [onLocalClipChanged]. */
    fun onClipboardFocusSync() = onLocalClipChanged()

    /** Record that [appId] was just launched/switched-to: stamp it now + persist,
     *  so the Apps grid floats recently-used apps near the top across sessions. */
    private fun recordAppUse(appId: String) {
        val now = System.currentTimeMillis()
        _appRecency.value = _appRecency.value + (appId to now)
        runCatching {
            appContext.getSharedPreferences("trireme.app_recency", Context.MODE_PRIVATE)
                .edit()
                .putLong(appId, now)
                .apply()
        }
    }

    // ============================================================
    // Host connection management
    // ============================================================

    /**
     * Dial [entry] if it isn't already connected; otherwise just (maybe) promote it
     * to the active session. Safe to call repeatedly from the Hosts pane.
     */
    fun connect(entry: ServerEntry, makeActive: Boolean = true) {
        synchronized(lock) {
            val existing = sessions[entry.id]
            if (existing != null) {
                // Already have a session — reuse it. If it had been stopped/failed,
                // re-dial; otherwise just switch active.
                existing.userStopped = false
                if (existing.status == ConnStatus.Disconnected ||
                    existing.status == ConnStatus.Failed ||
                    existing.status == ConnStatus.Preempted
                ) {
                    // Re-selecting a yielded (Preempted) host is the deliberate
                    // "reclaim the slot" action — re-dial, which pre-empts whoever
                    // currently holds it. The most-recently-chosen device wins.
                    dialLocked(existing)
                }
                if (makeActive) setActiveLocked(entry.id)
                publishLocked()
                return
            }
            val session = Session(
                entry = entry,
                hostId = entry.id,
                name = entry.friendlyName(),
                pace = paceOf(entry),
            )
            sessions[entry.id] = session
            dialLocked(session)
            if (makeActive) setActiveLocked(entry.id)
            publishLocked()
        }
    }

    /** Switch which session feeds the decoder/Surface. No-op if unknown. */
    fun setActive(hostId: String) {
        synchronized(lock) {
            if (!sessions.containsKey(hostId)) return
            setActiveLocked(hostId)
            publishLocked()
        }
    }

    /** User-initiated stop of ONE session (won't reconnect). Removes it. */
    fun disconnect(hostId: String) {
        synchronized(lock) {
            val s = sessions.remove(hostId) ?: return
            s.userStopped = true
            s.generation++
            teardownSessionLocked(s)
            if (_activeHostId.value == hostId) {
                // The active session went away — drop the decoder + audio/controllers
                // and promote any other live session, or fall idle.
                detachActiveLocked()
                val next = sessions.values.firstOrNull { it.status == ConnStatus.Live }
                    ?: sessions.values.firstOrNull()
                if (next != null) setActiveLocked(next.hostId)
                else postActiveStatus(ConnStatus.Disconnected)
            }
            publishLocked()
        }
    }

    /**
     * Launch [appId] on [hostId]: route to that host's [GamepadInput] and make it
     * the active stream (so the launched app shows up immediately). Connects first
     * if the host isn't dialed yet.
     */
    fun launchApp(hostId: String, appId: String) {
        synchronized(lock) {
            val s = sessions[hostId]
            if (s == null) {
                Log.w(TAG, "launchApp: no session for $hostId")
                return
            }
            Log.i(TAG, "launchApp '$appId' on ${s.name} (gp=${s.gamepad != null})")
            s.gamepad?.launchApp(appId)
            s.launchedKind = s.apps.find { it.id == appId }?.kind?.toInt()
            recordAppUse(appId)
            setActiveLocked(hostId)
            publishLocked()
        }
    }

    /**
     * Switch to (raise + focus) a running app on [hostId] — bring it to the front
     * of the host's window stack. Makes that host the active stream too, so the
     * app shows immediately.
     */
    fun switchTo(hostId: String, appId: String) {
        synchronized(lock) {
            val s = sessions[hostId] ?: return
            Log.i(TAG, "switchTo '$appId' on ${s.name}")
            s.gamepad?.switchTo(appId)
            s.launchedKind = s.apps.find { it.id == appId }?.kind?.toInt()
            recordAppUse(appId)
            setActiveLocked(hostId)
            publishLocked()
        }
    }

    /**
     * End a running app on [hostId] — the host kills its whole process tree
     * (including stragglers like a crash reporter). The catalog re-pushes shortly
     * after with the app no longer marked running.
     */
    fun endProcess(hostId: String, appId: String) {
        synchronized(lock) {
            val s = sessions[hostId] ?: return
            Log.i(TAG, "endProcess '$appId' on ${s.name}")
            s.gamepad?.endProcess(appId)
        }
    }

    /** Stop ALL sessions (real app exit). Tears down decoder + audio + controllers. */
    fun shutdown() {
        synchronized(lock) {
            for (s in sessions.values) {
                s.userStopped = true
                s.generation++
                teardownSessionLocked(s)
            }
            sessions.clear()
            detachActiveLocked()
            postActive(null, ConnStatus.Disconnected)
            publishLocked()
        }
        Log.i(TAG, "StreamHub shutdown: all sessions stopped")
    }

    // ============================================================
    // Surface lifecycle (the ACTIVE session renders here)
    // ============================================================

    /**
     * The shell hands us a fresh Surface + the [DisplayCaps] it computed from its
     * Window (refresh/VRR config needs a Window, so the shell owns
     * `configureRefresh()`). We bind it to the active Live session's decoder, and
     * if the active session had died while we were away, reconnect it.
     */
    fun onSurfaceAvailable(surface: Surface, caps: DisplayCaps) {
        synchronized(lock) {
            this.surface = surface
            this.caps = caps
            _displayCaps.value = caps
            val active = activeSessionLocked()
            if (active == null) {
                Log.i(TAG, "surface available, no active session yet")
                return
            }
            when {
                // Connection up (Live, or Connecting awaiting first frame): re-attach
                // the render + playback path that onSurfaceDestroyed tore down (decoder
                // AND audio/controllers — we stop all three while there is no Surface so
                // audio doesn't play in the background behind a full-screen sub-activity).
                active.isConnected() -> {
                    startDecoderLocked()
                    startAudioAndControllersLocked(active)
                }
                // Session died while the activity was gone — re-dial; the decoder
                // re-attaches once the connection comes back up.
                active.status == ConnStatus.Disconnected || active.status == ConnStatus.Failed -> {
                    active.userStopped = false
                    dialLocked(active)
                }
                else -> { /* Reconnecting: backoff re-dial is already in flight */ }
            }
            publishLocked()
        }
    }

    /** Surface size/identity change — rebind the decoder to the new Surface. */
    fun onSurfaceChanged(surface: Surface) {
        synchronized(lock) {
            if (this.surface === surface && decoder != null) return
            this.surface = surface
            // Recreate the decoder against the new Surface if the active session's
            // connection is up (stopDecoderLocked joins the old decode thread first,
            // so only one codec is ever bound to the Surface).
            stopDecoderLocked()
            if (activeSessionLocked()?.isConnected() == true) startDecoderLocked()
        }
    }

    /** The Surface is gone (activity backgrounded, or a full-screen sub-activity
     *  like Controllers/USB-export is on top). Drop the decoder AND the audio +
     *  controllers — with no Surface there is no video, so leaving the AudioTrack
     *  and opus-decode thread running would play sound in the background and hold
     *  resources. The QUIC sessions stay connected (catalog + launch), so
     *  reconnect is just a re-attach in [onSurfaceAvailable] (audio/controllers
     *  are recreated there when the active session is Live). */
    fun onSurfaceDestroyed() {
        synchronized(lock) {
            stopDecoderLocked()
            stopAudioAndControllersLocked()
            surface = null
        }
    }

    // ============================================================
    // Input passthrough
    // ============================================================

    /** The active session's [GamepadInput] (the shell forwards keyboard keys here;
     *  [ControllerManager] already holds it for gamepad/gyro). Null when idle. */
    fun activeGamepad(): GamepadInput? = activeSessionLocked()?.gamepad

    /** Ask the host for an immediate keyframe (IDR) so a corrupt decoder resyncs in
     *  ~1 RTT instead of waiting for the next periodic IDR. Called from the decode
     *  thread on a MediaCodec error; cheap, non-blocking, best-effort. */
    fun requestKeyframe() {
        runCatching { activeGamepad()?.requestKeyframe() }
    }

    /** Apply a host link-health beat with hysteresis (main thread): light the
     *  indicator on a degraded beat and hold it [linkDegradedHoldMs] after the last
     *  one, so intermittent drops don't flicker. A clean beat just lets the timer
     *  expire — absence of "degraded" is the recovery signal. */
    private fun applyLinkHealth(degraded: Boolean) {
        if (degraded) {
            _linkDegraded.value = true
            main.removeCallbacks(clearLinkDegraded)
            main.postDelayed(clearLinkDegraded, linkDegradedHoldMs)
        }
    }

    /**
     * Forward a key event to the active session's [ControllerManager] (gamepad
     * buttons / digital triggers / d-pad). The shell calls this when the navbar is
     * closed so controllers keep working across navbar nav and host switches.
     * @return true if a controller consumed it.
     */
    fun dispatchControllerKey(event: KeyEvent): Boolean = controllers?.onKey(event) == true

    /**
     * Forward a generic motion event (joystick sticks/triggers/hat) to the active
     * session's [ControllerManager]. @return true if a controller consumed it.
     */
    fun dispatchControllerMotion(event: MotionEvent): Boolean =
        controllers?.onMotion(event) == true

    // --- Pad-vs-pointer input-source arbitration (see SRC_TOUCH/SRC_GAMEPAD) ---

    /** Real controller activity (a button, or a stick past the deadzone) claims the
     *  gamepad as the active source, muting stray touch/gyro pointer motion so it
     *  can't fight the pad for the game's active-device focus. Called by
     *  [ControllerManager]; a pad merely connecting must NOT call this. */
    fun claimGamepadSource() {
        inputSource.set(SRC_GAMEPAD)
    }

    /** A deliberate touch gesture (a finger going down on the stream) claims touch
     *  as the active source, so its pointer motion is forwarded again after the pad
     *  had muted it. Called by MainActivity at gesture start. */
    fun claimTouchSource() {
        inputSource.set(SRC_TOUCH)
    }

    // --- Touch → host pointer (MainActivity drives these from onTouchEvent) ---
    // Always route through activeGamepad() per-call: a reconnect swaps the
    // session's GamepadInput for a fresh handle, and a cached one silently drops.

    /** Absolute pointer position, normalised 0..=1 over the video content rect.
     *  Gated on TOUCH being the active source (bare motion never claims), so a pad
     *  in use isn't fought by a stray or gyro-driven cursor. */
    fun pointerAbs(x: Float, y: Float) {
        if (inputSource.get() != SRC_TOUCH) return
        runCatching { activeGamepad()?.pointerAbs(x, y) }
    }

    /** Relative pointer motion in pixels (trackpad drag). Gated like [pointerAbs]. */
    fun pointerRel(dx: Float, dy: Float) {
        if (inputSource.get() != SRC_TOUCH) return
        runCatching { activeGamepad()?.pointerRel(dx, dy) }
    }

    /** Pointer button (0=Left, 1=Right, 2=Middle). Buttons (incl. releases) are NOT
     *  gated — the deliberate gesture already claimed touch at DOWN, and a release
     *  must always reach the host so a held button never sticks. */
    fun pointerButton(button: Int, pressed: Boolean) {
        runCatching { activeGamepad()?.pointerButton(button.toUByte(), pressed) }
    }

    /** Wheel scroll in notches (dy>0 = up, dx>0 = right). A deliberate action, so it
     *  claims touch (matching the desktop client, where scroll claims kbd/mouse). */
    fun pointerScroll(dx: Float, dy: Float) {
        inputSource.set(SRC_TOUCH)
        runCatching { activeGamepad()?.scroll(dx, dy) }
    }

    /** True while a live session can receive pointer input (gates touch capture). */
    fun hasActiveSession(): Boolean = activeGamepad() != null

    /** Touch-pointer mode: true = absolute (tap-to-position), false = trackpad
     *  (relative drag + PointerRel). Read at gesture time by MainActivity. */
    fun isTouchAbsolute(): Boolean = touchAbsolute

    /** True when the active stream is the host DESKTOP / mirror (the last app this
     *  client launched/switched-to on the active host was desktop-kind). Gates the
     *  desktop-only trackpad gestures (long-press → right-click, double-tap-hold →
     *  drag) so they can't fire mid-game. False on a fresh/idle session until a
     *  desktop app is launched. */
    fun isDesktopStream(): Boolean =
        synchronized(lock) { activeSessionLocked()?.launchedKind == 2 }

    /** Persist + apply the touch-pointer mode. Takes effect immediately (it only
     *  changes how the Activity maps touches — no re-dial). Safe on any thread. */
    fun setTouchAbsolute(on: Boolean) {
        touchAbsolute = on
        appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .edit()
            .putBoolean("touch_absolute", on)
            .apply()
        Log.i(TAG, "touch pointer mode = ${if (on) "absolute" else "trackpad"}")
    }

    private fun readTouchAbsolutePref(ctx: Context): Boolean =
        ctx.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .getBoolean("touch_absolute", true)

    /**
     * Live slot index of the controller with this VID:PID on the active session,
     * or null if none is attached locally. Lets the panel's swap-toggle row relay
     * a tuning push to the host's CURRENT slot so the swap takes effect on the
     * live pad immediately (host persists the profile either way, so unattached
     * controllers still pick the new swap up on next connect).
     */
    fun slotForController(vendor: Int, product: Int): Int? =
        controllers?.slotOf(vendor, product)

    // ============================================================
    // Internals — all callers hold [lock] unless noted
    // ============================================================

    private fun activeSessionLocked(): Session? =
        _activeHostId.value?.let { sessions[it] }

    /// A session whose QUIC connection is up (gamepad handle live) and is either
    /// awaiting its first frame (Connecting) or already showing media (Live). The
    /// decoder/audio are bound for both — they just wait on an empty queue until a
    /// frame arrives — so frame routing and the first-frame Live promotion can work.
    private fun Session.isConnected(): Boolean =
        gamepad != null && (status == ConnStatus.Connecting || status == ConnStatus.Live)

    /// Open the QUIC session for [s]. The FrameSink only feeds the decoder when
    /// this session is active; inactive sessions drop frames (bandwidth tradeoff
    /// documented at the top of this file). Audio/catalog sinks stay wired so the
    /// pool + a switch are instant.
    private fun dialLocked(s: Session) {
        // Bump generation so any in-flight onEnd from a previous dial is ignored.
        val gen = ++s.generation
        s.status = if (s.status == ConnStatus.Reconnecting) ConnStatus.Reconnecting else ConnStatus.Connecting
        if (s.active) postActiveStatus(s.status)

        val frameSink = object : FrameSink {
            override fun onFrame(data: ByteArray) {
                // Only the active session feeds the single decoder; everyone else
                // is connected purely for catalog + launch, so drop their frames.
                if (s.active) {
                    // Auto-negotiate the codec from the first access unit (HEVC
                    // Annex-B start code vs AV1 OBU temporal delimiter). If it
                    // differs from the decoder we built (always HEVC by default),
                    // swap to the matching one on the main thread.
                    if (!sniffed) {
                        sniffed = true
                        val want = sniffCodecMime(data)
                        if (want != streamMime || decoder?.mime != want) {
                            streamMime = want
                            main.post { swapDecoderMimeIfNeeded(want) }
                        }
                    }
                    // Feed only a decoder whose MIME matches the stream — never
                    // hand AV1 bytes to a HEVC decoder while a swap is in flight.
                    val dec = decoder
                    if (dec != null && dec.mime == streamMime) dec.feed(data)
                }
                // First real frame on the active session promotes it to Live — the
                // badge then reflects actual media, not just a spawned QUIC thread.
                if (s.active && s.status == ConnStatus.Connecting) {
                    main.post { markActiveLive(s, gen) }
                }
            }

            override fun onEnd(error: String?) {
                main.post { handleEnd(s, gen, error) }
            }

            override fun onLinkHealth(degraded: Boolean) {
                // Only the active session drives the indicator — a pooled background
                // host must not flip it (mirrors the s.active gate on onFrame/onPacket).
                // Hysteresis (hold ~1.5s) lives in applyLinkHealth on the main thread.
                if (s.active) main.post { applyLinkHealth(degraded) }
            }
        }
        val audioSink = object : AudioSink {
            override fun onPacket(opus: ByteArray) {
                // Audio also follows the active session only.
                if (s.active) audio?.feed(opus)
            }
        }
        val catalogSink = object : AppCatalogSink {
            override fun onCatalog(apps: List<AppInfo>) {
                main.post { onCatalog(s, apps) }
            }
        }
        // Host clipboard → our OS clipboard, but only for the ACTIVE session (like
        // audio/frames): a backgrounded/mirror session must not hijack the clipboard.
        val clipboardSink = object : ClipboardSink {
            override fun onClipboard(text: String) {
                if (s.active) main.post { applyHostClipboard(text) }
            }
        }

        val addr = s.entry.addressLabel()
        val expectedPubkey = s.entry.hostPubkey
        Log.i(TAG, "dialing ${s.name} ($addr, pace=${s.pace}, pinned=${expectedPubkey != null})")
        // Gyro samples relayed from the host's IMU reader land here on the QUIC
        // datagram thread (one packet per IMU sync). We publish to GyroTelemetry
        // unconditionally — the calibration dialog and any future controller-pane
        // visualizer read from there. We do NOT gate on `s.active`: the dialog
        // can be open while the user has briefly demoted this session, and a
        // stale GyroTelemetry would just show "Waiting" forever; the host only
        // sends gyro for slot 0 anyway, so an inactive session's packets are
        // already throttled at the host. Replaces an earlier null that left the
        // dialog stuck on "Waiting for the host gyro…" indefinitely whenever
        // ControllerManager's own short-lived session couldn't open (e.g.
        // when this main session already held the single-streamer slot).
        // Stream-time gyro opt-in: when off, pass `wants_gyro=false` so the
        // host skips IMU drain entirely AND pass a null GyroSink so the QUIC
        // datagram thread doesn't even map host samples to GyroTelemetry —
        // belt-and-braces against a v1 host that didn't honor the flag. When
        // on, the real sink publishes to GyroTelemetry (calibration dialog +
        // any future visualizer read from there).
        val wantsGyro = gyroEnabled
        val gyroSink: GyroSink? = if (wantsGyro) {
            object : GyroSink {
                override fun onSample(gx: Float, gy: Float, gz: Float) =
                    GyroTelemetry.publish(gx, gy, gz)
            }
        } else {
            null
        }
        // Phase 3c: tell the host the encode envelope to render at — the panel
        // native size, oriented by the Orientation setting, and capped by the user's
        // resolution target (lines on the SHORT side, e.g. 1080p) so a 4K panel
        // streaming 1080p doesn't push a 4K render. Portrait reports a TALL envelope
        // (short×long) so the host renders portrait (apps reflow), not a letterboxed
        // landscape; landscape/auto report wide (long×short). 0×0 (panel unknown) is
        // a safe no-op that keeps the host at its configured size. The host matches a
        // reorientation exactly and grows same-orientation, both via a clean restart.
        val portrait = runCatching {
            appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
                .getString("orientation", "auto") == "portrait"
        }.getOrDefault(false)
        val (envW, envH) = run {
            val longSide = maxOf(caps.width, caps.height)
            val shortSide = minOf(caps.width, caps.height)
            if (longSide <= 0 || shortSide <= 0) {
                0 to 0
            } else {
                val t = resTargetH
                val effShort = if (t <= 0 || t >= shortSide) shortSide else t
                val s = effShort and 1.inv()
                val l = ((longSide.toLong() * effShort / shortSide).toInt()) and 1.inv()
                if (portrait) s to l else l to s
            }
        }
        Log.i(
            TAG,
            "dial ${s.name}: refresh=${caps.chosenRefresh}Hz envelope=${envW}x${envH} " +
                "(panel ${caps.width}x${caps.height}, target=${resTargetH}p, portrait=$portrait)",
        )
        val gp = runCatching {
            // NON-BLOCKING: spins the QUIC client on its own thread.
            startVideoStream(
                addr,
                s.entry.streamAltAddrs(), // LAN-first; WAN/DDNS tried on fallback
                expectedPubkey,
                identityPath,
                frameSink,
                audioSink,
                gyroSink,
                catalogSink,
                clipboardSink,
                wantsGyro,
                // Requested stream mode: the panel-derived encode envelope (Phase 3c,
                // 0×0 = unknown → host keeps its configured size) + this panel's
                // refresh, so the host caps fps to what the panel can actually show
                // (the fix for the 144→60Hz decoder-overload corruption).
                StreamMode(
                    width = envW.coerceAtLeast(0).toUInt(),
                    height = envH.coerceAtLeast(0).toUInt(),
                    refreshMhz = (caps.chosenRefresh * 1000f).toInt().coerceAtLeast(0).toUInt(),
                ),
            )
        }.getOrElse {
            Log.e(TAG, "startVideoStream failed for ${s.name}: ${it.message}")
            // A dial that fails AT connect time (host asleep / mid-boot / transient
            // network — common with the WoL couch flow where we race the host coming
            // up) must retry, not land terminally in Failed. Mirror handleEnd's
            // capped backoff unless the user deliberately stopped this session.
            if (s.userStopped) {
                s.status = ConnStatus.Failed
                if (s.active) postActiveStatus(ConnStatus.Failed)
            } else {
                s.status = ConnStatus.Reconnecting
                if (s.active) postActiveStatus(ConnStatus.Reconnecting)
                val delay = s.backoffMs
                s.backoffMs = (s.backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS)
                val reconnectGen = s.generation
                main.postDelayed({ reconnect(s, reconnectGen) }, delay)
            }
            publishLocked()
            return
        }
        s.gamepad = gp
        // Re-assert the client's resolution range on this fresh connection (Phase
        // 2): the host starts at Auto each session, so push the saved [floor,
        // target] now. No-op at the default (0, 0).
        if (resTargetH != 0 || resFloorH != 0) {
            runCatching { gp.setResolutionPref(resTargetH.toUInt(), resFloorH.toUInt()) }
        }
        // Re-assert the bitrate range too (Phase 3). An untouched slider on wired
        // Ethernet auto-caps at WIRED_AUTO_MAX_KBPS (instead of the host's 80 Mbps
        // Auto default); otherwise no-op at the default (0, 0).
        val (bMin, bMax) = effectiveBitratePref()
        if (bMin != 0 || bMax != 0) {
            if (bitrateMinKbps == 0 && bitrateMaxKbps == 0) {
                Log.i(TAG, "bitrate pref: wired auto-cap ${bMax / 1000} Mbps (slider untouched)")
            }
            runCatching { gp.setBitratePref(bMin.toUInt(), bMax.toUInt()) }
        }
        // And the framerate range (Phase 3). No-op at the default (0, 0).
        if (fpsMin != 0 || fpsMax != 0) {
            runCatching { gp.setFpsPref(fpsMin.toUInt(), fpsMax.toUInt()) }
        }
        // The QUIC client thread is up but NO frame has decoded yet, so the badge
        // stays "Connecting…" — it only flips to Live on the first active frame
        // (FrameSink.onFrame → markActiveLive). The decoder/audio are still created
        // now so they're ready to consume the moment media arrives (they just wait
        // on an empty queue); we gate them on the connection being up, not on Live.
        s.status = ConnStatus.Connecting
        s.backoffMs = INITIAL_BACKOFF_MS
        if (s.active) {
            postActiveStatus(ConnStatus.Connecting)
            startDecoderLocked()
            startAudioAndControllersLocked(s)
        }
    }

    /// Promote the active session to Live once its first frame has decoded. Guarded
    /// by [gen] so a frame racing a re-dial/teardown can't resurrect a stale session.
    private fun markActiveLive(s: Session, gen: Int) {
        synchronized(lock) {
            if (s.generation != gen) return
            if (sessions[s.hostId] !== s) return
            if (!s.active || s.status != ConnStatus.Connecting) return
            s.status = ConnStatus.Live
            postActiveStatus(ConnStatus.Live)
            publishLocked()
        }
    }

    /// Handle a stream's `onEnd` on the main thread. A user disconnect or a stale
    /// generation is ignored; an unexpected end on a still-wanted session triggers
    /// graceful reconnect with capped exponential backoff.
    private fun handleEnd(s: Session, gen: Int, error: String?) {
        synchronized(lock) {
            // Stale callback from a torn-down dial, or session already removed.
            if (s.generation != gen) return
            if (sessions[s.hostId] !== s) return
            Log.i(TAG, "stream ended for ${s.name}: ${error ?: "clean"} (userStopped=${s.userStopped})")
            // This session's GamepadInput is dead; drop it.
            runCatching { s.gamepad?.stop() }
            s.gamepad = null
            if (s.active) {
                stopDecoderLocked()
                stopAudioAndControllersLocked()
            }
            if (s.userStopped) {
                s.status = ConnStatus.Disconnected
                if (s.active) postActiveStatus(ConnStatus.Disconnected)
                publishLocked()
                return
            }
            // Host PRE-EMPTION (another device connected and took the single
            // streaming slot) is NOT a network drop: if we instantly redial we'd
            // pre-empt that device straight back, and two connected clients would
            // ping-pong forever. So the LOSING client YIELDS — stop here, no
            // backoff redial — and waits for the user to reclaim the host (tap it
            // in Hosts), letting the most-recently-chosen device win. The Rust
            // client tags this end with "preempted" (the host's distinct QUIC
            // close code); a plain drop has a null/other reason → normal reconnect.
            if (error?.contains("preempted", ignoreCase = true) == true) {
                Log.i(TAG, "  → preempted by another device; yielding (no auto-reconnect)")
                s.backoffMs = INITIAL_BACKOFF_MS
                s.status = ConnStatus.Preempted
                if (s.active) postActiveStatus(ConnStatus.Preempted)
                publishLocked()
                return
            }
            // Unexpected end → reconnect after backoff. Picture resyncs ≤1s once
            // the host's IDR cadence lands.
            s.status = ConnStatus.Reconnecting
            if (s.active) postActiveStatus(ConnStatus.Reconnecting)
            publishLocked()
            val delay = s.backoffMs
            s.backoffMs = (s.backoffMs * 2).coerceAtMost(MAX_BACKOFF_MS)
            val reconnectGen = s.generation
            main.postDelayed({ reconnect(s, reconnectGen) }, delay)
        }
    }

    private fun reconnect(s: Session, gen: Int) {
        synchronized(lock) {
            if (s.generation != gen) return         // superseded by a newer dial
            if (sessions[s.hostId] !== s) return     // removed meanwhile
            if (s.userStopped) return
            dialLocked(s)
            publishLocked()
        }
    }

    private fun onCatalog(s: Session, apps: List<AppInfo>) {
        synchronized(lock) {
            if (sessions[s.hostId] !== s) return
            s.apps = apps
            publishLocked()
        }
    }

    /// Make [hostId] the active session: flip the `active` flags, move the decoder
    /// + audio + controllers onto it. The previously-active session stays connected
    /// (catalog/launch) with its frames now dropped.
    private fun setActiveLocked(hostId: String) {
        val target = sessions[hostId] ?: return
        if (_activeHostId.value == hostId && target.active) return

        // Demote the previous active session: stop feeding it audio/controllers and
        // tear down the decoder (it'll be rebuilt for the new active session).
        val prev = activeSessionLocked()
        if (prev != null && prev !== target) {
            prev.active = false
        }
        stopDecoderLocked()
        stopAudioAndControllersLocked()
        // The new host's stream size is unrelated — let the next decoder configure
        // from panel caps and learn the real size from its first format change.
        lastVideoSize = null

        target.active = true
        postActive(hostId, target.status)

        // If the new active session's connection is up (Live, or Connecting and
        // awaiting its first frame) and we have a Surface, bring the decoder +
        // audio + controllers up on it now; the first frame then promotes it.
        if (target.isConnected()) {
            startDecoderLocked()
            startAudioAndControllersLocked(target)
        } else if (target.status == ConnStatus.Disconnected || target.status == ConnStatus.Failed) {
            // Switched to a dead session — re-dial it.
            target.userStopped = false
            dialLocked(target)
        }
        // (Reconnecting: a backoff re-dial is already scheduled; nothing to do.)
    }

    /// Drop the decoder + audio + controllers from whatever session held them
    /// (used when the active session is removed). Does NOT clear _activeHostId.
    private fun detachActiveLocked() {
        stopDecoderLocked()
        stopAudioAndControllersLocked()
    }

    private fun startDecoderLocked() {
        val surf = surface ?: return
        if (decoder != null) return
        val active = activeSessionLocked() ?: return
        // Bind once the connection is up (Connecting too) so the decoder is ready
        // to consume the first frame, which is what promotes the session to Live.
        if (!active.isConnected()) return
        // Build for the remembered codec (HEVC until the first AU is sniffed; the
        // right codec on a reconnect). The first frame re-sniffs and swaps if the
        // host's codec changed.
        // The onStats sink is wired ONLY in the debug variant — release passes null
        // so the decoder skips it entirely (one per-second null check).
        val statsSink: ((DecoderStats) -> Unit)? =
            if (no.aasle.trireme.BuildConfig.DEBUG) { s -> _debugMetrics.value = s } else null
        val (kw, kh) = lastVideoSize ?: (0 to 0)
        decoder = runCatching {
            HevcDecoder(
                surf, effectivePace(active.pace), caps, streamMime,
                onStats = statsSink,
                onAspect = { a -> _videoAspect.value = a },
                initW = kw,
                initH = kh,
                onGeometry = { w, h -> onDecoderGeometry(w, h) },
            )
        }.getOrElse {
            Log.e(TAG, "decoder create failed: ${it.message}")
            null
        }
    }

    /// Decoder reported a coded-size change it can't absorb in place (see
    /// HevcDecoder.captureOutputFormat): it has already stopped its own loop
    /// without presenting any wrong-geometry frame. Rebuild a decoder configured
    /// at the reported size. Invoked on the decode thread → hop to main, where
    /// every other decoder lifecycle op runs.
    private fun onDecoderGeometry(w: Int, h: Int) {
        main.post {
            synchronized(lock) {
                lastVideoSize = w to h
                if (decoder == null) return@post
                Log.i(TAG, "stream geometry ${w}x${h} beyond decoder config — rebuilding decoder")
                stopDecoderLocked()
                startDecoderLocked()
                // Resync the fresh decoder in ~1 RTT instead of waiting for the
                // host's next scheduled IDR (≤1s).
                runCatching { requestKeyframe() }
            }
        }
    }

    private fun stopDecoderLocked() {
        decoder?.stop()
        decoder = null
        // Clear the HUD metrics so it reads "no stream" rather than a stale snapshot.
        _debugMetrics.value = null
        // Re-arm sniffing so the next stream/decoder re-detects the codec.
        sniffed = false
        // Clear any stale "degraded" indicator so it doesn't stick across a
        // reconnect/host switch (the host only emits degraded while actively
        // dropping, so absence must read as healthy).
        main.removeCallbacks(clearLinkDegraded)
        _linkDegraded.value = false
    }

    /// Rebuild the decoder for `mime` if the current one doesn't match (driven by
    /// the first-AU sniff in onFrame). Runs on the main thread under the lock,
    /// like every other decoder lifecycle op.
    private fun swapDecoderMimeIfNeeded(mime: String) {
        synchronized(lock) {
            streamMime = mime
            if (decoder?.mime == mime) return
            Log.i(TAG, "codec sniffed as $mime — rebuilding decoder")
            stopDecoderLocked()
            // stopDecoderLocked re-arms `sniffed`; we already consumed the sniff
            // for this stream, so keep it armed only if there's no decoder to build.
            startDecoderLocked()
            sniffed = true
        }
    }

    /// Sniff the video MIME from the first access unit. HEVC (Annex-B) opens with
    /// a NAL start code `00 00 [00] 01`; an AV1 low-overhead OBU temporal unit
    /// opens with an OBU header (forbidden bit 0) of type sequence-header (1),
    /// temporal-delimiter (2) or frame (6). Defaults to HEVC when ambiguous.
    private fun sniffCodecMime(au: ByteArray): String {
        val mime = sniffCodecMimeInner(au)
        val head = au.take(16).joinToString(" ") { "%02x".format(it.toInt() and 0xff) }
        Log.i(TAG, "codec sniff: [$head] (len=${au.size}) -> $mime")
        return mime
    }

    private fun sniffCodecMimeInner(au: ByteArray): String {
        if (au.size >= 3 && au[0].toInt() == 0 && au[1].toInt() == 0) {
            if (au[2].toInt() == 1) return "video/hevc"
            if (au.size >= 4 && au[2].toInt() == 0 && au[3].toInt() == 1) return "video/hevc"
        }
        if (au.isNotEmpty()) {
            val b = au[0].toInt() and 0xff
            if ((b and 0x80) == 0) {
                val obuType = (b shr 3) and 0x0f
                if (obuType == 1 || obuType == 2 || obuType == 6) return "video/av01"
            }
        }
        return "video/hevc"
    }

    /// Audio + controllers are owned per-active-session: they're created when a
    /// session becomes the live active one and persist across navbar nav. Audio is
    /// best-effort — a sink failure must not take down video.
    private fun startAudioAndControllersLocked(s: Session) {
        if (audio == null) {
            audio = runCatching { OpusAudioPlayer(appContext) }.getOrElse {
                Log.w(TAG, "audio init failed (${it.message}); video only")
                null
            }
        }
        val gp = s.gamepad
        // Keep the ControllerManager bound to the CURRENT session handle. A stale
        // manager bound to a dead GamepadInput after a reconnect/re-dial swapped
        // `s.gamepad` is the "pad none / no forwarding" bug: onKey/onMotion return
        // true but the sink's events vanish into a closed input stream. The handle
        // changes by reference identity (`!==`) on every fresh dial, so:
        //   - no handle       → tear the manager down;
        //   - manager absent  → build + start it against the live handle;
        //   - manager present → REBIND (re-point sink + re-announce pads) rather than
        //     tear down + re-detect, so the InputManager listener + NS-mode gyro
        //     registrations survive (no re-detect churn) and the pad can never be
        //     left forwarding into a dead handle.
        if (gp !== controllersGamepad) {
            val existing = controllers
            when {
                gp == null -> {
                    existing?.stop()
                    controllers = null
                }
                existing == null ->
                    controllers = ControllerManager(appContext, gp, controllerHandling)
                        .also { it.start() }
                else -> existing.rebind(gp)
            }
            controllersGamepad = gp
        }
    }

    private fun stopAudioAndControllersLocked() {
        controllers?.stop()
        controllers = null
        controllersGamepad = null
        audio?.stop()
        audio = null
    }

    /// Stop this session's FFI handle (deliberate teardown). The generation must
    /// already have been bumped by the caller so the resulting onEnd is ignored.
    private fun teardownSessionLocked(s: Session) {
        runCatching { s.gamepad?.stop() }
        s.gamepad = null
        s.status = ConnStatus.Disconnected
    }

    // ----- Pace resolution -----

    /// Map the per-server pace string (ServerEntry.pace) to a [PaceMode]. Defaults
    /// to VRR (the live-validated default in the original player).
    private fun paceOf(entry: ServerEntry): PaceMode = when (entry.pace?.lowercase()) {
        "low-latency", "low_latency", "lowlatency" -> PaceMode.LOW_LATENCY
        "balanced" -> PaceMode.BALANCED
        "smooth" -> PaceMode.SMOOTH
        "vrr" -> PaceMode.VRR
        else -> PaceMode.VRR
    }

    /**
     * The pace the decoder should actually run: the Settings [paceOverride] if set,
     * else the session's own pace. VRR is meaningless on a single-mode fixed-refresh
     * panel (it has nothing to switch to and behaves as LOW_LATENCY), so we fall back
     * there rather than dialing VRR — keeps the decoder consistent with the Settings UI,
     * which hides VRR on such panels.
     */
    private fun effectivePace(sessionPace: PaceMode): PaceMode {
        val want = paceOverride ?: sessionPace
        return if (want == PaceMode.VRR && !caps.rangeable) PaceMode.LOW_LATENCY else want
    }

    /**
     * Apply a pace preset chosen in Settings to the live stream: store it and rebuild
     * the active decoder so it takes effect now (the decoder's pace is fixed at
     * construction). Safe on the main thread. A no-op visually until a decoder exists.
     */
    fun setPace(mode: PaceMode) {
        synchronized(lock) {
            paceOverride = mode
            if (surface != null && activeSessionLocked()?.isConnected() == true) {
                stopDecoderLocked()
                startDecoderLocked()
            }
        }
        Log.i(TAG, "pace override set to $mode (effective rebuild)")
    }

    /** Read the global pace preset SettingsPane persisted (PaceChoice.name), or null. */
    private fun readPacePref(ctx: Context): PaceMode? =
        when (ctx.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .getString("pace", null)) {
            "LowLatency" -> PaceMode.LOW_LATENCY
            "Balanced" -> PaceMode.BALANCED
            "Smooth" -> PaceMode.SMOOTH
            "Vrr" -> PaceMode.VRR
            else -> null
        }

    /**
     * Current stream-time gyro opt-in (read on dial via the volatile field;
     * exposed for Settings to render the toggle's initial state).
     */
    fun isGyroEnabled(): Boolean = gyroEnabled

    /**
     * Persist the new gyro opt-in and update the in-memory flag, AND apply it LIVE
     * to the active session's controllers so it takes effect immediately — turning
     * it off unregisters the IMU listeners right now (a true off, so the continuous
     * gyro→mouse stream stops without a reconnect), turning it on starts them for
     * any already-attached pad. The next dial still re-reads the flag for the
     * handshake `wants_gyro`. Cheap; safe on any thread.
     */
    fun setGyroEnabled(on: Boolean) {
        gyroEnabled = on
        appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .edit()
            .putBoolean("gyro_enabled", on)
            .apply()
        controllers?.setGyroForwarding(on)
        Log.i(TAG, "gyro opt-in set to $on (applied live to attached pads)")
    }

    /** Read the persisted gyro opt-in (default false — never accidentally on). */
    private fun readGyroEnabledPref(ctx: Context): Boolean =
        ctx.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .getBoolean("gyro_enabled", false)

    /** Current controller-handling state (for the Controllers pane / Settings to
     *  render the toggle's initial value). */
    fun isControllerHandling(): Boolean = controllerHandling

    /**
     * Flip the controller-handling master switch and apply it live: persist the
     * pref, update the flag, and rebuild ONLY the active session's
     * [ControllerManager] (audio + decoder untouched) so attached pads re-register
     * in the new mode immediately — managed (profiles/gyro) ⇄ raw. Safe on the
     * main thread.
     */
    fun setControllerHandling(on: Boolean) {
        synchronized(lock) {
            if (controllerHandling == on) return
            controllerHandling = on
            appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
                .edit()
                .putBoolean("controller_handling", on)
                .apply()
            restartControllersLocked()
        }
        Log.i(TAG, "controller handling set to $on (live rebuild)")
    }

    /** Rebuild the active session's [ControllerManager] in place (no audio/decoder
     *  churn) so a managed⇄raw switch takes effect on already-attached pads without
     *  a reconnect. No-op when controllers weren't running (no surface/session) —
     *  the next [startAudioAndControllersLocked] picks up the new flag. */
    private fun restartControllersLocked() {
        val wasRunning = controllers != null
        controllers?.stop()
        controllers = null
        controllersGamepad = null
        if (!wasRunning) return
        val s = activeSessionLocked() ?: return
        val gp = s.gamepad ?: return
        if (s.isConnected()) {
            controllers = ControllerManager(appContext, gp, controllerHandling).also { it.start() }
            controllersGamepad = gp
        }
    }

    /** Read the persisted controller-handling opt-in (default false — pads forward raw). */
    private fun readControllerHandlingPref(ctx: Context): Boolean =
        ctx.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .getBoolean("controller_handling", false)

    /** Current debug-HUD visibility (for Settings to render the toggle's state). */
    fun isDebugHud(): Boolean = _debugHud.value

    /** Show/hide the on-screen debug HUD (debug builds). Persists + updates the
     *  flow the shell observes, so it takes effect immediately. */
    fun setDebugHud(on: Boolean) {
        _debugHud.value = on
        appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .edit()
            .putBoolean("debug_hud", on)
            .apply()
        Log.i(TAG, "debug HUD ${if (on) "shown" else "hidden"}")
    }

    /** Current resolution range (target, floor) in px; `0` = Auto / no floor.
     *  Exposed so Settings renders the slider's initial state. */
    fun resolutionPref(): Pair<Int, Int> = Pair(resTargetH, resFloorH)

    /**
     * Set the resolution range (Phase 2): `targetH` is the ceiling coded height
     * the host should uphold (`0` = Auto/native), `floorH` the minimum it may
     * auto-downscale to (`0` = no floor). Persists, updates the in-memory fields,
     * and pushes LIVE to the active host (re-asserted on each reconnect). Cheap;
     * takes effect within ~1s. Safe on the main thread.
     */
    fun setResolutionPref(targetH: Int, floorH: Int) {
        resTargetH = targetH
        resFloorH = floorH
        appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .edit()
            .putInt("res_target_h", targetH)
            .putInt("res_floor_h", floorH)
            .apply()
        runCatching { activeGamepad()?.setResolutionPref(targetH.toUInt(), floorH.toUInt()) }
        Log.i(TAG, "resolution pref → target=${targetH}p floor=${floorH}p (live)")
    }

    /** Current bitrate range (minKbps, maxKbps); `min=0` = host floor, `max=0` =
     *  Auto (host-owned). Exposed so Settings renders the slider's initial state. */
    fun bitratePref(): Pair<Int, Int> = Pair(bitrateMinKbps, bitrateMaxKbps)

    /**
     * The auto-cap for an UNTOUCHED bitrate slider on a wired link: 150 Mbps when
     * the active network is Ethernet (TV port or USB-C adapter), else 0 (no cap —
     * the host's Auto default applies). Transport-informed, not measured: Android
     * can't report the negotiated PHY rate to a normal app (AOSP hardcodes the
     * Ethernet link bandwidth to 100 Mbps, and /sys/class/net is SELinux-walled),
     * so this may overshoot a Fast-Ethernet port — the host's net-bitrate
     * controller backs off within ~2 QoS ticks and adapts from there.
     */
    fun wiredAutoMaxKbps(): Int = runCatching {
        val cm = appContext.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
        val caps = cm?.getNetworkCapabilities(cm.activeNetwork)
        if (caps?.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) == true) {
            WIRED_AUTO_MAX_KBPS
        } else {
            0
        }
    }.getOrDefault(0)

    /**
     * The bitrate range actually pushed to the host: the user's stored range when
     * they've set one; otherwise `(0, wiredAutoMaxKbps())` — the wired auto-cap on
     * Ethernet, or the inert `(0, 0)` (host-owned Auto) on everything else. The
     * slider can't produce a stored `(0, 0)` (its min stop is 5 Mbps), so `(0, 0)`
     * reliably means "never touched". Evaluated per call, so it tracks the
     * transport at each (re)connect.
     */
    fun effectiveBitratePref(): Pair<Int, Int> =
        if (bitrateMinKbps != 0 || bitrateMaxKbps != 0) {
            Pair(bitrateMinKbps, bitrateMaxKbps)
        } else {
            Pair(0, wiredAutoMaxKbps())
        }

    /**
     * Set the bitrate range (Phase 3) — the primary auto-recover lever. `minKbps`
     * is the floor the host won't drop below (`0` = the host's 2 Mbps default),
     * `maxKbps` the ceiling it strives for (`0` = uncapped, host owns the top).
     * Persists, updates the in-memory fields, and pushes LIVE to the active host
     * (re-asserted on each reconnect). Cheap; takes effect within ~1s.
     */
    fun setBitratePref(minKbps: Int, maxKbps: Int) {
        bitrateMinKbps = minKbps
        bitrateMaxKbps = maxKbps
        appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .edit()
            .putInt("bitrate_min_kbps", minKbps)
            .putInt("bitrate_max_kbps", maxKbps)
            .apply()
        runCatching { activeGamepad()?.setBitratePref(minKbps.toUInt(), maxKbps.toUInt()) }
        Log.i(TAG, "bitrate pref → min=${minKbps}kbps max=${maxKbps}kbps (live)")
    }

    /** Current framerate range (minFps, maxFps); `min=0` = host floor, `max=0` =
     *  Auto (panel refresh). Exposed so Settings renders the slider's initial state. */
    fun fpsPref(): Pair<Int, Int> = Pair(fpsMin, fpsMax)

    /**
     * Set the framerate range (Phase 3). `minFps` is the adaptation floor the host
     * won't auto-drop below (`0` = host default 24); `maxFps` the target ceiling it
     * paces to, clamped to the panel refresh (`0` = Auto/panel). Persists, updates
     * the in-memory fields, and pushes LIVE to the active host (re-asserted on each
     * reconnect). Cheap; takes effect within ~1s.
     */
    fun setFpsPref(minFps: Int, maxFps: Int) {
        fpsMin = minFps
        fpsMax = maxFps
        appContext.getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .edit()
            .putInt("fps_min", minFps)
            .putInt("fps_max", maxFps)
            .apply()
        runCatching { activeGamepad()?.setFpsPref(minFps.toUInt(), maxFps.toUInt()) }
        Log.i(TAG, "fps pref → min=$minFps max=$maxFps (live)")
    }

    // ----- StateFlow publishing (must end on the main thread) -----

    /// Recompute [connections] + [pooledApps] from the session map and publish.
    /// Safe to call under [lock]; StateFlow.value is thread-safe to set, but we
    /// build immutable snapshots so collectors never see torn lists.
    private fun publishLocked() {
        val activeId = _activeHostId.value
        val conns = sessions.values.map {
            HostConn(it.hostId, it.name, it.status, it.hostId == activeId)
        }
        val pool = sessions.values
            .filter { it.apps.isNotEmpty() }
            .map { HostApps(it.hostId, it.name, it.apps) }
        // MutableStateFlow.value is thread-safe; collectors observe on their own
        // dispatcher. We snapshot under lock so the two flows stay consistent.
        _connections.value = conns
        _pooledApps.value = pool
    }

    private fun postActive(hostId: String?, status: ConnStatus) {
        _activeHostId.value = hostId
        _activeStatus.value = status
    }

    private fun postActiveStatus(status: ConnStatus) {
        _activeStatus.value = status
    }

    private const val INITIAL_BACKOFF_MS = 500L
    private const val MAX_BACKOFF_MS = 4_000L
}
