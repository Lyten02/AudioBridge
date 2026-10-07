package app.audiobridge

import org.json.JSONArray
import org.json.JSONObject

/** Per-PC connection state (statusJson v2 `peers[].state`). */
enum class PeerState(val wire: String) {
    Starting("starting"),
    Connecting("connecting"),
    Connected("connected"),
    Reconnecting("reconnecting"),
    Stopped("stopped");

    companion object {
        fun fromWire(value: String): PeerState? = entries.firstOrNull { it.wire == value }
    }
}

enum class PathKind(val wire: String) {
    Lan("lan"),
    Tailscale("tailscale"),
    Direct("direct"),
    Relay("relay");

    companion object {
        fun fromWire(value: String?): PathKind? = entries.firstOrNull { it.wire == value }
    }
}

/** What the UI and the notification show for a connection. */
enum class Reachability { Connecting, Connected, Offline }

data class StreamStats(
    val active: Boolean = false,
    val bufferMs: Float = 0f,
    val underruns: Long = 0,
    val lost: Long = 0,
    val kbps: Float = 0f,
)

/** One paired PC as reported by the native hub. */
data class PeerStatus(
    val id: String,
    val name: String,
    val state: PeerState,
    val path: PathKind? = null,
    val rttMs: Float? = null,
    val pcAudioEnabled: Boolean = true,
    val micEnabled: Boolean = false,
    val micDemanded: Boolean = false,
    /** The PC's own mic switch ([micEnabled] is the effective state). */
    val pcMic: Boolean = false,
    /** The virtual mic is the Windows default recording device on that PC. */
    val micDefault: Boolean = false,
    /** The PC's default playback device volume in percent (0..100); null while unknown. */
    val pcVolume: Int? = null,
    val pcMuted: Boolean = false,
    val pcAudio: StreamStats = StreamStats(),
    val mic: StreamStats = StreamStats(),
    val error: String? = null,
) {
    val reachability: Reachability
        get() = when (state) {
            PeerState.Connected -> Reachability.Connected
            PeerState.Starting, PeerState.Connecting -> Reachability.Connecting
            PeerState.Reconnecting, PeerState.Stopped -> Reachability.Offline
        }

    /** A PC app is capturing the virtual mic and this PC accepts it. */
    val micInUse: Boolean get() = state == PeerState.Connected && micEnabled && micDemanded
}

/** UI model of the native statusJson v2 (see the shared contract for the schema). */
data class BridgeStatus(
    val running: Boolean = false,
    val micEnabled: Boolean = false,
    val micCapturing: Boolean = false,
    val micWanted: Boolean = false,
    val pcAudioActive: Boolean = false,
    val peers: List<PeerStatus> = emptyList(),
) {
    /** Audio flows in either direction; the service keeps Wi-Fi and the CPU awake only while this is true. */
    val isStreaming: Boolean get() = pcAudioActive || micCapturing

    val connectedPeers: List<PeerStatus> get() = peers.filter { it.state == PeerState.Connected }

    fun peer(id: String): PeerStatus? = peers.firstOrNull { it.id == id }

    companion object {
        val IDLE = BridgeStatus()

        /** Parses statusJson v2. Throws [org.json.JSONException] / [IllegalArgumentException] on malformed input. */
        fun parse(json: String): BridgeStatus {
            val o = JSONObject(json)
            val running = when (val state = o.getString("state")) {
                "idle" -> false
                "running" -> true
                else -> throw IllegalArgumentException("unknown hub state: $state")
            }
            val peersJson = o.getJSONArray("peers")
            return BridgeStatus(
                running = running,
                micEnabled = o.getBoolean("micEnabled"),
                micCapturing = o.getBoolean("micCapturing"),
                micWanted = o.getBoolean("micWanted"),
                pcAudioActive = o.getBoolean("pcAudioActive"),
                peers = List(peersJson.length()) { parsePeer(peersJson.getJSONObject(it)) },
            )
        }

        private fun parsePeer(o: JSONObject): PeerStatus {
            val stateWire = o.getString("state")
            return PeerStatus(
                id = o.getString("id"),
                name = o.getString("name"),
                state = PeerState.fromWire(stateWire)
                    ?: throw IllegalArgumentException("unknown peer state: $stateWire"),
                path = PathKind.fromWire(o.optStringOrNull("path")),
                rttMs = if (o.isNull("rttMs")) null else o.getDouble("rttMs").toFloat(),
                pcAudioEnabled = o.getBoolean("pcAudioEnabled"),
                micEnabled = o.getBoolean("micEnabled"),
                micDemanded = o.getBoolean("micDemanded"),
                pcMic = o.getBoolean("pcMic"),
                micDefault = o.getBoolean("micDefault"),
                pcVolume = if (o.isNull("pcVolume")) null else o.getInt("pcVolume"),
                pcMuted = o.getBoolean("pcMuted"),
                pcAudio = parseStream(o.getJSONObject("pcAudio")),
                mic = parseStream(o.getJSONObject("mic")),
                error = o.optStringOrNull("error"),
            )
        }

        private fun parseStream(o: JSONObject) = StreamStats(
            active = o.getBoolean("active"),
            bufferMs = o.getDouble("bufferMs").toFloat(),
            underruns = o.getLong("underruns"),
            lost = o.getLong("lost"),
            kbps = o.getDouble("kbps").toFloat(),
        )
    }
}

/**
 * A paired PC as persisted on the phone. [id] is the stable peer id from the pairing QR; [muted] means its audio is
 * not played on the phone (the connection and the mic keep working).
 */
data class PairedPc(val id: String, val name: String, val uri: String, val muted: Boolean = false) {
    companion object {
        /** Builds a PC from a scanned [uri] and the JSON returned by [NativeBridge.parsePairing]; null if invalid. */
        fun fromParseResult(uri: String, parseResult: String?): PairedPc? {
            parseResult ?: return null
            return try {
                val o = JSONObject(parseResult)
                val id = o.getString("id")
                if (id.isEmpty()) null else PairedPc(id, o.optString("name"), uri)
            } catch (e: org.json.JSONException) {
                null
            }
        }

        fun encodeList(pcs: List<PairedPc>): String = JSONArray().apply {
            pcs.forEach {
                put(JSONObject().put("id", it.id).put("name", it.name).put("uri", it.uri).put("muted", it.muted))
            }
        }.toString()

        /** Decodes a stored list; malformed entries are skipped and duplicates by id collapse to the last one. */
        fun decodeList(json: String?): List<PairedPc> {
            if (json.isNullOrEmpty()) return emptyList()
            val array = try {
                JSONArray(json)
            } catch (e: org.json.JSONException) {
                return emptyList()
            }
            var result = emptyList<PairedPc>()
            for (i in 0 until array.length()) {
                val o = array.optJSONObject(i) ?: continue
                val id = o.optString("id")
                val uri = o.optString("uri")
                if (id.isEmpty() || uri.isEmpty()) continue
                result = upsert(result, PairedPc(id, o.optString("name"), uri, o.optBoolean("muted")))
            }
            return result
        }

        /** Adds [pc] or replaces the entry with the same id in place (re-scanning a PC refreshes its address/name). */
        fun upsert(list: List<PairedPc>, pc: PairedPc): List<PairedPc> {
            val index = list.indexOfFirst { it.id == pc.id }
            return if (index < 0) list + pc else list.toMutableList().also { it[index] = pc }
        }

        /** The JSON array of URIs passed to [NativeBridge.setPeers]. */
        fun urisJson(pcs: List<PairedPc>): String = JSONArray().apply { pcs.forEach { put(it.uri) } }.toString()

        /** The JSON array of muted PC ids passed to [NativeBridge.setMuted]. */
        fun mutedIdsJson(pcs: List<PairedPc>): String =
            JSONArray().apply { pcs.filter { it.muted }.forEach { put(it.id) } }.toString()
    }
}

private fun JSONObject.optStringOrNull(key: String): String? = if (isNull(key)) null else getString(key)
