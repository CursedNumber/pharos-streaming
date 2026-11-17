// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme.ui.panes

import android.content.Context
import android.content.Intent
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background

// Note: the legacy ControllersActivity is gone; the Controllers row now routes
// through `onOpenControllers` to the in-shell Controllers pane (see NavShell /
// MainActivity). The Intent import below is still needed for USB export.
import androidx.compose.foundation.clickable
import androidx.compose.foundation.focusable
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.github.cursednumber.trireme.R
import io.github.cursednumber.trireme.UsbExportActivity
import io.github.cursednumber.trireme.UsbExportService
import io.github.cursednumber.trireme.UsbStore
import io.github.cursednumber.trireme.stream.PaceMode
import io.github.cursednumber.trireme.stream.StreamHub
import io.github.cursednumber.trireme.ui.TriremeTokens
import io.github.cursednumber.trireme.ui.triremeFocus
import kotlin.math.roundToInt

/**
 * The four pace presets surfaced by the pace row. Mirrors `io.github.cursednumber.trireme.stream.PaceMode`; kept
 * as a UI-local enum so the pane's option set stays decoupled from the decoder's pace enum. The
 * integration agent maps the persisted choice onto the live decoder via StreamHub.
 */
private enum class PaceChoice { LowLatency, Balanced, Smooth, Vrr }

private const val PREFS = "trireme.settings"
private const val KEY_PACE = "pace"
private const val KEY_ORIENTATION = "orientation"

/**
 * Settings pane — focusable ROWS (never cards; a clickable container traps the D-pad, see
 * lesson-compose-tv-focus). Rows: a pace selector (cycles a global preset on OK), Controllers
 * (routed back to the in-shell Controllers pane via [onOpenControllers]), USB export
 * (→ [UsbExportActivity]), and an about block. Each navigating row carries a trailing chevron
 * and announces "opens a separate screen"; the pace row exposes its current value via
 * `stateDescription`.
 *
 * [onOpenControllers] defaults to a no-op so this composable stays trivially previewable
 * and tolerant of being instantiated outside the shell.
 */
@Composable
fun SettingsPane(onOpenControllers: () -> Unit = {}) {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences(PREFS, Context.MODE_PRIVATE) }

    // Pace is persisted locally; the integration agent reads it back into the decoder.
    var pace by remember {
        mutableStateOf(
            runCatching { PaceChoice.valueOf(prefs.getString(KEY_PACE, null) ?: "Vrr") }
                .getOrDefault(PaceChoice.Vrr),
        )
    }

    // Stream-time gyro opt-in. Default off — the host's IMU reader stays idle
    // unless the user explicitly asks for gyro relay (cheap when nothing wants
    // it). StreamHub owns the source of truth (also read at process start) and
    // applies on the NEXT dial.
    var gyroOn by remember { mutableStateOf(StreamHub.isGyroEnabled()) }
    var phoneGyro by remember { mutableStateOf(StreamHub.isPhoneGyro()) }
    // Touch-pointer mode (absolute vs trackpad). Applies immediately — no re-dial.
    var touchAbs by remember { mutableStateOf(StreamHub.isTouchAbsolute()) }
    // Controller-as-cursor: the manual on/off (tracks the shared StateFlow so the
    // on-screen indicator stays in sync), and the hold-Start trigger option.
    val cursorOn by StreamHub.cursorMode.collectAsState()
    var holdStartCursor by remember { mutableStateOf(StreamHub.isHoldStartForCursor()) }
    // Physical-mouse sensitivity, cycled through presets (D-pad friendly).
    val mouseSens by StreamHub.mouseSensitivityFlow.collectAsState()

    // Master switches (both default off). USB/IP is persisted in UsbStore; controller
    // handling lives in StreamHub (which rebuilds the live ControllerManager on change).
    // These mirror the switches on the USB export screen / Controllers pane — all read
    // and write the same source of truth, so they stay consistent.
    var usbIpOn by remember { mutableStateOf(UsbStore(context).usbIpEnabled()) }
    var ctlHandlingOn by remember { mutableStateOf(StreamHub.isControllerHandling()) }

    // Scrollable: the pane outgrew the screen (3 quality sliders + toggles + nav
    // rows), so without this the lower rows (HUD toggle, USB) were unreachable.
    // foundation's verticalScroll auto-brings the D-pad-focused row into view, so
    // the remote can walk top→bottom. Grouped under category headers for legibility.
    Column(
        Modifier.fillMaxSize().padding(20.dp).verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            stringResource(R.string.settings_title),
            style = MaterialTheme.typography.headlineSmall,
            fontWeight = FontWeight.Bold,
            color = MaterialTheme.colorScheme.onSurface,
        )
        Spacer(Modifier.height(8.dp))
        SectionHeader("Display")

        // --- Pace selector: OK cycles to the next AVAILABLE preset ----------
        // VRR is only meaningful on a panel that can actually vary/switch refresh.
        // We detect that from DisplayCaps (Display.getSupportedModes) at shell start —
        // before any stream connects — and drop VRR on a single-mode fixed-refresh
        // panel (e.g. the Sony X90J at a fixed 60 Hz), where it silently behaves as
        // LOW_LATENCY anyway. Caps unknown (no display yet) → show everything.
        val caps by StreamHub.displayCaps.collectAsState()
        val capsKnown = caps.refreshRates.isNotEmpty()
        val choices = if (!capsKnown || caps.rangeable) PaceChoice.entries.toList()
            else PaceChoice.entries.filter { it != PaceChoice.Vrr }
        // Coerce a previously-saved VRR to a usable preset on a fixed-refresh panel.
        val effective = if (pace in choices) pace else PaceChoice.LowLatency
        val paceValue = stringResource(paceLabelRes(effective))
        SettingRow(
            title = stringResource(R.string.settings_pace),
            summary = stringResource(R.string.settings_pace_summary),
            trailing = paceValue,
            // a11y: announce the current pace as the row's state, not just the visual trailing.
            state = stringResource(R.string.settings_pace_state, paceValue),
            onClick = {
                val next = choices[(choices.indexOf(effective) + 1) % choices.size]
                pace = next
                prefs.edit().putString(KEY_PACE, next.name).apply()
                // Apply live: rebuild the active decoder with the new pace.
                StreamHub.setPace(next.toPaceMode())
            },
        )
        // Detected panel refresh — transparency about why VRR may be absent.
        if (capsKnown) {
            val hz = caps.refreshRates
            val caption = if (caps.rangeable) {
                stringResource(R.string.settings_display_range, hz.last().roundToInt())
            } else {
                stringResource(R.string.settings_display_fixed, hz.first().roundToInt())
            }
            Text(
                caption,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 4.dp, bottom = 4.dp),
            )
        }

        // --- Orientation: OK cycles Auto → Landscape → Landscape (flipped) →
        // Portrait. Applies on the NEXT launch (MainActivity reads it once at
        // onCreate) so we never rotate mid-session. Portrait also makes the client
        // report a portrait envelope so the host renders portrait (apps reflow),
        // not a letterboxed landscape. Auto = follow the device (default).
        var orientation by remember {
            mutableStateOf(prefs.getString(KEY_ORIENTATION, "auto") ?: "auto")
        }
        val orientationLabel = when (orientation) {
            "landscape" -> "Landscape"
            "landscape_reversed" -> "Landscape (flipped)"
            "portrait" -> "Portrait"
            else -> "Auto"
        }
        SettingRow(
            title = "Orientation",
            summary = "Lock, flip, or go portrait — applies on next launch",
            trailing = orientationLabel,
            state = orientationLabel,
            onClick = {
                val order = listOf("auto", "landscape", "landscape_reversed", "portrait")
                val next = order[(order.indexOf(orientation).coerceAtLeast(0) + 1) % order.size]
                orientation = next
                prefs.edit().putString(KEY_ORIENTATION, next).apply()
            },
        )

        SectionHeader("Video & quality")
        // --- Resolution range (Phase 2): Min..Max on ONE dual-thumb slider ----
        // D-pad (◀▶ move the active thumb, OK switches Min⇄Max), touch, and mouse
        // all drive it. The client sends [floor, target] live; the host adapts the
        // ladder within it. StreamHub owns the source of truth (persisted + pushed
        // on each connect); these locals mirror it for rendering.
        val (resTarget0, resFloor0) = remember { StreamHub.resolutionPref() }
        var maxIdx by remember { mutableStateOf(targetToStop(resTarget0)) }
        var minIdx by remember { mutableStateOf(floorToStop(resFloor0)) }
        ResolutionRangeRow(minIdx = minIdx, maxIdx = maxIdx) { mn, mx ->
            minIdx = mn
            maxIdx = mx
            StreamHub.setResolutionPref(stopToTarget(mx), stopToFloor(mn))
        }

        // --- Bitrate range (Phase 3): Min..Max on ONE dual-thumb slider --------
        // The PRIMARY auto-recover lever. The client sends [min, max] kbps live; the
        // host adapts bitrate within it (max=0 = Auto, host-owned). Same dual-thumb
        // widget, same three input modalities. StreamHub owns the persisted source
        // of truth.
        val (bMin0, bMax0) = remember { StreamHub.bitratePref() }
        var bMaxIdx by remember { mutableStateOf(maxToStop(bMax0)) }
        var bMinIdx by remember { mutableStateOf(minToStop(bMin0)) }
        BitrateRangeRow(minIdx = bMinIdx, maxIdx = bMaxIdx) { mn, mx ->
            bMinIdx = mn
            bMaxIdx = mx
            StreamHub.setBitratePref(stopToMin(mn), stopToMax(mx))
        }

        // --- Framerate range (Phase 3): Min..Max, DERIVED from the panel refresh --
        // The stops come from the panel's reported max refresh (from DisplayCaps), so
        // a 60 Hz panel offers up to 60 and a 120 Hz one up to 120. The client sends
        // [min, max] fps live; the host caps the encode pace (and direct-launched game
        // render) to it, never above the physical panel rate. Picking the top stop =
        // Auto (track the panel). Same dual-thumb widget + three input modalities.
        val panelMaxHz = (caps.refreshRates.maxOrNull()?.roundToInt() ?: 60).coerceAtLeast(30)
        val fpsStops = remember(panelMaxHz) { buildFpsStops(panelMaxHz) }
        if (fpsStops.size >= 2) {
            val (fpsMin0, fpsMax0) = remember { StreamHub.fpsPref() }
            var fMaxIdx by remember(fpsStops) {
                mutableStateOf(fpsToIdx(fpsStops, fpsMax0, fpsStops.lastIndex))
            }
            var fMinIdx by remember(fpsStops) {
                mutableStateOf(fpsToIdx(fpsStops, fpsMin0, 0).coerceAtMost(fpsStops.size - 2))
            }
            RangeSliderRow(
                title = "Framerate",
                stopLabel = { "${fpsStops[it]} fps" },
                n = fpsStops.size,
                minIdx = fMinIdx,
                maxIdx = fMaxIdx,
            ) { mn, mx ->
                fMinIdx = mn
                fMaxIdx = mx
                // Top stop = the panel max → send Auto (0) so it tracks the panel;
                // any lower stop is an explicit cap.
                val maxFps = if (mx == fpsStops.lastIndex) 0 else fpsStops[mx]
                StreamHub.setFpsPref(fpsStops[mn], maxFps)
            }
        }

        SectionHeader("Input")
        // --- Gyro relay opt-in (stream-time) --------------------------------
        // Off by default; OK toggles. Applies on next stream connect (the host
        // reads `wants_gyro` from the auth handshake, once).
        val gyroStateLabel = stringResource(
            if (gyroOn) R.string.settings_gyro_state_on else R.string.settings_gyro_state_off,
        )
        SettingRow(
            title = stringResource(R.string.settings_gyro),
            summary = stringResource(R.string.settings_gyro_summary),
            trailing = gyroStateLabel,
            state = gyroStateLabel,
            onClick = {
                val next = !gyroOn
                gyroOn = next
                StreamHub.setGyroEnabled(next)
            },
        )

        // --- Gyro source: controller IMU or this phone's own sensors ---------
        // Applies live. Phone = a phone in a clip-on controller, or a pad with no
        // gyro: the phone's motion drives the game (DSU) or the cursor.
        val gyroSourceLabel = if (phoneGyro) "Phone" else "Controller"
        SettingRow(
            title = "Gyro source",
            summary = "Where gyro comes from: the controller's motion sensor, or " +
                "this phone's own (for a phone clipped into a controller). Needs " +
                "the gyro setting above.",
            trailing = gyroSourceLabel,
            state = gyroSourceLabel,
            onClick = {
                val next = !phoneGyro
                phoneGyro = next
                StreamHub.setPhoneGyro(next)
            },
        )

        // --- Touch pointer mode: absolute (tap-to-position) vs trackpad (drag) ---
        // OK cycles the two. Applies immediately (no re-dial) — it only changes how
        // the shell maps finger touches to host pointer events.
        val touchLabel = if (touchAbs) "Absolute" else "Trackpad"
        SettingRow(
            title = "Touch pointer",
            summary = "How a finger drives the host cursor: Absolute = tap where you " +
                "want; Trackpad = drag to move + tap to click, two fingers to scroll.",
            trailing = touchLabel,
            state = touchLabel,
            onClick = {
                val next = !touchAbs
                touchAbs = next
                StreamHub.setTouchAbsolute(next)
            },
        )

        // --- Controller as cursor: manual on/off + the hold-Start trigger ---
        // For mouse-driven UIs (a game launcher, a menu, the desktop) on a TV with
        // no touchscreen. Right stick moves the host cursor, A = left / B = right
        // click. Applies immediately.
        SettingRow(
            title = "Controller as cursor",
            summary = "Right stick moves the host mouse; A = left click, B = right click. " +
                "For launcher dialogs and menus on a TV with no touchscreen.",
            trailing = if (cursorOn) "On" else "Off",
            state = if (cursorOn) "On" else "Off",
            onClick = { StreamHub.setCursorMode(!cursorOn) },
        )
        SettingRow(
            title = "Hold Start for cursor",
            summary = "Hold Start ~4.5s to toggle controller-as-cursor on/off (a quick " +
                "Start tap still works as a normal pad button). Off by default.",
            trailing = if (holdStartCursor) "On" else "Off",
            state = if (holdStartCursor) "On" else "Off",
            onClick = {
                val next = !holdStartCursor
                holdStartCursor = next
                StreamHub.setHoldStartForCursor(next)
            },
        )
        // --- Mouse sensitivity: click to cycle presets (higher = faster cursor) --
        // A 4K host canvas at 1:1 feels sluggish; this multiplies raw mouse motion.
        SettingRow(
            title = "Mouse sensitivity",
            summary = "How far the host cursor moves per mouse motion. Higher is faster; " +
                "raise it if the pointer feels sluggish on a 4K screen. Tap to cycle.",
            trailing = "${mouseSens}x",
            state = "${mouseSens}x",
            onClick = {
                val stops = listOf(1.5f, 2.0f, 2.5f, 3.0f, 4.0f, 5.0f)
                val next = stops.firstOrNull { it > mouseSens + 0.01f } ?: stops.first()
                StreamHub.setMouseSensitivity(next)
            },
        )

        // --- Controller handling master toggle (default off → raw forwarding) -
        val ctlStateLabel = stringResource(
            if (ctlHandlingOn) R.string.settings_gyro_state_on else R.string.settings_gyro_state_off,
        )
        SettingRow(
            title = stringResource(R.string.settings_controller_handling),
            summary = stringResource(R.string.settings_controller_handling_summary),
            trailing = ctlStateLabel,
            state = ctlStateLabel,
            onClick = {
                val next = !ctlHandlingOn
                ctlHandlingOn = next
                StreamHub.setControllerHandling(next)
            },
        )

        SectionHeader("Overlay")
        // --- Debug HUD toggle (show/hide the on-screen latency+fps overlay) --
        var debugHudOn by remember { mutableStateOf(StreamHub.isDebugHud()) }
        val debugHudLabel = stringResource(
            if (debugHudOn) R.string.settings_gyro_state_on else R.string.settings_gyro_state_off,
        )
        SettingRow(
            title = "Debug HUD",
            summary = "Show the on-screen latency & fps overlay",
            trailing = debugHudLabel,
            state = debugHudLabel,
            onClick = {
                val next = !debugHudOn
                debugHudOn = next
                StreamHub.setDebugHud(next)
            },
        )

        // --- Controllers → in-shell pane via the nav action -----------------
        SettingRow(
            title = stringResource(R.string.settings_controllers),
            summary = stringResource(R.string.settings_controllers_summary),
            opensScreen = true,
            onClick = onOpenControllers,
        )

        SectionHeader("System")
        // --- USB/IP forwarding master toggle (default off) ------------------
        val usbStateLabel = stringResource(
            if (usbIpOn) R.string.settings_gyro_state_on else R.string.settings_gyro_state_off,
        )
        SettingRow(
            title = stringResource(R.string.settings_usbip),
            summary = stringResource(R.string.settings_usbip_summary),
            trailing = usbStateLabel,
            state = usbStateLabel,
            onClick = {
                val next = !usbIpOn
                usbIpOn = next
                UsbStore(context).setUsbIpEnabled(next)
                if (next) UsbExportService.ensureRunning(context) else UsbExportService.stop(context)
            },
        )

        // --- USB export → UsbExportActivity ---------------------------------
        SettingRow(
            title = stringResource(R.string.settings_usb),
            summary = stringResource(R.string.settings_usb_summary),
            opensScreen = true,
            onClick = {
                context.startActivity(Intent(context, UsbExportActivity::class.java))
            },
        )

        // --- About (display-only row; still focusable so it's reachable) ----
        AboutRow(version = remember { versionName(context) })
    }
}

// Resolution-ladder stops (ascending). The last (0) is "Native"/Auto, reachable
// only by the MAX thumb; the MIN thumb tops out one stop below it (1440p).
private val RES_STOPS = listOf(540, 720, 1080, 1440, 0)

private fun resStopLabel(idx: Int): String =
    if (RES_STOPS[idx] == 0) "Native" else "${RES_STOPS[idx]}p"

/** MAX thumb stop index → `target_h` (0 = Auto/native). */
private fun stopToTarget(idx: Int): Int = RES_STOPS[idx.coerceIn(0, RES_STOPS.lastIndex)]

/** MIN thumb stop index → `floor_h` (never the Native stop). */
private fun stopToFloor(idx: Int): Int = RES_STOPS[idx.coerceIn(0, RES_STOPS.size - 2)]

/** `target_h` (0 = Auto) → MAX thumb stop index. */
private fun targetToStop(targetH: Int): Int =
    if (targetH == 0) RES_STOPS.lastIndex
    else RES_STOPS.indexOf(targetH).let { if (it < 0) RES_STOPS.lastIndex else it }

/** `floor_h` → MIN thumb stop index (0..n-2; unknown/0 → lowest stop). */
private fun floorToStop(floorH: Int): Int =
    (if (floorH == 0) 0 else RES_STOPS.indexOf(floorH).let { if (it < 0) 0 else it })
        .coerceIn(0, RES_STOPS.size - 2)

// Bitrate stops in kbps (ascending). The last (0) is "Auto" (host-owned),
// reachable only by the MAX thumb; the MIN thumb tops out one stop below it
// (300 Mbps). The MIN thumb's lowest stop (5 Mbps) maps to the on-wire "min" the
// host won't auto-drop below. The 80+ stops assume a wired/strong-WiFi path; the
// host clamps a cap it can't honor (client_max_bitrate_kbps) and its net
// controller adapts below it.
private val BITRATE_STOPS_KBPS =
    listOf(5_000, 10_000, 20_000, 35_000, 50_000, 80_000, 150_000, 300_000, 0)

private fun bitrateStopLabel(idx: Int): String =
    if (BITRATE_STOPS_KBPS[idx] == 0) "Auto" else "${BITRATE_STOPS_KBPS[idx] / 1000} Mbps"

/** MAX thumb stop index → `max_kbps` (0 = Auto). */
private fun stopToMax(idx: Int): Int =
    BITRATE_STOPS_KBPS[idx.coerceIn(0, BITRATE_STOPS_KBPS.lastIndex)]

/** MIN thumb stop index → `min_kbps` (never the Auto stop). */
private fun stopToMin(idx: Int): Int =
    BITRATE_STOPS_KBPS[idx.coerceIn(0, BITRATE_STOPS_KBPS.size - 2)]

/** `max_kbps` (0 = Auto) → MAX thumb stop index. */
private fun maxToStop(maxKbps: Int): Int =
    if (maxKbps == 0) BITRATE_STOPS_KBPS.lastIndex
    else BITRATE_STOPS_KBPS.indexOf(maxKbps).let {
        if (it < 0) BITRATE_STOPS_KBPS.lastIndex else it
    }

/** `min_kbps` → MIN thumb stop index (0..n-2; unknown/0 → lowest stop). */
private fun minToStop(minKbps: Int): Int =
    (if (minKbps == 0) 0 else BITRATE_STOPS_KBPS.indexOf(minKbps).let { if (it < 0) 0 else it })
        .coerceIn(0, BITRATE_STOPS_KBPS.size - 2)

// Framerate stops (ascending), DERIVED from the panel's reported max refresh: a
// fixed candidate ladder filtered to <= the panel rate, with the panel rate itself
// guaranteed as the top stop. So a 60 Hz panel → [24,30,48,60]; a 120 Hz one →
// [24,30,48,60,90,120]. The top stop is the panel max (selecting it = Auto).
private fun buildFpsStops(panelMaxHz: Int): List<Int> {
    val candidates = listOf(24, 30, 48, 60, 90, 120, 144)
    val base = candidates.filter { it <= panelMaxHz }.ifEmpty { listOf(panelMaxHz) }
    return if (base.last() < panelMaxHz) base + panelMaxHz else base
}

/** An fps value → its stop index in [stops]; `0` (unset) or unknown → [default]. */
private fun fpsToIdx(stops: List<Int>, fps: Int, default: Int): Int =
    if (fps == 0) default else stops.indexOf(fps).let { if (it < 0) default else it }

@Composable
private fun BitrateRangeRow(
    minIdx: Int,
    maxIdx: Int,
    onChange: (minIdx: Int, maxIdx: Int) -> Unit,
) = RangeSliderRow(
    title = "Bitrate",
    stopLabel = ::bitrateStopLabel,
    n = BITRATE_STOPS_KBPS.size,
    minIdx = minIdx,
    maxIdx = maxIdx,
    onChange = onChange,
)

/**
 * Dual-thumb resolution range on a SINGLE slider (the "min..max" bonus): the left
 * thumb is the minimum the ladder may auto-drop to, the right thumb the target
 * ceiling (rightmost = Native/Auto). Works with all three input modalities —
 *  - **D-pad** (Android TV): the ROW is the focus target; ◀/▶ move the *active*
 *    thumb, OK (center/enter) switches which thumb is active. ◀/▶ at a thumb's
 *    limit are NOT consumed, so focus can still escape the row.
 *  - **touch / mouse**: tap or drag the track — the nearer thumb follows the
 *    nearest stop (mouse is just a pointer to Compose, so the same gesture path
 *    serves both).
 *
 * Discrete stops only (the host's ladder rungs); [onChange] fires with the new
 * `(minIdx, maxIdx)` whenever either thumb moves.
 */
@Composable
private fun ResolutionRangeRow(
    minIdx: Int,
    maxIdx: Int,
    onChange: (minIdx: Int, maxIdx: Int) -> Unit,
) = RangeSliderRow(
    title = "Resolution",
    stopLabel = ::resStopLabel,
    n = RES_STOPS.size,
    minIdx = minIdx,
    maxIdx = maxIdx,
    onChange = onChange,
)

/**
 * Generic dual-thumb range on a SINGLE slider (the "min..max" bonus), driving
 * discrete [stopLabel]-named stops. Shared by Resolution and Bitrate. The left
 * thumb is the floor (never the top stop), the right thumb the ceiling. Works with
 * all three input modalities —
 *  - **D-pad** (Android TV): the ROW is the focus target; ◀/▶ move the *active*
 *    thumb, OK switches which thumb is active; ◀/▶ at a thumb's limit are NOT
 *    consumed, so focus can still escape the row.
 *  - **touch / mouse**: tap or drag the track — the nearer thumb follows.
 *
 * [onChange] fires with the new `(minIdx, maxIdx)` whenever either thumb moves.
 */
@Composable
private fun RangeSliderRow(
    title: String,
    stopLabel: (Int) -> String,
    n: Int,
    minIdx: Int,
    maxIdx: Int,
    onChange: (minIdx: Int, maxIdx: Int) -> Unit,
) {
    val minCap = n - 2 // the MIN thumb can't sit on the top (Native/Uncapped) stop
    var focused by remember { mutableStateOf(false) }
    // Which thumb the D-pad / a fresh touch drives. Start on Max (the common edit).
    var activeMax by remember { mutableStateOf(true) }
    // The touch/drag gesture detectors live in a long-lived pointerInput(widthPx)
    // coroutine that only restarts when the width changes — so it would capture the
    // FIRST minIdx/maxIdx forever and clobber the other thumb after any later move.
    // rememberUpdatedState lets those closures always read the CURRENT values
    // without restarting the coroutine (which would cancel an in-flight drag). The
    // onKeyEvent + Canvas blocks recompose every frame, so they read fresh already.
    val minLive = rememberUpdatedState(minIdx)
    val maxLive = rememberUpdatedState(maxIdx)

    // Clamp + keep ordered (min ≤ max), then publish.
    fun emit(rawMin: Int, rawMax: Int) {
        val mx = rawMax.coerceIn(0, n - 1)
        val mn = rawMin.coerceIn(0, minCap).coerceAtMost(mx)
        onChange(mn, mx.coerceAtLeast(mn))
    }

    val trackColor = MaterialTheme.colorScheme.surfaceVariant
    val rangeColor = MaterialTheme.colorScheme.primary
    val ringColor = MaterialTheme.colorScheme.onSurface

    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .onFocusChanged { focused = it.isFocused }
            .focusable()
            .triremeFocus(focused)
            .onKeyEvent { ev ->
                if (ev.type != KeyEventType.KeyDown) return@onKeyEvent false
                when (ev.key) {
                    // Move the active thumb; only CONSUME the event if it actually
                    // moved, so ◀/▶ at a limit still bubble up for focus escape.
                    Key.DirectionLeft -> {
                        val moved = if (activeMax) maxIdx > minIdx else minIdx > 0
                        if (moved) {
                            if (activeMax) emit(minIdx, maxIdx - 1) else emit(minIdx - 1, maxIdx)
                        }
                        moved
                    }
                    Key.DirectionRight -> {
                        val moved =
                            if (activeMax) maxIdx < n - 1 else minIdx < minOf(maxIdx, minCap)
                        if (moved) {
                            if (activeMax) emit(minIdx, maxIdx + 1) else emit(minIdx + 1, maxIdx)
                        }
                        moved
                    }
                    // OK switches which thumb ◀/▶ drives. Up/Down bubble (row nav).
                    Key.DirectionCenter, Key.Enter, Key.NumPadEnter -> {
                        activeMax = !activeMax
                        true
                    }
                    else -> false
                }
            }
            .semantics {
                role = Role.Button
                stateDescription = "Min ${stopLabel(minIdx)}, Max ${stopLabel(maxIdx)}"
            }
            .padding(horizontal = 14.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                title,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Text(
                "Min ${stopLabel(minIdx)}  —  Max ${stopLabel(maxIdx)}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(10.dp))
            // The track: touch + mouse tap/drag move the nearer thumb. Sized to a
            // known px width so the gesture maps x → nearest stop.
            BoxWithConstraints(Modifier.fillMaxWidth().height(26.dp)) {
                val widthPx = with(LocalDensity.current) { maxWidth.toPx() }
                fun stopAtX(x: Float): Int =
                    if (widthPx <= 0f) 0 else ((x / widthPx) * (n - 1)).roundToInt().coerceIn(0, n - 1)
                Canvas(
                    Modifier
                        .fillMaxSize()
                        .pointerInput(widthPx) {
                            detectTapGestures { off ->
                                val t = stopAtX(off.x)
                                val mn = minLive.value
                                val mx = maxLive.value
                                // Drive whichever thumb is nearer the tap.
                                activeMax = kotlin.math.abs(t - mx) <= kotlin.math.abs(t - mn)
                                if (activeMax) emit(mn, t) else emit(t, mx)
                            }
                        }
                        .pointerInput(widthPx) {
                            var dragMax = true
                            detectHorizontalDragGestures(
                                onDragStart = { off ->
                                    val t = stopAtX(off.x)
                                    dragMax = kotlin.math.abs(t - maxLive.value) <=
                                        kotlin.math.abs(t - minLive.value)
                                    activeMax = dragMax
                                },
                            ) { change, _ ->
                                val t = stopAtX(change.position.x)
                                if (dragMax) emit(minLive.value, t) else emit(t, maxLive.value)
                            }
                        },
                ) {
                    val cy = size.height / 2f
                    val stepX = if (n > 1) size.width / (n - 1) else 0f
                    fun cx(idx: Int) = stepX * idx
                    drawLine(trackColor, Offset(0f, cy), Offset(size.width, cy), strokeWidth = 6f)
                    drawLine(rangeColor, Offset(cx(minIdx), cy), Offset(cx(maxIdx), cy), strokeWidth = 6f)
                    for (i in 0 until n) {
                        drawCircle(trackColor, radius = 4f, center = Offset(cx(i), cy))
                    }
                    val r = 15f
                    drawCircle(rangeColor, radius = r, center = Offset(cx(minIdx), cy))
                    drawCircle(rangeColor, radius = r, center = Offset(cx(maxIdx), cy))
                    if (focused) {
                        val ax = if (activeMax) cx(maxIdx) else cx(minIdx)
                        drawCircle(ringColor, radius = r + 5f, center = Offset(ax, cy), style = Stroke(width = 3f))
                    }
                }
            }
            if (focused) {
                Text(
                    "◀ ▶ move ${if (activeMax) "Max" else "Min"} · OK switches thumb",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
        }
    }
}

/**
 * A non-focusable category label that groups the rows below it (Display / Video &
 * quality / Input / Overlay / System). Purely visual — the D-pad skips it and
 * lands on the next real row.
 */
@Composable
private fun SectionHeader(text: String) {
    Spacer(Modifier.height(8.dp))
    Text(
        text.uppercase(),
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.Bold,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 4.dp, bottom = 2.dp),
    )
}

/**
 * A focusable settings row. The ROW is the single D-pad/click target (no nested focusables).
 * Pass either [trailing]+[state] for an inline-value row (e.g. pace), or [opensScreen] for a
 * navigating row (trailing chevron + "opens a separate screen" semantics).
 */
@Composable
private fun SettingRow(
    title: String,
    summary: String,
    trailing: String? = null,
    state: String? = null,
    opensScreen: Boolean = false,
    onClick: () -> Unit,
) {
    var focused by remember { mutableStateOf(false) }
    val opensDesc = stringResource(R.string.settings_opens_screen)
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .onFocusChanged { focused = it.isFocused }
            .clickable(onClick = onClick)
            .triremeFocus(focused)
            .semantics {
                role = Role.Button
                // State (current value) or the "opens a screen" hint, whichever applies.
                state?.let { stateDescription = it }
                if (opensScreen) stateDescription = opensDesc
            }
            .padding(horizontal = 14.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                title,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Text(
                summary,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        trailing?.let {
            Spacer(Modifier.width(8.dp))
            Text(
                it,
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.primary,
            )
        }
        if (opensScreen) {
            Spacer(Modifier.width(8.dp))
            Icon(
                Icons.AutoMirrored.Filled.KeyboardArrowRight,
                contentDescription = null, // semantics carry "opens a separate screen"
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

/** About block — a non-navigating, focusable row (reachable by D-pad, but inert). */
@Composable
private fun AboutRow(version: String) {
    var focused by remember { mutableStateOf(false) }
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .onFocusChanged { focused = it.isFocused }
            // Focusable for traversal completeness; OK is a no-op (nothing to open).
            .clickable { }
            .triremeFocus(focused)
            .semantics { role = Role.Button }
            .padding(horizontal = 14.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                stringResource(R.string.settings_about),
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Text(
                stringResource(R.string.settings_about_summary),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Text(
            stringResource(R.string.settings_about_version, version),
            style = MaterialTheme.typography.labelLarge,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

private fun PaceChoice.next(): PaceChoice =
    PaceChoice.entries[(ordinal + 1) % PaceChoice.entries.size]

/** Map the UI choice to the decoder's pace enum (StreamHub applies it live). */
private fun PaceChoice.toPaceMode(): PaceMode = when (this) {
    PaceChoice.LowLatency -> PaceMode.LOW_LATENCY
    PaceChoice.Balanced -> PaceMode.BALANCED
    PaceChoice.Smooth -> PaceMode.SMOOTH
    PaceChoice.Vrr -> PaceMode.VRR
}

private fun paceLabelRes(p: PaceChoice): Int = when (p) {
    PaceChoice.LowLatency -> R.string.settings_pace_low_latency
    PaceChoice.Balanced -> R.string.settings_pace_balanced
    PaceChoice.Smooth -> R.string.settings_pace_smooth
    PaceChoice.Vrr -> R.string.settings_pace_vrr
}

/** Read the app's versionName via PackageManager (BuildConfig isn't generated for this module). */
private fun versionName(context: Context): String = runCatching {
    @Suppress("DEPRECATION")
    context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "?"
}.getOrDefault("?")
