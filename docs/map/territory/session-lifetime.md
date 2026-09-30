# Session lifetime

## What it is

How a `Session` begins and ends: `Session::open` runs the handshake, spawns the reader and writer
tasks and asks for the server's limits; `Session::close` drains and stops; `Drop` stops without
draining. Plus the frame reader both the handshake and `read_loop` use.

## Governing decisions

**None.**

## Design model

- **The stream is the only thing a `Session` knows about the world.** The type is not generic in its
  own signature on purpose — a caller should not have to name the stream type to hold a session — but
  the bound at `open` is exactly `AsyncRead + AsyncWrite + Unpin + Send + 'static`
  ([package and release](package-and-release.md)).
- **The handshake is on a clock**, and it is the one exchange where forgetting that hangs forever. A
  server can accept the `sftp` subsystem channel and then never send `SSH_FXP_VERSION` — the
  wedged-but-accepted case a caller has to tell apart from a dead link. `russh-sftp` routes its
  `init()` through the same timed path as every other request (R29).
- **The handshake runs before the stream is split**, so it needs no request id and no entry in the
  pending map. `russh-sftp` instead keys its map on `Option<u32>` and reserves `None` for this one
  exchange (R30); doing it inline removes the special case rather than encoding it in the key type.
- **Anything but v3 is refused at the handshake** (`Error::UnsupportedVersion`). Neither reference
  implementation has this guard (E4a, E4b), and it is not pedantry: it is what lets `Response::decode` read
  STATUS's message and language tag without a version check ([packets](packets.md)).
- **`limits@openssh.com` is asked once, at `open`, and only of a server that advertised it.** A
  status, a short body or a reply timeout leaves `None` and the defaults stand; a link that is gone
  during the query (`Eof`, `SessionEnded`, `WriteTimeout`) fails the open, because reporting success
  over a dead link is a lie ([transfer lengths](transfer-lengths.md)).
- **`read_packet` checks the declared length before sizing the body.** A length of 0 is `Truncated`;
  a length above the ceiling is `TooLong` ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)).
  The handshake and `read_loop` each pass `Config::max_inbound_packet`, and the reader's copy is the
  one that matters: every `NAME` and `DATA` reply arrives through it.
- **`close` drains what is already queued before stopping the writer.** Dropping the sender closes
  the channel, so the writer finishes the queue and exits on its own; it is not aborted, so no packet
  is truncated part-way. An earlier version aborted the writer first, throwing away every packet still
  in the channel — and the one most likely to be there is the `SSH_FXP_CLOSE` a listing queued to
  release its handle ([every handle is closed](../invariant/every-handle-is-closed.md)). Waiting for
  the writer is bounded by `write_timeout`: a wedged peer must not make teardown wait forever either.
- **`Drop` aborts the reader and not the writer, and that asymmetry is the point.** Dropping the
  sender lets the writer finish its current packet and the queue; aborting it would be the one
  remaining way to leave half a packet on the wire. The reader only reads, so stopping it mid-call
  costs nothing.
- **`Debug` is hand-written** because neither task handle nor the channel is `Debug`. It shows what a
  reader of a panic message needs — which server this is and how much work is outstanding — and
  deliberately not the stream.

## Code

- `src/session.rs` — `Session` (`open`, `query_limits`, `handshake`, `server_version`,
  `server_limits`, `close`), `impl Debug for Session`, `impl Drop for Session`, `read_packet`,
  `eof_as_eof`

## Reference behaviour

- `russh-sftp`'s timed `init()` and `Option<u32>` pending key are rows in
  [reference](../reference.md#russh-sftp).

## Cross-cutting invariants

- [The far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)
- [Every handle is closed](../invariant/every-handle-is-closed.md)

## Blast radius

- [Request pairing](request-pairing.md) — the loops `open` spawns.
- [Transfer lengths](transfer-lengths.md) — the limits `open` stores.
- [Packets](packets.md) — the handshake's codec.

## Known holes / open

**None.**
