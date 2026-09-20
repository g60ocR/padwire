//! Finding devices and naming them.
//!
//! Listing goes through sysfs when it is available: `/sys/bus/usb/devices` has
//! the descriptors, the bus id and the active configuration without opening
//! anything, so `usbfwd-server --list` works without the udev rule installed.
//! Where sysfs is not readable — notably Android — the same facts come out of
//! the device node itself, which is why nothing here is load-bearing for the
//! Android path.

use std::fs;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use crate::descriptors::{self, Descriptors};
use crate::{DeviceFilter, DeviceSummary, InterfaceSummary, Speed};

pub const USB_DEV_DIR: &str = "/dev/bus/usb";
pub const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

pub fn node_path(busnum: u32, devnum: u32) -> PathBuf {
    PathBuf::from(format!("{USB_DEV_DIR}/{busnum:03}/{devnum:03}"))
}

/// Recover `(busnum, devnum)` from a `/dev/bus/usb/BBB/DDD` path.
pub fn split_node_path(p: &Path) -> Option<(u32, u32)> {
    let devnum = p.file_name()?.to_str()?.parse().ok()?;
    let busnum = p.parent()?.file_name()?.to_str()?.parse().ok()?;
    Some((busnum, devnum))
}

pub fn sys_path(busid: &str) -> Option<PathBuf> {
    if !is_valid_busid(busid) {
        return None;
    }
    let p = PathBuf::from(format!("{SYSFS_USB_DEVICES}/{busid}"));
    p.exists().then_some(p)
}

/// sysfs bus ids look like `1-2`, `1-2.3` or `usb1`. Bus ids reach the server
/// straight off the network in `OP_REQ_IMPORT`, and every lookup below builds
/// a path out of one. The server only ever imports a bus id it already found
/// itself, so this is belt and braces — but it is the kind of belt that is
/// cheap to wear.
pub fn is_valid_busid(busid: &str) -> bool {
    !busid.is_empty()
        && busid.len() <= 63
        && busid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b':')
        && !busid.contains("..")
}

fn sysfs_attr(busid: &str, attr: &str) -> Option<String> {
    if !is_valid_busid(busid) {
        return None;
    }
    fs::read_to_string(format!("{SYSFS_USB_DEVICES}/{busid}/{attr}"))
        .ok()
        .map(|s| s.trim().to_owned())
}

pub fn sysfs_config_value(busid: &str) -> Option<u8> {
    sysfs_attr(busid, "bConfigurationValue")?.parse().ok()
}

pub fn sysfs_speed(busid: &str) -> Option<Speed> {
    sysfs_attr(busid, "speed").map(|s| Speed::from_sysfs(&s))
}

/// sysfs names USB devices by port path (`1-2.3`), not by device number, so
/// there is no formula — the mapping has to be looked up.
pub fn busid_for(busnum: u32, devnum: u32) -> Option<String> {
    for entry in fs::read_dir(SYSFS_USB_DEVICES).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(':') {
            continue; // an interface, not a device
        }
        if sysfs_attr(&name, "busnum").and_then(|s| s.parse::<u32>().ok()) == Some(busnum)
            && sysfs_attr(&name, "devnum").and_then(|s| s.parse::<u32>().ok()) == Some(devnum)
        {
            return Some(name);
        }
    }
    None
}

pub fn numbers_for_busid(busid: &str) -> Option<(u32, u32)> {
    Some((
        sysfs_attr(busid, "busnum")?.parse().ok()?,
        sysfs_attr(busid, "devnum")?.parse().ok()?,
    ))
}

/// Ask the kernel what file a descriptor refers to. On Android this is how the
/// exporter learns its own bus and device numbers, since the descriptor
/// arrives from `UsbDeviceConnection` with no path attached.
pub fn numbers_for_fd(fd: RawFd) -> Option<(u32, u32)> {
    let target = fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    split_node_path(&target)
}

/// Build a summary from already-parsed descriptors.
pub fn summarize(
    desc: &Descriptors,
    config_value: u8,
    busnum: u32,
    devnum: u32,
    busid: String,
) -> io::Result<DeviceSummary> {
    let config = desc.config(config_value).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("active configuration {config_value} is not among the device's descriptors"),
        )
    })?;
    let node = node_path(busnum, devnum).to_string_lossy().into_owned();
    Ok(DeviceSummary {
        busnum,
        devnum,
        sys_path: sys_path(&busid)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| node.clone()),
        node_path: node,
        busid,
        speed: Speed::Unknown,
        device: desc.device,
        config_value,
        interfaces: InterfaceSummary::list_from(config),
    })
}

/// Describe one device using sysfs alone — no open, so no permissions needed.
pub fn summary_from_sysfs(busid: &str) -> io::Result<DeviceSummary> {
    if !is_valid_busid(busid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{busid:?} is not a usable sysfs bus id"),
        ));
    }
    let blob = fs::read(format!("{SYSFS_USB_DEVICES}/{busid}/descriptors"))?;
    let desc = descriptors::parse(&blob)?;
    let (busnum, devnum) = numbers_for_busid(busid).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{busid} has no busnum/devnum in sysfs"),
        )
    })?;
    let config_value = sysfs_config_value(busid)
        .filter(|v| desc.config(*v).is_some())
        .or_else(|| desc.configs.first().map(|c| c.configuration_value))
        .unwrap_or(1);
    let mut s = summarize(&desc, config_value, busnum, devnum, busid.to_owned())?;
    s.speed = sysfs_speed(busid).unwrap_or(Speed::Unknown);
    Ok(s)
}

/// Every USB device sysfs knows about, unfiltered and in bus id order.
pub fn list_all() -> io::Result<Vec<DeviceSummary>> {
    let mut out = Vec::new();
    let dir = match fs::read_dir(SYSFS_USB_DEVICES) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{SYSFS_USB_DEVICES} is missing; is this a Linux host with usbcore loaded?"
                ),
            ))
        }
        Err(e) => return Err(e),
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `1-2:1.0` is an interface and `usb1` is a root hub; neither is
        // something a client can import.
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        match summary_from_sysfs(&name) {
            Ok(s) => out.push(s),
            // A device unplugged mid-scan is normal, not an error.
            Err(_) => continue,
        }
    }
    out.sort_by_key(|d| (d.busnum, d.devnum));
    Ok(out)
}

/// The exportable devices: everything the allow-list admits. Root hubs are
/// already excluded by [`list_all`].
pub fn list(filter: &DeviceFilter) -> io::Result<Vec<DeviceSummary>> {
    Ok(list_all()?
        .into_iter()
        .filter(|d| {
            let (vid, pid) = d.vid_pid();
            filter.matches(vid, pid)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_bus_ids_never_reach_the_filesystem() {
        for good in ["1-2", "1-2.3.4", "usb1", "1-2:1.0"] {
            assert!(is_valid_busid(good), "{good} should be accepted");
        }
        for bad in [
            "",
            "../../../etc/passwd",
            "1-2/../..",
            "1-2/descriptors",
            "a..b",
            "$(whoami)",
            "1 2",
        ] {
            assert!(!is_valid_busid(bad), "{bad:?} should be rejected");
            assert!(sysfs_attr(bad, "busnum").is_none());
            assert!(sys_path(bad).is_none());
            assert!(summary_from_sysfs(bad).is_err());
        }
    }

    #[test]
    fn node_paths_are_zero_padded() {
        assert_eq!(node_path(1, 7), PathBuf::from("/dev/bus/usb/001/007"));
        assert_eq!(node_path(12, 145), PathBuf::from("/dev/bus/usb/012/145"));
    }

    #[test]
    fn node_paths_round_trip() {
        for (b, d) in [(1u32, 7u32), (12, 145), (3, 1)] {
            assert_eq!(split_node_path(&node_path(b, d)), Some((b, d)));
        }
    }

    #[test]
    fn unrelated_paths_are_not_mistaken_for_device_nodes() {
        assert_eq!(split_node_path(Path::new("/dev/null")), None);
        assert_eq!(split_node_path(Path::new("/dev/bus/usb/001")), None);
        assert_eq!(split_node_path(Path::new("socket:[12345]")), None);
    }

    /// Runs against whatever is plugged into the build machine. It asserts
    /// shape, not contents, so it is meaningful on a bare VM too.
    #[test]
    fn listing_the_local_bus_is_self_consistent() {
        let Ok(devs) = list_all() else {
            return; // no usbcore, e.g. a container without /sys
        };
        for d in &devs {
            assert!(!d.busid.is_empty());
            assert!(d.busnum > 0, "{} has busnum 0", d.busid);
            assert_eq!(
                d.interfaces.len(),
                d.to_usbip().b_num_interfaces as usize,
                "{} advertises a different interface count than it encodes",
                d.busid
            );
            assert!(d.node_path.starts_with("/dev/bus/usb/"));
        }
    }
}
