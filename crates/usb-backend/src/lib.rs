//! USB device access for the usbfwd exporter.
//!
//! One backend, [`usbfs::UsbfsDevice`], implemented entirely in userspace on
//! top of `/dev/bus/usb`. That choice is what makes the exporter portable:
//! SteamOS has an immutable filesystem and no `usbip-host` module, and Android
//! has no root, but both expose usbfs. The two platforms differ only in where
//! the file descriptor comes from — opened directly on Linux, handed over by
//! `UsbDeviceConnection.getFileDescriptor()` on Android — so the ioctl code
//! below is shared verbatim.

pub mod descriptors;
pub mod enumerate;
pub mod filter;
pub mod sys;
pub mod usbfs;

use std::io;
use std::time::Duration;

pub use descriptors::{
    AltSetting, ConfigDescriptor, Descriptors, DeviceDescriptor, EndpointDescriptor, TransferType,
};
pub use filter::DeviceFilter;
pub use usbfs::UsbfsDevice;
pub use usbip_proto::pdu::Direction;
pub use usbip_proto::Speed;

/// An interface as `OP_REP_DEVLIST` advertises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceSummary {
    pub number: u8,
    pub class: u8,
    pub sub_class: u8,
    pub protocol: u8,
}

impl InterfaceSummary {
    /// Alt setting 0 of each interface, in `bInterfaceNumber` order. This is
    /// exactly what `usbip` reports out of sysfs and what `OP_REP_DEVLIST`
    /// carries.
    pub fn list_from(config: &ConfigDescriptor) -> Vec<InterfaceSummary> {
        config
            .primary_alt_settings()
            .into_iter()
            .map(|a| InterfaceSummary {
                number: a.interface_number,
                class: a.class,
                sub_class: a.sub_class,
                protocol: a.protocol,
            })
            .collect()
    }
}

/// Everything a client needs to see about a device before importing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    pub busnum: u32,
    pub devnum: u32,
    /// The sysfs-style key a client passes back in `OP_REQ_IMPORT`, e.g. `1-2`.
    pub busid: String,
    /// Informational. Falls back to the device node when sysfs is unreadable.
    pub sys_path: String,
    /// `/dev/bus/usb/BBB/DDD`.
    pub node_path: String,
    pub speed: Speed,
    pub device: DeviceDescriptor,
    /// `bConfigurationValue` of the configuration currently in effect.
    pub config_value: u8,
    /// Alt setting 0 of each interface in the active configuration.
    pub interfaces: Vec<InterfaceSummary>,
}

impl DeviceSummary {
    pub fn vid_pid(&self) -> (u16, u16) {
        (self.device.id_vendor, self.device.id_product)
    }

    pub fn to_usbip(&self) -> usbip_proto::UsbDevice {
        usbip_proto::UsbDevice {
            path: self.sys_path.clone(),
            busid: self.busid.clone(),
            busnum: self.busnum,
            devnum: self.devnum,
            speed: self.speed as u32,
            id_vendor: self.device.id_vendor,
            id_product: self.device.id_product,
            bcd_device: self.device.bcd_device,
            b_device_class: self.device.b_device_class,
            b_device_sub_class: self.device.b_device_sub_class,
            b_device_protocol: self.device.b_device_protocol,
            b_configuration_value: self.config_value,
            b_num_configurations: self.device.b_num_configurations,
            // Derived from the interfaces actually parsed rather than from
            // bNumInterfaces, so the encoded record can never disagree with
            // the array that follows it.
            b_num_interfaces: self.interfaces.len() as u8,
            interfaces: self
                .interfaces
                .iter()
                .map(|i| usbip_proto::UsbInterface {
                    class: i.class,
                    subclass: i.sub_class,
                    protocol: i.protocol,
                })
                .collect(),
        }
    }
}

/// One `USBIP_CMD_SUBMIT` translated into backend terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrbRequest {
    pub seqnum: u32,
    /// Endpoint number without the direction bit. 0 means the control endpoint.
    pub ep: u8,
    pub dir: Direction,
    /// Kernel URB flags as they arrived in `transfer_flags`.
    pub transfer_flags: u32,
    /// Only meaningful when `ep == 0`.
    pub setup: [u8; 8],
    /// For IN, how many bytes to ask for. For OUT, equals `data.len()`.
    pub buffer_length: usize,
    /// OUT payload; empty for IN.
    pub data: Vec<u8>,
}

/// One reaped URB, ready to become a `USBIP_RET_SUBMIT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrbCompletion {
    pub seqnum: u32,
    /// Negative Linux errno, or 0. USB/IP carries the same values.
    pub status: i32,
    pub actual_length: usize,
    /// IN data only.
    pub data: Vec<u8>,
    /// True if this URB was cancelled by a `USBIP_CMD_UNLINK`. The client has
    /// already had its `USBIP_RET_UNLINK` and must not also get a
    /// `USBIP_RET_SUBMIT`, so the caller drops these.
    pub unlinked: bool,
}

/// A submit can fail two ways, and conflating them kills sessions needlessly.
#[derive(Debug)]
pub enum SubmitError {
    /// The device refused this URB. Report it to the client as a
    /// `USBIP_RET_SUBMIT` carrying this negative errno and carry on.
    Urb(i32),
    /// Carried out directly instead of being submitted as a URB, because it
    /// changes state the kernel tracks for itself (see
    /// `tweak_special_request`). No completion will be reaped, so the caller
    /// owes the client a `USBIP_RET_SUBMIT` with this status — 0 on success.
    Handled(i32),
    /// The session cannot continue.
    Fatal(io::Error),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::Urb(e) => write!(f, "URB rejected with errno {}", -e),
            SubmitError::Handled(0) => write!(f, "handled directly"),
            SubmitError::Handled(e) => write!(f, "handled directly, errno {}", -e),
            SubmitError::Fatal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SubmitError {}

/// Result of waiting for URB completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wake {
    /// At least one URB is ready to reap.
    pub ready: bool,
    /// The device is gone. Drain any remaining completions first — usbfs
    /// keeps them reapable after disconnect — then tear the session down.
    pub gone: bool,
}

pub trait UsbBackend: Send + Sync {
    fn summary(&self) -> &DeviceSummary;

    /// Evict whatever driver is bound to each interface and take ownership.
    fn claim_all(&self) -> io::Result<()>;

    /// Hand the interfaces back and let the kernel rebind its drivers.
    fn release_all(&self);

    fn submit(&self, req: UrbRequest) -> Result<(), SubmitError>;

    /// Cancel an in-flight URB. Returns true if it was still in flight, which
    /// is the difference between `-ECONNRESET` and `0` in `USBIP_RET_UNLINK`.
    fn unlink(&self, seqnum: u32) -> io::Result<bool>;

    fn wait(&self, timeout: Duration) -> io::Result<Wake>;

    /// Reap one completion, or `None` if none are ready.
    fn reap(&self) -> io::Result<Option<UrbCompletion>>;

    /// Cancel every URB still in flight and drain what the cancellations
    /// complete, leaving no pending seqnums behind.
    ///
    /// A session must call this before it ends. Seqnums are per-session, but
    /// on Android the same device is handed to the next importer, so a URB
    /// left in flight here is reaped by *that* session and answered with a
    /// seqnum it never sent — which `vhci_hcd` rejects, killing the new
    /// attach. The Linux path opens a fresh device per session and so never
    /// showed this.
    fn cancel_pending(&self);
}
