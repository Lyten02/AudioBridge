package app.audiobridge

import android.view.KeyEvent

/**
 * Headphone buttons → PC media control: the pure part (no Android services), unit-tested on the JVM.
 *
 * What the phone can actually see: the earbuds turn taps into AVRCP commands in their firmware, and Android's
 * Bluetooth stack hands play/pause/next/previous to the media session of the app that played audio last as media
 * key events. Tap counts and which earbud was tapped never reach apps. Volume taps don't arrive as key events at
 * all: AVRCP volume up/down is not a media-session key, and absolute volume changes the phone's media volume
 * directly. So the 1-tap action (volume) cannot be remapped; 2 taps (play/pause) and 3 taps (next/previous) can.
 */

/** A user-selectable action for the single-earbud mode. [pref] is the persisted value: never rename. */
enum class TapAction(val pref: String) {
    PlayPause("play_pause"),
    Next("next"),
    Previous("previous"),
    VolumeUp("volume_up"),
    VolumeDown("volume_down"),
    Nothing("none");

    companion object {
        fun fromPref(value: String?, default: TapAction): TapAction = entries.firstOrNull { it.pref == value } ?: default
    }
}

/** Persisted headphone-button settings. */
data class HeadsetSettings(
    /** Headphone buttons control the music on a PC (the media session exists only while this is on). */
    val enabled: Boolean = true,
    /**
     * Single-earbud mode: the same actions on either earbud. Play/pause (2 taps) and next/previous (3 taps, left or
     * right) are treated as "2 taps" and "3 taps" and run [doubleTap] / [tripleTap].
     */
    val singleEarbud: Boolean = false,
    val doubleTap: TapAction = TapAction.PlayPause,
    val tripleTap: TapAction = TapAction.Next,
    /** The PC to control; null = automatic (see [MediaTarget]). */
    val targetId: String? = null,
)

/** What to do with one headphone command. */
sealed interface KeyAction {
    /** Send a `NativeBridge.MEDIA_*` command to the target PC. */
    data class Pc(val command: Int) : KeyAction

    /** Step the phone's media volume (what you hear in the earbuds) up or down. */
    data class PhoneVolume(val up: Boolean) : KeyAction

    /** A recognized command the user mapped to "nothing": consumed, no effect. */
    data object Ignore : KeyAction
}

/** The command group a media key belongs to, as far as the earbuds' usual layout goes. */
enum class KeyGroup {
    /** 2 taps on either earbud: the earbuds send play, pause or play/pause depending on what they think plays. */
    PlayPause,

    /** 3 taps: next on one earbud, previous on the other. */
    Track,
}

object MediaKeys {
    fun groupOf(keyCode: Int): KeyGroup? = when (keyCode) {
        KeyEvent.KEYCODE_MEDIA_PLAY, KeyEvent.KEYCODE_MEDIA_PAUSE, KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE,
        KeyEvent.KEYCODE_HEADSETHOOK -> KeyGroup.PlayPause
        KeyEvent.KEYCODE_MEDIA_NEXT, KeyEvent.KEYCODE_MEDIA_PREVIOUS -> KeyGroup.Track
        else -> null
    }

    /**
     * Maps a headphone media key to an action; null = not a key we handle (left to the framework).
     *
     * Normal mode keeps the command's meaning: play → play, pause → pause (the earbuds pick them from the state we
     * publish, which mirrors the PC), next → next. Single-earbud mode maps by [KeyGroup].
     */
    fun actionFor(keyCode: Int, settings: HeadsetSettings): KeyAction? {
        val group = groupOf(keyCode) ?: return null
        if (settings.singleEarbud) {
            return when (group) {
                KeyGroup.PlayPause -> actionOf(settings.doubleTap)
                KeyGroup.Track -> actionOf(settings.tripleTap)
            }
        }
        return KeyAction.Pc(
            when (keyCode) {
                KeyEvent.KEYCODE_MEDIA_PLAY -> NativeBridge.MEDIA_PLAY
                KeyEvent.KEYCODE_MEDIA_PAUSE -> NativeBridge.MEDIA_PAUSE
                KeyEvent.KEYCODE_MEDIA_NEXT -> NativeBridge.MEDIA_NEXT
                KeyEvent.KEYCODE_MEDIA_PREVIOUS -> NativeBridge.MEDIA_PREVIOUS
                else -> NativeBridge.MEDIA_PLAY_PAUSE
            },
        )
    }

    fun actionOf(tap: TapAction): KeyAction = when (tap) {
        TapAction.PlayPause -> KeyAction.Pc(NativeBridge.MEDIA_PLAY_PAUSE)
        TapAction.Next -> KeyAction.Pc(NativeBridge.MEDIA_NEXT)
        TapAction.Previous -> KeyAction.Pc(NativeBridge.MEDIA_PREVIOUS)
        TapAction.VolumeUp -> KeyAction.PhoneVolume(up = true)
        TapAction.VolumeDown -> KeyAction.PhoneVolume(up = false)
        TapAction.Nothing -> KeyAction.Ignore
    }

    /**
     * Every press arrives as ACTION_DOWN + ACTION_UP (a held key repeats DOWN). Acting on the first DOWN only makes
     * one press exactly one command.
     */
    fun isPress(action: Int, repeatCount: Int): Boolean = action == KeyEvent.ACTION_DOWN && repeatCount == 0
}

/**
 * Which PC the headphone buttons control. Exactly one PC or none: commands never fan out.
 *
 * A PC chosen in the settings is used only while it is connected. Automatic: among connected PCs that are not muted
 * on the phone, the one playing (if several play, the one that started playing last); if none plays, the PC that
 * was last playing or controlled; otherwise the only connected one. Anything else is ambiguous: no target.
 */
class MediaTarget {
    /** PC ids, most recently started playing or controlled last. */
    private val recent = ArrayList<String>()
    private val playing = HashSet<String>()

    /** Records PCs that started playing since the previous status. */
    fun observe(status: BridgeStatus) {
        val now = status.peers
            .filter { it.state == PeerState.Connected && it.media.playback == PcPlayback.Playing }
            .map { it.id }
            .toSet()
        for (id in now - playing) touch(id)
        playing.clear()
        playing.addAll(now)
    }

    /** The user controlled [id]: it stays the target when nothing plays. */
    fun touch(id: String) {
        recent.remove(id)
        recent.add(id)
    }

    fun resolve(status: BridgeStatus, pcs: List<PairedPc>, targetId: String?): PeerStatus? {
        val connected = status.peers.filter { it.state == PeerState.Connected }
        if (targetId != null && pcs.any { it.id == targetId }) return connected.firstOrNull { it.id == targetId }
        val muted = pcs.filter { it.muted }.map { it.id }.toSet()
        val candidates = connected.filter { it.id !in muted }
        val nowPlaying = candidates.filter { it.media.playback == PcPlayback.Playing }
        if (nowPlaying.size == 1) return nowPlaying[0]
        if (nowPlaying.size > 1) return nowPlaying.maxBy { recent.indexOf(it.id) }
        for (id in recent.asReversed()) candidates.firstOrNull { it.id == id }?.let { return it }
        return candidates.singleOrNull()
    }
}

/** The state our media session publishes for [target] (mirrors the PC so the earbuds send the right command). */
fun sessionPlaying(target: PeerStatus): Boolean = when (target.media.playback) {
    PcPlayback.Playing -> true
    PcPlayback.Paused, PcPlayback.Stopped -> false
    // No media state (no media session on the PC or an old PC app): judge by the audio that arrives.
    PcPlayback.None -> target.pcAudio.active
}
