//! Minimal little-endian encode/decode helpers for fixed on-disk structures.

use super::{OnDiskError, Result};

/// Appends little-endian fields to a byte buffer.
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn with_capacity(cap: usize) -> Self {
        Writer {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// Pad with zeros until the buffer length is `len`.
    pub fn pad_to(&mut self, len: usize) -> &mut Self {
        if self.buf.len() < len {
            self.buf.resize(len, 0);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads little-endian fields from a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(short)?;
        if end > self.buf.len() {
            return Err(short());
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
}

fn short() -> OnDiskError {
    OnDiskError::Inconsistent("structure shorter than expected".into())
}

/// CRC32C over `data`.
pub fn crc(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut w = Writer::with_capacity(32);
        w.u8(1).u16(2).u32(3).u64(4).bytes(b"xyz").pad_to(24);
        assert_eq!(w.len(), 24);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.u8().unwrap(), 1);
        assert_eq!(r.u16().unwrap(), 2);
        assert_eq!(r.u32().unwrap(), 3);
        assert_eq!(r.u64().unwrap(), 4);
        assert_eq!(r.bytes(3).unwrap(), b"xyz");
        assert_eq!(r.position(), 18);
        r.skip(6).unwrap();
        assert!(r.u8().is_err());
    }
}
