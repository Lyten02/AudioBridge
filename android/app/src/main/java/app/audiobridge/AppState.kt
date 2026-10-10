package app.audiobridge

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.provider.Settings
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Persistent user state (paired PCs and their mute flags, mic toggle, dismissed setup steps), observable from the UI. */
class Prefs private constructor(private val sp: SharedPreferences) {
    private val _pcs = MutableStateFlow(readPcs())
    val pcs: StateFlow<List<PairedPc>> = _pcs.asStateFlow()

    private val _micEnabled = MutableStateFlow(sp.getBoolean(KEY_MIC, true))
    val micEnabled: StateFlow<Boolean> = _micEnabled.asStateFlow()

    private val _autostartDone = MutableStateFlow(sp.getBoolean(KEY_AUTOSTART_DONE, false))
    val autostartDone: StateFlow<Boolean> = _autostartDone.asStateFlow()

    private val _headset = MutableStateFlow(readHeadset(sp))
    val headset: StateFlow<HeadsetSettings> = _headset.asStateFlow()

    private val _serviceEnabled = MutableStateFlow(readServiceEnabled(sp))

    /** The user's global AudioBridge switch; off = the service must not run (boot, updates, app start included). */
    val serviceEnabled: StateFlow<Boolean> = _serviceEnabled.asStateFlow()

    /** The background service should run: switched on and at least one PC is paired. */
    fun shouldRun(): Boolean = shouldRun(_serviceEnabled.value, _pcs.value.size)

    private fun readPcs(): List<PairedPc> {
        val legacyUri = sp.getString(KEY_LEGACY_URI, null) ?: return PairedPc.decodeList(sp.getString(KEY_PCS, null))
        // v1 stored a single PC; fold it into the list once.
        var pcs = PairedPc.decodeList(sp.getString(KEY_PCS, null))
        PairedPc.fromParseResult(legacyUri, NativeBridge.parsePairing(legacyUri))?.let { legacy ->
            if (pcs.none { it.id == legacy.id }) pcs = pcs + legacy
        }
        sp.edit {
            putString(KEY_PCS, PairedPc.encodeList(pcs))
            remove(KEY_LEGACY_URI)
            remove(KEY_LEGACY_PC_NAME)
        }
        return pcs
    }

    @Synchronized
    private fun storePcs(pcs: List<PairedPc>) {
        sp.edit { putString(KEY_PCS, PairedPc.encodeList(pcs)) }
        _pcs.value = pcs
    }

    /** Adds a PC, or replaces the one with the same id (keeping its mute setting). */
    @Synchronized
    fun addPc(pc: PairedPc) {
        val muted = _pcs.value.firstOrNull { it.id == pc.id }?.muted ?: pc.muted
        storePcs(PairedPc.upsert(_pcs.value, pc.copy(muted = muted)))
    }

    @Synchronized
    fun removePc(id: String) = storePcs(_pcs.value.filterNot { it.id == id })

    /** Mutes or unmutes the audio of one PC on the phone. */
    @Synchronized
    fun setMuted(id: String, muted: Boolean) =
        storePcs(_pcs.value.map { if (it.id == id) it.copy(muted = muted) else it })

    /** Mutes or unmutes the audio of every paired PC on the phone. */
    @Synchronized
    fun setAllMuted(muted: Boolean) = storePcs(_pcs.value.map { it.copy(muted = muted) })

    fun setMicEnabled(on: Boolean) {
        sp.edit { putBoolean(KEY_MIC, on) }
        _micEnabled.value = on
    }

    fun setAutostartDone(done: Boolean) {
        sp.edit { putBoolean(KEY_AUTOSTART_DONE, done) }
        _autostartDone.value = done
    }

    @Synchronized
    fun setHeadset(settings: HeadsetSettings) {
        sp.edit { writeHeadset(this, settings) }
        _headset.value = settings
    }

    /** Written synchronously: a reboot right after switching off must not start the service again. */
    @Synchronized
    fun setServiceEnabled(on: Boolean) {
        sp.edit(commit = true) { writeServiceEnabled(this, on) }
        _serviceEnabled.value = on
    }

    companion object {
        private const val KEY_PCS = "paired_pcs"
        private const val KEY_LEGACY_URI = "pairing_uri"
        private const val KEY_LEGACY_PC_NAME = "pc_name"
        private const val KEY_MIC = "mic_enabled"
        private const val KEY_AUTOSTART_DONE = "autostart_done"
        private const val KEY_HEADSET_ENABLED = "headset_enabled"
        private const val KEY_HEADSET_SINGLE = "headset_single_earbud"
        private const val KEY_HEADSET_DOUBLE = "headset_double_tap"
        private const val KEY_HEADSET_TRIPLE = "headset_triple_tap"
        private const val KEY_HEADSET_TARGET = "headset_target"
        private const val KEY_SERVICE_ENABLED = "service_enabled"

        /** Missing (installs from before the switch existed) = on. */
        fun readServiceEnabled(sp: SharedPreferences): Boolean = sp.getBoolean(KEY_SERVICE_ENABLED, true)

        fun writeServiceEnabled(editor: SharedPreferences.Editor, on: Boolean) {
            editor.putBoolean(KEY_SERVICE_ENABLED, on)
        }

        fun shouldRun(serviceEnabled: Boolean, pairedPcs: Int): Boolean = serviceEnabled && pairedPcs > 0

        /** Reads the headphone-button settings; missing or unknown values fall back to the defaults. */
        fun readHeadset(sp: SharedPreferences): HeadsetSettings {
            val d = HeadsetSettings()
            return HeadsetSettings(
                enabled = sp.getBoolean(KEY_HEADSET_ENABLED, d.enabled),
                singleEarbud = sp.getBoolean(KEY_HEADSET_SINGLE, d.singleEarbud),
                doubleTap = TapAction.fromPref(sp.getString(KEY_HEADSET_DOUBLE, null), d.doubleTap),
                tripleTap = TapAction.fromPref(sp.getString(KEY_HEADSET_TRIPLE, null), d.tripleTap),
                targetId = sp.getString(KEY_HEADSET_TARGET, null),
            )
        }

        fun writeHeadset(editor: SharedPreferences.Editor, s: HeadsetSettings) {
            editor.putBoolean(KEY_HEADSET_ENABLED, s.enabled)
            editor.putBoolean(KEY_HEADSET_SINGLE, s.singleEarbud)
            editor.putString(KEY_HEADSET_DOUBLE, s.doubleTap.pref)
            editor.putString(KEY_HEADSET_TRIPLE, s.tripleTap.pref)
            if (s.targetId == null) editor.remove(KEY_HEADSET_TARGET) else editor.putString(KEY_HEADSET_TARGET, s.targetId)
        }

        @Volatile
        private var instance: Prefs? = null

        fun get(context: Context): Prefs = instance ?: synchronized(this) {
            instance ?: Prefs(
                context.applicationContext.getSharedPreferences("audiobridge", Context.MODE_PRIVATE),
            ).also { instance = it }
        }
    }
}

/** Live state published by [BridgeService] and observed by the UI while it is visible. */
object StatusHub {
    val status = MutableStateFlow(BridgeStatus.IDLE)

    /** The service holds the microphone FGS type and RECORD_AUDIO is granted (mic ready); capture also needs the switch. */
    val micForeground = MutableStateFlow(false)

    /** The PC the headphone buttons control right now (null: none), resolved by the service. */
    val mediaTargetId = MutableStateFlow<String?>(null)

    /** The last headphone command the service handled, for the settings screen. */
    val lastMediaKey = MutableStateFlow<MediaKeyLog?>(null)
}

object Native {
    /** Loads the native library and starts its runtime; safe to call repeatedly. */
    fun ensureInit(context: Context) {
        NativeBridge.init(context.filesDir.absolutePath, deviceName(context))
    }

    /** User-visible device name ("POCO F5"), falling back to the model name. */
    fun deviceName(context: Context): String {
        val name = Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
        if (!name.isNullOrBlank()) return name
        val model = Build.MODEL.orEmpty()
        val maker = Build.MANUFACTURER.orEmpty().replaceFirstChar { it.uppercase() }
        return if (model.startsWith(maker, ignoreCase = true)) model else "$maker $model".trim()
    }
}
