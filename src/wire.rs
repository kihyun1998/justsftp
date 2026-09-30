//! The wire cursor: fields in and out of one packet body, and the outer frame. Hand-rolled, with no
//! `serde` (docs/map/invariant/paths-are-bytes.md, docs/map/territory/wire-cursor.md).

use crate::error::{Error, Result};

/// Reads fields out of one already-framed packet body, borrowing it.
#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The next `n` bytes, or `Truncated` with both numbers. Every read goes through here
    /// (docs/map/invariant/the-far-end-sizes-nothing.md).
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Truncated {
                needed: n,
                had: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// An SSH `string`: a `u32` count and then exactly that many bytes, handed back **as bytes**.
    pub(crate) fn string(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    /// A field the spec defines as text, decoded lossily. Only the STATUS error message and its
    /// language tag may call this (docs/map/invariant/paths-are-bytes.md).
    pub(crate) fn text(&mut self) -> Result<String> {
        let bytes = self.string()?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Everything not yet read, consumed. For a body whose layout the reader does not know.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let out = &self.buf[self.pos..];
        self.pos = self.buf.len();
        out
    }
}

/// Builds one packet body.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// An SSH `string`. Takes bytes, because on this wire that is what a string is.
    pub(crate) fn string(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    /// Bytes appended as they are, with no length prefix.
    pub(crate) fn raw(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

/// Wraps a body in the outer frame: `u32 length | u8 type | body`, where the length counts the
/// type byte (docs/map/territory/wire-cursor.md).
pub(crate) fn frame(packet_type: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.extend_from_slice(&((body.len() as u32) + 1).to_be_bytes());
    out.push(packet_type);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_is_a_length_and_raw_bytes_and_is_never_decoded() {
        // 한글.txt in EUC-KR, which is not valid UTF-8 (docs/map/invariant/paths-are-bytes.md).
        //
        // Mutation that reddens this: make `string()` return
        // `String::from_utf8_lossy(..).into_bytes()`.
        // A `Vec`, not a literal: against a literal `invalid_from_utf8` fires at build time, and the
        // fixture's precondition is meant to be a runtime assertion.
        let owned: Vec<u8> = vec![0xC7, 0xD1, 0xB1, 0xDB, 0x2E, 0x74, 0x78, 0x74];
        let raw: &[u8] = &owned;
        assert!(
            std::str::from_utf8(raw).is_err(),
            "fixture must not be valid UTF-8"
        );

        let mut w = Writer::new();
        w.string(raw);
        let encoded = w.into_inner();
        assert_eq!(&encoded[..4], &[0, 0, 0, 8], "u32 big-endian length prefix");
        assert_eq!(&encoded[4..], raw);

        let mut r = Reader::new(&encoded);
        assert_eq!(r.string().unwrap(), raw.to_vec());
        assert!(r.is_empty());
    }

    #[test]
    fn integers_are_big_endian() {
        // Mutation: `to_le_bytes` / `from_le_bytes` on either. Asserted on the bytes, since a round
        // trip through the same function cannot see byte order.
        let mut w = Writer::new();
        w.u32(0x0102_0304);
        w.u64(0x0102_0304_0506_0708);
        let b = w.into_inner();
        assert_eq!(b, vec![1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8]);

        let mut r = Reader::new(&b);
        assert_eq!(r.u32().unwrap(), 0x0102_0304);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
    }

    #[test]
    fn a_length_longer_than_the_packet_is_refused_rather_than_allocated() {
        // Mutation: drop the `if self.remaining() < n` guard in `take`. The slice then panics, so
        // this reddens as a panic rather than as a failed assertion.
        let packet = [0x00, 0x00, 0x10, 0x00, 0x41]; // claims 4096 bytes, carries 1
        let mut r = Reader::new(&packet);
        match r.string() {
            Err(Error::Truncated { needed, had }) => {
                assert_eq!(needed, 4096);
                assert_eq!(had, 1);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn the_frame_length_counts_the_type_byte() {
        // Mutation: write `body.len() as u32` instead of `+ 1`. Asserted on the four length bytes,
        // not on a round trip.
        let framed = frame(104, &[0xAA, 0xBB, 0xCC]);
        assert_eq!(&framed[..4], &[0, 0, 0, 4], "3 body bytes + 1 type byte");
        assert_eq!(framed[4], 104);
        assert_eq!(&framed[5..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn text_is_lossy_and_string_is_not_and_that_asymmetry_is_deliberate() {
        // Mutation: make `text()` call `String::from_utf8(..).unwrap()`, or make `string()` route
        // through `text()`. Either reddens — the first by panicking on a legal STATUS message, the
        // second by losing the filename bytes.
        let invalid: &[u8] = &[0xC7, 0xD1];
        let mut w = Writer::new();
        w.string(invalid);
        let buf = w.into_inner();

        let mut as_text = Reader::new(&buf);
        assert!(
            as_text.text().unwrap().contains('\u{FFFD}'),
            "text may lose bytes"
        );

        let mut as_bytes = Reader::new(&buf);
        assert_eq!(
            as_bytes.string().unwrap(),
            invalid.to_vec(),
            "a string may not"
        );
    }
}
