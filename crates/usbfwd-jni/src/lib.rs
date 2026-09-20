//! The Android bridge.
//!
//! Deliberately, none of these functions touch `JNIEnv`. They take and return
//! only `jint`, so the shim needs no JNI bindings crate and no knowledge of
//! the `JNIEnv` vtable — the two things that make hand-written JNI fragile.
//! Everything the exporter needs to know about the device it learns from the
//! file descriptor itself: `readlink("/proc/self/fd/N")` gives the
//! `/dev/bus/usb/BBB/DDD` path that `UsbDevice.getDeviceName()` would have
//! reported, and `read()` on it gives the descriptors.
//!
//! Kotlin side:
//!
//! ```kotlin
//! object NativeBridge {
//!     init { System.loadLibrary("usbfwd") }
//!     external fun nativeStart(fd: Int, port: Int, bindAny: Int, prefetch: Int): Int
//!     external fun nativeStop(): Int
//!     external fun nativeIsRunning(): Int
//! }
//! ```

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use usb_backend::UsbBackend;
use usbfwd_common::log::Level;
use usbfwd_common::{error, info, log, netif};
use usbfwd_server::registry::Registry;
use usbfwd_server::{accept_loop, Bind, Config, SingleDevice};

/// Negative results from [`nativeStart`]. A success returns the bound port,
/// which is always positive.
pub const ERR_ALREADY_RUNNING: c_int = -1;
pub const ERR_BAD_DESCRIPTOR: c_int = -2;
pub const ERR_NOT_A_USB_DEVICE: c_int = -3;
pub const ERR_BIND_FAILED: c_int = -4;
pub const ERR_INTERNAL: c_int = -5;

struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    port: u16,
}

static STATE: Mutex<Option<Running>> = Mutex::new(None);

// ------------------------------------------------------------------- logging

#[cfg(target_os = "android")]
#[link(name = "log")]
extern "C" {
    fn __android_log_write(
        prio: c_int,
        tag: *const std::os::raw::c_char,
        text: *const std::os::raw::c_char,
    ) -> c_int;
}

#[cfg(target_os = "android")]
fn logcat(level: Level, msg: &str) {
    // ANDROID_LOG_{ERROR,WARN,INFO,DEBUG,VERBOSE}
    let prio = match level {
        Level::Error => 6,
        Level::Warn => 5,
        Level::Info => 4,
        Level::Debug => 3,
        Level::Trace => 2,
    };
    use std::os::raw::c_char;
    let tag = b"usbfwd\0";
    // A NUL inside a log line would truncate it; replacing is better than
    // dropping the message.
    let mut text: Vec<u8> = msg.replace('\0', "?").into_bytes();
    text.push(0);
    unsafe {
        __android_log_write(
            prio,
            tag.as_ptr() as *const c_char,
            text.as_ptr() as *const c_char,
        );
    }
}

#[cfg(not(target_os = "android"))]
fn logcat(level: Level, msg: &str) {
    eprintln!("[{}] {msg}", level.tag());
}

fn init_logging() {
    log::set_sink(logcat);
    log::from_env();
}

// -------------------------------------------------------------------- exports

/// Start exporting the device behind `fd`.
///
/// `fd` is `UsbDeviceConnection.getFileDescriptor()`. It is duplicated, so the
/// caller keeps ownership and must keep the `UsbDeviceConnection` open for as
/// long as the server runs.
///
/// Returns the bound TCP port on success, or one of the `ERR_*` constants.
#[no_mangle]
pub extern "C" fn Java_dev_usbfwd_NativeBridge_nativeStart(
    _env: *mut c_void,
    _class: *mut c_void,
    fd: c_int,
    port: c_int,
    bind_any: c_int,
    prefetch: c_int,
) -> c_int {
    init_logging();
    start(fd, port, bind_any != 0, prefetch != 0)
}

/// Stop the server and release the device. Idempotent.
#[no_mangle]
pub extern "C" fn Java_dev_usbfwd_NativeBridge_nativeStop(
    _env: *mut c_void,
    _class: *mut c_void,
) -> c_int {
    stop();
    0
}

#[no_mangle]
pub extern "C" fn Java_dev_usbfwd_NativeBridge_nativeIsRunning(
    _env: *mut c_void,
    _class: *mut c_void,
) -> c_int {
    match STATE.lock() {
        Ok(g) => g.as_ref().map(|r| r.port as c_int).unwrap_or(0),
        Err(_) => 0,
    }
}

// ----------------------------------------------------------------- the guts

/// The same logic as the exported entry points, callable from Rust tests.
pub fn start(fd: c_int, port: c_int, bind_any: bool, prefetch: bool) -> c_int {
    let mut state = match STATE.lock() {
        Ok(g) => g,
        Err(_) => return ERR_INTERNAL,
    };
    if state.is_some() {
        return ERR_ALREADY_RUNNING;
    }

    let dev = match usb_backend::UsbfsDevice::from_borrowed_fd(fd) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            error!("cannot adopt descriptor {fd}: {e}");
            return if e.kind() == std::io::ErrorKind::InvalidData {
                ERR_NOT_A_USB_DEVICE
            } else {
                ERR_BAD_DESCRIPTOR
            };
        }
    };

    let summary = dev.summary().clone();
    let (vid, pid) = summary.vid_pid();
    info!(
        "exporting {} {vid:04x}:{pid:04x} with {} interfaces",
        summary.busid,
        summary.interfaces.len()
    );

    // Prefer the tailnet address, exactly as on Linux — but add loopback,
    // which on Android is not redundant with it.
    //
    // Tailscale there is a VpnService running gVisor netstack in userspace,
    // not a kernel TUN with routes. An inbound tailnet connection is
    // terminated inside the Tailscale app and re-dialled to 127.0.0.1, so it
    // never reaches a socket bound to the tun address — even though that
    // address shows up in getifaddrs like any other, which is what makes this
    // look fine from the outside. `tailscale_addrs()` filters loopback out
    // deliberately, so without these two the listener logs "listening on
    // 100.x.y.z:3240" and then refuses every connection from the tailnet.
    //
    // This is not the exposure `Bind::Any` is: the app sandbox keeps loopback
    // reachable only from this device, and Tailscale's forwarder is on it.
    let mut binds = vec![
        Bind::Tailscale,
        Bind::Addr(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        Bind::Addr(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ];
    if netif::tailscale_addrs()
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        if !bind_any {
            error!("no tailnet address and bindAny is false; refusing to expose USB traffic");
            return ERR_BIND_FAILED;
        }
        binds = vec![Bind::Any];
    }

    let cfg = Config {
        binds,
        port: if port > 0 && port <= u16::MAX as c_int {
            port as u16
        } else {
            usbfwd_server::Config::default().port
        },
        prefetch,
        ..Default::default()
    };

    let listeners = match usbfwd_server::bind_all(&cfg) {
        Ok(l) => l,
        Err(e) => {
            error!("cannot bind: {e}");
            return ERR_BIND_FAILED;
        }
    };
    let bound = listeners
        .first()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(cfg.port);

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let source = Arc::new(SingleDevice::new(Arc::clone(&dev)));
    let thread = std::thread::Builder::new()
        .name("usbfwd-accept".into())
        .spawn(move || {
            accept_loop(
                listeners,
                source,
                &Registry::new(),
                Arc::new(move || flag.load(Ordering::Relaxed)),
                prefetch,
            );
            // Dropping the last reference here releases the interfaces and
            // closes our duplicate of the descriptor.
            drop(dev);
            info!("exporter stopped");
        });

    match thread {
        Ok(t) => {
            *state = Some(Running {
                stop,
                thread: Some(t),
                port: bound,
            });
            bound as c_int
        }
        Err(e) => {
            error!("cannot spawn the accept thread: {e}");
            ERR_INTERNAL
        }
    }
}

pub fn stop() {
    let running = match STATE.lock() {
        Ok(mut g) => g.take(),
        Err(_) => return,
    };
    let Some(mut r) = running else { return };
    r.stop.store(true, Ordering::Relaxed);
    if let Some(t) = r.thread.take() {
        let _ = t.join();
    }
}

pub fn running_port() -> Option<u16> {
    STATE.lock().ok().and_then(|g| g.as_ref().map(|r| r.port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_running_to_begin_with() {
        assert_eq!(running_port(), None);
        // Stopping when idle must be a no-op, not a panic: the Android
        // service calls it from onDestroy regardless of state.
        stop();
        stop();
        assert_eq!(running_port(), None);
    }

    #[test]
    fn a_descriptor_that_is_not_a_usb_device_is_rejected() {
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        use std::os::fd::AsRawFd;
        let rc = start(f.as_raw_fd(), 0, true, false);
        assert!(rc < 0, "expected an error code, got {rc}");
        assert_eq!(running_port(), None, "a failed start must leave no state");
    }

    #[test]
    fn a_closed_descriptor_is_rejected() {
        let rc = start(-1, 0, true, false);
        assert_eq!(rc, ERR_BAD_DESCRIPTOR);
    }
}
