# Paths are bytes

## What it is

Every field that **addresses** something — paths, filenames, long names, handles, extension names
and values — is `Vec<u8>`, read as a length and that many raw bytes and never decoded. Only the
`SSH_FXP_STATUS` error message and its language tag are `String`: they are the two fields
draft-ietf-secsh-filexfer-02 mandates as UTF-8. Code: `src/wire.rs` (`Reader::string`),
`src/protocol.rs` (`DirEntry`, `Handle`), `src/error.rs` (`Status`).

## Governing decisions

**None** in this repository. The crate was written as PenTerm's ADR-0089 ("PenTerm owns its SFTP
implementation", D2 Amendment 2026-08-27) and moved here; PenTerm is not public, so that record is
cited as text.

## Design model

- **A name that does not survive the listing is untouchable.** SFTP has no open-by-directory-entry:
  `SSH_FXP_OPEN`, `REMOVE`, `RENAME` all take a path. A client that decodes a filename lossily
  cannot send the server's bytes back, so the file is visible in the listing and can never be
  opened, transferred, renamed or deleted.
- **The Korean case is the one to understand.** `한글.txt` in EUC-KR is
  `C7 D1 B1 DB 2E 74 78 74`. `C7` is an invalid lead byte and becomes U+FFFD, but `D1 B1` is a
  *valid* UTF-8 sequence decoding to U+0471, a Cyrillic letter. The lossy name does not read as
  broken. `tests/round_trip.rs` and `wire.rs`'s own test use these bytes.
- **The cause is `serde`, not a type choice.** Encode with `serde` → every field must implement
  `Serialize` → a text field is `String` → `String` is UTF-8 by type → `from_utf8_lossy` is the
  only decode left. A hand-rolled cursor never takes that branch, which is why the crate is written
  rather than forked (`src/wire.rs` header).
- **The handle is a second corruption site.** It is an opaque binary token (RFC 4251 § 5 `string`).
  Decoded as text, a mangled handle addresses the wrong file, or nothing, on every later read,
  write and close — with no unusual filename involved.
- **Extension names are bytes too.** The draft never calls them text. `openssh-sftp-protocol` reads
  them as bytes and skips a pair it cannot decode; `russh-sftp` decodes them lossily and then fails
  its own `has_extension` comparison, so a supported extension reads as absent.
- **Drawing a byte name is the caller's.** The crate has no encoding dependency and never converts a
  path ([dependency fence](dependency-fence.md)).

## Reference behaviour

Read at the installed versions in 2026-08 (PenTerm):

| Library | A non-UTF-8 filename |
|---|---|
| `russh-sftp` 2.1.1 / 2.4.0 | `from_utf8_lossy` — the folder opens, that file is untouchable. Upstream #42 open since 2024-07-11; the lossy decode was itself the fix (#39) for a listing that failed outright |
| `openssh-sftp-client` 0.15.8 | `Box<Path>` looks byte-safe, but serde's `PathBufVisitor` and `ssh_format::deserialize_str` enforce strict UTF-8 — **the whole folder fails to open** |
| `bssh-russh-sftp` 2.4.0 | a fork of `russh-sftp` for pipelined I/O; inherits `String` |
| OpenSSH `sftp` | bytes. The limit is the library, not the protocol |

## Blast radius

- [Dependency fence](dependency-fence.md) — adding `serde` reopens the chain above.
- [Upstream traps](upstream-traps.md) — the handle and extension findings sit beside the eight.
