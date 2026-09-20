//! USB/IP 1.1.1 wire format.
//!
//! Encode and decode only: nothing in here opens a socket or a device, which
//! keeps the whole protocol surface unit-testable against captured traffic.
//! The [`io`] module adds framing over `std::io::{Read, Write}` but still makes
//! no policy decisions.
//!
//! Layout reference: `Documentation/usb/usbip_protocol.rst` in the kernel tree,
//! cross-checked against `drivers/usb/usbip/{stub_tx,stub_rx,vhci_tx,vhci_rx}.c`
//! where the documentation and the implementation disagree. Those disagreements
//! are called out at the places they matter.
//!
//! Everything is big-endian.

#![forbid(unsafe_code)]

pub mod device;
pub mod io;
pub mod op;
pub mod pdu;
pub mod wire;

pub use device::{Speed, UsbDevice, UsbInterface};
pub use op::OpHeader;
pub use pdu::{BasicHeader, Body, CmdSubmit, Direction, Header, RetSubmit};

/// Protocol revision 1.1.1, the only one the in-tree tools speak.
pub const USBIP_VERSION: u16 = 0x0111;

/// `OP_REQ_*` / `OP_REP_*` common header.
pub const SIZE_OP_HEADER: usize = 8;
/// `struct usbip_usb_device`.
pub const SIZE_USB_DEVICE: usize = 312;
/// `struct usbip_usb_interface`.
pub const SIZE_USB_INTERFACE: usize = 4;
/// 20-byte `usbip_header_basic` + 28-byte command block.
pub const SIZE_CMD_HEADER: usize = 48;
/// `struct usbip_iso_packet_descriptor`.
pub const SIZE_ISO_DESC: usize = 16;
pub const BUSID_LEN: usize = 32;
pub const PATH_LEN: usize = 256;

pub const USBIP_PORT: u16 = 3240;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Truncated {
        need: usize,
        have: usize,
    },
    BadVersion(u16),
    BadCommand(u32),
    TooLarge {
        field: &'static str,
        value: i64,
        max: i64,
    },
    Utf8,
}

impl core::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProtoError::Truncated { need, have } => {
                write!(f, "truncated message: needed {need} more bytes, had {have}")
            }
            ProtoError::BadVersion(v) => write!(
                f,
                "peer speaks USB/IP version 0x{v:04x}, expected 0x{USBIP_VERSION:04x}"
            ),
            ProtoError::BadCommand(c) => write!(f, "unknown USB/IP command 0x{c:08x}"),
            ProtoError::TooLarge { field, value, max } => {
                write!(f, "{field} = {value} exceeds the limit of {max}")
            }
            ProtoError::Utf8 => write!(f, "text field is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<ProtoError> for std::io::Error {
    fn from(e: ProtoError) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}
