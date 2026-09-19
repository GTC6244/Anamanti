package com.ambientdisplay.ambient_display

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity

/**
 * Holds a WifiManager MulticastLock for the lifetime of the activity so the Rust
 * engine's mDNS browse (`_wyoming._tcp`) can actually receive the orchestrator's
 * multicast responses. Without this lock Android's WiFi stack filters inbound
 * multicast and discovery silently times out (Plan.MD §3 / TODO §1).
 */
class MainActivity : FlutterActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

    companion object {
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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Cache the MicBridge class for the Rust engine's native thread (see
        // MicBridge.nativeCacheClass). Runs here, on an app thread, so the class is
        // resolved through the app class loader before the engine ever starts.
        MicBridge.nativeCacheClass()
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
