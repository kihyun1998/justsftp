# The far end sizes nothing

## The rule

A length or a count that arrives from the server is checked against what is actually present, or
against a configured ceiling, before it sizes an allocation or a slice.

## Why

A field's length arrives from the far end, so a decoder that trusts it either panics on a slice or
allocates whatever the sender asked for. `russh-sftp` passes `u32::MAX` to its packet reader and then
allocates `vec![0; length]`, so a server — hostile, or merely broken — can make the client allocate
4 GiB from one packet header (R25, R26 in [reference](../reference.md#russh-sftp)).

## Sites

- [Wire cursor](../territory/wire-cursor.md) — every read goes through `Reader::take`, which returns
  `Truncated { needed, had }` instead of slicing past the end.
- [Session lifetime](../territory/session-lifetime.md) — `read_packet` checks the frame length
  against `Config::max_inbound_packet` before sizing the body; both the handshake and `read_loop`
  pass the ceiling ([transfer lengths](../territory/transfer-lengths.md)).
- [Packets](../territory/packets.md) — a `NAME` reply's entry count reserves at most what the
  remaining bytes could hold.
- [File attributes](../territory/file-attributes.md) — the `ATTR_EXTENDED` count reserves at most 64.
