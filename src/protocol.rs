//! Packet types, requests and responses. Version 3 of the protocol.
//!
//! **v3 is a decision, not a default.** Both implementations read on disk hard-code it
//! (`russh-sftp/protocol/mod.rs:67`, `openssh-sftp-protocol/constants.rs:22`) and it is what
//! essentially every server speaks. What the draft's § 10.1 records is that v3 is the version that
//! *added* the error message and language tag to `SSH_FXP_STATUS` — so a client that speaks v3 and
//! is answered with v2 will over-read every status packet. That is a real hole in both crates
//! measured, neither of which has a version branch; here the handshake refuses instead.

use crate::attrs::{AttrsUpdate, FileAttributes};
use crate::error::{Error, Result, Status, StatusCode};
use crate::wire::{frame, Reader, Writer};

/// The protocol version this client speaks.
pub const VERSION: u32 = 3;

pub(crate) mod packet {
    pub const INIT: u8 = 1;
    pub const VERSION: u8 = 2;
    pub const OPEN: u8 = 3;
    pub const CLOSE: u8 = 4;
    pub const READ: u8 = 5;
    pub const WRITE: u8 = 6;
    pub const LSTAT: u8 = 7;
    pub const FSTAT: u8 = 8;
    pub const SETSTAT: u8 = 9;
    pub const FSETSTAT: u8 = 10;
    pub const OPENDIR: u8 = 11;
    pub const READDIR: u8 = 12;
    pub const REMOVE: u8 = 13;
    pub const MKDIR: u8 = 14;
    pub const RMDIR: u8 = 15;
    pub const REALPATH: u8 = 16;
    pub const STAT: u8 = 17;
    pub const RENAME: u8 = 18;
    pub const READLINK: u8 = 19;
    // ⚠️ SYMLINK (20) is deliberately absent. See the note on `Request`.
    pub const STATUS: u8 = 101;
    pub const HANDLE: u8 = 102;
    pub const DATA: u8 = 103;
    pub const NAME: u8 = 104;
    pub const ATTRS: u8 = 105;
    pub const EXTENDED: u8 = 200;
    pub const EXTENDED_REPLY: u8 = 201;
}

/// The extension that reports a server's size limits (OpenSSH `PROTOCOL` § 4.8).
pub(crate) const LIMITS_EXTENSION: &[u8] = b"limits@openssh.com";

/// `SSH_FXF_*` open flags, draft § 6.3. Values confirmed identical in both implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenFlags(u32);

impl OpenFlags {
    pub const READ: Self = Self(0x0000_0001);
    pub const WRITE: Self = Self(0x0000_0002);
    pub const APPEND: Self = Self(0x0000_0004);
    pub const CREATE: Self = Self(0x0000_0008);
    pub const TRUNCATE: Self = Self(0x0000_0010);
    pub const EXCLUSIVE: Self = Self(0x0000_0020);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        // Legal here and nowhere near the POSIX mode word: these really are independent bits, so a
        // subset test is the right operation. Trap #36 is about a *type field* wearing a bitfield's
        // clothes — see `attrs::FileType`.
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// An opaque token the server gave us to address an open file or directory.
///
/// ⚠️ **Bytes.** `russh-sftp` types its handle as
/// `String` (`protocol/handle.rs:6-8`), so it runs an opaque binary token through the same
/// `from_utf8_lossy` that destroys filenames. That is a **second corruption site, independent of
/// the first**: a mangled handle addresses the wrong file — or nothing — on every subsequent read,
/// write and close, and no filename has to be unusual for it to happen. The draft gives the field
/// as a `string`, which is arbitrary binary data (RFC 4251 § 5), and servers are free to put a
/// pointer, a counter or a nonce in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle(pub Vec<u8>);

impl Handle {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// One entry of an `SSH_FXP_NAME` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The name **as the server holds it**. This is the value that addresses the file.
    pub filename: Vec<u8>,
    /// The server's `ls -l`-style rendering, kept rather than discarded.
    ///
    /// Both implementations measured throw this away (`openssh` at parse time, `russh-sftp` in
    /// `read_dir`). The draft describes it as *"suitable for use in the output of a directory
    /// listing command"*, and it is the only place a v3 server states a file's type as text — which
    /// matters precisely when the attributes carry no `PERMISSIONS` flag. Bytes, not text: it
    /// contains a filename.
    pub longname: Vec<u8>,
    pub attrs: FileAttributes,
}

/// What this client can ask for.
///
/// ⚠️ **`SSH_FXP_SYMLINK` is deliberately not here.** The draft's § 6.10 gives the arguments as
/// `linkpath` then `targetpath`, and `russh-sftp` follows it (`protocol/symlink.rs:5-9`);
/// `openssh-sftp-protocol` **swaps them on purpose** (`request.rs:276-281`) because the OpenSSH
/// server itself deviates. Whichever order is chosen is wrong against half the server population,
/// and nothing on disk settles it — it needs a measurement against real servers. Creating a symlink
/// is also outside this crate's scope (listing, transfer, basic metadata), so the
/// honest move is to leave the verb out rather than ship a coin flip. `READLINK` takes one path and
/// has no such ambiguity, so it stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Open { path: Vec<u8>, flags: OpenFlags, attrs: AttrsUpdate },
    Close { handle: Handle },
    Read { handle: Handle, offset: u64, len: u32 },
    Write { handle: Handle, offset: u64, data: Vec<u8> },
    LStat { path: Vec<u8> },
    FStat { handle: Handle },
    SetStat { path: Vec<u8>, attrs: AttrsUpdate },
    FSetStat { handle: Handle, attrs: AttrsUpdate },
    OpenDir { path: Vec<u8> },
    ReadDir { handle: Handle },
    Remove { path: Vec<u8> },
    MkDir { path: Vec<u8>, attrs: AttrsUpdate },
    RmDir { path: Vec<u8> },
    RealPath { path: Vec<u8> },
    Stat { path: Vec<u8> },
    Rename { from: Vec<u8>, to: Vec<u8> },
    ReadLink { path: Vec<u8> },
    /// `SSH_FXP_EXTENDED`: the extension's name, then its request-specific fields already encoded.
    /// The fields carry no length prefix of their own — each extension defines its own layout.
    Extended { name: Vec<u8>, data: Vec<u8> },
}

impl Request {
    pub(crate) fn packet_type(&self) -> u8 {
        match self {
            Self::Open { .. } => packet::OPEN,
            Self::Close { .. } => packet::CLOSE,
            Self::Read { .. } => packet::READ,
            Self::Write { .. } => packet::WRITE,
            Self::LStat { .. } => packet::LSTAT,
            Self::FStat { .. } => packet::FSTAT,
            Self::SetStat { .. } => packet::SETSTAT,
            Self::FSetStat { .. } => packet::FSETSTAT,
            Self::OpenDir { .. } => packet::OPENDIR,
            Self::ReadDir { .. } => packet::READDIR,
            Self::Remove { .. } => packet::REMOVE,
            Self::MkDir { .. } => packet::MKDIR,
            Self::RmDir { .. } => packet::RMDIR,
            Self::RealPath { .. } => packet::REALPATH,
            Self::Stat { .. } => packet::STAT,
            Self::Rename { .. } => packet::RENAME,
            Self::ReadLink { .. } => packet::READLINK,
            Self::Extended { .. } => packet::EXTENDED,
        }
    }

    /// The complete framed packet, request id included.
    pub(crate) fn encode(&self, id: u32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(id);
        match self {
            Self::Open { path, flags, attrs } => {
                w.string(path);
                w.u32(flags.bits());
                attrs.encode(&mut w);
            }
            Self::Close { handle } | Self::FStat { handle } | Self::ReadDir { handle } => {
                w.string(handle.as_bytes());
            }
            Self::Read { handle, offset, len } => {
                w.string(handle.as_bytes());
                w.u64(*offset);
                w.u32(*len);
            }
            Self::Write { handle, offset, data } => {
                w.string(handle.as_bytes());
                w.u64(*offset);
                w.string(data);
            }
            Self::LStat { path }
            | Self::OpenDir { path }
            | Self::Remove { path }
            | Self::RmDir { path }
            | Self::RealPath { path }
            | Self::Stat { path }
            | Self::ReadLink { path } => {
                w.string(path);
            }
            Self::SetStat { path, attrs } | Self::MkDir { path, attrs } => {
                w.string(path);
                attrs.encode(&mut w);
            }
            Self::FSetStat { handle, attrs } => {
                w.string(handle.as_bytes());
                attrs.encode(&mut w);
            }
            Self::Rename { from, to } => {
                w.string(from);
                w.string(to);
            }
            Self::Extended { name, data } => {
                w.string(name);
                w.raw(data);
            }
        }
        frame(self.packet_type(), &w.into_inner())
    }
}

/// What the server can answer with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Status(Status),
    Handle(Handle),
    Data(Vec<u8>),
    Name(Vec<DirEntry>),
    Attrs(FileAttributes),
    /// `SSH_FXP_EXTENDED_REPLY`: everything after the request id, undecoded — its layout belongs to
    /// the extension that was asked.
    ExtendedReply(Vec<u8>),
}

impl Response {
    /// The wire type byte this variant came from.
    ///
    /// ⚠️ **A number, not a label.** An earlier draft had this return `&'static str` and the caller
    /// mapped the string back to a number — in a crate whose entire thesis is *do not route a value
    /// through a type it does not fit*, that was the one place the pattern survived. A
    /// one-character drift in either copy of the literal silently collapsed every unexpected reply
    /// onto a single code, with no compile error to catch it.
    pub(crate) fn packet_type(&self) -> u8 {
        match self {
            Self::Status(_) => packet::STATUS,
            Self::Handle(_) => packet::HANDLE,
            Self::Data(_) => packet::DATA,
            Self::Name(_) => packet::NAME,
            Self::Attrs(_) => packet::ATTRS,
            Self::ExtendedReply(_) => packet::EXTENDED_REPLY,
        }
    }

    /// Decodes one response body. The request id has already been read off by the caller, because
    /// the caller needs it to find who is waiting before it knows whether the body will decode.
    pub(crate) fn decode(packet_type: u8, r: &mut Reader<'_>) -> Result<Self> {
        match packet_type {
            packet::STATUS => Ok(Self::Status(Status {
                code: StatusCode::from_wire(r.u32()?),
                // ⚠️ Read unconditionally, which is only correct because the handshake refused
                // anything below v3. These two fields are v3's own addition (draft § 10.1); against
                // a v2 server they are not there and reading them walks off the end of the packet.
                // Both crates measured read them unconditionally *without* the handshake guard.
                message: r.text()?,
                language_tag: r.text()?,
            })),
            packet::HANDLE => Ok(Self::Handle(Handle(r.string()?))),
            packet::DATA => Ok(Self::Data(r.string()?)),
            packet::NAME => {
                let count = r.u32()?;
                // The count is the far end's, so it cannot size an allocation on its own — a
                // declared 4 billion entries would reserve before a single one was read. Each entry
                // costs at least 12 bytes on the wire, so what is actually present bounds it.
                let cap = (count as usize).min(r.remaining() / 12 + 1);
                let mut entries = Vec::with_capacity(cap);
                for _ in 0..count {
                    entries.push(DirEntry {
                        filename: r.string()?,
                        longname: r.string()?,
                        attrs: FileAttributes::decode(r)?,
                    });
                }
                Ok(Self::Name(entries))
            }
            packet::ATTRS => Ok(Self::Attrs(FileAttributes::decode(r)?)),
            packet::EXTENDED_REPLY => Ok(Self::ExtendedReply(r.rest().to_vec())),
            other => Err(Error::UnknownPacketType(other)),
        }
    }
}

/// `SSH_FXP_INIT`. No request id — the handshake is the one exchange that has none.
pub(crate) fn encode_init(version: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(version);
    frame(packet::INIT, &w.into_inner())
}

/// The server's half of the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    pub version: u32,
    pub extensions: Vec<(Vec<u8>, Vec<u8>)>,
}

impl ServerVersion {
    /// Whether the server listed this extension name.
    pub fn advertises(&self, name: &[u8]) -> bool {
        self.extensions.iter().any(|(n, _)| n == name)
    }
}

/// The server's answer to `limits@openssh.com`, as the server stated it.
///
/// A field of `0` means the server states no limit for it (OpenSSH `PROTOCOL` § 4.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    /// The largest whole packet the server accepts.
    pub max_packet_len: u64,
    /// The largest length an `SSH_FXP_READ` should ask for.
    pub max_read_len: u64,
    /// The largest data an `SSH_FXP_WRITE` may carry.
    pub max_write_len: u64,
    pub max_open_handles: u64,
}

pub(crate) fn decode_limits(body: &[u8]) -> Result<ServerLimits> {
    let mut r = Reader::new(body);
    Ok(ServerLimits {
        max_packet_len: r.u64()?,
        max_read_len: r.u64()?,
        max_write_len: r.u64()?,
        max_open_handles: r.u64()?,
    })
}

/// ⚠️ **Extensions run to the end of the packet and carry no count** (draft § 4). Both
/// implementations agree. `russh-sftp` reads the names as `String` through its lossy path, so a
/// non-UTF-8 extension name becomes U+FFFD and then silently fails its own `has_extension` name
/// comparison — a supported extension reads as absent. Bytes here for the same reason as everywhere
/// else.
pub(crate) fn decode_version(r: &mut Reader<'_>) -> Result<ServerVersion> {
    let version = r.u32()?;
    let mut extensions = Vec::new();
    while !r.is_empty() {
        let name = r.string()?;
        let value = r.string()?;
        extensions.push((name, value));
    }
    Ok(ServerVersion { version, extensions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_frames_as_length_type_id_then_fields() {
        // Mutation: reorder `w.u32(id)` after the path in `encode`. Every server would then read the
        // first four bytes of the path as the request id.
        let req = Request::Stat { path: b"/tmp".to_vec() };
        let bytes = req.encode(0x0102_0304);
        assert_eq!(&bytes[..4], &[0, 0, 0, 13], "1 type + 4 id + 4 len + 4 path = 13");
        assert_eq!(bytes[4], packet::STAT);
        assert_eq!(&bytes[5..9], &[1, 2, 3, 4], "request id");
        assert_eq!(&bytes[9..13], &[0, 0, 0, 4], "path length");
        assert_eq!(&bytes[13..], b"/tmp");
    }

    #[test]
    fn a_non_utf8_path_reaches_the_wire_byte_for_byte() {
        // The half that matters. Listing the bytes is not enough — a request addressed by them has
        // to carry them unchanged, or the file is still untouchable.
        //
        // Mutation: type `Request::Open.path` as `String`. That does not compile, which is the
        // point; the runnable mutation is to route `path` through `String::from_utf8_lossy` in
        // `encode`, and this assertion reddens.
        let path: Vec<u8> = vec![0xC7, 0xD1, 0xB1, 0xDB, 0x2E, 0x74, 0x78, 0x74];
        let bytes = Request::Open {
            path: path.clone(),
            flags: OpenFlags::READ,
            attrs: AttrsUpdate::new(),
        }
        .encode(1);
        // 4 len | 1 type | 4 id | 4 pathlen | 8 path | 4 pflags | 4 attr flags
        assert_eq!(&bytes[13..21], &path[..]);
        assert_eq!(&bytes[21..25], &[0, 0, 0, 1], "pflags = READ");
        assert_eq!(&bytes[25..29], &[0, 0, 0, 0], "no attributes requested");
        assert_eq!(bytes.len(), 29);
    }

    #[test]
    fn open_flags_or_together_into_the_documented_bits() {
        // Mutation: change any constant. These are wire values, so they are asserted as numbers.
        assert_eq!(OpenFlags::READ.bits(), 0x1);
        assert_eq!(OpenFlags::WRITE.bits(), 0x2);
        assert_eq!(OpenFlags::APPEND.bits(), 0x4);
        assert_eq!(OpenFlags::CREATE.bits(), 0x8);
        assert_eq!(OpenFlags::TRUNCATE.bits(), 0x10);
        assert_eq!(OpenFlags::EXCLUSIVE.bits(), 0x20);
        let w = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        assert_eq!(w.bits(), 0x1A);
        assert!(w.contains(OpenFlags::CREATE));
        assert!(!w.contains(OpenFlags::READ));
    }

    #[test]
    fn a_handle_is_opaque_bytes_and_goes_back_unchanged() {
        // ⚠️ The second corruption site. This handle is not valid UTF-8 — servers put counters and
        // pointers in these — and `russh-sftp` would lossy-convert it, addressing the wrong file on
        // every later operation.
        //
        // Mutation: type `Handle` as `String`. Non-compiling, so the runnable one is to route the
        // handle through `from_utf8_lossy` in `Request::encode`; this reddens.
        let handle = Handle(vec![0x00, 0xFF, 0x80, 0x01]);
        let bytes = Request::Read { handle, offset: 0x1122_3344_5566_7788, len: 4096 }.encode(7);
        assert_eq!(&bytes[9..13], &[0, 0, 0, 4], "handle length");
        assert_eq!(&bytes[13..17], &[0x00, 0xFF, 0x80, 0x01], "handle bytes, unchanged");
        assert_eq!(&bytes[17..25], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88], "offset u64");
        assert_eq!(&bytes[25..29], &[0, 0, 0x10, 0x00], "len 4096");
    }

    #[test]
    fn write_sends_the_handle_then_the_offset_then_the_data() {
        // ⚠️ This arm was the one distinct field order with no test, and it is the data-destroying
        // one. Mutation: swap `w.u64(*offset)` and `w.string(data)`. Nothing else in the crate
        // reddens — while against a real server the first four bytes of the payload become the high
        // half of a 64-bit offset, so the write lands gigabytes into the file.
        let bytes = Request::Write {
            handle: Handle(vec![0xAA]),
            offset: 0x0102_0304_0506_0708,
            data: vec![0xDE, 0xAD],
        }
        .encode(9);
        assert_eq!(&bytes[..4], &[0, 0, 0, 24], "23 body bytes + the type byte");
        assert_eq!(bytes[4], packet::WRITE);
        assert_eq!(&bytes[5..9], &[0, 0, 0, 9], "request id");
        assert_eq!(&bytes[9..14], &[0, 0, 0, 1, 0xAA], "handle");
        assert_eq!(&bytes[14..22], &[1, 2, 3, 4, 5, 6, 7, 8], "offset, u64 big-endian");
        assert_eq!(&bytes[22..28], &[0, 0, 0, 2, 0xDE, 0xAD], "data");
        assert_eq!(bytes.len(), 28);
    }

    #[test]
    fn every_response_variant_reports_its_own_wire_type() {
        // Mutation: point any arm of `packet_type` at another constant. Before this existed the
        // mapping went through `&'static str` and back, where a one-character drift collapsed every
        // unexpected reply onto one code with no compile error.
        use crate::error::Status;
        let status = Status {
            code: StatusCode::Ok,
            message: String::new(),
            language_tag: String::new(),
        };
        assert_eq!(Response::Status(status).packet_type(), 101);
        assert_eq!(Response::Handle(Handle(vec![])).packet_type(), 102);
        assert_eq!(Response::Data(vec![]).packet_type(), 103);
        assert_eq!(Response::Name(vec![]).packet_type(), 104);
        assert_eq!(Response::Attrs(FileAttributes::default()).packet_type(), 105);
        assert_eq!(Response::ExtendedReply(vec![]).packet_type(), 201);
    }

    #[test]
    fn rename_sends_from_before_to() {
        // Mutation: swap the two `w.string` calls. Renaming then moves the wrong way, and against a
        // real server it usually *succeeds* — which is why this is asserted on the bytes.
        let bytes = Request::Rename { from: b"a".to_vec(), to: b"bb".to_vec() }.encode(1);
        assert_eq!(&bytes[9..14], &[0, 0, 0, 1, b'a']);
        assert_eq!(&bytes[14..20], &[0, 0, 0, 2, b'b', b'b']);
    }

    #[test]
    fn an_extended_request_frames_its_payload_after_the_name_without_a_prefix() {
        // Mutation: `w.string(data)` for `w.raw(data)` — the payload gains a length prefix, and an
        // extension with fields (e.g. `statvfs@`'s path) is misread by every server.
        let bytes =
            Request::Extended { name: b"x@y".to_vec(), data: vec![0, 0, 0, 1, b'/'] }.encode(2);
        assert_eq!(
            bytes,
            [
                0, 0, 0, 17, // length: 1 type + 4 id + 4 + 3 name + 5 payload
                200, // SSH_FXP_EXTENDED
                0, 0, 0, 2, // id
                0, 0, 0, 3, b'x', b'@', b'y', // name
                0, 0, 0, 1, b'/', // payload, as given
            ]
        );
    }

    #[test]
    fn an_extended_reply_keeps_everything_after_the_id() {
        // Mutation: decode 201 as a `string`. The limits reply's first u64 would then be read as a
        // length and the body lost.
        let body = [0, 0, 0, 0, 0, 4, 0, 0, 0xAB];
        let mut r = Reader::new(&body);
        assert_eq!(Response::decode(201, &mut r).unwrap(), Response::ExtendedReply(body.to_vec()));
        assert!(r.is_empty());
    }

    #[test]
    fn limits_decode_in_the_order_openssh_writes_them() {
        // `sftp-server.c` `process_extended_limits`: packet, read, write, handles. Mutation: swap
        // any two fields in `decode_limits`.
        let mut body = Vec::new();
        for v in [262_144u64, 261_120, 261_000, 1_019] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        let l = decode_limits(&body).unwrap();
        assert_eq!(
            (l.max_packet_len, l.max_read_len, l.max_write_len, l.max_open_handles),
            (262_144, 261_120, 261_000, 1_019)
        );
        assert!(decode_limits(&body[..20]).is_err(), "a short body is an error, not zeros");
    }

    #[test]
    fn version_extensions_run_to_the_end_of_the_packet_without_a_count() {
        // Mutation: read a `u32` count before the loop. The first extension name's length is then
        // eaten as a count and everything after it is garbage.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes());
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(b"post");
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(b"v1");
        let mut r = Reader::new(&body);
        let v = decode_version(&mut r).unwrap();
        assert_eq!(v.version, 3);
        assert_eq!(v.extensions, vec![(b"post".to_vec(), b"v1".to_vec())]);
    }

    #[test]
    fn an_unknown_response_type_names_the_byte_rather_than_guessing() {
        let mut r = Reader::new(&[]);
        match Response::decode(200, &mut r) {
            Err(Error::UnknownPacketType(200)) => {}
            other => panic!("expected UnknownPacketType(200), got {other:?}"),
        }
    }
}
