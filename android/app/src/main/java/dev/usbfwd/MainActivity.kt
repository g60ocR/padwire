package dev.usbfwd

import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.widget.Button
import android.widget.CheckBox
import android.widget.LinearLayout
import android.widget.TextView

/**
 * A deliberately plain screen: pick the device, grant permission, start the
 * service. The interesting parts are the permission dance and the auto-launch
 * on attach, both of which exist so that plugging the puck in is the whole
 * user interaction.
 */
class MainActivity : Activity() {

    companion object {
        private const val TAG = "usbfwd"
        private const val ACTION_PERMISSION = "dev.usbfwd.USB_PERMISSION"
        private const val VALVE_VENDOR_ID = 0x28de
        private const val PORT = 3240

        /// Long enough for startForegroundService to have run begin().
        private const val START_SETTLE_MS = 1000L
    }

    private lateinit var status: TextView
    private lateinit var toggle: Button
    private lateinit var prefetchBox: CheckBox

    /// Set between asking the service to start and the native port appearing.
    ///
    /// `startForegroundService` is asynchronous, so `runningPort()` still
    /// reads 0 for a moment afterwards. Refreshing straight away would leave
    /// the button saying "Start" when forwarding is already coming up, and the
    /// obvious response to that — tap it again — would stop what was just
    /// started.
    private var starting = false

    private val permissionReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != ACTION_PERMISSION) return
            @Suppress("DEPRECATION")
            val device: UsbDevice? =
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
                } else {
                    intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
                }
            val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
            if (granted && device != null) {
                beginStarting(device)
            } else {
                Log.w(TAG, "USB permission refused")
                refresh()
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        status = TextView(this).apply { setPadding(0, 0, 0, 48) }
        toggle = Button(this).apply { setOnClickListener { onToggle() } }
        prefetchBox = CheckBox(this).apply {
            text = getString(R.string.prefetch_label)
            isChecked = false
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(64, 64, 64, 64)
            addView(status)
            addView(prefetchBox)
            addView(toggle)
        })

        val filter = IntentFilter(ACTION_PERMISSION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(permissionReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(permissionReceiver, filter)
        }

        // Launched by the USB_DEVICE_ATTACHED intent filter: start immediately.
        handleAttachIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleAttachIntent(intent)
    }

    private fun handleAttachIntent(intent: Intent?) {
        if (intent?.action != UsbManager.ACTION_USB_DEVICE_ATTACHED) return
        @Suppress("DEPRECATION")
        val device: UsbDevice? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
        } else {
            intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
        }
        // Attaching via the intent filter already grants permission for this
        // device, so the service can start without a prompt. The filter is
        // vendor-wide so a new controller needs no edit to be recognised,
        // which means plugging in a dock lands here too — forward only what
        // has something to forward.
        if (device == null) return
        if (!device.isForwardable()) {
            Log.i(TAG, "ignoring %04x:%04x — no HID interface".format(device.vendorId, device.productId))
            refresh()
            return
        }
        ForwardService.start(this, device, PORT)
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    override fun onDestroy() {
        unregisterReceiver(permissionReceiver)
        super.onDestroy()
    }

    private fun onToggle() {
        if (starting) return
        if (NativeBridge.runningPort() != 0) {
            ForwardService.stop(this)
            refresh()
            return
        }
        val manager = getSystemService(Context.USB_SERVICE) as UsbManager
        val device = manager.deviceList.values
            .filter { it.vendorId == VALVE_VENDOR_ID && it.isForwardable() }
            .minByOrNull { it.deviceId }
        if (device == null) {
            val valve = manager.deviceList.values.filter { it.vendorId == VALVE_VENDOR_ID }
            status.text = if (valve.isEmpty()) {
                "No Valve device is attached."
            } else {
                // Almost always a dock: a Steam Deck dock presents a USB
                // billboard (28de:2001) on the same vendor id as the
                // controller, with no HID interface to forward.
                "No forwardable Valve device.\n\nSkipped:\n" + valve.joinToString("\n") {
                    "  %04x:%04x  no HID interface".format(it.vendorId, it.productId)
                }
            }
            return
        }
        if (manager.hasPermission(device)) {
            beginStarting(device)
        } else {
            manager.requestPermission(
                device,
                PendingIntent.getBroadcast(
                    this,
                    0,
                    Intent(ACTION_PERMISSION).setPackage(packageName),
                    PendingIntent.FLAG_IMMUTABLE,
                ),
            )
        }
    }

    /// Ask the service to start and hold the UI in a "starting" state until
    /// the native port shows up, so the button never invites a second tap
    /// that would stop what the first one started.
    private fun beginStarting(device: UsbDevice) {
        val prefetch = prefetchBox.isChecked
        ForwardService.start(this, device, PORT, prefetch)
        starting = true
        status.text = "Starting %04x:%04x%s…".format(
            device.vendorId,
            device.productId,
            if (prefetch) " with prefetch" else "",
        )
        toggle.text = "Stop"
        toggle.postDelayed({
            starting = false
            refresh()
        }, START_SETTLE_MS)
    }

    /// Worth forwarding only if it actually has a HID interface.
    ///
    /// Valve's vendor id covers more than controllers: a Steam Deck dock
    /// presents a USB billboard (`28de:2001`) that shares the vendor id and
    /// has nothing to forward. Matching on the interface class rather than a
    /// list of product ids keeps new controllers working without an edit,
    /// while never picking a dock, a hub or a charger.
    private fun UsbDevice.isForwardable(): Boolean =
        (0 until interfaceCount).any {
            getInterface(it).interfaceClass == UsbConstants.USB_CLASS_HID
        }

    private fun refresh() {
        val port = NativeBridge.runningPort()
        if (port != 0) {
            status.text = "Forwarding on port $port.\n\n" +
                "On the host:\n  usbip attach -r <this tablet> -b <busid>\n" +
                "or let usbfwd-attach do it."
            toggle.text = "Stop"
            prefetchBox.isEnabled = false
            return
        }
        val manager = getSystemService(Context.USB_SERVICE) as UsbManager
        val devices = manager.deviceList.values
        status.text = if (devices.isEmpty()) {
            "Not forwarding. Plug in the controller or the Proteus puck."
        } else {
            "Not forwarding.\n\nAttached (* = would be forwarded):\n" +
                devices.joinToString("\n") {
                    val eligible = it.vendorId == VALVE_VENDOR_ID && it.isForwardable()
                    "%s %04x:%04x  %s".format(
                        if (eligible) "*" else " ",
                        it.vendorId,
                        it.productId,
                        it.productName ?: it.deviceName,
                    )
                }
        }
        toggle.text = "Start"
        prefetchBox.isEnabled = true
    }
}
