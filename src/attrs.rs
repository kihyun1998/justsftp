//! File attributes — the trap surface.
//!
//! Three of the eight upstream defects (`docs/map/territory/upstream-traps.md`) live in this one
//! structure, and two of them are designed out here rather than guarded against. The split that
//! does it is the split between [`FileAttributes`] (what the server said) and [`AttrsUpdate`] (what we are asking it to change).
//! They are different types on purpose, and the purpose is that a value read from the server
//! **cannot be handed back as a write**.

use crate::error::Result;
use crate::wire::{Reader, Writer};

/// `SSH_FILEXFER_ATTR_*`, draft-ietf-secsh-filexfer-02 § 5. Confirmed identical in both
/// implementations read on disk (`russh-sftp/protocol/file_attrs.rs:25-31`,
/// `openssh-sftp-protocol/constants.rs:68-72`).
pub(crate) const ATTR_SIZE: u32 = 0x0000_0001;
pub(crate) const ATTR_UIDGID: u32 = 0x0000_0002;
pub(crate) const ATTR_PERMISSIONS: u32 = 0x0000_0004;
pub(crate) const ATTR_ACMODTIME: u32 = 0x0000_0008;
pub(crate) const ATTR_EXTENDED: u32 = 0x8000_0000;

/// POSIX file-type mask. **This is the whole of trap #36.**
const S_IFMT: u32 = 0o170_000;
/// Permission bits, including setuid / setgid / sticky.
const S_IPERM: u32 = 0o7777;

/// What kind of thing a directory entry is.
///
/// ⚠️ **Read by masking and comparing for equality, never by a subset test.** The POSIX type field
/// is an integer packed into the mode word, not a bitfield, and its values overlap:
/// `S_IFSOCK` (0o140000) contains every bit of `S_IFDIR` (0o40000) **and** of `S_IFREG` (0o100000),
/// and `S_IFLNK` (0o120000) contains every bit of `S_IFREG`. `russh-sftp` tests them with
/// `bitflags::contains` (`file_attrs.rs:204-232`), which is a subset test, so on that crate a
/// **symlink answers `true` to `is_regular()`** and a socket answers `true` to both `is_dir()` and
/// `is_regular()`. That is upstream #36, and it is why there are no `is_*` predicates in this file
/// at all — an enum with one answer cannot express the contradiction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Fifo,
    CharDevice,
    Directory,
    BlockDevice,
    Regular,
    Symlink,
    Socket,
    /// A type value POSIX does not define. Carried rather than guessed at.
    Other(u32),
}

impl FileType {
    fn from_mode(mode: u32) -> Self {
        match mode & S_IFMT {
            0o010_000 => Self::Fifo,
            0o020_000 => Self::CharDevice,
            0o040_000 => Self::Directory,
            0o060_000 => Self::BlockDevice,
            0o100_000 => Self::Regular,
            0o120_000 => Self::Symlink,
            0o140_000 => Self::Socket,
            other => Self::Other(other),
        }
    }
}

/// One `name = value` pair from the `SSH_FILEXFER_ATTR_EXTENDED` tail.
///
/// Both halves are bytes: the draft gives them as `string`, and § 5 never says they are text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// Attributes as the server sent them. **Read-only, and that is enforced by there being no way to
/// send one.**
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileAttributes {
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// The raw POSIX mode word, type nibble included. Use [`Self::file_type`] and
    /// [`Self::permissions`] rather than reading it directly.
    pub mode: Option<u32>,
    pub atime: Option<u32>,
    pub mtime: Option<u32>,
    pub extensions: Vec<Extension>,
}

impl FileAttributes {
    /// `None` means **the server did not tell us**, which is not the same as "a regular file" and
    /// not the same as a zero mode.
    ///
    /// `russh-sftp` collapses those three: `file_type()` does `unwrap_or_default()` on an absent
    /// mode (`file_attrs.rs:247-249`), so "unknown" and "a genuine mode of 0" both arrive as
    /// `FileType::Other`. `openssh-sftp-protocol` keeps the distinction with an `Option`
    /// (`file_attrs.rs:314-325`) and its comment says why — *"filetype is only set by the
    /// sftp-server"*. This follows openssh.
    pub fn file_type(&self) -> Option<FileType> {
        self.mode.map(FileType::from_mode)
    }

    /// The permission bits with the type nibble masked off.
    pub fn permissions(&self) -> Option<u32> {
        self.mode.map(|m| m & S_IPERM)
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let flags = r.u32()?;
        let mut a = Self::default();

        if flags & ATTR_SIZE != 0 {
            a.size = Some(r.u64()?);
        }
        // ⚠️ uid and gid are ONE flag and two fields. Reading either without the other desynchronises
        // the packet, which is why they are not separately optional.
        if flags & ATTR_UIDGID != 0 {
            a.uid = Some(r.u32()?);
            a.gid = Some(r.u32()?);
        }
        if flags & ATTR_PERMISSIONS != 0 {
            a.mode = Some(r.u32()?);
        }
        // ⚠️ atime before mtime. Confirmed three ways — `russh-sftp/file_attrs.rs:429-438`,
        // `openssh-sftp-protocol/file_attrs.rs:401-404` (pinned by its own serde test at :549), and
        // the draft's § 5 field order. They were swapped upstream once (fixed 2025-02-24) and the
        // symptom is that every timestamp in a listing is the wrong one **and still looks plausible**.
        if flags & ATTR_ACMODTIME != 0 {
            a.atime = Some(r.u32()?);
            a.mtime = Some(r.u32()?);
        }
        // ⚠️ **This tail must be consumed even though nothing reads it**, and skipping it is not a
        // missing feature — it is a framing bug. `russh-sftp` recognises the flag (it is a declared
        // bit, so `from_bits_truncate` keeps it) and then parses nothing after mtime
        // — its deserialiser's visitor (`file_attrs.rs:386-445`) stops at `mtime` around :434-438
        // with no extended branch at all, and the only acknowledgement anywhere is a
        // `// todo: extended implementation` sitting in the **serialiser** at :380. Against a server that sets
        // ATTR_EXTENDED the leftover bytes desynchronise **every later entry in the same NAME
        // packet**, which surfaces as garbage filenames rather than as a missing field.
        if flags & ATTR_EXTENDED != 0 {
            let count = r.u32()?;
            a.extensions.reserve(count.min(64) as usize);
            for _ in 0..count {
                a.extensions.push(Extension {
                    name: r.string()?,
                    value: r.string()?,
                });
            }
        }
        Ok(a)
    }
}

/// A request to change attributes. **Every field starts absent and can only be filled by an
/// explicit call.**
///
/// ⚠️ **This type exists to make upstream #89 unrepresentable**, and it is worth being precise
/// about what #89 actually is, because the shape usually quoted is not the one in the shipped
/// source. Measured on `russh-sftp` 2.4.0 as installed: `FileAttributes` derives `Default`, so
/// every field really is `None` and the serializer really is `is_some()`-driven. The truncation
/// arrives by a different road — `Metadata` is a **type alias for `FileAttributes`**
/// (`client/fs/mod.rs:13`), `metadata()` hands back the server's attrs verbatim including
/// `size: Some(..)`, and passing that value to `set_metadata` re-sends `ATTR_SIZE`. The file is
/// then truncated to whatever length an earlier stat happened to observe, with no error anywhere.
///
/// A guard against that would be a rule someone has to remember. Two types is not a rule: there is
/// no `From<FileAttributes>` here and no field to copy one into, so the read-modify-write cannot be
/// written down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttrsUpdate {
    size: Option<u64>,
    owner: Option<(u32, u32)>,
    permissions: Option<u32>,
    times: Option<(u32, u32)>,
}

impl AttrsUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Truncate or extend the file. **Named for what it does**, because `size` reads like metadata
    /// and this is the one field on the wire that destroys data.
    pub fn truncate_to(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// ⚠️ **Both, because the wire has one flag for both.** `SSH_FILEXFER_ATTR_UIDGID` covers uid
    /// and gid together, so a server receiving it reads two fields whatever the caller meant.
    /// `russh-sftp` lets you set one and quietly ships `unwrap_or(0)` for the other
    /// (`file_attrs.rs:366-369`), which hands the file to root. Requiring both makes the weld
    /// visible instead of filling it in.
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some((uid, gid));
        self
    }

    /// The permission bits. Any type nibble in `mode` is discarded.
    ///
    /// ⚠️ **The two implementations disagree here and we follow `openssh`.** It masks the type
    /// nibble out before writing (`file_attrs.rs:274-278`, `349-351`); `russh-sftp` writes the mode
    /// word raw (`file_attrs.rs:371-373`), so a value that came from a stat carries `S_IFMT` back
    /// to the server on a SETSTAT. POSIX `chmod` does not change a file's type, so sending type
    /// bits is at best ignored and at worst rejected. The draft is silent — it says only *"a bit
    /// mask of file permissions as defined by posix"* — so this is decided on POSIX, not on the
    /// spec text.
    pub fn permissions(mut self, mode: u32) -> Self {
        self.permissions = Some(mode & S_IPERM);
        self
    }

    /// ⚠️ **Both, for the same reason as [`Self::owner`].** One flag, two fields. Setting only
    /// mtime through `russh-sftp` ships `atime = 0` — the epoch (`file_attrs.rs:375-378`).
    pub fn times(mut self, atime: u32, mtime: u32) -> Self {
        self.times = Some((atime, mtime));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.size.is_none()
            && self.owner.is_none()
            && self.permissions.is_none()
            && self.times.is_none()
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        let mut flags = 0u32;
        if self.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if self.owner.is_some() {
            flags |= ATTR_UIDGID;
        }
        if self.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if self.times.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        w.u32(flags);

        if let Some(size) = self.size {
            w.u64(size);
        }
        if let Some((uid, gid)) = self.owner {
            w.u32(uid);
            w.u32(gid);
        }
        if let Some(mode) = self.permissions {
            w.u32(mode);
        }
        if let Some((atime, mtime)) = self.times {
            w.u32(atime);
            w.u32(mtime);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> FileAttributes {
        let mut r = Reader::new(bytes);
        FileAttributes::decode(&mut r).expect("decode")
    }

    #[test]
    fn an_empty_update_sends_no_fields_at_all() {
        // Upstream #89's class, asserted at the byte level. Mutation: give `AttrsUpdate` a
        // `Default` with `size: Some(0)`, which is the shape the defect is usually described as.
        // The assertion below goes from 4 bytes to 12 and reddens.
        let mut w = Writer::new();
        AttrsUpdate::new().encode(&mut w);
        assert_eq!(
            w.into_inner(),
            vec![0, 0, 0, 0],
            "flags word only, no fields"
        );
    }

    #[test]
    fn setting_permissions_alone_does_not_send_a_size() {
        // The acceptance box, byte-exact. Mutation: add `flags |= ATTR_SIZE` unconditionally in
        // `encode`, or make `permissions()` also fill `self.size`. Either reddens on the flags word.
        let mut w = Writer::new();
        AttrsUpdate::new().permissions(0o644).encode(&mut w);
        let out = w.into_inner();
        assert_eq!(
            &out[..4],
            &[0, 0, 0, 0x04],
            "ATTR_PERMISSIONS and nothing else"
        );
        assert_eq!(out.len(), 8, "flags + one u32; a size would make this 16");
    }

    #[test]
    fn a_type_nibble_never_survives_into_a_permissions_write() {
        // Mutation: drop the `& S_IPERM` in `permissions()`. The mode word then carries S_IFREG
        // back to the server on a SETSTAT.
        let mut w = Writer::new();
        AttrsUpdate::new().permissions(0o100_644).encode(&mut w);
        let out = w.into_inner();
        assert_eq!(
            &out[4..8],
            &[0, 0, 0x01, 0xA4],
            "0o644, with 0o100000 masked off"
        );
    }

    #[test]
    fn setuid_setgid_and_sticky_are_not_dropped() {
        // ⚠️ `russh-sftp`'s permission bitflags have no setuid/setgid/sticky at all and it uses
        // `from_bits_truncate`, so those three bits vanish from any view it hands you.
        // Mutation: narrow S_IPERM to 0o777. This reddens.
        let mut w = Writer::new();
        AttrsUpdate::new().permissions(0o6755).encode(&mut w);
        assert_eq!(
            &w.into_inner()[4..8],
            &[0, 0, 0x0D, 0xED],
            "0o6755 kept whole"
        );
    }

    #[test]
    fn a_symlink_is_not_a_regular_file_and_a_socket_is_not_a_directory() {
        // Upstream #36, stated as the two overlaps that actually bite. A subset test says `true` to
        // both halves of each pair; masked equality says `false`.
        //
        // Mutation: replace `mode & S_IFMT` with a `mode & 0o100_000 != 0` style subset test in
        // `from_mode`. The symlink row then reads Regular and this reddens.
        let attrs = |mode: u32| FileAttributes {
            mode: Some(mode),
            ..Default::default()
        };

        assert_eq!(attrs(0o120_777).file_type(), Some(FileType::Symlink));
        assert_eq!(attrs(0o140_755).file_type(), Some(FileType::Socket));
        assert_eq!(attrs(0o100_644).file_type(), Some(FileType::Regular));
        assert_eq!(attrs(0o040_755).file_type(), Some(FileType::Directory));
        assert_eq!(attrs(0o060_644).file_type(), Some(FileType::BlockDevice));

        // The pairs that make `contains()` wrong, asserted as inequality so the test states the
        // defect rather than merely exercising the happy path.
        assert_ne!(attrs(0o120_777).file_type(), Some(FileType::Regular));
        assert_ne!(attrs(0o140_755).file_type(), Some(FileType::Directory));
        assert_ne!(attrs(0o140_755).file_type(), Some(FileType::Regular));
        assert_ne!(attrs(0o060_644).file_type(), Some(FileType::Directory));
    }

    #[test]
    fn an_absent_mode_is_unknown_rather_than_a_regular_file() {
        // Mutation: `self.mode.unwrap_or_default()` in `file_type()`, which is what `russh-sftp`
        // does. `None` then becomes `Some(Other(0))` and a server that sent no PERMISSIONS flag is
        // indistinguishable from one that sent a zero mode.
        let silent = FileAttributes::default();
        assert_eq!(silent.file_type(), None);
        assert_eq!(silent.permissions(), None);

        let zero = FileAttributes {
            mode: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.file_type(), Some(FileType::Other(0)));
        assert_ne!(zero.file_type(), silent.file_type());
    }

    #[test]
    fn atime_is_read_before_mtime() {
        // Mutation: swap the two lines in `decode`. This is the upstream defect of 2025-02-24, and
        // it is invisible in a round trip through our own encoder — which is why the input here is
        // a literal byte array with two distinguishable values rather than an encode/decode pair.
        let bytes = [
            0, 0, 0, 0x08, // flags = ACMODTIME
            0x11, 0x11, 0x11, 0x11, // atime
            0x22, 0x22, 0x22, 0x22, // mtime
        ];
        let a = decode(&bytes);
        assert_eq!(a.atime, Some(0x1111_1111));
        assert_eq!(a.mtime, Some(0x2222_2222));
    }

    #[test]
    fn the_extended_tail_is_consumed_so_the_next_field_stays_aligned() {
        // ⚠️ The framing bug, and the assertion that observes it is the trailing marker — not the
        // extension itself. A decoder that parses the flag and skips the tail leaves 14 bytes in
        // the buffer, so the next read returns extension bytes instead of the marker.
        //
        // Mutation: delete the `ATTR_EXTENDED` block in `decode`. The final assertion reddens with
        // a wrong value rather than an error, which is exactly how this defect presents in the wild.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&(ATTR_PERMISSIONS | ATTR_EXTENDED).to_be_bytes());
        buf.extend_from_slice(&0o100_644u32.to_be_bytes()); // permissions
        buf.extend_from_slice(&1u32.to_be_bytes()); // one extension pair
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(b"ab"); // name
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(b"cd"); // value
        buf.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // the next field in the packet

        let mut r = Reader::new(&buf);
        let a = FileAttributes::decode(&mut r).expect("decode");
        assert_eq!(a.file_type(), Some(FileType::Regular));
        assert_eq!(
            a.extensions,
            vec![Extension {
                name: b"ab".to_vec(),
                value: b"cd".to_vec()
            }]
        );
        assert_eq!(
            r.u32().unwrap(),
            0xDEAD_BEEF,
            "the reader must be aligned on the next field"
        );
    }

    #[test]
    fn owner_and_times_round_trip_as_pairs() {
        // Mutation: in `encode`, write only `uid` under ATTR_UIDGID. The decode then reads gid out
        // of the following field and everything after it shifts.
        let mut w = Writer::new();
        AttrsUpdate::new()
            .owner(1000, 1001)
            .times(7, 9)
            .encode(&mut w);
        let a = decode(&w.into_inner());
        assert_eq!((a.uid, a.gid), (Some(1000), Some(1001)));
        assert_eq!((a.atime, a.mtime), (Some(7), Some(9)));
    }
}
