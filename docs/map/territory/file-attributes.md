# File attributes

## What it is

The attribute block of the protocol, in two types that go in opposite directions: `FileAttributes`
(what the server said, read-only) and `AttrsUpdate` (what the caller asks the server to change).
Plus `FileType`, read from the POSIX mode word, and the `ATTR_EXTENDED` tail.

## Governing decisions

**None.**

## Design model

- **A value read from the server cannot be handed back as a write.** There is no
  `From<FileAttributes>` for `AttrsUpdate` and no field to copy one into, so the read-modify-write
  cannot be written down. In `russh-sftp` 2.4.0 it can: `Metadata` is a type alias for
  `FileAttributes`, `metadata()` hands back the server's attrs verbatim including `size: Some(..)`,
  and passing that value to `set_metadata` re-sends `ATTR_SIZE` (R10, R11b). The file is then
  truncated to whatever length an earlier stat happened to observe, with no error anywhere. A guard
  against that would be a rule someone has to remember; two types is not a rule.
- **An empty update sends no fields at all** — the flags word only. That is the shape of upstream
  #89: through 2.3.0 `russh-sftp`'s `Default` for attributes carried `size: Some(0)`, so an
  "empty" set truncated the file (R11); 2.4.0 fixed it.
- **`truncate_to` is named for what it does**, because `size` reads like metadata and this is the
  one field on the wire that destroys data.
- **`FileType` is read by masking and comparing for equality, never by a subset test.** The POSIX
  type field is an integer packed into the mode word, not a bitfield, and its values overlap:
  `S_IFSOCK` (0o140000) contains every bit of `S_IFDIR` (0o40000) **and** of `S_IFREG` (0o100000),
  and `S_IFLNK` (0o120000) contains every bit of `S_IFREG`. `russh-sftp`'s `is_*` predicates test
  them with `contains`, so there a symlink answers `true` to `is_regular()` and a socket answers
  `true` to both `is_dir()` and `is_regular()` (R3) — the class of upstream #36, which fixed the same
  mistake in its `FileType` conversion and not in the predicates. That is why there are no `is_*`
  predicates here at all: an enum with one answer cannot express the contradiction. A type value
  POSIX does not define is carried as `Other` rather than guessed at.
- **An absent mode is unknown, not a regular file and not a zero mode.** `file_type()` and
  `permissions()` return `None` when the server sent no `PERMISSIONS` flag. `russh-sftp` collapses
  "unknown" and "a genuine mode of 0" into `FileType::Other` through `unwrap_or_default()`;
  `openssh-sftp-protocol` keeps the distinction with an `Option`, and its comment says why —
  *"filetype is only set by the sftp-server"*. This follows openssh.
- **uid and gid are one flag and two fields**, and so are atime and mtime. Reading either without
  the other desynchronises the packet, which is why the decoder reads both under one flag and
  `AttrsUpdate` takes them only as a pair (`owner`, `times`); `FileAttributes` keeps four `Option`s.
  `russh-sftp` lets a caller set one and quietly ships `unwrap_or(0)` for the other — uid 0 hands the
  file to root, atime 0 is the epoch. Requiring both makes the weld visible instead of filling it in.
- **atime before mtime.** Confirmed three ways — `russh-sftp` (R6), `openssh-sftp-protocol`, pinned
  by its own serde test (R7), and the draft's § 5 field order. A swap is invisible in a round trip
  through one's own encoder and every timestamp is then the wrong one **and still looks plausible**.
  `russh-sftp` did swap the two once, in its server half's conversion of local metadata (`9010bffd`),
  not in its decoder.
- **Permission writes mask the type nibble off, following openssh.** `openssh-sftp-protocol` masks
  before writing; `russh-sftp` writes the mode word raw, so a value that came from a stat carries
  `S_IFMT` back to the server on a SETSTAT. POSIX `chmod` does not change a file's type, so sending
  type bits is at best ignored and at worst rejected. The draft is silent — *"a bit mask of file
  permissions as defined by posix"* — so this is decided on POSIX, not on the spec text.
- **setuid, setgid and sticky are kept** (`S_IPERM` is `0o7777`). `russh-sftp`'s permission bitflags
  have none of the three and it uses `from_bits_truncate`, so those bits vanish from any view it
  hands back.
- **The `ATTR_EXTENDED` tail must be consumed even though nothing reads it.** Skipping it is not a
  missing feature — it is a framing bug. `russh-sftp` recognises the flag and then parses nothing
  after mtime, with a `todo: extended implementation` in its serialiser. Against a server that sets
  `ATTR_EXTENDED` the leftover bytes desynchronise **every later entry in the same `NAME` packet**,
  which surfaces as garbage filenames rather than as a missing field. The pair count reserves at most
  64 ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)); both halves of a pair
  are bytes, since § 5 never says they are text.
- **The `ATTR_*` flag values** are draft § 5's and identical in both reference implementations.

## Code

- `src/attrs.rs` — `ATTR_*`, `S_IFMT`, `S_IPERM`, `FileType` (`from_mode`), `Extension`,
  `FileAttributes` (`file_type`, `permissions`, `decode`), `AttrsUpdate` (`truncate_to`, `owner`,
  `permissions`, `times`, `is_empty`, `encode`)

## Reference behaviour

- Every `russh-sftp` and `openssh-sftp-protocol` behaviour above is a row in
  [reference](../reference.md).

## Cross-cutting invariants

- [Paths are bytes](../invariant/paths-are-bytes.md)
- [The far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)

## Blast radius

- [Packets](packets.md) — `DirEntry`, `Request::Open`, `SetStat`, `MkDir` carry these.
- [Path verbs](path-verbs.md) — `set_stat` takes an `AttrsUpdate`; `stat`/`lstat`/`fstat` return
  `FileAttributes`.

## Known holes / open

**None.**
