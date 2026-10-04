package dev.idfremote.android

import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.PersistableBundle
import android.text.InputType
import android.view.WindowManager
import android.widget.*
import java.net.Inet4Address
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import java.security.SecureRandom

internal data class HttpConfig(val lan: Boolean, val port: Int, val token: String)

/** The app owns listener preferences and secrets; Rust enforces the listener policy. */
internal class GatewaySettings(private val activity: Activity) {
    private val prefs = activity.getSharedPreferences("gateway", Context.MODE_PRIVATE)

    init {
        if (!prefs.contains("token")) newToken()
        applyKeepScreenOn(prefs.getBoolean("keep_screen_on", true))
    }

    fun http() = HttpConfig(prefs.getBoolean("lan", false), prefs.getInt("port", 38473), prefs.getString("token", "")!!)

    private fun newToken(): String {
        val bytes = ByteArray(32).also { SecureRandom().nextBytes(it) }
        return bytes.joinToString("") { "%02x".format(it.toInt() and 255) }.also {
            prefs.edit().putString("token", it).apply()
        }
    }

    private fun applyKeepScreenOn(enabled: Boolean) {
        if (enabled) activity.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        else activity.window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }

    private fun copy(label: String, value: String, sensitive: Boolean = false) {
        val clip = ClipData.newPlainText(label, value)
        if (sensitive) clip.description.extras = PersistableBundle().apply {
            putBoolean("android.content.extra.IS_SENSITIVE", true)
        }
        (activity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager).setPrimaryClip(clip)
        if (Build.VERSION.SDK_INT < 33) Toast.makeText(activity, "$label copied", Toast.LENGTH_SHORT).show()
    }

    private fun addresses(config: HttpConfig): List<String> {
        if (!config.lan) return listOf("http://127.0.0.1:${config.port}")
        return runCatching {
            val connectivity = activity.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
            @Suppress("DEPRECATION")
            val networks = connectivity.allNetworks
            networks.filter { network ->
                val caps = connectivity.getNetworkCapabilities(network)
                caps != null && (caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) || caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET))
            }.flatMap { connectivity.getLinkProperties(it)?.linkAddresses.orEmpty() }
                .map { it.address }.filterIsInstance<Inet4Address>()
                .filter { !it.isLoopbackAddress && !it.isLinkLocalAddress }
                .map { "http://${it.hostAddress}:${config.port}" }.distinct().sorted()
        }.getOrDefault(emptyList())
    }

    fun show(active: HttpConfig?, connecting: Boolean) {
        val initial = active ?: http()
        val locked = active != null || connecting
        var token = initial.token
        val column = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            val pad = (20 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad / 2, pad, 0)
        }
        fun text(value: String) = TextView(activity).apply { text = value }.also { column.addView(it) }
        text(if (locked) "HTTP active or connecting. Disconnect USB to change network settings." else "HTTP starts when you connect a USB device.")
        val lan = Switch(activity).apply { text = "LAN access (token required)"; isChecked = initial.lan; isEnabled = !locked }
        column.addView(lan)
        text("Port")
        val port = EditText(activity).apply {
            inputType = InputType.TYPE_CLASS_NUMBER; setSingleLine(true)
            setText(initial.port.toString()); isEnabled = !locked
        }
        column.addView(port)
        val endpoints = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
        column.addView(endpoints)
        val tokenLabel = text("")
        val copyToken = Button(activity).apply { text = "Copy token"; setOnClickListener { copy("IDF Remote token", token, true) } }
        column.addView(copyToken)
        val regenerate = Button(activity).apply {
            text = "New token"; isEnabled = !locked
            setOnClickListener { token = newToken(); Toast.makeText(activity, "New token saved", Toast.LENGTH_SHORT).show() }
        }
        column.addView(regenerate)
        fun renderAddresses() {
            endpoints.removeAllViews()
            val number = port.text.toString().toIntOrNull()
            if (number != null && number in 1..65535) {
                val urls = addresses(HttpConfig(lan.isChecked, number, token))
                if (urls.isEmpty()) endpoints.addView(TextView(activity).apply { text = "No IPv4 address. Connect Wi-Fi and refresh addresses." })
                urls.forEach { url -> endpoints.addView(Button(activity).apply {
                    text = "Copy $url"; isAllCaps = false; setOnClickListener { copy("IDF Remote URL", url) }
                }) }
            }
            tokenLabel.text = if (lan.isChecked) "Bearer token • stored on this phone" else "Loopback only • no token required"
            copyToken.isEnabled = lan.isChecked
            regenerate.isEnabled = lan.isChecked && !locked
        }
        column.addView(Button(activity).apply { text = "Refresh addresses"; setOnClickListener { renderAddresses() } })
        lan.setOnCheckedChangeListener { _, _ -> renderAddresses() }
        port.addTextChangedListener(object : android.text.TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) { renderAddresses() }
            override fun afterTextChanged(s: android.text.Editable?) {}
        })
        val awake = Switch(activity).apply {
            text = "Keep screen on while app is visible"
            isChecked = prefs.getBoolean("keep_screen_on", true)
            setOnCheckedChangeListener { _, checked ->
                prefs.edit().putBoolean("keep_screen_on", checked).apply()
                applyKeepScreenOn(checked)
            }
        }
        column.addView(awake)
        text("Keep the app open during use. HTTP is unencrypted; use a trusted network.")
        renderAddresses()
        val dialog = AlertDialog.Builder(activity).setTitle("HTTP / Display")
            .setView(ScrollView(activity).apply { addView(column) })
            .setNegativeButton("Close", null)
            .setPositiveButton(if (locked) "Done" else "Save", null).create()
        dialog.setOnShowListener {
            dialog.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener {
                if (!locked) {
                    val number = port.text.toString().toIntOrNull()
                    if (number == null || number !in 1..65535) { port.error = "Use a port from 1 to 65535"; return@setOnClickListener }
                    prefs.edit().putBoolean("lan", lan.isChecked).putInt("port", number).apply()
                }
                dialog.dismiss()
            }
        }
        dialog.show()
    }
}
