package app.audiobridge

/** JNI surface of libaudiobridge.so (crates/android-native). Shape fixed by the shared contract. */
object NativeBridge {
    init {
        System.loadLibrary("audiobridge")
    }

    /** Idempotent; starts the runtime and logging (logcat tag "AudioBridge"). */
    external fun init(filesDir: String, deviceName: String)

    /** Returns JSON `{"id":"<peer_id>","name":"LYTEN"}`, or null if [uri] is not a valid pairing URI. Works before [init]. */
    external fun parsePairing(uri: String): String?

    /** Sets the full set of PCs to stay connected to (JSON array of pairing URIs); `[]` disconnects all. */
    external fun setPeers(urisJson: String)

    /** Sets the full set of PC ids whose audio is muted on the phone (JSON array of ids); `[]` unmutes all. */
    external fun setMuted(idsJson: String)

    /**
     * Phone mic state: [enabled] = the user's mic switch; [ready] = RECORD_AUDIO is granted and the service holds FGS
     * type microphone. Native capture needs both; PCs see both.
     */
    external fun setMicState(enabled: Boolean, ready: Boolean)

    /** Phone media volume in percent (0..100) reported to the PCs; -1 = unknown. */
    external fun setVolume(percent: Int)

    /**
     * Remote control of the connected PC [peerId]: [action] is one of the `PC_*` constants, [value] is 0/1 for the
     * switches and 0..100 for [PC_VOLUME]. Dropped (logged) if that PC is not connected.
     */
    external fun controlPc(peerId: String, action: Int, value: Int)

    external fun networkChanged()

    external fun statusJson(): String

    /**
     * Native calls [StatusListener.onStatus] on each status change (debounced ≥250 ms) and the remote-control
     * callbacks as requests arrive, from a native thread.
     */
    external fun setListener(listener: StatusListener?)

    // [controlPc] actions; keep in sync with crates/android-native/src/controls.rs.
    const val PC_AUDIO = 0
    const val PC_MIC = 1
    const val PC_MIC_DEFAULT = 2
    const val PC_VOLUME = 3
    const val PC_MUTE = 4
}
