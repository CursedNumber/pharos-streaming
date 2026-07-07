// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.compose.runtime.mutableStateOf
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.ConcurrentHashMap
import uniffi.core_ffi.UsbDeviceInfo
import uniffi.core_ffi.UsbExportHandle
import uniffi.core_ffi.UsbInterfaceInfo
import uniffi.core_ffi.UsbTransfer
import uniffi.core_ffi.UsbTransferResult
import uniffi.core_ffi.startUsbExport

private const val TAG = "TriremeUSB"

/** Host-side USB/IP import listener port (`pharos-usbimport`); mirrors the Rust
 *  `core_proto::ports::USBIP`. The export target is `<host>:USBIP_PORT`. */
const val USBIP_PORT = 4434
private const val ACTION_USB_PERMISSION = "no.aasle.trireme.USB_PERMISSION"

internal fun key(vid: Int, pid: Int) = (vid shl 16) or pid
internal fun epKey(num: Int, dirIn: Boolean) = (num shl 1) or (if (dirIn) 1 else 0)
internal fun hex(v: Int) = "%04x".format(v)

/** A persisted choice of device to share + its last-known friendly name.
 *  `blocked` = the user marked it "never export" (e.g. the TV remote, whose keys
 *  would otherwise be tunnelled to the host and stop driving the local UI). A
 *  blocked device is never forwarded, never prompts for permission, and its export
 *  toggle is inert until it's un-ignored. */
data class Entry(
    val vid: Int,
    val pid: Int,
    val name: String,
    val enabled: Boolean,
    val blocked: Boolean = false,
)

/** A UI row: a persisted entry plus live state (present? exporting?). */
data class DeviceRow(
    val vid: Int,
    val pid: Int,
    val name: String,
    val enabled: Boolean,
    val online: Boolean,
    val exporting: Boolean,
    val blocked: Boolean,
)

/** Persistent store for the share-list + Pharos address. */
class UsbStore(ctx: Context) {
    private val app = ctx.applicationContext
    private val p = app.getSharedPreferences("trireme_usb", Context.MODE_PRIVATE)

    /** USB/IP export target `<host>:port`. An explicit user override wins;
     *  otherwise it's derived from the paired host's address (its LAN host + the
     *  USB/IP port), so it follows whatever host you actually paired with. Empty
     *  when no host is known yet — the export screen then asks for one. We never
     *  ship a hard-coded personal LAN address as the default. */
    fun pharos(): String =
        p.getString("pharos", null)?.takeIf { it.isNotBlank() } ?: derivedPharos() ?: ""

    fun setPharos(a: String) = p.edit().putString("pharos", a).apply()

    /** Best-effort export target from the saved host registry: a paired entry if
     *  there is one, else the first known host; its host part + [USBIP_PORT].
     *  Null when no host has been added yet. */
    private fun derivedPharos(): String? {
        val servers = ServerStore(app).load().servers
        val host = (servers.firstOrNull { it.paired } ?: servers.firstOrNull())?.host
        return host?.takeIf { it.isNotBlank() }?.let { "$it:$USBIP_PORT" }
    }

    /** Master USB/IP switch. Default OFF: with it off nothing is forwarded, the
     *  service never runs, and a plug-in just bounces straight to the stream — no
     *  export screen, no permission prompts. The user opts in from the export
     *  screen (or Settings). */
    fun usbIpEnabled(): Boolean = p.getBoolean("usbip_enabled", false)
    fun setUsbIpEnabled(on: Boolean) = p.edit().putBoolean("usbip_enabled", on).apply()

    fun entries(): LinkedHashMap<Int, Entry> {
        val out = LinkedHashMap<Int, Entry>()
        val arr = JSONArray(p.getString("entries", "[]"))
        for (i in 0 until arr.length()) {
            val o = arr.getJSONObject(i)
            val e = Entry(
                o.getInt("vid"), o.getInt("pid"), o.getString("name"),
                o.getBoolean("enabled"), o.optBoolean("blocked", false),
            )
            out[key(e.vid, e.pid)] = e
        }
        return out
    }

    fun save(entries: Collection<Entry>) {
        val arr = JSONArray()
        entries.forEach {
            arr.put(
                JSONObject().put("vid", it.vid).put("pid", it.pid).put("name", it.name)
                    .put("enabled", it.enabled).put("blocked", it.blocked),
            )
        }
        p.edit().putString("entries", arr.toString()).apply()
    }

    fun anyEnabled(): Boolean = entries().values.any { it.enabled }
}

/**
 * The USB/IP export engine: forwards enabled + present USB devices to a Pharos host.
 *
 * Extracted from [UsbExportActivity] so it can run inside [UsbExportService] (a
 * foreground service) and keep forwarding while the user is on another screen —
 * e.g. streaming a game — instead of only while the export screen is open. A
 * process singleton: the service owns the periodic [tick] loop; the UI reads
 * [rows] (Compose state) and toggles devices via [toggle].
 */
class UsbExportEngine private constructor(context: Context) {
    private val app = context.applicationContext
    private val usb = app.getSystemService(Context.USB_SERVICE) as UsbManager
    private val store = UsbStore(app)
    private val active = ConcurrentHashMap<Int, ActiveExport>()
    // Devices we've shown an "Allow access?" dialog for THIS SESSION — we do not auto-
    // re-ask. requestMissingPermissions() runs on every onResume, and the system dialog
    // itself pauses→resumes us, so re-asking would re-prompt after every Cancel forever
    // (the X90J never persists the grant) — the "asks 14 times" loop. We ask once; the
    // user re-asks by explicitly re-enabling the device. Cleared on re-enable, unplug, stop.
    private val askedPerm = ConcurrentHashMap.newKeySet<Int>()
    private val handler = Handler(Looper.getMainLooper())
    private var wifiLock: WifiManager.WifiLock? = null
    @Volatile private var looping = false

    /** Compose-observable device rows for the UI. */
    val rows = mutableStateOf<List<DeviceRow>>(emptyList())

    /** Fired by the loop when no devices are enabled, so the service can stop itself. */
    var onIdle: (() -> Unit)? = null

    private class ActiveExport(
        val conn: UsbDeviceConnection,
        val handle: UsbExportHandle,
        val sink: TransferSink,
    )

    private val permReceiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, i: Intent) {
            // A permission result arrived — re-run the loop so a just-granted device starts
            // exporting. We deliberately KEEP the device in askedPerm (no re-prompt after the
            // user answered, granted OR denied — re-asking is exactly the nag loop). A fresh
            // prompt only happens when the user explicitly re-enables the device (see toggle).
            if (i.action == ACTION_USB_PERMISSION) tick()
        }
    }

    private val supervisor = object : Runnable {
        override fun run() {
            tick()
            if (looping) handler.postDelayed(this, 2000)
        }
    }

    /**
     * Begin the periodic supervisor loop and hold the low-latency wifi lock (so
     * power-save doesn't park the radio and spike the USB/IP round-trip). The
     * service calls this; idempotent.
     */
    fun startLoop() {
        if (looping) return
        looping = true
        val flags = if (Build.VERSION.SDK_INT >= 33) Context.RECEIVER_EXPORTED else 0
        runCatching { app.registerReceiver(permReceiver, IntentFilter(ACTION_USB_PERMISSION), flags) }
        val wifi = app.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val mode = if (Build.VERSION.SDK_INT >= 29)
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY else WifiManager.WIFI_MODE_FULL_HIGH_PERF
        wifiLock = wifi.createWifiLock(mode, "trireme:usb-export").apply { acquire() }
        handler.post(supervisor)
    }

    /** Stop the loop and every active export; release the lock + receiver. */
    fun stopLoop() {
        if (!looping) return
        looping = false
        handler.removeCallbacks(supervisor)
        for (k in active.keys.toList()) stopExport(k)
        askedPerm.clear() // receiver gone → no result will arrive to clear these
        runCatching { app.unregisterReceiver(permReceiver) }
        wifiLock?.let { if (it.isHeld) it.release() }
        wifiLock = null
    }

    /**
     * Display-only refresh for the UI: enroll newly-seen devices and recompute
     * [rows]. Does NOT start exports — those are owned by the service loop — so
     * the export screen can list available devices without the service running.
     */
    fun refreshRows() {
        val present = usb.deviceList.values
        val presentKeys = present.map { key(it.vendorId, it.productId) }.toHashSet()
        val entries = store.entries()
        for (d in present) {
            if (d.deviceClass == UsbConstants.USB_CLASS_HUB) continue
            entries.getOrPut(key(d.vendorId, d.productId)) {
                Entry(d.vendorId, d.productId, friendlyName(d), false)
            }
        }
        store.save(entries.values)
        rows.value = entries.values.map { e ->
            val k = key(e.vid, e.pid)
            DeviceRow(e.vid, e.pid, e.name, e.enabled, presentKeys.contains(k), active.containsKey(k), e.blocked)
        }
    }

    fun anyEnabled(): Boolean = store.anyEnabled()

    /**
     * Ask the user to grant USB access for every enabled + present device that
     * lacks it. Call this from the foreground export screen: because the app is a
     * registered USB handler (the device_filter on [UsbExportActivity]) and the
     * request comes from the visible activity, the system dialog offers the "use
     * by default" checkbox — the only way to a *persistent* grant on this TV.
     */
    fun requestMissingPermissions() {
        if (!store.usbIpEnabled()) return // master off — never prompt
        val present = usb.deviceList.values.associateBy { key(it.vendorId, it.productId) }
        for (e in store.entries().values) {
            if (!e.enabled || e.blocked) continue // never prompt for an ignored device
            val k = key(e.vid, e.pid)
            if (askedPerm.contains(k)) continue // already asked this session — never re-prompt (the nag-loop fix)
            val dev = present[k] ?: continue
            if (!usb.hasPermission(dev)) requestPermission(dev)
        }
    }

    /**
     * Flip a device's enabled flag. Returns whether any device is still enabled
     * (so the caller can start/stop the service). Enabling an export is picked up
     * by the next service [tick]; disabling stops it immediately.
     */
    fun toggle(row: DeviceRow): Boolean {
        val entries = store.entries()
        val e = entries[key(row.vid, row.pid)] ?: return anyEnabled()
        if (e.blocked) return anyEnabled() // ignored devices can't be exported
        val k = key(row.vid, row.pid)
        entries[k] = e.copy(enabled = !e.enabled)
        store.save(entries.values)
        if (e.enabled) {
            stopExport(k) // was on → now off
        } else {
            askedPerm.remove(k) // turning ON: allow one fresh permission prompt
        }
        refreshRows()
        return anyEnabled()
    }

    /**
     * Mark a device as ignored (never export / never ask) or un-ignore it. Ignoring
     * also disables + stops any current export immediately — so the user can reclaim a
     * device whose input was being tunnelled (e.g. the TV remote) and Trireme will
     * never grab it again. Persisted, so it survives restarts.
     */
    fun setIgnored(row: DeviceRow, ignored: Boolean) {
        val entries = store.entries()
        val k = key(row.vid, row.pid)
        val e = entries[k] ?: return
        entries[k] = e.copy(blocked = ignored, enabled = if (ignored) false else e.enabled)
        store.save(entries.values)
        if (ignored) stopExport(k)
        refreshRows()
    }

    /** One supervisor pass: enroll, prune vanished exports, (re)start enabled + online. */
    private fun tick() {
        // Master switch off — stop any exports and let the service wind down. A
        // belt-and-braces guard: the service won't even start when the master is
        // off, but if it's flipped off mid-session this reclaims every device.
        if (!store.usbIpEnabled()) {
            for (k in active.keys.toList()) stopExport(k)
            onIdle?.invoke()
            return
        }
        val present = usb.deviceList.values
        val presentKeys = present.map { key(it.vendorId, it.productId) }.toHashSet()
        askedPerm.retainAll(presentKeys) // drop awaited prompts for unplugged devices
        val byKey = present.associateBy { key(it.vendorId, it.productId) }
        val entries = store.entries()
        for (d in present) {
            if (d.deviceClass == UsbConstants.USB_CLASS_HUB) continue
            entries.getOrPut(key(d.vendorId, d.productId)) {
                Entry(d.vendorId, d.productId, friendlyName(d), false)
            }
        }
        store.save(entries.values)
        for (k in active.keys.filter { !presentKeys.contains(it) }) {
            Log.i(TAG, "device ${"%08x".format(k)} unplugged; stopping export")
            stopExport(k)
        }
        for (e in entries.values) {
            val k = key(e.vid, e.pid)
            if (e.enabled && !e.blocked && presentKeys.contains(k) && !active.containsKey(k)) {
                val dev = byKey[k]!!
                // The loop only exports devices that already have permission; it
                // never prompts. Permission is granted from the foreground export
                // screen (requestMissingPermissions), where the system offers the
                // "use by default" persistent grant.
                if (usb.hasPermission(dev)) startExport(dev, e)
            }
        }
        rows.value = entries.values.map { e ->
            val k = key(e.vid, e.pid)
            DeviceRow(e.vid, e.pid, e.name, e.enabled, presentKeys.contains(k), active.containsKey(k), e.blocked)
        }
        if (!store.anyEnabled()) onIdle?.invoke()
    }

    private fun stopExport(k: Int) {
        active.remove(k)?.let { it.handle.cancel(); it.sink.shutdown() }
    }

    private fun startExport(dev: UsbDevice, entry: Entry) {
        val conn = usb.openDevice(dev) ?: run {
            Log.w(TAG, "openDevice('${entry.name}') failed (no permission?)")
            return
        }
        val endpoints = HashMap<Int, UsbEndpoint>()
        for (i in 0 until dev.interfaceCount) {
            val iface = dev.getInterface(i)
            if (!conn.claimInterface(iface, true)) {
                Log.w(TAG, "claimInterface ${iface.id} failed for '${entry.name}'")
            }
            for (j in 0 until iface.endpointCount) {
                val ep = iface.getEndpoint(j)
                val dirIn = ep.direction == UsbConstants.USB_DIR_IN
                endpoints[epKey(ep.endpointNumber, dirIn)] = ep
            }
        }
        val sink = TransferSink(conn, endpoints) { err -> onExportEnd(entry, err) }
        val devid = dev.deviceId.toUInt()
        Log.i(TAG, "exporting '${entry.name}' [${hex(entry.vid)}:${hex(entry.pid)}] → ${store.pharos()}")
        val handle = startUsbExport(store.pharos(), entry.name, devid, 3u, deviceInfo(dev), sink) // speed 3 = high
        active[key(entry.vid, entry.pid)] = ActiveExport(conn, handle, sink)
    }

    /** Descriptor fields for the v2 hello (what a Windows import bridge echoes
     *  into OP_REP_IMPORT). Android's `deviceId` encodes the Linux bus/dev pair
     *  as `bus * 1000 + dev` (see AOSP `usb_device_get_unique_id`). */
    private fun deviceInfo(dev: UsbDevice) = UsbDeviceInfo(
        busnum = (dev.deviceId / 1000).toUInt(),
        devnum = (dev.deviceId % 1000).toUInt(),
        vendorId = dev.vendorId.toUShort(),
        productId = dev.productId.toUShort(),
        bcdDevice = bcdVersion(dev.version),
        deviceClass = dev.deviceClass.toUByte(),
        deviceSubclass = dev.deviceSubclass.toUByte(),
        deviceProtocol = dev.deviceProtocol.toUByte(),
        configurationValue = if (dev.configurationCount > 0) dev.getConfiguration(0).id.toUByte() else 1u,
        numConfigurations = dev.configurationCount.toUByte(),
        // Android lists every *alternate setting* as its own UsbInterface (same
        // id, differing altSetting), so a plain map would over-count bNumInterfaces
        // and duplicate class triples — corrupting the OP_REP_IMPORT block the
        // Windows vhci builds its PDO from. Dedup by id (first = altSetting 0, the
        // active one), matching the Rust exporter's one-triple-per-interface hello.
        interfaces = (0 until dev.interfaceCount).map { dev.getInterface(it) }
            .distinctBy { it.id }
            .map { UsbInterfaceInfo(it.interfaceClass.toUByte(), it.interfaceSubclass.toUByte(), it.interfaceProtocol.toUByte()) },
    )

    /** `UsbDevice.version` ("2.25") → BCD (0x0225), the `bcdDevice` coding.
     *  Each of major/minor is two BCD nibbles; unparseable → 0. */
    private fun bcdVersion(v: String): UShort {
        val m = Regex("""(\d+)\.(\d+)""").find(v) ?: return 0u
        val maj = (m.groupValues[1].toIntOrNull() ?: 0).coerceIn(0, 99)
        val minor = m.groupValues[2].padStart(2, '0').take(2)
        val bcd = ((maj / 10) shl 12) or ((maj % 10) shl 8) or
            ((minor[0] - '0') shl 4) or (minor[1] - '0')
        return bcd.toUShort()
    }

    private fun onExportEnd(entry: Entry, error: String?) {
        active.remove(key(entry.vid, entry.pid))?.sink?.shutdown()
        Log.i(TAG, "export of '${entry.name}' ended: ${error ?: "ok"}")
    }

    private fun requestPermission(dev: UsbDevice) {
        askedPerm.add(key(dev.vendorId, dev.productId)) // remember we asked this session (no auto re-prompt)
        val flags = if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
        val pi = PendingIntent.getBroadcast(app, 0, Intent(ACTION_USB_PERMISSION).setPackage(app.packageName), flags)
        usb.requestPermission(dev, pi)
    }

    private fun friendlyName(d: UsbDevice): String {
        val prod = if (Build.VERSION.SDK_INT >= 21) d.productName else null
        val mfr = if (Build.VERSION.SDK_INT >= 21) d.manufacturerName else null
        return when {
            !prod.isNullOrBlank() && !mfr.isNullOrBlank() -> "$prod ($mfr)"
            !prod.isNullOrBlank() -> prod
            !mfr.isNullOrBlank() -> mfr
            else -> "${hex(d.vendorId)}:${hex(d.productId)}"
        }
    }

    companion object {
        @Volatile private var instance: UsbExportEngine? = null
        fun get(context: Context): UsbExportEngine =
            instance ?: synchronized(this) {
                instance ?: UsbExportEngine(context).also { instance = it }
            }
    }
}

/** The Kotlin hardware backend the Rust core calls per URB. */
class TransferSink(
    private val conn: UsbDeviceConnection,
    private val endpoints: Map<Int, UsbEndpoint>,
    private val onEndCb: (String?) -> Unit,
) : UsbTransfer {
    private val timeout = 1000
    @Volatile private var closed = false

    /** Stop in-flight transfers and release the device. Closing the connection
     *  unblocks a pending bulkTransfer (it returns < 0); `closed` makes the
     *  interrupt re-pend loop exit instead of retrying. */
    fun shutdown() {
        closed = true
        conn.runCatching { close() }
    }

    override fun transfer(
        ep: UByte,
        dirIn: Boolean,
        setup: ByteArray,
        outData: ByteArray,
        length: UInt,
    ): UsbTransferResult {
        if (ep.toInt() == 0) {
            val bm = setup[0].toInt() and 0xff
            val req = setup[1].toInt() and 0xff
            val value = (setup[2].toInt() and 0xff) or ((setup[3].toInt() and 0xff) shl 8)
            val index = (setup[4].toInt() and 0xff) or ((setup[5].toInt() and 0xff) shl 8)
            return if (dirIn) {
                val buf = ByteArray(length.toInt())
                val n = conn.controlTransfer(bm, req, value, index, buf, buf.size, timeout)
                if (n < 0) UsbTransferResult(-32, 0, ByteArray(0)) else UsbTransferResult(0, n, buf.copyOf(n))
            } else {
                val n = conn.controlTransfer(bm, req, value, index, outData, outData.size, timeout)
                if (n < 0) UsbTransferResult(-32, 0, ByteArray(0)) else UsbTransferResult(0, maxOf(n, 0), ByteArray(0))
            }
        }
        val endpoint = endpoints[epKey(ep.toInt(), dirIn)]
            ?: return UsbTransferResult(-19, 0, ByteArray(0)) // -ENODEV
        // Interrupt IN (HID input reports, HCI events): the host pends this URB
        // indefinitely, and the device only sends when there's something (a
        // controller is idle until you move a stick). Re-pend across timeout
        // slices so an idle device never returns a spurious -110 — the exact bug
        // that broke btusb and would kill a controller.
        if (dirIn && endpoint.type == UsbConstants.USB_ENDPOINT_XFER_INT) {
            return interruptRead(endpoint, length.toInt())
        }
        // Bulk (either direction) + interrupt OUT (rumble): one transfer.
        return if (dirIn) {
            val buf = ByteArray(length.toInt())
            val n = conn.bulkTransfer(endpoint, buf, buf.size, timeout)
            if (n < 0) UsbTransferResult(-110, 0, ByteArray(0)) else UsbTransferResult(0, n, buf.copyOf(n))
        } else {
            val n = conn.bulkTransfer(endpoint, outData, outData.size, timeout)
            if (n < 0) UsbTransferResult(-110, 0, ByteArray(0)) else UsbTransferResult(0, maxOf(n, 0), ByteArray(0))
        }
    }

    /** bulkTransfer works on interrupt endpoints (the kernel submits the right URB
     *  type — the btusb -110 proved it reached the endpoint, it just timed out).
     *  The only fix needed: keep re-pending on the idle timeout instead of failing.
     *  A burst of *instant* failures means the device is gone → bail with -ENODEV. */
    private fun interruptRead(ep: UsbEndpoint, length: Int): UsbTransferResult {
        val buf = ByteArray(maxOf(length, 1))
        var fastFails = 0
        while (!closed) {
            val t0 = System.nanoTime()
            val n = conn.bulkTransfer(ep, buf, buf.size, 50) // 50ms slices: limits CMD_UNLINK drop window
            if (n >= 0) return UsbTransferResult(0, n, buf.copyOf(n))
            // n < 0: idle timeout (re-pend) or error. Near-instant returns ⇒ gone.
            if ((System.nanoTime() - t0) / 1_000_000 < 50) {
                if (++fastFails >= 10) return UsbTransferResult(-19, 0, ByteArray(0))
                try { Thread.sleep(20) } catch (_: InterruptedException) {}
            } else {
                fastFails = 0
            }
        }
        return UsbTransferResult(-19, 0, ByteArray(0)) // cancelled
    }

    override fun onEnd(error: String?) = onEndCb(error)
}
