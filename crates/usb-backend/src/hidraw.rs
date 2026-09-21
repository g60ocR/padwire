//! Reading a device's HID reports *without* taking it away from anyone.
//!
//! This exists for one job: watching for the toggle chord while the device is
//! **not** being forwarded. While a session is live the exporter owns every
//! interface and sees each input report go past on its way to the socket, so
//! there is nothing to discover here. Once it lets go, the kernel rebinds its
//! own driver and the only way back in is the `hidraw` node that driver
//! creates.
//!
//! `hidraw` is the right seam for that because it is not exclusive: every open
//! descriptor gets a copy of each report, so a Steam client on this machine
//! keeps working while the watcher reads along. Nothing here claims, evicts or
//! writes anything — it is a read-only tap, and that is the whole reason a
//! chord pressed on the device's *own* buttons can be noticed at a moment when
//! usbfwd has deliberately given the device up.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, io};

use crate::enumerate::{is_valid_busid, SYSFS_USB_DEVICES};

/// Largest HID report worth reading in one go. Valve's are 64 bytes; the
/// ceiling only exists so a misbehaving device cannot size an allocation.
pub const MAX_REPORT: usize = 256;

/// Every `/dev/hidrawN` belonging to one USB device, in sysfs order.
///
/// A Steam Deck's own controls present several HID interfaces — keyboard,
/// mouse and the vendor one that carries the button state — so this returns a
/// list and the caller watches all of them. Sorting out which is which is the
/// decoder's job, not this one's: a report that does not parse is simply not a
/// chord.
pub fn nodes_for_busid(busid: &str) -> io::Result<Vec<PathBuf>> {
    if !is_valid_busid(busid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{busid:?} is not a usable sysfs bus id"),
        ));
    }
    let root = PathBuf::from(format!("{SYSFS_USB_DEVICES}/{busid}"));
    let mut out = Vec::new();
    // /sys/bus/usb/devices/3-3/3-3:1.2/0003:28DE:1205.0003/hidraw/hidraw3
    for iface in read_dir_sorted(&root)? {
        let name = iface.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with(&format!("{busid}:")) {
            continue;
        }
        for hid in read_dir_sorted(&iface).unwrap_or_default() {
            for node in read_dir_sorted(&hid.join("hidraw")).unwrap_or_default() {
                let n = node.file_name().unwrap_or_default().to_string_lossy();
                if n.starts_with("hidraw") {
                    out.push(PathBuf::from(format!("/dev/{n}")));
                }
            }
        }
    }
    Ok(out)
}

fn read_dir_sorted(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    v.sort();
    Ok(v)
}

/// A poll over several `hidraw` nodes at once.
///
/// Opened non-blocking, so a device that has gone quiet costs nothing but the
/// poll timeout, and a device that disappears shows up as an error on its own
/// descriptor rather than wedging the thread.
pub struct Tap {
    files: Vec<File>,
    paths: Vec<PathBuf>,
}

impl Tap {
    /// Open every node. Nodes that cannot be opened are skipped with their
    /// error returned alongside, because the usual cause — a missing udev rule
    /// for `hidraw` — is worth reporting once rather than retrying silently.
    pub fn open(paths: &[PathBuf]) -> (Tap, Vec<(PathBuf, io::Error)>) {
        let mut files = Vec::new();
        let mut kept = Vec::new();
        let mut errors = Vec::new();
        for p in paths {
            match open_nonblocking(p) {
                Ok(f) => {
                    files.push(f);
                    kept.push(p.clone());
                }
                Err(e) => errors.push((p.clone(), e)),
            }
        }
        (Tap { files, paths: kept }, errors)
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Wait up to `timeout` and return every report that arrived.
    ///
    /// `Ok(None)` means a node reported an error or hangup — the device was
    /// unplugged, or its driver rebound — and the caller should rebuild the
    /// tap rather than keep polling descriptors that will never be ready.
    pub fn read(&mut self, timeout: Duration) -> io::Result<Option<Vec<Vec<u8>>>> {
        if self.files.is_empty() {
            return Ok(None);
        }
        let mut pfds: Vec<libc::pollfd> = self
            .files
            .iter()
            .map(|f| libc::pollfd {
                fd: f.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();

        let deadline = Instant::now() + timeout;
        let rc = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) && Instant::now() < deadline {
                    continue;
                }
                if e.raw_os_error() == Some(libc::EINTR) {
                    break 0;
                }
                return Err(e);
            }
            break rc;
        };
        if rc == 0 {
            return Ok(Some(Vec::new()));
        }

        let mut reports = Vec::new();
        for (i, pfd) in pfds.iter().enumerate() {
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Ok(None);
            }
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            // Drain: several reports can be queued behind one wake-up, and the
            // chord detector needs each of them in order.
            loop {
                let mut buf = [0u8; MAX_REPORT];
                match self.files[i].read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => reports.push(buf[..n].to_vec()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // The node went away under us.
                    Err(_) => return Ok(None),
                }
            }
        }
        Ok(Some(reports))
    }
}

fn open_nonblocking(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_bus_ids_never_reach_the_filesystem() {
        for bad in ["", "../../dev", "1-2/../..", "a..b"] {
            assert!(nodes_for_busid(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn a_device_with_no_hid_interfaces_yields_nothing() {
        // Root hubs exist on every machine with usbcore and have no hidraw.
        if let Ok(nodes) = nodes_for_busid("usb1") {
            assert!(nodes.is_empty(), "a root hub should expose no hidraw node");
        }
    }

    #[test]
    fn an_empty_tap_reports_itself_gone_rather_than_blocking() {
        let (mut tap, errs) = Tap::open(&[]);
        assert!(tap.is_empty());
        assert!(errs.is_empty());
        assert!(tap.read(Duration::from_millis(10)).unwrap().is_none());
    }

    #[test]
    fn opening_a_missing_node_is_reported_not_silently_dropped() {
        let (tap, errs) = Tap::open(&[PathBuf::from("/dev/hidraw-does-not-exist")]);
        assert!(tap.is_empty());
        assert_eq!(errs.len(), 1);
    }
}
