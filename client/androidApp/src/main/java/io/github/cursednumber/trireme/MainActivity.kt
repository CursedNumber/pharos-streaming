// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme

import android.content.Context
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Display
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.compose.setContent
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Wifi
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.ui.graphics.Color
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.platform.LocalView
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.compose.runtime.collectAsState
import kotlinx.coroutines.delay
import io.github.cursednumber.trireme.stream.ConnStatus
import io.github.cursednumber.trireme.stream.DisplayCaps
import io.github.cursednumber.trireme.stream.StreamHub
import io.github.cursednumber.trireme.ui.NavDest
import io.github.cursednumber.trireme.ui.NavMode
import io.github.cursednumber.trireme.ui.DebugHud
import io.github.cursednumber.trireme.ui.NavShell
import io.github.cursednumber.trireme.ui.RAIL_WIDTH
import io.github.cursednumber.trireme.ui.navBandWidth
import io.github.cursednumber.trireme.ui.TriremeTheme
import io.github.cursednumber.trireme.ui.TriremeTokens
import io.github.cursednumber.trireme.ui.panes.AppsPane
import io.github.cursednumber.trireme.ui.panes.ControllersPane
import io.github.cursednumber.trireme.ui.panes.HostsPane
import io.github.cursednumber.trireme.ui.panes.SettingsPane

private const val TAG = "Trireme"
// Window within which a second Back press (navbar open) means "exit" rather than
// re-prompting; matches the contract's ~2s double-Back-to-exit.
/** How long the touch edge-grip hint stays visible after the navbar closes, then fades. */
private const val EDGE_HINT_MS = 4_000L
/** How long the Peek (icon-rail) stays before auto-hiding if not expanded to Full. */
private const val PEEK_TIMEOUT_MS = 10_000L
// (Quitting is the rail Exit item — graceful StreamHub.shutdown() + finish() — not Back.)

// Touch → pointer tuning (see MainActivity.onTouchEvent). Pixel values; rough but
// serviceable — refine per density later if needed.
/** Max press duration for a trackpad tap to count as a click. */
private const val TAP_MS = 180L
/** Movement (px) beyond which a trackpad gesture is a drag, not a tap. */
private const val TAP_SLOP = 24f
/** Trackpad relative-motion gain (host pixels per screen pixel of drag). */
private const val TRACKPAD_SENS = 1.5f
/** Two-finger scroll: screen px of drag per wheel notch. */
private const val SCROLL_DIV = 48f
/** Hold duration (ms) for a still trackpad press to fire a right-click (desktop only). */
private const val LONG_PRESS_MS = 650L
/** Movement (px) that CANCELS a pending long-press. Deliberately larger than
 *  [TAP_SLOP] so small finger jitter is tolerated during the hold. */
private const val LONG_PRESS_SLOP = 48f
/** Max gap (ms) between the first tap's UP and the second tap's DOWN for a
 *  double-tap. A second tap that then holds/drags becomes a left-button drag.
 *  Generous so a deliberate double-tap-and-hold isn't missed and mis-read as a
 *  plain double-click. */
private const val DOUBLE_TAP_MS = 420L
/** Max distance (px) between the two taps of a double-tap. */
private const val DOUBLE_TAP_SLOP = 72f

/**
 * Trireme shell — a SINGLE full-screen, immersive Activity that is the launcher.
 *
 * A full-bleed background [SurfaceView] hosts the live video; the active session's
 * frames render here via [StreamHub] (which owns the single MediaCodec decoder,
 * audio and controllers across navbar navigation and host switches — see the
 * multi-host model in StreamHub). A Compose overlay on top hosts the left-edge
 * [NavShell] and the active pane; the navbar slides in over the still-playing
 * stream and never tears it down.
 *
 * Ownership note (from the contract): refresh/VRR configuration needs a Window, so
 * `configureRefresh()` stays HERE in the shell; its [DisplayCaps] are handed to
 * [StreamHub.onSurfaceAvailable] and forwarded to the decoder. The shell otherwise
 * holds no streaming state — that all lives in the process-lifetime hub so it
 * survives activity recreate and graceful reconnects.
 */
class MainActivity : ComponentActivity(), SurfaceHolder.Callback {
    // --- Navbar reveal state machine (driven from dispatchKeyEvent) ---
    // Whether the navbar overlay is open, and which destination is showing. Plain
    // Compose state read by the overlay content and written from the key handler /
    // surface lifecycle, all on the main thread.
    private val navMode = mutableStateOf(NavMode.Hidden)
    private val navDest = mutableStateOf(NavDest.Hosts)
    // Bumped on each rail navigation while in Peek so the auto-hide timer restarts
    // (Compose LaunchedEffect keys on it). See ShellContent's peek timer.
    private val peekTick = mutableStateOf(0)

    // The gamepad Start+Select combo — a couch opener for the navbar when there's
    // no usable remote key (many TV remotes route Menu to the launcher), preserved
    // from the original player; here it opens the navbar instead of the old grid.
    private var startDown = false
    private var selectDown = false
    // Swallow the long-press OK release that opens the navbar so it doesn't
    // immediately click the focused rail item / row.
    private var swallowCenterUntilUp = false

    // --- Touch → host pointer gesture state (see onTouchEvent) ---
    private val hasTouch by lazy {
        packageManager.hasSystemFeature(android.content.pm.PackageManager.FEATURE_TOUCHSCREEN)
    }
    private var tDownTime = 0L
    private var tDownX = 0f
    private var tDownY = 0f
    private var tLastX = 0f
    private var tLastY = 0f
    private var tMoved = false // exceeded slop → a drag, not a tap
    private var tInContent = false // absolute: DOWN landed inside the content rect
    private var tScrolling = false // two-finger scroll active
    private var tScrollLastY = 0f
    private var tPressed = false // a left button is held down (absolute drag)
    // --- Controller-as-cursor state ---
    // Cursor mode is a persistent TOGGLE (StreamHub.cursorMode): while on, the right
    // stick drives the host cursor via a ~60Hz ticker (a held stick fires only one
    // motion event, so a poll loop is needed) and A/B become left/right click.
    // The gamepad way to flip it: HOLD Start ~4.5s (Artemis-style), gated by the
    // "Hold Start for cursor" Settings option; a normal Start tap still passes to
    // the pad. The ticker runs only while on, so it's zero-cost otherwise.
    @Volatile private var rStickX = 0f
    @Volatile private var rStickY = 0f
    private val cursorHandler by lazy { Handler(Looper.getMainLooper()) }
    private var cursorTickerRunning = false
    private var cursorToggleRunnable: Runnable? = null // the pending 4.5s Start-hold
    // Host cursor pixels per tick at full stick deflection (~60Hz → ~840 px/s).
    private val cursorSpeed = 14f
    private val cursorDeadzone = 0.12f
    private val cursorHoldMs = 4500L // Start-hold duration to toggle the cursor

    private fun cursorActive(): Boolean = StreamHub.isCursorMode()

    /** Cancel a pending Start-hold toggle (Start released early, or Select joined
     *  to form the navbar combo). */
    private fun cancelCursorToggleHold() {
        cursorToggleRunnable?.let { cursorHandler.removeCallbacks(it) }
        cursorToggleRunnable = null
    }

    private fun ensureCursorTicker() {
        if (cursorTickerRunning || !cursorActive()) return
        cursorTickerRunning = true
        val tick = object : Runnable {
            override fun run() {
                if (!cursorActive() || !StreamHub.hasActiveSession() ||
                    navMode.value != NavMode.Hidden
                ) {
                    cursorTickerRunning = false
                    return
                }
                val x = rStickX
                val y = rStickY
                val mag = kotlin.math.hypot(x, y)
                if (mag > cursorDeadzone) {
                    // Rescale past the deadzone so slow movement stays precise, and
                    // square the magnitude for finer control near center.
                    val scaled = ((mag - cursorDeadzone) / (1f - cursorDeadzone)).coerceIn(0f, 1f)
                    val f = scaled * scaled / mag * cursorSpeed
                    StreamHub.pointerRel(x * f, y * f)
                }
                cursorHandler.postDelayed(this, 16L)
            }
        }
        cursorHandler.post(tick)
    }

    // --- USB-mouse → host pointer state ---
    // A mouse connected to the TV drives the host cursor directly (absolute over
    // the video rect). Android splits mouse input awkwardly — hover comes via
    // generic-motion, drags via the touch pipeline, buttons via a bitmask on both
    // — so one forwardMouse() handles all three; this tracks the last button
    // bitmask so a press and release each fire exactly once.
    private var lastMouseButtons = 0

    // Trackpad advanced gestures (desktop-only): long-press → right-click,
    // double-tap-hold → left-drag. All driven from handleTrackpadTouch.
    private val touchHandler by lazy { Handler(Looper.getMainLooper()) }
    private var tLongPressRunnable: Runnable? = null
    private var tLongPressFired = false // right-click already fired this gesture
    private var tDragging = false // left button held for a double-tap drag
    private var tLastTapUpTime = 0L // for double-tap detection
    private var tLastTapUpX = 0f
    private var tLastTapUpY = 0f

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Apply the saved orientation preference (Settings → Orientation). Read once
        // here so a change takes effect on the next launch (as the Settings row says)
        // rather than rotating mid-session. Landscape-family only — a landscape host
        // has nothing useful to show in portrait, so we never lock portrait.
        applyOrientationPref()

        // Full-screen immersive: draw edge-to-edge behind the (hidden) system bars,
        // hide them with sticky-immersive swipe behaviour, and keep the screen on
        // for the duration of a couch session. No ActionBar/title — the old
        // "Trireme" banner is gone; the shell IS the chrome.
        WindowCompat.setDecorFitsSystemWindows(window, false)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }

        // Auto-start USB export so enrolled controllers reconnect without opening
        // the USB screen (no-op when nothing is enabled). Kept from the old launcher.
        UsbExportService.ensureRunning(this)

        // Back TOGGLES the menu via the modern dispatcher (NOT only KEYCODE_BACK) so it
        // fires for the TV remote's hardware Back, 3-button Back, AND gesture-nav
        // swipe-back / predictive back alike (those arrive through the dispatcher, not as
        // a key event): on the stream (Hidden) Back OPENS the menu (Peek), and when the
        // menu is open Back CLOSES it back to the stream. Quitting the app is the explicit
        // Exit item in the rail (graceful StreamHub.shutdown() + finish()). Always enabled
        // so Back never falls through to a bare finish() while a stream is live.
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                if (navMode.value == NavMode.Hidden) openNavbar() else closeNavbar()
            }
        })

        // Cold start with no active session → open the FULL menu on Hosts so the user
        // can connect (an empty stream with no chrome is a dead end; Peek alone would
        // hide before they reach the host list).
        if (StreamHub.activeHostId.value == null) {
            navDest.value = NavDest.Hosts
            navMode.value = NavMode.Full
        }

        val store = ServerStore(this)
        setContent {
            // Mouse pointer capture is delivered to the FOCUSED view — the Compose
            // host view, not the decorView — so attach the listener + capture there.
            val composeView = LocalView.current
            LaunchedEffect(composeView) {
                composeView.isFocusableInTouchMode = true
                captureView = composeView
                composeView.setOnCapturedPointerListener { _, event -> handleCapturedMouse(event) }
                Log.i(TAG, "mouseDiag capture listener attached to compose view")
                updatePointerCapture()
            }
            TriremeTheme {
                ShellContent(
                    mode = navMode.value,
                    navDest = navDest.value,
                    peekTick = peekTick.value,
                    store = store,
                    // Selecting a rail icon picks that dest AND expands to the full pane
                    // (works from Peek = expand-on-pick, and from Full = switch pane).
                    onSelect = { navDest.value = it; navMode.value = NavMode.Full },
                    onCloseToStream = { closeNavbar() },
                )
            }
        }

        // Raw mouse via pointer capture is set up inside setContent on the Compose
        // host view (the focused view that actually receives captured events).
        Log.i(TAG, "mouseDiag build=pointer-capture-v2 (onCreate)")
    }

    override fun onResume() {
        super.onResume()
        // Re-assert immersive mode: the system can transiently re-show the bars
        // (e.g. after a swipe or returning from another activity).
        WindowInsetsControllerCompat(window, window.decorView)
            .hide(WindowInsetsCompat.Type.systemBars())
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        // Regained focus (e.g. returned from copying in another app): push the
        // current phone clipboard to the host, since the change listener only fires
        // while we're focused and would have missed a copy made elsewhere.
        if (hasFocus) StreamHub.onClipboardFocusSync()
        // Capture is dropped whenever the window loses focus; re-assert to match.
        updatePointerCapture()
    }

    // ============================================================
    // Navbar reveal helpers
    // ============================================================

    /** Reveal the navbar at the Peek (icon-rail) level; (re)starts the auto-hide timer. */
    private fun openNavbar() {
        // The navbar is about to take over touches; release any in-progress touch
        // gesture so an absolute-mode held button doesn't stick down on the host
        // (its ACTION_UP would be swallowed by onTouchEvent's navbar guard).
        if (tPressed) {
            StreamHub.pointerButton(0, false)
            tPressed = false
        }
        tScrolling = false
        if (navMode.value == NavMode.Hidden) navMode.value = NavMode.Peek
        peekTick.value++
        updatePointerCapture() // navbar up → release capture so the remote drives it
    }

    /** Expand Peek → Full (rail + active pane); cancels the auto-hide timer. */
    private fun expandToFull() {
        navMode.value = NavMode.Full
        updatePointerCapture()
    }

    /** Close the navbar back to full-screen stream (the stream never stopped). */
    private fun closeNavbar() {
        navMode.value = NavMode.Hidden
        updatePointerCapture() // back to the stream → recapture the mouse
    }

    // ============================================================
    // Surface lifecycle — forward to StreamHub (the ACTIVE session renders here)
    // ============================================================

    override fun surfaceCreated(holder: SurfaceHolder) {
        // configureRefresh() needs a Window, so it lives here; the resulting caps
        // are handed to the hub, which forwards them to the decoder.
        StreamHub.onSurfaceAvailable(holder.surface, configureRefresh())
    }

    override fun surfaceChanged(h: SurfaceHolder, format: Int, w: Int, height: Int) {
        StreamHub.onSurfaceChanged(h.surface)
    }

    override fun surfaceDestroyed(h: SurfaceHolder) {
        // Drop the decoder but keep the sessions connected (StreamHub re-attaches
        // on the next surface, reconnecting any session that died meanwhile).
        StreamHub.onSurfaceDestroyed()
    }

    // ============================================================
    // Input routing
    // ============================================================

    // Route input. When the navbar is CLOSED, gamepad/keyboard go to the active
    // session (ControllerManager owns gamepad/gyro inside StreamHub; a real
    // keyboard's keys are forwarded to the active host). When it is OPEN, Compose
    // owns navigation and Back drives the double-tap-to-exit. The reveal rules
    // (UP-at-top / LEFT-at-far-edge / Back-when-closed) open the navbar.
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val kc = event.keyCode
        val down = event.action == KeyEvent.ACTION_DOWN
        val up = event.action == KeyEvent.ACTION_UP

        // Track the gamepad Start+Select combo — preserved couch opener.
        when (kc) {
            KeyEvent.KEYCODE_BUTTON_START -> if (down) startDown = true else if (up) startDown = false
            KeyEvent.KEYCODE_BUTTON_SELECT -> if (down) selectDown = true else if (up) selectDown = false
        }
        val comboOpen = down &&
            (kc == KeyEvent.KEYCODE_BUTTON_START || kc == KeyEvent.KEYCODE_BUTTON_SELECT) &&
            startDown && selectDown
        // The Start-hold cursor toggle: cancel the pending timer if Start is
        // released early, or if Select joins Start (that's the navbar combo, not a
        // cursor toggle). Whatever the navbar state, so it can't fire stale.
        if ((kc == KeyEvent.KEYCODE_BUTTON_START && up) ||
            (kc == KeyEvent.KEYCODE_BUTTON_SELECT && down)
        ) {
            cancelCursorToggleHold()
        }

        if (navMode.value == NavMode.Hidden) {
            // --- Stream (navbar HIDDEN) ---
            // Controller-as-cursor input.
            //   • HOLD Start ~4.5s (Artemis-style, gated by the Settings option) to
            //     TOGGLE cursor mode on/off. Start is NOT consumed — a normal tap
            //     still reaches the game pad, and the Start+Select navbar combo still
            //     works (the timer is cancelled when Select joins). Only the long
            //     hold flips the mode; a toast confirms it.
            if (kc == KeyEvent.KEYCODE_BUTTON_START && down && event.repeatCount == 0 &&
                StreamHub.isHoldStartForCursor() && !selectDown
            ) {
                cancelCursorToggleHold()
                val r = Runnable {
                    cursorToggleRunnable = null
                    val next = !StreamHub.isCursorMode()
                    StreamHub.setCursorMode(next)
                    if (next) ensureCursorTicker()
                    android.widget.Toast.makeText(
                        this,
                        if (next) "Controller cursor ON" else "Controller cursor OFF",
                        android.widget.Toast.LENGTH_SHORT,
                    ).show()
                }
                cursorToggleRunnable = r
                cursorHandler.postDelayed(r, cursorHoldMs)
                // fall through: Start still reaches the pad as normal.
            }
            //   • While cursor mode is on, A = left click, B = right click.
            //     Consume ALL A/B events (incl. held repeats) so they never leak to
            //     the pad; the button edge fires on the first down and the up.
            if (cursorActive()) {
                when (kc) {
                    KeyEvent.KEYCODE_BUTTON_A -> {
                        if (down && event.repeatCount == 0) StreamHub.pointerButton(0, true)
                        else if (up) StreamHub.pointerButton(0, false)
                        return true
                    }
                    KeyEvent.KEYCODE_BUTTON_B -> {
                        if (down && event.repeatCount == 0) StreamHub.pointerButton(1, true)
                        else if (up) StreamHub.pointerButton(1, false)
                        return true
                    }
                }
            }
            // Forward to the active session FIRST: ControllerManager only consumes
            // input from a REGISTERED gamepad (incl. its d-pad → host hat axes), so a
            // connected pad's DPAD_LEFT/UP reaches the game instead of being eaten by
            // the reveal rule below. The TV remote's d-pad is NOT a registered
            // controller, so it falls through and still opens the navbar. This ordering
            // fixes the live-validated gamepad regression where pressing d-pad left/up
            // on an Elite 2 / Flydigi / Switch Pro popped the navbar mid-game.
            //
            // (Back is handled by the OnBackPressedDispatcher callback, not here, so
            // it works for gesture-nav swipe-back + predictive back too — see onCreate.)
            if (StreamHub.dispatchControllerKey(event)) return true

            // Reveal rules for the remote/keyboard (no registered pad consumed it):
            // the stream surface has no focusable content, so a D-pad nudge toward the
            // navbar (LEFT, its edge) or UP (top edge) reveals it; a long-press of OK
            // and the gamepad Start+Select combo also open it (remotes that grab Menu
            // still deliver D-pad/OK to us). As a belt-and-braces guard the edge reveal
            // is restricted to non-gamepad/joystick sources so a pad never triggers it.
            val fromPad = event.source and android.view.InputDevice.SOURCE_GAMEPAD != 0 ||
                event.source and android.view.InputDevice.SOURCE_JOYSTICK != 0
            // A registered pad's d-pad goes to the GAME while a session is active (so it
            // doesn't pop the navbar mid-play — use Start+Select to open then). But when
            // DISCONNECTED (no active session) there's nothing to forward to, so let a
            // controller's LEFT/UP reveal the menu too — otherwise a pad-only user is
            // stuck on a dead stream. The TV remote (not a pad) always reveals.
            val noSession = StreamHub.activeGamepad() == null
            val edgeReveal = down && (!fromPad || noSession) &&
                (kc == KeyEvent.KEYCODE_DPAD_LEFT || kc == KeyEvent.KEYCODE_DPAD_UP)
            val longOk = kc == KeyEvent.KEYCODE_DPAD_CENTER && down && event.repeatCount == 1
            val menu = kc == KeyEvent.KEYCODE_MENU && down
            if (edgeReveal || longOk || comboOpen || menu) {
                swallowCenterUntilUp = kc == KeyEvent.KEYCODE_DPAD_CENTER
                openNavbar()
                return true
            }

            // Swallow the OK release (and held repeats) that opened the navbar, so
            // the rail/row doesn't instantly register a click on the focused item.
            if (swallowCenterUntilUp && kc == KeyEvent.KEYCODE_DPAD_CENTER) {
                if (up) swallowCenterUntilUp = false
                return true
            }

            // Then a hardware keyboard's keys to the active host (a real keyboard,
            // incl. one exported via USB/IP, that the platform surfaces here).
            if (forwardKeyToHost(event)) return true
            return super.dispatchKeyEvent(event)
        }

        // --- Navbar OPEN (Peek or Full) ---
        // Couch / remote close gestures: the gamepad combo and Menu both dismiss to the
        // stream. (Back also closes the menu — handled in the OnBackPressedDispatcher
        // callback; see onCreate. Quitting is the rail Exit item.)
        if (comboOpen) {
            closeNavbar()
            return true
        }
        if (down && kc == KeyEvent.KEYCODE_MENU) {
            closeNavbar()
            return true
        }
        if (navMode.value == NavMode.Peek) {
            // Swallow the OK-release that opened Peek via long-press OK, so it doesn't
            // instantly select a rail icon (which would expand to Full).
            if (swallowCenterUntilUp && kc == KeyEvent.KEYCODE_DPAD_CENTER) {
                if (up) swallowCenterUntilUp = false
                return true
            }
            // A second LEFT expands Peek → Full (OK on an icon also expands, via the rail
            // item's onClick → onSelect; Compose handles that).
            if (down && kc == KeyEvent.KEYCODE_DPAD_LEFT) {
                expandToFull()
                return true
            }
            // Navigating the rail (UP/DOWN) restarts the auto-hide timer; Compose moves focus.
            if (down && (kc == KeyEvent.KEYCODE_DPAD_UP || kc == KeyEvent.KEYCODE_DPAD_DOWN)) {
                peekTick.value++
            }
        }
        // Otherwise Compose owns D-pad navigation / OK selection (incl. the Apps grid's
        // own left/right column navigation in Full — which is why RIGHT does not close).
        return super.dispatchKeyEvent(event)
    }

    // Forward a hardware-keyboard key to the active host. Only keys with a real
    // evdev scanCode (a physical keyboard, incl. one exported via USB/IP that the
    // platform surfaces here) are sent; virtual keys (remote/system, scanCode 0)
    // and a small reserved set stay local. Returns true if consumed.
    private fun forwardKeyToHost(event: KeyEvent): Boolean {
        val gp = StreamHub.activeGamepad() ?: return false
        if (event.scanCode == 0) return false // virtual key — leave local
        when (event.keyCode) {
            KeyEvent.KEYCODE_BACK,
            KeyEvent.KEYCODE_HOME,
            KeyEvent.KEYCODE_APP_SWITCH,
            KeyEvent.KEYCODE_VOLUME_UP,
            KeyEvent.KEYCODE_VOLUME_DOWN,
            KeyEvent.KEYCODE_VOLUME_MUTE,
            KeyEvent.KEYCODE_POWER -> return false
        }
        return when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                gp.key(event.scanCode.toUInt(), true); true
            }
            KeyEvent.ACTION_UP -> {
                gp.key(event.scanCode.toUInt(), false); true
            }
            else -> false
        }
    }

    override fun dispatchTouchEvent(event: MotionEvent): Boolean {
        if (event.isFromSource(android.view.InputDevice.SOURCE_MOUSE)) {
            Log.i(TAG, "mouseDiag dispatchTouch source=${event.source} action=${event.actionMasked} btn=${event.buttonState} nav=${navMode.value}")
            // A mouse click's ACTION_DOWN arrives via the touch pipeline. If Compose
            // consumes it before onStreamTouch, forward it here at the Activity level
            // (navbar hidden + streaming) so the click still reaches the host.
            if (navMode.value == NavMode.Hidden && StreamHub.hasActiveSession() && forwardMouse(event)) {
                return true
            }
        }
        return super.dispatchTouchEvent(event)
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (event.isFromSource(android.view.InputDevice.SOURCE_MOUSE)) {
            Log.i(TAG, "mouseDiag genericMotion source=${event.source} action=${event.actionMasked} nav=${navMode.value}")
        }
        // Don't steer the host with the sticks / mouse while the navbar is up.
        if (navMode.value != NavMode.Hidden) return super.dispatchGenericMotionEvent(event)
        // A USB mouse connected to the TV drives the host cursor (hover motion,
        // button press/release, and scroll all arrive here as generic motion).
        if (event.isFromSource(android.view.InputDevice.SOURCE_MOUSE) && forwardMouse(event)) {
            return true
        }
        // Controller-as-cursor: capture the right stick (AXIS_Z/RZ — the same axes
        // ControllerManager maps to the pad's right stick) for the cursor ticker.
        // Still forward the event to the pad: a mouse-UI launcher ignores the pad,
        // and suppressing only the right stick would need per-axis surgery for no
        // real benefit here.
        if (cursorActive() && event.isFromSource(android.view.InputDevice.SOURCE_JOYSTICK)) {
            rStickX = event.getAxisValue(MotionEvent.AXIS_Z)
            rStickY = event.getAxisValue(MotionEvent.AXIS_RZ)
            ensureCursorTicker()
        }
        if (StreamHub.dispatchControllerMotion(event)) return true
        return super.dispatchGenericMotionEvent(event)
    }

    /** Forward one mouse MotionEvent to the host pointer: absolute position over the
     *  video content rect, button press/release (diffed from the button bitmask so
     *  each edge fires once), and wheel scroll. Called from BOTH the generic-motion
     *  path (hover, button events) and [onStreamTouch] (a drag with a button held,
     *  which Android routes through the touch pipeline). Returns true when consumed. */
    private fun forwardMouse(event: MotionEvent): Boolean {
        Log.i(
            TAG,
            "mouseDiag forwardMouse action=${event.actionMasked} btnState=${event.buttonState} " +
                "x=${event.x} y=${event.y} session=${StreamHub.hasActiveSession()} nav=${navMode.value}",
        )
        if (!StreamHub.hasActiveSession()) return false
        if (event.actionMasked == MotionEvent.ACTION_SCROLL) {
            val h = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
            val v = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
            StreamHub.pointerScroll(h, v)
            return true
        }
        // Position → absolute, normalised over the letterboxed video rect (bars
        // clamp to the edges rather than being ignored, so a click never no-ops).
        contentRect()?.let { r ->
            if (r[2] > 0f && r[3] > 0f) {
                val nx = ((event.x - r[0]) / r[2]).coerceIn(0f, 1f)
                val ny = ((event.y - r[1]) / r[3]).coerceIn(0f, 1f)
                StreamHub.pointerAbs(nx, ny)
            }
        }
        val nb = event.buttonState
        if (nb != lastMouseButtons) {
            fun edge(mask: Int, hostButton: Int) {
                val was = (lastMouseButtons and mask) != 0
                val now = (nb and mask) != 0
                if (was != now) StreamHub.pointerButton(hostButton, now)
            }
            edge(MotionEvent.BUTTON_PRIMARY, 0) // left
            edge(MotionEvent.BUTTON_SECONDARY, 1) // right
            edge(MotionEvent.BUTTON_TERTIARY, 2) // middle
            lastMouseButtons = nb
        }
        return true
    }

    // --- Pointer capture (mouse) ---
    // On this TV an absolute mouse's hover/click doesn't reach the Activity's touch
    // / generic-motion dispatch in a usable form (it routes through hover + a system
    // cursor), so a normal mouse can't drive the host. Pointer CAPTURE takes raw
    // mouse input (relative motion + buttons) via onCapturedPointerListener, which
    // bypasses that AND hides the OS cursor (no double-cursor). Held only while
    // streaming full-screen (navbar hidden, window focused, session live); released
    // when the navbar opens so the remote can drive the Trireme menu.
    private var lastCapturedButtons = 0
    // The view that owns pointer capture + the captured-event listener. MUST be the
    // FOCUSED view (the Compose host view, via LocalView) — captured events are
    // dispatched to whatever holds input focus, so the decorView never sees them.
    private var captureView: android.view.View? = null

    /** Request or release mouse pointer-capture to match the current state. Called
     *  on focus change and every navbar transition; idempotent. */
    private fun updatePointerCapture() {
        val v = captureView ?: return
        val want = hasWindowFocus() && navMode.value == NavMode.Hidden &&
            StreamHub.hasActiveSession()
        runCatching {
            if (want) {
                v.requestFocus()
                v.requestPointerCapture()
            } else {
                v.releasePointerCapture()
            }
        }
    }

    /** Raw captured mouse → host pointer: relative motion, buttons (edge-diffed),
     *  and wheel scroll. Returns true (consumed). */
    private fun handleCapturedMouse(event: MotionEvent): Boolean {
        if (!StreamHub.hasActiveSession()) return false
        if (event.actionMasked == MotionEvent.ACTION_SCROLL) {
            StreamHub.pointerScroll(
                event.getAxisValue(MotionEvent.AXIS_HSCROLL),
                event.getAxisValue(MotionEvent.AXIS_VSCROLL),
            )
            return true
        }
        // In capture mode motion is RELATIVE. Prefer the dedicated relative axes;
        // fall back to x/y (relative while captured) for HW that doesn't set them.
        var dx = event.getAxisValue(MotionEvent.AXIS_RELATIVE_X)
        var dy = event.getAxisValue(MotionEvent.AXIS_RELATIVE_Y)
        if (dx == 0f && dy == 0f) {
            dx = event.x; dy = event.y
        }
        // Scale raw device deltas by the user's sensitivity: a 4K host canvas at
        // 1:1 feels sluggish (a lot of physical travel to cross the screen).
        val sens = StreamHub.mouseSensitivity()
        Log.i(TAG, "mouseDiag captured action=${event.actionMasked} dx=$dx dy=$dy sens=$sens btn=${event.buttonState}")
        if (dx != 0f || dy != 0f) StreamHub.pointerRel(dx * sens, dy * sens)
        val nb = event.buttonState
        if (nb != lastCapturedButtons) {
            fun edge(mask: Int, host: Int) {
                if ((lastCapturedButtons and mask) != (nb and mask)) {
                    StreamHub.pointerButton(host, (nb and mask) != 0)
                }
            }
            edge(MotionEvent.BUTTON_PRIMARY, 0)
            edge(MotionEvent.BUTTON_SECONDARY, 1)
            edge(MotionEvent.BUTTON_TERTIARY, 2)
            lastCapturedButtons = nb
        }
        return true
    }

    // ============================================================
    // Touch → host pointer. onTouchEvent (not dispatchTouchEvent) so Compose owns
    // the navbar/scrim/edge-grip gestures first; we only see touches on the bare
    // stream area while the navbar is Hidden. Two modes (Settings → Touch):
    //   absolute — tap/drag to place the cursor + click (touchscreen feel)
    //   trackpad — one-finger drag = relative motion (PointerRel), tap = click,
    //              two fingers = scroll.
    // ============================================================

    /** The letterboxed video content rect in window pixels `[x, y, w, h]`, matching
     *  ShellContent's aspect-fit ("contain") exactly, or null if not laid out yet. */
    private fun contentRect(): FloatArray? {
        val v = window?.decorView ?: return null
        val winW = v.width.toFloat()
        val winH = v.height.toFloat()
        if (winW <= 0f || winH <= 0f) return null
        val aspect = StreamHub.videoAspect.value
        if (aspect <= 0f) return null
        val cw: Float
        val ch: Float
        if (winW / winH < aspect) {
            cw = winW; ch = winW / aspect // fit width, bars top/bottom
        } else {
            ch = winH; cw = winH * aspect // fit height, bars left/right
        }
        return floatArrayOf((winW - cw) / 2f, (winH - ch) / 2f, cw, ch)
    }

    /** Dispatch a stream-area touch to the host pointer. Driven by a Compose
     *  `pointerInteropFilter` on the video box (see ShellContent), which is only
     *  attached while the navbar is Hidden — so this never fights the navbar/edge-grip
     *  gestures. `Activity.onTouchEvent` can't be used: the ComposeView consumes the
     *  events before they reach the Activity. Returns true to consume. */
    fun onStreamTouch(event: MotionEvent): Boolean {
        // A mouse's button-held DRAG lands here (Android routes it through the touch
        // pipeline, not generic-motion). Forward it as mouse, NOT as a finger touch,
        // so the two input models never fight over the host cursor.
        if (event.isFromSource(android.view.InputDevice.SOURCE_MOUSE)) {
            return forwardMouse(event)
        }
        // Always release a held button on UP/CANCEL, so it never sticks on the host.
        val a = event.actionMasked
        if ((a == MotionEvent.ACTION_UP || a == MotionEvent.ACTION_CANCEL) && tPressed) {
            StreamHub.pointerButton(0, false)
            tPressed = false
        }
        if (!StreamHub.hasActiveSession()) return false
        return if (StreamHub.isTouchAbsolute()) handleAbsoluteTouch(event)
        else handleTrackpadTouch(event)
    }

    /** Absolute: the finger IS the cursor. DOWN places it + presses; MOVE drags it;
     *  UP releases. Touches in the letterbox bars are ignored. */
    private fun handleAbsoluteTouch(event: MotionEvent): Boolean {
        val rect = contentRect() ?: return false
        val cx = rect[0]
        val cy = rect[1]
        val cw = rect[2]
        val ch = rect[3]
        fun sendAbs(x: Float, y: Float) {
            val nx = ((x - cx) / cw).coerceIn(0f, 1f)
            val ny = ((y - cy) / ch).coerceIn(0f, 1f)
            StreamHub.pointerAbs(nx, ny)
        }
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                val x = event.x
                val y = event.y
                tInContent = x >= cx && x < cx + cw && y >= cy && y < cy + ch
                if (BuildConfig.DEBUG) {
                    // One line per touch-DOWN so a live pass can confirm the mapping:
                    // if the host cursor lands wrong, compare the content rect/aspect
                    // here against the decoder's "output format WxH" — a mismatch is a
                    // stale/wrong aspect, a correct rect points at the host or gyro.
                    Log.d(
                        TAG,
                        "absTouch DOWN screen=(${event.x},${event.y}) " +
                            "rect=[$cx,$cy,$cw,$ch] aspect=${StreamHub.videoAspect.value} " +
                            "inContent=$tInContent",
                    )
                }
                if (tInContent) {
                    // A deliberate finger-down on the video claims the touch source, so
                    // its motion is forwarded again after a pad had muted it (and so a
                    // stray brush while gaming with a pad doesn't — the pad reclaims on
                    // its next input). Claim BEFORE sendAbs, which is gated on it.
                    StreamHub.claimTouchSource()
                    sendAbs(x, y)
                    StreamHub.pointerButton(0, true)
                    tPressed = true
                }
            }
            MotionEvent.ACTION_MOVE -> if (tInContent) sendAbs(event.x, event.y)
            MotionEvent.ACTION_UP -> if (tPressed) {
                StreamHub.pointerButton(0, false)
                tPressed = false
            }
            MotionEvent.ACTION_CANCEL -> if (tPressed) {
                StreamHub.pointerButton(0, false)
                tPressed = false
            }
        }
        return true
    }

    /** Trackpad: one-finger drag → PointerRel; quick tap → left click; two-finger
     *  drag → scroll. Desktop/mirror streams add: long-press → right-click, and
     *  double-tap-then-hold → left-button drag (both gated by
     *  [StreamHub.isDesktopStream] so they never fire mid-game). */
    private fun handleTrackpadTouch(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                // Deliberate touch → claim the touch source (see handleAbsoluteTouch);
                // trackpad drag/tap is gated on it in StreamHub.pointerRel.
                StreamHub.claimTouchSource()
                tDownTime = event.eventTime
                tDownX = event.x
                tDownY = event.y
                tLastX = event.x
                tLastY = event.y
                tMoved = false
                tScrolling = false
                tLongPressFired = false
                cancelLongPress()
                val desktop = StreamHub.isDesktopStream()
                // Second tap of a double-tap that lands close in time+space → begin a
                // left-button drag: press now, hold through MOVE, release on UP. A
                // quick second tap (no hold/move) then reads as press+release = the
                // second click of a double-click. Desktop-only.
                val isDoubleDown = desktop &&
                    (event.eventTime - tLastTapUpTime) < DOUBLE_TAP_MS &&
                    kotlin.math.abs(event.x - tLastTapUpX) < DOUBLE_TAP_SLOP &&
                    kotlin.math.abs(event.y - tLastTapUpY) < DOUBLE_TAP_SLOP
                if (isDoubleDown) {
                    tDragging = true
                    tLastTapUpTime = 0L // consume so a 3rd tap doesn't chain
                    StreamHub.pointerButton(0, true)
                } else {
                    tDragging = false
                    // Arm long-press → right-click (desktop only).
                    if (desktop) armLongPress()
                }
            }
            MotionEvent.ACTION_POINTER_DOWN -> if (event.pointerCount >= 2) {
                tScrolling = true
                tScrollLastY = (event.getY(0) + event.getY(1)) / 2f
                cancelLongPress()
                // Going to two-finger scroll — abandon any single-finger drag cleanly.
                if (tDragging) {
                    StreamHub.pointerButton(0, false)
                    tDragging = false
                }
            }
            MotionEvent.ACTION_MOVE -> {
                if (tScrolling && event.pointerCount >= 2) {
                    val avgY = (event.getY(0) + event.getY(1)) / 2f
                    val dy = avgY - tScrollLastY
                    tScrollLastY = avgY
                    // Drag down → scroll down (natural). SCROLL_DIV px per notch.
                    StreamHub.pointerScroll(0f, -dy / SCROLL_DIV)
                } else if (!tScrolling) {
                    val dx = event.x - tLastX
                    val dy = event.y - tLastY
                    tLastX = event.x
                    tLastY = event.y
                    if (kotlin.math.abs(event.x - tDownX) > TAP_SLOP ||
                        kotlin.math.abs(event.y - tDownY) > TAP_SLOP
                    ) {
                        tMoved = true
                    }
                    // Cancel a pending long-press only once motion leaves the (larger)
                    // tolerance window, so a small drift during the hold is forgiven.
                    if (kotlin.math.abs(event.x - tDownX) > LONG_PRESS_SLOP ||
                        kotlin.math.abs(event.y - tDownY) > LONG_PRESS_SLOP
                    ) {
                        cancelLongPress()
                    }
                    // Motion moves the cursor; if a drag is in progress the left button
                    // is held, so the host sees a click-drag.
                    StreamHub.pointerRel(dx * TRACKPAD_SENS, dy * TRACKPAD_SENS)
                }
            }
            MotionEvent.ACTION_POINTER_UP -> {
                // Second finger lifted; keep scrolling flag until full UP so the
                // remaining finger doesn't dump a relative jump.
            }
            MotionEvent.ACTION_UP -> {
                cancelLongPress()
                when {
                    tDragging -> {
                        StreamHub.pointerButton(0, false)
                        tDragging = false
                    }
                    // Right-click already fired on the timer → UP emits no left click.
                    tLongPressFired -> {}
                    else -> {
                        val quick = event.eventTime - tDownTime < TAP_MS
                        if (!tScrolling && !tMoved && quick) {
                            StreamHub.pointerButton(0, true)
                            StreamHub.pointerButton(0, false)
                            // Remember this tap so a fast follow-up becomes a double.
                            tLastTapUpTime = event.eventTime
                            tLastTapUpX = tDownX
                            tLastTapUpY = tDownY
                        }
                    }
                }
                tScrolling = false
            }
            MotionEvent.ACTION_CANCEL -> {
                cancelLongPress()
                if (tDragging) {
                    StreamHub.pointerButton(0, false)
                    tDragging = false
                }
                tScrolling = false
            }
        }
        return true
    }

    /** Arm the long-press → right-click timer for the current trackpad press.
     *  Fires a discrete right-click (press+release) plus haptic feedback after
     *  [LONG_PRESS_MS] if the finger is still down, roughly still, and not
     *  scrolling/dragging. Cancelled by [cancelLongPress] on motion/lift/scroll. */
    private fun armLongPress() {
        val r = Runnable {
            if (!tScrolling && !tDragging) {
                tLongPressFired = true
                StreamHub.pointerButton(1, true)
                StreamHub.pointerButton(1, false)
                window.decorView.performHapticFeedback(
                    HapticFeedbackConstants.LONG_PRESS,
                    HapticFeedbackConstants.FLAG_IGNORE_VIEW_SETTING,
                )
            }
            tLongPressRunnable = null
        }
        tLongPressRunnable = r
        touchHandler.postDelayed(r, LONG_PRESS_MS)
    }

    private fun cancelLongPress() {
        tLongPressRunnable?.let { touchHandler.removeCallbacks(it) }
        tLongPressRunnable = null
    }

    // ============================================================
    // Refresh / VRR configuration (needs a Window, so it lives in the shell)
    // ============================================================

    /** Lock the activity to the saved orientation (Settings → Orientation). Default
     *  "auto" keeps the manifest's fullUser behaviour (follow the device); the two
     *  explicit choices pin a landscape so the user can flip the phone to suit their
     *  grip/cable. Read only at onCreate, so changes apply on the next launch. */
    private fun applyOrientationPref() {
        val pref = getSharedPreferences("trireme.settings", Context.MODE_PRIVATE)
            .getString("orientation", "auto")
        requestedOrientation = when (pref) {
            "landscape" -> ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE
            "landscape_reversed" -> ActivityInfo.SCREEN_ORIENTATION_REVERSE_LANDSCAPE
            "portrait" -> ActivityInfo.SCREEN_ORIENTATION_PORTRAIT
            else -> ActivityInfo.SCREEN_ORIENTATION_FULL_USER // "auto"
        }
    }

    /// Enumerate every display mode, request the best one for our content, and
    /// report whether the panel can actually vary its refresh. This is what makes
    /// VRR "capable": on a multi-mode display (120Hz phone, gaming monitor, real
    /// VRR panel) it selects the high-refresh mode and lets the source cadence
    /// drive it; on a single-mode panel (this Bravia) it says so plainly. The
    /// resulting [DisplayCaps] are handed to StreamHub for the decoder.
    private fun configureRefresh(): DisplayCaps {
        @Suppress("DEPRECATION")
        val disp: Display? =
            if (Build.VERSION.SDK_INT >= 30) display else windowManager.defaultDisplay
        if (disp == null) {
            Log.w(TAG, "no Display; cannot configure refresh")
            return DisplayCaps.NONE
        }

        val modes = disp.supportedModes.toList()
        modes.forEach { m ->
            val alts =
                if (Build.VERSION.SDK_INT >= 31) m.alternativeRefreshRates.joinToString() else "n/a"
            Log.i(
                TAG,
                "display mode ${m.modeId}: ${m.physicalWidth}x${m.physicalHeight} " +
                    "@ ${m.refreshRate}Hz (seamless alts=[$alts])",
            )
        }

        val refreshRates = modes.map { it.refreshRate }.distinct().sorted()
        val rangeable = refreshRates.size > 1 ||
            (Build.VERSION.SDK_INT >= 31 && modes.any { it.alternativeRefreshRates.isNotEmpty() })

        if (!rangeable) {
            val hz = refreshRates.firstOrNull()?.let { "%.2f".format(it) } ?: "?"
            Log.w(
                TAG,
                "VRR/HFR unavailable: this display exposes a single fixed ${hz}Hz mode " +
                    "(no alternative refresh rates). setFrameRate/preferredDisplayModeId have " +
                    "nothing to switch to — presenting low-latency at the panel's fixed rate.",
            )
            val m0 = modes.firstOrNull()
            val (pw0, ph0) = landscapeDims(m0?.physicalWidth ?: 0, m0?.physicalHeight ?: 0)
            return DisplayCaps(
                refreshRates, false, m0?.modeId ?: 0, refreshRates.firstOrNull() ?: 0f, pw0, ph0,
            )
        }

        // Capable panel: keep native resolution, take the highest refresh available
        // so there is headroom for the panel to track a variable/lower source rate
        // (true VRR fills the gap; fixed-mode falls back to the integer cadence).
        val native = disp.mode
        val target = modes
            .filter { it.physicalWidth == native.physicalWidth && it.physicalHeight == native.physicalHeight }
            .maxByOrNull { it.refreshRate }
            ?: native

        val lp = window.attributes
        lp.preferredDisplayModeId = target.modeId
        if (Build.VERSION.SDK_INT >= 30) lp.preferredRefreshRate = target.refreshRate
        window.attributes = lp
        Log.i(
            TAG,
            "VRR/HFR engaged: requested mode ${target.modeId} (${target.refreshRate}Hz @ " +
                "${target.physicalWidth}x${target.physicalHeight}); source cadence will drive " +
                "the panel within [${refreshRates.first()}..${refreshRates.last()}]Hz.",
        )
        val (pw, ph) = landscapeDims(target.physicalWidth, target.physicalHeight)
        return DisplayCaps(refreshRates, true, target.modeId, target.refreshRate, pw, ph)
    }

    /** Normalise a mode's physical size to landscape (width ≥ height) — a phone's
     *  natural orientation is portrait, but we always stream landscape, so the host
     *  wants the landscape envelope (e.g. 2712×1220, not 1220×2712). */
    private fun landscapeDims(w: Int, h: Int): Pair<Int, Int> =
        if (w <= 0 || h <= 0) 0 to 0 else maxOf(w, h) to minOf(w, h)

    // The SurfaceView holder callback is registered against THIS activity by the
    // AndroidView factory below.
    private fun bindSurface(view: SurfaceView) {
        view.holder.addCallback(this)
    }

    /**
     * The shell's composition: the full-bleed background [SurfaceView] (video) with
     * the [NavShell] overlay on top. The SurfaceView is an [AndroidView] so the
     * Compose overlay layers above it; the navbar's `navScrim` keeps panel text
     * legible over arbitrary frames. The pane body wires each [NavDest] to its pane.
     *
     * Touch affordances (additive, coexist with the TV D-pad reveal in
     * dispatchKeyEvent — see the contract's Touch section):
     *  - When the navbar is CLOSED, a slim NON-focusable left-edge grip is drawn so a
     *    phone/tablet user can discover + TAP to open (a tap is not an edge-swipe, so
     *    the OS gesture-nav doesn't eat it; no exclusion-rect hack). The grip is
     *    `canFocus = false` so it never steals D-pad focus on a TV.
     *  - When the navbar is OPEN, the dimmed area to the RIGHT of the navbar band is a
     *    tap-to-dismiss scrim. (No custom dismiss swipe — right-edge swipes are system
     *    Back on many phones, which already routes through the Back callback.)
     *  - When the navbar is CLOSED and the active stream is not yet Live, a small status
     *    badge (Connecting / Reconnecting / No active stream) is shown over the surface
     *    as a polite live region, so a full-screen user gets feedback over a black frame.
     */
    @OptIn(ExperimentalComposeUiApi::class)
    @Composable
    private fun ShellContent(
        mode: NavMode,
        navDest: NavDest,
        peekTick: Int,
        store: ServerStore,
        onSelect: (NavDest) -> Unit,
        onCloseToStream: () -> Unit,
    ) {
        // Peek auto-hides after PEEK_TIMEOUT_MS if the user doesn't expand to Full;
        // peekTick (bumped on each rail navigation) restarts the countdown, and reaching
        // Full or Hidden cancels it (changing either key restarts/cancels the effect).
        LaunchedEffect(mode, peekTick) {
            if (mode == NavMode.Peek) {
                delay(PEEK_TIMEOUT_MS)
                if (navMode.value == NavMode.Peek) closeNavbar()
            }
        }
        BoxWithConstraints(Modifier.fillMaxSize()) {
            // Navbar band width — Full uses the responsive band, Peek is just the rail;
            // shared with NavShell so the dismiss-scrim lines up with the band edge.
            val band = if (mode == NavMode.Full) navBandWidth(maxWidth) else RAIL_WIDTH

            // Background video, letterboxed to the TRUE source aspect (from the decoder's
            // output format — see StreamHub.videoAspect) so it never STRETCHES and fills
            // correctly on any panel: a 16:9 stream on a 16:9 TV fills exactly; a 20:9
            // (2712x1220) stream on a 20:9 phone fills exactly. Fit width when the screen
            // is taller than the video, else fit height — i.e. "contain" for any aspect.
            val videoAspect by StreamHub.videoAspect.collectAsState()
            val fitWidth = (maxWidth / maxHeight) < videoAspect
            // Touch → host pointer: only while the navbar is Hidden (so it never
            // fights the navbar/scrim/edge-grip gestures) and on a touch device.
            // pointerInteropFilter hands us raw MotionEvents inside Compose — the
            // ComposeView consumes touches before Activity.onTouchEvent, so that
            // hook can't be used.
            val touchMod = if (mode == NavMode.Hidden && hasTouch) {
                Modifier.pointerInteropFilter { ev -> onStreamTouch(ev) }
            } else {
                Modifier
            }
            Box(
                Modifier.fillMaxSize().then(touchMod),
                contentAlignment = Alignment.Center,
            ) {
                AndroidView(
                    factory = { ctx -> SurfaceView(ctx).also { bindSurface(it) } },
                    modifier = (if (fitWidth) Modifier.fillMaxWidth() else Modifier.fillMaxHeight())
                        .aspectRatio(videoAspect),
                )
            }

            if (mode == NavMode.Hidden) {
                // Status badge over the surface when the active stream isn't showing
                // media yet (cold-start Connecting, graceful Reconnecting, or idle).
                StreamStatusBadge()
                // Touch-only discoverability grip on the left edge → opens the navbar (Peek).
                EdgeOpenHandle(onOpen = { openNavbar() })
            } else {
                // Tap the dimmed stream to the right of the navbar band to dismiss.
                val scrimDesc = stringResource(R.string.nav_close_to_stream)
                Box(
                    Modifier
                        .fillMaxSize()
                        .padding(start = band)
                        // NON-focusable: `.clickable` is focusable by default, and since the
                        // scrim sits to the RIGHT of the pane the D-pad could land on it (focus
                        // escaping the menu onto the invisible stream area). canFocus=false keeps
                        // it touch-only, so RIGHT at the pane's edge simply stays put.
                        .focusProperties { canFocus = false }
                        .clickable(
                            // No ripple — this is a touch scrim, not a focusable target;
                            // the D-pad dismisses via Back / the Stream rail row.
                            indication = null,
                            interactionSource = remember { MutableInteractionSource() },
                            onClick = onCloseToStream,
                        )
                        .semantics { contentDescription = scrimDesc },
                )
            }

            // The navbar + active pane float over the live stream.
            NavShell(
                mode = mode,
                current = navDest,
                onSelect = onSelect,
                onCloseToStream = onCloseToStream,
                // Exit = graceful teardown of every session + finish (the discoverable
                // twin of the double-Back-from-stream exit).
                onExit = { StreamHub.shutdown(); finish() },
                content = { dest ->
                    when (dest) {
                        NavDest.Hosts -> HostsPane(store)
                        NavDest.Apps -> AppsPane(onCloseToStream = onCloseToStream)
                        NavDest.Controllers -> ControllersPane()
                        // Settings' Controllers row now routes back to the
                        // Controllers pane in this same shell instead of starting
                        // the old standalone activity (which is gone). The route
                        // goes through `onSelect` so the rail focus animation
                        // matches a direct click on the Controllers rail item.
                        NavDest.Settings -> SettingsPane(
                            onOpenControllers = { onSelect(NavDest.Controllers) },
                        )
                        // Stream isn't a pane — selecting it closes the navbar (handled
                        // by NavShell routing to onCloseToStream); render nothing.
                        NavDest.Stream -> Unit
                    }
                },
            )

            // Link-degraded indicator (yellow wifi, top-left). Renders over a LIVE
            // stream too, so it sits outside the NavMode branch above.
            LinkHealthBadge()

            // Debug-variant only: live metrics overlay, top-right (clear of the
            // top-left MangoHud in-frame and the top-center status badge). Last
            // child = topmost z-order; touch/D-pad inert (see DebugHud). Gated on
            // the Settings toggle (StreamHub.debugHud) so it can be hidden live.
            val debugHudOn by StreamHub.debugHud.collectAsState()
            if (debugHudOn) {
                DebugHud(Modifier.align(Alignment.TopEnd))
            }
        }
    }

    /**
     * Slim, low-opacity, NON-focusable left-edge grip — a TOUCH discoverability hint
     * for opening the navbar (tap-only; swiping is the system Back gesture, which also
     * opens the navbar). Two guards keep it from cluttering the stream:
     *  - **Touch only:** skipped entirely on a device with no touchscreen (a TV), where
     *    the D-pad UP/LEFT reveal + Back are the path — so it never sits over the video
     *    there. (`canFocus = false` also keeps it off the D-pad anywhere.)
     *  - **Auto-fade:** the visible pill fades out a few seconds after the navbar closes
     *    so it doesn't linger over a live stream; the (invisible) edge tap-target stays,
     *    so the edge is still tappable and the system swipe-back still opens the navbar.
     * This composable re-enters whenever the navbar closes, so the hint re-shows then.
     */
    @Composable
    private fun EdgeOpenHandle(onOpen: () -> Unit) {
        val context = LocalContext.current
        val hasTouch = remember {
            context.packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN)
        }
        if (!hasTouch) return // a TV: no grip over the stream at all

        val desc = stringResource(R.string.nav_open_handle_desc)
        var hintShown by remember { mutableStateOf(true) }
        LaunchedEffect(Unit) {
            delay(EDGE_HINT_MS)
            hintShown = false
        }
        val hintAlpha by animateFloatAsState(
            targetValue = if (hintShown) 1f else 0f,
            label = "edgeHintAlpha",
        )
        Box(
            Modifier
                .fillMaxHeight()
                .width(20.dp)
                .focusProperties { canFocus = false }
                .clickable(
                    indication = null,
                    interactionSource = remember { MutableInteractionSource() },
                    onClick = onOpen,
                )
                .semantics { contentDescription = desc },
            contentAlignment = Alignment.CenterStart,
        ) {
            // The visible grip: a short rounded pill, faint so it doesn't fight the video;
            // fades to 0 after EDGE_HINT_MS (the tap-target Box above stays active).
            Box(
                Modifier
                    .padding(start = 3.dp)
                    .width(4.dp)
                    .fillMaxHeight(0.16f)
                    .alpha(hintAlpha)
                    .clip(RoundedCornerShape(2.dp))
                    .background(MaterialTheme.colorScheme.onSurface.copy(alpha = 0.30f)),
            )
        }
    }

    /**
     * Active-stream status overlay, shown over the surface ONLY while the navbar is
     * closed and the active session is not Live (or there is none). Reflects the
     * real media state now that the hub flips to Live on the first decoded frame
     * (not merely on a spawned QUIC thread). A polite live region so TalkBack
     * announces Connecting → Reconnecting → (gone, on Live).
     */
    @Composable
    private fun StreamStatusBadge() {
        val status by StreamHub.activeStatus.collectAsState()
        val activeId by StreamHub.activeHostId.collectAsState()
        val connections by StreamHub.connections.collectAsState()

        // Live = full media playing; nothing to overlay. Hide the badge.
        if (status == ConnStatus.Live) return

        val text = when {
            activeId == null -> stringResource(R.string.stream_idle)
            status == ConnStatus.Connecting -> stringResource(R.string.stream_status_connecting)
            status == ConnStatus.Reconnecting -> stringResource(R.string.stream_status_reconnecting)
            status == ConnStatus.Preempted -> stringResource(R.string.stream_status_preempted)
            status == ConnStatus.Failed -> stringResource(R.string.stream_status_failed)
            status == ConnStatus.Disconnected -> {
                val name = connections.firstOrNull { it.hostId == activeId }?.name
                if (name != null) stringResource(R.string.stream_active_on, name)
                else stringResource(R.string.stream_idle)
            }
            else -> stringResource(R.string.stream_idle)
        }

        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopCenter) {
            Box(
                Modifier
                    .padding(top = 24.dp)
                    .clip(RoundedCornerShape(TriremeTokens.tileCorner))
                    .background(MaterialTheme.colorScheme.surfaceContainerHighest.copy(alpha = 0.92f))
                    .padding(horizontal = 16.dp, vertical = 8.dp)
                    .semantics { liveRegion = LiveRegionMode.Polite },
            ) {
                Text(
                    text,
                    style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.onSurface,
                )
            }
        }
    }

    /**
     * Subtle "link degraded" indicator: a small yellow wifi glyph in the top-left
     * corner while the host reports it's shedding frames (DG_LINK_HEALTH). Unlike
     * [StreamStatusBadge] it renders during a LIVE stream (that's the point — the
     * link is struggling mid-game), so it's a sibling of the shell, not gated on
     * NavMode/connection status. Non-focusable; clear of the top-center status
     * badge and the top-right debug HUD.
     */
    @Composable
    private fun LinkHealthBadge() {
        val degraded by StreamHub.linkDegraded.collectAsState()
        if (!degraded) return
        val desc = stringResource(R.string.stream_link_degraded_desc)
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopStart) {
            Box(
                Modifier
                    .padding(top = 24.dp, start = 16.dp)
                    .clip(RoundedCornerShape(TriremeTokens.tileCorner))
                    .background(MaterialTheme.colorScheme.surfaceContainerHighest.copy(alpha = 0.92f))
                    .padding(6.dp)
                    .focusProperties { canFocus = false }
                    .semantics {
                        contentDescription = desc
                        liveRegion = LiveRegionMode.Polite
                    },
            ) {
                Icon(
                    Icons.Filled.Wifi,
                    contentDescription = null,
                    tint = Color(0xFFFFC107),
                    modifier = Modifier.size(20.dp),
                )
            }
        }
    }
}
