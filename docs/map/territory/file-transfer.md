# File transfer

## What it is

Moving a file's bytes. Reading: `Session::read` at an offset, `ReadFile` pulled by the caller,
`read_file_watched` owning the loop. Writing: `Session::write` at an offset, `WriteFile` pushed into
by the caller, `write_file_watched` owning the loop and asking the caller for each chunk through
`Feed`. The results say how much moved and whether it was the whole file.

## Governing decisions

**None.**

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
  They are built on `ReadFile` / `WriteFile`, so there is one place each kind of handle is closed and
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

## Code

- `src/client.rs` — `Download`, `Session::read`, `Session::read_file`, `Session::read_file_from`,
  `Session::read_file_watched`, `ReadFile` (`next`, `server_read_len`, `close`, `Drop`), `WriteFile`
  (`write`, `written`, `close`, `Drop`), `Feed`, `Upload`, `Session::write_file`,
  `Session::overwrite_file`, `Session::write_file_from`, `Session::write_file_watched`,
  `Session::write`
- `tests/download.rs`, `tests/upload.rs`

## Reference behaviour

**None.** No other client's transfer loop was read.

## Cross-cutting invariants

- [Every handle is closed](../invariant/every-handle-is-closed.md)
- [A stopped result says so](../invariant/a-stopped-result-says-so.md)
- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)

## Blast radius

- [Transfer lengths](transfer-lengths.md) — `max_chunk`, `max_read_len`, the server's write bound.
- [Listing](listing.md) — the watched loops share one shape.

## Known holes / open

- **Zero-length `DATA` and an endless empty `Feed::Bytes`** are unguarded on purpose, above.
- **`overwrite_file` against a server that enforces § 6.3** is unmeasured; such a server may refuse
  `TRUNCATE` without `CREATE`.
