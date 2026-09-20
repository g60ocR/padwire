//! `struct usbip_usb_device` / `struct usbip_usb_interface` as they appear in
//! `OP_REP_DEVLIST` and `OP_REP_IMPORT`.

use crate::wire::{Reader, Writer};
use crate::{ProtoError, BUSID_LEN, PATH_LEN, SIZE_USB_DEVICE, SIZE_USB_INTERFACE};

type Result<T> = core::result::Result<T, ProtoError>;

/// `enum usb_device_speed` from the kernel. `OP_REP_IMPORT` carries this value
/// verbatim and `vhci-hcd` uses it to decide what kind of port to fake, so it
/// has to be the enum, not a bit rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Speed {
    Unknown = 0,
    Low = 1,
    Full = 2,
    High = 3,
    Wireless = 4,
    Super = 5,
    SuperPlus = 6,
}

impl Speed {
    pub fn from_raw(v: u32) -> Speed {
        match v {
            1 => Speed::Low,
            2 => Speed::Full,
            3 => Speed::High,
            4 => Speed::Wireless,
            5 => Speed::Super,
            6 => Speed::SuperPlus,
            _ => Speed::Unknown,
        }
    }

    /// Parse the sysfs `speed` attribute, which is a bit rate in Mbit/s.
    pub fn from_sysfs(s: &str) -> Speed {
        match s.trim() {
            "1.5" => Speed::Low,
            "12" => Speed::Full,
            "480" => Speed::High,
            "5000" => Speed::Super,
            "10000" | "20000" => Speed::SuperPlus,
            _ => Speed::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsbInterface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsbDevice {
    /// Informational sysfs path, e.g. `/sys/bus/usb/devices/1-2`.
    pub path: String,
    /// The key a client passes back in `OP_REQ_IMPORT`, e.g. `1-2`.
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub b_device_class: u8,
    pub b_device_sub_class: u8,
    pub b_device_protocol: u8,
    pub b_configuration_value: u8,
    pub b_num_configurations: u8,
    pub b_num_interfaces: u8,
    /// Only present in `OP_REP_DEVLIST`; `OP_REP_IMPORT` omits the interface array.
    pub interfaces: Vec<UsbInterface>,
}

impl UsbDevice {
    /// The `devid` that `USBIP_CMD_SUBMIT` headers carry.
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | (self.devnum & 0xffff)
    }

    /// Encode the fixed 312-byte device record, optionally followed by the
    /// 4-byte-per-interface array (`OP_REP_DEVLIST` only).
    pub fn encode(&self, w: &mut Writer, with_interfaces: bool) {
        let start = w.len();
        w.text_field(&self.path, PATH_LEN);
        w.text_field(&self.busid, BUSID_LEN);
        w.u32(self.busnum);
        w.u32(self.devnum);
        w.u32(self.speed);
        w.u16(self.id_vendor);
        w.u16(self.id_product);
        w.u16(self.bcd_device);
        w.u8(self.b_device_class);
        w.u8(self.b_device_sub_class);
        w.u8(self.b_device_protocol);
        w.u8(self.b_configuration_value);
        w.u8(self.b_num_configurations);
        w.u8(self.b_num_interfaces);
        debug_assert_eq!(w.len() - start, SIZE_USB_DEVICE);

        if with_interfaces {
            for i in 0..self.b_num_interfaces as usize {
                let intf = self.interfaces.get(i).copied().unwrap_or_default();
                w.u8(intf.class);
                w.u8(intf.subclass);
                w.u8(intf.protocol);
                w.u8(0); // padding
            }
        }
    }

    pub fn decode(r: &mut Reader, with_interfaces: bool) -> Result<UsbDevice> {
        let mut d = UsbDevice {
            path: r.text_field(PATH_LEN)?,
            busid: r.text_field(BUSID_LEN)?,
            busnum: r.u32()?,
            devnum: r.u32()?,
            speed: r.u32()?,
            id_vendor: r.u16()?,
            id_product: r.u16()?,
            bcd_device: r.u16()?,
            b_device_class: r.u8()?,
            b_device_sub_class: r.u8()?,
            b_device_protocol: r.u8()?,
            b_configuration_value: r.u8()?,
            b_num_configurations: r.u8()?,
            b_num_interfaces: r.u8()?,
            interfaces: Vec::new(),
        };
        if with_interfaces {
            for _ in 0..d.b_num_interfaces {
                let intf = UsbInterface {
                    class: r.u8()?,
                    subclass: r.u8()?,
                    protocol: r.u8()?,
                };
                r.skip(1)?; // padding
                d.interfaces.push(intf);
            }
        }
        Ok(d)
    }

    /// Wire size of this record, for sizing a read buffer before decoding.
    pub fn encoded_len(&self, with_interfaces: bool) -> usize {
        SIZE_USB_DEVICE
            + if with_interfaces {
                self.b_num_interfaces as usize * SIZE_USB_INTERFACE
            } else {
                0
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UsbDevice {
        UsbDevice {
            path: "/sys/bus/usb/devices/1-2".into(),
            busid: "1-2".into(),
            busnum: 1,
            devnum: 7,
            speed: Speed::High as u32,
            id_vendor: 0x28de,
            id_product: 0x1304,
            bcd_device: 0x0100,
            b_device_class: 0,
            b_device_sub_class: 0,
            b_device_protocol: 0,
            b_configuration_value: 1,
            b_num_configurations: 1,
            b_num_interfaces: 7,
            interfaces: vec![
                UsbInterface {
                    class: 3,
                    subclass: 0,
                    protocol: 0
                };
                7
            ],
        }
    }

    #[test]
    fn device_record_is_312_bytes() {
        let mut w = Writer::new();
        sample().encode(&mut w, false);
        assert_eq!(w.len(), SIZE_USB_DEVICE);
        assert_eq!(w.len(), 312);
    }

    #[test]
    fn roundtrip_with_interfaces() {
        let d = sample();
        let mut w = Writer::new();
        d.encode(&mut w, true);
        assert_eq!(w.len(), 312 + 7 * 4);
        let v = w.into_vec();
        let mut r = Reader::new(&v);
        assert_eq!(UsbDevice::decode(&mut r, true).unwrap(), d);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn roundtrip_without_interfaces() {
        let mut d = sample();
        let mut w = Writer::new();
        d.encode(&mut w, false);
        let v = w.into_vec();
        let mut r = Reader::new(&v);
        d.interfaces.clear();
        assert_eq!(UsbDevice::decode(&mut r, false).unwrap(), d);
    }

    #[test]
    fn devid_packs_bus_and_dev() {
        assert_eq!(sample().devid(), (1 << 16) | 7);
    }

    #[test]
    fn missing_interfaces_are_padded_not_panicked() {
        // b_num_interfaces says 7 but the vector is short: encode must still
        // emit 7 records so the frame length stays correct.
        let mut d = sample();
        d.interfaces.truncate(2);
        let mut w = Writer::new();
        d.encode(&mut w, true);
        assert_eq!(w.len(), 312 + 7 * 4);
    }
}
