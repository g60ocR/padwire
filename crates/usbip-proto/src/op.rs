//! The connection-setup half of USB/IP: `OP_REQ_*` / `OP_REP_*`.
//!
//! Every message starts with the same 8-byte header (version, code, status).
//! A connection is in "op mode" until an `OP_REQ_IMPORT` succeeds, after which
//! both sides switch to the 48-byte command headers in [`crate::pdu`].

use crate::device::UsbDevice;
use crate::wire::{Reader, Writer};
use crate::{ProtoError, BUSID_LEN, SIZE_OP_HEADER, USBIP_VERSION};

type Result<T> = core::result::Result<T, ProtoError>;

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

/// `OP_REP_*` status codes. Anything non-zero means the request failed and no
/// payload follows.
pub const ST_OK: u32 = 0x00;
pub const ST_NA: u32 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

impl OpHeader {
    pub fn new(code: u16, status: u32) -> OpHeader {
        OpHeader {
            version: USBIP_VERSION,
            code,
            status,
        }
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.version);
        w.u16(self.code);
        w.u32(self.status);
    }

    pub fn to_bytes(&self) -> [u8; SIZE_OP_HEADER] {
        let mut w = Writer::with_capacity(SIZE_OP_HEADER);
        self.encode(&mut w);
        let v = w.into_vec();
        let mut a = [0u8; SIZE_OP_HEADER];
        a.copy_from_slice(&v);
        a
    }

    pub fn decode(r: &mut Reader) -> Result<OpHeader> {
        Ok(OpHeader {
            version: r.u16()?,
            code: r.u16()?,
            status: r.u32()?,
        })
    }

    pub fn from_bytes(b: &[u8]) -> Result<OpHeader> {
        OpHeader::decode(&mut Reader::new(b))
    }

    /// Reject a peer speaking a different revision rather than misparsing it.
    pub fn check_version(&self) -> Result<()> {
        if self.version == USBIP_VERSION {
            Ok(())
        } else {
            Err(ProtoError::BadVersion(self.version))
        }
    }
}

/// `OP_REQ_IMPORT` body: a single NUL-padded bus id.
pub fn encode_import_request(busid: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(SIZE_OP_HEADER + BUSID_LEN);
    OpHeader::new(OP_REQ_IMPORT, 0).encode(&mut w);
    w.text_field(busid, BUSID_LEN);
    w.into_vec()
}

pub fn decode_import_request_body(b: &[u8]) -> Result<String> {
    Reader::new(b).text_field(BUSID_LEN)
}

/// `OP_REP_IMPORT`: header plus, on success, the device record without interfaces.
pub fn encode_import_reply(dev: Option<&UsbDevice>) -> Vec<u8> {
    let mut w = Writer::with_capacity(SIZE_OP_HEADER + 312);
    match dev {
        Some(d) => {
            OpHeader::new(OP_REP_IMPORT, ST_OK).encode(&mut w);
            d.encode(&mut w, false);
        }
        None => OpHeader::new(OP_REP_IMPORT, ST_NA).encode(&mut w),
    }
    w.into_vec()
}

pub fn decode_import_reply_body(b: &[u8]) -> Result<UsbDevice> {
    UsbDevice::decode(&mut Reader::new(b), false)
}

/// `OP_REQ_DEVLIST` has no body beyond the header.
pub fn encode_devlist_request() -> Vec<u8> {
    OpHeader::new(OP_REQ_DEVLIST, 0).to_bytes().to_vec()
}

/// `OP_REP_DEVLIST`: header, device count, then each device with its interfaces.
pub fn encode_devlist_reply(devs: &[UsbDevice]) -> Vec<u8> {
    let mut w = Writer::with_capacity(SIZE_OP_HEADER + 4 + devs.len() * 340);
    OpHeader::new(OP_REP_DEVLIST, ST_OK).encode(&mut w);
    w.u32(devs.len() as u32);
    for d in devs {
        d.encode(&mut w, true);
    }
    w.into_vec()
}

pub fn decode_devlist_reply_body(b: &[u8]) -> Result<Vec<UsbDevice>> {
    let mut r = Reader::new(b);
    let n = r.u32()?;
    // The count is attacker-controlled; bound it before reserving.
    if n > 1024 {
        return Err(ProtoError::TooLarge {
            field: "devlist count",
            value: n as i64,
            max: 1024,
        });
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push(UsbDevice::decode(&mut r, true)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_header_is_eight_bytes_big_endian() {
        let h = OpHeader::new(OP_REQ_DEVLIST, 0);
        assert_eq!(h.to_bytes(), [0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0]);
        assert_eq!(OpHeader::from_bytes(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let mut h = OpHeader::new(OP_REQ_IMPORT, 0);
        h.version = 0x0106;
        assert!(matches!(
            h.check_version(),
            Err(ProtoError::BadVersion(0x0106))
        ));
    }

    #[test]
    fn import_request_roundtrip() {
        let buf = encode_import_request("1-2.3");
        assert_eq!(buf.len(), SIZE_OP_HEADER + BUSID_LEN);
        let h = OpHeader::from_bytes(&buf[..SIZE_OP_HEADER]).unwrap();
        assert_eq!(h.code, OP_REQ_IMPORT);
        assert_eq!(
            decode_import_request_body(&buf[SIZE_OP_HEADER..]).unwrap(),
            "1-2.3"
        );
    }

    #[test]
    fn import_failure_carries_no_body() {
        let buf = encode_import_reply(None);
        assert_eq!(buf.len(), SIZE_OP_HEADER);
        assert_eq!(OpHeader::from_bytes(&buf).unwrap().status, ST_NA);
    }

    #[test]
    fn devlist_roundtrip() {
        let d = UsbDevice {
            busid: "1-2".into(),
            b_num_interfaces: 2,
            interfaces: vec![Default::default(); 2],
            ..Default::default()
        };
        let buf = encode_devlist_reply(std::slice::from_ref(&d));
        let h = OpHeader::from_bytes(&buf[..SIZE_OP_HEADER]).unwrap();
        assert_eq!(h.code, OP_REP_DEVLIST);
        let got = decode_devlist_reply_body(&buf[SIZE_OP_HEADER..]).unwrap();
        assert_eq!(got, vec![d]);
    }

    #[test]
    fn absurd_device_count_is_refused_before_allocating() {
        let mut w = Writer::new();
        w.u32(u32::MAX);
        assert!(decode_devlist_reply_body(w.as_slice()).is_err());
    }
}
