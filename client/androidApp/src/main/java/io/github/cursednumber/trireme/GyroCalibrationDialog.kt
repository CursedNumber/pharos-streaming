// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme

import android.content.Context
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.hardware.display.DisplayManager
import android.view.Display
import android.view.Surface as ViewSurface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlin.math.abs
import io.github.cursednumber.trireme.stream.StreamHub
import uniffi.core_ffi.ControllerProfile
import uniffi.core_ffi.FrameSink
import uniffi.core_ffi.GamepadInput
import uniffi.core_ffi.GyroSink
import uniffi.core_ffi.GyroTuning
import uniffi.core_ffi.defaultControllerProfile
import uniffi.core_ffi.startVideoStream

private val AXIS_NAMES = listOf("RX (roll)", "RY (pitch)", "RZ (yaw)")
private val AXIS_SHORT = listOf("RX", "RY", "RZ")

/**
 * Live gyro calibration overlay for one controller. Opens a short-lived telemetry
 * session to the host (the IMU is read there, not on this TV), draws the raw
 * per-axis rotation as a live visualiser, and edits the controller's gyro feel —
 * persisted locally and relayed to the host. Laid out in flex-flow ([FlowRow]) so
 * every control lands on one screen in any aspect ratio.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun GyroCalibrationDialog(
    label: String,
    vendor: Int,
    product: Int,
    prefs: GamepadPrefs,
    host: ServerEntry?,
    identityPath: String,
    onDismiss: () -> Unit,
) {
    val key = GamepadPrefs.controllerKey(vendor, product)
    val initial = remember(key) { prefs.profile(key) }
    var enabled by remember(key) { mutableStateOf(initial.gyro.enabled) }
    var sx by remember(key) { mutableStateOf(initial.gyro.sensX) }
    var sy by remember(key) { mutableStateOf(initial.gyro.sensY) }
    var accel by remember(key) { mutableStateOf(initial.gyro.accel) }
    var pow by remember(key) { mutableStateOf(initial.gyro.accelPow) }
    var invX by remember(key) { mutableStateOf(initial.gyro.invertX) }
    var invY by remember(key) { mutableStateOf(initial.gyro.invertY) }
    var axH by remember(key) { mutableStateOf(initial.gyro.axisH.toInt()) }
    var axV by remember(key) { mutableStateOf(initial.gyro.axisV.toInt()) }
    var showManage by remember(key) { mutableStateOf(false) }

    // Open a telemetry-only session to the host while the dialog is up; tear it
    // down on dismiss. The host streams the IMU back over the same path as video.
    val handle = remember { mutableStateOf<GamepadInput?>(null) }
    DisposableEffect(host?.id) {
        var h: GamepadInput? = null
        if (host != null) {
            val discard = object : FrameSink {
                override fun onFrame(data: ByteArray) {}
                override fun onEnd(error: String?) {}
                // Calibration-only throwaway sink: link-health is irrelevant here.
                override fun onLinkHealth(degraded: Boolean) {}
            }
            val sink = object : GyroSink {
                override fun onSample(gx: Float, gy: Float, gz: Float) =
                    GyroTelemetry.publish(gx, gy, gz)
            }
            h = runCatching {
                // Short-lived gyro-calibration session: no app catalog needed.
                // `wantsGyro=true` regardless of the global Settings toggle —
                // this dialog's whole purpose is to see live gyro, so it must
                // ask the host to relay raw samples even when the user keeps
                // the streaming-time flag off.
                startVideoStream(
                    "${host.host}:${host.port}",
                    host.streamAltAddrs(),
                    host.hostPubkey,
                    identityPath,
                    discard,
                    null,
                    sink,
                    null,
                    null, // clipboardSink: none for the short-lived gyro session
                    true,
                    // Gyro-calibration session: video is discarded, so Auto res + no
                    // refresh cap (0) is fine.
                    uniffi.core_ffi.StreamMode(width = 0u, height = 0u, refreshMhz = 0u),
                )
            }.getOrNull()
            handle.value = h
            GyroTelemetry.startSession(h) // owned centrally so onPause can tear it down too
        }
        onDispose { GyroTelemetry.stopSession() }
    }

    // With this phone as the gyro source, show its own sensor: the host never
    // sees it until a stream is up, and the choices here shape it all the same
    // (they reach the host's emulator motion even with controller handling off).
    val ctx = LocalContext.current.applicationContext
    val phoneGyro = remember { StreamHub.isPhoneGyro() }
    DisposableEffect(phoneGyro) {
        val sm = if (phoneGyro) ctx.getSystemService(Context.SENSOR_SERVICE) as? SensorManager else null
        val sensor = sm?.getDefaultSensor(Sensor.TYPE_GYROSCOPE)
        val display = (ctx.getSystemService(Context.DISPLAY_SERVICE) as? DisplayManager)
            ?.getDisplay(Display.DEFAULT_DISPLAY)
        val listener = object : SensorEventListener {
            private val c = FloatArray(3)
            override fun onSensorChanged(e: SensorEvent) {
                ControllerManager.canonical(e.values, display?.rotation ?: ViewSurface.ROTATION_90, c)
                // Canonical (x = yaw, y = -pitch, z = roll) → [roll, pitch, yaw] deg/s.
                val deg = (180.0 / Math.PI).toFloat()
                GyroTelemetry.publishLocal(c[2] * deg, -c[1] * deg, c[0] * deg)
            }
            override fun onAccuracyChanged(s: Sensor?, accuracy: Int) {}
        }
        if (sensor != null) sm.registerListener(listener, sensor, SensorManager.SENSOR_DELAY_GAME)
        onDispose { sm?.unregisterListener(listener) }
    }

    fun current(): ControllerProfile = ControllerProfile(
        swapAb = prefs.swapAb(key),
        swapXy = prefs.swapXy(key),
        gyro = GyroTuning(
            enabled = enabled, sensX = sx, sensY = sy, accel = accel, accelPow = pow,
            accelRef = initial.gyro.accelRef, accelMax = initial.gyro.accelMax,
            invertX = invX, invertY = invY,
            axisH = axH.toUByte(), axisV = axV.toUByte(),
        ),
    )
    fun apply() {
        val p = current()
        prefs.setProfile(key, p)
        handle.value?.setTuning(0.toUByte(), vendor.toUShort(), product.toUShort(), p)
    }
    // A hand-edit: persist + relay, and mark the controller's profile as "Custom"
    // so its dropdown stops claiming a preset the values have since drifted from.
    fun edited() {
        apply()
        prefs.setAssignedProfile(key, GamepadPrefs.PROFILE_CUSTOM)
    }
    fun load(p: ControllerProfile) {
        enabled = p.gyro.enabled; sx = p.gyro.sensX; sy = p.gyro.sensY
        accel = p.gyro.accel; pow = p.gyro.accelPow
        invX = p.gyro.invertX; invY = p.gyro.invertY
        axH = p.gyro.axisH.toInt(); axV = p.gyro.axisV.toInt()
        apply() // pushes the loaded values; the assigned-profile name is set by the caller
    }

    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        Surface(
            shape = RoundedCornerShape(14.dp),
            color = MaterialTheme.colorScheme.surface,
            modifier = Modifier.fillMaxWidth(0.94f).padding(8.dp),
        ) {
            LazyColumn(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Gyro calibration — $label", fontWeight = FontWeight.SemiBold, fontSize = 16.sp, modifier = Modifier.weight(1f))
                        OutlinedButton(onClick = onDismiss) { Text("Close", fontSize = 12.sp) }
                    }
                }
                item { GyroVisualizer(axH, axV, invX, invY, host != null) }
                item {
                    // Flex-flow: each control is a fixed-width cell that wraps to
                    // fit the screen — one screenful in any aspect ratio.
                    FlowRow(
                        Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                        verticalArrangement = Arrangement.spacedBy(2.dp),
                    ) {
                        Cell { ToggleRow("Enabled", enabled) { enabled = it; edited() } }
                        Cell { GyroSlider("Sensitivity ←→", sx, 1f..40f) { sx = it; edited() } }
                        Cell { GyroSlider("Sensitivity ↑↓", sy, 1f..40f) { sy = it; edited() } }
                        Cell { GyroSlider("Acceleration", accel, 0f..6f) { accel = it; edited() } }
                        Cell { GyroSlider("Accel curve", pow, 1f..4f) { pow = it; edited() } }
                        Cell { ToggleRow("Invert ←→", invX) { invX = it; edited() } }
                        Cell { ToggleRow("Invert ↑↓", invY) { invY = it; edited() } }
                        Cell { AxisRow("Axis ←→", axH) { axH = it; edited() } }
                        Cell { AxisRow("Axis ↑↓", axV) { axV = it; edited() } }
                    }
                }
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Reset to defaults", fontSize = 12.sp, modifier = Modifier.weight(1f))
                        OutlinedButton(onClick = {
                            load(defaultControllerProfile())
                            prefs.setAssignedProfile(key, GamepadPrefs.PROFILE_DEFAULT)
                        }) {
                            Text("Reset", fontSize = 12.sp)
                        }
                    }
                }
                item {
                    // Apply a saved profile (or Default) to this controller; Manage
                    // saves/renames/deletes presets. onApplied re-reads the now-applied
                    // values into the sliders and relays them to the host.
                    ProfileRow(
                        prefs = prefs,
                        controllerKey = key,
                        onApplied = { load(prefs.profile(key)) },
                        onManage = { showManage = true },
                    )
                }
            }
        }
    }

    if (showManage) {
        ManageProfilesDialog(
            prefs = prefs,
            controllerKey = key,
            onChanged = {},
            onDismiss = { showManage = false },
        )
    }
}

/** One flex-flow cell sized so a handful tile across a TV and wrap on narrow screens. */
@Composable
private fun Cell(content: @Composable () -> Unit) {
    Box(Modifier.width(260.dp)) { content() }
}

/** Cycle which raw gyro axis (RX/RY/RZ) drives an output. */
@Composable
private fun AxisRow(label: String, axis: Int, onChange: (Int) -> Unit) {
    FocusRow(onClick = { onChange((axis + 1) % 3) }) {
        Text(label, fontSize = 13.sp, modifier = Modifier.weight(1f))
        Text(AXIS_NAMES.getOrElse(axis) { "?" }, fontSize = 13.sp, fontWeight = FontWeight.SemiBold)
    }
}

/**
 * Live visualiser: a horizontal bar per raw gyro axis (signed rate from centre),
 * with the axes currently mapped to ←→ and ↑↓ highlighted, plus a crosshair dot
 * showing the resulting aim vector. Reads [GyroTelemetry] each frame.
 */
@Composable
private fun GyroVisualizer(hAxis: Int, vAxis: Int, invX: Boolean, invY: Boolean, haveHost: Boolean) {
    var rx by remember { mutableStateOf(0f) }
    var ry by remember { mutableStateOf(0f) }
    var rz by remember { mutableStateOf(0f) }
    var live by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        while (true) {
            withFrameNanos { now ->
                rx = GyroTelemetry.rx; ry = GyroTelemetry.ry; rz = GyroTelemetry.rz
                live = GyroTelemetry.isLive(now)
            }
        }
    }
    val axes = floatArrayOf(rx, ry, rz)
    val maxRate = 300f // deg/s → full-scale bar

    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(10.dp))
            .background(Color(0x18, 0x1A, 0x20))
            .padding(12.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.size(9.dp).clip(CircleShape).background(if (live) Color(70, 190, 90) else Color(120, 120, 120)))
            Spacer(Modifier.width(6.dp))
            Text(
                if (live) "Live — move the controller" else if (haveHost) "Waiting for the host gyro…" else "No paired host — connect to calibrate live",
                fontSize = 12.sp,
                color = if (live) Color(120, 200, 130) else Color(160, 160, 160),
            )
        }
        Spacer(Modifier.height(8.dp))
        // Three axis bars.
        for (i in 0..2) {
            val mapped = when (i) {
                hAxis -> " → ←→"
                vAxis -> " → ↑↓"
                else -> ""
            }
            val highlight = i == hAxis || i == vAxis
            Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(vertical = 3.dp)) {
                Text(
                    AXIS_SHORT[i] + mapped,
                    fontSize = 11.sp,
                    color = if (highlight) Color(150, 200, 255) else Color(150, 150, 150),
                    modifier = Modifier.width(78.dp),
                )
                BiBar(value = axes[i] / maxRate, highlight = highlight)
            }
        }
        Spacer(Modifier.height(8.dp))
        // Crosshair: the resulting aim vector from the mapped axes (+ inversion).
        val sgnX = if (invX) 1f else -1f
        val sgnY = if (invY) -1f else 1f
        val hv = (axes.getOrElse(hAxis) { 0f } / maxRate) * sgnX
        val vv = (axes.getOrElse(vAxis) { 0f } / maxRate) * sgnY
        Crosshair(hv, vv)
    }
}

/** A centre-anchored signed bar in [-1, 1]. */
@Composable
private fun BiBar(value: Float, highlight: Boolean) {
    val v = value.coerceIn(-1f, 1f)
    val col = if (highlight) Color(90, 160, 240) else Color(110, 130, 150)
    Canvas(Modifier.fillMaxWidth().height(14.dp)) {
        val midY = size.height / 2f
        val cx = size.width / 2f
        // track + centre tick
        drawLine(Color(0x33, 0x38, 0x42), Offset(0f, midY), Offset(size.width, midY), strokeWidth = 2f)
        drawLine(Color(0x55, 0x5C, 0x68), Offset(cx, 0f), Offset(cx, size.height), strokeWidth = 2f)
        val w = (size.width / 2f) * abs(v)
        if (v >= 0f) {
            drawRect(col, topLeft = Offset(cx, midY - 4f), size = androidx.compose.ui.geometry.Size(w, 8f))
        } else {
            drawRect(col, topLeft = Offset(cx - w, midY - 4f), size = androidx.compose.ui.geometry.Size(w, 8f))
        }
    }
}

/** A 2-D dot showing the combined aim direction; (x, y) in roughly [-1, 1]. */
@Composable
private fun Crosshair(x: Float, y: Float) {
    Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
        Canvas(Modifier.size(96.dp)) {
            val cx = size.width / 2f
            val cy = size.height / 2f
            drawLine(Color(0x33, 0x38, 0x42), Offset(0f, cy), Offset(size.width, cy), strokeWidth = 2f)
            drawLine(Color(0x33, 0x38, 0x42), Offset(cx, 0f), Offset(cx, size.height), strokeWidth = 2f)
            val px = cx + (x.coerceIn(-1f, 1f)) * cx
            val py = cy + (y.coerceIn(-1f, 1f)) * cy
            drawCircle(Color(90, 200, 120), radius = 7f, center = Offset(px, py))
        }
    }
}
