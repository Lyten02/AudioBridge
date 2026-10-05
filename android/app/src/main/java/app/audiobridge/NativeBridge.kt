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

    /** True only when RECORD_AUDIO is granted, the service holds FGS type microphone and the user toggle is on. */
    external fun setMicAllowed(allowed: Boolean)

    external fun networkChanged()

    external fun statusJson(): String

    /** Native calls [StatusListener.onStatus] on each status change (debounced ≥250 ms) from a native thread. */
    external fun setListener(listener: StatusListener?)
}
