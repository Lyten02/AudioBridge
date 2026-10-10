package app.audiobridge

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/** Restarts the bridge after boot or an app update, if the phone is paired and the user has not switched it off. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED,
            ACTION_QUICKBOOT_POWERON,
            Intent.ACTION_MY_PACKAGE_REPLACED,
            -> Unit
            else -> return
        }
        if (!Prefs.get(context).shouldRun()) return
        try {
            BridgeService.start(context, fromForeground = false)
        } catch (e: RuntimeException) {
            // ForegroundServiceStartNotAllowedException (Android 12+) if the OS refuses this start.
            Log.w("AudioBridge", "could not start bridge service on ${intent.action}", e)
        }
    }

    private companion object {
        const val ACTION_QUICKBOOT_POWERON = "android.intent.action.QUICKBOOT_POWERON"
    }
}
