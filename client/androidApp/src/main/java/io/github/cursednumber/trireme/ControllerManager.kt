// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme

import android.content.Context
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.hardware.display.DisplayManager
import android.hardware.input.InputManager
import android.os.Build
import android.util.Log
import android.view.Display
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import io.github.cursednumber.trireme.stream.StreamHub
import uniffi.core_ffi.GamepadInput
import uniffi.core_ffi.classifyController
import uniffi.core_ffi.defaultControllerProfile
import kotlin.math.abs

/// A connected gamepad's exposed IMU, as seen by the standard Android sensor API.
/// Surfaced read-only in the debug HUD to CONFIRM gyro/accel are visible (e.g. a
/// Switch-Pro-mode Flydigi Vader on Android 12+, where the kernel hid-nintendo
/// driver decodes the controller IMU into an InputDevice sensor). See
/// [ControllerManager.probeSensors].
data class PadSensorCaps(
    val name: String,
    val vendorId: Int,
    val productId: Int,
    val gyro: Boolean,
    val accel: Boolean,
)

/**
 * Detects, classifies, and forwards every connected game controller to the host.
 *
 * Each controller gets a slot (one host virtual Xbox 360 pad per slot). Buttons
 * map by keycode and sticks/triggers/hat by [MotionEvent] axis — both to the
 * canonical Trireme indices. Controllers with a gyroscope (DualShock/DualSense/
 * Switch/Flydigi) also stream motion samples, which the host turns into mouse
 * aim. The detected model + capabilities come from the shared [classifyController]
 * (also surfaced in the selector UI); the presentation to the game is always a
 * standard Xbox pad for maximum compatibility.
 */
class ControllerManager(
    context: Context,
    // The live session's GamepadInput. A `var` (not `val`): a reconnect/re-dial
    // swaps the session handle, and [rebind] re-points every forwarded event —
    // including the gyro-listener closure, which reads this property at call time —
    // at the fresh one so the sink can never be left targeting a dead handle.
    private var sink: GamepadInput,
    // Controller-handling master switch (Settings / Controllers pane). When true
    // (managed) we apply per-controller profiles, swaps and gyro; when false (raw)
    // pads are still forwarded but with the host DEFAULT profile and no gyro — see
    // [attach]. The pad works either way; "off" just means no Trireme processing.
    private val managed: Boolean,
) : InputManager.InputDeviceListener {
    private val appContext = context.applicationContext
    private val inputManager =
        context.getSystemService(Context.INPUT_SERVICE) as InputManager
    private val prefs = GamepadPrefs(context)

    private val slotOf = HashMap<Int, Int>() // deviceId -> slot
    private val used = BooleanArray(MAX_PADS)
    private val gyro = HashMap<Int, GyroReg>() // deviceId -> sensor registration
    private var phoneGyro: GyroReg? = null // this phone's own IMU, when it's the gyro source
    private val lastButton = HashMap<Long, Boolean>()
    private val lastAxis = HashMap<Long, Float>()
    private val dpadHeld = HashMap<Int, MutableSet<Int>>() // slot -> held d-pad keycodes
    private val vidPidToSlot = HashMap<Long, Int>() // (vid<<16|pid) -> slot

    private class GyroReg(val sm: SensorManager, val listener: SensorEventListener)

    /** Begin listening; attach any already-connected controllers. */
    fun start() {
        inputManager.registerInputDeviceListener(this, null)
        for (id in inputManager.inputDeviceIds) {
            InputDevice.getDevice(id)?.let { if (isGamepad(it)) attach(it) }
        }
        relayUsbExportProfiles()
        // Phone gyro is its own opt-in, independent of controller handling: raw
        // mode only means "don't process the pad", not "no phone sensors".
        if (StreamHub.isGyroEnabled() && StreamHub.isPhoneGyro()) registerPhoneGyro()
    }

    /**
     * Relay saved profiles for controllers exported to this host over USB/IP.
     * Their input flows host-side (not through Android), so they never `attach`
     * here — but the host still applies their gyro feel from the relayed profile
     * (the host's IMU reader picks up whichever gyro controller is present). We
     * push them once at stream start, keyed by VID:PID, slot 0.
     */
    private fun relayUsbExportProfiles() {
        if (!managed) return // raw mode: host keeps its default profile, no relay
        runCatching {
            val usb = appContext.getSharedPreferences("trireme_usb", Context.MODE_PRIVATE)
            val arr = org.json.JSONArray(usb.getString("entries", "[]"))
            for (i in 0 until arr.length()) {
                val o = arr.getJSONObject(i)
                if (!o.getBoolean("enabled")) continue
                val vid = o.getInt("vid")
                val pid = o.getInt("pid")
                val key = GamepadPrefs.controllerKey(vid, pid)
                sink.setTuning(0.toUByte(), vid.toUShort(), pid.toUShort(), prefs.profile(key))
                Log.i(TAG, "relayed USB/IP profile for $key")
            }
        }
    }

    fun stop() {
        inputManager.unregisterInputDeviceListener(this)
        for (id in slotOf.keys.toList()) detach(id)
        unregisterPhoneGyro()
    }

    /**
     * Re-point every forwarded event at [newSink] after a reconnect/re-dial swapped
     * the session's GamepadInput, and re-announce all currently-attached pads
     * (connect + saved tuning) so the fresh host session registers them. Cheaper and
     * safer than tearing the manager down and re-detecting devices: the [InputManager]
     * listener + gyro sensor registrations survive (no NS-mode re-detect churn), and
     * `sink` is never left pointing at a dead handle — the failure mode where
     * onKey/onMotion return true but the events vanish into a closed input stream
     * and the host shows "pad none". No-op when the handle is unchanged.
     */
    fun rebind(newSink: GamepadInput) {
        if (newSink === sink) return
        sink = newSink
        // Announce every pad we currently hold to the new host session. Snapshot the
        // slot map so a concurrent add/remove on the input thread can't CME.
        for ((deviceId, slot) in slotOf.entries.toList()) {
            val d = InputDevice.getDevice(deviceId) ?: continue
            sink.connect(slot.toUByte(), d.vendorId.toUShort(), d.productId.toUShort())
            val key = GamepadPrefs.controllerKey(d.vendorId, d.productId)
            // The gyro choices survive raw mode: they only shape motion the user opted
        // into separately (the phone gyro works with handling off), so the gyro
        // screen stays meaningful either way.
        val profile = if (managed) {
            prefs.profile(key)
        } else {
            defaultControllerProfile().copy(gyro = prefs.profile(key).gyro)
        }
            sink.setTuning(slot.toUByte(), d.vendorId.toUShort(), d.productId.toUShort(), profile)
        }
        if (managed) relayUsbExportProfiles()
        Log.i(TAG, "controllers rebound to fresh session handle (${slotOf.size} pad(s) re-announced)")
    }

    override fun onInputDeviceAdded(deviceId: Int) {
        InputDevice.getDevice(deviceId)?.let { if (isGamepad(it)) attach(it) }
    }

    override fun onInputDeviceRemoved(deviceId: Int) = detach(deviceId)

    override fun onInputDeviceChanged(deviceId: Int) {
        // A device may have changed mode in place (e.g. mouse↔gamepad, as
        // controllers that mode-switch do). Only act on a real gamepad-ness flip:
        // Android also reports "changed" on every input reconfigure (pointer-capture
        // toggles, focus loss to a heads-up notification, battery/LED updates), and
        // tearing a still-valid pad down made the host destroy + recreate its
        // virtual pad every few seconds (a GameSir G8 over OTG: every ~10s).
        val d = InputDevice.getDevice(deviceId)
        val nowPad = d != null && isGamepad(d)
        val attached = slotOf.containsKey(deviceId)
        when {
            attached && !nowPad -> detach(deviceId)
            !attached && nowPad -> attach(d!!)
        }
    }

    private fun attach(d: InputDevice) {
        if (slotOf.containsKey(d.id)) return
        val slot = allocSlot() ?: return
        slotOf[d.id] = slot
        val key = GamepadPrefs.controllerKey(d.vendorId, d.productId)
        vidPidToSlot[vidPidKey(d.vendorId, d.productId)] = slot
        sink.connect(slot.toUByte(), d.vendorId.toUShort(), d.productId.toUShort())
        // Managed: push this controller's saved tuning so the host applies its feel
        // (gyro + face swaps) the instant it connects — and re-asserts it after a
        // host restart or a client reinstall. Raw (handling off): push the host
        // DEFAULT face buttons instead so any swap previously left on the host is
        // reset: "raw" must mean no swaps regardless of what the host remembers.
        val profile = if (managed) prefs.profile(key) else defaultControllerProfile()
        sink.setTuning(slot.toUByte(), d.vendorId.toUShort(), d.productId.toUShort(), profile)
        val info = runCatching {
            classifyController(d.vendorId.toUShort(), d.productId.toUShort(), d.name ?: "")
        }.getOrNull()
        Log.i(
            TAG,
            "controller +slot$slot: ${d.name} " +
                "${"%04x:%04x".format(d.vendorId, d.productId)} = ${info?.label} " +
                "(${if (managed) "managed" else "raw"})",
        )
        // Register gyro only when controller handling is on AND the user opted in
        // via Settings → "Send controller gyro to host". Raw mode forwards
        // sticks/buttons but no motion. This gate is essential, not just an
        // optimisation: the host's GamepadMotion→mouse path (host_input.rs) runs
        // UNCONDITIONALLY — unlike its USB/IP IMU path it honours neither wants_gyro
        // nor the per-app GyroMode — so a phone-attached pad's gyro would flood the
        // host cursor (fighting the pad, "wild" motion) with the toggle off. Read at
        // attach; a live flip applies on the next reconnect, matching the wants_gyro
        // handshake contract (the setting "applies on next dial").
        // With the phone as the gyro source the pad's own IMU stays off: two
        // sources on one slot would be summed host-side.
        if (managed && StreamHub.isGyroEnabled() && !StreamHub.isPhoneGyro()) registerGyro(d, slot)
    }

    private fun detach(deviceId: Int) {
        val slot = slotOf.remove(deviceId) ?: return
        used[slot] = false
        gyro.remove(deviceId)?.let { it.sm.unregisterListener(it.listener) }
        lastButton.keys.removeAll { slotOfKey(it) == slot }
        lastAxis.keys.removeAll { slotOfKey(it) == slot }
        dpadHeld.remove(slot)
        vidPidToSlot.entries.removeAll { it.value == slot }
        sink.disconnect(slot.toUByte())
        Log.i(TAG, "controller -slot$slot")
    }

    /**
     * Live slot index for the controller with this VID:PID, or null if it isn't
     * currently attached on this device. Used by the panel's per-controller
     * toggles (Swap A/B, Swap X/Y) to relay a tuning push to the host's CURRENT
     * slot — otherwise the relay would land on slot 0 and the live pad (e.g.
     * slot 1) would keep its old swap until the next reconnect.
     */
    fun slotOf(vendor: Int, product: Int): Int? = vidPidToSlot[vidPidKey(vendor, product)]

    private fun vidPidKey(vendor: Int, product: Int): Long =
        ((vendor.toLong() and 0xffff) shl 16) or (product.toLong() and 0xffff)

    /** @return true if this key was a gamepad button (consumed). */
    fun onKey(event: KeyEvent): Boolean {
        val kc = event.keyCode
        val knownSlot = slotOf[event.deviceId]
        // Default to slot 0 for an as-yet-unregistered device (attach race, or a
        // test injection) — only real gamepad codes get this far.
        val slot = knownSlot ?: 0
        val pressed = event.action == KeyEvent.ACTION_DOWN

        // Digital triggers: pads like the Switch Pro report ZL/ZR as buttons, not
        // analog axes. Map them onto the trigger axes (0/1) so they're not lost.
        val trig = when (kc) {
            KeyEvent.KEYCODE_BUTTON_L2 -> AX_LT
            KeyEvent.KEYCODE_BUTTON_R2 -> AX_RT
            else -> -1
        }
        if (trig >= 0) {
            if (event.repeatCount == 0) {
                StreamHub.claimGamepadSource()
                sink.axis(slot.toUByte(), trig.toUByte(), if (pressed) 1f else 0f)
            }
            return true
        }

        // D-pad as discrete keys → hat axes, but ONLY from a registered controller
        // so the TV remote's d-pad keeps navigating the launcher.
        if (knownSlot != null && isDpadKey(kc)) {
            if (event.repeatCount == 0) updateDpad(slot, kc, pressed)
            return true
        }

        // Face/shoulder/menu buttons.
        // NOTE: the client transmits RAW canonical indices (0=A, 1=B, 2=X, 3=Y).
        // The host is the single source of truth for Swap A/B and Swap X/Y —
        // applied from the per-controller profile in `PadManager::button`. Doing
        // it both places used to double-swap (cancel out); see also
        // `ControllersPane` which now relays toggle edits to the host directly.
        val idx = buttonIndex(kc) ?: return false
        if (event.repeatCount > 0) return true
        val key = stateKey(slot, idx)
        if (lastButton[key] == pressed) return true
        lastButton[key] = pressed
        StreamHub.claimGamepadSource()
        sink.button(slot.toUByte(), idx.toUByte(), pressed)
        return true
    }

    private fun isDpadKey(kc: Int): Boolean = kc == KeyEvent.KEYCODE_DPAD_UP ||
        kc == KeyEvent.KEYCODE_DPAD_DOWN ||
        kc == KeyEvent.KEYCODE_DPAD_LEFT ||
        kc == KeyEvent.KEYCODE_DPAD_RIGHT

    /** Track held d-pad directions per slot and emit the resolved hat axes. */
    private fun updateDpad(slot: Int, kc: Int, pressed: Boolean) {
        StreamHub.claimGamepadSource()
        val held = dpadHeld.getOrPut(slot) { mutableSetOf() }
        if (pressed) held.add(kc) else held.remove(kc)
        val hx = (if (KeyEvent.KEYCODE_DPAD_RIGHT in held) 1f else 0f) +
            (if (KeyEvent.KEYCODE_DPAD_LEFT in held) -1f else 0f)
        val hy = (if (KeyEvent.KEYCODE_DPAD_DOWN in held) 1f else 0f) +
            (if (KeyEvent.KEYCODE_DPAD_UP in held) -1f else 0f)
        sink.axis(slot.toUByte(), AX_HX.toUByte(), hx)
        sink.axis(slot.toUByte(), AX_HY.toUByte(), hy)
    }

    /** @return true if this was joystick motion from a known controller (consumed). */
    fun onMotion(event: MotionEvent): Boolean {
        if (!event.isFromSource(InputDevice.SOURCE_JOYSTICK)) return false
        if (event.action != MotionEvent.ACTION_MOVE) return false
        val slot = slotOf[event.deviceId] ?: 0
        sendAxis(slot, AX_LX, event.getAxisValue(MotionEvent.AXIS_X), stick = true)
        sendAxis(slot, AX_LY, event.getAxisValue(MotionEvent.AXIS_Y), stick = true)
        sendAxis(slot, AX_RX, event.getAxisValue(MotionEvent.AXIS_Z), stick = true)
        sendAxis(slot, AX_RY, event.getAxisValue(MotionEvent.AXIS_RZ), stick = true)
        sendAxis(slot, AX_LT, trigger(event, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE), false)
        sendAxis(slot, AX_RT, trigger(event, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS), false)
        sendAxis(slot, AX_HX, event.getAxisValue(MotionEvent.AXIS_HAT_X), false)
        sendAxis(slot, AX_HY, event.getAxisValue(MotionEvent.AXIS_HAT_Y), false)
        return true
    }

    private fun sendAxis(slot: Int, idx: Int, raw: Float, stick: Boolean) {
        val v = if (stick && abs(raw) < CENTER_DEADZONE) 0f else raw
        val key = stateKey(slot, idx)
        val last = lastAxis[key]
        if (last != null && abs(last - v) < AXIS_EPSILON) return
        lastAxis[key] = v
        // A real, past-deadzone axis change (this only runs when the value actually
        // moved — jitter is deduped above) is deliberate pad use, so claim the
        // gamepad source to mute a stray/gyro cursor fighting the pad.
        StreamHub.claimGamepadSource()
        sink.axis(slot.toUByte(), idx.toUByte(), v)
    }

    private fun registerGyro(d: InputDevice, slot: Int) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val sm = d.sensorManager ?: return
        val g = sm.getDefaultSensor(Sensor.TYPE_GYROSCOPE) ?: return
        val accel = sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)
        val listener = object : SensorEventListener {
            private var ax = 0f
            private var ay = 0f
            private var az = 0f
            override fun onSensorChanged(e: SensorEvent) {
                when (e.sensor.type) {
                    Sensor.TYPE_ACCELEROMETER -> {
                        ax = e.values[0]; ay = e.values[1]; az = e.values[2]
                    }
                    Sensor.TYPE_GYROSCOPE -> {
                        // Stream raw samples — the host's per-controller GyroTuning
                        // owns the scale/sign/axis-pick. No client-side pre-scale.
                        sink.motion(
                            slot.toUByte(),
                            e.values[0], e.values[1], e.values[2],
                            ax, ay, az,
                        )
                    }
                }
            }
            override fun onAccuracyChanged(s: Sensor?, accuracy: Int) {}
        }
        sm.registerListener(listener, g, SensorManager.SENSOR_DELAY_GAME)
        if (accel != null) sm.registerListener(listener, accel, SensorManager.SENSOR_DELAY_GAME)
        gyro[d.id] = GyroReg(sm, listener)
        Log.i(TAG, "gyro streaming for slot$slot (${d.name})")
    }

    /**
     * Stream this phone's own gyro + accelerometer (Settings → Gyro source → Phone),
     * for a phone clipped into a controller or a pad without a gyro. Samples go to
     * the lowest attached pad's slot (slot 0 with none), so an emulator pairs them
     * with player 1. The phone's axes are re-expressed in Trireme's canonical frame
     * (landscape, top edge to the left) whatever the screen rotation, so turning the
     * phone over doesn't flip the host's axis mapping.
     */
    private fun registerPhoneGyro() {
        if (phoneGyro != null) return
        val sm = appContext.getSystemService(Context.SENSOR_SERVICE) as? SensorManager ?: return
        val g = sm.getDefaultSensor(Sensor.TYPE_GYROSCOPE)
        if (g == null) {
            Log.w(TAG, "phone gyro requested but this device has no gyroscope")
            return
        }
        val accel = sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)
        val display = (appContext.getSystemService(Context.DISPLAY_SERVICE) as? DisplayManager)
            ?.getDisplay(Display.DEFAULT_DISPLAY)
        val listener = object : SensorEventListener {
            private val a = FloatArray(3)
            private val out = FloatArray(3)
            override fun onSensorChanged(e: SensorEvent) {
                val rot = display?.rotation ?: Surface.ROTATION_90
                when (e.sensor.type) {
                    Sensor.TYPE_ACCELEROMETER -> canonical(e.values, rot, a)
                    Sensor.TYPE_GYROSCOPE -> {
                        canonical(e.values, rot, out)
                        val slot = slotOf.values.minOrNull() ?: 0
                        sink.motion(slot.toUByte(), out[0], out[1], out[2], a[0], a[1], a[2])
                    }
                }
            }
            override fun onAccuracyChanged(s: Sensor?, accuracy: Int) {}
        }
        sm.registerListener(listener, g, SensorManager.SENSOR_DELAY_GAME)
        if (accel != null) sm.registerListener(listener, accel, SensorManager.SENSOR_DELAY_GAME)
        phoneGyro = GyroReg(sm, listener)
        Log.i(TAG, "phone gyro streaming")
    }

    private fun unregisterPhoneGyro() {
        phoneGyro?.let {
            it.sm.unregisterListener(it.listener)
            Log.i(TAG, "phone gyro stopped")
        }
        phoneGyro = null
    }

    /**
     * Start or stop controller-gyro forwarding LIVE for every attached pad, without
     * waiting for a reconnect. The attach-time gate ([attach]) only decides gyro for
     * pads that connect *after* the Settings flip; this re-applies the choice to pads
     * already connected. Disabling unregisters every IMU listener, so the client truly
     * stops reading the sensor (a real off, not just "don't send") — the clean A/B
     * lever for "is the continuous gyro→mouse stream what's stealing the game's active
     * device from the pad?". No-op in raw mode (gyro is never forwarded there).
     */
    fun setGyroForwarding(enabled: Boolean) {
        if (enabled && StreamHub.isPhoneGyro()) {
            registerPhoneGyro()
            return
        }
        if (!enabled) unregisterPhoneGyro()
        if (!managed) return
        if (enabled) {
            for ((deviceId, slot) in slotOf.entries.toList()) {
                if (gyro.containsKey(deviceId)) continue
                InputDevice.getDevice(deviceId)?.let { registerGyro(it, slot) }
            }
        } else {
            for ((_, reg) in gyro) reg.sm.unregisterListener(reg.listener)
            val n = gyro.size
            gyro.clear()
            unregisterPhoneGyro()
            Log.i(TAG, "gyro forwarding OFF live — unregistered $n IMU listener(s)")
        }
    }

    private fun allocSlot(): Int? {
        for (i in used.indices) if (!used[i]) {
            used[i] = true
            return i
        }
        return null
    }

    private fun trigger(e: MotionEvent, primary: Int, fallback: Int): Float {
        val p = e.getAxisValue(primary)
        return if (p != 0f) p else e.getAxisValue(fallback)
    }

    private fun stateKey(slot: Int, idx: Int): Long = (slot.toLong() shl 8) or idx.toLong()
    private fun slotOfKey(key: Long): Int = (key shr 8).toInt()

    private fun buttonIndex(keyCode: Int): Int? = when (keyCode) {
        KeyEvent.KEYCODE_BUTTON_A -> 0
        KeyEvent.KEYCODE_BUTTON_B -> 1
        KeyEvent.KEYCODE_BUTTON_X -> 2
        KeyEvent.KEYCODE_BUTTON_Y -> 3
        KeyEvent.KEYCODE_BUTTON_L1 -> 4
        KeyEvent.KEYCODE_BUTTON_R1 -> 5
        KeyEvent.KEYCODE_BUTTON_SELECT -> 6
        KeyEvent.KEYCODE_BUTTON_START -> 7
        KeyEvent.KEYCODE_BUTTON_MODE -> 8
        KeyEvent.KEYCODE_BUTTON_THUMBL -> 9
        KeyEvent.KEYCODE_BUTTON_THUMBR -> 10
        else -> null
    }

    companion object {
        private const val TAG = "TriremePads"
        private const val MAX_PADS = 8
        private const val CENTER_DEADZONE = 0.06f
        private const val AXIS_EPSILON = 0.004f

        /**
         * Device-frame vector → the canonical ROTATION_90 device frame. Derivation: map
         * device axes to screen axes for the current rotation, then back to the device
         * axes they'd be at ROTATION_90 (z, out of the screen, never changes).
         */
        internal fun canonical(v: FloatArray, rotation: Int, out: FloatArray) {
            val x = v[0]
            val y = v[1]
            when (rotation) {
                Surface.ROTATION_0 -> { out[0] = y; out[1] = -x }
                Surface.ROTATION_180 -> { out[0] = -y; out[1] = x }
                Surface.ROTATION_270 -> { out[0] = -x; out[1] = -y }
                else -> { out[0] = x; out[1] = y }
            }
            out[2] = v[2]
        }

        // Canonical axis indices (core_proto::gamepad).
        private const val AX_LX = 0
        private const val AX_LY = 1
        private const val AX_RX = 2
        private const val AX_RY = 3
        private const val AX_LT = 4
        private const val AX_RT = 5
        private const val AX_HX = 6
        private const val AX_HY = 7

        /** True if the device is a real game controller (not a remote/keyboard).
         *  Requires analog sticks (SOURCE_JOYSTICK) AND a face button — TV
         *  pseudo-devices like "virtual-search" claim SOURCE_GAMEPAD but have
         *  neither, so they're excluded. */
        fun isGamepad(d: InputDevice): Boolean {
            val joystick = (d.sources and InputDevice.SOURCE_JOYSTICK) == InputDevice.SOURCE_JOYSTICK
            return joystick && d.hasKeys(KeyEvent.KEYCODE_BUTTON_A)[0]
        }

        /// Probe every currently-connected game controller for an IMU exposed via
        /// the standard [InputDevice.getSensorManager] API. Read-only and
        /// independent of the controller-handling master switch — used by the
        /// debug HUD to confirm a pad's gyro/accel are visible (the W9
        /// "getSensorList()" check). Returns empty below API 31 (no per-device
        /// sensor manager) or when nothing is connected.
        fun probeSensors(context: Context): List<PadSensorCaps> {
            val im = context.getSystemService(Context.INPUT_SERVICE) as? InputManager
                ?: return emptyList()
            // .toList() first: inputDeviceIds is an IntArray platform type, and the
            // primitive-array mapNotNull overload trips inference with the nullable
            // lambda — going via List<Int> resolves cleanly.
            return im.inputDeviceIds.toList().mapNotNull { id ->
                val d = InputDevice.getDevice(id) ?: return@mapNotNull null
                if (!isGamepad(d)) return@mapNotNull null
                var gyro = false
                var accel = false
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    val sm = d.sensorManager
                    gyro = sm?.getDefaultSensor(Sensor.TYPE_GYROSCOPE) != null
                    accel = sm?.getDefaultSensor(Sensor.TYPE_ACCELEROMETER) != null
                }
                PadSensorCaps(d.name ?: "?", d.vendorId, d.productId, gyro, accel)
            }
        }
    }
}
