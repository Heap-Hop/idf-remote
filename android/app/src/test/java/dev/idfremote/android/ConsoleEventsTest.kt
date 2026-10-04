package dev.idfremote.android

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import java.util.Base64

class ConsoleEventsTest {
    private fun raw(bytes: ByteArray) = JSONObject().put("kind", "raw")
        .put("data", JSONObject().put("base64", Base64.getEncoder().encodeToString(bytes)))

    @Test fun promptsAndEchoAppearWithoutNewlinesAndLogsDoNotDuplicate() {
        val decoder = ConsoleEvents()
        assertEquals("device> ", decoder.render(raw("device> ".toByteArray())))
        assertEquals("a", decoder.render(raw("a".toByteArray())))
        assertEquals("", decoder.render(JSONObject("""{"kind":"log","data":{"text":"device> a"}}""")))
    }

    @Test fun utf8SurvivesEveryByteBoundaryAndInvalidBytesRecover() {
        val decoder = ConsoleEvents()
        val text = "中文🙂"
        val result = text.toByteArray().joinToString("") { decoder.render(raw(byteArrayOf(it))) }
        assertEquals(text, result)
        assertEquals("�x", decoder.render(raw(byteArrayOf(0xff.toByte(), 'x'.code.toByte()))))
    }

    @Test fun arbitraryApplicationJsonDoesNotBreakFollowingConsole() {
        val decoder = ConsoleEvents()
        for (value in listOf("{}", "[]", "\"hello\"", "42", "true", "null")) {
            val rendered = decoder.render(JSONObject("""{"kind":"application_event","data":$value}"""))
            assertTrue(rendered.startsWith("[application_event "))
            assertEquals("next", decoder.render(raw("next".toByteArray())))
        }
        assertEquals("", decoder.render(JSONObject("""{"kind":"future_event","data":null}""")))
    }
}
