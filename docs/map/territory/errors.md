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
- **`Eof` is not an error.** It is how the server ends a directory walk and a file read, so
  `is_error` excludes it with `Ok`.
- **`Status.message` is prose a remote server wrote**, parsed as text and handed over verbatim,
  escape sequences and control characters included. Every other `Error` variant is a structured
  value, not a sentence. Whether it may be shown is the caller's
  ([mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)).
- **A failing `STATUS` becomes `Error::Status`, not a shape complaint**, even where the verb expected
  another reply type (`client::unexpected`): *the server said no* is the useful sentence — the caller
  wants `NoSuchFile`, not "expected HANDLE".
- **Each pair of look-alike variants is kept apart on purpose:**
  - `RequestIdInUse` versus `UnknownRequestId` — exact inverses, nothing waiting versus something
    already waiting. An earlier draft reused `UnknownRequestId` for the wrapped counter, so the log
    line read as the opposite of what happened.
  - `WriteTimeout` versus `Timeout` — `Timeout` means the server did not answer; `WriteTimeout`
    means the bytes never got out, a peer that accepted the subsystem and then stopped reading so the
    channel window never reopens. Collapsing the two reports "the server is slow" for a connection
    that is wedged ([request pairing](request-pairing.md)).
  - `SessionEnded { cause }` versus `Eof` — `Eof` is a false statement for a `TooLong` refusal,
    where the stream is not over and this client refused to continue.
- **`SessionEnded` holds an `Arc`, and it is not decoration.** `Error` cannot be `Clone` —
  `std::io::Error` is not — so the `Arc` is what lets every waiter learn the one real cause.
- **`Truncated` carries what was asked for and what was left**, so a fixture that is one byte short
  says so instead of saying "bad message".
- **`TooLong` is the guard against an allocation sized by the far end**
  ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)), and also the outbound
  refusal ([transfer lengths](transfer-lengths.md)).

## Code

- `src/error.rs` — `Result`, `StatusCode` (`from_wire`, `to_wire`, `is_error`), `Status`, `Error`
- `src/client.rs` — `unexpected`

## Reference behaviour

- `russh-sftp`'s status enum and `openssh-sftp-protocol`'s rejection of 6 and 7 are rows in
  [reference](../reference.md).

## Cross-cutting invariants

- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)

## Blast radius

- [Request pairing](request-pairing.md) — produces `RequestIdInUse`, `WriteTimeout`, `Timeout`,
  `SessionEnded`, `Eof`.
- [Packets](packets.md) — decodes `Status`.

## Known holes / open

- **`UnknownRequestId` is never constructed.** `read_loop` drops a reply whose id nothing waits on
  rather than failing anything ([request pairing](request-pairing.md)), so the variant is public and
  unreachable.
