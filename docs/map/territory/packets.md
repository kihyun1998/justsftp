# Packets

## What it is

The SFTP v3 vocabulary: the packet type numbers, every `Request` this client can send and how it
encodes, every `Response` it can decode, the handshake's two halves (`SSH_FXP_INIT`,
`SSH_FXP_VERSION`), and the `limits@openssh.com` reply.

## Governing decisions

**None** in this repository. The verb set follows PenTerm's ADR-0011 § SFTP scope (S1) — listing,
transfer and basic metadata, bounded by the SFTP protocol rather than by a packet list, with each
extension admitted on its own argument (see [MAP](../MAP.md#penterm-provenance)).

## Design model

- **v3 is a decision, not a default.** Both reference implementations hard-code it (R17, R18) and it is what
  essentially every server speaks. The draft's § 10.1 records that v3 *added* the error message and
  language tag to `SSH_FXP_STATUS`, so a client that speaks v3 and is answered with v2 over-reads
  every status packet; neither reference has a version branch (E4a, E4b). Here the handshake refuses anything
  else ([session lifetime](session-lifetime.md)), and that refusal is what lets `Response::decode`
  read the two fields unconditionally.
- **`SSH_FXP_SYMLINK` is deliberately absent.** The draft's § 6.10 gives the arguments as `linkpath`
  then `targetpath`, and `russh-sftp` follows it (R20); `openssh-sftp-protocol` swaps them on purpose
  (R21) because the OpenSSH server itself deviates and says so (R21b, [reference](../reference.md)). Whichever order is
  chosen is wrong against half the server population, and nothing on disk settles it — it needs a
  measurement against real servers. Creating a symlink is also outside this crate's scope, so the
  verb is left out rather than shipped as a coin flip. `READLINK` takes one path and has no such
  ambiguity, so it stays.
- **`Handle` is bytes** ([paths are bytes](../invariant/paths-are-bytes.md)).
- **`DirEntry.longname` is kept rather than discarded.** Both reference implementations throw it
  away (E6a, E6b). The draft describes it as *"suitable for use in the output of a directory listing
  command"*, and it is the only place a v3 server states a file's type as text — which matters
  precisely when the attributes carry no `PERMISSIONS` flag.
- **`OpenFlags::contains` is a subset test, and that is legal here.** These really are independent
  bits. Upstream #36 is about a *type field* wearing a bitfield's clothes
  ([file attributes](file-attributes.md)).
- **`Response::packet_type` returns a number, not a label.** An earlier draft returned
  `&'static str` and the caller mapped the string back to a number; a one-character drift in either
  copy of the literal silently collapsed every unexpected reply onto a single code, with no compile
  error to catch it.
- **The request id is read before the body is decoded**, by the caller of `Response::decode`,
  because it needs the id to find who is waiting before it knows whether the body will decode
  ([request pairing](request-pairing.md)).
- **A `NAME` reply's count cannot size an allocation on its own** — a declared 4 billion entries
  would reserve before a single one was read. Each entry costs at least 12 bytes on the wire, so what
  is actually present bounds the reservation
  ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)).
- **`SSH_FXP_EXTENDED` carries its fields after the name with no length prefix of their own** — each
  extension defines its own layout — and `SSH_FXP_EXTENDED_REPLY` is kept undecoded after the request
  id, for the extension that was asked to read.
- **The handshake is the one exchange with no request id.**
- **`SSH_FXP_VERSION`'s extensions run to the end of the packet and carry no count** (draft § 4).
  Both reference implementations agree.
- **`limits@openssh.com`'s four `u64`s decode in the order OpenSSH writes them**: packet, read,
  write, handles (R42). A field of `0` means no limit stated (R41). A short body is an error, not zeros.
- **Open flags are wire values**, confirmed identical in both reference implementations. The tests
  assert the open flags and the `Response` type numbers as literals.

## Code

- `src/protocol.rs` — `VERSION`, `packet`, `LIMITS_EXTENSION`, `OpenFlags`, `Handle`, `DirEntry`,
  `Request` (`packet_type`, `encode`), `Response` (`packet_type`, `decode`), `encode_init`,
  `ServerVersion` (`advertises`), `ServerLimits`, `decode_limits`, `decode_version`

## Reference behaviour

- `russh-sftp` 2.4.0 and `openssh-sftp-protocol` were read for every field order here; the rows are
  in [reference](../reference.md).

## Cross-cutting invariants

- [Paths are bytes](../invariant/paths-are-bytes.md)
- [The far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)

## Blast radius

- [Request pairing](request-pairing.md) — `Request::encode` is what the outbound ceiling measures.
- [Session lifetime](session-lifetime.md) — the handshake uses `encode_init` and `decode_version`.
- [Transfer lengths](transfer-lengths.md) — `WRITE_OVERHEAD` is the shape `Request::Write` encodes to.
- [Errors](errors.md) — `Status` is decoded here.

## Known holes / open

- **`SSH_FXP_SYMLINK`'s argument order** is unmeasured against real servers.
