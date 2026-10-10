package app.audiobridge

import org.json.JSONException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BridgeStatusTest {
    private fun peer(
        id: String = "abc",
        name: String = "LYTEN",
        state: String = "connected",
        path: String = "\"lan\"",
        rtt: String = "4.2",
        pcActive: Boolean = true,
        micEnabled: Boolean = true,
        micDemanded: Boolean = false,
        pcMic: Boolean = true,
        micDefault: Boolean = false,
        pcVolume: String = "64",
        pcMuted: Boolean = false,
        media: String = """{"playback":"playing","app":"Яндекс Музыка","title":"Танцуй!","artist":"SATS"}""",
        error: String = "null",
    ) = """
        {"id":"$id","name":"$name","state":"$state","path":$path,"rttMs":$rtt,"pcAudioEnabled":true,
         "micEnabled":$micEnabled,"micDemanded":$micDemanded,"pcMic":$pcMic,"micDefault":$micDefault,
         "pcVolume":$pcVolume,"pcMuted":$pcMuted,"media":$media,
         "pcAudio":{"active":$pcActive,"bufferMs":31.0,"underruns":2,"lost":7,"kbps":190.5},
         "mic":{"active":false,"bufferMs":0,"underruns":0,"lost":0,"kbps":0},
         "error":$error}
    """.trimIndent()

    private fun hub(
        state: String = "running",
        pcAudioActive: Boolean = true,
        micCapturing: Boolean = false,
        micWanted: Boolean = false,
        peers: List<String> = listOf(peer()),
    ) = """{"state":"$state","micEnabled":true,"micCapturing":$micCapturing,"micWanted":$micWanted,
        "pcAudioActive":$pcAudioActive,"peers":[${peers.joinToString(",")}]}"""

    @Test
    fun parsesContractSample() {
        val s = BridgeStatus.parse(hub())
        assertTrue(s.running)
        assertTrue(s.isStreaming)
        assertEquals(1, s.peers.size)
        val p = s.peer("abc")!!
        assertEquals("LYTEN", p.name)
        assertEquals(PeerState.Connected, p.state)
        assertEquals(Reachability.Connected, p.reachability)
        assertEquals(PathKind.Lan, p.path)
        assertEquals(4.2f, p.rttMs!!, 1e-4f)
        assertEquals(StreamStats(active = true, bufferMs = 31f, underruns = 2, lost = 7, kbps = 190.5f), p.pcAudio)
        assertFalse(p.mic.active)
        assertNull(p.error)
        assertTrue(p.pcMic)
        assertFalse(p.micDefault)
        assertEquals(64, p.pcVolume)
        assertFalse(p.pcMuted)
    }

    @Test
    fun parsesPcRemoteControls() {
        val p = BridgeStatus.parse(
            hub(peers = listOf(peer(micEnabled = false, micDefault = true, pcVolume = "37", pcMuted = true))),
        ).peers[0]
        // The PC's own switch stays on while the effective state is off.
        assertFalse(p.micEnabled)
        assertTrue(p.pcMic)
        assertTrue(p.micDefault)
        assertEquals(37, p.pcVolume)
        assertTrue(p.pcMuted)

        val unknown = BridgeStatus.parse(hub(peers = listOf(peer(pcMic = false, pcVolume = "null")))).peers[0]
        assertFalse(unknown.pcMic)
        assertNull(unknown.pcVolume)
        assertFalse(unknown.pcMuted)
    }

    @Test
    fun parsesPcMedia() {
        val p = BridgeStatus.parse(hub()).peers[0]
        assertEquals(PcMedia(PcPlayback.Playing, "Яндекс Музыка", "Танцуй!", "SATS"), p.media)
        for (playback in PcPlayback.entries) {
            val media = """{"playback":"${playback.wire}","app":"","title":"","artist":""}"""
            assertEquals(PcMedia(playback), BridgeStatus.parse(hub(peers = listOf(peer(media = media)))).peers[0].media)
        }
    }

    @Test(expected = IllegalArgumentException::class)
    fun unknownPlaybackIsRejected() {
        BridgeStatus.parse(hub(peers = listOf(peer(media = """{"playback":"buffering","app":"","title":"","artist":""}"""))))
    }

    @Test
    fun idleHubWithNoPeers() {
        val s = BridgeStatus.parse(hub(state = "idle", pcAudioActive = false, peers = emptyList()))
        assertFalse(s.running)
        assertFalse(s.isStreaming)
        assertTrue(s.peers.isEmpty())
        assertTrue(s.connectedPeers.isEmpty())
    }

    @Test
    fun multiplePeersKeepOrderAndPerPeerState() {
        val s = BridgeStatus.parse(
            hub(
                peers = listOf(
                    peer(id = "a", name = "LYTEN", micDemanded = true),
                    peer(id = "b", name = "LAPTOP", state = "reconnecting", path = "null", rtt = "null", error = "\"timed out\""),
                ),
            ),
        )
        assertEquals(listOf("a", "b"), s.peers.map { it.id })
        assertEquals(listOf("LYTEN"), s.connectedPeers.map { it.name })
        assertTrue(s.peer("a")!!.micInUse)
        val offline = s.peer("b")!!
        assertEquals(Reachability.Offline, offline.reachability)
        assertNull(offline.path)
        assertNull(offline.rttMs)
        assertEquals("timed out", offline.error)
        assertNull(s.peer("missing"))
    }

    @Test
    fun micInUseNeedsConnectionAndPcSideEnable() {
        assertFalse(BridgeStatus.parse(hub(peers = listOf(peer(micDemanded = true, micEnabled = false)))).peers[0].micInUse)
        assertFalse(BridgeStatus.parse(hub(peers = listOf(peer(micDemanded = true, state = "connecting")))).peers[0].micInUse)
    }

    @Test
    fun everyPathAndPeerStateMaps() {
        val paths = mapOf("lan" to PathKind.Lan, "tailscale" to PathKind.Tailscale, "direct" to PathKind.Direct, "relay" to PathKind.Relay)
        paths.forEach { (wire, kind) ->
            assertEquals(kind, BridgeStatus.parse(hub(peers = listOf(peer(path = "\"$wire\"")))).peers[0].path)
        }
        val reach = mapOf(
            "starting" to Reachability.Connecting,
            "connecting" to Reachability.Connecting,
            "connected" to Reachability.Connected,
            "reconnecting" to Reachability.Offline,
            "stopped" to Reachability.Offline,
        )
        reach.forEach { (wire, r) ->
            assertEquals(wire, r, BridgeStatus.parse(hub(peers = listOf(peer(state = wire)))).peers[0].reachability)
        }
    }

    @Test
    fun micCaptureAloneCountsAsStreaming() {
        assertTrue(BridgeStatus.parse(hub(pcAudioActive = false, micCapturing = true)).isStreaming)
    }

    @Test(expected = IllegalArgumentException::class)
    fun unknownHubStateIsRejected() {
        BridgeStatus.parse(hub(state = "connected"))
    }

    @Test(expected = IllegalArgumentException::class)
    fun unknownPeerStateIsRejected() {
        BridgeStatus.parse(hub(peers = listOf(peer(state = "flying"))))
    }

    @Test(expected = JSONException::class)
    fun missingKeyIsRejected() {
        BridgeStatus.parse("""{"state":"running","peers":[]}""")
    }
}
