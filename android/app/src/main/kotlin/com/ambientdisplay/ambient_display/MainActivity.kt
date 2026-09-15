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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
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
