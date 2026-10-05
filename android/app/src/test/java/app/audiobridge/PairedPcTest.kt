package app.audiobridge

import org.json.JSONArray
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairedPcTest {
    private val lyten = PairedPc("id-1", "LYTEN", "audiobridge://pair?d=one")
    private val laptop = PairedPc("id-2", "LAPTOP", "audiobridge://pair?d=two")

    @Test
    fun upsertAppendsNewAndReplacesSameIdInPlace() {
        val list = PairedPc.upsert(PairedPc.upsert(emptyList(), lyten), laptop)
        assertEquals(listOf(lyten, laptop), list)

        val rescanned = PairedPc("id-1", "LYTEN-2", "audiobridge://pair?d=new")
        assertEquals(listOf(rescanned, laptop), PairedPc.upsert(list, rescanned))
    }

    @Test
    fun listRoundTripsAndDecodeDedupesById() {
        val list = listOf(lyten, laptop)
        assertEquals(list, PairedPc.decodeList(PairedPc.encodeList(list)))

        val newer = lyten.copy(uri = "audiobridge://pair?d=newer")
        assertEquals(listOf(newer, laptop), PairedPc.decodeList(PairedPc.encodeList(listOf(lyten, laptop, newer))))
    }

    @Test
    fun decodeSkipsGarbage() {
        assertEquals(emptyList<PairedPc>(), PairedPc.decodeList(null))
        assertEquals(emptyList<PairedPc>(), PairedPc.decodeList("not json"))
        assertEquals(
            listOf(lyten),
            PairedPc.decodeList("""[{"id":"","uri":"x"},{"id":"z"},7,${PairedPc.encodeList(listOf(lyten)).removeSurrounding("[", "]")}]"""),
        )
    }

    @Test
    fun parseResultBuildsPcOrRejects() {
        assertEquals(lyten, PairedPc.fromParseResult(lyten.uri, """{"id":"id-1","name":"LYTEN"}"""))
        assertNull(PairedPc.fromParseResult(lyten.uri, null))
        assertNull(PairedPc.fromParseResult(lyten.uri, """{"name":"LYTEN"}"""))
        assertNull(PairedPc.fromParseResult(lyten.uri, """{"id":"","name":"LYTEN"}"""))
    }

    @Test
    fun urisJsonIsArrayOfUrisInOrder() {
        val array = JSONArray(PairedPc.urisJson(listOf(lyten, laptop)))
        assertEquals(listOf(lyten.uri, laptop.uri), List(array.length()) { array.getString(it) })
        assertEquals("[]", PairedPc.urisJson(emptyList()))
    }

    @Test
    fun mutedFlagRoundTripsAndDefaultsToUnmuted() {
        val list = listOf(lyten.copy(muted = true), laptop)
        assertEquals(list, PairedPc.decodeList(PairedPc.encodeList(list)))
        // Lists stored before muting existed have no "muted" key.
        assertEquals(listOf(lyten), PairedPc.decodeList("""[{"id":"id-1","name":"LYTEN","uri":"${lyten.uri}"}]"""))
    }

    @Test
    fun mutedIdsJsonListsOnlyMutedPcs() {
        val array = JSONArray(PairedPc.mutedIdsJson(listOf(lyten, laptop.copy(muted = true))))
        assertEquals(listOf(laptop.id), List(array.length()) { array.getString(it) })
        assertEquals("[]", PairedPc.mutedIdsJson(listOf(lyten, laptop)))
    }
}
