package app.audiobridge

import android.Manifest
import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat

/**
 * Keeps the native client connected in the background.
 *
 * Starts with FGS type connectedDevice only (allowed from BOOT_COMPLETED and background restarts). The microphone
 * type is added only from a while-in-use context: the visible activity ([ACTION_FOREGROUND]) or the user tapping the
 * notification action ([ACTION_ENABLE_MIC]). Native mic capture is allowed only while that type is held.
 */
class BridgeService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var prefs: Prefs
    private lateinit var notifications: NotificationManager

    /** Last JSON array handed to [NativeBridge.setPeers]. */
    private var appliedPeers: String? = null
    private var fgsType = NOT_FOREGROUND
    private var shownNotificationKey: String? = null

    private var wifiLock: WifiManager.WifiLock? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    private val statusListener = object : StatusListener {
        override fun onStatus(json: String) {
            val status = try {
                BridgeStatus.parse(json)
            } catch (e: Exception) {
                Log.w(TAG, "bad status json: $json", e)
                return
            }
            StatusHub.status.value = status
            main.post { onStatusChanged(status) }
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        prefs = Prefs.get(this)
        notifications = getSystemService(NotificationManager::class.java)
        createChannel()
        Native.ensureInit(this)
        NativeBridge.setListener(statusListener)
        registerNetworkCallback()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_ENABLE_MIC -> prefs.setMicEnabled(true)
            ACTION_DISABLE_MIC -> prefs.setMicEnabled(false)
        }
        val promote = intent?.action == ACTION_FOREGROUND || intent?.action == ACTION_ENABLE_MIC
        if (!updateForeground(promote)) {
            stopSelf()
            return START_NOT_STICKY
        }

        val pcs = prefs.pcs.value
        if (pcs.isEmpty()) {
            stopSelf()
            return START_NOT_STICKY
        }
        val uris = PairedPc.urisJson(pcs)
        if (uris != appliedPeers) {
            NativeBridge.setPeers(uris)
            appliedPeers = uris
        }
        return START_STICKY
    }

    override fun onDestroy() {
        NativeBridge.setListener(null)
        NativeBridge.setMicAllowed(false)
        NativeBridge.setPeers("[]")
        appliedPeers = null
        networkCallback?.let { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) }
        networkCallback = null
        main.removeCallbacksAndMessages(null)
        setStreamingLocks(false)
        StatusHub.status.value = BridgeStatus.IDLE
        StatusHub.micForeground.value = false
        super.onDestroy()
    }

    private fun hasRecordAudio() =
        ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    /**
     * (Re)enters the foreground with the right types. [promote] means we are in a while-in-use context and may add
     * the microphone type. Returns false if the service could not become foreground at all.
     */
    private fun updateForeground(promote: Boolean): Boolean {
        val wantMic = prefs.micEnabled.value && hasRecordAudio()
        val holdsMic = fgsType != NOT_FOREGROUND && (fgsType and MIC_TYPE) == MIC_TYPE
        val desired = if (wantMic && (promote || holdsMic)) BASE_TYPE or MIC_TYPE else BASE_TYPE

        if (desired != fgsType) {
            // Native capture must stop before the microphone FGS type goes away.
            if (!isMicType(desired)) revokeMic()
            fgsType = try {
                startForegroundTyped(desired)
                desired
            } catch (e: RuntimeException) {
                // SecurityException / ForegroundServiceStartNotAllowedException: mic not allowed from this context.
                if (desired == BASE_TYPE) {
                    Log.e(TAG, "cannot enter foreground", e)
                    return false
                }
                Log.w(TAG, "microphone FGS type refused; continuing without mic", e)
                try {
                    startForegroundTyped(BASE_TYPE)
                } catch (e2: RuntimeException) {
                    Log.e(TAG, "cannot enter foreground", e2)
                    return false
                }
                BASE_TYPE
            }
        } else {
            refreshNotification(force = true)
        }
        applyMicAllowed()
        return true
    }

    private fun startForegroundTyped(type: Int) {
        val notification = buildNotification(StatusHub.status.value, micReady = isMicType(type))
        ServiceCompat.startForeground(this, NOTIFICATION_ID, notification, type)
        shownNotificationKey = notificationKey(StatusHub.status.value, isMicType(type))
    }

    private fun isMicType(type: Int): Boolean =
        type != NOT_FOREGROUND && (type and MIC_TYPE) == MIC_TYPE && prefs.micEnabled.value && hasRecordAudio()

    private fun applyMicAllowed() {
        val allowed = isMicType(fgsType)
        StatusHub.micForeground.value = allowed
        NativeBridge.setMicAllowed(allowed)
    }

    private fun revokeMic() {
        StatusHub.micForeground.value = false
        NativeBridge.setMicAllowed(false)
    }

    private fun onStatusChanged(status: BridgeStatus) {
        if (fgsType == NOT_FOREGROUND) return
        setStreamingLocks(status.isStreaming)
        refreshNotification(force = false)
    }

    // region locks

    private fun setStreamingLocks(streaming: Boolean) {
        if (streaming) acquireStreamingLocks() else releaseStreamingLocks()
    }

    @SuppressLint("WakelockTimeout") // Held exactly while audio streams; released as soon as both directions idle.
    private fun acquireStreamingLocks() {
        val wifi = wifiLock ?: createWifiLock().also { wifiLock = it }
        if (!wifi.isHeld) wifi.acquire()
        val wake = wakeLock ?: getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "AudioBridge:stream")
            .apply { setReferenceCounted(false) }
            .also { wakeLock = it }
        if (!wake.isHeld) wake.acquire()
    }

    private fun releaseStreamingLocks() {
        wifiLock?.takeIf { it.isHeld }?.release()
        wakeLock?.takeIf { it.isHeld }?.release()
    }

    private fun createWifiLock(): WifiManager.WifiLock {
        val wifi = applicationContext.getSystemService(WifiManager::class.java)
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            @Suppress("DEPRECATION")
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        return wifi.createWifiLock(mode, "AudioBridge:stream").apply { setReferenceCounted(false) }
    }

    // endregion

    // region network

    private fun registerNetworkCallback() {
        val cm = getSystemService(ConnectivityManager::class.java)
        val callback = object : ConnectivityManager.NetworkCallback() {
            private var current: Network? = null
            private var addresses: List<String> = emptyList()
            private var initial = true

            override fun onAvailable(network: Network) {
                if (network == current) return
                current = network
                addresses = emptyList()
                if (initial) {
                    initial = false
                } else {
                    NativeBridge.networkChanged()
                }
            }

            override fun onLost(network: Network) {
                if (network != current) return
                current = null
                addresses = emptyList()
                initial = false
                NativeBridge.networkChanged()
            }

            override fun onLinkPropertiesChanged(network: Network, linkProperties: LinkProperties) {
                if (network != current) return
                val now = linkProperties.linkAddresses.map { it.toString() }.sorted()
                val changed = addresses.isNotEmpty() && now != addresses
                addresses = now
                if (changed) NativeBridge.networkChanged()
            }
        }
        cm.registerDefaultNetworkCallback(callback)
        networkCallback = callback
    }

    // endregion

    // region notification

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.channel_status),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = getString(R.string.channel_status_desc)
            setShowBadge(false)
        }
        notifications.createNotificationChannel(channel)
    }

    private fun notificationKey(status: BridgeStatus, micReady: Boolean): String =
        "${notificationText(status)}|$micReady|${status.micCapturing}|${prefs.micEnabled.value}|${hasRecordAudio()}"

    private fun refreshNotification(force: Boolean) {
        val status = StatusHub.status.value
        val micReady = isMicType(fgsType)
        val key = notificationKey(status, micReady)
        if (!force && key == shownNotificationKey) return
        shownNotificationKey = key
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        notifications.notify(NOTIFICATION_ID, buildNotification(status, micReady))
    }

    /** "Подключено: LYTEN, LAPTOP" / "Подключение…" / "Нет связи"; a single connected PC also shows its path. */
    private fun notificationText(status: BridgeStatus): String {
        val connected = status.connectedPeers
        val onlyPath = connected.singleOrNull()?.path
        return when {
            onlyPath != null -> getString(R.string.notif_connected_one_path, connected[0].name, getString(pathLabel(onlyPath)))
            connected.isNotEmpty() -> getString(R.string.notif_connected, connected.joinToString(", ") { it.name })
            status.peers.any { it.reachability == Reachability.Connecting } -> getString(R.string.notif_connecting)
            else -> getString(R.string.notif_offline)
        }
    }

    private fun buildNotification(status: BridgeStatus, micReady: Boolean): Notification {
        val text = notificationText(status)
        val openApp = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val builder = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(openApp)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setShowWhen(false)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)

        when {
            micReady && status.micCapturing -> builder.setSubText(getString(R.string.notif_mic_live))
            micReady -> builder.setSubText(getString(R.string.notif_mic_ready))
            else -> Unit
        }

        if (micReady) {
            builder.addAction(
                R.drawable.ic_mic,
                getString(R.string.notif_action_mic_off),
                servicePendingIntent(ACTION_DISABLE_MIC, 2),
            )
        } else {
            val action = if (hasRecordAudio()) {
                servicePendingIntent(ACTION_ENABLE_MIC, 1)
            } else {
                // Permission has to be requested from the activity.
                PendingIntent.getActivity(
                    this,
                    3,
                    Intent(this, MainActivity::class.java)
                        .setAction(MainActivity.ACTION_REQUEST_MIC)
                        .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
                    PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                )
            }
            builder.addAction(R.drawable.ic_mic, getString(R.string.notif_action_mic_on), action)
        }
        return builder.build()
    }

    private fun servicePendingIntent(action: String, requestCode: Int): PendingIntent =
        PendingIntent.getService(
            this,
            requestCode,
            Intent(this, BridgeService::class.java).setAction(action),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

    // endregion

    companion object {
        private const val TAG = "AudioBridge"
        private const val CHANNEL_ID = "status"
        private const val NOTIFICATION_ID = 1
        private const val NOT_FOREGROUND = -1

        /** Sent by the visible activity: a while-in-use context, so the microphone type may be added. */
        const val ACTION_FOREGROUND = "app.audiobridge.action.FOREGROUND"
        const val ACTION_ENABLE_MIC = "app.audiobridge.action.ENABLE_MIC"
        const val ACTION_DISABLE_MIC = "app.audiobridge.action.DISABLE_MIC"

        private val BASE_TYPE =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE else 0

        /** Before API 30 there is no microphone type and no while-in-use restriction, so 0 means "always allowed". */
        private val MIC_TYPE =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE else 0

        /** Starts (or pokes) the service. [fromForeground] = called from the visible activity. */
        fun start(context: Context, fromForeground: Boolean) {
            val intent = Intent(context, BridgeService::class.java)
            if (fromForeground) intent.action = ACTION_FOREGROUND
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, BridgeService::class.java))
        }
    }
}

fun pathLabel(path: PathKind): Int = when (path) {
    PathKind.Lan -> R.string.path_lan
    PathKind.Tailscale -> R.string.path_tailscale
    PathKind.Direct -> R.string.path_direct
    PathKind.Relay -> R.string.path_relay
}
