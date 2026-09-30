# Upstream traps

## What it is

The defects `russh-sftp` shipped and later fixed, or still has, counted over all its issues and
commits in 2026-08 (PenTerm). They are the checklist this crate was written against; each is
designed out rather than inherited.

## Governing decisions

**None** in this repository.

## Design model

**Real-server quirks (2):**

- **#57** — some servers refuse `open` without `FileAttributes`.
- **Chunk size must follow the server's advertised `max_packet_len`, not a constant** (2026-04-30).
  Split here: refusing an oversized packet is the crate's (`Config::max_outbound_packet`),
  chunking a transfer to fit is the caller's. `WRITE_OVERHEAD` is 25 plus the handle's length,
  and the handle length is the server's choice — a consumer that picked an upload chunk without it
  overflowed every full chunk (PenTerm), which is why `WriteFile::max_chunk` exists.

**Implementation traps (6):**

- **#33** — mangled packets under concurrent requests. Here: one writer task owns the write half
  and nobody can cancel it mid-packet (`session.rs`, `write_loop`).
- **#36** — POSIX mode type bits overlap, so `contains()` misclassifies (`attrs.rs`, `FileType`).
- **#89** — handing `metadata()`'s value back to `set_metadata` re-sends `ATTR_SIZE` and truncates
  the file. Here it is unrepresentable: `FileAttributes` (what the server said) and `AttrsUpdate`
  (what to change) are separate types with no conversion.
- **atime/mtime swapped** (2025-02-24). Here atime is read first, pinned by
  `attrs.rs`'s `atime_is_read_before_mtime`.
- **#17** — symlink type in `read_dir`. Here a listing's type comes from the mode bits through
  the same `FileType` as #36.
- **#95** — the reply timeout starts on enqueue, so time blocked writing is charged to the server.
  Still open upstream. Here the reply clock starts after the bytes reach the stream, and the write
  has its own clock (`Config::write_timeout`, `Error::WriteTimeout`).

**Found while writing, not in the count:**

- The handle and extension names are decoded as text ([paths are bytes](paths-are-bytes.md)).
- `ATTR_EXTENDED` is recognised but its tail is not parsed, so leftover bytes desynchronise every
  later entry in the same `NAME` packet.
- Inbound packets are unbounded: `read_packet(stream, u32::MAX)` into `vec![0; len]` lets a server
  induce a 4 GiB allocation from one header. Here: `Config::max_inbound_packet`.

## Blast radius

- [Scope](scope.md)
- [Paths are bytes](paths-are-bytes.md)
