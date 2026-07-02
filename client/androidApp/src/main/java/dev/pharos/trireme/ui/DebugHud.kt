package dev.pharos.trireme.ui

import android.content.Context
import android.os.Build
import android.os.PowerManager
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.pharos.trireme.BuildConfig
import dev.pharos.trireme.ControllerManager
import dev.pharos.trireme.PadSensorCaps
import dev.pharos.trireme.stream.StreamHub
import kotlinx.coroutines.delay

/**
 * On-screen metrics overlay for the **debug variant only** (the shell gates this
 * on `BuildConfig.DEBUG`). A compact, top-RIGHT, monospace readout of live decode
 * health + device state — the instrument for the HDR/VRR/gyro e2e work.
 *
 * Anchored top-right on purpose: MangoHud (captured from the host game) sits
 * top-left in the frame, and the connect/idle status badge is top-center, so the
 * right corner is the only clear real estate.
 *
 * Cost/footprint: recomposes ~1/s (the decoder emits stats on its 1s cadence
 * window; thermal + pad sensors are polled on a 1s loop). Touch/D-pad inert — no
 * clickable/focusable modifiers and `canFocus = false`, so it never steals focus
 * (the Compose-TV trap) or eats a tap; it's purely a HUD.
 *
 * What it shows today (slots for W3/W4/W8 fields land as those workstreams do):
 *  - build id + active host + connection status
 *  - codec (HEVC/AV1) · hw/SW · pace
 *  - actual output WxH @ measured fps
 *  - decode queue depth / capacity, cumulative drops, decode-lag ms
 *  - thermal status + headroom (the W8b throttle signal)
 *  - per-pad gyro/accel sensor presence (the W9 confirmation)
 */
@Composable
fun DebugHud(modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val metrics by StreamHub.debugMetrics.collectAsState()
    val status by StreamHub.activeStatus.collectAsState()
    val activeId by StreamHub.activeHostId.collectAsState()
    val connections by StreamHub.connections.collectAsState()
    val caps by StreamHub.displayCaps.collectAsState()

    // Thermal + controller-sensor caps have no StateFlow; poll them on a 1s loop
    // (matches the decoder's stats cadence). getThermalHeadroom is rate-limited, so
    // 1s is the right interval.
    var thermal by remember { mutableStateOf("") }
    var pads by remember { mutableStateOf<List<PadSensorCaps>>(emptyList()) }
    LaunchedEffect(Unit) {
        while (true) {
            thermal = readThermal(context)
            pads = runCatching { ControllerManager.probeSensors(context) }.getOrDefault(emptyList())
            delay(1_000)
        }
    }

    val hostName = connections.firstOrNull { it.hostId == activeId }?.name ?: "—"

    Column(
        modifier
            // Never participate in D-pad focus (Compose-TV lesson) — display only.
            .focusProperties { canFocus = false }
            .padding(top = 8.dp, end = 8.dp)
            .clip(RoundedCornerShape(6.dp))
            .background(Color(0xCC0A0A0A))
            .padding(horizontal = 8.dp, vertical = 6.dp),
    ) {
        HudLine("TRIREME DEBUG ${BuildConfig.VERSION_NAME}", header = true)
        HudLine("host $hostName · $status")

        val m = metrics
        if (m == null) {
            HudLine("decoder idle", dim = true)
        } else {
            val codecShort = if (m.mime.endsWith("av01")) "AV1" else "HEVC"
            // "pace X" = the CLIENT pacing policy (PaceMode), not a panel mode — see panel line.
            HudLine("$codecShort ${if (m.hw) "hw" else "SW"} · pace ${m.pace}")
            HudLine("${m.width}x${m.height} @ ${"%.1f".format(m.fps)}fps")
            HudLine("q${m.queueDepth}/${m.capacity} drop${m.drops} lag${m.decodeLagMs}ms")
            HudLine(m.codecName, dim = true)
        }
        // Panel refresh truth: this clarifies that a "pace VRR" above does NOT mean the panel
        // is doing continuous VRR. "multi-sync(N)" = N discrete seamless rates (e.g. 60/90/120/
        // 144 on the 14T Pro, which has no continuous-VRR range); "fixed" = single rate.
        if (caps.refreshRates.isNotEmpty()) {
            HudLine(
                "panel ${"%.0f".format(caps.chosenRefresh)}Hz " +
                    if (caps.rangeable) "multi-sync(${caps.refreshRates.size})" else "fixed",
            )
        }

        if (thermal.isNotEmpty()) HudLine(thermal)

        if (pads.isEmpty()) {
            HudLine("pad none", dim = true)
        } else {
            pads.forEach { p ->
                HudLine(
                    "pad ${"%04x:%04x".format(p.vendorId, p.productId)} " +
                        "gyro${if (p.gyro) "✓" else "✗"} acc${if (p.accel) "✓" else "✗"}",
                )
            }
        }
    }
}

@Composable
private fun HudLine(text: String, header: Boolean = false, dim: Boolean = false) {
    Text(
        text = text,
        color = when {
            header -> Color(0xFF7FE0FF)
            dim -> Color(0xFFA8A8A8)
            else -> Color(0xFFEAEAEA)
        },
        fontFamily = FontFamily.Monospace,
        fontSize = if (header) 11.sp else 10.sp,
        lineHeight = 14.sp,
    )
}

/// Current thermal status + (API 30+) headroom — the W8b throttle signal. On the
/// 14T Pro the gating sensor is SKIN (LIGHT ≈ 53 °C); status reflects that ladder.
private fun readThermal(context: Context): String {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return ""
    val pm = context.getSystemService(Context.POWER_SERVICE) as? PowerManager ?: return ""
    val status = when (pm.currentThermalStatus) {
        PowerManager.THERMAL_STATUS_NONE -> "none"
        PowerManager.THERMAL_STATUS_LIGHT -> "LIGHT"
        PowerManager.THERMAL_STATUS_MODERATE -> "MODERATE"
        PowerManager.THERMAL_STATUS_SEVERE -> "SEVERE"
        PowerManager.THERMAL_STATUS_CRITICAL -> "CRITICAL"
        PowerManager.THERMAL_STATUS_EMERGENCY -> "EMERGENCY"
        PowerManager.THERMAL_STATUS_SHUTDOWN -> "SHUTDOWN"
        else -> "?"
    }
    val hr = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        runCatching { pm.getThermalHeadroom(0) }.getOrNull()?.takeIf { !it.isNaN() }
    } else null
    return "thermal $status" + (hr?.let { " hr${"%.2f".format(it)}" } ?: "")
}
