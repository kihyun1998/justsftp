# Dependency fence

## What it is

The crate depends on `tokio` and nothing else (`Cargo.toml`). Three absences are deliberate, and
each is held only by being absent, so it comes off without an error.

## Governing decisions

**None** in this repository.

## Design model

- **No SSH dependency, not even under `[dev-dependencies]`.** The crate is driven over any
  `AsyncRead + AsyncWrite + Unpin + Send`; `russh::Channel::into_stream()` happens to yield one,
  and that coincidence is the whole relationship. A dependency added "just for a test" is how it
  stops being true. Tests run over `tokio::io::duplex`, an in-memory pipe.
- **No encoding crate.** Turning a filename's bytes into something to draw is a decoding decision,
  and it belongs with the caller's other decoding decisions — for a terminal client, the session's
  encoding label. A crate that decoded for display would put one fact in two places.
- **No `serde`.** Encoding with `serde` is what forces a text field to be `String`
  ([paths are bytes](paths-are-bytes.md)). A second effect, which is the consumer's to rely on or
  not: with no `serde`, nothing here can `#[derive(Serialize)]`, so `Status.message` — prose a
  remote server wrote, escape sequences included — cannot reach a serialized payload through this
  crate's types. A consumer that must keep server prose out of its UI holds that with its own
  check; PenTerm's is in its `ssh/sftp.rs`.

## Blast radius

- [Paths are bytes](paths-are-bytes.md)
