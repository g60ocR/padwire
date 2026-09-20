package dev.usbfwd

/**
 * The Rust exporter.
 *
 * Every signature here is ints only. The native side never touches `JNIEnv`,
 * which is what lets it be written without a JNI bindings crate: it recovers
 * the device path from `/proc/self/fd/<fd>` and the descriptors by reading the
 * usbfs node, so a file descriptor is genuinely all it needs.
 */
object NativeBridge {
    init {
        System.loadLibrary("usbfwd")
    }

    /** Returned by [start] when the exporter is already running. */
    const val ERR_ALREADY_RUNNING = -1
    const val ERR_BAD_DESCRIPTOR = -2
    const val ERR_NOT_A_USB_DEVICE = -3
    const val ERR_BIND_FAILED = -4
    const val ERR_INTERNAL = -5

    private external fun nativeStart(fd: Int, port: Int, bindAny: Int): Int
    private external fun nativeStop(): Int
    private external fun nativeIsRunning(): Int

    /**
     * Start exporting the device behind [fd].
     *
     * The descriptor is duplicated natively, so the caller keeps ownership —
     * but the `UsbDeviceConnection` it came from must stay open for as long as
     * the exporter runs, or the kernel will tear the device state down.
     *
     * @return the bound TCP port, or a negative `ERR_*` value.
     */
    fun start(fd: Int, port: Int = 3240, bindAny: Boolean = true): Int =
        nativeStart(fd, port, if (bindAny) 1 else 0)

    fun stop() {
        nativeStop()
    }

    /** The port currently bound, or 0 if the exporter is not running. */
    fun runningPort(): Int = nativeIsRunning()

    fun describe(code: Int): String = when (code) {
        ERR_ALREADY_RUNNING -> "already running"
        ERR_BAD_DESCRIPTOR -> "the USB file descriptor was rejected"
        ERR_NOT_A_USB_DEVICE -> "that descriptor is not a USB device node"
        ERR_BIND_FAILED -> "could not bind a listening socket — is Tailscale up?"
        ERR_INTERNAL -> "internal error; check logcat for the tag usbfwd"
        else -> "error $code"
    }
}
