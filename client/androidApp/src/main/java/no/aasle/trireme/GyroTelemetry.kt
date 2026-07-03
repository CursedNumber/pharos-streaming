package no.aasle.trireme

import uniffi.core_ffi.GamepadInput

/**
 * Process-global sink for live gyro telemetry streamed back from the host.
 *
 * The controller's IMU is read on the host (over USB/IP or Bluetooth — Android
 * never exposes it for these pads), so the host streams the raw per-axis rate
 * back as datagrams. The calibration overlay's `GyroSink` writes here; the
 * visualizer reads it each frame. Plain volatiles + a frame-clock read keep it
 * lock-free across the QUIC (writer) and UI (reader) threads.
 *
 * Also owns the overlay's short-lived host connection so it can be torn down
 * reliably — from the dialog's `onDispose` AND the activity's `onPause`, since
 * `DisposableEffect` inside a Compose `Dialog` doesn't always fire on dismiss.
 */
object GyroTelemetry {
    @Volatile var rx = 0f
    @Volatile var ry = 0f
    @Volatile var rz = 0f
    @Volatile private var lastNs = 0L
    @Volatile private var session: GamepadInput? = null

    /** Called from the QUIC datagram thread for each sample (deg/s, raw axes). */
    fun publish(gx: Float, gy: Float, gz: Float) {
        rx = gx; ry = gy; rz = gz
        lastNs = System.nanoTime()
    }

    /** True if a sample arrived recently (host connected + IMU reporting). */
    fun isLive(nowNs: Long = System.nanoTime()): Boolean =
        lastNs != 0L && nowNs - lastNs < FRESH_NS

    /** Register the overlay's live session so it can be torn down from anywhere. */
    @Synchronized
    fun startSession(handle: GamepadInput?) {
        session?.stop() // never leave an older session dangling
        session = handle
    }

    /** Tear down the live session + reset staleness. Idempotent. */
    @Synchronized
    fun stopSession() {
        session?.stop()
        session = null
        lastNs = 0L; rx = 0f; ry = 0f; rz = 0f
    }

    private const val FRESH_NS = 500_000_000L // 0.5 s
}
