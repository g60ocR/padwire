//! Framing helpers: the thinnest possible layer that turns a byte stream into
//! whole PDUs. All the parsing lives in [`crate::pdu`] and [`crate::op`]; this
//! module only decides how many bytes to read next.

use std::io::{self, Read, Write};

use crate::device::UsbDevice;
use crate::op::{OpHeader, OP_REP_DEVLIST, OP_REP_IMPORT, OP_REQ_DEVLIST, OP_REQ_IMPORT, ST_OK};
use crate::pdu::{Body, Direction, Header, IsoPacketDescriptor, RetSubmit};
use crate::wire::Reader;
use crate::{
    ProtoError, BUSID_LEN, SIZE_CMD_HEADER, SIZE_ISO_DESC, SIZE_OP_HEADER, SIZE_USB_DEVICE,
    SIZE_USB_INTERFACE,
};

/// A complete `USBIP_CMD_*` message: header plus whatever trailed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    pub header: Header,
    pub data: Vec<u8>,
    pub iso: Vec<IsoPacketDescriptor>,
}

/// What a server may see once a connection has switched to command mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpRequest {
    DevList,
    Import { busid: String },
}

pub fn read_op_header<R: Read>(r: &mut R) -> io::Result<OpHeader> {
    let mut buf = [0u8; SIZE_OP_HEADER];
    r.read_exact(&mut buf)?;
    Ok(OpHeader::from_bytes(&buf)?)
}

/// Read an `OP_REQ_*` and its body. Returns `None` for a well-formed request
/// this server does not implement, so the caller can answer rather than hang.
pub fn read_op_request<R: Read>(r: &mut R) -> io::Result<Option<OpRequest>> {
    let h = read_op_header(r)?;
    h.check_version()?;
    match h.code {
        OP_REQ_DEVLIST => Ok(Some(OpRequest::DevList)),
        OP_REQ_IMPORT => {
            let mut buf = [0u8; BUSID_LEN];
            r.read_exact(&mut buf)?;
            let busid = Reader::new(&buf).text_field(BUSID_LEN)?;
            Ok(Some(OpRequest::Import { busid }))
        }
        _ => Ok(None),
    }
}

/// More devices than any real exporter has; bounds the allocation a hostile
/// count field could otherwise ask for.
pub const MAX_DEVLIST: u32 = 1024;

fn expect_reply(h: &OpHeader, code: u16) -> io::Result<()> {
    h.check_version()?;
    if h.code != code {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected reply 0x{code:04x}, got 0x{:04x}", h.code),
        ));
    }
    if h.status != ST_OK {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("peer refused the request (status {})", h.status),
        ));
    }
    Ok(())
}

/// Read an `OP_REP_DEVLIST`.
///
/// Each record is variable length — the interface array follows the fixed part
/// and its length is the last byte of that part — so this reads one device at
/// a time rather than slurping a frame whose size is not known in advance.
pub fn read_devlist_reply<R: Read>(r: &mut R) -> io::Result<Vec<UsbDevice>> {
    expect_reply(&read_op_header(r)?, OP_REP_DEVLIST)?;
    let mut count = [0u8; 4];
    r.read_exact(&mut count)?;
    let n = u32::from_be_bytes(count);
    if n > MAX_DEVLIST {
        return Err(ProtoError::TooLarge {
            field: "devlist count",
            value: n as i64,
            max: MAX_DEVLIST as i64,
        }
        .into());
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let mut buf = vec![0u8; SIZE_USB_DEVICE];
        r.read_exact(&mut buf)?;
        // b_num_interfaces is the final byte of the fixed-size record.
        let ifs = buf[SIZE_USB_DEVICE - 1] as usize;
        buf.resize(SIZE_USB_DEVICE + ifs * SIZE_USB_INTERFACE, 0);
        r.read_exact(&mut buf[SIZE_USB_DEVICE..])?;
        out.push(UsbDevice::decode(&mut Reader::new(&buf), true)?);
    }
    Ok(out)
}

/// Read an `OP_REP_IMPORT`. A refusal surfaces as an error rather than a
/// device, since there is nothing useful to return in that case.
pub fn read_import_reply<R: Read>(r: &mut R) -> io::Result<UsbDevice> {
    expect_reply(&read_op_header(r)?, OP_REP_IMPORT)?;
    let mut buf = vec![0u8; SIZE_USB_DEVICE];
    r.read_exact(&mut buf)?;
    Ok(UsbDevice::decode(&mut Reader::new(&buf), false)?)
}

/// Read one command PDU.
///
/// `max_transfer` bounds `transfer_buffer_length` before anything is allocated.
/// This is a network-facing parser, so the length prefix is treated as hostile
/// even though the transport is normally a tailnet.
pub fn read_pdu<R: Read>(r: &mut R, max_transfer: usize) -> io::Result<Pdu> {
    let mut hbuf = [0u8; SIZE_CMD_HEADER];
    r.read_exact(&mut hbuf)?;
    let header = Header::decode(&hbuf)?;

    let (data_len, iso_n) = match header.body {
        Body::CmdSubmit(c) => {
            if c.transfer_buffer_length < 0 || c.transfer_buffer_length as usize > max_transfer {
                return Err(ProtoError::TooLarge {
                    field: "transfer_buffer_length",
                    value: c.transfer_buffer_length as i64,
                    max: max_transfer as i64,
                }
                .into());
            }
            if c.iso_count() > MAX_ISO_PACKETS {
                return Err(ProtoError::TooLarge {
                    field: "number_of_packets",
                    value: c.number_of_packets as i64,
                    max: MAX_ISO_PACKETS as i64,
                }
                .into());
            }
            (c.payload_len(header.base.dir()), c.iso_count())
        }
        Body::CmdUnlink(_) => (0, 0),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "expected a USBIP_CMD_* message, got command {}",
                    header.base.command
                ),
            ))
        }
    };

    let mut data = vec![0u8; data_len];
    r.read_exact(&mut data)?;

    let mut iso = Vec::with_capacity(iso_n);
    if iso_n > 0 {
        let mut ibuf = vec![0u8; iso_n * SIZE_ISO_DESC];
        r.read_exact(&mut ibuf)?;
        let mut rd = Reader::new(&ibuf);
        for _ in 0..iso_n {
            iso.push(IsoPacketDescriptor::decode(&mut rd)?);
        }
    }

    Ok(Pdu { header, data, iso })
}

/// A submit big enough to need this many isochronous packets is not something
/// a HID device produces; refuse rather than allocate.
pub const MAX_ISO_PACKETS: usize = 1024;

pub fn write_header<W: Write>(w: &mut W, h: &Header) -> io::Result<()> {
    w.write_all(&h.to_bytes())
}

/// Write a `USBIP_RET_SUBMIT` and its payload as a single `write_all`.
///
/// One syscall matters here: with `TCP_NODELAY` set, splitting the header and
/// the payload puts two small segments on the wire per input report.
pub fn write_ret_submit<W: Write>(
    w: &mut W,
    seqnum: u32,
    ret: RetSubmit,
    data: &[u8],
) -> io::Result<()> {
    let h = Header::ret_submit(seqnum, ret);
    let mut buf = Vec::with_capacity(SIZE_CMD_HEADER + data.len());
    buf.extend_from_slice(&h.to_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

pub fn write_ret_unlink<W: Write>(w: &mut W, seqnum: u32, status: i32) -> io::Result<()> {
    write_header(w, &Header::ret_unlink(seqnum, status))
}

/// Read a `USBIP_RET_SUBMIT` reply. The caller supplies the direction of the
/// original submit because the reply header does not carry it.
pub fn read_ret_submit<R: Read>(
    r: &mut R,
    dir_for: impl FnOnce(u32) -> Option<Direction>,
    max_transfer: usize,
) -> io::Result<(Header, Vec<u8>)> {
    let mut hbuf = [0u8; SIZE_CMD_HEADER];
    r.read_exact(&mut hbuf)?;
    let header = Header::decode(&hbuf)?;
    let ret = match header.body {
        Body::RetSubmit(r) => r,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected USBIP_RET_SUBMIT",
            ))
        }
    };
    let dir = dir_for(header.base.seqnum).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("reply for unknown seqnum {}", header.base.seqnum),
        )
    })?;
    let n = ret.payload_len(dir);
    if n > max_transfer {
        return Err(ProtoError::TooLarge {
            field: "actual_length",
            value: n as i64,
            max: max_transfer as i64,
        }
        .into());
    }
    let mut data = vec![0u8; n];
    r.read_exact(&mut data)?;
    Ok((header, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{encode_devlist_request, encode_import_request};
    use crate::pdu::{BasicHeader, CmdSubmit, CmdUnlink, DIR_IN, DIR_OUT};
    use crate::wire::Writer;

    fn submit(dir: u32, len: i32, npkt: i32) -> Header {
        Header::cmd_submit(
            BasicHeader {
                command: 0,
                seqnum: 5,
                devid: 1,
                direction: dir,
                ep: 2,
            },
            CmdSubmit {
                transfer_flags: 0,
                transfer_buffer_length: len,
                start_frame: 0,
                number_of_packets: npkt,
                interval: 0,
                setup: [0; 8],
            },
        )
    }

    #[test]
    fn reads_an_out_submit_with_its_payload() {
        let mut w = Writer::new();
        submit(DIR_OUT, 4, -1).encode(&mut w);
        w.bytes(&[1, 2, 3, 4]);
        let buf = w.into_vec();
        let pdu = read_pdu(&mut &buf[..], 65536).unwrap();
        assert_eq!(pdu.data, vec![1, 2, 3, 4]);
        assert!(pdu.iso.is_empty());
    }

    #[test]
    fn an_in_submit_has_no_payload_despite_a_nonzero_length() {
        let mut w = Writer::new();
        submit(DIR_IN, 64, 0).encode(&mut w);
        let buf = w.into_vec();
        let mut cur = &buf[..];
        let pdu = read_pdu(&mut cur, 65536).unwrap();
        assert!(pdu.data.is_empty());
        assert_eq!(cur.len(), 0, "must not over-read into the next PDU");
    }

    #[test]
    fn two_pdus_back_to_back_stay_in_frame() {
        let mut w = Writer::new();
        submit(DIR_OUT, 2, -1).encode(&mut w);
        w.bytes(&[0xaa, 0xbb]);
        Header {
            base: BasicHeader {
                command: crate::pdu::USBIP_CMD_UNLINK,
                seqnum: 6,
                devid: 1,
                direction: 0,
                ep: 0,
            },
            body: Body::CmdUnlink(CmdUnlink { unlink_seqnum: 5 }),
        }
        .encode(&mut w);
        let buf = w.into_vec();
        let mut cur = &buf[..];
        assert_eq!(read_pdu(&mut cur, 65536).unwrap().data, vec![0xaa, 0xbb]);
        let second = read_pdu(&mut cur, 65536).unwrap();
        assert_eq!(second.header.base.seqnum, 6);
        assert_eq!(cur.len(), 0);
    }

    #[test]
    fn iso_descriptors_are_consumed_so_the_stream_stays_aligned() {
        // We refuse to *service* isochronous transfers, but we still have to
        // parse them or the next PDU lands mid-frame.
        let mut w = Writer::new();
        submit(DIR_OUT, 0, 3).encode(&mut w);
        for i in 0..3u32 {
            IsoPacketDescriptor {
                offset: i,
                length: 8,
                actual_length: 0,
                status: 0,
            }
            .encode(&mut w);
        }
        let buf = w.into_vec();
        let mut cur = &buf[..];
        let pdu = read_pdu(&mut cur, 65536).unwrap();
        assert_eq!(pdu.iso.len(), 3);
        assert_eq!(pdu.iso[2].offset, 2);
        assert_eq!(cur.len(), 0);
    }

    #[test]
    fn an_oversized_transfer_is_refused_before_allocating() {
        let mut w = Writer::new();
        submit(DIR_OUT, i32::MAX, 0).encode(&mut w);
        let buf = w.into_vec();
        let err = read_pdu(&mut &buf[..], 4096).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn op_requests_decode() {
        let b = encode_devlist_request();
        assert_eq!(
            read_op_request(&mut &b[..]).unwrap(),
            Some(OpRequest::DevList)
        );
        let b = encode_import_request("1-4");
        assert_eq!(
            read_op_request(&mut &b[..]).unwrap(),
            Some(OpRequest::Import {
                busid: "1-4".into()
            })
        );
    }

    #[test]
    fn devlist_replies_of_mixed_interface_counts_stay_in_frame() {
        use crate::device::{UsbDevice, UsbInterface};
        let mk = |busid: &str, n: u8| UsbDevice {
            busid: busid.into(),
            b_num_interfaces: n,
            interfaces: vec![
                UsbInterface {
                    class: 3,
                    subclass: 0,
                    protocol: 0
                };
                n as usize
            ],
            ..Default::default()
        };
        // A 7-interface puck next to a 1-interface wired controller: the
        // records are different lengths, which is exactly where an
        // off-by-one framing bug would show up.
        let devs = vec![mk("3-12", 7), mk("1-4", 1), mk("2-1", 0)];
        let wire = crate::op::encode_devlist_reply(&devs);
        let mut cur = &wire[..];
        assert_eq!(read_devlist_reply(&mut cur).unwrap(), devs);
        assert_eq!(cur.len(), 0);
    }

    #[test]
    fn a_refused_reply_is_an_error_not_an_empty_list() {
        let wire = crate::op::encode_import_reply(None);
        let err = read_import_reply(&mut &wire[..]).unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[test]
    fn an_import_reply_carries_no_interface_array() {
        use crate::device::UsbDevice;
        let d = UsbDevice {
            busid: "3-12".into(),
            b_num_interfaces: 7,
            ..Default::default()
        };
        let wire = crate::op::encode_import_reply(Some(&d));
        assert_eq!(wire.len(), SIZE_OP_HEADER + SIZE_USB_DEVICE);
        let got = read_import_reply(&mut &wire[..]).unwrap();
        assert_eq!(got.busid, "3-12");
        assert!(got.interfaces.is_empty());
    }

    #[test]
    fn ret_submit_writes_header_and_payload_together() {
        let mut out = Vec::new();
        write_ret_submit(
            &mut out,
            7,
            RetSubmit {
                status: 0,
                actual_length: 3,
                ..Default::default()
            },
            &[9, 8, 7],
        )
        .unwrap();
        assert_eq!(out.len(), SIZE_CMD_HEADER + 3);
        let (h, data) = read_ret_submit(
            &mut &out[..],
            |s| {
                assert_eq!(s, 7);
                Some(Direction::In)
            },
            65536,
        )
        .unwrap();
        assert_eq!(h.base.seqnum, 7);
        assert_eq!(data, vec![9, 8, 7]);
    }
}
