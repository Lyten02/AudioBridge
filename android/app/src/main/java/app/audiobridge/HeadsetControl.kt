package app.audiobridge

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.media.AudioManager
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock
import android.util.Log
import android.view.KeyEvent

/** One handled headphone command, shown in the settings so the user can see what the phone receives. */
data class MediaKeyLog(val group: KeyGroup?, val action: KeyAction, val pcName: String?, val atElapsedMs: Long)

/**
 * Headphone buttons → the music on a PC, through a [MediaSession] owned by [BridgeService] (main thread only).
 *
 * Android delivers headset media keys (AVRCP play/pause/next/previous) to the media session of the app that played
 * audio last, so while PC audio plays through AudioBridge the keys come here, also with the screen off. The session
 * exists only while the feature is on and exactly one PC is the target ([MediaTarget]); without a target it is
 * released, so the keys go to other players as usual. Its playback state mirrors the target PC, which also tells the
 * earbuds whether their next double tap is "pause" or "play".
 */
class HeadsetControl(private val context: Context, private val audio: AudioManager) {
    private val target = MediaTarget()
    private val main = Handler(Looper.getMainLooper())
    private var session: MediaSession? = null
    private var settings = HeadsetSettings()
    private var current: PeerStatus? = null
    private var publishedPlaying: Boolean? = null
    private var publishedMeta: List<String>? = null

    private val wakeLock: PowerManager.WakeLock by lazy {
        context.getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "AudioBridge:media-key")
            .apply { setReferenceCounted(false) }
    }

    private val callback = object : MediaSession.Callback() {
        override fun onMediaButtonEvent(mediaButtonIntent: Intent): Boolean {
            val event = keyEvent(mediaButtonIntent) ?: return super.onMediaButtonEvent(mediaButtonIntent)
            // Fully handled here: the default implementation would turn a double HEADSETHOOK press into "next".
            val action = MediaKeys.actionFor(event.keyCode, settings)
                ?: return super.onMediaButtonEvent(mediaButtonIntent)
            if (MediaKeys.isPress(event.action, event.repeatCount)) {
                perform(action, MediaKeys.groupOf(event.keyCode))
            }
            return true
        }

        // Transport controls (system media controls, Bluetooth "addressed player" play): literal, never remapped.
        override fun onPlay() = perform(KeyAction.Pc(NativeBridge.MEDIA_PLAY), null)
        override fun onPause() = perform(KeyAction.Pc(NativeBridge.MEDIA_PAUSE), null)
        override fun onStop() = perform(KeyAction.Pc(NativeBridge.MEDIA_PAUSE), null)
        override fun onSkipToNext() = perform(KeyAction.Pc(NativeBridge.MEDIA_NEXT), null)
        override fun onSkipToPrevious() = perform(KeyAction.Pc(NativeBridge.MEDIA_PREVIOUS), null)
    }

    /** Re-resolves the target and updates (or creates/releases) the session. */
    fun update(status: BridgeStatus, pcs: List<PairedPc>, settings: HeadsetSettings) {
        this.settings = settings
        target.observe(status)
        val t = if (settings.enabled) target.resolve(status, pcs, settings.targetId) else null
        current = t
        StatusHub.mediaTargetId.value = t?.id
        if (t == null) {
            release()
            return
        }
        val s = session ?: createSession()
        val playing = sessionPlaying(t)
        if (playing != publishedPlaying) {
            s.setPlaybackState(
                PlaybackState.Builder()
                    .setActions(ACTIONS)
                    .setState(
                        if (playing) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED,
                        PlaybackState.PLAYBACK_POSITION_UNKNOWN,
                        if (playing) 1f else 0f,
                    )
                    .build(),
            )
            publishedPlaying = playing
        }
        val title = t.media.title.ifBlank { t.name }
        val album = if (t.media.app.isBlank()) t.name else context.getString(R.string.headset_media_source, t.media.app, t.name)
        val meta = listOf(title, t.media.artist, album)
        if (meta != publishedMeta) {
            s.setMetadata(
                MediaMetadata.Builder()
                    .putString(MediaMetadata.METADATA_KEY_TITLE, title)
                    .putString(MediaMetadata.METADATA_KEY_ARTIST, t.media.artist)
                    .putString(MediaMetadata.METADATA_KEY_ALBUM, album)
                    .build(),
            )
            publishedMeta = meta
        }
        if (!s.isActive) s.isActive = true
    }

    fun release() {
        session?.let {
            it.isActive = false
            it.release()
            Log.i(TAG, "media session released")
        }
        session = null
        publishedPlaying = null
        publishedMeta = null
    }

    private fun createSession(): MediaSession {
        val s = MediaSession(context, "AudioBridge")
        s.setCallback(callback, main)
        s.setSessionActivity(
            PendingIntent.getActivity(
                context,
                0,
                Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
                PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            ),
        )
        session = s
        Log.i(TAG, "media session created")
        return s
    }

    private fun perform(action: KeyAction, group: KeyGroup?) {
        val t = current
        when (action) {
            is KeyAction.Pc -> {
                if (t == null) {
                    Log.i(TAG, "media command ${action.command} dropped: no target PC")
                    return
                }
                // The system's dispatch wake lock ends when this callback returns; the QUIC write happens right
                // after on the native runtime. Keep the CPU up briefly so a screen-off tap is not delayed.
                wakeLock.acquire(WAKE_MS)
                NativeBridge.controlPc(t.id, NativeBridge.PC_MEDIA, action.command)
                target.touch(t.id)
                Log.i(TAG, "media command ${action.command} -> ${t.name}")
            }
            is KeyAction.PhoneVolume -> try {
                audio.adjustStreamVolume(
                    AudioManager.STREAM_MUSIC,
                    if (action.up) AudioManager.ADJUST_RAISE else AudioManager.ADJUST_LOWER,
                    AudioManager.FLAG_SHOW_UI,
                )
            } catch (e: SecurityException) {
                // Do Not Disturb policy may refuse volume changes.
                Log.w(TAG, "media volume change refused", e)
            }
            KeyAction.Ignore -> Unit
        }
        StatusHub.lastMediaKey.value = MediaKeyLog(group, action, t?.name, SystemClock.elapsedRealtime())
    }

    private fun keyEvent(intent: Intent): KeyEvent? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_KEY_EVENT, KeyEvent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_KEY_EVENT)
        }

    private companion object {
        const val TAG = "AudioBridge"
        const val WAKE_MS = 3_000L
        const val ACTIONS = PlaybackState.ACTION_PLAY or PlaybackState.ACTION_PAUSE or
            PlaybackState.ACTION_PLAY_PAUSE or PlaybackState.ACTION_SKIP_TO_NEXT or
            PlaybackState.ACTION_SKIP_TO_PREVIOUS or PlaybackState.ACTION_STOP
    }
}
