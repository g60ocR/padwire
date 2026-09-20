//! Parsing the raw USB descriptor blob.
//!
//! The same bytes are available three ways, and all three land here:
//!   * `read()` on a `/dev/bus/usb/BBB/DDD` node (works on Android too),
//!   * `/sys/bus/usb/devices/<busid>/descriptors` (Linux, no open needed),
//!   * a `GET_DESCRIPTOR` control transfer (last resort).
//!
//! The blob is the 18-byte device descriptor followed by every configuration
//! descriptor in full.

// usbfs byte-swaps the *device* descriptor's 16-bit fields into host order on
// read() (devio.c:usbdev_read) while leaving the configuration descriptors in
// bus order. On a little-endian host the two are the same and a single
// little-endian reader is correct. Every target usbfwd builds for is
// little-endian; refuse to compile rather than produce silently wrong VID/PIDs.
#[cfg(target_endian = "big")]
compile_error!(
    "usbfs returns the device descriptor in host byte order; the big-endian path is unimplemented"
);

use std::io;

pub const DT_DEVICE: u8 = 0x01;
pub const DT_CONFIG: u8 = 0x02;
pub const DT_INTERFACE: u8 = 0x04;
pub const DT_ENDPOINT: u8 = 0x05;

pub const DEVICE_DESC_LEN: usize = 18;

/// `bmAttributes & 0x03` of an endpoint descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

impl TransferType {
    pub fn from_attributes(attrs: u8) -> TransferType {
        match attrs & 0x03 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub b_device_class: u8,
    pub b_device_sub_class: u8,
    pub b_device_protocol: u8,
    pub b_max_packet_size0: u8,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub b_num_configurations: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// `bEndpointAddress`, including the 0x80 IN bit.
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl EndpointDescriptor {
    pub fn transfer_type(&self) -> TransferType {
        TransferType::from_attributes(self.attributes)
    }

    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltSetting {
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub class: u8,
    pub sub_class: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDescriptor {
    pub configuration_value: u8,
    pub num_interfaces: u8,
    pub alt_settings: Vec<AltSetting>,
}

impl ConfigDescriptor {
    /// Distinct `bInterfaceNumber`s, ascending. These are what get claimed.
    pub fn interface_numbers(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self
            .alt_settings
            .iter()
            .map(|a| a.interface_number)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Alt setting 0 of each interface, which is what `OP_REP_DEVLIST`
    /// advertises (and what `usbip` reads out of sysfs).
    pub fn primary_alt_settings(&self) -> Vec<&AltSetting> {
        let mut out = Vec::new();
        for n in self.interface_numbers() {
            if let Some(a) = self
                .alt_settings
                .iter()
                .find(|a| a.interface_number == n && a.alternate_setting == 0)
                .or_else(|| self.alt_settings.iter().find(|a| a.interface_number == n))
            {
                out.push(a);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptors {
    pub device: DeviceDescriptor,
    pub configs: Vec<ConfigDescriptor>,
}

impl Descriptors {
    pub fn config(&self, value: u8) -> Option<&ConfigDescriptor> {
        self.configs.iter().find(|c| c.configuration_value == value)
    }
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Parse a device descriptor followed by zero or more configuration descriptors.
pub fn parse(blob: &[u8]) -> io::Result<Descriptors> {
    if blob.len() < DEVICE_DESC_LEN {
        return Err(bad(format!(
            "descriptor blob is {} bytes, need at least {DEVICE_DESC_LEN}",
            blob.len()
        )));
    }
    if blob[1] != DT_DEVICE {
        return Err(bad(format!(
            "blob does not start with a device descriptor (bDescriptorType = {})",
            blob[1]
        )));
    }

    let device = DeviceDescriptor {
        bcd_usb: u16le(blob, 2),
        b_device_class: blob[4],
        b_device_sub_class: blob[5],
        b_device_protocol: blob[6],
        b_max_packet_size0: blob[7],
        id_vendor: u16le(blob, 8),
        id_product: u16le(blob, 10),
        bcd_device: u16le(blob, 12),
        b_num_configurations: blob[17],
    };

    let mut configs = Vec::new();
    let mut pos = blob[0] as usize;
    if pos < DEVICE_DESC_LEN {
        pos = DEVICE_DESC_LEN;
    }
    while pos + 2 <= blob.len() {
        let len = blob[pos] as usize;
        let dtype = blob[pos + 1];
        if len < 2 || pos + len > blob.len() {
            break;
        }
        if dtype != DT_CONFIG {
            // Should not happen at the top level; skip rather than abort so a
            // device with an odd trailing descriptor still enumerates.
            pos += len;
            continue;
        }
        if len < 9 {
            return Err(bad("configuration descriptor shorter than 9 bytes"));
        }
        let total = u16le(blob, pos + 2) as usize;
        let end = (pos + total).min(blob.len());
        configs.push(parse_config(&blob[pos..end])?);
        if total < len {
            break;
        }
        pos += total;
    }

    Ok(Descriptors { device, configs })
}

fn parse_config(buf: &[u8]) -> io::Result<ConfigDescriptor> {
    let mut cfg = ConfigDescriptor {
        num_interfaces: buf[4],
        configuration_value: buf[5],
        alt_settings: Vec::new(),
    };

    let mut pos = buf[0] as usize;
    while pos + 2 <= buf.len() {
        let len = buf[pos] as usize;
        let dtype = buf[pos + 1];
        if len < 2 || pos + len > buf.len() {
            break;
        }
        match dtype {
            DT_INTERFACE if len >= 9 => cfg.alt_settings.push(AltSetting {
                interface_number: buf[pos + 2],
                alternate_setting: buf[pos + 3],
                class: buf[pos + 5],
                sub_class: buf[pos + 6],
                protocol: buf[pos + 7],
                endpoints: Vec::new(),
            }),
            DT_ENDPOINT if len >= 7 => {
                let ep = EndpointDescriptor {
                    address: buf[pos + 2],
                    attributes: buf[pos + 3],
                    max_packet_size: u16le(buf, pos + 4),
                    interval: buf[pos + 6],
                };
                match cfg.alt_settings.last_mut() {
                    Some(a) => a.endpoints.push(ep),
                    // An endpoint before any interface descriptor is malformed;
                    // dropping it is better than inventing an interface for it.
                    None => return Err(bad("endpoint descriptor outside any interface")),
                }
            }
            // HID, class-specific and vendor descriptors interleave here and
            // are simply not our business: USB/IP forwards transfers, not
            // semantics.
            _ => {}
        }
        pos += len;
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 7-interface composite in the shape of the Proteus puck: interfaces
    /// 0-1 internal comms, 2-5 controller slots, 6 pogo pins, each with one
    /// interrupt IN and one interrupt OUT endpoint.
    fn puck_blob() -> Vec<u8> {
        let mut dev = vec![
            18, DT_DEVICE, 0x00, 0x02, 0x00, 0x00, 0x00, 64, 0xde, 0x28, 0x04, 0x13, 0x00, 0x01, 1,
            2, 0, 1,
        ];
        let mut cfg_body = Vec::new();
        for ifn in 0..7u8 {
            cfg_body.extend_from_slice(&[9, DT_INTERFACE, ifn, 0, 2, 3, 0, 0, 0]);
            // A HID descriptor between the interface and its endpoints, which
            // is where real devices put one.
            cfg_body.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0x00, 1, 0x22, 0x40, 0x00]);
            cfg_body.extend_from_slice(&[7, DT_ENDPOINT, 0x81 + ifn, 0x03, 64, 0, 1]);
            cfg_body.extend_from_slice(&[7, DT_ENDPOINT, 0x01 + ifn, 0x03, 64, 0, 1]);
        }
        let total = 9 + cfg_body.len();
        let mut cfg = vec![
            9,
            DT_CONFIG,
            (total & 0xff) as u8,
            (total >> 8) as u8,
            7,
            1,
            0,
            0xa0,
            250,
        ];
        cfg.extend_from_slice(&cfg_body);
        dev.extend_from_slice(&cfg);
        dev
    }

    #[test]
    fn parses_the_device_descriptor() {
        let d = parse(&puck_blob()).unwrap();
        assert_eq!(d.device.id_vendor, 0x28de);
        assert_eq!(d.device.id_product, 0x1304);
        assert_eq!(d.device.b_num_configurations, 1);
    }

    #[test]
    fn finds_all_seven_interfaces_and_their_endpoints() {
        let d = parse(&puck_blob()).unwrap();
        let cfg = d.config(1).expect("config 1");
        assert_eq!(cfg.num_interfaces, 7);
        assert_eq!(cfg.interface_numbers(), vec![0, 1, 2, 3, 4, 5, 6]);
        // Interfaces 2..=5 are the controller slots SDL gates on; they must be
        // present or Steam rejects the device outright.
        for slot in 2..=5u8 {
            let a = cfg
                .alt_settings
                .iter()
                .find(|a| a.interface_number == slot)
                .expect("slot interface");
            assert_eq!(a.class, 3, "HID");
            assert_eq!(a.endpoints.len(), 2);
            assert_eq!(a.endpoints[0].transfer_type(), TransferType::Interrupt);
            assert!(a.endpoints[0].is_in());
            assert!(!a.endpoints[1].is_in());
        }
    }

    #[test]
    fn interleaved_hid_descriptors_do_not_shift_the_walk() {
        let d = parse(&puck_blob()).unwrap();
        let cfg = d.config(1).unwrap();
        assert_eq!(cfg.alt_settings.len(), 7);
        assert!(cfg.alt_settings.iter().all(|a| a.endpoints.len() == 2));
    }

    /// The wired 2026 controller: one unified interface, no interface-number
    /// gate in SDL, so it is the simpler of the two cases.
    fn wired_blob() -> Vec<u8> {
        let mut dev = vec![
            18, DT_DEVICE, 0x00, 0x02, 0, 0, 0, 64, 0xde, 0x28, 0x06, 0x13, 0x00, 0x01, 1, 2, 0, 1,
        ];
        let body = vec![
            9,
            DT_INTERFACE,
            0,
            0,
            2,
            3,
            0,
            0,
            0, //
            7,
            DT_ENDPOINT,
            0x82,
            0x03,
            64,
            0,
            1, //
            7,
            DT_ENDPOINT,
            0x02,
            0x03,
            64,
            0,
            1,
        ];
        let total = 9 + body.len();
        let mut cfg = vec![9, DT_CONFIG, total as u8, 0, 1, 1, 0, 0xa0, 250];
        cfg.extend_from_slice(&body);
        dev.extend_from_slice(&cfg);
        dev
    }

    #[test]
    fn parses_a_single_interface_device() {
        let d = parse(&wired_blob()).unwrap();
        let cfg = d.config(1).unwrap();
        assert_eq!(cfg.interface_numbers(), vec![0]);
        assert_eq!(cfg.primary_alt_settings().len(), 1);
    }

    #[test]
    fn alt_settings_collapse_to_one_entry_per_interface() {
        let body = vec![
            9,
            DT_INTERFACE,
            0,
            0,
            0,
            1,
            1,
            0,
            0, //
            9,
            DT_INTERFACE,
            0,
            1,
            1,
            1,
            2,
            0,
            0, //
            7,
            DT_ENDPOINT,
            0x81,
            0x01,
            192,
            0,
            1,
        ];
        let total = 9 + body.len();
        let mut blob = vec![
            18, DT_DEVICE, 0, 2, 0, 0, 0, 64, 0xde, 0x28, 0, 0, 0, 1, 0, 0, 0, 1,
        ];
        blob.extend_from_slice(&[9, DT_CONFIG, total as u8, 0, 1, 1, 0, 0x80, 50]);
        blob.extend_from_slice(&body);
        let cfg = parse(&blob).unwrap().configs.remove(0);
        assert_eq!(cfg.alt_settings.len(), 2);
        assert_eq!(cfg.interface_numbers(), vec![0]);
        let primary = cfg.primary_alt_settings();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].alternate_setting, 0);
    }

    #[test]
    fn multiple_configurations_are_kept_separate() {
        let mut blob = vec![
            18, DT_DEVICE, 0, 2, 0, 0, 0, 64, 0x01, 0x00, 0x02, 0x00, 0, 1, 0, 0, 0, 2,
        ];
        for value in [1u8, 2u8] {
            let body = vec![9, DT_INTERFACE, 0, 0, 0, value, 0, 0, 0];
            let total = 9 + body.len();
            blob.extend_from_slice(&[9, DT_CONFIG, total as u8, 0, 1, value, 0, 0x80, 50]);
            blob.extend_from_slice(&body);
        }
        let d = parse(&blob).unwrap();
        assert_eq!(d.configs.len(), 2);
        assert_eq!(d.config(2).unwrap().alt_settings[0].class, 2);
        assert!(d.config(3).is_none());
    }

    #[test]
    fn truncated_input_errors_rather_than_panics() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[18, DT_DEVICE, 0, 2]).is_err());
        // A config descriptor claiming more bytes than are present must be
        // clamped, not indexed past the end.
        let mut blob = vec![
            18, DT_DEVICE, 0, 2, 0, 0, 0, 64, 1, 0, 2, 0, 0, 1, 0, 0, 0, 1,
        ];
        blob.extend_from_slice(&[9, DT_CONFIG, 0xff, 0xff, 1, 1, 0, 0x80, 50]);
        let d = parse(&blob).unwrap();
        assert_eq!(d.configs.len(), 1);
        assert!(d.configs[0].alt_settings.is_empty());
    }

    #[test]
    fn wrong_leading_descriptor_type_is_rejected() {
        let mut blob = vec![9, DT_CONFIG, 9, 0, 1, 1, 0, 0x80, 50];
        blob.resize(24, 0);
        assert!(parse(&blob).is_err());
    }
}
