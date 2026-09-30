# Paths are bytes

## The rule

Every field that **addresses** something — a path, a filename, a long name, a handle, an extension
name or value — is read as a `u32` count and that many raw bytes, kept as `Vec<u8>`, and sent back
unchanged. Only the two fields draft-ietf-secsh-filexfer-02 defines as text, the `SSH_FXP_STATUS`
error message and its language tag, are `String`.

## Why

- **A name that does not survive the listing is untouchable.** SFTP has no open-by-directory-entry:
  `SSH_FXP_OPEN`, `REMOVE` and `RENAME` all take a path. A client that decodes a filename lossily
  cannot send the server's bytes back, so the file is visible in the listing and can never be
  opened, transferred, renamed or deleted.
- **The Korean case is the one to understand.** `한글.txt` in EUC-KR is `C7 D1 B1 DB 2E 74 78 74`.
  `C7` is an invalid lead byte and becomes U+FFFD, but the `D1 B1` after it is a *valid* UTF-8
  sequence decoding to U+0471, a Cyrillic letter. The name does not read as broken — part of it
  reads as ordinary text in another script. The fixture in `tests/round_trip.rs` and in
  `wire.rs`'s own test is these eight bytes.
- **The cause is `serde`, not a type choice.** Encode with `serde` → every field must implement
  `Serialize` → a text field is `String` → `String` is UTF-8 by type in Rust → a lossy decode is
  the only one left. A cursor never takes that branch, because reading four bytes and then that
  many more is not a typed operation at all. That is why the wire cursor is hand-rolled and why the
  crate has no `serde` ([package and release](../territory/package-and-release.md)).
- **The normative type backs this up rather than merely permitting it.** RFC 4251 § 5 defines an
  SSH `string` as *"arbitrary length binary string... allowed to contain arbitrary binary data,
  including null characters and 8-bit characters"*, and the draft mandates UTF-8 for exactly one
  field pair — the STATUS error message and language tag.
- **The split is the point.** A crate where *everything* is bytes would be the wrong change: the
  two text fields are shown to a person and never used to address anything, so a replacement
  character there costs legibility and cannot cost reachability. That reason does not generalise,
  which is why `Reader::text` has exactly two callers.

## Sites

- [Wire cursor](../territory/wire-cursor.md) — `Reader::string` returns bytes; `Reader::text` is
  lossy and called only for the STATUS message and language tag.
- [Packets](../territory/packets.md) — `Request` paths, `DirEntry.filename` and `longname`,
  `Handle`, `ServerVersion.extensions`.
- [File attributes](../territory/file-attributes.md) — `Extension` name and value.
- [Errors](../territory/errors.md) — `Status.message` and `language_tag`, the only `String`s.
- [Listing](../territory/listing.md) — `real_path` and `read_link` return bytes, not a `DirEntry`.

## Where each site came from

The same lossy decode reaches more than filenames in the reference implementations
([reference](../reference.md#russh-sftp)):

- **The handle is a second corruption site, independent of the first.** `russh-sftp` types it
  `String` (R19), so an opaque binary token goes through the same `from_utf8_lossy`. A mangled handle
  addresses the wrong file — or nothing — on every later read, write and close, and no filename has
  to be unusual for it to happen. Servers are free to put a pointer, a counter or a nonce in it.
- **Extension names are bytes too.** The draft never calls them text. `russh-sftp` reads them as
  `String` through its lossy path, so a non-UTF-8 extension name becomes U+FFFD and then silently
  fails its own `has_extension` comparison — a supported extension reads as absent.
- **`longname` contains a filename**, so it is bytes although it is meant for display.

## Drawing a name is the caller's

The crate has no encoding dependency and never converts a path. Turning a filename's bytes into
something to draw is a decoding decision that belongs with the caller's other decoding decisions —
for a terminal client, the session's encoding label. A crate that decoded for display would put one
fact in two places.
