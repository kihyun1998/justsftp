//! The wire cursor. Hand-rolled, and that is the whole architecture argument.
//!
//! ⚠️ **There is no `serde` here, and its absence is the reason this crate exists.** The causal
//! chain (`docs/map/territory/paths-are-bytes.md`) runs: encode with `serde` -> every field must
//! implement `Serialize` -> a text field is `String` -> `String` is UTF-8 by type in Rust -> a
//! lossy decode is the only one left. A cursor never takes that branch, because reading four bytes and then that
//! many more is not a typed operation at all.
//!
//! The normative type backs this up rather than merely permitting it. RFC 4251 § 5 defines an SSH
//! `string` as *"arbitrary length binary string... allowed to contain arbitrary binary data,
//! including null characters and 8-bit characters"*, and draft-ietf-secsh-filexfer-02 mandates UTF-8
//! for exactly one field pair — the STATUS error message and language tag. Both crates measured on
//! disk are non-conformant here, in opposite directions: `russh-sftp` corrupts silently
//! (`buf.rs:25`, `from_utf8_lossy`) and `openssh-sftp-client` rejects the whole response
//! (`ssh_format/de.rs:226`, `InvalidStr`). Neither can hand back the server's bytes.

use crate::error::{Error, Result};

/// Reads fields out of one already-framed packet body.
///
/// It borrows rather than owning, so decoding a NAME response with many entries copies each
/// filename exactly once — into the `Vec<u8>` the caller keeps.
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

    /// ⚠️ **Every read goes through here, and the bounds check is why.** A field's length arrives
    /// from the far end, so a decoder that trusts it either panics on a slice or allocates whatever
    /// the sender asked for. Returning `Truncated` with both numbers is what makes a fixture that
    /// is one byte short say so.
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
    ///
    /// This is the function `russh-sftp` also has — `try_get_bytes`, `buf.rs:11-20`, byte-for-byte
    /// the same operation — and then does not use for filenames. Its own maintainer's stated reason
    /// for rejecting `Vec<u8>` on upstream #42 was that *"the packet defines the number of
    /// characters, not bytes"*, which this function in their own crate contradicts.
    pub(crate) fn string(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    /// A field the spec defines as text.
    ///
    /// ⚠️ **Lossy here is correct, and it is correct for a reason that does not generalise.** These
    /// bytes are shown to a person and are never used to *address* anything, so a replacement
    /// character costs legibility and cannot cost reachability. The defect this crate exists to fix
    /// is the opposite case: a filename decoded lossily can no longer name the file it came from,
    /// and SFTP has no open-by-directory-entry to recover it with. Two fields qualify — the STATUS
    /// error message and its language tag. Nothing else may call this.
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

/// Wraps a body in the outer frame: `u32 length | u8 type | body`.
///
/// ⚠️ **The length covers the type byte.** Both implementations on disk agree
/// (`russh-sftp/protocol/mod.rs:272-279` writes `payload.len() + 1`; `ssh_format/ser.rs:32-36`
/// counts the type byte into its running total), and the draft's § 3 makes the payload
/// `byte[length - 1]` with the type inside it. Off by this one byte and every packet after the
/// first is misframed, which reads as a corrupt stream rather than as an arithmetic slip.
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
        // The fixture is a real EUC-KR filename: 한글.txt. It is NOT valid UTF-8 — `C7` is an
        // invalid lead byte — and it is the exact shape paths-are-bytes.md records, where a lossy
        // decode turns `D1 B1` into a real Cyrillic letter so the name does not even look broken.
        //
        // Mutation that reddens this: make `string()` return
        // `String::from_utf8_lossy(..).into_bytes()`. That is the one-line version of the upstream
        // defect, and the assertion below is what observes it.
        // The `Vec` is not decoration: against a literal, `invalid_from_utf8` fires and the
        // compiler tells you at build time that the check can only ever pass. It is a runtime
        // assertion on purpose, so the fixture's own precondition is stated in the test.
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
        // Mutation: `to_le_bytes` / `from_le_bytes` on either. Byte order is invisible in
        // a round trip that uses the same function both ways, so each assertion pins the *bytes*.
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
        // ⚠️ This is the bounds check, and it is not hypothetical. `russh-sftp` reads its outer
        // frame with `read_packet(stream, u32::MAX)` (`client/mod.rs:74`) straight into
        // `vec![0; length as usize]` (`utils.rs:21`), so a server that declares 4 GiB gets 4 GiB
        // allocated. Here the declared length is checked against what is actually present first.
        //
        // Mutation: drop the `if self.remaining() < n` guard in `take`. The slice then panics, so
        // this reddens as a panic rather than as a failed assertion — still red, and the test names
        // which it expects.
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
        // Mutation: write `body.len() as u32` instead of `+ 1`. Every packet after the first is
        // then misframed by one byte, which surfaces far from here as a corrupt stream — so the
        // assertion is on the four length bytes directly, not on a round trip.
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
