//! The URB-carrying half of USB/IP: `USBIP_CMD_*` / `USBIP_RET_*`.
//!
//! Layout is a 20-byte `usbip_header_basic` followed by a 28-byte command
//! block, 48 bytes in total, then any payload.
//!
//! One asymmetry is worth stating up front because it drives the API here: the
//! kernel's stub zeroes `devid`, `ep` and `direction` on every reply
//! (`stub_tx.c:setup_base_pdu`), so a `USBIP_RET_SUBMIT` header does *not* say
//! whether a payload follows. Only the sender of the original
//! `USBIP_CMD_SUBMIT` knows, by matching `seqnum` against its own pending URB.
//! [`RetSubmit::payload_len`] therefore takes the direction as an argument
//! instead of reading it off the header.

use crate::wire::{Reader, Writer};
use crate::{ProtoError, SIZE_CMD_HEADER, SIZE_ISO_DESC};

type Result<T> = core::result::Result<T, ProtoError>;

pub const USBIP_CMD_SUBMIT: u32 = 0x0001;
pub const USBIP_CMD_UNLINK: u32 = 0x0002;
pub const USBIP_RET_SUBMIT: u32 = 0x0003;
pub const USBIP_RET_UNLINK: u32 = 0x0004;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

impl Direction {
    pub fn from_raw(v: u32) -> Direction {
        if v == DIR_IN {
            Direction::In
        } else {
            Direction::Out
        }
    }

    pub fn as_raw(self) -> u32 {
        match self {
            Direction::Out => DIR_OUT,
            Direction::In => DIR_IN,
        }
    }

    pub fn is_in(self) -> bool {
        self == Direction::In
    }
}

/// Kernel URB flags that USB/IP passes through in `transfer_flags`.
pub mod transfer_flags {
    pub const SHORT_NOT_OK: u32 = 0x0000_0001;
    pub const ISO_ASAP: u32 = 0x0000_0002;
    pub const ZERO_PACKET: u32 = 0x0000_0040;
    pub const NO_INTERRUPT: u32 = 0x0000_0080;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BasicHeader {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

impl BasicHeader {
    pub fn dir(&self) -> Direction {
        Direction::from_raw(self.direction)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdSubmit {
    pub transfer_flags: u32,
    pub transfer_buffer_length: i32,
    pub start_frame: i32,
    /// Signed on purpose. The protocol documentation says non-isochronous
    /// submits set this to `0xffffffff`, while the in-tree `vhci-hcd` client
    /// sends `0`. Reading it as `i32` makes both mean "not isochronous".
    pub number_of_packets: i32,
    pub interval: i32,
    pub setup: [u8; 8],
}

impl CmdSubmit {
    pub fn is_iso(&self) -> bool {
        self.number_of_packets > 0
    }

    pub fn iso_count(&self) -> usize {
        if self.is_iso() {
            self.number_of_packets as usize
        } else {
            0
        }
    }

    /// Bytes of transfer data that follow the header. Only OUT submits carry
    /// data; an IN submit is just a request for the peer to fill a buffer.
    pub fn payload_len(&self, dir: Direction) -> usize {
        if dir.is_in() {
            0
        } else {
            self.transfer_buffer_length.max(0) as usize
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetSubmit {
    /// Negative Linux errno, or 0. Passed straight through from `urb->status`.
    pub status: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
}

impl RetSubmit {
    /// See the module note: `dir` is the direction of the *original* submit,
    /// which the reply header does not repeat.
    pub fn payload_len(&self, dir: Direction) -> usize {
        if dir.is_in() {
            self.actual_length.max(0) as usize
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CmdUnlink {
    /// The `seqnum` of the submit being cancelled.
    pub unlink_seqnum: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetUnlink {
    /// `-ECONNRESET` if the URB was still in flight and got cancelled, 0 if it
    /// had already completed.
    pub status: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    CmdSubmit(CmdSubmit),
    RetSubmit(RetSubmit),
    CmdUnlink(CmdUnlink),
    RetUnlink(RetUnlink),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub base: BasicHeader,
    pub body: Body,
}

impl Header {
    pub fn cmd_submit(base: BasicHeader, c: CmdSubmit) -> Header {
        Header {
            base: BasicHeader {
                command: USBIP_CMD_SUBMIT,
                ..base
            },
            body: Body::CmdSubmit(c),
        }
    }

    /// Build a `USBIP_RET_SUBMIT` for `seqnum`.
    ///
    /// `devid`, `ep` and `direction` are zeroed to match `stub_tx.c`; the
    /// client correlates on `seqnum` alone.
    pub fn ret_submit(seqnum: u32, r: RetSubmit) -> Header {
        Header {
            base: BasicHeader {
                command: USBIP_RET_SUBMIT,
                seqnum,
                devid: 0,
                direction: 0,
                ep: 0,
            },
            body: Body::RetSubmit(r),
        }
    }

    pub fn ret_unlink(seqnum: u32, status: i32) -> Header {
        Header {
            base: BasicHeader {
                command: USBIP_RET_UNLINK,
                seqnum,
                devid: 0,
                direction: 0,
                ep: 0,
            },
            body: Body::RetUnlink(RetUnlink { status }),
        }
    }

    pub fn encode(&self, w: &mut Writer) {
        let start = w.len();
        w.u32(self.base.command);
        w.u32(self.base.seqnum);
        w.u32(self.base.devid);
        w.u32(self.base.direction);
        w.u32(self.base.ep);
        match self.body {
            Body::CmdSubmit(c) => {
                w.u32(c.transfer_flags);
                w.i32(c.transfer_buffer_length);
                w.i32(c.start_frame);
                w.i32(c.number_of_packets);
                w.i32(c.interval);
                w.bytes(&c.setup);
            }
            Body::RetSubmit(r) => {
                w.i32(r.status);
                w.i32(r.actual_length);
                w.i32(r.start_frame);
                w.i32(r.number_of_packets);
                w.i32(r.error_count);
                w.u64(0); // padding
            }
            Body::CmdUnlink(u) => {
                w.u32(u.unlink_seqnum);
                w.zeros(24);
            }
            Body::RetUnlink(u) => {
                w.i32(u.status);
                w.zeros(24);
            }
        }
        debug_assert_eq!(w.len() - start, SIZE_CMD_HEADER);
    }

    pub fn to_bytes(&self) -> [u8; SIZE_CMD_HEADER] {
        let mut w = Writer::with_capacity(SIZE_CMD_HEADER);
        self.encode(&mut w);
        let v = w.into_vec();
        let mut a = [0u8; SIZE_CMD_HEADER];
        a.copy_from_slice(&v);
        a
    }

    pub fn decode(buf: &[u8]) -> Result<Header> {
        let mut r = Reader::new(buf);
        let base = BasicHeader {
            command: r.u32()?,
            seqnum: r.u32()?,
            devid: r.u32()?,
            direction: r.u32()?,
            ep: r.u32()?,
        };
        let body = match base.command {
            USBIP_CMD_SUBMIT => Body::CmdSubmit(CmdSubmit {
                transfer_flags: r.u32()?,
                transfer_buffer_length: r.i32()?,
                start_frame: r.i32()?,
                number_of_packets: r.i32()?,
                interval: r.i32()?,
                setup: r.array::<8>()?,
            }),
            USBIP_RET_SUBMIT => {
                let b = RetSubmit {
                    status: r.i32()?,
                    actual_length: r.i32()?,
                    start_frame: r.i32()?,
                    number_of_packets: r.i32()?,
                    error_count: r.i32()?,
                };
                r.skip(8)?;
                Body::RetSubmit(b)
            }
            USBIP_CMD_UNLINK => {
                let b = CmdUnlink {
                    unlink_seqnum: r.u32()?,
                };
                r.skip(24)?;
                Body::CmdUnlink(b)
            }
            USBIP_RET_UNLINK => {
                let b = RetUnlink { status: r.i32()? };
                r.skip(24)?;
                Body::RetUnlink(b)
            }
            other => return Err(ProtoError::BadCommand(other)),
        };
        Ok(Header { base, body })
    }

    /// Isochronous packet descriptors that follow the payload, in bytes.
    pub fn iso_bytes(&self) -> usize {
        match self.body {
            Body::CmdSubmit(c) => c.iso_count() * SIZE_ISO_DESC,
            Body::RetSubmit(r) => r.number_of_packets.max(0) as usize * SIZE_ISO_DESC,
            _ => 0,
        }
    }
}

/// `struct usbip_iso_packet_descriptor` — 16 bytes, unlike the kernel's own
/// 12-byte `usbdevfs_iso_packet_desc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoPacketDescriptor {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: u32,
}

impl IsoPacketDescriptor {
    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.offset);
        w.u32(self.length);
        w.u32(self.actual_length);
        w.u32(self.status);
    }

    pub fn decode(r: &mut Reader) -> Result<IsoPacketDescriptor> {
        Ok(IsoPacketDescriptor {
            offset: r.u32()?,
            length: r.u32()?,
            actual_length: r.u32()?,
            status: r.u32()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_header_is_48_bytes() {
        let h = Header::cmd_submit(
            BasicHeader {
                command: 0,
                seqnum: 1,
                devid: 0x10007,
                direction: DIR_IN,
                ep: 3,
            },
            CmdSubmit {
                transfer_flags: 0,
                transfer_buffer_length: 64,
                start_frame: 0,
                number_of_packets: -1,
                interval: 1,
                setup: [0; 8],
            },
        );
        assert_eq!(h.to_bytes().len(), 48);
        assert_eq!(Header::decode(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn all_four_bodies_roundtrip_at_48_bytes() {
        let base = BasicHeader {
            command: 0,
            seqnum: 42,
            devid: 7,
            direction: DIR_OUT,
            ep: 1,
        };
        let hs = [
            Header::cmd_submit(
                base,
                CmdSubmit {
                    transfer_flags: transfer_flags::SHORT_NOT_OK,
                    transfer_buffer_length: 8,
                    start_frame: 0,
                    number_of_packets: 0,
                    interval: 0,
                    setup: [0x21, 0x09, 0, 3, 0, 0, 64, 0],
                },
            ),
            Header::ret_submit(
                42,
                RetSubmit {
                    status: -71,
                    actual_length: 8,
                    start_frame: 0,
                    number_of_packets: 0,
                    error_count: 0,
                },
            ),
            Header {
                base: BasicHeader {
                    command: USBIP_CMD_UNLINK,
                    ..base
                },
                body: Body::CmdUnlink(CmdUnlink { unlink_seqnum: 41 }),
            },
            Header::ret_unlink(42, -104),
        ];
        for h in hs {
            let b = h.to_bytes();
            assert_eq!(b.len(), 48);
            assert_eq!(Header::decode(&b).unwrap(), h);
        }
    }

    #[test]
    fn replies_zero_the_routing_fields() {
        // stub_tx.c:setup_base_pdu zeroes these; matching it keeps us
        // indistinguishable from the in-tree server.
        let h = Header::ret_submit(9, RetSubmit::default());
        assert_eq!(h.base.devid, 0);
        assert_eq!(h.base.ep, 0);
        assert_eq!(h.base.direction, 0);
        assert_eq!(h.base.seqnum, 9);
    }

    #[test]
    fn both_non_iso_encodings_read_as_non_iso() {
        for n in [0i32, -1i32] {
            let c = CmdSubmit {
                transfer_flags: 0,
                transfer_buffer_length: 0,
                start_frame: 0,
                number_of_packets: n,
                interval: 0,
                setup: [0; 8],
            };
            assert!(!c.is_iso(), "number_of_packets={n} should not mean iso");
            assert_eq!(c.iso_count(), 0);
        }
        let c = CmdSubmit {
            transfer_flags: 0,
            transfer_buffer_length: 0,
            start_frame: 0,
            number_of_packets: 4,
            interval: 0,
            setup: [0; 8],
        };
        assert!(c.is_iso());
        assert_eq!(c.iso_count(), 4);
    }

    #[test]
    fn payload_follows_only_the_data_carrying_direction() {
        let c = CmdSubmit {
            transfer_flags: 0,
            transfer_buffer_length: 64,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
        };
        assert_eq!(c.payload_len(Direction::Out), 64);
        assert_eq!(c.payload_len(Direction::In), 0);

        let r = RetSubmit {
            status: 0,
            actual_length: 20,
            ..Default::default()
        };
        assert_eq!(r.payload_len(Direction::In), 20);
        assert_eq!(r.payload_len(Direction::Out), 0);
    }

    #[test]
    fn negative_lengths_do_not_underflow() {
        let c = CmdSubmit {
            transfer_flags: 0,
            transfer_buffer_length: -5,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
        };
        assert_eq!(c.payload_len(Direction::Out), 0);
    }

    #[test]
    fn unknown_command_is_rejected() {
        let mut w = Writer::new();
        w.u32(99);
        w.zeros(44);
        assert!(matches!(
            Header::decode(w.as_slice()),
            Err(ProtoError::BadCommand(99))
        ));
    }

    #[test]
    fn iso_descriptor_is_16_bytes() {
        let d = IsoPacketDescriptor {
            offset: 1,
            length: 2,
            actual_length: 3,
            status: 4,
        };
        let mut w = Writer::new();
        d.encode(&mut w);
        assert_eq!(w.len(), SIZE_ISO_DESC);
        assert_eq!(
            IsoPacketDescriptor::decode(&mut Reader::new(w.as_slice())).unwrap(),
            d
        );
    }
}
