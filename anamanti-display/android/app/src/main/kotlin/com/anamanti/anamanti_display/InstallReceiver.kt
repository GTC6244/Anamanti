package com.anamanti.anamanti_display

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import java.io.File

/**
 * Receives [PackageInstaller] session results for the in-app updater
 * (plans/UpdaterPlan.md). Two cases:
 *
 *  - [PackageInstaller.STATUS_PENDING_USER_ACTION]: launch the system "install this
 *    app?" confirmation activity carried in [Intent.EXTRA_INTENT]. On Android 11
 *    this is the normal, expected first result — the OS always asks the user.
 *  - a terminal status (success / failure): delete the cached APK and relay the
 *    outcome to Dart via [MainActivity.reportInstallStatus].
 *
 * Registered only in the `selfUpdate` flavor manifest
 * (`android/app/src/selfUpdate/AndroidManifest.xml`), so the `fdroid` flavor ships
 * without the installer. Targeted by an explicit `Intent`, so it is not exported.
 */
class InstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val status =
            intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        val apkPath = intent.getStringExtra(EXTRA_APK_PATH)

        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = extraIntent(intent)
                if (confirm != null) {
                    confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                    context.startActivity(confirm)
                } else {
                    apkPath?.let { File(it).delete() }
                    MainActivity.reportInstallStatus(false, "missing install confirmation intent")
                }
            }

            PackageInstaller.STATUS_SUCCESS -> {
                apkPath?.let { File(it).delete() }
                MainActivity.reportInstallStatus(true, "")
            }

            else -> {
                apkPath?.let { File(it).delete() }
                val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
                    ?: "install failed (status $status)"
                MainActivity.reportInstallStatus(false, message)
            }
        }
    }

    @Suppress("DEPRECATION")
    private fun extraIntent(intent: Intent): Intent? =
        if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
        } else {
            intent.getParcelableExtra(Intent.EXTRA_INTENT)
        }

    companion object {
        /** Absolute path of the cached APK, so we can delete it once install ends. */
        const val EXTRA_APK_PATH = "com.anamanti.anamanti_display.APK_PATH"

        /** Explicit action for the session-result broadcast (self-targeted). */
        const val ACTION_INSTALL_STATUS = "com.anamanti.anamanti_display.INSTALL_STATUS"
    }
}
