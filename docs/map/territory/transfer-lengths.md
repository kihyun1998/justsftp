# Transfer lengths

## What it is

How big a packet may be in each direction, and how much data fits in one `SSH_FXP_READ` or
`SSH_FXP_WRITE`: the two ceilings in `Config`, the per-packet overheads, the server's stated
`limits@openssh.com`, and what the crate reports so a caller can split (`Session::max_read_len`,
`Session::server_read_len`, `WriteFile::max_chunk`).

## Governing decisions

**None.**

## Design model

- **There has to be an inbound ceiling** (`max_inbound_packet`, 4 MiB by default). `russh-sftp`
  passes `u32::MAX` to its reader, so a server can make it allocate 4 GiB from one header (R25,
  R26); its outbound check (R24) does not help, because it guards what is sent
  ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)). The default is 16× the
  256 KiB default outbound packet, which leaves room for a large `READDIR` batch while keeping the
  number bounded.
- **The outbound ceiling refuses; it does not chunk** (`max_outbound_packet`, 262,144, matching
  `russh-sftp`'s default, R27). *"Read/write chunk size must follow the server's advertised
  `max_packet_len`, not a constant"* is one of the eight upstream traps, answered here in two halves
  ([mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)).
- **The two overheads count different things — do not "fix" the asymmetry.** `WRITE_OVERHEAD` (25)
  is length prefix(4) + type(1) + id(4) + handle length prefix(4) + offset(8) + data length
  prefix(4), because the check it feeds is `req.encode(id).len() > max_outbound_packet` and `encode`
  returns the framed packet, prefix included — `protocol.rs`'s encoding test pins a 1-byte handle and
  2 data bytes at 28 total. `READ_OVERHEAD` (9) is type(1) + id(4) + data length(4): it bounds a
  requested length against the reply that comes back.
- **The write chunk is per file, not per session.** The handle's own bytes are not in
  `WRITE_OVERHEAD`, because the server chooses that length per file and may make it up to 256 bytes.
  A session-wide number would be right for OpenSSH's 4-byte handle and wrong, by exactly the
  difference (R44), for a server that hands out longer ones — so `max_chunk` lives on `WriteFile`, the twin
  of `max_read_len`.
- **The chunk arithmetic is easy to get wrong from outside.** A consumer's upload loop picked 256 KiB
  — the same number as `max_outbound_packet` — so every full chunk overflowed by the header and
  `Session::write` refused it; an upload of any file larger than one chunk had never worked
  (PenTerm). The read side never had the bug because `max_read_len` already existed; the write side
  had no twin until `max_chunk`.
- **Off by one in the small direction is the silent half.** Too large is refused loudly; too small
  merely wastes a fraction of every packet and nothing ever says so. The tests pin the value from
  both sides for that reason ([verification](verification.md)).
- **A server's stated limits only ever lower a length.** `max_read_len` and `max_chunk` each take
  the smaller of this client's ceiling and the server's field; a larger field is ignored.
  `Session::write` also refuses data above the server's write bound — the outbound ceiling is
  enforced where every request is encoded, and this is the server's own bound, which only a write
  has.
- **A field of `0` is "no limit stated"** (`stated`; R41).
- **Lengths floor at 64 bytes** (`SMALLEST_TRANSFER_LEN`), as OpenSSH's own client does after
  `sftp_get_limits` (R43), so a broken server cannot drive a chunk to zero and stall a transfer.
- **The floor also covers the packet bound, so it can exceed what the server stated.**
  `server_write_len` applies it after taking the smaller of the write field and the packet field less
  the header. A server stating a packet under 64 + header therefore gets writes it refuses: a failed
  transfer rather than a stall. PenTerm's note records that OpenSSH floors only read and write and
  ignores the packet field, and that its client reads a `0` as a limit where this crate reads no limit
  (`sftp-transfer`, PenTerm `2aec7cd40`, 2026-11-06; not re-read upstream).
- **`max_open_handles` is decoded and reported, and nothing here acts on it.** It reaches a caller
  through `Session::server_limits`; no verb counts handles.
- **`server_read_len` carries none of this client's ceiling**, for a caller whose read size is set
  by something else and only needs the server's bound. PenTerm's transfer engine is that caller: its
  sink sets the size and this caps it (`explorer_transfer/engine.rs`, PenTerm `2aec7cd40`, 2026-11-06).
- **Real OpenSSH states 261,120 for read and write** (PenTerm), under this client's defaults, so the
  limits lower lengths on the common server too.

## Code

- `src/session.rs` — `Config` (`max_inbound_packet`, `max_outbound_packet`), `READ_OVERHEAD`,
  `WRITE_OVERHEAD`, `SMALLEST_TRANSFER_LEN`, `stated`, `Session::max_read_len`,
  `Session::server_read_len`, `Session::server_write_len`, `Session::config`
- `src/client.rs` — `WriteFile::max_chunk`, `Session::write`
- `tests/limits.rs`

## Reference behaviour

- OpenSSH `PROTOCOL` § 4.8, `sftp-server.c` and `sftp-client.c`, and `russh-sftp`'s defaults are
  rows in [reference](../reference.md).

## Cross-cutting invariants

- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)
- [The far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)

## Blast radius

- [Request pairing](request-pairing.md) — `enqueue` refuses by the encoded length, so the overheads
  here have to agree with what it measures, or a chunk reported as fitting is refused.
- [Session lifetime](session-lifetime.md) — `read_packet` ends the session on an oversized reply, so a
  read length larger than the inbound ceiling is not refused but fatal.
- [Packets](packets.md) — `WRITE_OVERHEAD` is a hand count of `Request::Write`'s encoding.
- [File transfer](file-transfer.md) — callers split by these numbers, and a value that is too low only
  wastes a little of every packet with no error anywhere.
- [Verification](verification.md) — `tests/limits.rs` and the `max_chunk` tests pin the values from
  both sides; each changed number has a test that should redden.

## Known holes / open

- **No packet size above 262,144 has been measured** against a real server.
- **`max_read_len` looks only at the outbound ceiling.** With `max_inbound_packet` set below
  `max_outbound_packet`, a read of `max_read_len` can draw a reply above the inbound ceiling, which
  ends the session as `TooLong` rather than refusing the read. `Config` does not check the two against
  each other, and `read_file_watched` does not check `chunk_len` either. Found by reading; the defaults
  are safe (4 MiB inbound, 262,144 outbound).
- **Depth has no number here, and the SSH channel window under the crate sets what is useful.** It
  cannot see the window. Measured with russh 0.63.1's default 2,097,152 and chunk 261,120 at RTT 30 ms,
  download stops rising at depth 4 to 8 and upload at 8 to 16; a client window of 16 MiB at depth 64
  takes download from 33.4 to 101.7 MB/s
  ([#1's result](https://github.com/kihyun1998/justsftp/issues/1#issuecomment-6677251816)). russh tops
  up below half a window (R54), which matches about `window / (2 × RTT)` on download; upload is bounded
  by the server's window. Whether to document a formula resting on one client and one server is open.
  Tracked: [#1](https://github.com/kihyun1998/justsftp/issues/1).
