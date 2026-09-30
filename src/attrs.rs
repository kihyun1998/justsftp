//! File attributes: [`FileAttributes`] as the server sent them, [`AttrsUpdate`] as a change to ask
//! for, with no conversion between the two (docs/map/territory/file-attributes.md).

use crate::error::Result;
use crate::wire::{Reader, Writer};

/// `SSH_FILEXFER_ATTR_*`, draft-ietf-secsh-filexfer-02 § 5.
pub(crate) const ATTR_SIZE: u32 = 0x0000_0001;
pub(crate) const ATTR_UIDGID: u32 = 0x0000_0002;
pub(crate) const ATTR_PERMISSIONS: u32 = 0x0000_0004;
pub(crate) const ATTR_ACMODTIME: u32 = 0x0000_0008;
pub(crate) const ATTR_EXTENDED: u32 = 0x8000_0000;

/// POSIX file-type mask.
const S_IFMT: u32 = 0o170_000;
/// Permission bits, including setuid / setgid / sticky.
const S_IPERM: u32 = 0o7777;

/// What kind of thing a directory entry is, read from the type field of the POSIX mode word.
// Masked and compared for equality, never tested as a subset: docs/map/territory/file-attributes.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// A named pipe (`S_IFIFO`).
    Fifo,
    /// A character device (`S_IFCHR`).
    CharDevice,
    /// A directory (`S_IFDIR`).
    Directory,
    /// A block device (`S_IFBLK`).
    BlockDevice,
    /// A regular file (`S_IFREG`).
    Regular,
    /// A symbolic link (`S_IFLNK`). Seen from [`crate::Session::lstat`] and in listings; `stat`
    /// follows the link.
    Symlink,
    /// A socket (`S_IFSOCK`).
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

/// One `name = value` pair from the `SSH_FILEXFER_ATTR_EXTENDED` tail, both halves as bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    /// The extension's name, such as `foo@example.com`.
    pub name: Vec<u8>,
    /// Its value, in whatever format the extension defines.
    pub value: Vec<u8>,
}

/// Attributes as the server sent them. There is no way to send one back; build an
/// [`AttrsUpdate`] instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileAttributes {
    /// The size in bytes.
    pub size: Option<u64>,
    /// The owner's numeric user id. Present exactly when `gid` is.
    pub uid: Option<u32>,
    /// The owner's numeric group id. Present exactly when `uid` is.
    pub gid: Option<u32>,
    /// The raw POSIX mode word, type nibble included. Use [`Self::file_type`] and
    /// [`Self::permissions`] rather than reading it directly.
    pub mode: Option<u32>,
    /// The last access time, in seconds since the Unix epoch. Present exactly when `mtime` is.
    pub atime: Option<u32>,
    /// The last modification time, in seconds since the Unix epoch. Present exactly when `atime`
    /// is.
    pub mtime: Option<u32>,
    /// Extended attributes the server attached, if any.
    pub extensions: Vec<Extension>,
}

impl FileAttributes {
    /// `None` means **the server did not say**, which is neither "a regular file" nor a zero mode.
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
        // One flag, two fields.
        if flags & ATTR_UIDGID != 0 {
            a.uid = Some(r.u32()?);
            a.gid = Some(r.u32()?);
        }
        if flags & ATTR_PERMISSIONS != 0 {
            a.mode = Some(r.u32()?);
        }
        // atime before mtime (docs/map/territory/file-attributes.md).
        if flags & ATTR_ACMODTIME != 0 {
            a.atime = Some(r.u32()?);
            a.mtime = Some(r.u32()?);
        }
        // Consumed even though nothing reads it, or the rest of the NAME packet is misframed
        // (docs/map/territory/file-attributes.md).
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
/// explicit call**, and there is no conversion from [`FileAttributes`].
// A read value cannot be written back, which is what keeps a stale size from truncating a file:
// docs/map/territory/file-attributes.md.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttrsUpdate {
    size: Option<u64>,
    owner: Option<(u32, u32)>,
    permissions: Option<u32>,
    times: Option<(u32, u32)>,
}

impl AttrsUpdate {
    /// An update that changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Truncate or extend the file to `size` bytes.
    pub fn truncate_to(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// The owner, uid and gid together: the wire carries them under one flag.
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some((uid, gid));
        self
    }

    /// The permission bits, setuid, setgid and sticky included. Any type nibble in `mode` is
    /// discarded.
    pub fn permissions(mut self, mode: u32) -> Self {
        self.permissions = Some(mode & S_IPERM);
        self
    }

    /// Access and modification times together, for the same reason as [`Self::owner`].
    pub fn times(mut self, atime: u32, mtime: u32) -> Self {
        self.times = Some((atime, mtime));
        self
    }

    /// Whether this update changes nothing.
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
        // Mutation: give `AttrsUpdate` a `Default` with `size: Some(0)`. The output goes from 4
        // bytes to 12.
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
        // Mutation: add `flags |= ATTR_SIZE` unconditionally in `encode`, or make `permissions()`
        // also fill `self.size`. Either reddens on the flags word.
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
        // Mutation: narrow S_IPERM to 0o777.
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
        // The type values that overlap bitwise, each read as exactly one type.
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
        // Mutation: `self.mode.unwrap_or_default()` in `file_type()`. `None` then becomes
        // `Some(Other(0))`.
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
        // Mutation: swap the two lines in `decode`. A literal with two distinguishable values, since
        // a round trip through this crate's own encoder cannot see the swap.
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
        // The trailing marker is what observes a skipped tail: the next read would return extension
        // bytes instead.
        //
        // Mutation: delete the `ATTR_EXTENDED` block in `decode`. The final assertion reddens with
        // a wrong value rather than an error.
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
