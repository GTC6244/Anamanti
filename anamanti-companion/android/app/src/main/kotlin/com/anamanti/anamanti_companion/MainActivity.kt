package com.anamanti.anamanti_companion

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity

/**
 * Holds a WifiManager MulticastLock while the activity lives so the Rust engine's
 * mDNS browse (`_wyoming._tcp`, crate `mdns-sd`) actually receives the Core's
 * multicast responses — without it Android's Wi-Fi stack filters inbound multicast
 * and discovery silently finds nothing. Same approach as Anamanti Display. This is
 * not a networking data path: all sockets stay in Rust.
 */
class MainActivity : FlutterActivity() {
    private var multicastLock: WifiManager.MulticastLock? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("anamanti-companion-mdns").apply {
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
