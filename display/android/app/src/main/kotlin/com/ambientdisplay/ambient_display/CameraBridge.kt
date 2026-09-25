package com.ambientdisplay.ambient_display

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.ImageFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.ImageReader
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Range
import android.util.Size

/**
 * Kotlin Camera2 capture shim feeding front-camera luma frames to the Rust engine
 * over JNI — the visual twin of [MicBridge].
 *
 * The Echo Show's front camera is used as a cheap proximity sensor: this shim opens
 * it at a tiny resolution and a few frames per second, extracts the packed Y (luma)
 * plane from each frame, and pushes it down through [nativePushLuma] into the Rust
 * presence detector (`rust/src/camera/bridge.rs` → `camera/presence.rs`), which
 * measures frame-to-frame motion and emits a `Presence` event when someone
 * approaches or the room goes quiet. **No image analysis happens here** — the shim
 * only gets the pixels out of the [ImageReader]; the sensing lives in Rust, per the
 * repo's Rust/Flutter boundary (`agents.md`).
 *
 * Lifecycle is driven from the Rust engine thread via JNI up-calls
 * ([startCamera]/[stopCamera]). Everything runs on a dedicated background thread so
 * the camera callbacks never touch the UI or audio threads.
 */
object CameraBridge {
    private const val TAG = "CameraBridge"

    private lateinit var appContext: Context

    @Volatile private var running = false
    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    private var manager: CameraManager? = null
    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var reader: ImageReader? = null

    /** Push one packed luma (Y-plane) frame ([width]×[height] bytes) into the Rust ring. */
    @JvmStatic external fun nativePushLuma(data: ByteArray, width: Int, height: Int)

    /**
     * Hand the Rust side this class (resolved via the app class loader) so the
     * engine's native thread can up-call [startCamera]/[stopCamera] without a
     * `FindClass` (which fails on native threads). See [MicBridge.nativeCacheClass].
     */
    @JvmStatic external fun nativeCacheClass()

    /**
     * Store the app context and cache the class for the native side. Called once
     * from `MainActivity.onCreate` (an app thread), before the engine starts.
     */
    @JvmStatic
    fun attach(context: Context) {
        appContext = context.applicationContext
        nativeCacheClass()
    }

    /**
     * Open the front camera and begin delivering luma frames. Called from the Rust
     * engine thread. Camera2 open is asynchronous, so this returns as soon as the
     * open is *kicked off* (0), or -1 if it can't even start (no permission, no front
     * camera, no context). Frames then flow to [nativePushLuma] on the bg thread.
     *
     * @param targetWidth  desired luma width  (snapped to the nearest supported size)
     * @param targetHeight desired luma height (snapped to the nearest supported size)
     * @param fps          target capture frame rate
     */
    @JvmStatic
    fun startCamera(targetWidth: Int, targetHeight: Int, fps: Int): Int {
        if (running) stopCamera()
        if (!::appContext.isInitialized) {
            Log.e(TAG, "startCamera before attach(): no context")
            return -1
        }
        if (appContext.checkSelfPermission(Manifest.permission.CAMERA)
            != PackageManager.PERMISSION_GRANTED
        ) {
            Log.e(TAG, "CAMERA permission not granted; proximity disabled")
            return -1
        }

        val mgr = appContext.getSystemService(Context.CAMERA_SERVICE) as? CameraManager
        if (mgr == null) {
            Log.e(TAG, "no CameraManager")
            return -1
        }
        manager = mgr

        val frontId = frontCameraId(mgr)
        if (frontId == null) {
            Log.e(TAG, "no front-facing camera")
            return -1
        }

        val size = chooseSize(mgr, frontId, targetWidth, targetHeight)
        Log.i(TAG, "opening front camera $frontId at ${size.width}x${size.height} @${fps}fps")

        val ht = HandlerThread("camera-bridge").also { it.start() }
        val hdl = Handler(ht.looper)
        thread = ht
        handler = hdl

        val ir = ImageReader.newInstance(
            size.width, size.height, ImageFormat.YUV_420_888, 2,
        )
        ir.setOnImageAvailableListener({ r -> onFrame(r) }, hdl)
        reader = ir

        running = true
        return try {
            mgr.openCamera(frontId, object : CameraDevice.StateCallback() {
                override fun onOpened(cam: CameraDevice) {
                    device = cam
                    startSession(cam, ir, fps, hdl)
                }

                override fun onDisconnected(cam: CameraDevice) {
                    Log.w(TAG, "camera disconnected")
                    cam.close()
                    device = null
                }

                override fun onError(cam: CameraDevice, error: Int) {
                    Log.e(TAG, "camera open error $error")
                    cam.close()
                    device = null
                }
            }, hdl)
            0
        } catch (e: SecurityException) {
            Log.e(TAG, "openCamera SecurityException (permission?)", e)
            stopCamera()
            -1
        } catch (e: Exception) {
            Log.e(TAG, "openCamera failed", e)
            stopCamera()
            -1
        }
    }

    /** Stop capture and release the camera + reader + bg thread. Idempotent. */
    @JvmStatic
    fun stopCamera() {
        running = false
        try {
            session?.close()
        } catch (e: Exception) {
            Log.w(TAG, "session close threw", e)
        }
        session = null
        try {
            device?.close()
        } catch (e: Exception) {
            Log.w(TAG, "device close threw", e)
        }
        device = null
        reader?.close()
        reader = null
        thread?.quitSafely()
        thread = null
        handler = null
        manager = null
        Log.i(TAG, "stopped")
    }

    private fun startSession(cam: CameraDevice, ir: ImageReader, fps: Int, hdl: Handler) {
        try {
            @Suppress("DEPRECATION")
            cam.createCaptureSession(
                listOf(ir.surface),
                object : CameraCaptureSession.StateCallback() {
                    override fun onConfigured(s: CameraCaptureSession) {
                        if (!running) {
                            s.close()
                            return
                        }
                        session = s
                        try {
                            val req = cam.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW)
                            req.addTarget(ir.surface)
                            req.set(
                                CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE,
                                Range(fps, fps),
                            )
                            s.setRepeatingRequest(req.build(), null, hdl)
                            Log.i(TAG, "capture session streaming")
                        } catch (e: Exception) {
                            Log.e(TAG, "setRepeatingRequest failed", e)
                        }
                    }

                    override fun onConfigureFailed(s: CameraCaptureSession) {
                        Log.e(TAG, "capture session config failed")
                    }
                },
                hdl,
            )
        } catch (e: Exception) {
            Log.e(TAG, "createCaptureSession failed", e)
        }
    }

    /** Pull the packed Y plane out of the newest frame and hand it to Rust. */
    private fun onFrame(r: ImageReader) {
        val image = try {
            r.acquireLatestImage()
        } catch (e: Exception) {
            Log.w(TAG, "acquireLatestImage threw", e)
            null
        } ?: return
        try {
            val w = image.width
            val h = image.height
            val plane = image.planes[0]
            val buffer = plane.buffer
            val rowStride = plane.rowStride
            val pixelStride = plane.pixelStride
            val out = ByteArray(w * h)
            if (pixelStride == 1) {
                var pos = 0
                for (row in 0 until h) {
                    buffer.position(row * rowStride)
                    val n = minOf(w, buffer.remaining())
                    buffer.get(out, pos, n)
                    pos += w
                }
            } else {
                // Rare: interleaved Y plane. Copy each row then decimate by pixelStride.
                val rowBuf = ByteArray(rowStride)
                var pos = 0
                for (row in 0 until h) {
                    buffer.position(row * rowStride)
                    val n = minOf(rowStride, buffer.remaining())
                    buffer.get(rowBuf, 0, n)
                    var c = 0
                    var i = 0
                    while (c < w && i < n) {
                        out[pos + c] = rowBuf[i]
                        c++
                        i += pixelStride
                    }
                    pos += w
                }
            }
            if (running) nativePushLuma(out, w, h)
        } catch (e: Exception) {
            Log.w(TAG, "frame extract failed", e)
        } finally {
            image.close()
        }
    }

    private fun frontCameraId(mgr: CameraManager): String? {
        return try {
            mgr.cameraIdList.firstOrNull { id ->
                mgr.getCameraCharacteristics(id)
                    .get(CameraCharacteristics.LENS_FACING) ==
                    CameraCharacteristics.LENS_FACING_FRONT
            }
        } catch (e: Exception) {
            Log.e(TAG, "cameraIdList failed", e)
            null
        }
    }

    /** Nearest supported YUV_420_888 size (by area) to the requested geometry. */
    private fun chooseSize(mgr: CameraManager, id: String, w: Int, h: Int): Size {
        val fallback = Size(w, h)
        return try {
            val map = mgr.getCameraCharacteristics(id)
                .get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
                ?: return fallback
            val sizes = map.getOutputSizes(ImageFormat.YUV_420_888) ?: return fallback
            val target = (w * h).toLong()
            sizes.minByOrNull { s ->
                kotlin.math.abs(s.width.toLong() * s.height.toLong() - target)
            } ?: fallback
        } catch (e: Exception) {
            Log.w(TAG, "chooseSize failed; using $fallback", e)
            fallback
        }
    }
}
