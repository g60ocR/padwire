//! The usbfs backend.
//!
//! Lifetime rules for an in-flight URB, since they drive most of the `unsafe`
//! here: between `USBDEVFS_SUBMITURB` and `USBDEVFS_REAPURB*` the kernel holds
//! the userspace address of our `struct usbdevfs_urb` and, for IN transfers,
//! of the data buffer. It writes to neither outside of those two ioctls —
//! `devio.c` keeps its own kmalloc'd transfer buffer and copies into ours in
//! `processcompl()`, during the reap. So the allocation must stay put until
//! reaped, and once the file descriptor is closed no reap can ever happen and
//! any stragglers are safe to free.
//!
//! Every raw pointer below is dereferenced only while `State`'s mutex is held,
//! and exactly one allocation exists per in-flight `seqnum`.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::raw::{c_int, c_uint, c_void};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use usbfwd_common::{debug, warn};
use usbip_proto::pdu::{transfer_flags as tf, Direction};

use crate::descriptors::{self, Descriptors, TransferType};
use crate::enumerate;
use crate::sys::*;
use crate::{
    DeviceSummary, InterfaceSummary, Speed, SubmitError, UrbCompletion, UrbRequest, UsbBackend,
    Wake,
};

/// A HID gamepad's largest transfer is a report descriptor of a few hundred
/// bytes. One megabyte is far more than enough and keeps a hostile
/// `transfer_buffer_length` from turning into an allocation.
pub const DEFAULT_MAX_TRANSFER: usize = 1 << 20;

const SETUP_LEN: usize = 8;

/// See [`UsbfsDevice::reconnect_one`].
const RECONNECT_ATTEMPTS: u32 = 4;
const RECONNECT_BACKOFF: Duration = Duration::from_millis(50);

#[repr(C)]
struct PendingUrb {
    /// First field on purpose: `USBDEVFS_REAPURB` hands back exactly this
    /// address, and `usercontext` points here too.
    urb: UsbdevfsUrb,
    buffer: Vec<u8>,
    seqnum: u32,
    dir: Direction,
    /// Control transfers keep the setup packet at the head of `buffer`, so
    /// their payload starts 8 bytes in.
    data_offset: usize,
    /// The client's `transfer_buffer_length`: never hand back more than this.
    max_reply: usize,
}

#[derive(Clone, Copy)]
struct UrbPtr(*mut PendingUrb);

// Safety: see the module comment. The pointer is owned by exactly one `Entry`
// and only touched under the mutex.
unsafe impl Send for UrbPtr {}

struct Entry {
    ptr: UrbPtr,
    /// Set by `unlink`. The client has already been told the URB was
    /// cancelled, so its completion must not also be reported.
    unlinked: bool,
}

#[derive(Default)]
struct State {
    pending: HashMap<u32, Entry>,
    claimed: Vec<u8>,
}

pub struct UsbfsDevice {
    /// `Option` only so that `Drop` can close it before freeing URB buffers.
    fd: Option<OwnedFd>,
    summary: DeviceSummary,
    descriptors: Descriptors,
    /// Endpoint address (direction bit included) to usbfs URB type.
    ep_types: HashMap<u8, u8>,
    caps: u32,
    max_transfer: usize,
    state: Mutex<State>,
}

impl UsbfsDevice {
    /// Open `/dev/bus/usb/BBB/DDD`.
    pub fn open_node(node_path: &Path) -> io::Result<UsbfsDevice> {
        let (busnum, devnum) = enumerate::split_node_path(node_path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a /dev/bus/usb/BBB/DDD path", node_path.display()),
            )
        })?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node_path)
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "opening {} for read/write: {e} \
                         (install packaging/99-usbfwd.rules, or run as root)",
                        node_path.display()
                    ),
                )
            })?;
        UsbfsDevice::build(OwnedFd::from(file), busnum, devnum)
    }

    /// Resolve a sysfs bus id such as `1-2.3` and open it.
    pub fn open_busid(busid: &str) -> io::Result<UsbfsDevice> {
        let (busnum, devnum) = enumerate::numbers_for_busid(busid).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no such device: {busid}"))
        })?;
        UsbfsDevice::open_node(&enumerate::node_path(busnum, devnum))
    }

    /// Adopt a descriptor obtained elsewhere — on Android, the one behind
    /// `UsbDeviceConnection.getFileDescriptor()`.
    ///
    /// The descriptor is duplicated, so closing this device does not close the
    /// caller's connection.
    pub fn from_borrowed_fd(fd: RawFd) -> io::Result<UsbfsDevice> {
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = unsafe { OwnedFd::from_raw_fd(dup) };
        // Android's UsbDevice.getDeviceName() is the usbfs path, and the
        // kernel will tell us the same thing through /proc/self/fd.
        let (busnum, devnum) = enumerate::numbers_for_fd(owned.as_raw_fd()).unwrap_or((0, 0));
        UsbfsDevice::build(owned, busnum, devnum)
    }

    fn build(fd: OwnedFd, busnum: u32, devnum: u32) -> io::Result<UsbfsDevice> {
        let raw = fd.as_raw_fd();
        let blob = read_descriptor_blob(raw)?;
        let descriptors = descriptors::parse(&blob)?;

        let busid =
            enumerate::busid_for(busnum, devnum).unwrap_or_else(|| format!("{busnum}-{devnum}"));
        let node_path = enumerate::node_path(busnum, devnum)
            .to_string_lossy()
            .into_owned();
        let sys_path = enumerate::sys_path(&busid)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| node_path.clone());

        let config_value = active_config(raw, &busid, &descriptors);
        let config = descriptors.config(config_value).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "device reports active configuration {config_value}, which it did not describe"
                ),
            )
        })?;

        let speed = get_speed(raw)
            .or_else(|| enumerate::sysfs_speed(&busid))
            .unwrap_or(Speed::Unknown);

        let interfaces = InterfaceSummary::list_from(config);

        // Every alternate setting, not just the primary ones: the client may
        // issue SET_INTERFACE and start using endpoints only the alt exposes.
        let mut ep_types = HashMap::new();
        ep_types.insert(0x00u8, USBDEVFS_URB_TYPE_CONTROL);
        ep_types.insert(0x80u8, USBDEVFS_URB_TYPE_CONTROL);
        for alt in &config.alt_settings {
            for ep in &alt.endpoints {
                let t = match ep.transfer_type() {
                    TransferType::Control => USBDEVFS_URB_TYPE_CONTROL,
                    TransferType::Isochronous => USBDEVFS_URB_TYPE_ISO,
                    TransferType::Bulk => USBDEVFS_URB_TYPE_BULK,
                    TransferType::Interrupt => USBDEVFS_URB_TYPE_INTERRUPT,
                };
                ep_types.insert(ep.address, t);
            }
        }

        let summary = DeviceSummary {
            busnum,
            devnum,
            busid,
            sys_path,
            node_path,
            speed,
            device: descriptors.device,
            config_value,
            interfaces,
        };

        Ok(UsbfsDevice {
            fd: Some(fd),
            summary,
            descriptors,
            ep_types,
            caps: get_capabilities(raw),
            max_transfer: DEFAULT_MAX_TRANSFER,
            state: Mutex::new(State::default()),
        })
    }

    pub fn with_max_transfer(mut self, n: usize) -> Self {
        self.max_transfer = n;
        self
    }

    pub fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    pub fn capabilities(&self) -> u32 {
        self.caps
    }

    pub fn descriptors(&self) -> &Descriptors {
        &self.descriptors
    }

    pub fn interface_numbers(&self) -> Vec<u8> {
        self.descriptors
            .config(self.summary.config_value)
            .map(|c| c.interface_numbers())
            .unwrap_or_default()
    }

    fn raw(&self) -> RawFd {
        self.fd.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1)
    }

    /// Evict the bound driver and claim one interface.
    ///
    /// `USBDEVFS_DISCONNECT_CLAIM` with no flags does both in one step and
    /// without a window where another process could grab the interface.
    /// Kernels too old for it (pre-3.4) get the two-ioctl equivalent.
    fn claim_one(&self, ifnum: u8) -> io::Result<()> {
        let mut dc = UsbdevfsDisconnectClaim {
            interface: ifnum as c_uint,
            flags: 0,
            driver: [0; MAXDRIVERNAME + 1],
        };
        let rc = unsafe { libc::ioctl(self.raw(), USBDEVFS_DISCONNECT_CLAIM as _, &mut dc) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOTTY) | Some(libc::EINVAL) => {}
            _ => return Err(err),
        }

        let mut cmd = UsbdevfsIoctl {
            ifno: ifnum as c_int,
            ioctl_code: USBDEVFS_DISCONNECT as c_int,
            data: std::ptr::null_mut(),
        };
        let rc = unsafe { libc::ioctl(self.raw(), USBDEVFS_IOCTL as _, &mut cmd) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            // ENODATA just means nothing was bound in the first place.
            if e.raw_os_error() != Some(libc::ENODATA) {
                return Err(e);
            }
        }
        let n = ifnum as c_uint;
        let rc = unsafe { libc::ioctl(self.raw(), USBDEVFS_CLAIMINTERFACE as _, &n) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn release_one(&self, ifnum: u8) {
        let n = ifnum as c_uint;
        unsafe {
            libc::ioctl(self.raw(), USBDEVFS_RELEASEINTERFACE as _, &n);
        }
    }

    /// Ask the kernel to rebind a driver to an interface we have let go of, so
    /// the controller works locally again once the session ends. Releasing
    /// does not do this on its own — `usb_driver_release_interface` only
    /// detaches.
    ///
    /// `USBDEVFS_CONNECT` runs `device_attach`, which runs the driver's probe
    /// synchronously. For a HID interface that probe fetches a report
    /// descriptor, and firing seven of them back to back at a full-speed
    /// device is enough to make the later ones time out. Hence the retries:
    /// they cost nothing in the common case and are the difference between
    /// "the controller works again" and "replug it".
    fn reconnect_one(&self, ifnum: u8) -> io::Result<c_int> {
        let mut last = io::Error::from_raw_os_error(libc::EAGAIN);
        for attempt in 0..RECONNECT_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(RECONNECT_BACKOFF);
            }
            let mut cmd = UsbdevfsIoctl {
                ifno: ifnum as c_int,
                ioctl_code: USBDEVFS_CONNECT as c_int,
                data: std::ptr::null_mut(),
            };
            let rc = unsafe { libc::ioctl(self.raw(), USBDEVFS_IOCTL as _, &mut cmd) };
            if rc >= 0 {
                // device_attach returns 1 when a driver bound, 0 when none
                // matched. Zero is not an error — a vendor-specific interface
                // may legitimately have no driver — but it is worth seeing.
                debug!("interface {ifnum}: rebind returned {rc}");
                return Ok(rc);
            }
            let e = io::Error::last_os_error();
            // EBUSY means something already bound it, which is the outcome
            // we wanted anyway — typically a multi-interface driver that
            // claimed this one from a sibling's probe.
            if e.raw_os_error() == Some(libc::EBUSY) {
                debug!("interface {ifnum}: already rebound by another driver's probe");
                return Ok(0);
            }
            debug!(
                "interface {ifnum}: rebind attempt {} failed: {e}",
                attempt + 1
            );
            last = e;
        }
        Err(last)
    }

    fn urb_type_for(&self, ep: u8, dir: Direction) -> Result<(u8, u8), SubmitError> {
        let addr = if dir.is_in() { ep | 0x80 } else { ep & 0x7f };
        if ep == 0 {
            return Ok((addr, USBDEVFS_URB_TYPE_CONTROL));
        }
        match self.ep_types.get(&addr) {
            Some(&USBDEVFS_URB_TYPE_ISO) => {
                // Deliberate: isochronous is the expensive half of a general
                // USB/IP server and no HID device has an iso endpoint.
                Err(SubmitError::Urb(-libc::EOPNOTSUPP))
            }
            Some(&t) => Ok((addr, t)),
            None => Err(SubmitError::Urb(-libc::EPIPE)),
        }
    }

    /// Carry out the standard requests that change state the kernel tracks on
    /// our behalf, instead of passing them down as raw control URBs.
    ///
    /// This is what `tweak_special_requests()` does in the in-tree server
    /// (`drivers/usb/usbip/stub_rx.c`), and skipping it is subtly fatal. The
    /// device acts on a raw SET_CONFIGURATION — it resets every endpoint's
    /// data toggle to DATA0 — but the exporting kernel never sees it happen
    /// and keeps its own toggles where they were. If a kernel driver was
    /// bound before usbfwd claimed the device (on Android, `usbhid` always
    /// is), its toggle for the interrupt IN endpoint is already advanced, and
    /// after the mismatch that endpoint never completes another URB: the
    /// device's packets are discarded as duplicates. Control and OUT traffic
    /// carry on working, so the device looks healthy while input is dead.
    ///
    /// Returning `Some` means no URB was submitted and the caller owes the
    /// client a completion.
    #[allow(clippy::needless_return)]
    fn tweak_special_request(&self, req: &UrbRequest) -> Option<Result<(), SubmitError>> {
        let special = Special::classify(&req.setup)?;
        let outcome = match special {
            // vhci assigns addresses on its side. The exporting kernel owns
            // the real one, so acknowledge and do nothing.
            Special::SetAddress(addr) => {
                debug!("{}: SET_ADDRESS({addr}) ignored", self.summary.busid);
                Ok(())
            }
            Special::SetConfiguration(cfg) => {
                debug!("{}: SET_CONFIGURATION({cfg})", self.summary.busid);
                self.ioctl_u32(USBDEVFS_SETCONFIGURATION, cfg as u32)
            }
            Special::SetInterface { interface, alt } => {
                debug!(
                    "{}: SET_INTERFACE({interface}, alt {alt})",
                    self.summary.busid
                );
                let mut arg = UsbdevfsSetinterface {
                    interface: interface as c_uint,
                    altsetting: alt as c_uint,
                };
                let rc =
                    unsafe { libc::ioctl(self.raw(), USBDEVFS_SETINTERFACE as _, &mut arg) };
                if rc < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
            // USBDEVFS_CLEAR_HALT resets the kernel's data toggle as well as
            // clearing the stall, which a raw control transfer would not.
            Special::ClearHalt(ep) => {
                debug!("{}: CLEAR_HALT(ep {ep:#04x})", self.summary.busid);
                self.ioctl_u32(USBDEVFS_CLEAR_HALT, ep as u32)
            }
        };

        // Answer success even when the ioctl failed, matching the in-tree
        // server, whose tweaks all `return 0` after logging. The failure that
        // matters is SET_CONFIGURATION: usbfs refuses it with -EBUSY while any
        // interface is claimed, which is always — claiming them is how we took
        // the device. Reporting that back fails the URB, and the importing
        // kernel gives up with "can't set config #1, error -16" and drops the
        // device before it is ever usable.
        //
        // Swallowing it is also the correct outcome for the toggles. The bug
        // was never that they need resetting; it was that forwarding the
        // request raw reset the *device's* toggles while the exporting
        // kernel's stayed put. Doing neither leaves both sides as they were,
        // which is consistent.
        Some(match outcome {
            Ok(()) => Err(SubmitError::Handled(0)),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENODEV) | Some(libc::ESHUTDOWN)) => {
                Err(SubmitError::Fatal(e))
            }
            Err(e) => {
                warn!(
                    "{}: {special:?} failed ({e}); answering success",
                    self.summary.busid
                );
                Err(SubmitError::Handled(0))
            }
        })
    }

    fn ioctl_u32(&self, code: u32, arg: u32) -> io::Result<()> {
        let mut arg = arg as c_uint;
        let rc = unsafe { libc::ioctl(self.raw(), code as _, &mut arg) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn usbfs_flags(&self, req: &UrbRequest, urb_type: u8) -> c_uint {
        let mut flags: c_uint = 0;
        if req.dir.is_in() && req.transfer_flags & tf::SHORT_NOT_OK != 0 {
            flags |= USBDEVFS_URB_SHORT_NOT_OK;
        }
        if !req.dir.is_in()
            && urb_type == USBDEVFS_URB_TYPE_BULK
            && req.transfer_flags & tf::ZERO_PACKET != 0
            && self.caps & USBDEVFS_CAP_ZERO_PACKET != 0
        {
            flags |= USBDEVFS_URB_ZERO_PACKET;
        }
        if req.transfer_flags & tf::NO_INTERRUPT != 0 {
            flags |= USBDEVFS_URB_NO_INTERRUPT;
        }
        flags
    }
}

impl UsbBackend for UsbfsDevice {
    fn summary(&self) -> &DeviceSummary {
        &self.summary
    }

    fn claim_all(&self) -> io::Result<()> {
        let mut st = self.state.lock().unwrap();
        for ifnum in self.interface_numbers() {
            if st.claimed.contains(&ifnum) {
                continue;
            }
            self.claim_one(ifnum).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("claiming interface {ifnum} of {}: {e}", self.summary.busid),
                )
            })?;
            st.claimed.push(ifnum);
        }
        Ok(())
    }

    fn release_all(&self) {
        let mut st = self.state.lock().unwrap();
        let claimed = std::mem::take(&mut st.claimed);
        if claimed.is_empty() {
            return;
        }
        debug!("{}: releasing interfaces {claimed:?}", self.summary.busid);

        // Two passes, and the order is load-bearing. A multi-interface driver
        // claims its extra interfaces during probe: cdc_acm binds the
        // Communications interface and then calls usb_driver_claim_interface
        // for the CDC Data one. Rebinding interface 0 while interface 1 is
        // still held by usbfs makes that claim fail with -EBUSY, the probe
        // fails, and the device is left with no driver at all until it is
        // physically replugged. The Proteus puck has exactly this shape —
        // interfaces 0 and 1 are its internal comms pair — so releasing
        // everything before reconnecting anything is what keeps the controller
        // working locally after a session ends.
        for ifnum in &claimed {
            self.release_one(*ifnum);
        }
        for ifnum in &claimed {
            if let Err(e) = self.reconnect_one(*ifnum) {
                warn!(
                    "{}: interface {ifnum} could not be handed back to a kernel driver: {e}. \
                     It will work again after a replug.",
                    self.summary.busid
                );
            }
        }
        debug!("{}: released", self.summary.busid);
    }

    fn submit(&self, req: UrbRequest) -> Result<(), SubmitError> {
        let (ep_addr, urb_type) = self.urb_type_for(req.ep, req.dir)?;
        let is_control = urb_type == USBDEVFS_URB_TYPE_CONTROL;

        if is_control {
            if let Some(r) = self.tweak_special_request(&req) {
                return r;
            }
        }

        // For control transfers the kernel re-reads the length from the setup
        // packet's wLength and requires our buffer to cover it, so size on
        // whichever of the two is larger rather than trusting one of them.
        let (payload_len, data_offset) = if is_control {
            let w_length = u16::from_le_bytes([req.setup[6], req.setup[7]]) as usize;
            (req.buffer_length.max(w_length), SETUP_LEN)
        } else {
            (req.buffer_length, 0)
        };
        if payload_len > self.max_transfer {
            return Err(SubmitError::Urb(-libc::EINVAL));
        }

        let total = data_offset + payload_len;
        // Always keep a real allocation behind urb.buffer, even for a
        // zero-length transfer, so the pointer handed to the kernel is valid.
        let mut buffer = vec![0u8; total.max(1)];
        if is_control {
            buffer[..SETUP_LEN].copy_from_slice(&req.setup);
        }
        if !req.dir.is_in() && !req.data.is_empty() {
            let n = req.data.len().min(payload_len);
            buffer[data_offset..data_offset + n].copy_from_slice(&req.data[..n]);
        }

        let flags = self.usbfs_flags(&req, urb_type);
        let seqnum = req.seqnum;

        let mut st = self.state.lock().unwrap();
        if st.pending.contains_key(&seqnum) {
            // A client reusing a live seqnum would make completions
            // ambiguous; refuse rather than lose track of an allocation.
            return Err(SubmitError::Urb(-libc::EBUSY));
        }

        let boxed = Box::new(PendingUrb {
            urb: UsbdevfsUrb {
                typ: urb_type,
                endpoint: ep_addr,
                flags,
                buffer_length: total as c_int,
                ..Default::default()
            },
            buffer,
            seqnum,
            dir: req.dir,
            data_offset,
            max_reply: req.buffer_length,
        });
        let raw = Box::into_raw(boxed);

        // Safety: `raw` is a fresh, uniquely owned allocation. It is registered
        // in `st.pending` only if the kernel accepts it, and freed here if not.
        unsafe {
            (*raw).urb.buffer = (*raw).buffer.as_mut_ptr() as *mut c_void;
            (*raw).urb.usercontext = raw as *mut c_void;
            let rc = libc::ioctl(self.raw(), USBDEVFS_SUBMITURB as _, &mut (*raw).urb);
            if rc < 0 {
                let err = io::Error::last_os_error();
                drop(Box::from_raw(raw));
                return Err(match err.raw_os_error() {
                    Some(libc::ENODEV) | Some(libc::ESHUTDOWN) => SubmitError::Fatal(err),
                    Some(e) => SubmitError::Urb(-e),
                    None => SubmitError::Fatal(err),
                });
            }
        }
        st.pending.insert(
            seqnum,
            Entry {
                ptr: UrbPtr(raw),
                unlinked: false,
            },
        );
        Ok(())
    }

    fn unlink(&self, seqnum: u32) -> io::Result<bool> {
        // The lock is held across the ioctl on purpose: it is the only thing
        // keeping the reaper from freeing the allocation under us.
        let mut st = self.state.lock().unwrap();
        let Some(entry) = st.pending.get_mut(&seqnum) else {
            return Ok(false);
        };
        // Set this whether or not the discard lands. Either way the client has
        // been told the URB is finished and must not see a second completion.
        entry.unlinked = true;
        let urb = unsafe { std::ptr::addr_of_mut!((*entry.ptr.0).urb) };
        // USBDEVFS_DISCARDURB takes the URB address *as* the argument.
        let rc = unsafe { libc::ioctl(self.raw(), USBDEVFS_DISCARDURB as _, urb) };
        Ok(rc == 0)
    }

    fn wait(&self, timeout: Duration) -> io::Result<Wake> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = left.as_millis().min(i32::MAX as u128) as c_int;
            // usbfs reports completed URBs as POLLOUT (devio.c:usbdev_poll),
            // not POLLIN — the file is "writable" in the sense that there is
            // something to collect.
            let mut pfd = libc::pollfd {
                fd: self.raw(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    if Instant::now() >= deadline {
                        return Ok(Wake::default());
                    }
                    continue;
                }
                return Err(e);
            }
            if rc == 0 {
                return Ok(Wake::default());
            }
            return Ok(Wake {
                ready: pfd.revents & libc::POLLOUT != 0,
                gone: pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0,
            });
        }
    }

    fn cancel_pending(&self) {
        let seqnums: Vec<u32> = self
            .state
            .lock()
            .map(|s| s.pending.keys().copied().collect())
            .unwrap_or_default();
        if seqnums.is_empty() {
            return;
        }
        debug!(
            "{}: cancelling {} URB(s) still in flight",
            self.summary.busid,
            seqnums.len()
        );
        for s in seqnums {
            let _ = self.unlink(s);
        }

        // Drain what the cancellations completed. Bounded: a device that has
        // stopped answering must not hold a session's teardown open.
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            if self
                .state
                .lock()
                .map(|s| s.pending.is_empty())
                .unwrap_or(true)
            {
                break;
            }
            match self.reap() {
                Ok(Some(_)) => continue,
                Ok(None) => {
                    let _ = self.wait(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    fn reap(&self) -> io::Result<Option<UrbCompletion>> {
        let mut st = self.state.lock().unwrap();
        let mut urb_ptr: *mut UsbdevfsUrb = std::ptr::null_mut();
        let rc = unsafe { libc::ioctl(self.raw(), USBDEVFS_REAPURBNDELAY as _, &mut urb_ptr) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EAGAIN) => Ok(None),
                _ => Err(e),
            };
        }
        if urb_ptr.is_null() {
            return Ok(None);
        }

        // Safety: the kernel hands back the same address we passed to
        // SUBMITURB, and that allocation is still alive because only this
        // function frees one and it holds the lock.
        let ctx = unsafe { (*urb_ptr).usercontext } as *mut PendingUrb;
        if ctx.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reaped a URB that usbfwd did not submit",
            ));
        }
        let pending = unsafe { Box::from_raw(ctx) };
        let unlinked = st
            .pending
            .remove(&pending.seqnum)
            .map(|e| e.unlinked)
            .unwrap_or(false);
        drop(st);

        let status = pending.urb.status;
        let actual = pending.urb.actual_length.max(0) as usize;
        let data = if pending.dir.is_in() {
            let off = pending.data_offset;
            let avail = pending.buffer.len().saturating_sub(off);
            let n = actual.min(pending.max_reply).min(avail);
            pending.buffer[off..off + n].to_vec()
        } else {
            Vec::new()
        };
        let actual_length = if pending.dir.is_in() {
            data.len()
        } else {
            actual
        };

        Ok(Some(UrbCompletion {
            seqnum: pending.seqnum,
            status,
            actual_length,
            data,
            unlinked,
        }))
    }
}

/// A standard request that must be carried out through its own usbfs ioctl
/// rather than forwarded as a raw control URB.
#[derive(Debug, PartialEq, Eq)]
enum Special {
    SetAddress(u16),
    SetConfiguration(u16),
    SetInterface { interface: u16, alt: u16 },
    ClearHalt(u16),
}

impl Special {
    /// Classify an 8-byte SETUP packet. Device-to-host requests are never
    /// special: the same request codes read state instead of changing it.
    fn classify(setup: &[u8; SETUP_LEN]) -> Option<Special> {
        let (typ, request) = (setup[0], setup[1]);
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let index = u16::from_le_bytes([setup[4], setup[5]]);
        if typ & 0x80 != 0 {
            return None;
        }
        match (typ, request) {
            (0x00, 0x05) => Some(Special::SetAddress(value)),
            (0x00, 0x09) => Some(Special::SetConfiguration(value)),
            (0x01, 0x0b) => Some(Special::SetInterface {
                interface: index,
                alt: value,
            }),
            // Only ENDPOINT_HALT (feature selector 0) resets a data toggle.
            (0x02, 0x01) if value == 0x0000 => Some(Special::ClearHalt(index)),
            _ => None,
        }
    }
}

impl Drop for UsbfsDevice {
    fn drop(&mut self) {
        // Anything this cannot drain is handled by closing the descriptor
        // below: after close(2) no reap can occur.
        self.cancel_pending();

        self.release_all();

        // Close first, then free. After close(2) no reap can occur, so the
        // kernel can no longer reference any of these buffers (see the module
        // comment) and freeing the stragglers is sound.
        self.fd = None;
        if let Ok(mut st) = self.state.lock() {
            for (_, e) in st.pending.drain() {
                unsafe { drop(Box::from_raw(e.ptr.0)) };
            }
        }
    }
}

fn read_descriptor_blob(fd: RawFd) -> io::Result<Vec<u8>> {
    // Reading a usbfs node from offset 0 yields the device descriptor followed
    // by every configuration descriptor, which is the same content as
    // /sys/.../descriptors and is available on Android where sysfs is not.
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; 65536];
    let mut total = 0usize;
    while total < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf[total..].as_mut_ptr() as *mut c_void,
                buf.len() - total,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            break;
        }
        total += n as usize;
    }
    buf.truncate(total);
    Ok(buf)
}

fn get_speed(fd: RawFd) -> Option<Speed> {
    let rc = unsafe { libc::ioctl(fd, USBDEVFS_GET_SPEED as _) };
    if rc < 0 {
        return None;
    }
    Some(Speed::from_raw(rc as u32))
}

fn get_capabilities(fd: RawFd) -> u32 {
    let mut caps: u32 = 0;
    let rc = unsafe { libc::ioctl(fd, USBDEVFS_GET_CAPABILITIES as _, &mut caps) };
    if rc < 0 {
        0
    } else {
        caps
    }
}

/// Which configuration is in effect. sysfs knows on Linux; elsewhere ask the
/// device; failing both, assume its first configuration.
fn active_config(fd: RawFd, busid: &str, desc: &Descriptors) -> u8 {
    if let Some(v) = enumerate::sysfs_config_value(busid) {
        if desc.config(v).is_some() {
            return v;
        }
    }
    let mut buf = [0u8; 1];
    // GET_CONFIGURATION: bmRequestType 0x80, bRequest 0x08.
    if control(fd, 0x80, 0x08, 0, 0, &mut buf, 1000).is_ok() && desc.config(buf[0]).is_some() {
        return buf[0];
    }
    desc.configs
        .first()
        .map(|c| c.configuration_value)
        .unwrap_or(1)
}

fn control(
    fd: RawFd,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    buf: &mut [u8],
    timeout_ms: u32,
) -> io::Result<usize> {
    let mut ct = UsbdevfsCtrltransfer {
        b_request_type: request_type,
        b_request: request,
        w_value: value,
        w_index: index,
        w_length: buf.len() as u16,
        timeout: timeout_ms,
        data: buf.as_mut_ptr() as *mut c_void,
    };
    let rc = unsafe { libc::ioctl(fd, USBDEVFS_CONTROL as _, &mut ct) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

/// Take ownership of a raw descriptor without duplicating it. Used by the JNI
/// shim, where Kotlin hands over a descriptor it will not close itself.
pub fn adopt_raw_fd(fd: RawFd) -> io::Result<UsbfsDevice> {
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let (busnum, devnum) = enumerate::numbers_for_fd(owned.as_raw_fd()).unwrap_or((0, 0));
    // build() takes ownership, so a failure here closes the descriptor rather
    // than leaking it back to the caller.
    UsbfsDevice::build(owned, busnum, devnum)
}

/// Release a descriptor back to the caller, undoing [`adopt_raw_fd`].
pub fn into_raw_fd(dev: UsbfsDevice) -> RawFd {
    let mut dev = std::mem::ManuallyDrop::new(dev);
    match dev.fd.take() {
        Some(f) => f.into_raw_fd(),
        None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four requests `tweak_special_requests()` intercepts in
    /// `drivers/usb/usbip/stub_rx.c`. Forwarding any of them as a raw control
    /// URB leaves the exporting kernel's idea of the device out of step with
    /// the device — for SET_CONFIGURATION that means stale data toggles, and
    /// an interrupt IN endpoint that never completes another URB.
    #[test]
    fn state_changing_standard_requests_are_intercepted() {
        // SET_CONFIGURATION(1): bmRequestType 0x00, bRequest 0x09.
        assert_eq!(
            Special::classify(&[0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]),
            Some(Special::SetConfiguration(1))
        );
        // SET_INTERFACE(interface 2, alt 1): type 0x01, request 0x0b.
        assert_eq!(
            Special::classify(&[0x01, 0x0b, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00]),
            Some(Special::SetInterface {
                interface: 2,
                alt: 1
            })
        );
        // CLEAR_FEATURE(ENDPOINT_HALT) on ep 0x81: type 0x02, request 0x01,
        // wValue 0 (ENDPOINT_HALT).
        assert_eq!(
            Special::classify(&[0x02, 0x01, 0x00, 0x00, 0x81, 0x00, 0x00, 0x00]),
            Some(Special::ClearHalt(0x81))
        );
        // SET_ADDRESS(5): acknowledged, never acted on.
        assert_eq!(
            Special::classify(&[0x00, 0x05, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00]),
            Some(Special::SetAddress(5))
        );
    }

    #[test]
    fn ordinary_control_traffic_is_left_alone() {
        // GET_DESCRIPTOR(HID report) — device-to-host, so not special even
        // though 0x06 is a standard request.
        assert_eq!(
            Special::classify(&[0x81, 0x06, 0x00, 0x22, 0x00, 0x00, 0x74, 0x01]),
            None
        );
        // GET_CONFIGURATION reads the same state SET_CONFIGURATION writes;
        // the direction bit is the only thing telling them apart.
        assert_eq!(
            Special::classify(&[0x80, 0x08, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00]),
            None
        );
        // SET_IDLE — a HID class request usbhid sends on open. Class, not
        // standard, so it must still go to the device as a URB.
        assert_eq!(
            Special::classify(&[0x21, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            None
        );
        // CLEAR_FEATURE with a non-zero selector is not ENDPOINT_HALT and
        // must not be turned into a toggle reset.
        assert_eq!(
            Special::classify(&[0x02, 0x01, 0x01, 0x00, 0x81, 0x00, 0x00, 0x00]),
            None
        );
    }

    /// Mirrors the sizing rule in `submit`, which is the one place a
    /// disagreement between the two lengths turns into `-EINVAL` from the
    /// kernel rather than a visible bug here.
    fn control_payload_len(setup: &[u8; 8], transfer_buffer_length: usize) -> usize {
        let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        transfer_buffer_length.max(w_length)
    }

    #[test]
    fn control_payload_is_sized_from_whichever_length_is_larger() {
        // GET_DESCRIPTOR(HID report), wLength = 64. devio.c re-reads the
        // length out of the setup packet and checks our buffer against it, so
        // a client that sends transfer_buffer_length = 0 must not produce a
        // zero-sized allocation.
        let setup = [0x80u8, 0x06, 0x00, 0x22, 0x00, 0x00, 0x40, 0x00];
        assert_eq!(control_payload_len(&setup, 0), 64);
        assert_eq!(control_payload_len(&setup, 64), 64);
        // And the converse: a client asking for more than wLength gets the
        // bigger buffer, so the kernel's copy-back cannot overrun it.
        assert_eq!(control_payload_len(&setup, 200), 200);
        // A zero-length control transfer stays zero-length.
        let no_data = [0x21u8, 0x0a, 0, 0, 0, 0, 0, 0];
        assert_eq!(control_payload_len(&no_data, 0), 0);
    }

    #[test]
    fn transfer_type_maps_to_the_right_usbfs_urb_type() {
        // The two numbering schemes disagree, which is an easy bug to write.
        assert_eq!(USBDEVFS_URB_TYPE_ISO, 0);
        assert_eq!(USBDEVFS_URB_TYPE_INTERRUPT, 1);
        assert_eq!(USBDEVFS_URB_TYPE_CONTROL, 2);
        assert_eq!(USBDEVFS_URB_TYPE_BULK, 3);
    }
}
