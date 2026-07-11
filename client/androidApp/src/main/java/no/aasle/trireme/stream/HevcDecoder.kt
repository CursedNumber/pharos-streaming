// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.stream

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Display
import android.view.Surface
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import kotlin.concurrent.thread

// Debug-only decoder trace: set with `adb shell setprop debug.trireme.decode_trace 1`
// (no reboot needed). When enabled, emits one Log.i per ~1s window with
//   decode_pts_lag_ms=<wall-clock − last presented pts>
//   queue_depth=<current AUs queued>/<capacity>
//   drops=<AUs evicted by FIFO overflow since the last window>
// Gated so production logs stay quiet by default.
private const val DEBUG_TRACE_PROP = "debug.trireme.decode_trace"
private fun decodeTraceEnabled(): Boolean = try {
    val cls = Class.forName("android.os.SystemProperties")
    val m = cls.getMethod("get", String::class.java, String::class.java)
    (m.invoke(null, DEBUG_TRACE_PROP, "0") as? String)?.let { it == "1" || it.equals("true", true) } ?: false
} catch (_: Throwable) {
    false
}

// Decode/pace constants for the live HEVC path. WIDTH/HEIGHT are the LAST-RESORT
// configure size (used only when neither a known stream geometry nor panel caps
// are available); the real configure size is computed per-instance — see the
// geometry fields in [HevcDecoder]. FPS seeds cadence; measured + re-hinted live.
private const val TAG = "Trireme"
private const val WIDTH = 1920
private const val HEIGHT = 1080
private const val FPS = 60
// Upper bound on how long stop() waits for the decode thread to release the codec.
// The loop's queue.poll is ≤50ms and we also interrupt it, so this is slack.
private const val JOIN_TIMEOUT_MS = 200L

/// Presentation pacing — a latency↔smoothness spectrum, plus a VRR mode that
/// lets a variable-refresh panel track the source cadence (low latency AND smooth).
///
/// Copied VERBATIM from MainActivity.kt (the original player) into the stream
/// package so StreamHub owns the decode/pace policy independently of the shell.
enum class PaceMode {
    LOW_LATENCY, // depth-1, drop-stale, low-latency decode, present now. Min latency.
    BALANCED,    // small jitter buffer, present now. Absorbs network jitter.
    SMOOTH,      // deeper buffer, present on a timed cadence (~V-sync). Smoothest.
    VRR,         // request the panel's best mode + hint source cadence. Low latency AND smooth.
}

/// What the *display* can actually do with refresh rate. Derived from
/// [Display.getSupportedModes] — the only source of truth (the dumpsys summary
/// and `setFrameRate` both ultimately reduce to this mode list). `rangeable` is
/// true only when there is more than one refresh rate to switch between, i.e.
/// when VRR/HFR is physically possible on this panel.
///
/// The shell still owns `configureRefresh()` (it needs a Window) and hands the
/// resulting caps to [StreamHub.onSurfaceAvailable], which forwards them here.
data class DisplayCaps(
    val refreshRates: List<Float>,
    val rangeable: Boolean,
    val chosenModeId: Int,
    val chosenRefresh: Float,
    /** Panel native resolution in px, landscape-normalised (width ≥ height); 0 if
     *  unknown. Reported to the host in the StreamMode handshake so it can render
     *  at panel-native size (Phase 3c). The dial caps this by the user's
     *  resolution target so a 4K panel streaming 1080p doesn't grow the envelope. */
    val width: Int = 0,
    val height: Int = 0,
) {
    companion object {
        val NONE = DisplayCaps(emptyList(), false, 0, 0f, 0, 0)
    }
}

/// One ~1s snapshot of live decode health, emitted via [HevcDecoder]'s `onStats`
/// callback for the on-screen debug HUD (debug variant only). Values come from the
/// same 1s window the cadence log uses. `width`/`height` are the decoder's actual
/// OUTPUT geometry (from the output MediaFormat / crop rect), i.e. the real stream
/// size — not the 1080p init constants — so the HUD reflects ground truth once a
/// non-1080p stream lands (a W7b precursor).
data class DecoderStats(
    val mime: String,
    val codecName: String,
    val hw: Boolean,
    val width: Int,
    val height: Int,
    val fps: Float,
    val queueDepth: Int,
    val capacity: Int,
    val drops: Long,
    val decodeLagMs: Long,
    val pace: PaceMode,
)

/// MediaCodec HEVC decoder rendering to a Surface, with a selectable pacing
/// policy. Access units are queued by the FrameSink and consumed on a thread.
///
/// Copied VERBATIM from MainActivity.kt — behaviour (VRR/KEY_LOW_LATENCY/cadence
/// measurement) is unchanged; only the package moved so StreamHub is the sole
/// owner of the single live decoder.
class HevcDecoder(
    private val surface: Surface,
    private val pace: PaceMode,
    private val caps: DisplayCaps,
    /// The decode MIME — `video/hevc` (default) or `video/av01`. The stream's
    /// codec is sniffed from the first access unit (see StreamHub); the host
    /// repeats the sequence header / parameter sets every IDR (≤1s) so a decoder
    /// bound to the right MIME resyncs quickly even mid-stream.
    val mime: String = "video/hevc",
    /// Optional 1s-cadence stats sink for the debug HUD. Null in release (StreamHub
    /// only wires it under BuildConfig.DEBUG), so production decode pays nothing but
    /// a per-second null check. Invoked on the decode thread; the receiver
    /// (a MutableStateFlow set) is thread-safe.
    private val onStats: ((DecoderStats) -> Unit)? = null,
    /// Always-on sink for the real video aspect ratio (width/height), fired once the
    /// decoder reports its output geometry (INFO_OUTPUT_FORMAT_CHANGED). The shell
    /// uses this to letterbox to the TRUE source aspect instead of a hardcoded 16:9
    /// — so a 20:9 (2712x1220) stream fills a 20:9 phone. Wired in release too.
    private val onAspect: ((Float) -> Unit)? = null,
    /// Last-known stream geometry (from a previous instance's
    /// INFO_OUTPUT_FORMAT_CHANGED) — the preferred configure size on a rebuild.
    /// 0 = unknown → configure at panel native (caps), else the 1080p fallback.
    private val initW: Int = 0,
    private val initH: Int = 0,
    /// Fired (on the decode thread) when the stream's coded size changes from what
    /// this codec instance was configured for — the MTK OMX decoder on the Bravia
    /// X90J renders such frames at the wrong stride (tiled/quadrant picture over
    /// uninitialized garbage) instead of adapting, even though it ADVERTISES
    /// FEATURE_AdaptivePlayback. The loop stops itself first; the receiver
    /// (StreamHub) rebuilds a decoder with the reported size as the new configure
    /// hint and requests an IDR, so the switch lands in well under a second.
    private val onGeometry: ((Int, Int) -> Unit)? = null,
) {
    // Codec selection: enumerate MediaCodecList for the first HEVC decoder that
    // is hardware-accelerated and NOT a known software fallback (OMX.google.*,
    // c2.android.*). createDecoderByType("video/hevc") will silently hand back
    // a software decoder on devices whose HW HEVC entry isn't first in the
    // OMX/Codec2 priority list — that path can't sustain 1080p60 and is the
    // SILENT failure mode this guards against. If no hardware match exists
    // (e.g. an older TV / emulator), we fall back to createDecoderByType and
    // log loudly so the symptom is visible in logcat (one grep on `hw=`).
    private val codec: MediaCodec = pickDecoder(mime)
    // The chosen codec name + HW accel bool — captured once for the init log
    // below. Held as fields so the cadence/restart logs can reference them
    // without re-querying MediaCodecInfo.
    private val codecName: String = codec.name
    private val codecIsHw: Boolean = isHardwareAccelerated(codec)
    // Input AU (access unit) queue. Sized per-pace: LOW_LATENCY/VRR were bumped
    // 2→4 to absorb couch-streaming jitter without dropping AUs; this adds at
    // most ~33ms worst-case latency (2 extra slots × 16.67ms@60fps) which is
    // acceptable for movies/games. Twitch FPS would want a separate preset.
    private val capacity = when (pace) {
        PaceMode.LOW_LATENCY, PaceMode.VRR, PaceMode.BALANCED -> 4
        PaceMode.SMOOTH -> 8
    }
    private val queue = LinkedBlockingQueue<ByteArray>(capacity)
    @Volatile private var running = true

    // Trace counters (read by the debug-trace path). `dropsSinceLastTrace` is
    // window-delta (getAndSet 0) for the trace line; `totalDrops` is the always-on
    // cumulative count surfaced in the HUD. `lastPresentedPtsUs`/`firstFeedNs` are
    // now set unconditionally (cheap) so the HUD's decode-lag works without the
    // trace prop.
    private val dropsSinceLastTrace = AtomicLong(0)
    private val totalDrops = AtomicLong(0)
    private val lastPresentedPtsUs = AtomicLong(0)
    private val trace = decodeTraceEnabled()
    private var traceWindowStart = 0L
    // Wall-clock anchor — set on the first feed() — against which
    // last_presented_pts is compared to derive decode_pts_lag_ms.
    @Volatile private var firstFeedNs = 0L
    // ---- Configure geometry ----
    // The codec is configured at the best size we know — the last-seen stream
    // geometry (rebuild), else panel native (caps, landscape-normalised), else the
    // 1080p fallback. The old hardcoded 1920x1080 configure relied on the codec's
    // in-place port reconfigure when the real SPS arrived; the X90J's MtkOmxVdec
    // sometimes botches that (wrong-stride tiled frames over green uninitialized
    // rows), which is exactly the corruption this replaces.
    private val cfgW = if (initW > 0 && initH > 0) initW else if (caps.width > 0) caps.width else WIDTH
    private val cfgH = if (initW > 0 && initH > 0) initH else if (caps.height > 0) caps.height else HEIGHT
    // Diagnostic ONLY — logged at start, never trusted. In-place adaptive
    // switching (FEATURE_AdaptivePlayback + KEY_MAX_WIDTH/HEIGHT) was the first
    // design: the X90J's MtkOmxVdec CLAIMS the feature but rendered stale-stride
    // quadrant/garbage frames on a live 2160→1440→1080 down-switch (2026-07-11).
    // So ANY coded-size change now rebuilds the decoder instead (see
    // captureOutputFormat); resync is <1s via the requested IDR.
    private val adaptiveOk: Boolean = runCatching {
        codec.codecInfo.getCapabilitiesForType(mime)
            .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_AdaptivePlayback)
    }.getOrDefault(false)

    // Actual decoder OUTPUT geometry, captured on INFO_OUTPUT_FORMAT_CHANGED.
    // Seeded to the configure size until the first format change arrives.
    @Volatile private var outW = cfgW
    @Volatile private var outH = cfgH

    private val frameDurNs = 1_000_000_000L / FPS
    private var baseNs = 0L // SMOOTH: cadence anchor
    private var presented = 0L

    // Rolling cadence measurement. The host doesn't advertise its fps yet, so we
    // measure the real present rate from arriving frames and (VRR) re-hint the
    // panel to track it — works for a fixed 120 stream and a variable one alike.
    private var measWindowStart = 0L
    private var measCount = 0
    private var lastHintedFps = FPS.toFloat()

    // The decode thread; held so [stop] can join it and guarantee the codec is
    // released before the next decoder is bound to the (shared) Surface.
    private var decodeThread: Thread? = null

    // Exit-cause diagnostic: set on the path that ends this decoder instance so
    // the loop's finally{} can emit a single 'decoder restart cause=...' line.
    // The caller (StreamHub) drives the *next* restart; this log surfaces *why
    // this instance stopped*, which is the same information from the decoder's
    // POV. Values: clean_stop | interrupted | mediacodec_err | error | geometry
    @Volatile private var exitCause: String = "clean_stop"
    @Volatile private var exitErr: String? = null

    init {
        // Configure at exactly the expected stream size — no KEY_MAX_WIDTH/HEIGHT
        // adaptive pre-sizing. Every coded-size change rebuilds the decoder (the
        // only path the MTK OMX decoder gets right), so the hints would only make
        // the codec pre-allocate panel-native buffers it never adapts into.
        val fmt = MediaFormat.createVideoFormat(mime, cfgW, cfgH)
        if ((pace == PaceMode.LOW_LATENCY || pace == PaceMode.VRR) && Build.VERSION.SDK_INT >= 30) {
            fmt.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        codec.configure(fmt, surface, null, 0)
        codec.start()
        if (pace == PaceMode.VRR) hintSourceCadence()
        decodeThread = thread(name = "hevc-decode") { loop() }
        // ALWAYS log codec name + HW accel — this is the one diagnostic that
        // permanently answers "is this TV using the hardware HEVC decoder?".
        // Not gated by trace mode; one info line per decoder lifecycle, paired
        // with the existing "decoder restart cause=" line on teardown.
        Log.i(
            TAG,
            "video decoder started (mime=$mime, pace=$pace, queue=$capacity, codec=$codecName, " +
                "hw=$codecIsHw, cfg=${cfgW}x${cfgH}, adaptive=$adaptiveOk[unused])"
        )
    }

    companion object {
        /// Find a hardware decoder for `mime` by enumerating MediaCodecList.
        /// Filters out OMX.google.* and c2.android.* (both Android's software
        /// fallbacks) and prefers `isHardwareAccelerated()` on API 29+. Falls back
        /// to `createDecoderByType(mime)` if nothing matches, with a loud warning
        /// so the symptom is visible in logcat. Works for `video/hevc` and
        /// `video/av01` (the X90J and most 2020+ Android TVs have HW AV1 decode).
        private fun pickDecoder(mime: String): MediaCodec {
            val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            val match: MediaCodecInfo? = list.codecInfos.firstOrNull { info ->
                if (info.isEncoder) return@firstOrNull false
                val name = info.name
                if (name.startsWith("OMX.google.") || name.startsWith("c2.android.")) {
                    return@firstOrNull false
                }
                val supports = info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
                if (!supports) return@firstOrNull false
                // API 29+ exposes the explicit HW bit; on older API we trust
                // the non-software-prefix filter above (every shipping hardware
                // video decoder pre-API-29 used a vendor prefix).
                if (Build.VERSION.SDK_INT >= 29) info.isHardwareAccelerated else true
            }
            return if (match != null) {
                try {
                    MediaCodec.createByCodecName(match.name)
                } catch (t: Throwable) {
                    Log.w(TAG, "createByCodecName(${match.name}) failed, falling back to createDecoderByType($mime)", t)
                    MediaCodec.createDecoderByType(mime)
                }
            } else {
                Log.w(TAG, "NO HARDWARE DECODER FOUND for $mime — falling back to createDecoderByType (expect software-decode stutter or failure)")
                MediaCodec.createDecoderByType(mime)
            }
        }

        /// Best-effort HW-accel probe for a constructed MediaCodec instance.
        /// On API 29+ uses MediaCodec.getCodecInfo().isHardwareAccelerated;
        /// on older API falls back to the non-software-name heuristic.
        private fun isHardwareAccelerated(codec: MediaCodec): Boolean = try {
            if (Build.VERSION.SDK_INT >= 29) {
                codec.codecInfo.isHardwareAccelerated
            } else {
                val n = codec.name
                !(n.startsWith("OMX.google.") || n.startsWith("c2.android."))
            }
        } catch (_: Throwable) {
            false
        }
    }

    /// Push a content-cadence hint to the surface so a capable panel tracks it.
    /// API 31+ uses CHANGE_FRAME_RATE_ALWAYS to permit a (non-seamless) switch;
    /// on a single-mode panel the call is a documented no-op.
    private fun setSurfaceFrameRate(fps: Float) {
        if (Build.VERSION.SDK_INT < 30) return
        runCatching {
            if (Build.VERSION.SDK_INT >= 31) {
                surface.setFrameRate(
                    fps,
                    Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                    Surface.CHANGE_FRAME_RATE_ALWAYS,
                )
            } else {
                surface.setFrameRate(fps, Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
            }
        }
    }

    private fun hintSourceCadence() {
        setSurfaceFrameRate(FPS.toFloat())
        if (caps.rangeable) {
            Log.i(TAG, "VRR: hinted ${FPS}fps source cadence → panel mode ${caps.chosenModeId} (${caps.chosenRefresh}Hz); will track measured rate")
        } else {
            Log.i(TAG, "VRR: hinted ${FPS}fps, but panel is fixed-refresh — behaves as LOW_LATENCY here")
        }
    }

    fun feed(au: ByteArray) {
        if (firstFeedNs == 0L) firstFeedNs = System.nanoTime()
        // Drop the oldest unit if we fall behind.
        if (!queue.offer(au)) {
            queue.poll()
            queue.offer(au)
            totalDrops.incrementAndGet()
            if (trace) dropsSinceLastTrace.incrementAndGet()
        }
    }

    /// Synchronous stop: signal the loop to exit, then JOIN it so the MediaCodec
    /// is fully released before we return. StreamHub binds a new decoder to the
    /// SAME output Surface on a host switch / surface change, so the old codec
    /// MUST be gone first — otherwise two codecs briefly present to one Surface
    /// (torn frames / releaseOutputBuffer IllegalStateException). The loop wakes
    /// from its ≤50ms queue.poll within a frame, so the bounded join is cheap.
    fun stop() {
        running = false
        val t = decodeThread
        decodeThread = null
        if (t != null && t !== Thread.currentThread()) {
            // Wake the loop promptly (it may be parked on queue.poll) and wait for
            // the finally{} that releases the codec. Interrupt as a backstop.
            t.interrupt()
            runCatching { t.join(JOIN_TIMEOUT_MS) }
            if (t.isAlive) Log.w(TAG, "decode thread did not exit within ${JOIN_TIMEOUT_MS}ms")
        }
    }

    private fun loop() {
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                val au = queue.poll(50, TimeUnit.MILLISECONDS)
                if (au != null) {
                    val inIdx = codec.dequeueInputBuffer(10_000)
                    if (inIdx >= 0) {
                        val buf = codec.getInputBuffer(inIdx)!!
                        buf.clear()
                        buf.put(au)
                        // Stamp the input PTS with wall-clock-since-first-feed (µs) so the
                        // decode-lag metric measures REAL feed→present latency. The old
                        // synthetic frameDur-spaced PTS assumed 60fps and went nonsense
                        // (negative) at other rates. Presentation is unaffected: VRR/
                        // LOW_LATENCY present immediately and SMOOTH paces off its own clock.
                        val ptsUs = (System.nanoTime() - firstFeedNs).coerceAtLeast(0) / 1000
                        codec.queueInputBuffer(inIdx, 0, au.size, ptsUs, 0)
                    }
                }
                var outIdx = codec.dequeueOutputBuffer(info, 0)
                while (outIdx >= 0) {
                    // Record the PTS of the frame about to be presented so the
                    // debug trace / HUD can compute wall-clock − last_presented_pts.
                    lastPresentedPtsUs.set(info.presentationTimeUs)
                    present(outIdx)
                    outIdx = codec.dequeueOutputBuffer(info, 0)
                }
                // The decoder reports the real output geometry here (and on a
                // resolution change mid-stream) — capture it for the HUD.
                if (outIdx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) captureOutputFormat()
                if (trace) emitTraceIfDue()
            }
        } catch (e: InterruptedException) {
            // Expected: stop() interrupts a thread parked on queue.poll as a
            // backstop to running=false. Not an error — fall through to release.
            Log.i(TAG, "decode loop interrupted on stop")
            if (exitCause == "clean_stop") exitCause = "interrupted"
        } catch (e: MediaCodec.CodecException) {
            // MediaCodec-specific failure: surface lost, codec internal, etc.
            // diagnosticInfo carries the underlying error code on supported APIs.
            exitCause = "mediacodec_err"
            exitErr = "${e.errorCode}/${e.diagnosticInfo ?: e.message}"
            Log.e(TAG, "decode loop MediaCodec error code=${e.errorCode} diag=${e.diagnosticInfo}", e)
            // Corrupt-stream codec failure: ask the host for a fresh IDR so the
            // decoder StreamHub rebuilds resyncs in ~1 RTT. Best-effort.
            runCatching { StreamHub.requestKeyframe() }
        } catch (e: Exception) {
            exitCause = "error"
            exitErr = "${e::class.java.simpleName}: ${e.message}"
            Log.e(TAG, "decode loop error", e)
        } finally {
            runCatching { codec.stop() }
            runCatching { codec.release() }
            // One key=value line per decoder instance ending — pairs with the
            // 'HEVC decoder started' line so the lifecycle is greppable. The
            // StreamHub call site decides whether to spin a fresh decoder up;
            // this surfaces *how this one died* (clean vs codec error vs etc).
            val depth = queue.size
            Log.i(
                TAG,
                "decoder restart cause=$exitCause err=$exitErr queue=$depth/$capacity"
            )
        }
    }

    private fun present(outIdx: Int) {
        if (pace == PaceMode.SMOOTH) {
            // Even cadence, offset by a 2-frame jitter buffer; the surface flip
            // is aligned to vsync at the requested presentation time.
            val now = System.nanoTime()
            if (baseNs == 0L) baseNs = now + 2 * frameDurNs
            codec.releaseOutputBuffer(outIdx, baseNs + presented * frameDurNs)
            presented++
        } else {
            // LOW_LATENCY / BALANCED / VRR: present immediately.
            codec.releaseOutputBuffer(outIdx, true)
        }
        measureCadence()
    }

    /// Debug-only: once-per-second trace of decode-side health. Enabled via
    /// `setprop debug.trireme.decode_trace 1`. Emits key=value so it's greppable
    /// and easy to scrape from logcat alongside the existing cadence line.
    private fun emitTraceIfDue() {
        val now = System.nanoTime()
        if (traceWindowStart == 0L) { traceWindowStart = now; return }
        if (now - traceWindowStart < 1_000_000_000L) return
        val lagMs = if (firstFeedNs == 0L) -1L else {
            val wallUs = (now - firstFeedNs) / 1000
            (wallUs - lastPresentedPtsUs.get()) / 1000
        }
        val drops = dropsSinceLastTrace.getAndSet(0)
        Log.i(
            TAG,
            "decode_trace decode_pts_lag_ms=$lagMs queue_depth=${queue.size}/$capacity drops=$drops"
        )
        traceWindowStart = now
    }

    /// Read the decoder's output MediaFormat (called on INFO_OUTPUT_FORMAT_CHANGED)
    /// to capture the true presented geometry. Prefers the crop rectangle (the
    /// visible size) over the buffer width/height when present.
    ///
    /// Also the geometry-mismatch tripwire: if the stream's coded size moved away
    /// from what this codec was configured for, stop BEFORE the wrong-geometry
    /// frames are dequeued — the format change arrives ahead of its buffers, so
    /// exiting here means no corrupt frame is ever presented — and hand the new
    /// size to StreamHub for a rebuild. ANY change restarts: in-place adaptive
    /// switching is deliberately not attempted even when the codec advertises
    /// FEATURE_AdaptivePlayback, because the X90J's MtkOmxVdec advertises it and
    /// then renders stale-stride garbage on a live down-switch (2026-07-11).
    private fun captureOutputFormat() {
        runCatching {
            val of = codec.outputFormat
            var w = if (of.containsKey(MediaFormat.KEY_WIDTH)) of.getInteger(MediaFormat.KEY_WIDTH) else outW
            var h = if (of.containsKey(MediaFormat.KEY_HEIGHT)) of.getInteger(MediaFormat.KEY_HEIGHT) else outH
            if (of.containsKey("crop-left") && of.containsKey("crop-right") &&
                of.containsKey("crop-top") && of.containsKey("crop-bottom")
            ) {
                val cw = of.getInteger("crop-right") - of.getInteger("crop-left") + 1
                val ch = of.getInteger("crop-bottom") - of.getInteger("crop-top") + 1
                if (cw > 0 && ch > 0) { w = cw; h = ch }
            }
            outW = w
            outH = h
            if (w > 0 && h > 0) onAspect?.invoke(w.toFloat() / h.toFloat())
            Log.i(TAG, "output format: ${w}x${h} (mime=$mime, cfg=${cfgW}x${cfgH})")
            // Compare CROP size (the true video size) — buffer dims carry codec row
            // alignment (e.g. 1088 for 1080) and would false-trip a rebuild loop.
            val changed = w > 0 && h > 0 && (w != cfgW || h != cfgH)
            if (changed) {
                exitCause = "geometry"
                exitErr = "${w}x${h} vs cfg ${cfgW}x${cfgH} (adaptive=$adaptiveOk, not trusted)"
                running = false
                onGeometry?.invoke(w, h)
            }
        }
    }

    /// Debug-only: emit a [DecoderStats] snapshot once per cadence window. No-op
    /// (one null check) when no sink is wired (release). Runs on the decode thread;
    /// the sink writes a thread-safe MutableStateFlow.
    private fun emitStats(fps: Float) {
        val cb = onStats ?: return
        val lagMs = if (firstFeedNs == 0L) -1L else {
            val wallUs = (System.nanoTime() - firstFeedNs) / 1000
            (wallUs - lastPresentedPtsUs.get()) / 1000
        }
        cb(
            DecoderStats(
                mime = mime,
                codecName = codecName,
                hw = codecIsHw,
                width = outW,
                height = outH,
                fps = fps,
                queueDepth = queue.size,
                capacity = capacity,
                drops = totalDrops.get(),
                decodeLagMs = lagMs,
                pace = pace,
            )
        )
    }

    /// Measure the actual present rate over ~1s windows; log it, and (VRR, on a
    /// capable panel) re-hint the surface so it follows the true source cadence.
    private fun measureCadence() {
        val now = System.nanoTime()
        if (measWindowStart == 0L) {
            measWindowStart = now
            measCount = 0
            return
        }
        measCount++
        val elapsed = now - measWindowStart
        if (elapsed >= 1_000_000_000L) {
            val fps = measCount * 1e9f / elapsed
            Log.i(TAG, "measured present cadence: ${"%.1f".format(fps)} fps")
            if (pace == PaceMode.VRR && caps.rangeable && kotlin.math.abs(fps - lastHintedFps) > 3f) {
                setSurfaceFrameRate(fps)
                lastHintedFps = fps
                Log.i(TAG, "VRR: re-hinted panel to measured ${"%.1f".format(fps)} fps")
            }
            emitStats(fps)
            measWindowStart = now
            measCount = 0
        }
    }
}
