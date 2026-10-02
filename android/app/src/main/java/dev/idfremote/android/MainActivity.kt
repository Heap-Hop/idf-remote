package dev.idfremote.android

import android.app.Activity
import android.app.PendingIntent
import android.content.*
import android.hardware.usb.*
import android.os.*
import android.graphics.Typeface
import android.widget.*
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

object Native {
    init { System.loadLibrary("idf_remote_android") }
    @JvmStatic external fun open(fd: Int, cache: String): Long
    @JvmStatic external fun poll(id: Long): String
    @JvmStatic external fun write(id: Long, bytes: ByteArray): String
    @JvmStatic external fun application(id: Long, method: String, params: String): String
    @JvmStatic external fun close(id: Long)
}

/** Foreground example. Android owns permission/lifecycle; idf_remote owns all USB IO. */
class MainActivity : Activity() {
    private val usb by lazy { getSystemService(USB_SERVICE) as UsbManager }
    private val worker = Executors.newSingleThreadExecutor()
    private val ui = Handler(Looper.getMainLooper())
    private val pending = StringBuilder()
    private val connectedButtons = mutableListOf<Button>()
    private lateinit var log: TextView
    private lateinit var status: TextView
    private lateinit var devices: LinearLayout
    @Volatile private var destroyed = false
    private var connection: UsbDeviceConnection? = null // worker-owned
    private var handle = 0L
    private var currentDevice: String? = null
    private var reader: Thread? = null
    private var running = AtomicBoolean(false)
    private var permissionPending: String? = null // UI-owned
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
            if (text.isNotEmpty()) log.text = (log.text.toString()+text).takeLast(32768)
            if (!destroyed) ui.postDelayed(this,100)
        }
    }
    @Suppress("DEPRECATION")
    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val device = intent.getParcelableExtra<UsbDevice>(UsbManager.EXTRA_DEVICE)
            when (intent.action) {
                permissionAction -> {
                    if (device?.deviceName != permissionPending) return
                    permissionPending = null
                    if (device != null && usb.hasPermission(device)) connect(device)
                    else state("USB permission denied")
                }
                UsbManager.ACTION_USB_DEVICE_DETACHED -> {
                    if (device?.deviceName == permissionPending) permissionPending = null
                    worker.execute { if (currentDevice == device?.deviceName) { disconnect(); state("USB detached; reconnect explicitly") } }
                    refresh()
                }
                UsbManager.ACTION_USB_DEVICE_ATTACHED -> refresh()
            }
        }
    }
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(android.view.WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
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
            "Clear" to { synchronized(pending) { pending.setLength(0) }; log.text="" })
        val scroll = ScrollView(this)
        log = TextView(this).apply { typeface=Typeface.MONOSPACE; textSize=12f; setTextIsSelectable(true) }
        scroll.addView(log); root.addView(scroll,LinearLayout.LayoutParams(-1,0,1f))
        val input=EditText(this).apply { hint="Console input"; setSingleLine(true) }; root.addView(input)
        row("Send ↵" to { send(input.text.toString()+"\r") }, "Raw" to { send(input.text.toString()) },
            "Tab" to { send("\t") }, "Ctrl-C" to { send("\u0003") }, needsConnection=true)
        row("App connect" to { application("", "null") }, "Status" to { application("status","null") },
            "Echo" to { application("echo",JSONObject.quote(input.text.toString())) }, needsConnection=true)
        val method=EditText(this).apply { hint="Application method"; setSingleLine(true) }; root.addView(method)
        val params=EditText(this).apply { hint="JSON params (default null)"; setSingleLine(true) }; root.addView(params)
        row("Call" to { application(method.text.toString(),params.text.toString().ifBlank { "null" }) }, needsConnection=true)
        root.addView(TextView(this).apply { text="HTTP 127.0.0.1:38473 while connected • flash / monitor / application"; textSize=11f })
        setContentView(root)
        val filter=IntentFilter(permissionAction).apply { addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED); addAction(UsbManager.ACTION_USB_DEVICE_DETACHED) }
        if (Build.VERSION.SDK_INT>=33) registerReceiver(receiver,filter,Context.RECEIVER_NOT_EXPORTED) else registerReceiver(receiver,filter)
        ui.post(render); refresh()
    }
    private fun refresh() {
        devices.removeAllViews()
        usb.deviceList.values.sortedBy {it.deviceName}.forEach {device ->
            devices.addView(Button(this).apply {
                text="%04x:%04x %s — Connect".format(device.vendorId,device.productId,device.deviceName)
                isEnabled=device.vendorId==0x303a && device.productId==0x1001
                setOnClickListener {
                    if (usb.hasPermission(device)) connect(device) else {
                        permissionPending=device.deviceName
                        usb.requestPermission(device,PendingIntent.getBroadcast(this@MainActivity,0,
                            Intent(permissionAction).setPackage(packageName),PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE))
                    }
                }
            })
        }
        if (devices.childCount==0) devices.addView(TextView(this).apply { text="No USB device connected" })
    }
    private fun connect(device: UsbDevice) { worker.execute {
        if (destroyed) return@execute
        disconnect(); state("Connecting…")
        try {
            val conn=usb.openDevice(device) ?: error("Android could not open device")
            connection=conn
            handle=Native.open(conn.fileDescriptor,cacheDir.absolutePath)
            currentDevice=device.deviceName
            val id=handle; val active=AtomicBoolean(true); running=active
            reader=Thread({
                try {
                    while (active.get()) {
                        val batch=JSONObject(Native.poll(id)); val events=batch.getJSONArray("events")
                        for (i in 0 until events.length()) {
                            val event=events.getJSONObject(i); val kind=event.getString("kind"); val data=event.getJSONObject("data")
                            when (kind) {
                                "log" -> append(data.optString("text")+"\n")
                                "application_event", "application_connected", "application_disconnected", "reconnecting", "reconnected" -> append("[$kind $data]\n")
                            }
                        }
                        Thread.sleep(30)
                    }
                } catch (e: Exception) {
                    if (active.get() && !destroyed) state("Event stream failed: ${e.message}")
                }
            },"idfr-events").also {it.start()}
            connected(true); state("Connected • shared Rust worker + HTTP ready")
        } catch (e: Exception) { disconnect(); state("Connect failed: ${e.message}") }
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
        handle=0; connection?.close(); connection=null; currentDevice=null
    }
    override fun onDestroy() {
        destroyed=true; unregisterReceiver(receiver); ui.removeCallbacksAndMessages(null)
        worker.execute {disconnect()}; worker.shutdown(); super.onDestroy()
    }
}
