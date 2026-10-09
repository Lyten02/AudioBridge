package app.audiobridge

import android.content.SharedPreferences
import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class MediaKeysTest {
    private val normal = HeadsetSettings()
    private val single = HeadsetSettings(singleEarbud = true, doubleTap = TapAction.PlayPause, tripleTap = TapAction.Next)

    private fun pc(cmd: Int) = KeyAction.Pc(cmd)

    @Test
    fun normalModeKeepsTheMeaningOfEachCommand() {
        val cases = mapOf(
            KeyEvent.KEYCODE_MEDIA_PLAY to pc(NativeBridge.MEDIA_PLAY),
            KeyEvent.KEYCODE_MEDIA_PAUSE to pc(NativeBridge.MEDIA_PAUSE),
            KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE to pc(NativeBridge.MEDIA_PLAY_PAUSE),
            KeyEvent.KEYCODE_HEADSETHOOK to pc(NativeBridge.MEDIA_PLAY_PAUSE),
            KeyEvent.KEYCODE_MEDIA_NEXT to pc(NativeBridge.MEDIA_NEXT),
            KeyEvent.KEYCODE_MEDIA_PREVIOUS to pc(NativeBridge.MEDIA_PREVIOUS),
        )
        cases.forEach { (key, action) -> assertEquals("key $key", action, MediaKeys.actionFor(key, normal)) }
    }

    @Test
    fun volumeAndOtherKeysAreNotHandled() {
        // Volume never reaches a media session as a key; if it did, it must stay with the system.
        for (key in listOf(
            KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.KEYCODE_MEDIA_STOP,
            KeyEvent.KEYCODE_MEDIA_FAST_FORWARD, KeyEvent.KEYCODE_ENTER,
        )) {
            assertNull("key $key", MediaKeys.actionFor(key, normal))
            assertNull("key $key", MediaKeys.actionFor(key, single))
        }
    }

    @Test
    fun singleEarbudGivesBothSidesTheSameActions() {
        // Left and right triple taps (previous / next) both run the 3-tap action.
        assertEquals(pc(NativeBridge.MEDIA_NEXT), MediaKeys.actionFor(KeyEvent.KEYCODE_MEDIA_PREVIOUS, single))
        assertEquals(pc(NativeBridge.MEDIA_NEXT), MediaKeys.actionFor(KeyEvent.KEYCODE_MEDIA_NEXT, single))
        // A double tap arrives as play or pause depending on what the earbud thinks: both toggle on the PC,
        // which decides from its real state.
        for (key in listOf(KeyEvent.KEYCODE_MEDIA_PLAY, KeyEvent.KEYCODE_MEDIA_PAUSE, KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE)) {
            assertEquals(pc(NativeBridge.MEDIA_PLAY_PAUSE), MediaKeys.actionFor(key, single))
        }
    }

    @Test
    fun singleEarbudRunsTheChosenActions() {
        val s = single.copy(doubleTap = TapAction.VolumeUp, tripleTap = TapAction.VolumeDown)
        assertEquals(KeyAction.PhoneVolume(up = true), MediaKeys.actionFor(KeyEvent.KEYCODE_MEDIA_PAUSE, s))
        assertEquals(KeyAction.PhoneVolume(up = false), MediaKeys.actionFor(KeyEvent.KEYCODE_MEDIA_NEXT, s))
        val none = single.copy(doubleTap = TapAction.Nothing, tripleTap = TapAction.Previous)
        assertEquals(KeyAction.Ignore, MediaKeys.actionFor(KeyEvent.KEYCODE_MEDIA_PLAY, none))
        assertEquals(pc(NativeBridge.MEDIA_PREVIOUS), MediaKeys.actionFor(KeyEvent.KEYCODE_MEDIA_NEXT, none))
    }

    @Test
    fun onePressIsExactlyOneCommand() {
        // DOWN, repeated DOWNs of a held key, UP: only the first DOWN acts.
        val events = listOf(KeyEvent.ACTION_DOWN to 0, KeyEvent.ACTION_DOWN to 1, KeyEvent.ACTION_DOWN to 2, KeyEvent.ACTION_UP to 0)
        assertEquals(1, events.count { (action, repeat) -> MediaKeys.isPress(action, repeat) })
    }

    @Test
    fun mediaCommandCodesMatchTheNativeContract() {
        // crates/core/src/proto.rs MediaCommand::code
        assertEquals(listOf(0, 1, 2, 3, 4), listOf(
            NativeBridge.MEDIA_PLAY, NativeBridge.MEDIA_PAUSE, NativeBridge.MEDIA_PLAY_PAUSE,
            NativeBridge.MEDIA_NEXT, NativeBridge.MEDIA_PREVIOUS,
        ))
        assertEquals(5, NativeBridge.PC_MEDIA)
    }

    // region target

    private fun peer(id: String, state: PeerState = PeerState.Connected, playback: PcPlayback = PcPlayback.Paused, audio: Boolean = false) =
        PeerStatus(id = id, name = id.uppercase(), state = state, media = PcMedia(playback), pcAudio = StreamStats(active = audio))

    private fun status(vararg peers: PeerStatus) = BridgeStatus(running = true, peers = peers.toList())

    private fun paired(vararg ids: String, muted: Set<String> = emptySet()) =
        ids.map { PairedPc(it, it.uppercase(), "audiobridge://pair?d=$it", muted = it in muted) }

    private fun MediaTarget.at(s: BridgeStatus, pcs: List<PairedPc>, choice: String? = null): String? {
        observe(s)
        return resolve(s, pcs, choice)?.id
    }

    @Test
    fun autoPicksThePlayingPc() {
        val t = MediaTarget()
        assertEquals("b", t.at(status(peer("a"), peer("b", playback = PcPlayback.Playing)), paired("a", "b")))
    }

    @Test
    fun severalPlayingPicksTheOneThatStartedLast() {
        val t = MediaTarget()
        val pcs = paired("a", "b")
        assertEquals("a", t.at(status(peer("a", playback = PcPlayback.Playing), peer("b")), pcs))
        assertEquals(
            "b",
            t.at(status(peer("a", playback = PcPlayback.Playing), peer("b", playback = PcPlayback.Playing)), pcs),
        )
    }

    @Test
    fun whenNothingPlaysTheLastPlayingOrControlledPcStays() {
        val t = MediaTarget()
        val pcs = paired("a", "b")
        t.at(status(peer("a"), peer("b", playback = PcPlayback.Playing)), pcs)
        // paused with a double tap: the next double tap must resume the same PC
        assertEquals("b", t.at(status(peer("a"), peer("b")), pcs))
        t.touch("a")
        assertEquals("a", t.at(status(peer("a"), peer("b")), pcs))
    }

    @Test
    fun ambiguityMeansNoTargetInsteadOfFanOut() {
        assertNull(MediaTarget().at(status(peer("a"), peer("b")), paired("a", "b")))
        // a single connected PC needs no history
        assertEquals("a", MediaTarget().at(status(peer("a"), peer("b", state = PeerState.Reconnecting)), paired("a", "b")))
    }

    @Test
    fun mutedAndDisconnectedPcsAreNotAutoTargets() {
        val t = MediaTarget()
        val s = status(peer("a", playback = PcPlayback.Playing), peer("b"))
        assertEquals("b", t.at(s, paired("a", "b", muted = setOf("a"))))
        assertNull(MediaTarget().at(status(peer("a", state = PeerState.Reconnecting, playback = PcPlayback.Playing)), paired("a")))
    }

    @Test
    fun chosenPcIsUsedOnlyWhileConnected() {
        val pcs = paired("a", "b")
        val playingA = status(peer("a", playback = PcPlayback.Playing), peer("b"))
        assertEquals("b", MediaTarget().at(playingA, pcs, choice = "b"))
        // the chosen PC is offline: no silent fallback to another PC
        assertNull(MediaTarget().at(status(peer("a", playback = PcPlayback.Playing), peer("b", state = PeerState.Connecting)), pcs, choice = "b"))
        // a choice that is no longer paired means automatic
        assertEquals("a", MediaTarget().at(playingA, pcs, choice = "gone"))
    }

    @Test
    fun publishedStateMirrorsThePc() {
        assertTrue(sessionPlaying(peer("a", playback = PcPlayback.Playing)))
        assertFalse(sessionPlaying(peer("a", playback = PcPlayback.Paused, audio = true)))
        assertFalse(sessionPlaying(peer("a", playback = PcPlayback.Stopped)))
        // no media state from the PC: judge by the arriving audio
        assertTrue(sessionPlaying(peer("a", playback = PcPlayback.None, audio = true)))
        assertFalse(sessionPlaying(peer("a", playback = PcPlayback.None, audio = false)))
    }

    // endregion

    // region persistence

    @Test
    fun settingsSurviveARoundTrip() {
        val sp = MemoryPrefs()
        assertEquals(HeadsetSettings(), Prefs.readHeadset(sp))
        val custom = HeadsetSettings(
            enabled = false,
            singleEarbud = true,
            doubleTap = TapAction.VolumeDown,
            tripleTap = TapAction.Previous,
            targetId = "pc-1",
        )
        sp.edit().also { Prefs.writeHeadset(it, custom) }.apply()
        assertEquals(custom, Prefs.readHeadset(sp))
        sp.edit().also { Prefs.writeHeadset(it, custom.copy(targetId = null)) }.apply()
        assertNull(Prefs.readHeadset(sp).targetId)
    }

    @Test
    fun persistedValuesAreStableAndUnknownOnesFallBack() {
        assertEquals(
            listOf("play_pause", "next", "previous", "volume_up", "volume_down", "none"),
            TapAction.entries.map { it.pref },
        )
        val sp = MemoryPrefs()
        sp.edit().putString("headset_double_tap", "rewind").putString("headset_triple_tap", "volume_up").apply()
        val s = Prefs.readHeadset(sp)
        assertEquals(TapAction.PlayPause, s.doubleTap)
        assertEquals(TapAction.VolumeUp, s.tripleTap)
    }

    /** In-memory [SharedPreferences] (the JVM tests have no Android runtime). */
    private class MemoryPrefs : SharedPreferences {
        val map = HashMap<String, Any?>()

        override fun getAll(): Map<String, *> = map
        override fun getString(key: String, defValue: String?) = map[key] as String? ?: defValue
        override fun getStringSet(key: String, defValues: Set<String>?) = defValues
        override fun getInt(key: String, defValue: Int) = map[key] as Int? ?: defValue
        override fun getLong(key: String, defValue: Long) = map[key] as Long? ?: defValue
        override fun getFloat(key: String, defValue: Float) = map[key] as Float? ?: defValue
        override fun getBoolean(key: String, defValue: Boolean) = map[key] as Boolean? ?: defValue
        override fun contains(key: String) = key in map
        override fun registerOnSharedPreferenceChangeListener(l: SharedPreferences.OnSharedPreferenceChangeListener) = Unit
        override fun unregisterOnSharedPreferenceChangeListener(l: SharedPreferences.OnSharedPreferenceChangeListener) = Unit

        override fun edit(): SharedPreferences.Editor = object : SharedPreferences.Editor {
            val pending = HashMap<String, Any?>()
            val removed = HashSet<String>()
            override fun putString(key: String, value: String?) = apply { pending[key] = value }
            override fun putStringSet(key: String, values: Set<String>?) = apply { pending[key] = values }
            override fun putInt(key: String, value: Int) = apply { pending[key] = value }
            override fun putLong(key: String, value: Long) = apply { pending[key] = value }
            override fun putFloat(key: String, value: Float) = apply { pending[key] = value }
            override fun putBoolean(key: String, value: Boolean) = apply { pending[key] = value }
            override fun remove(key: String) = apply { removed.add(key) }
            override fun clear() = apply { removed.addAll(map.keys) }
            override fun commit(): Boolean {
                apply()
                return true
            }
            override fun apply() {
                removed.forEach { map.remove(it) }
                map.putAll(pending)
            }
        }
    }

    // endregion
}
