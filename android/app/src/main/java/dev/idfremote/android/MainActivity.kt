package dev.idfremote.android

import android.app.Activity
import android.app.PendingIntent
import android.content.*
import android.hardware.usb.*
import android.os.*
import android.widget.*
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

object Native {
    init { System.loadLibrary("idf_remote_android") }
    @JvmStatic external fun open(fd: Int, cache: String, lan: Boolean, port: Int, token: String): Long
    @JvmStatic external fun poll(id: Long): String
    @JvmStatic external fun write(id: Long, bytes: ByteArray): String
    @JvmStatic external fun application(id: Long, method: String, params: String): String
    @JvmStatic external fun close(id: Long)
}

/** Foreground example. Android owns permission/lifecycle; idf_remote owns all USB IO. */
class MainActivity : Activity() {
    private lateinit var settings: GatewaySettings
    @Volatile private var activeHttp: HttpConfig? = null
    @Volatile private var connecting = false
    private val usb by lazy { getSystemService(USB_SERVICE) as UsbManager }
    private val worker = Executors.newSingleThreadExecutor()
    private val ui = Handler(Looper.getMainLooper())
    private val pending = StringBuilder()
    private val connectedButtons = mutableListOf<Button>()
    private lateinit var log: ConsoleLogView
    private lateinit var status: TextView
    private lateinit var devices: LinearLayout
    @Volatile private var destroyed = false
    private var connection: UsbDeviceConnection? = null // worker-owned
    private var handle = 0L
    @Volatile private var currentDevice: String? = null
    private var reader: Thread? = null
    private var running = AtomicBoolean(false)
    private data class PermissionRequest(val device: UsbDevice, val id: Int)
    private var permissionPending: PermissionRequest? = null // UI-owned
    private var nextPermissionRequest = 0
    private val permissionAction get() = "$packageName.USB_PERMISSION"
    private fun append(text: String) { synchronized(pending) {
        pending.append(text)
        if (pending.length > 32768) pending.delete(0,pending.length-32768)
    } }
    private fun state(text: String) {
        android.util.Log.i("IdfRemote", text)
        append("\n[$text]\n")
        ui.post { if (!destroyed) status.text = text }
    }
    private fun connected(value: Boolean) { ui.post {
        if (!destroyed) connectedButtons.forEach { it.isEnabled = value }
    } }
    private val render = object : Runnable {
        override fun run() {
            val text = synchronized(pending) { pending.toString().also { pending.setLength(0) } }
            if (text.isNotEmpty()) log.append(text)
            if (!destroyed) ui.postDelayed(this,100)
        }
    }
    @Suppress("DEPRECATION")
    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val device = intent.getParcelableExtra<UsbDevice>(UsbManager.EXTRA_DEVICE)
            when (intent.action) {
                permissionAction -> {
                    val request = permissionPending ?: return
                    if (intent.getIntExtra("request_id", -1) != request.id ||
                        device?.deviceName != request.device.deviceName || device.deviceId != request.device.deviceId) return
                    permissionPending = null
                    val live = usb.deviceList[request.device.deviceName]
                    if (live == null || live.deviceId != request.device.deviceId || !supported(live))
                        state("USB unavailable; select Connect again")
                    else if (usb.hasPermission(live)) connect(live)
                    else state("USB permission denied")
                }
                UsbManager.ACTION_USB_DEVICE_DETACHED -> {
                    if (device?.deviceName == permissionPending?.device?.deviceName) permissionPending = null
                    worker.execute { if (currentDevice == device?.deviceName) { disconnect(); state("USB detached • waiting for authorized reconnect") } }
                    refresh()
                }
                UsbManager.ACTION_USB_DEVICE_ATTACHED -> { refresh(); handleUsbAttach(intent) }
            }
        }
    }
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        settings = GatewaySettings(this)
        val root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.setOnApplyWindowInsetsListener { view, insets ->
            @Suppress("DEPRECATION")
            view.setPadding(20,insets.systemWindowInsetTop+12,20,insets.systemWindowInsetBottom+12)
            insets
        }
        status = TextView(this).apply { text = "IDF Remote • Select an authorized USB device" }
        root.addView(status)
        devices = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        root.addView(devices)
        fun row(vararg buttons: Pair<String, () -> Unit>, needsConnection: Boolean = false) {
            val row = LinearLayout(this)
            buttons.forEach { (label, action) ->
                val button = Button(this).apply { text=label; textSize=12f; setOnClickListener { action() } }
                if (needsConnection) { button.isEnabled=false; connectedButtons.add(button) }
                row.addView(button,LinearLayout.LayoutParams(0,-2,1f))
            }
            root.addView(row)
        }
        row("Refresh" to { refresh() }, "Disconnect" to { worker.execute { disconnect(); state("Disconnected") } },
            "Clear" to { synchronized(pending) { pending.setLength(0) }; log.clear() })
        row("HTTP / Display" to { settings.show(activeHttp, connecting) }, "Latest logs" to { log.followLatest() })
        log = ConsoleLogView(this)
        root.addView(log,LinearLayout.LayoutParams(-1,0,1f))
        val input=EditText(this).apply { hint="Console input"; setSingleLine(true) }; root.addView(input)
        row("Send ↵" to { send(input.text.toString()+"\r") }, "Raw" to { send(input.text.toString()) },
            "Tab" to { send("\t") }, "Ctrl-C" to { send("\u0003") }, needsConnection=true)
        row("App connect" to { application("", "null") }, "Status" to { application("status","null") },
            "Echo" to { application("echo",JSONObject.quote(input.text.toString())) }, needsConnection=true)
        val method=EditText(this).apply { hint="Application method"; setSingleLine(true) }; root.addView(method)
        val params=EditText(this).apply { hint="JSON params (default null)"; setSingleLine(true) }; root.addView(params)
        row("Call" to { application(method.text.toString(),params.text.toString().ifBlank { "null" }) }, needsConnection=true)
        root.addView(TextView(this).apply { text="HTTP / Display: copy connection details • keep app open"; textSize=11f })
        setContentView(root)
        val filter=IntentFilter(permissionAction).apply { addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED); addAction(UsbManager.ACTION_USB_DEVICE_DETACHED) }
        if (Build.VERSION.SDK_INT>=33) registerReceiver(receiver,filter,Context.RECEIVER_NOT_EXPORTED) else registerReceiver(receiver,filter)
        ui.post(render); refresh(); handleUsbAttach(intent)
    }
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        refresh()
        handleUsbAttach(intent)
    }
    @Suppress("DEPRECATION")
    private fun handleUsbAttach(intent: Intent) {
        if (intent.action != UsbManager.ACTION_USB_DEVICE_ATTACHED) return
        val announced = intent.getParcelableExtra<UsbDevice>(UsbManager.EXTRA_DEVICE) ?: return
        // Intents are notifications, never permission or identity evidence.
        val live = usb.deviceList[announced.deviceName] ?: return
        if (live.deviceId != announced.deviceId || !supported(live)) return
        if (usb.hasPermission(live)) connect(live, automatic = true)
        // The system may deliver its broadcast before granting the default app.
        // The subsequent Activity intent handles that case without another prompt.
    }
    private fun supported(device: UsbDevice) = device.vendorId == 0x303a && device.productId == 0x1001

    private fun refresh() {
        devices.removeAllViews()
        usb.deviceList.values.sortedBy {it.deviceName}.forEach {device ->
            devices.addView(Button(this).apply {
                text="%04x:%04x %s — Connect".format(device.vendorId,device.productId,device.deviceName)
                isEnabled=supported(device)
                setOnClickListener {
                    if (usb.hasPermission(device)) connect(device) else {
                        if (permissionPending != null) return@setOnClickListener
                        val request = PermissionRequest(device, ++nextPermissionRequest)
                        permissionPending = request
                        // Immutable callbacks retain our device/request snapshot. Android's
                        // fill-in extras are ignored; hasPermission remains authoritative.
                        val callback = PendingIntent.getBroadcast(this@MainActivity, request.id,
                            Intent(permissionAction).setPackage(packageName)
                                .putExtra(UsbManager.EXTRA_DEVICE, device).putExtra("request_id", request.id),
                            PendingIntent.FLAG_ONE_SHOT or PendingIntent.FLAG_IMMUTABLE)
                        try { usb.requestPermission(device, callback) }
                        catch (e: Exception) {
                            permissionPending = null
                            callback.cancel()
                            state("USB permission request failed: ${e.message}")
                        }
                    }
                }
            })
        }
        if (devices.childCount==0) devices.addView(TextView(this).apply { text="No USB device connected" })
    }
    private fun connect(device: UsbDevice, automatic: Boolean = false) {
        if (connecting && !automatic) return
        val http = settings.http()
        connecting = true
        worker.execute {
        if (destroyed) { connecting = false; return@execute }
        // Duplicate broadcast/Activity intents must not reopen the active USB fd.
        // Nor should auto-attach switch away from a different selected device.
        if (automatic && currentDevice != null) { connecting = false; return@execute }
        val live = usb.deviceList[device.deviceName]
        if (live == null || live.deviceId != device.deviceId || !supported(live) || !usb.hasPermission(live)) {
            connecting = false
            state("USB unavailable or permission required; select Connect")
            return@execute
        }
        disconnect(); state("Connecting…")
        try {
            val conn=usb.openDevice(device) ?: error("Android could not open device")
            connection=conn
            handle=Native.open(conn.fileDescriptor,cacheDir.absolutePath,http.lan,http.port,http.token)
            activeHttp=http
            currentDevice=device.deviceName
            val id=handle; val active=AtomicBoolean(true); running=active
            val consoleEvents = ConsoleEvents()
            reader=Thread({
                try {
                    while (active.get()) {
                        val batch=JSONObject(Native.poll(id)); val events=batch.getJSONArray("events")
                        for (i in 0 until events.length()) {
                            try { append(consoleEvents.render(events.getJSONObject(i))) }
                            catch (e: Exception) { append("[Invalid event skipped: ${e.message}]\n") }
                        }
                        Thread.sleep(30)
                    }
                } catch (e: Exception) {
                    if (active.get() && !destroyed) state("Event stream failed: ${e.message}")
                }
            },"idfr-events").also {it.start()}
            connected(true); state("Connected • HTTP ${if(http.lan) "LAN + token" else "loopback"}:${http.port}")
        } catch (e: Exception) { disconnect(); state("Connect failed: ${e.message}") }
        finally { connecting = false }
    } }
    private fun send(text: String) { worker.execute {
        try { check(handle!=0L){"Not connected"}; val result=JSONObject(Native.write(handle,text.toByteArray(Charsets.UTF_8)))
            append("[console accepted: ${result.opt("result")}]\n")
        } catch (e: Exception) { state("Console failed (not replayed): ${e.message}") }
    } }
    private fun application(method: String, params: String) { worker.execute {
        try { check(handle!=0L){"Not connected"}; val result=JSONObject(Native.application(handle,method,params))
            state("${if(method.isEmpty()) "Application connected" else method}: ${result.opt("result")}")
        } catch (e: Exception) { state("Application failed (not replayed): ${e.message}") }
    } }
    private fun disconnect() {
        connected(false); running.set(false); reader?.join(); reader=null
        if (handle!=0L) Native.close(handle)
        handle=0; activeHttp=null; connection?.close(); connection=null; currentDevice=null
    }
    override fun onDestroy() {
        destroyed=true; unregisterReceiver(receiver); ui.removeCallbacksAndMessages(null)
        worker.execute {disconnect()}; worker.shutdown(); super.onDestroy()
    }
}
