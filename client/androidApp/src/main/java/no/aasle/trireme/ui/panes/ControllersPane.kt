// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.ui.panes

import android.content.Context
import android.hardware.Sensor
import android.hardware.input.InputManager
import android.hardware.usb.UsbManager
import android.os.Build
import android.view.InputDevice
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Bluetooth
import androidx.compose.material.icons.filled.Layers
import androidx.compose.material.icons.filled.Usb
import androidx.compose.material.icons.filled.Wifi
import androidx.compose.material3.Card
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import no.aasle.trireme.ControllerDetailsDialog
import no.aasle.trireme.ControllerGroupView
import no.aasle.trireme.ControllerManager
import no.aasle.trireme.FocusRow
import no.aasle.trireme.GamepadPrefs
import no.aasle.trireme.GyroCalibrationDialog
import no.aasle.trireme.GyroTelemetry
import no.aasle.trireme.HostAliases
import no.aasle.trireme.ManageProfilesDialog
import no.aasle.trireme.MemberView
import no.aasle.trireme.ProfileRow
import no.aasle.trireme.ServerEntry
import no.aasle.trireme.ServerStore
import no.aasle.trireme.TileButton
import no.aasle.trireme.stream.StreamHub
import org.json.JSONArray
import uniffi.core_ffi.ControllerInfo
import uniffi.core_ffi.classifyController

/**
 * Controllers pane — moved out of the legacy `ControllersActivity` so it lives
 * inside the [NavShell] like every other destination, sharing the shell's
 * MaterialTheme, focus rules, and back-handling. The pane is otherwise the
 * same controller selector as before MINUS the global "Gyro → mouse aim"
 * toggle and the global pre-scale slider: with per-app `GyroMode` in `apps.toml`
 * the gyro policy is decided per app on the host, not globally on the client,
 * and the host's per-controller [`GyroTuning`] owns sensitivity now.
 *
 * State (the live device scan, paired-host lookup, identity path) is built
 * inside a `DisposableEffect` whose `onDispose` unregisters the
 * `InputDeviceListener` and stops the calibration telemetry session — same
 * lifetime story as the old activity's `onResume`/`onPause`.
 */
@Composable
fun ControllersPane() {
    val context = LocalContext.current
    val appContext = context.applicationContext
    val inputManager = remember { appContext.getSystemService(Context.INPUT_SERVICE) as InputManager }
    val usbManager = remember { appContext.getSystemService(Context.USB_SERVICE) as UsbManager }
    val prefs = remember { GamepadPrefs(appContext) }
    val store = remember { ServerStore(appContext) }
    val pairedHost = remember { store.load().servers.firstOrNull { it.paired } }
    val identityPath = remember { store.identityPath(appContext) }

    val pads = remember { mutableStateListOf<DetectedPad>() }

    fun rescan() {
        val next = mutableListOf<DetectedPad>()
        for (id in inputManager.inputDeviceIds) {
            val d = InputDevice.getDevice(id) ?: continue
            if (!ControllerManager.isGamepad(d)) continue
            val info = runCatching {
                classifyController(d.vendorId.toUShort(), d.productId.toUShort(), d.name ?: "")
            }.getOrNull()
            val gyroSensor = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                runCatching { d.sensorManager?.getDefaultSensor(Sensor.TYPE_GYROSCOPE) != null }
                    .getOrDefault(false)
            } else {
                false
            }
            // Android's InputDevice doesn't expose a SOURCE_BLUETOOTH bit
            // (only SOURCE_BLUETOOTH_STYLUS exists, which is unrelated). Best
            // signal we have client-side is the device descriptor + name:
            // a Bluetooth HID pad's descriptor typically encodes its MAC
            // (six `xx:xx` groups), while a wired/USB-HID pad hashes a USB
            // path. Falls back to USB on ambiguity; the host-side probe
            // (control-ipc `ControllerBus`) is the source of truth for the
            // panel — this is the on-device card row only.
            val bus = classifyBus(d.descriptor.orEmpty(), d.name.orEmpty())
            next.add(
                DetectedPad(
                    d.id, d.name ?: "Controller", d.vendorId, d.productId,
                    descriptor = d.descriptor.ifBlank { null },
                    info = info, gyroSensor = gyroSensor, connected = true,
                    bus = bus,
                ),
            )
        }
        // Controllers configured for USB/IP export to a Pharos host. Their input flows
        // host-side so they never appear as a live Android pad — surface them so their
        // profile can still be pre-tuned. Status reflects real USB presence (never a
        // false "Connected"). Skip any that are also a live local pad (same VID:PID).
        runCatching {
            val usb = appContext.getSharedPreferences("trireme_usb", Context.MODE_PRIVATE)
            val host = usb.getString("pharos", null)
            val presentUsb = runCatching {
                usbManager.deviceList.values.map { (it.vendorId shl 16) or it.productId }.toHashSet()
            }.getOrDefault(hashSetOf())
            val arr = JSONArray(usb.getString("entries", "[]"))
            for (i in 0 until arr.length()) {
                val o = arr.getJSONObject(i)
                if (!o.getBoolean("enabled")) continue
                val vid = o.getInt("vid")
                val pid = o.getInt("pid")
                if (next.any { it.vendor == vid && it.product == pid && it.remoteHost == null }) continue
                val name = o.getString("name")
                val info = runCatching {
                    classifyController(vid.toUShort(), pid.toUShort(), name)
                }.getOrNull()
                if (info == null || info.kind == uniffi.core_ffi.ControllerKind.GENERIC) continue
                next.add(
                    DetectedPad(
                        deviceId = -(vid * 0x10000 + pid + 1),
                        name = name,
                        vendor = vid,
                        product = pid,
                        descriptor = null,
                        info = info,
                        gyroSensor = false,
                        connected = presentUsb.contains((vid shl 16) or pid),
                        remoteHost = host,
                        usbExport = true,
                        // USB/IP exports always reach the host as USB.
                        bus = Bus.USB,
                    ),
                )
            }
        }
        pads.clear()
        pads.addAll(next)
    }

    // Initial scan + live device-listener registration. The listener is tied
    // to a DisposableEffect so it always unregisters when this pane leaves
    // composition (i.e. when the user navigates away in the shell), and the
    // calibration telemetry session is torn down here too — same belt-and-
    // braces guard the old onPause had.
    DisposableEffect(Unit) {
        rescan()
        val listener = object : InputManager.InputDeviceListener {
            override fun onInputDeviceAdded(deviceId: Int) = rescan()
            override fun onInputDeviceRemoved(deviceId: Int) = rescan()
            override fun onInputDeviceChanged(deviceId: Int) = rescan()
        }
        inputManager.registerInputDeviceListener(listener, null)
        onDispose {
            inputManager.unregisterInputDeviceListener(listener)
            GyroTelemetry.stopSession()
        }
    }

    ControllersScreen(pads, prefs, pairedHost, identityPath)
}

/**
 * Transport bus a controller arrived on, as classified from the Android
 * `InputDevice.sources` bits (or forced to [USB] for USB/IP exports, which
 * always reach the host as USB). Mirrors `control_ipc::ControllerBus` so the
 * card UI on host + client tell the same story.
 */
private enum class Bus { USB, BT, UNKNOWN }

/**
 * Best-effort bus classification from the Android `InputDevice` we already
 * have. Android doesn't expose a transport bit on `InputDevice` (the
 * `SOURCE_BLUETOOTH_STYLUS` flag is the only `BLUETOOTH` constant and is for
 * pen styluses, not gamepads), so we read it out of the device descriptor +
 * name: a BT HID pad's stable descriptor commonly encodes its MAC address
 * ("XX:XX:XX:XX:XX:XX"), and Android's own pad naming sometimes prefixes
 * "Bluetooth ". USB-attached / wired pads land on USB. Returns [Bus.USB] on
 * ambiguity — the card's host-side counterpart (`control_ipc::ControllerBus`)
 * is the authoritative pill for cross-device truth.
 */
private val MAC_PATTERN = Regex("(?i)([0-9a-f]{2}[:-]){5}[0-9a-f]{2}")
private fun classifyBus(descriptor: String, name: String): Bus {
    if (MAC_PATTERN.containsMatchIn(descriptor)) return Bus.BT
    val lower = name.lowercase()
    if (lower.startsWith("bluetooth") || lower.contains(" bt ") || lower.endsWith(" bt")) {
        return Bus.BT
    }
    return Bus.USB
}

private data class DetectedPad(
    val deviceId: Int,
    val name: String,
    val vendor: Int,
    val product: Int,
    /** Android's stable per-device descriptor hash; null for synthetic USB/IP rows. */
    val descriptor: String?,
    val info: ControllerInfo?,
    val gyroSensor: Boolean,
    val connected: Boolean,
    val remoteHost: String? = null,
    val usbExport: Boolean = false,
    /** Bus classification for the inline icon (USB / BT / hidden if Unknown). */
    val bus: Bus = Bus.UNKNOWN,
) {
    /** Stable per-controller key ("vvvv:pppp") — the prefs/relay identity. */
    val key: String get() = GamepadPrefs.controllerKey(vendor, product)
    val recognized: Boolean get() = info != null && info.kind != uniffi.core_ffi.ControllerKind.GENERIC
    val score: Int get() = (if (recognized) 100 else 0) +
        (if (gyroSensor || info?.gyro == true) 10 else 0) +
        (if (info?.touchpad == true || info?.paddles == true) 1 else 0)
}

private data class ControllerGroup(
    val primary: DetectedPad,
    val members: List<DetectedPad>,
) {
    val key: String get() = primary.key
    val remoteHost: String? get() = primary.remoteHost
    val composite: Boolean get() = members.size > 1
    val connected: Boolean get() = members.any { it.connected }
}

/**
 * Collapse the flat interface list into one entry per physical controller.
 * Same heuristic as the legacy activity: recognised controllers anchor their
 * own groups, unrecognised bridge interfaces fold in either when they share a
 * descriptor/name or there's exactly one anchor, and a user-forced split
 * (persisted [GamepadPrefs.isSplit]) keeps an interface alone.
 */
private fun groupControllers(pads: List<DetectedPad>, prefs: GamepadPrefs): List<ControllerGroup> {
    val groups = mutableListOf<MutableList<DetectedPad>>()
    pads.groupBy { it.remoteHost }.forEach { (_, bucket) ->
        val anchors = bucket.filter { it.recognized }.map { mutableListOf(it) }
        for (pad in bucket.filter { !it.recognized }) {
            val target = if (prefs.isSplit(pad.key)) {
                null
            } else {
                anchors.firstOrNull { sameDevice(pad, it.first()) }
                    ?: anchors.singleOrNull()
            }
            if (target != null) target.add(pad) else groups.add(mutableListOf(pad))
        }
        groups.addAll(anchors)
    }
    return groups
        .map { m -> m.sortedByDescending { it.score } }
        .map { ControllerGroup(primary = it.first(), members = it) }
        .sortedWith(
            compareByDescending<ControllerGroup> { it.connected }
                .thenBy { it.remoteHost != null }
                .thenBy {
                    HostAliases.get(it.key)
                        ?: prefs.customName(it.key)
                        ?: it.primary.info?.label
                        ?: it.primary.name
                },
        )
}

private fun sameDevice(a: DetectedPad, b: DetectedPad): Boolean {
    if (a.descriptor != null && a.descriptor == b.descriptor) return true
    val an = a.name.trim().lowercase()
    return an.isNotEmpty() && an == b.name.trim().lowercase()
}

private fun memberRole(m: DetectedPad, primary: DetectedPad): String {
    val tags = mutableListOf(
        when {
            m.deviceId == primary.deviceId -> "primary"
            m.recognized -> "extra interface"
            else -> "bridge / control interface"
        },
    )
    if (m.gyroSensor || m.info?.gyro == true) tags.add("gyro/IMU")
    if (m.info?.touchpad == true) tags.add("touchpad")
    return tags.joinToString(" · ")
}

@Composable
private fun ControllersScreen(
    pads: List<DetectedPad>,
    prefs: GamepadPrefs,
    host: ServerEntry?,
    identityPath: String,
) {
    // Bumped when a dialog mutates grouping state (split toggle, rename) so the
    // list below re-runs groupControllers without a rescan.
    var regroup by remember { mutableStateOf(0) }
    // Controller-handling master switch (default off → raw forwarding). StreamHub
    // owns the source of truth and rebuilds the live ControllerManager on change.
    var handling by remember { mutableStateOf(StreamHub.isControllerHandling()) }
    LazyColumn(
        Modifier.fillMaxSize().padding(20.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        item {
            Text(
                "Controllers",
                style = MaterialTheme.typography.headlineSmall,
                fontWeight = FontWeight.Bold,
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
        item {
            ToggleRow("Controller handling", handling, bold = true) {
                handling = it
                StreamHub.setControllerHandling(it)
            }
        }
        if (!handling) {
            item {
                Text(
                    "Controllers are forwarded raw — turn this on to apply profiles, " +
                        "button swaps and gyro. Pads still work either way.",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        val groups = regroup.let { groupControllers(pads, prefs) }
        item {
            Text(
                "Detected controllers (${groups.size})",
                fontWeight = FontWeight.SemiBold,
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
        if (groups.isEmpty()) {
            item {
                Text(
                    "None connected. Pair a controller to the TV (Bluetooth or USB).",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        } else {
            items(groups, key = { it.key }) {
                GroupCard(it, prefs, host, identityPath, dim = !handling, onRegroup = { regroup++ })
            }
        }
        item {
            Text(
                "Tap a pad's Gyro tile to calibrate with a live visualiser. A controller " +
                    "that shows up as several devices (a composite dongle, ⧉) is folded into " +
                    "one card — open Details to see its interfaces or rename it. Gyro behaviour " +
                    "is set per app on the host (see Apps in the panel).",
                fontSize = 11.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun GroupCard(
    g: ControllerGroup,
    prefs: GamepadPrefs,
    host: ServerEntry?,
    identityPath: String,
    // Dimmed when controller handling is off — edits are informational then
    // (a re-attach in raw mode resets the host to the default profile).
    dim: Boolean = false,
    onRegroup: () -> Unit,
) {
    val p = g.primary
    val key = g.key
    var rev by remember(key) { mutableStateOf(0) }
    var ab by remember(key, rev) { mutableStateOf(prefs.swapAb(key)) }
    var xy by remember(key, rev) { mutableStateOf(prefs.swapXy(key)) }
    var rumble by remember(key) { mutableStateOf(prefs.rumble(key)) }
    var paddles by remember(key) { mutableStateOf(prefs.paddles(key)) }
    var showGyro by remember(key) { mutableStateOf(false) }
    var showDetails by remember(key) { mutableStateOf(false) }
    var showManage by remember(key) { mutableStateOf(false) }

    val rawTitle = HostAliases.get(key)
        ?: prefs.customName(key)
        ?: p.name.ifBlank { null }
        ?: p.info?.label
        ?: "Generic gamepad"
    // Truncate at render time only — storage keeps the full alias so an
    // editor click still sees the raw string.
    val title = rawTitle.truncateCard()
    Card(Modifier.fillMaxWidth().alpha(if (dim) 0.5f else 1f)) {
        Column(Modifier.padding(12.dp)) {
            // Header row collapses to one line (inline order: liveness dot ·
            // name (truncated) · bus icon · composite badge). The dedicated
            // "Connected" / "Saved" / "USB/IP → host" subtitle row that used
            // to live below is folded in here — the icon's tint conveys the
            // same on/off signal, USB/IP exports keep their Wi-Fi glyph.
            Row(verticalAlignment = Alignment.CenterVertically) {
                Dot(if (g.connected) Color(70, 180, 90) else Color(120, 120, 120))
                Spacer(Modifier.width(6.dp))
                Text(
                    title,
                    fontWeight = FontWeight.SemiBold,
                    fontSize = 16.sp,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                Spacer(Modifier.width(8.dp))
                BusIcon(g, p)
                if (g.composite) {
                    Spacer(Modifier.width(8.dp))
                    CompositeBadge(g.members.size)
                }
            }

            Spacer(Modifier.height(8.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                val gyroSensor = g.members.any { it.gyroSensor }
                // The phone's own gyro can stand in for any pad's.
                val gyroCapable = gyroSensor || StreamHub.isPhoneGyro() ||
                    g.members.any { it.info?.gyro == true }
                if (gyroCapable) {
                    TileButton(
                        text = if (gyroSensor) "Gyro ✓" else "Gyro",
                        color = if (gyroSensor) Color(60, 150, 80) else Color(170, 120, 40),
                        active = true,
                    ) { showGyro = true }
                }
                if (g.members.any { it.info?.touchpad == true }) CapBadge("Touchpad", Color(70, 110, 160))
                if (g.members.any { it.info?.paddles == true }) {
                    TileButton("Paddles", Color(120, 90, 170), active = paddles) {
                        paddles = !paddles; prefs.setPaddles(key, paddles)
                    }
                }
                if (g.members.any { it.info?.rumble == true }) {
                    TileButton("Rumble", Color(150, 110, 90), active = rumble) {
                        rumble = !rumble; prefs.setRumble(key, rumble)
                    }
                }
                TileButton("Details", Color(110, 120, 135), active = false) { showDetails = true }
            }

            Spacer(Modifier.height(8.dp))
            ProfileRow(
                prefs = prefs,
                controllerKey = key,
                onApplied = { rev++ },
                onManage = { showManage = true },
            )

            Spacer(Modifier.height(4.dp))
            ToggleRow("Swap A / B", ab) {
                ab = it
                prefs.setSwapAb(key, it)
                prefs.setAssignedProfile(key, GamepadPrefs.PROFILE_CUSTOM)
                relayProfile(p, prefs.profile(key))
            }
            ToggleRow("Swap X / Y", xy) {
                xy = it
                prefs.setSwapXy(key, it)
                prefs.setAssignedProfile(key, GamepadPrefs.PROFILE_CUSTOM)
                relayProfile(p, prefs.profile(key))
            }
        }
    }

    if (showGyro) {
        GyroCalibrationDialog(
            label = title,
            vendor = p.vendor,
            product = p.product,
            prefs = prefs,
            host = host,
            identityPath = identityPath,
            onDismiss = { showGyro = false; rev++ },
        )
    }
    if (showDetails) {
        ControllerDetailsDialog(
            group = ControllerGroupView(
                key = key,
                title = title,
                members = g.members.map { m ->
                    MemberView(
                        id = m.deviceId,
                        name = m.name,
                        address = m.key,
                        role = memberRole(m, p),
                        recognized = m.recognized,
                    )
                },
            ),
            prefs = prefs,
            onChanged = { rev++; onRegroup() },
            onDismiss = { showDetails = false },
        )
    }
    if (showManage) {
        ManageProfilesDialog(
            prefs = prefs,
            controllerKey = key,
            onChanged = { rev++ },
            onDismiss = { showManage = false },
        )
    }
}

/**
 * Inline bus icon next to the controller name. USB/IP exports keep their
 * Wi-Fi glyph (it reads as "remote", which matches the card body), and direct
 * pads pick Filled.Usb or Filled.Bluetooth based on the rescan's classification.
 * Hidden entirely on `Bus.UNKNOWN` so a row with no signal stays clean. Size
 * is 16dp; tint is the green liveness color when connected, gray otherwise —
 * same palette the panel's Rust [`bus_pill`] uses, so the same pad looks the
 * same on both sides.
 */
@Composable
private fun BusIcon(g: ControllerGroup, p: DetectedPad) {
    val active = g.connected
    val tint = if (active) Color(70, 180, 90) else Color(120, 120, 120)
    if (p.usbExport) {
        val remote = g.remoteHost
        val cd = when {
            active && remote != null -> "USB/IP attached, $remote"
            remote != null -> "USB/IP, $remote, not attached"
            else -> "USB/IP export"
        }
        Icon(
            Icons.Filled.Wifi,
            contentDescription = cd,
            tint = tint,
            modifier = Modifier.size(16.dp),
        )
        return
    }
    when (p.bus) {
        Bus.USB -> Icon(
            Icons.Filled.Usb,
            contentDescription = if (active) "USB, connected" else "USB, saved",
            tint = tint,
            modifier = Modifier.size(16.dp),
        )
        Bus.BT -> Icon(
            Icons.Filled.Bluetooth,
            contentDescription = if (active) "Bluetooth, connected" else "Bluetooth, saved",
            tint = tint,
            modifier = Modifier.size(16.dp),
        )
        Bus.UNKNOWN -> {} // no signal — render no icon, keep the row clean
    }
}

/**
 * Render-layer truncator for card titles. Long alias strings ("Steam Deck —
 * primary account · living room") would otherwise wrap into a second line and
 * shove the bus icon off the row. Storage keeps the full string for editing.
 * Mirrors the panel's Rust [`crate::widgets::truncate_for_card`] so both
 * surfaces clip at the same point.
 */
private fun String.truncateCard(): String {
    val max = 32
    return if (length <= max) this else take(max - 1) + "…"
}

/**
 * Push the controller's updated profile to the host via the active session's
 * [uniffi.core_ffi.GamepadInput]. The host is the single source of truth for
 * Swap A/B and Swap X/Y (see [no.aasle.trireme.ControllerManager]) — the
 * client transmits raw indices, the host swaps them. Used by the per-controller
 * toggle rows here so a swap change takes effect on the live pad immediately
 * (no app restart, no reconnect). No-op when no session is active; the host
 * picks the new swap up from the persisted profile on next [GamepadConnect].
 */
private fun relayProfile(p: DetectedPad, profile: uniffi.core_ffi.ControllerProfile) {
    val gp = StreamHub.activeGamepad() ?: return
    // USB/IP-exported controllers are always on slot 0 at the host, regardless
    // of any Android-side slot assignment. Local controllers use their actual slot.
    val slot = if (p.usbExport) 0 else (StreamHub.slotForController(p.vendor, p.product) ?: 0)
    runCatching {
        gp.setTuning(slot.toUByte(), p.vendor.toUShort(), p.product.toUShort(), profile)
    }
}

@Composable
private fun CompositeBadge(count: Int) {
    Row(
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(Color(90, 110, 150).copy(alpha = 0.28f))
            .padding(horizontal = 7.dp, vertical = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            Icons.Filled.Layers,
            contentDescription = "Composite device",
            tint = Color(150, 175, 220),
            modifier = Modifier.size(13.dp),
        )
        Spacer(Modifier.width(4.dp))
        Text("$count-in-1", fontSize = 10.sp, color = Color(150, 175, 220))
    }
}

@Composable
private fun Dot(color: Color) {
    Box(Modifier.size(9.dp).clip(CircleShape).background(color))
}

/**
 * Pane-local wrapper around the shared [no.aasle.trireme.FocusRow] that adds
 * the right onSurface color for the pane's theme. The shared helper draws labels
 * unstyled so multiple themes can wrap it (the gyro overlay uses the same row
 * inside its own dark Surface, where we don't want to force onSurface).
 */
@Composable
private fun ToggleRow(label: String, checked: Boolean, bold: Boolean = false, onChange: (Boolean) -> Unit) {
    FocusRow(onClick = { onChange(!checked) }) {
        Text(
            label,
            fontSize = if (bold) 15.sp else 13.sp,
            fontWeight = if (bold) FontWeight.SemiBold else FontWeight.Normal,
            color = MaterialTheme.colorScheme.onSurface,
            modifier = Modifier.weight(1f),
        )
        androidx.compose.material3.Switch(checked = checked, onCheckedChange = null)
    }
}

@Composable
private fun CapBadge(text: String, color: Color) {
    Box(
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(color.copy(alpha = 0.25f))
            .padding(horizontal = 8.dp, vertical = 2.dp),
    ) {
        Text(text, color = color, fontSize = 11.sp)
    }
}
