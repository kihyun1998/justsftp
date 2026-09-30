# Reference

The pinned record of every outside source a note or a comment relies on. A note links a row here
rather than restating a file and line. Rows were checked against the source at the version named;
a line number belongs to that version only.

Versions read:

- **russh-sftp 2.4.0** (and 2.1.1 where it differs) — the crate this one was written against.
- **openssh-sftp-protocol 0.24.2** and **ssh_format 0.14.2** — what `openssh-sftp-client` 0.15.8
  resolves to.
- **OpenSSH portable** `master` at `ccc26c76cd47` — `PROTOCOL`, `sftp-server.c`, `sftp-client.c`.
- **draft-ietf-secsh-filexfer-02** (SFTP v3) and **RFC 4251**.

To re-check a row, fetch the crate with
`curl -sL https://static.crates.io/crates/<name>/<name>-<ver>.crate | tar xz`.

## russh-sftp

| id | path:lines @2.4.0 | what it shows |
|---|---|---|
| R35 | `src/buf.rs:25` | `try_get_string` decodes with `String::from_utf8_lossy` — every filename. |
| R37 | `src/buf.rs:11-20` | `try_get_bytes`: a `u32` length, then that many bytes — the byte operation, unused for filenames. |
| E7 | `src/protocol/file.rs:7-8` | `File { filename: String }`, shared by client and server halves, so its server cannot emit a non-UTF-8 name. |
| R19 | `src/protocol/handle.rs:6-8` | `Handle { handle: String }` — the handle goes through the lossy decode too. |
| R39 | `src/protocol/mod.rs:272-279` | the frame writes `payload.len() + 1`, the type byte counted. |
| R17 | `src/protocol/mod.rs:67` | `VERSION: u32 = 3`, hard-coded. |
| R20 | `src/protocol/symlink.rs:5-9` | `Symlink { linkpath, targetpath }`, in the draft's order. |
| R1 | `src/protocol/file_attrs.rs:25-31` | the `ATTR_*` flag values. |
| R3 | `src/protocol/file_attrs.rs:204-232` | `is_dir`/`is_regular`/`is_symlink` test `FileMode::contains` — a subset test on overlapping type values. |
| R4 | `src/protocol/file_attrs.rs:247-249` | `file_type()` does `unwrap_or_default()` on an absent mode: "unknown" and "mode 0" both give `Other`. |
| E1 | `src/protocol/file_attrs.rs:45-55` | the permission flags have no setuid, setgid or sticky, read with `from_bits_truncate`. |
| R12 | `src/protocol/file_attrs.rs:366-369` | setting only one of uid/gid sends `unwrap_or(0)` for the other. |
| R14 | `src/protocol/file_attrs.rs:371-373` | the permissions field is written as the raw mode word. |
| R15 | `src/protocol/file_attrs.rs:375-378` | setting only one of atime/mtime sends `unwrap_or(0)` for the other. |
| R9a | `src/protocol/file_attrs.rs:380` | `// todo: extended implementation`, in the serialiser. |
| R8 | `src/protocol/file_attrs.rs:404`, `:429-439` | the decoder keeps the `EXTENDED` flag and returns after `mtime`, leaving the tail unread. |
| R6 | `src/protocol/file_attrs.rs:429-438` | the decoder reads atime, then mtime. |
| R11 | `src/protocol/file_attrs.rs:192`, `:287-299` (2.1.1: `:297-311`) | 2.1.1's `impl Default for FileAttributes` sets `size: Some(0)` and every other field to `Some`; 2.4.0 derives `Default` (all `None`) and adds `dummy()`. Commit `1d4d1bd0`, 2026-08-03, *"fix: omit default attributes; dummy() for placeholders (closes #89)"*. |
| E3 | `src/protocol/file_attrs.rs:287-299` | `dummy()` — the attributes a `REALPATH` reply carries. |
| R10 | `src/client/fs/mod.rs:13` | `pub type Metadata = FileAttributes`. |
| R11b | `src/client/session.rs:240-251` | `metadata()` returns the stat attributes verbatim, and `set_metadata()` sends them back as they are, `size` included — a read value written back re-sends `ATTR_SIZE`. |
| E6a | `src/client/session.rs:182-187` | `read_dir` keeps `filename` and `attrs` and drops `longname`. |
| E2 | `src/protocol/status.rs:6-41` | `StatusCode` is a serde enum of nine variants with no catch-all. |
| E4a | `src/protocol/status.rs:46-51` | the STATUS message and language tag are read unconditionally. |
| R27 | `src/client/mod.rs:41-49` | `Config` defaults: `max_packet_len` 262,144, `request_timeout_secs` 10. |
| R25 | `src/client/mod.rs:74` | the reader is called with `read_packet(stream, u32::MAX)`. |
| R26 | `src/utils.rs:21` | `vec![0; length as usize]` — sized by the declared length. |
| R31 | `src/client/mod.rs:88-89`, `:105`, `:121` | a `CancellationToken` shared by the reader and writer tasks. |
| R32 | `src/client/mod.rs:110-127` | a spawned writer task owns the write half and drains an unbounded channel. |
| R33 | `src/client/mod.rs:119` | `let _ = wr.write_all(..)` — a failed write is discarded. |
| R28 | `src/client/rawsession.rs:2` | the pending map is a synchronous `DashMap`. |
| R30 | `src/client/rawsession.rs:31`, `:240` | the pending map is keyed on `Option<u32>`; `None` is the handshake's. |
| R24 | `src/client/rawsession.rs:199-203` | an outbound packet over `max_packet_len` is refused. |
| R23 | `src/client/rawsession.rs:206`, `:218` | the pending map `insert`s with no collision check; a dropped sender surfaces as "sender dropped". |
| R34 | `src/client/rawsession.rs:212-216` | `request()` enqueues, then starts the reply timeout — the clock includes the write. |
| R29 | `src/client/rawsession.rs:239-246` | `init()` goes through the same timed `request` path. |
| E5 | `src/client/rawsession.rs:380` | `write_nowait` returns the reply receiver; `File`'s write path calls it. |

### Issues and commits

| id | what it is |
|---|---|
| #17 | *"Read_dir unable to read link file"*. Closed 2024-02-11. |
| #33 | Concurrent requests read mangled packets: a `select!` cancelled a half-read packet. Closed 2024-09-27, fixed with `io::split` and a `CancellationToken`. |
| #36 | `From<FileMode> for FileType` used `contains`. Closed 2024-07-16, fixed to `==`. The `is_*` predicates (R3) still use `contains` in 2.4.0 — the same class at a site #36 did not report. |
| #42 | Support non-UTF-8 file names. **Open** since 2024-07-11. The maintainer, 2024-07-13: *"Replacing `String` with `Vec<u8>` also won't yield results because the packet defines the number of characters, not bytes."* — contradicted by R37. |
| #57 | Some servers refuse `open` with `PermissionDenied` unless `FileAttributes` is sent, while `metadata()` works. Closed 2024-11-05. |
| #89 | The `Default` in R11 made an "empty" attribute set carry `size: Some(0)`. Fixed in 2.4.0 (R11). |
| #95 | The reply timeout starts when `send()` pushes to an unbounded channel, before the socket write (R34). **Open**, created 2026-07-27. |
| `9010bffd` | 2025-02-24, *"fix: swap accessed and modified time assignments"* — in `From<&Metadata>`, the server half's conversion of local metadata, not the wire decoder. |

### The eight upstream traps

Counted over every upstream issue and commit in 2026-08 (PenTerm). Two are real-server quirks:
**#57**, and **chunk size must follow the server's advertised `max_packet_len`, not a constant**
(2026-04-30). Six are implementation traps: **#33**, **#36**, **#89**, **`9010bffd`**, **#17**,
**#95**. Where each lands here: [request pairing](territory/request-pairing.md) (#33, #95),
[file attributes](territory/file-attributes.md) (#36, #89, the atime/mtime order),
[path verbs](territory/path-verbs.md) (#57), [transfer lengths](territory/transfer-lengths.md)
(the chunk size). #17 has no site of its own here: no measurement ties it to a line of this crate.

## openssh-sftp-protocol and ssh_format

| id | path:lines | what it shows |
|---|---|---|
| R2 | openssh-sftp-protocol `src/constants.rs:68-72` | the `ATTR_*` flag values, the same as R1. |
| R18 | openssh-sftp-protocol `src/constants.rs:22` | `SSH2_FILEXFER_VERSION = 3`. |
| R5 | openssh-sftp-protocol `src/file_attrs.rs:313-325` | `get_filetype() -> Option<FileType>`, documented *"filetype is only set by the sftp-server"*. |
| R7 | openssh-sftp-protocol `src/file_attrs.rs:401-404`, test `:549` | atime before mtime, pinned by its own serde test. |
| R13 | openssh-sftp-protocol `src/file_attrs.rs:307-311`, `:349-351` | the type nibble is dropped on write (`Permissions::from_bits_truncate`) before the permissions are serialised. |
| R21 | openssh-sftp-protocol `src/request.rs:276-281` | `SSH_FXP_SYMLINK` is sent as `targetpath, linkpath` — swapped from the draft. |
| R22 | openssh-sftp-protocol `src/response.rs:245-249` | status codes 6 and 7 are rejected at parse time: *"Server MUST NOT return"*. |
| E6b | openssh-sftp-protocol `src/response.rs:191` | `longname` is read and discarded. |
| E4b | openssh-sftp-protocol `src/response.rs:177-180` | the STATUS message and language tag are read unconditionally. |
| R36 | ssh_format `src/de.rs:226` | a string is decoded with `String::from_utf8`, failing as `InvalidStr` — the whole response is rejected. |
| R40 | ssh_format `src/ser.rs:32-36` | the header length counts the type byte. |

## OpenSSH

| id | where | what it shows |
|---|---|---|
| R41 | `PROTOCOL` § 4.8 | `limits@openssh.com`: four `uint64` — max packet, read, write, open handles; *"If the server doesn't enforce a specific limit, then the field may be set to 0."* |
| R21b | `PROTOCOL` § 4.1 | OpenSSH's server reversed `SSH_FXP_SYMLINK`'s arguments and tells clients to send `targetpath, linkpath`. |
| R42 | `sftp-server.c` `process_extended_limits` | writes packet, read, write, handles, in that order. |
| R43 | `sftp-client.c`, after `sftp_get_limits` | the download and upload lengths floor at 64. |
| R44 | `sftp-server.c` `handle_to_string` | a handle is 4 bytes. |

## Specifications

- **draft-ietf-secsh-filexfer-02** — § 3 (framing), § 4 (`SSH_FXP_VERSION` extensions run to the
  end), § 5 (attributes; permissions *"a bit mask of file permissions as defined by posix"*),
  § 6.3 (`OPEN` carries attributes), § 6.4 (*"this may return fewer bytes than requested"*), § 6.10
  (`SYMLINK` argument order), § 6.11 (`REALPATH` reply attributes are dummy), § 7 (status codes),
  § 10.1 (v3 added STATUS's message and language tag).
- **RFC 4251 § 5** — an SSH `string` is *"arbitrary length binary string ... allowed to contain
  arbitrary binary data, including null characters and 8-bit characters"*.
