package app.audiobridge

/** Callbacks from native code, all made from one native thread (called by name: see proguard-rules.pro). */
interface StatusListener {
    /** A statusJson snapshot. */
    fun onStatus(json: String)

    /** A connected PC asks to switch the phone mic on or off. */
    fun onRemoteMic(enabled: Boolean)

    /** A connected PC asks to set the phone media volume, in percent (0..100). */
    fun onRemoteVolume(percent: Int)
}
