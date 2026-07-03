// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme

import android.content.Intent
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp
import no.aasle.trireme.ui.TriremeTheme
import no.aasle.trireme.ui.triremeFocus
import kotlinx.coroutines.delay

/**
 * Trireme USB export: pick which local USB devices to share with Pharos. The
 * actual forwarding runs in [UsbExportService] (a foreground service) so it
 * survives leaving this screen — and keeps going while a game is streaming. This
 * activity is a thin UI over [UsbExportEngine]: it lists devices and toggles them;
 * enabling one starts the service, disabling the last one stops it.
 */
class UsbExportActivity : ComponentActivity() {
    private lateinit var engine: UsbExportEngine
    private lateinit var pharos: String
    // True when the system opened us on a fresh USB attach. That delivery already
    // granted permission for the device, so the first onResume must NOT also fire
    // a redundant "Allow access?" prompt.
    private var launchedViaAttach = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        launchedViaAttach = intent?.action == UsbManager.ACTION_USB_DEVICE_ATTACHED
        engine = UsbExportEngine.get(this)
        val store = UsbStore(this)
        pharos = store.pharos()

        // On a fresh plug-in (attach), skip this screen entirely when there is
        // nothing for the user to decide — bounce straight to the stream shell.
        // The live session resumes (StreamHub is a process singleton), or the
        // Hosts menu shows if nothing is connected. We only show the export screen
        // for a *new* device that the user might want to Share or Ignore.
        if (launchedViaAttach && skipScreenOnAttach(store)) {
            // Already enrolled (the attach itself just granted the session): make
            // sure forwarding is running before we leave. No-op when master off.
            UsbExportService.ensureRunning(this)
            bounceToStream()
            finish()
            return
        }

        setContent {
            TriremeTheme {
                ExportScreen(
                    engine = engine,
                    pharos = pharos,
                    initialUsbIp = store.usbIpEnabled(),
                    onSetUsbIp = { on ->
                        store.setUsbIpEnabled(on)
                        if (on) {
                            UsbExportService.ensureRunning(this)
                            engine.refreshRows()
                            engine.requestMissingPermissions()
                        } else {
                            UsbExportService.stop(this)
                        }
                    },
                    onToggle = { row ->
                        if (engine.toggle(row)) {
                            UsbExportService.ensureRunning(this)
                            // Enabling a device: ask for its USB access now, from this
                            // visible screen, so the dialog offers "use by default".
                            engine.requestMissingPermissions()
                        } else {
                            UsbExportService.stop(this)
                        }
                    },
                    onIgnore = { row ->
                        // Toggle the "never export" mark; ignoring also de-exports it
                        // (reclaims e.g. the TV remote whose keys were being tunnelled).
                        engine.setIgnored(row, !row.blocked)
                        if (!engine.anyEnabled()) UsbExportService.stop(this)
                    },
                )
            }
        }
    }

    /**
     * Decide whether a fresh attach has nothing to ask the user, so we can skip
     * the export screen and go straight to the stream:
     *  - master USB/IP off → not forwarding anything, just open the app;
     *  - the attached device is ignored → silently reclaimed, never offered;
     *  - the attached device is already enrolled → resume forwarding + stream.
     * A *new* (unknown / not-yet-enrolled, not-ignored) device returns false so the
     * screen is shown. Missing device extra → false (show the screen, fail safe).
     */
    private fun skipScreenOnAttach(store: UsbStore): Boolean {
        if (!store.usbIpEnabled()) return true // master off — nothing to offer here
        val dev = attachedDevice() ?: return false
        val entry = store.entries()[key(dev.vendorId, dev.productId)] ?: return false
        return entry.blocked || entry.enabled
    }

    private fun attachedDevice(): UsbDevice? {
        val i = intent ?: return null
        @Suppress("DEPRECATION")
        return if (Build.VERSION.SDK_INT >= 33) {
            i.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
        } else {
            i.getParcelableExtra(UsbManager.EXTRA_DEVICE)
        }
    }

    /** Bring the stream shell to the front (resumes the live session via StreamHub,
     *  or shows Hosts if idle). REORDER_TO_FRONT reuses the running MainActivity
     *  without recreating it; if it isn't running yet, a normal start happens. */
    private fun bounceToStream() {
        startActivity(
            Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_REORDER_TO_FRONT),
        )
    }

    override fun onResume() {
        super.onResume()
        engine.refreshRows()
        UsbExportService.ensureRunning(this) // resume forwarding any enrolled devices
        // Prompt for any already-enabled device that's plugged in but not yet
        // granted — unless we were just opened by an attach, which already granted.
        if (!launchedViaAttach) engine.requestMissingPermissions()
        launchedViaAttach = false
    }
}

@Composable
private fun ExportScreen(
    engine: UsbExportEngine,
    pharos: String,
    initialUsbIp: Boolean,
    onSetUsbIp: (Boolean) -> Unit,
    onToggle: (DeviceRow) -> Unit,
    onIgnore: (DeviceRow) -> Unit,
) {
    var usbIp by remember { mutableStateOf(initialUsbIp) }
    // Keep the list fresh on plug/unplug while this screen is open, even when the
    // service loop isn't running (nothing enabled yet).
    LaunchedEffect(Unit) {
        while (true) {
            engine.refreshRows()
            delay(2000)
        }
    }
    val rows by engine.rows
    Surface(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize().safeDrawingPadding().padding(16.dp)) {
            Text(
                if (pharos.isBlank()) "Trireme — share USB with the host"
                else "Trireme — share USB with $pharos",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
            )
            Spacer(Modifier.padding(4.dp))
            // Master switch (default off): with it off nothing is forwarded and no
            // plug-in prompts fire. The whole row is the single D-pad target.
            MasterSwitchRow(
                label = "USB/IP forwarding",
                enabled = usbIp,
                onClick = {
                    usbIp = !usbIp
                    onSetUsbIp(usbIp)
                },
            )
            Spacer(Modifier.padding(4.dp))
            if (!usbIp) {
                Text(
                    "USB/IP forwarding is off. Turn it on to share attached USB devices " +
                        "(controllers, Bluetooth dongles) with the host.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else if (pharos.isBlank()) {
                // No host to export to yet: the target is derived from the paired
                // host, and none is known. Sharing here would silently no-connect,
                // so point the user at pairing instead of a dead toggle.
                Text(
                    "No paired host yet. Pair a host on the Hosts screen first — the " +
                        "USB/IP target is derived from it.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                Text(
                    "Share a device with the host, or Ignore it so Trireme never takes it — e.g. " +
                        "your TV remote, whose keys would otherwise be tunnelled to the host and " +
                        "stop driving this UI.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.padding(4.dp))
                LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    items(rows) { row ->
                        DeviceItem(row, onToggle = { onToggle(row) }, onIgnore = { onIgnore(row) })
                    }
                }
            }
        }
    }
}

/**
 * A focusable master on/off row — the whole row is the single D-pad target (no
 * nested focusables; see lesson-compose-tv-focus). The trailing On/Off label is
 * display-only; OK toggles it.
 */
@Composable
private fun MasterSwitchRow(label: String, enabled: Boolean, onClick: () -> Unit) {
    var focused by remember { mutableStateOf(false) }
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 56.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(MaterialTheme.colorScheme.surfaceContainerHighest)
            .onFocusChanged { focused = it.isFocused }
            .clickable(onClick = onClick)
            .triremeFocus(focused)
            .semantics {
                role = Role.Switch
                stateDescription = if (enabled) "On" else "Off"
            }
            .padding(horizontal = 14.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onSurface,
            modifier = Modifier.weight(1f),
        )
        Text(
            if (enabled) "On" else "Off",
            style = MaterialTheme.typography.labelLarge,
            color = if (enabled) MaterialTheme.colorScheme.primary
            else MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

/**
 * One device row: name + status on the left, then two EXPLICIT focusable buttons —
 * Ignore (mark "never export", e.g. the TV remote) and Share (toggle USB/IP export).
 * Both are their own D-pad targets (no hidden long-press, which doesn't fire reliably on
 * a TV remote); an ignored device shows only "Allow" since sharing is forced off.
 */
@Composable
private fun DeviceItem(row: DeviceRow, onToggle: () -> Unit, onIgnore: () -> Unit) {
    val status = when {
        row.blocked -> "ignored"
        row.exporting -> "exporting"
        row.online -> "online"
        else -> "offline"
    }
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 56.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .alpha(if (row.online || row.blocked) 1f else 0.6f)
            .padding(start = 12.dp, top = 8.dp, end = 8.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(row.name, style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurface)
            Text(
                "${hex(row.vid)}:${hex(row.pid)} · $status",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        // Two explicit focusable buttons (D-pad reliable — no hidden long-press). Ignore
        // marks "never share" (e.g. the remote); Share toggles export. Ignored devices show
        // only "Allow" (sharing is forced off until allowed).
        ActionPill(
            label = if (row.blocked) "Allow" else "Ignore",
            emphasized = row.blocked,
            emphasisColor = MaterialTheme.colorScheme.error,
            onClick = onIgnore,
        )
        if (!row.blocked) {
            Spacer(Modifier.width(8.dp))
            ActionPill(
                label = if (row.enabled) "Stop" else "Share",
                emphasized = row.enabled,
                emphasisColor = MaterialTheme.colorScheme.primary,
                enabled = row.online,
                onClick = onToggle,
            )
        }
    }
}

/** A focusable pill button — its own D-pad target (gold ring via [triremeFocus]). */
@Composable
private fun ActionPill(
    label: String,
    emphasized: Boolean,
    emphasisColor: Color,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    var focused by remember { mutableStateOf(false) }
    val fg = when {
        !enabled -> MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.4f)
        emphasized -> emphasisColor
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Box(
        Modifier
            .heightIn(min = 40.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(
                if (emphasized) emphasisColor.copy(alpha = 0.15f)
                else MaterialTheme.colorScheme.surfaceContainerHighest,
            )
            .onFocusChanged { focused = it.isFocused }
            .clickable(enabled = enabled, onClick = onClick)
            .triremeFocus(focused)
            .semantics { role = Role.Button }
            .padding(horizontal = 16.dp, vertical = 8.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = MaterialTheme.typography.labelLarge, color = fg)
    }
}
