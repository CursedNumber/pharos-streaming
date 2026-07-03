// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

/**
 * Plays the host's Opus audio stream. The host (Pharos) captures system audio,
 * encodes Opus at 48 kHz / stereo / 20 ms frames, and ships one packet per
 * unreliable QUIC datagram. The Rust core hands each packet to [feed] via the
 * FFI `AudioSink`; we decode with `MediaCodec("audio/opus")` and render through
 * an `AudioTrack` — no libopus in the app (mirrors the video path).
 *
 * **Low-latency, device-adaptive.** The AudioTrack buffer is sized to the
 * *device's own* native fast-mixer burst (`PROPERTY_OUTPUT_FRAMES_PER_BUFFER`),
 * queried at runtime — NOT a fixed ~100 ms, which silently disqualifies the
 * "fast track" low-latency path on every device. A creation ladder tries
 * low-latency→standard and tiny→min buffer in order, each attempt wrapped so a
 * refused config can never leave us silent (the failure mode that sank the
 * earlier USAGE_GAME attempt). So a phone/box that grants the fast path gets
 * ~10–20 ms output, while a TV whose HAL denies it (e.g. the Sony X90J, where
 * `getMinBufferSize` is already ~100 ms and FAST is denied) lands on its own
 * known-good floor — never broken, just bounded by that device's HAL (a USB DAC
 * is the only sub-HAL escape there).
 *
 * Threading mirrors [HevcDecoder]: packets are queued by the callback and
 * consumed on a dedicated decode thread. The queue drops the oldest packet when
 * full (audio is unreliable anyway — a tiny gap beats growing latency).
 */
class OpusAudioPlayer(context: Context) {
    private val sampleRate = 48000
    private val channels = 2
    private val bytesPerFrame = channels * 2 // stereo PCM16 = 4 bytes
    private val audioManager =
        context.applicationContext.getSystemService(Context.AUDIO_SERVICE) as AudioManager
    private val codec = MediaCodec.createDecoderByType("audio/opus")
    private val track: AudioTrack
    private val queue = LinkedBlockingQueue<ByteArray>(16)
    @Volatile private var running = true
    private var ptsUs = 0L
    private var played = 0L

    init {
        val fmt = MediaFormat.createAudioFormat("audio/opus", sampleRate, channels)
        // Opus codec-specific data the decoder needs to configure:
        //   csd-0 = OpusHead identification header
        //   csd-1 = codec delay  (pre-skip, in nanoseconds, int64 LE)
        //   csd-2 = seek pre-roll (in nanoseconds, int64 LE)
        val preSkip = 3840
        fmt.setByteBuffer("csd-0", opusHead(channels, preSkip, sampleRate))
        fmt.setByteBuffer("csd-1", nanosLe(preSkip.toLong() * 1_000_000_000L / sampleRate))
        fmt.setByteBuffer("csd-2", nanosLe(80_000_000L))
        codec.configure(fmt, null, null, 0)
        codec.start()

        // The device's native fast-mixer geometry. The fast/low-latency track only
        // opens when the buffer is a small multiple of the native frames-per-burst;
        // a ~100 ms buffer forces the slow deep-buffer mixer. We log the native rate
        // too — it's 48 k on virtually every device (matches our stream); a mismatch
        // would force resampling onto the slow path (a client-resample TODO).
        val fpb = nativeProperty(AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER, 256)
        val nativeRate = nativeProperty(AudioManager.PROPERTY_OUTPUT_SAMPLE_RATE, 48000)
        val minBuf = AudioTrack.getMinBufferSize(
            sampleRate,
            AudioFormat.CHANNEL_OUT_STEREO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        // Tiny = 2 frames (Moonlight's value): the framework rounds it UP to the
        // fast-track burst on a device that grants the fast path, and the ladder
        // falls back to the platform min buffer where it won't. Never the old 100 ms.
        val tinyBuf = bytesPerFrame * 2
        val largeBuf = maxOf(minBuf, tinyBuf)

        track = buildTrackLadder(tinyBuf, largeBuf)
        track.play()

        thread(name = "opus-decode") { loop() }

        // Diagnostics: report what the platform ACTUALLY granted, so logcat tells us
        // each device's verdict (fast path or HAL-floored) without guessing.
        val bufFrames = if (Build.VERSION.SDK_INT >= 23) track.bufferSizeInFrames else -1
        val perfMode = if (Build.VERSION.SDK_INT >= 26) track.performanceMode else -1
        val fastGranted = perfMode == AudioTrack.PERFORMANCE_MODE_LOW_LATENCY
        val halLatencyMs = runCatching {
            AudioTrack::class.java.getMethod("getLatency").invoke(track) as Int
        }.getOrDefault(-1)
        Log.i(
            TAG,
            "Opus audio started: nativeRate=$nativeRate fpb=$fpb minBuf=$minBuf " +
                "bufFrames=$bufFrames perfMode=$perfMode(1=lowlat, fast=$fastGranted) " +
                "halLatencyMs=$halLatencyMs " +
                "(~${if (bufFrames > 0) bufFrames * 1000 / sampleRate else -1}ms buffer)",
        )
        if (!fastGranted) {
            Log.w(
                TAG,
                "fast/low-latency track DENIED by this HAL — output latency is floored at " +
                    "the device's HAL latency (~halLatencyMs above); not app-reducible " +
                    "(a USB DAC/headset bypasses the HDMI/speaker HAL).",
            )
        }
        if (nativeRate != sampleRate) {
            Log.w(
                TAG,
                "device native rate $nativeRate != stream $sampleRate — resampling (slow " +
                    "path / added latency on this device); client-side resample is a TODO.",
            )
        }
    }

    private fun nativeProperty(key: String, fallback: Int): Int =
        audioManager.getProperty(key)?.toIntOrNull()?.takeIf { it > 0 } ?: fallback

    /**
     * Create the AudioTrack via a fallback ladder — low-latency→standard,
     * tiny→min buffer. Each attempt is wrapped so a `build()` exception or an
     * uninitialised track can NEVER leave us silent (the bug that sank the earlier
     * USAGE_GAME try, which was a single no-fallback call). The last rung is the
     * platform's known-good deep-buffer config.
     */
    private fun buildTrackLadder(tinyBuf: Int, largeBuf: Int): AudioTrack {
        data class Attempt(val lowLat: Boolean, val buf: Int, val tag: String)
        // 4 rungs (mirrors Moonlight): keep the small+standard rung — on some HALs
        // the fast track is denied but a small standard buffer still beats the min.
        val ladder = listOf(
            Attempt(true, tinyBuf, "lowlat+tiny"),
            Attempt(true, largeBuf, "lowlat+min"),
            Attempt(false, tinyBuf, "standard+tiny"),
            Attempt(false, largeBuf, "standard+min"),
        )
        for (a in ladder) {
            val t = runCatching { buildTrack(a.buf, a.lowLat) }.getOrNull()
            if (t != null && t.state == AudioTrack.STATE_INITIALIZED) {
                Log.i(TAG, "AudioTrack via ${a.tag} (reqBuf=${a.buf}B)")
                return t
            }
            runCatching { t?.release() }
            Log.w(TAG, "AudioTrack ${a.tag} unavailable; trying next rung")
        }
        error("AudioTrack: every creation attempt failed")
    }

    private fun buildTrack(bufBytes: Int, lowLat: Boolean): AudioTrack {
        if (Build.VERSION.SDK_INT < 26) {
            @Suppress("DEPRECATION")
            return AudioTrack(
                AudioManager.STREAM_MUSIC,
                sampleRate,
                AudioFormat.CHANNEL_OUT_STEREO,
                AudioFormat.ENCODING_PCM_16BIT,
                bufBytes,
                AudioTrack.MODE_STREAM,
            )
        }
        // USAGE_MEDIA + CONTENT_TYPE_MOVIE — the confirmed-good token on the X90J HAL.
        // (USAGE_GAME is what Moonlight ships, but it was implicated in the earlier
        // silent-track failure here; the ladder would make trialling it safe, but we
        // keep the conservative usage by default — never lose audio.) Volume-only, no
        // AudioEffect/loudness-enhancer anywhere — any effect disqualifies the fast mixer.
        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
            .build()
        val format = AudioFormat.Builder()
            .setSampleRate(sampleRate) // native 48 k — no SRC (resampling forces the slow path)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
            .build()
        val b = AudioTrack.Builder()
            .setAudioAttributes(attrs)
            .setAudioFormat(format)
            .setBufferSizeInBytes(bufBytes)
            .setTransferMode(AudioTrack.MODE_STREAM)
        if (lowLat) b.setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
        return b.build()
    }

    /** Queue one Opus packet; drops the oldest if the decoder fell behind. */
    fun feed(opus: ByteArray) {
        if (!queue.offer(opus)) {
            queue.poll()
            queue.offer(opus)
        }
    }

    fun stop() {
        running = false
    }

    private fun loop() {
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                val pkt = queue.poll(50, TimeUnit.MILLISECONDS)
                if (pkt != null) {
                    val inIdx = codec.dequeueInputBuffer(10_000)
                    if (inIdx >= 0) {
                        val buf = codec.getInputBuffer(inIdx)!!
                        buf.clear()
                        buf.put(pkt)
                        codec.queueInputBuffer(inIdx, 0, pkt.size, ptsUs, 0)
                        ptsUs += 20_000 // one 20 ms Opus frame
                    }
                }
                var outIdx = codec.dequeueOutputBuffer(info, 0)
                while (outIdx >= 0) {
                    val out = codec.getOutputBuffer(outIdx)
                    if (out != null && info.size > 0) {
                        val pcm = ByteArray(info.size)
                        out.position(info.offset)
                        out.get(pcm, 0, info.size)
                        track.write(pcm, 0, pcm.size)
                        if (played == 0L) Log.i(TAG, "first PCM out (${pcm.size} B) — audio flowing")
                        played += info.size.toLong()
                    }
                    codec.releaseOutputBuffer(outIdx, false)
                    outIdx = codec.dequeueOutputBuffer(info, 0)
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "opus decode loop error", e)
        } finally {
            runCatching { codec.stop() }
            runCatching { codec.release() }
            runCatching { track.stop() }
            runCatching { track.release() }
            Log.i(TAG, "Opus audio player stopped (played $played PCM bytes)")
        }
    }

    /** 19-byte OpusHead identification header (RFC 7845 §5.1). */
    private fun opusHead(ch: Int, preSkip: Int, rate: Int): ByteBuffer =
        ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN).apply {
            put("OpusHead".toByteArray(Charsets.US_ASCII)) // 8 bytes magic
            put(1)                      // version
            put(ch.toByte())            // channel count
            putShort(preSkip.toShort()) // pre-skip
            putInt(rate)                // input sample rate
            putShort(0)                 // output gain (Q7.8)
            put(0)                      // channel mapping family
            flip()
        }

    private fun nanosLe(ns: Long): ByteBuffer =
        ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).apply {
            putLong(ns)
            flip()
        }

    companion object {
        private const val TAG = "TriremeAudio"
    }
}
