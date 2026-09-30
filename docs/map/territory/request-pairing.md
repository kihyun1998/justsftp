# Request pairing

## What it is

How one request becomes one reply: `enqueue` registers a waiter under a fresh id and hands the
encoded packet to the writer task; `request` waits for the bytes to reach the stream and then for the
reply; `read_loop` hands each reply to whoever is waiting on its id; `write_loop` owns the write half.
Two of the eight upstream traps are here, both on the concurrency path
([reference](../reference.md#the-eight-upstream-traps)).

## Governing decisions

**None.**

## Design model

The order of the steps in `enqueue` and `request` is the fix for upstream #33 and #95, and neither is
recoverable by a later change.

- **Liveness before anything else.** The reader ending is the ordinary shape of a peer going away —
  a half-close on TCP, `SSH_MSG_CHANNEL_EOF` on a channel — and the write half stays writable through
  it. Without the `reader_done` check a request issued afterwards enqueues happily, is written
  happily, and then waits the full reply budget for an answer that structurally cannot come,
  reporting `Timeout` — *"the server is slow"* — about a connection that is already gone.
  `russh-sftp` closes the same hole with a `CancellationToken` shared by both halves (R31).
- **① Register before writing.** A reply cannot arrive for a request that has not been sent, but it
  can arrive before this task is scheduled again — so registering after the write is a race that
  loses the reply and reports a timeout.
- **An id collision is an error, not an overwrite.** After `u32::MAX` requests the counter wraps,
  and silently answering the wrong caller is worse than failing. `russh-sftp` also registers first
  but `insert`s blindly: the previous sender is dropped and the earlier caller is told "sender
  dropped" (R23).
- **② Refuse an oversized packet before it is sent** ([transfer lengths](transfer-lengths.md)).
  `russh-sftp` does the same (R24).
- **③ The whole packet goes to the writer task in one synchronous send, and that is what makes
  `request()` cancel-safe.** An earlier draft took a lock on the write half and called `write_all`
  inline, which reads as simpler and is wrong: `write_all` is not cancel-safe, so a caller dropped
  inside it (a `select!`, an outer timeout, an aborted task) releases the lock with **half an SFTP
  packet on the wire**. The next request appends its frame directly after it, the server misreads
  the length prefix, and every packet from then on is misframed — silently, until the reader gives
  up. Both reference implementations avoid this the same way, by owning the write half in a task
  nobody can cancel (`russh-sftp`'s is R32).
  Cancelling a transfer part way is a supported operation, so this has to be right here.
- **④ The write is on its own clock** (`Config::write_timeout`). Splitting the write out of the reply
  budget is what makes the reply budget honest (upstream #95) — but an earlier version split it and
  then put no clock on the write half at all. A server that accepts the `sftp` subsystem and then
  stops reading never reopens the channel window, so the write pends forever and, because writes are
  serial, **every** request in flight pends behind it: no `Timeout`, no `Io`, no `Eof`. That is the
  same wedged-but-accepted case the handshake clock exists for, arriving on the other side. The write
  budget is deliberately larger than the reply budget: a slow write is ordinary on a congested link,
  whereas a server that has gone silent is not.
- **A failed write is reported rather than swallowed.** `russh-sftp`'s writer task discards the
  result of `write_all` (R33), so a send that never happened surfaces only as a reply timeout with no cause
  attached.
- **⑤ The reply clock starts after the bytes are on the stream.** That is upstream #95: there the
  timeout begins when `send()` returns, and `send()` only pushes onto an unbounded channel drained by
  a different task, so time spent blocked in `write_all` — an exhausted SSH channel window, a slow
  link — is charged against the *response* budget and reported as a server timeout (R34). So
  `Config::request_timeout` covers server latency and the response transfer only; do not compare it
  with `russh-sftp`'s 10 seconds (R27) without reading what each measures — there the budget is shared
  between waiting for the socket and waiting for the server, so a larger number here is also a
  stricter one.
- **A cancelled request reclaims its slot through a `Drop` guard (`Slot`), and nothing else would.**
  `read_loop` removes on a matching reply and `close()` drains, but a dropped future runs neither,
  because the sender half was moved into the map and only the receiver dies with the future. Against
  a server that never answers, the entry is permanent. The guard lives for the whole of `request`, so
  every early return and a caller dropped at any await point reclaim it; if the reply already
  arrived, the removal is a no-op.
- **The pending map is behind a synchronous mutex, deliberately.** No critical section holds the lock
  across an `.await` — each is an insert, a remove, or a drain followed by a synchronous
  `oneshot::send` — so an async mutex buys nothing, and it costs the one thing this map needs: a
  `Drop` impl can take a `std::sync::Mutex` guard and cannot take a `tokio::sync::Mutex` one.
  `russh-sftp` reaches the same place from the other direction, with a synchronous `DashMap` (R28). A
  poisoned lock is recovered rather than propagated: the map is plain data and a poisoned map is
  still a usable map.
- **`in_flight` is public because an invariant nothing can observe is an invariant nothing will
  keep.** With the map private and no accessor, no test could tell a working `Slot` guard from a
  missing one, which is how the leak survived a full adversarial pass. Steady state is zero.
- **`write_loop` owns the write half and is never aborted.** Nothing can cancel a packet part-way
  through: the only thing that awaits there is that task, and it ends when the channel closes.
- **A write failure reaches every request queued behind it.** An earlier version handed the real
  `io::Error` to the failing job and a synthesised bare `BrokenPipe` — with no `raw_os_error` — to
  the rest, which is indistinguishable in a log from a real one. One `Arc` is cloned to everybody.
- **`read_loop` reads the id before decoding the body**, so a packet this client cannot parse fails
  that one request with a real reason instead of killing the session.
- **A reply nobody is waiting for is dropped.** The request timed out and deregistered, or the
  server is answering twice; tearing the session down over a packet that harms nothing would turn a
  stray packet into a dropped connection.
- **When the reader ends, every waiter gets the real cause.** An earlier draft did `Err(_) => break`
  and then told everyone `Eof`, which made a `TooLong` refusal — the whole point of the inbound
  ceiling — indistinguishable from a server hanging up. A later one sent the cause to one waiter and
  `Eof` to the rest, and `HashMap::drain` has no order, so *which* caller learned the truth was
  nondeterministic between runs. `reader_done` is published before the drain, so a racing request
  either sees the flag and fails fast or is already in the map and gets the cause.
- **`enqueue` is split out of `request` for pipelining.** Pipelined file I/O needs a dispatch that
  returns the reply receiver without awaiting anything, so a caller can keep N requests outstanding
  — `russh-sftp`'s `send`, exposed as `write_nowait` and consumed by its `AsyncWrite for File` (E5). Fused
  into one `async fn`, a `poll_write` built on top could not return until the server answered, which
  is zero pipelining. It stays private until a pipelining policy is measured; the split keeps the
  seam cheap to expose.

## Code

- `src/session.rs` — `Pending`, `lock`, `Slot`, `Outbound`, `Session::enqueue`, `Session::request`,
  `Session::in_flight`, `write_loop`, `read_loop`

## Reference behaviour

- `russh-sftp`'s `rawsession.rs` and `client/mod.rs` were read for every comparison above; rows in
  [reference](../reference.md#russh-sftp).

## Cross-cutting invariants

- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)

## Blast radius

- [Session lifetime](session-lifetime.md) — `open` spawns the two loops; `close` and `Drop` stop them.
- [Errors](errors.md) — every variant this produces.
- [Verification](verification.md) — `tests/concurrency.rs` holds each step above.

## Known holes / open

- **Pipelining is not built.** No measurement has picked a policy.
