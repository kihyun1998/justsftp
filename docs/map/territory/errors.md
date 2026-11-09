# Errors

## What it is

`Error`, the crate's one error type; `Status`, a server's `SSH_FXP_STATUS` answer; and `StatusCode`,
the status number as it comes off the wire.

## Governing decisions

**None.**

## Design model

- **`StatusCode::Unknown(u32)` is load-bearing, not defensive.** `russh-sftp` models the code as a
  serde enum with nine variants and no catch-all (E2), so a server answering with a code from a later
  protocol version does not produce an unknown *status* — it fails the whole packet, and the request
  it belonged to surfaces as a decode error with the real reason discarded. A status code is a number
  on the wire; refusing to hold one without a name buys nothing and loses the reply.
- **Codes 6 and 7 are kept.** `openssh-sftp-protocol` rejects `NoConnection` and `ConnectionLost` at
  parse time on the grounds that they are locally generated pseudo-errors a server "MUST NOT return"
  (R22).
  That reasoning is about what a *correct* server does; a client that turns a protocol violation into
  a decode failure loses the ability to report what the server actually said. The value is carried
  and the caller decides.
- **`Eof` is not an error, and answers only a walk and a read.** It is how the server ends a directory
  walk and a file read, so `is_error` excludes it with `Ok`. It does not answer anything else:
  `status_ok`, behind every verb that expects a status, accepts `Ok` alone, as OpenSSH does for a write
  (R51). Otherwise a `WRITE` answered with `EOF` would count as acknowledged, past bytes the server may
  not hold, and a caller would resume after them.
- **`Status.message` is prose a remote server wrote**, parsed as text and handed over verbatim,
  escape sequences and control characters included. Every other `Error` variant is a structured
  value, not a sentence. Whether it may be shown is the caller's
  ([mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)).
- **A failing `STATUS` becomes `Error::Status`, not a shape complaint**, even where the verb expected
  another reply type (`client::unexpected`): *the server said no* is the useful sentence — the caller
  wants `NoSuchFile`, not "expected HANDLE".
- **Each pair of look-alike variants is kept apart on purpose:**
  - `RequestIdInUse` names a wrapped counter — something already waiting under the id, or an
    abandoned open's reply still due. There is no error for the inverse, a reply nothing waits on:
    `read_loop` drops it, closing an abandoned open's handle first
    ([request pairing](request-pairing.md)). An earlier draft reused an "unknown id" variant for the
    wrapped counter, so the log line read as the opposite of what happened.
  - `WriteTimeout` versus `Timeout` — `Timeout` means no answer arrived in budget; `WriteTimeout`
    means the bytes did not get out, a peer that accepted the subsystem and then stopped reading so the
    channel window never reopens. Either can also be a slow link with other requests ahead
    ([request pairing](request-pairing.md)). Collapsing the two reports "the server is slow" for a connection
    that is wedged ([request pairing](request-pairing.md)).
  - `SessionEnded { cause }` versus `Eof` — `Eof` is a false statement for a `TooLong` refusal,
    where the stream is not over and this client refused to continue.
- **`SessionEnded` holds an `Arc`, and it is not decoration.** `Error` cannot be `Clone` —
  `std::io::Error` is not — so the `Arc` is what lets every waiter learn the one real cause.
- **`Truncated` carries what was asked for and what was left**, so a fixture that is one byte short
  says so instead of saying "bad message".
- **`TooLong` is the guard against an allocation sized by the far end**
  ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)), the outbound refusal
  ([transfer lengths](transfer-lengths.md)), and `ReadAhead`'s refusal of a reply longer than asked.
  A new variant was not added for that: `Error` is not `non_exhaustive`, so one would break an
  exhaustive `match`.
- **`Error::repeated` is not `Clone`.** `std::io::Error` cannot be cloned, so an `Io` repeats with its
  kind and text only; the rest are copied. A `WriteBehind` holds it for the calls after a failure.

## Code

- `src/error.rs` — `Result`, `StatusCode` (`from_wire`, `to_wire`, `is_error`), `Status`, `Error`
  (`repeated`)
- `src/client.rs` — `unexpected`, `status_ok`

## Reference behaviour

- `russh-sftp`'s status enum and `openssh-sftp-protocol`'s rejection of 6 and 7 are rows in
  [reference](../reference.md).

## Cross-cutting invariants

- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)
- [Paths are bytes](../invariant/paths-are-bytes.md) — `Status.message` and `language_tag` are its
  only exception.

## Blast radius

- [Request pairing](request-pairing.md) — a caller's retry decision reads the difference between
  `RequestIdInUse`, `WriteTimeout`, `Timeout`, `SessionEnded` and `Eof`; merging or renaming a
  variant here changes what those paths mean.
- [Session lifetime](session-lifetime.md) — `query_limits` fails the open on exactly `Eof`,
  `SessionEnded` and `WriteTimeout` and treats everything else as "no limits"; a new connection-level
  variant is silently read as "no limits" until it is added there.
- [Packets](packets.md) — `StatusCode::from_wire` must hold every number `Response::decode` reads, or
  a reply fails as a decode error instead of a status.
- [Listing](listing.md) and [file transfer](file-transfer.md) — both end their loops on
  `StatusCode::Eof`; `is_error` deciding otherwise turns every completed walk and download into a
  failure.

## Known holes / open

**None.**
