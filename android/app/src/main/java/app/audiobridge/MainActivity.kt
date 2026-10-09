package app.audiobridge

import android.Manifest
import android.annotation.SuppressLint
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.StringRes
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.core.content.ContextCompat
import androidx.core.net.toUri
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.lifecycleScope
import app.audiobridge.ui.AudioBridgeTheme
import app.audiobridge.ui.MainScreen
import app.audiobridge.ui.MicUiState
import app.audiobridge.ui.SetupItem
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    private lateinit var prefs: Prefs
    private val snackbar = SnackbarHostState()

    /** Bumped on resume and after permission results: system-owned settings may have changed. */
    private val systemStateVersion = mutableIntStateOf(0)

    private val requestMic = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        systemStateVersion.intValue++
        if (granted) {
            prefs.setMicEnabled(true)
            startBridge()
        } else {
            prefs.setMicEnabled(false)
            startBridge()
            val permanentlyDenied = !shouldShowRequestPermissionRationale(Manifest.permission.RECORD_AUDIO)
            showMessage(R.string.mic_permission_denied, actionLabel = R.string.open_settings.takeIf { permanentlyDenied }) {
                openAppDetails()
            }
        }
    }

    private val requestNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            systemStateVersion.intValue++
            if (granted) {
                startBridge()
            } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
                !shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS)
            ) {
                openNotificationSettings()
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        prefs = Prefs.get(this)

        setContent {
            AudioBridgeTheme {
                val pcs by prefs.pcs.collectAsStateWithLifecycle()
                val status by StatusHub.status.collectAsStateWithLifecycle()
                val micForeground by StatusHub.micForeground.collectAsStateWithLifecycle()
                val micEnabled by prefs.micEnabled.collectAsStateWithLifecycle()
                val autostartDone by prefs.autostartDone.collectAsStateWithLifecycle()
                val headset by prefs.headset.collectAsStateWithLifecycle()
                val headsetTarget by StatusHub.mediaTargetId.collectAsStateWithLifecycle()
                val lastMediaKey by StatusHub.lastMediaKey.collectAsStateWithLifecycle()
                val version = systemStateVersion.intValue

                val micPermission = remember(version) { hasPermission(Manifest.permission.RECORD_AUDIO) }
                val setupItems = remember(version, autostartDone) { setupItems(autostartDone) }

                MainScreen(
                    pcs = pcs,
                    status = status,
                    mic = MicUiState(
                        userEnabled = micEnabled,
                        permissionGranted = micPermission,
                        foregroundGranted = micForeground,
                    ),
                    setupItems = setupItems,
                    snackbarHostState = snackbar,
                    onAddPc = ::scan,
                    onRemovePc = ::removePc,
                    onMutePc = ::setPcMuted,
                    onMuteAll = ::setAllMuted,
                    onMicToggle = ::setMic,
                    onPcControl = ::controlPc,
                    headset = headset,
                    headsetTargetId = headsetTarget,
                    lastMediaKey = lastMediaKey,
                    // The service observes the settings and applies them right away.
                    onHeadsetChange = prefs::setHeadset,
                )
            }
        }

        if (savedInstanceState == null) handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleIntent(intent)
    }

    override fun onResume() {
        super.onResume()
        systemStateVersion.intValue++
        // Visible activity = while-in-use context: lets the service (re)acquire the microphone FGS type.
        startBridge()
    }

    private fun handleIntent(intent: Intent?) {
        intent ?: return
        val data = intent.data
        when {
            intent.action == Intent.ACTION_VIEW && data?.scheme == "audiobridge" && data.host == "pair" ->
                pair(data.toString())
            intent.action == ACTION_REQUEST_MIC -> setMic(true)
        }
    }

    // region pairing

    private fun scan() {
        val options = GmsBarcodeScannerOptions.Builder()
            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
            .enableAutoZoom()
            .build()
        GmsBarcodeScanning.getClient(this, options)
            .startScan()
            .addOnSuccessListener { barcode ->
                val raw = barcode.rawValue
                if (raw == null) showMessage(R.string.scan_invalid) else pair(raw)
            }
            .addOnFailureListener { e ->
                Log.w(TAG, "code scanner failed", e)
                showMessage(R.string.scan_failed)
            }
    }

    private fun pair(uri: String) {
        val trimmed = uri.trim()
        val pc = PairedPc.fromParseResult(trimmed, NativeBridge.parsePairing(trimmed))
        if (pc == null) {
            showMessage(R.string.scan_invalid)
            return
        }
        val known = prefs.pcs.value.any { it.id == pc.id }
        prefs.addPc(pc)
        startBridge()
        showMessage(getString(if (known) R.string.pc_updated else R.string.pc_added, pc.name))
    }

    private fun removePc(id: String) {
        prefs.removePc(id)
        if (prefs.pcs.value.isEmpty()) BridgeService.stop(this) else startBridge()
    }

    private fun setPcMuted(id: String, muted: Boolean) {
        prefs.setMuted(id, muted)
        startBridge()
    }

    private fun setAllMuted(muted: Boolean) {
        prefs.setAllMuted(muted)
        startBridge()
    }

    /** (Re)starts the service so it picks up the current PC list and, being visible, may add the mic FGS type. */
    private fun startBridge() {
        if (prefs.pcs.value.isEmpty()) return
        try {
            BridgeService.start(this, fromForeground = true)
        } catch (e: RuntimeException) {
            Log.w(TAG, "could not start bridge service", e)
        }
    }

    // endregion

    // region mic

    private fun setMic(on: Boolean) {
        if (on && !hasPermission(Manifest.permission.RECORD_AUDIO)) {
            requestMic.launch(Manifest.permission.RECORD_AUDIO)
            return
        }
        prefs.setMicEnabled(on)
        startBridge()
    }

    // endregion

    // region remote control

    /** [action] is a `NativeBridge.PC_*` constant; native drops the request (logged) if that PC is not connected. */
    private fun controlPc(pcId: String, action: Int, value: Int) {
        NativeBridge.controlPc(pcId, action, value)
    }

    // endregion

    // region setup checklist

    private fun hasPermission(permission: String) =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    private fun setupItems(autostartDone: Boolean): List<SetupItem> = buildList {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !hasPermission(Manifest.permission.POST_NOTIFICATIONS)) {
            add(
                SetupItem(
                    key = "notifications",
                    title = R.string.setup_notifications,
                    body = R.string.setup_notifications_body,
                    primaryLabel = R.string.action_allow,
                    onPrimary = { requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS) },
                ),
            )
        }
        if (!hasPermission(Manifest.permission.RECORD_AUDIO)) {
            add(
                SetupItem(
                    key = "mic",
                    title = R.string.setup_mic,
                    body = R.string.setup_mic_body,
                    primaryLabel = R.string.action_allow,
                    onPrimary = { requestMic.launch(Manifest.permission.RECORD_AUDIO) },
                ),
            )
        }
        if (!getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(packageName)) {
            add(
                SetupItem(
                    key = "battery",
                    title = R.string.setup_battery,
                    body = R.string.setup_battery_body,
                    primaryLabel = R.string.action_allow,
                    onPrimary = ::requestBatteryExemption,
                ),
            )
        }
        if (isXiaomi() && !autostartDone) {
            add(
                SetupItem(
                    key = "autostart",
                    title = R.string.setup_autostart,
                    body = R.string.setup_autostart_body,
                    primaryLabel = R.string.action_open,
                    onPrimary = ::openAutostartSettings,
                    secondaryLabel = R.string.action_done,
                    onSecondary = { prefs.setAutostartDone(true) },
                ),
            )
        }
    }

    // A connected-device bridge that must stay reachable in the background is an accepted use case.
    @SuppressLint("BatteryLife")
    private fun requestBatteryExemption() {
        val request = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, "package:$packageName".toUri())
        if (!tryStart(request)) tryStart(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
    }

    private fun openAutostartSettings() {
        val miui = Intent().setComponent(
            ComponentName(
                "com.miui.securitycenter",
                "com.miui.permcenter.autostart.AutoStartManagementActivity",
            ),
        )
        if (!tryStart(miui)) openAppDetails()
    }

    private fun openAppDetails() {
        tryStart(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, "package:$packageName".toUri()))
    }

    private fun openNotificationSettings() {
        val intent = Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE, packageName)
        if (!tryStart(intent)) openAppDetails()
    }

    private fun tryStart(intent: Intent): Boolean = try {
        startActivity(intent)
        true
    } catch (e: ActivityNotFoundException) {
        Log.w(TAG, "no activity for $intent", e)
        false
    } catch (e: SecurityException) {
        Log.w(TAG, "not allowed to open $intent", e)
        false
    }

    private fun isXiaomi(): Boolean {
        val vendors = setOf("xiaomi", "redmi", "poco")
        return Build.MANUFACTURER.lowercase() in vendors || Build.BRAND.lowercase() in vendors
    }

    // endregion

    private fun showMessage(@StringRes text: Int, @StringRes actionLabel: Int? = null, onAction: () -> Unit = {}) {
        showMessage(getString(text), actionLabel?.let(::getString), onAction)
    }

    private fun showMessage(text: String, actionLabel: String? = null, onAction: () -> Unit = {}) {
        lifecycleScope.launch {
            val result = snackbar.showSnackbar(
                message = text,
                actionLabel = actionLabel,
                duration = if (actionLabel != null) SnackbarDuration.Long else SnackbarDuration.Short,
            )
            if (result == SnackbarResult.ActionPerformed) onAction()
        }
    }

    companion object {
        private const val TAG = "AudioBridge"

        /** Opened from the notification when the mic is requested but RECORD_AUDIO is not granted yet. */
        const val ACTION_REQUEST_MIC = "app.audiobridge.action.REQUEST_MIC"
    }
}
