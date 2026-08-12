//! Non-panicking big-endian cursor over an untrusted byte slice.
//!
//! Every method returns `Option`; nothing here indexes or slices with `[a..b]`
//! or calls `unwrap`, so a truncated/malformed buffer can never panic.

/// A cursor over `&'a [u8]` that reads big-endian integers and byte slices.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Reads a big-endian `u32` without advancing the cursor.
    pub fn peek_u32(&self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let bytes = self.buf.get(self.pos..end)?;
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    pub fn read_u16(&mut self) -> Option<u16> {
        let end = self.pos.checked_add(2)?;
        let bytes = self.buf.get(self.pos..end)?;
        let v = u16::from_be_bytes(bytes.try_into().ok()?);
        self.pos = end;
        Some(v)
    }

    pub fn read_u32(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let bytes = self.buf.get(self.pos..end)?;
        let v = u32::from_be_bytes(bytes.try_into().ok()?);
        self.pos = end;
        Some(v)
    }

    pub fn read_u64(&mut self) -> Option<u64> {
        let end = self.pos.checked_add(8)?;
        let bytes = self.buf.get(self.pos..end)?;
        let v = u64::from_be_bytes(bytes.try_into().ok()?);
        self.pos = end;
        Some(v)
    }

    /// Reads exactly `n` bytes, or `None` if fewer than `n` remain.
    pub fn read_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let bytes = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }

    /// Consumes and returns everything left in the buffer.
    pub fn read_rest(&mut self) -> &'a [u8] {
        let bytes = self.buf.get(self.pos..).unwrap_or(&[]);
        self.pos = self.buf.len();
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_be_values() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0xABCDu16.to_be_bytes());
        buf.extend_from_slice(&0x11223344u32.to_be_bytes());
        buf.extend_from_slice(&0x0102030405060708u64.to_be_bytes());
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_u16(), Some(0xABCD));
        assert_eq!(r.read_u32(), Some(0x11223344));
        assert_eq!(r.read_u64(), Some(0x0102030405060708));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn truncated_buffer_returns_none() {
        let buf = [0x00u8, 0x01];
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_u32(), None);
        assert_eq!(r.read_u16(), Some(0x0001));
    }

    #[test]
    fn peek_u32_does_not_advance() {
        let buf = 0xDEADBEEFu32.to_be_bytes();
        let r = Reader::new(&buf);
        assert_eq!(r.peek_u32(), Some(0xDEADBEEF));
        assert_eq!(r.peek_u32(), Some(0xDEADBEEF));
        assert_eq!(r.remaining(), 4);
    }

    #[test]
    fn read_bytes_and_rest() {
        let buf = [1u8, 2, 3, 4, 5];
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_bytes(2), Some(&[1u8, 2][..]));
        assert_eq!(r.read_bytes(10), None);
        assert_eq!(r.read_rest(), &[3u8, 4, 5][..]);
    }

    #[test]
    fn read_bytes_exact_remaining_ok() {
        let buf = [9u8, 8, 7];
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_bytes(3), Some(&[9u8, 8, 7][..]));
        assert_eq!(r.remaining(), 0);
    }
}
