# The far end sizes nothing

## The fact

A length or a count that arrives from the server is checked against what is actually present, or
against a configured ceiling, before it sizes an allocation or a slice.

## Why it is cross-cutting

The four sites below read different fields at different layers, and none reaches the check through a
shared primitive: `read_packet` has its ceiling before there is a `Reader`, `Reader::take` guards
slices, and the two `count` reservations in `Response::decode` and `FileAttributes::decode` happen
before any `take` runs. Removing the check at one leaves the others passing.

## Territories it holds in

- [Wire cursor](../territory/wire-cursor.md) — every read goes through `Reader::take`, which returns
  `Truncated { needed, had }` instead of slicing past the end.
- [Session lifetime](../territory/session-lifetime.md) — `read_packet` checks the frame length
  against `Config::max_inbound_packet` before sizing the body; both the handshake and `read_loop`
  pass the ceiling ([transfer lengths](../territory/transfer-lengths.md)).
- [Packets](../territory/packets.md) — a `NAME` reply's entry count reserves at most what the
  remaining bytes could hold.
- [File attributes](../territory/file-attributes.md) — the `ATTR_EXTENDED` count reserves at most 64.
- [Transfer lengths](../territory/transfer-lengths.md) — the inbound ceiling and its default.

## What a violation looks like

A decoder that trusts the field either panics on a slice or allocates whatever the sender asked for.
`russh-sftp` passes `u32::MAX` to its packet reader and then allocates `vec![0; length]`, so a server —
hostile, or merely broken — can make the client allocate 4 GiB from one packet header (R25, R26 in
[reference](../reference.md#russh-sftp)).

## Discovery history

- From reading `russh-sftp`'s reader against this crate's, rows R25 and R26. The reader's own copy of
  the ceiling was once untested; see [verification](../territory/verification.md).

## Where it will recur

Any code that reads a length or a count off the wire and then reserves, allocates or slices by it.
What this does not cover is the number of packets: a server can keep sending, and a listing grows
([listing](../territory/listing.md)).
