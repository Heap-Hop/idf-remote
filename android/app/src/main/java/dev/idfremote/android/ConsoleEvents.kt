package dev.idfremote.android

import org.json.JSONObject
import java.nio.ByteBuffer
import java.nio.CharBuffer
import java.nio.charset.CodingErrorAction
import java.util.Base64

/** One decoder per attachment; incomplete UTF-8 bytes survive event/poll boundaries. */
internal class ConsoleEvents {
    private val decoder = Charsets.UTF_8.newDecoder()
        .onMalformedInput(CodingErrorAction.REPLACE)
        .onUnmappableCharacter(CodingErrorAction.REPLACE)
    private var pending = byteArrayOf()

    fun render(event: JSONObject): String {
        val kind = event.getString("kind")
        return when (kind) {
            "raw" -> decode(Base64.getDecoder().decode(event.getJSONObject("data").getString("base64")))
            // raw and log describe the same console bytes; do not display them twice.
            "log" -> ""
            "application_event", "application_connected", "application_disconnected",
            "application_protocol_error", "reconnecting", "reconnected" -> "[$kind ${event.get("data")}]\n"
            else -> ""
        }
    }

    private fun decode(bytes: ByteArray): String {
        val input = ByteBuffer.wrap(pending + bytes)
        val output = CharBuffer.allocate(input.remaining().coerceAtLeast(1))
        decoder.decode(input, output, false).throwExceptionIfError()
        pending = ByteArray(input.remaining()).also { input.get(it) }
        output.flip()
        return output.toString()
    }

    private fun java.nio.charset.CoderResult.throwExceptionIfError() {
        if (isError) throwException()
        check(!isOverflow) { "UTF-8 output buffer overflow" }
    }
}
