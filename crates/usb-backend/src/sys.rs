//! Raw `usbfs` (`USBDEVFS_*`) definitions.
//!
//! The ioctl request numbers are computed rather than hard-coded, because the
//! `_IOC` encoding embeds `sizeof` the argument type. `struct usbdevfs_urb` is
//! 56 bytes on LP64 and 44 bytes on 32-bit ARM, so `USBDEVFS_SUBMITURB` is
//! `0x8038550a` on a Steam Deck and `0x802c550a` on a 32-bit Android device.
//! Copying the constants from an x86_64 header would silently break armeabi.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

// asm-generic/ioctl.h. Correct for every architecture usbfwd targets
// (x86, arm, arm64, riscv); the alpha/mips/powerpc/sparc encodings differ.
const NRBITS: u32 = 8;
const TYPEBITS: u32 = 8;
const SIZEBITS: u32 = 14;
const NRSHIFT: u32 = 0;
const TYPESHIFT: u32 = NRSHIFT + NRBITS;
const SIZESHIFT: u32 = TYPESHIFT + TYPEBITS;
const DIRSHIFT: u32 = SIZESHIFT + SIZEBITS;

const DIR_NONE: u32 = 0;
const DIR_WRITE: u32 = 1;
const DIR_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    debug_assert!(size < (1 << SIZEBITS));
    (dir << DIRSHIFT) | (b'U' as u32) << TYPESHIFT | (nr << NRSHIFT) | ((size as u32) << SIZESHIFT)
}

const fn io(nr: u32) -> u32 {
    ioc(DIR_NONE, nr, 0)
}
const fn ior(nr: u32, size: usize) -> u32 {
    ioc(DIR_READ, nr, size)
}
const fn iow(nr: u32, size: usize) -> u32 {
    ioc(DIR_WRITE, nr, size)
}
const fn iowr(nr: u32, size: usize) -> u32 {
    ioc(DIR_READ | DIR_WRITE, nr, size)
}

const PTR: usize = std::mem::size_of::<*mut c_void>();

// Note the reversed-looking directions: the kernel's own headers declare
// SUBMITURB as _IOR and REAPURB as _IOW. They are wrong from userspace's point
// of view but they are what the kernel compares against, so reproduce them.
pub const USBDEVFS_CONTROL: u32 = iowr(0, std::mem::size_of::<UsbdevfsCtrltransfer>());
pub const USBDEVFS_SETINTERFACE: u32 = ior(4, std::mem::size_of::<UsbdevfsSetinterface>());
pub const USBDEVFS_SETCONFIGURATION: u32 = ior(5, 4);
pub const USBDEVFS_SUBMITURB: u32 = ior(10, std::mem::size_of::<UsbdevfsUrb>());
pub const USBDEVFS_DISCARDURB: u32 = io(11);
pub const USBDEVFS_REAPURB: u32 = iow(12, PTR);
pub const USBDEVFS_REAPURBNDELAY: u32 = iow(13, PTR);
pub const USBDEVFS_CLAIMINTERFACE: u32 = ior(15, 4);
pub const USBDEVFS_RELEASEINTERFACE: u32 = ior(16, 4);
pub const USBDEVFS_IOCTL: u32 = iowr(18, std::mem::size_of::<UsbdevfsIoctl>());
pub const USBDEVFS_RESET: u32 = io(20);
pub const USBDEVFS_CLEAR_HALT: u32 = ior(21, 4);
pub const USBDEVFS_DISCONNECT: u32 = io(22);
pub const USBDEVFS_CONNECT: u32 = io(23);
pub const USBDEVFS_GET_CAPABILITIES: u32 = ior(26, 4);
pub const USBDEVFS_DISCONNECT_CLAIM: u32 = ior(27, std::mem::size_of::<UsbdevfsDisconnectClaim>());
pub const USBDEVFS_GET_SPEED: u32 = io(31);

pub const USBDEVFS_URB_TYPE_ISO: u8 = 0;
pub const USBDEVFS_URB_TYPE_INTERRUPT: u8 = 1;
pub const USBDEVFS_URB_TYPE_CONTROL: u8 = 2;
pub const USBDEVFS_URB_TYPE_BULK: u8 = 3;

pub const USBDEVFS_URB_SHORT_NOT_OK: c_uint = 0x01;
pub const USBDEVFS_URB_ISO_ASAP: c_uint = 0x02;
pub const USBDEVFS_URB_BULK_CONTINUATION: c_uint = 0x04;
pub const USBDEVFS_URB_ZERO_PACKET: c_uint = 0x40;
pub const USBDEVFS_URB_NO_INTERRUPT: c_uint = 0x80;

pub const USBDEVFS_CAP_ZERO_PACKET: u32 = 0x01;
pub const USBDEVFS_CAP_BULK_CONTINUATION: u32 = 0x02;
pub const USBDEVFS_CAP_NO_PACKET_SIZE_LIM: u32 = 0x04;
pub const USBDEVFS_CAP_BULK_SCATTER_GATHER: u32 = 0x08;
pub const USBDEVFS_CAP_REAP_AFTER_DISCONNECT: u32 = 0x10;

/// `flags = 0` means "disconnect whatever driver is bound, then claim".
pub const USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER: c_uint = 0x01;
pub const USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER: c_uint = 0x02;

pub const MAXDRIVERNAME: usize = 255;

#[repr(C)]
#[derive(Debug)]
pub struct UsbdevfsUrb {
    pub typ: u8,
    pub endpoint: u8,
    pub status: c_int,
    pub flags: c_uint,
    pub buffer: *mut c_void,
    pub buffer_length: c_int,
    pub actual_length: c_int,
    pub start_frame: c_int,
    /// Union with `stream_id`; only meaningful for isochronous and bulk streams.
    pub number_of_packets: c_int,
    pub error_count: c_int,
    pub signr: c_uint,
    pub usercontext: *mut c_void,
    // struct usbdevfs_iso_packet_desc iso_frame_desc[0] — unused, we reject ISO.
}

impl Default for UsbdevfsUrb {
    fn default() -> Self {
        UsbdevfsUrb {
            typ: 0,
            endpoint: 0,
            status: 0,
            flags: 0,
            buffer: std::ptr::null_mut(),
            buffer_length: 0,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            signr: 0,
            usercontext: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct UsbdevfsIoctl {
    pub ifno: c_int,
    pub ioctl_code: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct UsbdevfsDisconnectClaim {
    pub interface: c_uint,
    pub flags: c_uint,
    pub driver: [c_char; MAXDRIVERNAME + 1],
}

#[repr(C)]
pub struct UsbdevfsSetinterface {
    pub interface: c_uint,
    pub altsetting: c_uint,
}

#[repr(C)]
pub struct UsbdevfsCtrltransfer {
    pub b_request_type: u8,
    pub b_request: u8,
    pub w_value: u16,
    pub w_index: u16,
    pub w_length: u16,
    pub timeout: u32,
    pub data: *mut c_void,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urb_layout_matches_the_kernel_header() {
        // Values taken from a C program compiled against <linux/usbdevice_fs.h>.
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<UsbdevfsUrb>(), 56);
            let b = UsbdevfsUrb::default();
            let base = &b as *const _ as usize;
            assert_eq!(&b.status as *const _ as usize - base, 4);
            assert_eq!(&b.flags as *const _ as usize - base, 8);
            assert_eq!(&b.buffer as *const _ as usize - base, 16);
            assert_eq!(&b.buffer_length as *const _ as usize - base, 24);
            assert_eq!(&b.actual_length as *const _ as usize - base, 28);
            assert_eq!(&b.start_frame as *const _ as usize - base, 32);
            assert_eq!(&b.number_of_packets as *const _ as usize - base, 36);
            assert_eq!(&b.error_count as *const _ as usize - base, 40);
            assert_eq!(&b.signr as *const _ as usize - base, 44);
            assert_eq!(&b.usercontext as *const _ as usize - base, 48);
        }
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<UsbdevfsUrb>(), 44);
    }

    #[test]
    fn other_struct_sizes_match() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<UsbdevfsIoctl>(), 16);
            assert_eq!(std::mem::size_of::<UsbdevfsCtrltransfer>(), 24);
        }
        assert_eq!(std::mem::size_of::<UsbdevfsSetinterface>(), 8);
        assert_eq!(std::mem::size_of::<UsbdevfsDisconnectClaim>(), 264);
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    fn ioctl_numbers_match_the_x86_64_header() {
        assert_eq!(USBDEVFS_CONTROL, 0xc018_5500);
        assert_eq!(USBDEVFS_SETINTERFACE, 0x8008_5504);
        assert_eq!(USBDEVFS_SETCONFIGURATION, 0x8004_5505);
        assert_eq!(USBDEVFS_SUBMITURB, 0x8038_550a);
        assert_eq!(USBDEVFS_DISCARDURB, 0x0000_550b);
        assert_eq!(USBDEVFS_REAPURB, 0x4008_550c);
        assert_eq!(USBDEVFS_REAPURBNDELAY, 0x4008_550d);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550f);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
        assert_eq!(USBDEVFS_IOCTL, 0xc010_5512);
        assert_eq!(USBDEVFS_RESET, 0x0000_5514);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
        assert_eq!(USBDEVFS_DISCONNECT, 0x0000_5516);
        assert_eq!(USBDEVFS_CONNECT, 0x0000_5517);
        assert_eq!(USBDEVFS_GET_CAPABILITIES, 0x8004_551a);
        assert_eq!(USBDEVFS_DISCONNECT_CLAIM, 0x8108_551b);
        assert_eq!(USBDEVFS_GET_SPEED, 0x0000_551f);
    }
}
