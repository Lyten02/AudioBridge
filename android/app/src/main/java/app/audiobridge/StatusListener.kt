package app.audiobridge

/** Receives statusJson snapshots from native code (called from a native thread). */
interface StatusListener {
    fun onStatus(json: String)
}
