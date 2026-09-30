# Wire cursor

## What it is

Reading and writing the fields of one SFTP packet body — `u32`, `u64`, SSH `string`, the two text
fields — and wrapping a body in the outer frame `u32 length | u8 type | body`. Hand-rolled, with no
`serde`.

## Governing decisions

**None.**

## Design model

- **No `serde`, and that is the architecture.** See
  [paths are bytes](../invariant/paths-are-bytes.md) for the chain that makes a `serde` field a
  `String`.
- **Every read goes through `take`, and the bounds check is why.** Returning `Truncated` with both
  numbers is what makes a fixture that is one byte short say so
  ([the far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)).
- **`Reader` borrows rather than owning**, so decoding a `NAME` response with many entries copies
  each filename exactly once — into the `Vec<u8>` the caller keeps.
- **`string` hands back bytes; `text` is lossy, and the asymmetry is deliberate.** `text` exists for
  the two fields the spec defines as text and nothing else may call it. `russh-sftp` has the same
  byte operation (`try_get_bytes`) and does not use it for filenames; its maintainer rejected
  `Vec<u8>` on upstream #42 because *"the packet defines the number of characters, not bytes"*, which
  that function in their own crate contradicts (R37, #42 in [reference](../reference.md#russh-sftp)).
- **The frame length counts the type byte.** The draft's § 3 makes the payload `byte[length - 1]`
  with the type inside it, and both reference implementations agree (R39, R40 in
  [reference](../reference.md)). Off by this one byte and every packet after the first is
  misframed, which reads as a corrupt stream rather than as an arithmetic slip.
- **Integers are big-endian**, and byte order is invisible in a round trip that uses the same
  function both ways, so the tests pin the bytes.

## Code

- `src/wire.rs` — `Reader` (`take`, `u32`, `u64`, `string`, `text`, `rest`), `Writer` (`string`,
  `raw`), `frame`

## Reference behaviour

- `russh-sftp` corrupts a non-UTF-8 string silently (`from_utf8_lossy`); `openssh-sftp-client`
  rejects the whole response (`ssh_format`'s strict string decode). Neither can hand back the
  server's bytes (R35, R36 in [reference](../reference.md)).

## Cross-cutting invariants

- [Paths are bytes](../invariant/paths-are-bytes.md)
- [The far end sizes nothing](../invariant/the-far-end-sizes-nothing.md)

## Blast radius

- [Packets](packets.md) — every request and response is built on this cursor.
- [File attributes](file-attributes.md) — decodes and encodes through it.

## Known holes / open

**None.**
