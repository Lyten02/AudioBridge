package app.audiobridge

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ServiceSwitchTest {
    @Test
    fun installsFromBeforeTheSwitchStayOn() {
        assertTrue(Prefs.readServiceEnabled(MemoryPrefs()))
    }

    @Test
    fun offSurvivesARestartAndOnComesBack() {
        val sp = MemoryPrefs()
        sp.edit().also { Prefs.writeServiceEnabled(it, false) }.apply()
        // what BootReceiver / a sticky restart reads after a reboot
        assertFalse(Prefs.readServiceEnabled(sp))
        sp.edit().also { Prefs.writeServiceEnabled(it, true) }.apply()
        assertTrue(Prefs.readServiceEnabled(sp))
    }

    @Test
    fun switchingOffKeepsPairingsAndSettings() {
        val sp = MemoryPrefs()
        sp.edit().putString("paired_pcs", "[...]").putBoolean("mic_enabled", false).apply()
        val headset = HeadsetSettings(singleEarbud = true, tripleTap = TapAction.Previous)
        sp.edit().also { Prefs.writeHeadset(it, headset) }.apply()
        sp.edit().also { Prefs.writeServiceEnabled(it, false) }.apply()
        assertEquals("[...]", sp.getString("paired_pcs", null))
        assertFalse(sp.getBoolean("mic_enabled", true))
        assertEquals(headset, Prefs.readHeadset(sp))
    }

    @Test
    fun runsOnlyWhenSwitchedOnWithAPairedPc() {
        assertTrue(Prefs.shouldRun(serviceEnabled = true, pairedPcs = 1))
        assertFalse(Prefs.shouldRun(serviceEnabled = false, pairedPcs = 3))
        assertFalse(Prefs.shouldRun(serviceEnabled = true, pairedPcs = 0))
    }
}
