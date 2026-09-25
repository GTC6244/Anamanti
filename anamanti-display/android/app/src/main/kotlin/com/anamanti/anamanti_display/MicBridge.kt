package com.anamanti.anamanti_display

import android.media.AudioFormat
import android.media.AudioRecord
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AutomaticGainControl
import android.media.audiofx.NoiseSuppressor
import android.util.Log
import kotlin.concurrent.thread

/**
 * Kotlin `AudioRecord` capture layer feeding PCM to the Rust engine over JNI.
 *
 * `cpal` 0.18's AAudio backend opens the default mic source with `inputPreset = 0`
 * (raw) and exposes no way to pick an input preset or attach platform effects. This
 * shim opens the mic through `AudioRecord` so we can (a) select the HAL's far-field-
 * tuned `VOICE_RECOGNITION` source (array beamforming) and (b) attach the platform
 * `AcousticEchoCanceler` / `AutomaticGainControl` / `NoiseSuppressor` to the record
 * session when the device offers them.
 *
 * Lifecycle is driven from the Rust engine thread via JNI up-calls
 * ([startRecording]/[stopRecording]); the reader thread pushes 10 ms `i16` chunks
 * back down through [nativePush] into the engine's lock-free capture ring
 * (`rust/src/audio/mic_bridge.rs`). Requesting 16 kHz mono directly means the device
 * reports the true rate — no `cpal` 48 k/24 k rate-calibration workaround needed.
 */
object MicBridge {
    private const val TAG = "MicBridge"

    @Volatile private var running = false
    private var record: AudioRecord? = null
    private var reader: Thread? = null
    private var aec: AcousticEchoCanceler? = null
    private var agc: AutomaticGainControl? = null
    private var ns: NoiseSuppressor? = null

    /** Push one mono `i16` PCM chunk (first [len] samples of [data]) into the Rust ring. */
    @JvmStatic external fun nativePush(data: ShortArray, len: Int)

    /**
     * Hand the Rust side this class (resolved via the app class loader) so the
     * engine's native thread can up-call [startRecording]/[stopRecording] without a
     * `FindClass` (which fails on native threads). Must be called once from an app
     * thread — see `MainActivity.onCreate` — before the engine starts.
     */
    @JvmStatic external fun nativeCacheClass()

    /**
     * Open + start capture. Called from the Rust engine thread. Returns the actual
     * sample rate on success, or -1 on failure (engine then reports capture error).
     *
     * @param source an `android.media.MediaRecorder.AudioSource` value (6 = VOICE_RECOGNITION).
     */
    @JvmStatic
    fun startRecording(
        sampleRate: Int,
        source: Int,
        enableAec: Boolean,
        enableAgc: Boolean,
        enableNs: Boolean,
    ): Int {
        if (running) stopRecording()

        val minBuf = AudioRecord.getMinBufferSize(
            sampleRate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
        )
        if (minBuf <= 0) {
            Log.e(TAG, "getMinBufferSize failed: $minBuf")
            return -1
        }
        // ~200 ms of internal buffering, floored at the HAL minimum. Mono: the device
        // only exposes a single effective mic — a stereo probe confirmed both channels
        // are level-identical with zero inter-mic delay (no usable array/beamforming).
        val bufBytes = maxOf(minBuf, sampleRate / 5 * 2)

        val ar = try {
            AudioRecord(
                source, sampleRate, AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT, bufBytes,
            )
        } catch (e: Exception) {
            Log.e(TAG, "AudioRecord construction failed", e)
            return -1
        }
        if (ar.state != AudioRecord.STATE_INITIALIZED) {
            Log.e(TAG, "AudioRecord not initialized (state=${ar.state})")
            ar.release()
            return -1
        }

        val sid = ar.audioSessionId
        aec = attachEffect("AEC", enableAec, AcousticEchoCanceler.isAvailable()) {
            AcousticEchoCanceler.create(sid)?.also { it.enabled = true }
        }
        agc = attachEffect("AGC", enableAgc, AutomaticGainControl.isAvailable()) {
            AutomaticGainControl.create(sid)?.also { it.enabled = true }
        }
        ns = attachEffect("NS", enableNs, NoiseSuppressor.isAvailable()) {
            NoiseSuppressor.create(sid)?.also { it.enabled = true }
        }

        record = ar
        val actualRate = ar.sampleRate
        ar.startRecording()
        if (ar.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
            Log.e(TAG, "startRecording did not enter RECORDING state")
            stopRecording()
            return -1
        }
        running = true

        val chunk = maxOf(160, actualRate / 100) // 10 ms
        reader = thread(name = "mic-bridge-reader", isDaemon = true) {
            val buf = ShortArray(chunk)
            while (running) {
                val n = ar.read(buf, 0, buf.size)
                when {
                    n > 0 -> nativePush(buf, n)
                    n < 0 -> { Log.e(TAG, "AudioRecord.read error $n"); break }
                    // n == 0: no data yet, loop.
                }
            }
        }
        Log.i(TAG, "started: source=$source rate=$actualRate (requested $sampleRate) session=$sid")
        return actualRate
    }

    /** Stop capture and release the record session + effects. Idempotent. */
    @JvmStatic
    fun stopRecording() {
        running = false
        reader?.join(500)
        reader = null
        aec?.release(); aec = null
        agc?.release(); agc = null
        ns?.release(); ns = null
        record?.let {
            try {
                if (it.recordingState == AudioRecord.RECORDSTATE_RECORDING) it.stop()
            } catch (e: Exception) {
                Log.w(TAG, "AudioRecord.stop threw", e)
            }
            it.release()
        }
        record = null
        Log.i(TAG, "stopped")
    }

    private inline fun <T> attachEffect(
        name: String,
        requested: Boolean,
        available: Boolean,
        create: () -> T?,
    ): T? {
        if (!requested) return null
        if (!available) {
            Log.i(TAG, "$name requested but not available on this device")
            return null
        }
        return try {
            val fx = create()
            Log.i(TAG, "$name attached=${fx != null}")
            fx
        } catch (e: Exception) {
            Log.w(TAG, "$name attach failed", e)
            null
        }
    }
}
