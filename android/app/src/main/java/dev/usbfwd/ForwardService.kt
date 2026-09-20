package dev.usbfwd

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log

/**
 * Holds the USB connection open and runs the native exporter.
 *
 * A foreground service with a wake lock is not decoration: without it Android
 * will freeze the process or kill it outright the moment the screen goes off,
 * and a controller that stops responding when the tablet dims is worse than no
 * controller at all.
 */
class ForwardService : Service() {

    companion object {
        const val EXTRA_DEVICE = "device"
        const val EXTRA_PORT = "port"
        const val EXTRA_PREFETCH = "prefetch"
        const val ACTION_STOP = "dev.usbfwd.STOP"
        private const val TAG = "usbfwd"
        private const val CHANNEL_ID = "forwarding"
        private const val NOTIFICATION_ID = 1

        fun start(
            context: Context,
            device: UsbDevice,
            port: Int = 3240,
            prefetch: Boolean = false,
        ) {
            val i = Intent(context, ForwardService::class.java).apply {
                putExtra(EXTRA_DEVICE, device)
                putExtra(EXTRA_PORT, port)
                putExtra(EXTRA_PREFETCH, prefetch)
            }
            context.startForegroundService(i)
        }

        fun stop(context: Context) {
            context.startService(
                Intent(context, ForwardService::class.java).setAction(ACTION_STOP)
            )
        }
    }

    private var connection: UsbDeviceConnection? = null
    private var claimed = mutableListOf<Int>()
    private var device: UsbDevice? = null
    private var wakeLock: PowerManager.WakeLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }

        @Suppress("DEPRECATION")
        val dev: UsbDevice? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent?.getParcelableExtra(EXTRA_DEVICE, UsbDevice::class.java)
        } else {
            intent?.getParcelableExtra(EXTRA_DEVICE)
        }
        if (dev == null) {
            Log.w(TAG, "started without a device")
            stopSelf()
            return START_NOT_STICKY
        }
        if (NativeBridge.runningPort() != 0) {
            Log.i(TAG, "already forwarding; ignoring a second start")
            return START_STICKY
        }

        val port = intent?.getIntExtra(EXTRA_PORT, 3240) ?: 3240
        val prefetch = intent?.getBooleanExtra(EXTRA_PREFETCH, false) ?: false
        startForeground(NOTIFICATION_ID, buildNotification(dev, port))

        return if (begin(dev, port, prefetch)) START_STICKY else { stopSelf(); START_NOT_STICKY }
    }

    private fun begin(dev: UsbDevice, port: Int, prefetch: Boolean): Boolean {
        val manager = getSystemService(Context.USB_SERVICE) as UsbManager
        val conn = manager.openDevice(dev)
        if (conn == null) {
            Log.e(TAG, "openDevice returned null — was permission granted?")
            return false
        }
        connection = conn
        device = dev

        // force = true detaches whatever kernel driver holds the interface.
        // It is the Android equivalent of USBDEVFS_DISCONNECT, and it needs no
        // root. Because the native side dups this same file description, the
        // claims made here are the claims the exporter sees.
        for (i in 0 until dev.interfaceCount) {
            val intf = dev.getInterface(i)
            if (conn.claimInterface(intf, true)) {
                claimed.add(i)
            } else {
                Log.w(TAG, "could not claim interface ${intf.id}")
            }
        }
        if (claimed.isEmpty()) {
            Log.e(TAG, "no interface could be claimed; giving up")
            cleanup()
            return false
        }

        val rc = NativeBridge.start(conn.fileDescriptor, port, bindAny = true, prefetch = prefetch)
        if (rc < 0) {
            Log.e(TAG, "exporter failed to start: ${NativeBridge.describe(rc)}")
            cleanup()
            return false
        }
        Log.i(TAG, "forwarding ${dev.deviceName} on port $rc (prefetch=$prefetch)")
        acquireWakeLock()
        if (rc != port) {
            // Bound somewhere else than asked; keep the notification honest.
            startForeground(NOTIFICATION_ID, buildNotification(dev, rc))
        }
        return true
    }

    private fun acquireWakeLock() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "usbfwd:forwarding").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun cleanup() {
        NativeBridge.stop()
        wakeLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
        val conn = connection
        val dev = device
        if (conn != null && dev != null) {
            for (i in claimed) {
                conn.releaseInterface(dev.getInterface(i))
            }
        }
        claimed.clear()
        // Closing last: the native exporter holds a dup of this descriptor and
        // stops before we get here, so there is nothing left to tear down.
        conn?.close()
        connection = null
        device = null
    }

    override fun onDestroy() {
        cleanup()
        super.onDestroy()
    }

    private fun buildNotification(dev: UsbDevice, port: Int): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL_ID) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    getString(R.string.channel_name),
                    NotificationManager.IMPORTANCE_LOW,
                ).apply { description = getString(R.string.channel_description) }
            )
        }
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val name = dev.productName ?: dev.deviceName
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.notification_title, name))
            .setContentText(getString(R.string.notification_text, port))
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setOngoing(true)
            .setContentIntent(open)
            .build()
    }
}
