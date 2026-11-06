# File transfer

## What it is

Moving a file's bytes. Reading: `Session::read` at an offset, `ReadFile` pulled by the caller,
`read_file_watched` owning the loop. Writing: `Session::write` at an offset, `WriteFile` pushed into
by the caller, `write_file_watched` owning the loop and asking the caller for each chunk through
`Feed`. `ReadAhead` and `WriteBehind` do the same with several requests outstanding. The results say
how much moved and whether it was the whole file.

## Governing decisions

No record in this repository. The maintainer's decision on pipelining and its reasons are on
[#1](https://github.com/kihyun1998/justsftp/issues/1#issuecomment-6677251816), 2026-11-06, and are
marked below.

## Design model

- **A short read is normal and is not the end of the file.** The draft says so for device files in
  § 6.4 — *"this may return fewer bytes than requested"* — so a loop that treats
  `data.len() < len` as `EOF` silently truncates the download: the file arrives, opens, and is wrong.
  Only `Ok(None)` is `EOF`, and the offset advances by what arrived, never by what was asked for — a
  loop that advanced by what it asked for would re-read and skip at the same time, and the assembled
  file would be silently corrupt rather than short.
- **There is no short write, and that is the asymmetry with reading.** `SSH_FXP_WRITE` answers
  `SSH_FXP_STATUS` — it wrote everything or it failed. So the offset advances by exactly what was
  handed over, and there is no partial-progress case to get wrong.
- **The watched loops live here and take `chunk_len` from the caller**
  ([mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)).
  They are built on `ReadFile`, `WriteFile` and `ReadAhead`, so there is one place each kind of handle is closed and
  one place the offset advances ([every handle is closed](../invariant/every-handle-is-closed.md)).
- **One callback rather than two**, for the reason [listing](listing.md) gives, and the total it
  reports is cumulative.
- **No `Vec<u8>` is returned.** A 4 GB download that had to be assembled in memory before anyone
  could write it would be a size limit disguised as a return type; the bytes go out through the
  callback one chunk at a time, and where they land is the caller's.
- **No `stat` first.** A denominator for a progress bar is the caller's question and costs a round
  trip. Unlike a listing, a file *can* have one — `SSH_FXP_STAT` answers a file's size.
- **A zero-length `DATA` reply is not treated as `EOF`, and is not guarded against either.** The
  draft gives it no meaning, so inventing one would be inventing a workaround for a server defect
  nobody has measured. The failure it would produce is a total that stops moving while the caller's
  stop still works on the next chunk — visible and reversible, which is the bar for not writing a
  guard. **The same call is made on the write side**: a caller that answers `Feed::Bytes` with an
  empty vector forever will loop; the loop is bounded by round trips rather than CPU, and the caller
  controls it.
- **`ReadFile` exists for remote to remote.** `read_file_watched` owns its loop, which is right when
  the destination is passive — a local file, a progress bar. It cannot serve a remote-to-remote copy,
  where the far side's `write_file_watched` wants to own a loop too and neither can yield. Pulling is
  the shape that lets one drive the other. Prefer the watched loop wherever the destination is
  passive.
- **`WriteFile` exists for an asynchronous source.** `write_file_watched` pulls through a
  **synchronous** `Feed`, which is enough when the source is a local file and not when the source must
  `await` — a remote-to-remote copy awaits the far side's read inside the loop. Pushing is the shape
  that lets an async source drive an async sink.
- **With `ReadFile` and `WriteFile` the close is a request, not a structure**
  ([every handle is closed](../invariant/every-handle-is-closed.md)).
- **`write_file` creates and truncates.** Opening without `TRUNCATE` leaves the tail of a longer
  previous file behind — a corrupt destination that reports success: every write returns `Ok` and the
  file is simply too long, and nothing downstream can notice.
- **`overwrite_file` is `WRITE | TRUNCATE` without `CREATE`**, so a missing file is refused
  (`NoSuchFile`) rather than made. It is the in-place save of a file being edited — the same inode,
  so owner and permissions stay. The draft (§ 6.3) requires `CREATE` whenever `TRUNCATE` is set;
  OpenSSH's server maps each flag to its own `open(2)` flag (R45), so it works there.
- **`write_file_from` resumes, and leaves out both `TRUNCATE` and `APPEND`.** Truncating is right for
  an ordinary copy and exactly wrong for a resume, where the bytes already there are the point.
  `APPEND` is left out too: under it the server ignores each write's offset and puts the data at the
  current end, so a resume that is wrong about the offset would still land bytes somewhere and report
  success. Writing at an explicit offset means a wrong offset writes to the wrong place *visibly*.
  The caller owes the invariant this cannot check — the bytes already at the destination are a
  correct prefix of the source — and nothing on the wire can confirm it.
- **`read_file_from` starts partway in**, for resume: `SSH_FXP_READ` carries the offset, so this only
  decides where the first read points. An offset past the end is not an error here: the server answers
  the first read with `EOF`, which reads as an empty file, and whether that is sensible is the caller's
  to judge.
- **`Session::write` refuses data above the server's stated write bound**
  ([transfer lengths](transfer-lengths.md)).
- **An empty file is `OPEN` then one `READ` answering `EOF`**; the callback is never invoked, so a
  caller's "finished" signal must not depend on the first chunk.
- **`ReadFile` and `WriteFile` are the shape the known consumer drives.** PenTerm's transfer engine
  holds them across its own loop, and its drag-out reader calls `open_file` and `Session::read`
  directly (`explorer_transfer/engine.rs`, `explorer_drag_out/remote.rs`, PenTerm `2aec7cd40`,
  2026-11-06), so their signatures carry a caller who is not in this repository.
- **A transfer's rate is bound by the round trip, and depth lifts it until the channel window stops
  it.** Measured once, in MB/s, on the path and conditions in
  [#1's result](https://github.com/kihyun1998/justsftp/issues/1#issuecomment-6677251816): at RTT 30 ms
  one request at a time moves 7.7 down and 7.6 up, OpenSSH's `sftp` 37.0 and 38.3, and concurrent
  `Session::read` and `Session::write` reach 33.4 and 58.4. No short read occurred, so that path was
  not exercised. See [transfer lengths](transfer-lengths.md) for what sets the ceiling.
- **Pipelining goes in new public types beside `ReadFile` and `WriteFile`** (maintainer, 2026-11-06).
  The crate owns offsets, reassembly and failure position; the caller passes the depth. Decided against
  a depth on the existing types, where `write` would mean "done" or "queued" by how the file was opened
  and `next(len)` does not fit read-ahead, and against leaving it to callers, each of whom would then
  own reassembly and a contiguous acknowledged offset. Names and the read's watched loop are the
  maintainer's too (2026-11-06); `WriteBehind` has no watched loop, as nothing known would call it.
- **Waiting on the front of the queue is the design.** Replies that land early stay in their channels,
  so there is no reassembly buffer. Memory is `depth` chunks from a server that answers what was asked
  and `depth × max_inbound_packet` from one that does not, since a reply's length is judged at the
  front. It settles the rest: bytes leave in
  file order, `EOF` counts only when it is the front's, a failure is reported after every byte before
  it, and each clock is the time since the last reply. OpenSSH writes at each reply's offset instead
  and tracks a contiguous one (R50), which this crate's in-order callback cannot do.
- **`next` sends only when called**, and after a failure or `EOF` gives up what was due. Calling again
  asks again from `offset`, the contract `ReadFile::next` already has, so a retry needs no reopening
  unless the caller wants one. A stop therefore sends nothing further.
- **`ReadAhead::close` takes the replies still due before it sends `CLOSE`, on the front's clock.** A
  server answering in order puts `CLOSE`'s reply behind them, so sending it first costs no time and
  charges all of them to one budget: a stop on a slow link came back `Timeout`, losing
  `Download::stopped` (`a_stop_on_a_slow_link_still_says_it_stopped`). A reply that does not come in
  time ends the wait, and `CLOSE` still goes.
- **`ReadAhead` refuses a reply longer than asked** (`TooLong`), since it would overlap the next offset;
  `ReadFile` still accepts one, below. A zero-length reply is handed over as an empty chunk and asked
  again, the rule the loops above already follow; OpenSSH ends on a second one (R48).
- **No ramp.** The first `next` sends `depth` requests, where OpenSSH starts at one and adds per reply
  (R47): a small file costs extra requests and no extra round trip, and nothing has measured that as a
  problem. Nor does a short reply shrink later requests (R49): the caller fixed the chunk.
- **A failed `WriteBehind` stays failed.** Bytes past `acknowledged` were given to it and are not held,
  so accepting another write could only continue from the wrong place. It gives up the replies still
  due, which cannot move the contiguous offset, where OpenSSH takes them (R51). Every later call repeats the failure (`Error::repeated`, as `Error` cannot be
  `Clone`) and `close` still closes. A refusal for
  length sends nothing and is not a failure. It does not truncate: on a dead link it could not, and it
  would change a server's file unasked (maintainer, 2026-11-06). `depth` is `NonZeroUsize` and a read's
  `chunk_len` is `NonZeroU32`, so no check is needed for a queue that could never be sent or a read that
  could never advance.
- **Both `close`s wait before they send `CLOSE`, unlike every other close here.** Dropped while waiting,
  they send none, so `closed` is set only once the wait is over and the drop assertion still fires. A
  caller bounding one with an outer timeout leaks the handle.
- **The drop assertions stay quiet during a panic.** A second panic aborts the process, which turned
  one failing test into a run with no results.
- **Against OpenSSH 9.6 through a relay at RTT 30 ms and 1 Gbit/s**, measured 2026-11-06 on the final
  code, median MB/s of 3 runs: `ReadAhead` 7.8 at depth 1, 30.5 at 4, 33.7 at 8, 33.7 at 16, and 106.6 at
  64 with a 16 MiB window; `WriteBehind` 7.7, 30.5, 57.9, 50.7, where depth 16 ranged from 47.2 to 60.8.
  Every file matched byte for byte, including 320 real short replies from asking above what
  `sftp-server` hands out at once. Cutting the link mid-upload six times left a correct prefix up to
  `acknowledged`, with the file longer than it and, since writes reached that server in order, no hole.
- **Short replies and deep uploads cost something with no delay, by amounts these runs do not pin.** In
  two runs each: 8 requests asking above the server's length moved 104 and 269 MB/s against 402 and 591
  at the length it states, and an upload at depth 64 moved 103 and 125 MB/s against 221 and 287 at depth
  8. Why depth lowers that rate has not been looked into.

## Code

- `src/client.rs` — `Download`, `Session::read`, `Session::read_file`, `Session::read_file_from`,
  `Session::read_file_watched`, `ReadFile` (`next`, `server_read_len`, `close`, `Drop`), `WriteFile`
  (`write`, `written`, `close`, `Drop`), `Feed`, `Upload`, `Session::write_file`,
  `Session::overwrite_file`, `Session::write_file_from`, `Session::write_file_watched`,
  `Session::write`
- `src/pipeline.rs` — `ReadAhead` (`next`, `offset`, `close`), `WriteBehind` (`write`, `flush`,
  `acknowledged`, `offset`, `max_chunk`, `close`), `Session::read_file_ahead`,
  `Session::read_file_ahead_from`, `Session::read_file_ahead_watched`, `Session::write_file_behind`,
  `Session::write_file_behind_from`
- `tests/download.rs`, `tests/upload.rs`, `tests/pipelined.rs`

## Reference behaviour

- OpenSSH's `sftp_download` and `sftp_upload` keep many requests outstanding, track a contiguous offset
  apart from the highest one, re-request the rest of a short reply and end on a reply longer than asked
  (R46 to R51). `russh-sftp` pipelines writes only (R53). Each was read for
  [#1](https://github.com/kihyun1998/justsftp/issues/1). The refusal of a reply longer than asked and
  the contiguous offset follow OpenSSH; the front-of-queue wait does not, and writes are shaped like
  `russh-sftp`'s oldest-ack wait, with clocks it does not have.

## Cross-cutting invariants

- [Every handle is closed](../invariant/every-handle-is-closed.md)
- [A stopped result says so](../invariant/a-stopped-result-says-so.md)
- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)

## Blast radius

- [Transfer lengths](transfer-lengths.md) — a caller splits by `max_read_len` and `max_chunk`, and
  `write` is refused above them; a loop that stopped asking for these lengths would have its full
  chunks refused or misjudged.
- [Listing](listing.md) — the watched loops share one promise about stopping and closing; a change to
  one leaves the other promising the old one.
- [Request pairing](request-pairing.md) — `ReadFile` and `WriteFile` keep one request in flight because
  each call awaits `Session::request`; `ReadAhead` and `WriteBehind` hold `Sent`s, so that note's
  cancellation and clock rules are what keeps their queues correct.
- [Errors](errors.md) — both loops read `StatusCode::Eof` as the end, and `written` counts what
  `expect_ok` accepted.
- [Verification](verification.md) — `download.rs` and `upload.rs` servers read and answer one frame at
  a time, so they assert order a different request shape would not keep.
- [Package](package-and-release.md) — these signatures are public and PenTerm drives them from outside
  this repository's CI, and the README and crate docs state how many requests a call keeps in flight;
  a change here can be a breaking one and can falsify those sentences.

## Known holes / open

- **What a ramp would save on many small files is unmeasured**, and so is whether shrinking requests
  would recover what a server short on every request costs at a real RTT.
- **`close` can lose the resume point.** It consumes the value, and a caller that writes and then closes
  gets the failure with no `acknowledged` to read; it must `flush` first, which only a doc comment says.
- **`close` has no overall deadline.** A server answering just inside each budget can hold it for
  `depth` times the two budgets.
- **A length of 0 is still accepted on the older paths.** `ReadAhead`'s `chunk_len` is a `NonZeroU32`, but
  `read_file_watched` and `ReadFile::next` take a `u32`: answered with `EOF`, 0 reports a complete empty
  file, and answered with empty `DATA`, empty chunks forever. Changing them is a breaking change.
- **Resuming from a destination's size is not safe after a `WriteBehind` failure.** A write failing at k
  with k+1 accepted leaves a hole, and that caller reads it as a correct prefix. PenTerm resumes this way
  (`resume_verdict` in `explorer_transfer/engine.rs`, PenTerm `2aec7cd40`, 2026-11-06), so moving it to
  this type also means resuming from `acknowledged`. A cut link left none, measured above; a failure
  from the server itself, a full disk for one, has not been measured.
- **`chunk_len` is fixed at open, before a sink exists.** PenTerm's `copy_file` opens the source first
  and takes the chunk from the sink's handle (`max_chunk`), so a remote-to-remote copy through
  `ReadAhead` cannot size the chunk for its sink without opening it first, which truncates a destination
  before the source is known readable. `ReadFile` remains the shape for that case.
- **`ReadAhead` promises file order, and a device file that ignores offsets cannot keep it.** OpenSSH
  refuses a non-regular source (`sftp-client.c:1651-1655`, at 102bee7); this crate does not `stat` first.
  No such server was tried.
- **Zero-length `DATA` and an endless empty `Feed::Bytes`** are unguarded on purpose, above.
- **A `DATA` reply longer than was asked for is accepted.** `Session::read` does not compare it with
  `len`, and `ReadFile::next` advances the offset by what arrived. The inbound ceiling is the only
  bound. Found by reading `Session::read`; no server is known to do this. `ReadAhead` refuses one, and
  OpenSSH ends the transfer on it (R48).
- **`overwrite_file` against a server that enforces § 6.3** is unmeasured; such a server may refuse
  `TRUNCATE` without `CREATE`.
