package app.audiobridge

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.provider.Settings
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Persistent user state (paired PCs, mic toggle, dismissed setup steps), observable from the UI. */
class Prefs private constructor(private val sp: SharedPreferences) {
    private val _pcs = MutableStateFlow(readPcs())
    val pcs: StateFlow<List<PairedPc>> = _pcs.asStateFlow()

    private val _micEnabled = MutableStateFlow(sp.getBoolean(KEY_MIC, true))
    val micEnabled: StateFlow<Boolean> = _micEnabled.asStateFlow()

    private val _autostartDone = MutableStateFlow(sp.getBoolean(KEY_AUTOSTART_DONE, false))
    val autostartDone: StateFlow<Boolean> = _autostartDone.asStateFlow()

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

    /** Adds a PC, or replaces the one with the same id. */
    @Synchronized
    fun addPc(pc: PairedPc) = storePcs(PairedPc.upsert(_pcs.value, pc))

    @Synchronized
    fun removePc(id: String) = storePcs(_pcs.value.filterNot { it.id == id })

    fun setMicEnabled(on: Boolean) {
        sp.edit { putBoolean(KEY_MIC, on) }
        _micEnabled.value = on
    }

    fun setAutostartDone(done: Boolean) {
        sp.edit { putBoolean(KEY_AUTOSTART_DONE, done) }
        _autostartDone.value = done
    }

    companion object {
        private const val KEY_PCS = "paired_pcs"
        private const val KEY_LEGACY_URI = "pairing_uri"
        private const val KEY_LEGACY_PC_NAME = "pc_name"
        private const val KEY_MIC = "mic_enabled"
        private const val KEY_AUTOSTART_DONE = "autostart_done"

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
