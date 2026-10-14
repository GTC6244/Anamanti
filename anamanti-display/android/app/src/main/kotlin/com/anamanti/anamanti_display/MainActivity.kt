package com.anamanti.anamanti_display

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel
import java.io.File

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

    // The in-flight permission request's Dart reply, held between requestPermissions()
    // and onRequestPermissionsResult (only one request runs at a time).
    private var pendingPermissionResult: MethodChannel.Result? = null

    companion object {
        private const val BRIGHTNESS_CHANNEL = "anamanti_display/brightness"

        // In-app updater channel (plans/UpdaterPlan.md). Dart drives
        // the check/download in Rust, then calls here for the native install steps
        // (`PackageInstaller`, unknown-sources redirect) which have no Rust/Dart
        // equivalent. The channel is held statically so InstallReceiver — created by
        // the system, not us — can post the final install status back to Dart.
        private const val UPDATER_CHANNEL = "anamanti_display/updater"

        // Kiosk escape hatch: open Android settings / switch Home app from the
        // in-app Settings → System page (see SettingsScreen._systemPage).
        private const val MAINTENANCE_CHANNEL = "anamanti_display/maintenance"

        // Runtime-permission inspector/requester: the in-app Settings → Permissions
        // page reads grant state and fires the Android permission dialog through here
        // (see SettingsScreen._permissionsPage). Needed because this kiosk app has no
        // other moment to prompt — grants were previously only possible over adb.
        private const val PERMISSIONS_CHANNEL = "anamanti_display/permissions"

        // Arbitrary request code tying our requestPermissions() call to its callback.
        private const val PERMISSION_REQUEST_CODE = 0xA11

        @Volatile
        private var updaterChannel: MethodChannel? = null
        private val mainHandler = Handler(Looper.getMainLooper())

        /**
         * Relay a terminal install outcome to Dart (`installStatus` method). Called
         * from [InstallReceiver] on a binder thread; hops to the main thread because
         * MethodChannel must be invoked there. No-op if the engine is gone.
         */
        fun reportInstallStatus(success: Boolean, message: String) {
            val channel = updaterChannel ?: return
            mainHandler.post {
                channel.invokeMethod(
                    "installStatus",
                    mapOf("success" to success, "message" to message),
                )
            }
        }

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

        val updater = MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            UPDATER_CHANNEL,
        )
        updater.setMethodCallHandler { call, result ->
            when (call.method) {
                // Whether this build ships the self-updater (false on the fdroid
                // flavor). Dart hides all updater UI when false.
                "isSelfUpdateEnabled" -> result.success(BuildConfig.ENABLE_SELF_UPDATE)
                // The running app's Android versionCode — Dart compares it to latest.json.
                "getVersionCode" -> result.success(packageVersionCode())
                // Has the user granted "install unknown apps" to us?
                "canInstallPackages" ->
                    result.success(packageManager.canRequestPackageInstalls())
                // Open the per-app unknown-sources settings screen.
                "openInstallSettings" -> {
                    openUnknownSourcesSettings()
                    result.success(null)
                }
                // Install a downloaded+verified APK via a PackageInstaller session.
                "installApk" -> {
                    val path = call.argument<String>("path")
                    if (path.isNullOrEmpty()) {
                        result.error("no_path", "installApk requires a 'path' argument", null)
                    } else {
                        installApk(path, result)
                    }
                }
                else -> result.notImplemented()
            }
        }
        updaterChannel = updater

        // Kiosk escape hatch (plans/UpdaterPlan.md watchdog). This app is the device
        // Home/launcher so the OS relaunches it after a crash; these let an operator
        // reach the stock UI from inside the app (Settings → System page).
        MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            MAINTENANCE_CHANNEL,
        ).setMethodCallHandler { call, result ->
            when (call.method) {
                // Open the Android "Home app" picker so the stock LineageOS launcher
                // can be set back as default for maintenance.
                "openHomeSettings" -> {
                    startSettings(Settings.ACTION_HOME_SETTINGS)
                    result.success(null)
                }
                // Open the full system Settings app.
                "openSystemSettings" -> {
                    startSettings(Settings.ACTION_SETTINGS)
                    result.success(null)
                }
                else -> result.notImplemented()
            }
        }

        // Permissions inspector/requester for the Settings → Permissions page.
        MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            PERMISSIONS_CHANNEL,
        ).setMethodCallHandler { call, result ->
            when (call.method) {
                // Map every permission this app declares → whether it is granted.
                "status" -> result.success(permissionStatus())
                // Fire the Android runtime dialog for the given permission names; the
                // refreshed status is returned from onRequestPermissionsResult.
                "request" -> {
                    val names = call.argument<List<String>>("permissions") ?: emptyList()
                    requestAppPermissions(names, result)
                }
                // Open this app's "App info" page (fallback for a permanently-denied
                // permission, where the runtime dialog no longer appears).
                "openAppSettings" -> {
                    openAppDetailsSettings()
                    result.success(null)
                }
                else -> result.notImplemented()
            }
        }
    }

    /** Map every permission this app declares to whether it is currently granted. */
    private fun permissionStatus(): Map<String, Boolean> {
        val declared = try {
            packageManager.getPackageInfo(
                packageName,
                android.content.pm.PackageManager.GET_PERMISSIONS,
            ).requestedPermissions
        } catch (_: Exception) {
            null
        } ?: emptyArray()
        return declared.associateWith { name ->
            checkSelfPermission(name) ==
                android.content.pm.PackageManager.PERMISSION_GRANTED
        }
    }

    /**
     * Fire the Android runtime permission dialog for [names]. The outcome arrives in
     * [onRequestPermissionsResult], which replies to [result] with the refreshed
     * status map. Only one request runs at a time — a second while one is pending is
     * rejected rather than silently dropping the earlier reply.
     */
    private fun requestAppPermissions(names: List<String>, result: MethodChannel.Result) {
        if (names.isEmpty()) {
            result.success(permissionStatus())
            return
        }
        if (pendingPermissionResult != null) {
            result.error("busy", "A permission request is already in flight", null)
            return
        }
        pendingPermissionResult = result
        requestPermissions(names.toTypedArray(), PERMISSION_REQUEST_CODE)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == PERMISSION_REQUEST_CODE) {
            val reply = pendingPermissionResult
            pendingPermissionResult = null
            // Reply with the full freshly-read status so Dart refreshes every row.
            reply?.success(permissionStatus())
        }
    }

    /** Open this app's system "App info" screen, where permissions can be toggled. */
    private fun openAppDetailsSettings() {
        try {
            startActivity(
                Intent(
                    Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.parse("package:$packageName"),
                ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } catch (_: Exception) {
            startSettings(Settings.ACTION_SETTINGS)
        }
    }

    /** Launch a system Settings activity by action, tolerating a missing screen. */
    private fun startSettings(action: String) {
        try {
            startActivity(Intent(action).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        } catch (e: Exception) {
            // Some ROMs lack a given settings screen; fall back to the top-level
            // Settings app so the escape hatch always lands somewhere usable.
            if (action != Settings.ACTION_SETTINGS) {
                try {
                    startActivity(
                        Intent(Settings.ACTION_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                    )
                } catch (_: Exception) {
                    // Nothing more we can do; the adb fallback is documented in the UI.
                }
            }
        }
    }

    /** The running app's versionCode (`longVersionCode` on API 28+). */
    private fun packageVersionCode(): Long {
        val info = packageManager.getPackageInfo(packageName, 0)
        return if (Build.VERSION.SDK_INT >= 28) {
            info.longVersionCode
        } else {
            @Suppress("DEPRECATION")
            info.versionCode.toLong()
        }
    }

    /** Open Settings so the user can allow this app to install unknown apps. */
    private fun openUnknownSourcesSettings() {
        val intent = Intent(
            Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
            Uri.parse("package:$packageName"),
        ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        startActivity(intent)
    }

    /**
     * Stream the cached APK into a [PackageInstaller] session and commit it. The
     * session result (user-action prompt, then success/failure) arrives at
     * [InstallReceiver] via the committed [PendingIntent]; the final outcome is
     * relayed to Dart through [reportInstallStatus]. The channel result here only
     * reports that the session was committed (or failed to start).
     */
    private fun installApk(path: String, result: MethodChannel.Result) {
        val apk = File(path)
        if (!apk.exists()) {
            result.error("missing_apk", "APK not found at $path", null)
            return
        }
        Thread {
            try {
                val installer = packageManager.packageInstaller
                val params = PackageInstaller.SessionParams(
                    PackageInstaller.SessionParams.MODE_FULL_INSTALL,
                )
                val sessionId = installer.createSession(params)
                installer.openSession(sessionId).use { session ->
                    apk.inputStream().use { input ->
                        session.openWrite("app", 0, apk.length()).use { out ->
                            input.copyTo(out)
                            session.fsync(out)
                        }
                    }
                    val statusIntent = Intent(this, InstallReceiver::class.java).apply {
                        action = InstallReceiver.ACTION_INSTALL_STATUS
                        putExtra(InstallReceiver.EXTRA_APK_PATH, path)
                    }
                    // The session result broadcast must be MUTABLE so the OS can fill
                    // in EXTRA_STATUS / EXTRA_INTENT (the flag constant is API 31+).
                    val mutable =
                        if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
                    val pending = PendingIntent.getBroadcast(
                        this,
                        sessionId,
                        statusIntent,
                        mutable or PendingIntent.FLAG_UPDATE_CURRENT,
                    )
                    session.commit(pending.intentSender)
                }
                runOnUiThread { result.success(null) }
            } catch (e: Exception) {
                runOnUiThread {
                    result.error("install_failed", e.message ?: e.toString(), null)
                }
            }
        }.start()
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
        updaterChannel = null
        super.onDestroy()
    }
}
