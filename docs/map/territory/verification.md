# Verification

## What it is

How the crate is tested: unit tests beside the code in `src/`, integration tests in `tests/` that
drive a real `Session` against a fake server over an in-memory pipe, and the CI gates in
`.github/workflows/test.yml`.

## Governing decisions

**None.**

## Design model

- **No SSH anywhere.** The transport is `tokio::io::duplex` — two ends of an in-memory pipe. No SSH,
  no server, no channel, no network, no `russh`. That separability is the design, not a testing
  convenience ([package and release](package-and-release.md)).
- **The unit tests in `src/`, `concurrency.rs` and `round_trip.rs` name the mutation that reddens
  them** in a comment; the control-flow files (`walk.rs`, `download.rs`, `upload.rs`, `limits.rs`) do
  not. A green test proves nothing until it has been seen red; a named mutation is how the next
  reader re-checks it.
- **A decoder is graded against hand-written bytes, never against its own encoder.** A decoder
  checked against its own encoder agrees with itself and proves nothing. So `tests/round_trip.rs`
  writes its packets out by hand, and its `read_frame` is a second, tiny implementation rather than
  the crate's. Three other fixtures were rejected for that file: `russh-sftp`'s server half (its
  `File.filename` is a `String` and the same struct serves both halves, so it **cannot emit** a
  non-UTF-8 name), a packet built with this crate's encoder (self-drawn), and a string literal (a
  `String` cannot hold the bytes). `the_fixtures_are_internally_consistent` checks each fixture's
  declared length against the array, so a hand-counting slip surfaces in the test file rather than as
  a decoder bug.
- **Where control flow is under test, builders are fine.** `tests/walk.rs`, `download.rs`,
  `upload.rs` and `limits.rs` build their replies with helpers, because what is graded is how many
  round trips happen, what the callback is told, and whether `CLOSE` goes out — logic the builders
  share none of. `concurrency.rs`'s `attrs_reply` is the same case: it shares none of the pairing
  logic it exists to test.
- **Adjacent same-typed fields must carry different values in a fixture.** `round_trip.rs`'s `NAME`
  entries once repeated the same eight bytes in `filename` and `longname`, and a swap of those two
  decoder lines left the entire suite green — while on a real server `entry.filename` would become an
  `ls -l` line and opening by it would address nothing. The `rename` test uses `a` and `bb` for the
  same reason, and `atime_is_read_before_mtime` uses two distinguishable values.
- **The `NAME` fixture has two entries, and the first carries an `ATTR_EXTENDED` tail.** A decoder
  that skips the tail misreads entry two; with one entry the defect is invisible.
- **The fixture handle is not valid UTF-8 and contains a NUL**, as a server's counters and pointers
  would.
- **A byte-exact assertion is paired with a one-byte corruption**
  (`corrupting_one_byte_of_the_fixture_changes_exactly_that_byte`). Alone, "corrupted input gives a
  different answer" is weak — a decoder returning a constant would fail it too; paired, it shows the
  assertion reads the fixture rather than something it reconstructed.
- **The request that went out is read back.** `open_file` could return a handle while having sent a
  mangled path, because a byte-array server does not check its input; so the `OPEN` request's bytes
  are asserted too.
- **The fake servers keep serving after `EOF`.** A stopped walk, read or upload sends `CLOSE` without
  asking for the last batch, and a server that exited would make those cases hang instead of failing.
- **`byte_at(n)` is position-dependent**, so a chunk delivered at the wrong offset — or twice — is
  visible in the assembled file rather than plausible.
- **Tests that need a window choose it to be observable** (#95's runs on virtual time):
  - upstream #95: the pipe holds 8 bytes so a request cannot be written in one go, the server waits
    5 virtual seconds before draining, and the budget is 3 seconds. Asserting that a timeout
    eventually happens would not observe *when* the clock starts; this requires success.
  - out-of-order replies: both requests are taken off the wire before either is answered, and they
    are answered in reverse, because replying in order would let a first-in-first-out implementation
    pass.
  - cancel safety: the path is 2,000 bytes. Through an 8-byte pipe a ~50-byte packet finished its
    write before the drop was scheduled, so a mutation that abandons the write mid-packet came back
    green; at 2,000 bytes it reddens. `biased` polls the request first, so the drop lands after the
    enqueue and before the write completes.
- **A half-close needs two independent pipes, not one duplex split in two.** `tokio::io::split`
  hands back halves that share one `DuplexStream` behind a lock, so dropping one signals nothing to
  the peer — the stream closes only when both are gone. Measured: the half-close test failed with
  `Timeout` twice while the fix under test was working, once because the wrong half was dropped and
  once because dropping either changes nothing. Two pipes joined with `tokio::io::join` give
  independent lifetimes. The write-failure test needs the same shape for the opposite half.
- **What the write-failure test cannot see.** It cannot tell the shared cause from a synthesised one
  — both are `Io(BrokenPipe)` and a duplex's error carries no `raw_os_error`. It pins what is
  observable: the queued caller learns a real cause rather than `Eof`.
- **The inbound ceiling is tested on the reader's path.** An earlier version tested it only through
  `Session::open`, which calls `read_packet` directly; the reader task gets its own copy of the
  limit, so raising *that* one to `usize::MAX` left the suite green. The test declares 1 MiB against
  a 4 KiB ceiling rather than 4 GiB, so the mutation that removes the check is safe to run. It sends
  **two** requests, because with one waiter "the cause reaches the caller" is satisfied by handing it
  to an arbitrary one.
- **The outbound refusal test exists because mutation found the guard unobserved.** Replacing the
  guard with `if false` left the whole suite green. Its timeout is short so that removing the guard
  fails fast as `Timeout`.
- **The three `max_chunk` tests measure different things.** The first two ask `max_chunk()` for a
  number and check `write()` against it, so they pin agreement between the two; a ceiling one byte
  low leaves both green (measured). The third checks the number against the encoder's arithmetic and
  pins the value; mutating the ceiling by one in either direction reddens it and nothing else.
- **A test that forgets `close` fails**, because `ReadFile`/`WriteFile` assert on drop — which caught
  `the_ceiling_leaves_room_for_this_file_s_handle` forgetting.
- **CI** runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and
  `cargo doc` with `-D warnings` (links must resolve on the public page), all `--locked`, on the
  toolchain `rust-toolchain.toml` pins.

## Code

- `tests/round_trip.rs`, `tests/concurrency.rs`, `tests/walk.rs`, `tests/download.rs`,
  `tests/upload.rs`, `tests/limits.rs`
- the `#[cfg(test)] mod tests` in each `src/*.rs`
- `.github/workflows/test.yml`, `rust-toolchain.toml`

## Reference behaviour

**None.**

## Cross-cutting invariants

- [Paths are bytes](../invariant/paths-are-bytes.md)
- [Every handle is closed](../invariant/every-handle-is-closed.md)

## Blast radius

- Every territory: each names the tests that hold it.

## Known holes / open

- **No test runs against a real server.** Every server here is a fixture.
