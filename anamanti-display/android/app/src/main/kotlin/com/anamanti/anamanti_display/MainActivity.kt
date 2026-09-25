package com.anamanti.anamanti_display

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

/**
 * Holds a WifiManager MulticastLock for the lifetime of the activity so the Rust
 * engine's mDNS browse (`_wyoming._tcp`) can actually receive the orchestrator's
 * multicast responses. Without this lock Android's WiFi stack filters inbound
 * multicast and discovery silently times out (Plan.MD §3 / TODO §1).
 *
 * Also hosts the [BRIGHTNESS_CHANNEL] MethodChannel: the Rust camera proximity
 * sensor decides presence and the Dart UI calls through here to actually set the
 * window backlight (brighten on approach, dim when the room is quiet — Plan.MD §5).
 * Brightness is presentation, so the actuation stays on the Flutter/Android side.
 */
class MainActivity : FlutterActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

    companion object {
        private const val BRIGHTNESS_CHANNEL = "anamanti_display/brightness"

        init {
            // Load the Rust engine through the *Java* path so the JVM runs its
            // `JNI_OnLoad`, which initializes `ndk_context` (JavaVM + Application
            // context). cpal's AAudio output backend needs that to query the Java
            // AudioManager; without it `start_playback` panics and TTS is silent.
            // flutter_rust_bridge later opens the same library via dlopen, which
            // reuses this already-loaded, already-initialized instance.
            System.loadLibrary("rust_lib_ambient_display")
        }
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            BRIGHTNESS_CHANNEL,
        ).setMethodCallHandler { call, result ->
            when (call.method) {
                "setBrightness" -> {
                    // [0.0, 1.0] sets an absolute window brightness; a negative value
                    // reverts to the system/user default (BRIGHTNESS_OVERRIDE_NONE).
                    val level = (call.arguments as? Double)?.toFloat() ?: -1f
                    setWindowBrightness(level)
                    result.success(null)
                }
                else -> result.notImplemented()
            }
        }
    }

    /** Set (or clear, when negative) the window's screen brightness. UI thread. */
    private fun setWindowBrightness(level: Float) {
        val lp = window.attributes
        lp.screenBrightness = if (level < 0f) {
            android.view.WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE
        } else {
            level.coerceIn(0f, 1f)
        }
        window.attributes = lp
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Cache the MicBridge class for the Rust engine's native thread (see
        // MicBridge.nativeCacheClass). Runs here, on an app thread, so the class is
        // resolved through the app class loader before the engine ever starts.
        MicBridge.nativeCacheClass()
        // Give the camera bridge the app context + cache its class (same rationale).
        CameraBridge.attach(applicationContext)
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("ambient-mdns").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    override fun onDestroy() {
        multicastLock?.let { if (it.isHeld) it.release() }
        multicastLock = null
        super.onDestroy()
    }
}
