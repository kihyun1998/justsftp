# Scope

## What it is

What the verb set covers: listing, transfer and basic metadata over SFTP v3 — the `impl Session`
block in `src/client.rs` (listing, reading and writing files, `stat`/`lstat`/`fstat`, `set_stat`,
`remove`, `rename`, `mkdir`, `rmdir`, `read_link`) and `src/protocol.rs` (`Request`).

## Governing decisions

**None** in this repository. Inherited from PenTerm's ADR-0011 § SFTP scope (S1): bounded by the
SFTP protocol, not by a packet list — `SSH_FXP_EXTENDED` is inside it, and each extension is
admitted on its own argument. So far only `limits@openssh.com`.

## Design model

- **v3 only, and the handshake refuses anything else** rather than negotiating down. Draft § 10.1
  records that STATUS's message and language tag were added in v3; `russh-sftp` and
  `openssh-sftp-protocol` both read them unconditionally while hard-coding v3, so against a v2
  server every status packet is over-read. Refusing at the handshake is what lets
  `Response::decode` read them without a per-packet version check (`session.rs`, `handshake`).
- **`SSH_FXP_SYMLINK` is left out.** The draft orders its arguments `linkpath, targetpath`;
  OpenSSH's server swaps them, and `openssh-sftp-protocol` follows OpenSSH. Either order is wrong
  against half the servers and nothing on disk settles it (`protocol.rs`, `Request`).
- **Mechanism here, policy in the caller.** The crate owns every loop that holds a handle — walk,
  download, upload — because a server has a finite number of handles and a caller that drove the
  loop itself would have to reproduce the close. How big a chunk is, how many requests are in
  flight, and when to stop are passed in (`Walk`, `Feed`, `chunk_len`).
- **Not pipelined.** `Session::enqueue` already returns the reply receiver without awaiting, so a
  pipelining policy can be exposed without reshaping the session; it stays private until one is
  measured.
- **Server limits only lower lengths.** `limits@openssh.com` is asked once at `Session::open`, only
  of a server that advertised it. A field of `0` is "no limit stated" (OpenSSH `PROTOCOL` § 4.8);
  lengths floor at 64 bytes, as OpenSSH's client does, so a broken server cannot drive a chunk to
  zero. Real OpenSSH states 261,120 for read and write.

## Blast radius

- [Upstream traps](upstream-traps.md)
