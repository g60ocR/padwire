//! Big-endian cursor helpers.
//!
//! Every multi-byte field in USB/IP is network byte order, so these are the only
//! integer accessors the crate uses. Both halves are total functions over byte
//! slices: nothing here touches a socket.

use crate::ProtoError;

type Result<T> = core::result::Result<T, ProtoError>;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(ProtoError::Truncated {
                need: n,
                have: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let b = self.bytes(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(b);
        Ok(a)
    }

    /// A fixed-width NUL-padded text field (`path[256]`, `busid[32]`).
    pub fn text_field(&mut self, n: usize) -> Result<String> {
        let b = self.bytes(n)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        core::str::from_utf8(&b[..end])
            .map(|s| s.to_owned())
            .map_err(|_| ProtoError::Utf8)
    }
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Writer {
            buf: Vec::with_capacity(n),
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }

    /// Write `s` into a fixed-width NUL-padded field, truncating if it does not fit.
    pub fn text_field(&mut self, s: &str, n: usize) {
        let src = s.as_bytes();
        let take = core::cmp::min(src.len(), n.saturating_sub(1));
        self.buf.extend_from_slice(&src[..take]);
        self.zeros(n - take);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_scalars() {
        let mut w = Writer::new();
        w.u8(0x11);
        w.u16(0x2233);
        w.u32(0x44556677);
        w.i32(-2);
        w.u64(0x0102030405060708);
        let v = w.into_vec();
        // Explicitly big-endian on the wire.
        assert_eq!(&v[..7], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
        assert_eq!(&v[7..11], &[0xff, 0xff, 0xff, 0xfe]);

        let mut r = Reader::new(&v);
        assert_eq!(r.u8().unwrap(), 0x11);
        assert_eq!(r.u16().unwrap(), 0x2233);
        assert_eq!(r.u32().unwrap(), 0x44556677);
        assert_eq!(r.i32().unwrap(), -2);
        assert_eq!(r.u64().unwrap(), 0x0102030405060708);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn text_field_is_nul_padded_and_truncated() {
        let mut w = Writer::new();
        w.text_field("1-2", 8);
        assert_eq!(w.as_slice(), b"1-2\0\0\0\0\0");

        let mut w = Writer::new();
        w.text_field("abcdefghij", 4);
        // Always leaves room for the terminator.
        assert_eq!(w.as_slice(), b"abc\0");

        let mut r = Reader::new(b"1-2\0\0\0\0\0");
        assert_eq!(r.text_field(8).unwrap(), "1-2");
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut r = Reader::new(&[0u8; 3]);
        assert!(matches!(
            r.u32(),
            Err(ProtoError::Truncated { need: 4, have: 3 })
        ));
    }
}
