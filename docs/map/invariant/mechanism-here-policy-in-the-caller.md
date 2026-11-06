# Mechanism here, policy in the caller

## The fact

The crate owns what is only correct with the whole protocol state in hand — the loop that holds a
handle, the offset arithmetic, the refusal of a packet a strict server would reject. Everything that
is a choice — how big a chunk is, how many requests are in flight, when to stop, how often to report,
what a byte name looks like on screen, whether a server's sentence may be shown — is passed in by the
caller or handed back to it. Checkable: `src/` holds no default chunk length, no retry and no filter
over what a server sent.

## Why it is cross-cutting

The sites below share no code: `read_file_watched`'s `chunk_len`, `Config::max_outbound_packet`'s
refusal and `Status::message`'s verbatim hand-over are separate decisions kept the same way. A default
added at one site gives callers two answers to the same question, because a caller reads the others as
leaving it the choice.

- **A copy of an invariant is how two copies stop agreeing.** A caller that drove
  `open_dir`/`read_dir` or `open_file`/`read` itself in order to insert a cancel check would have to
  reproduce the close discipline ([every handle is closed](every-handle-is-closed.md)). So the loop
  stays here and the *decision* is injected: `Walk` for a walk and a download, `Feed` for an upload.
- **Refusing and chunking are different halves.** Refusing to emit an oversized packet belongs here,
  because a codec that silently emits something a strict server will reject is wrong at any policy.
  Chunking a large transfer into packets that fit does not: how to split, how many to keep in flight,
  and whether to re-issue a short read are transfer *policy*.
- **A limit invented here would be a policy for a case nobody has measured.** A zero-length `DATA`
  reply and a caller answering `Feed::Bytes` with an empty vector forever are both left unguarded
  for that reason ([file transfer](../territory/file-transfer.md)).

## Territories it holds in

- [Listing](../territory/listing.md) — `list_dir_watched` owns the walk; the caller answers `Walk`.
  `.` and `..` are not filtered: they are real entries the server sent, and whether to draw them is
  the caller's.
- [File transfer](../territory/file-transfer.md) — `read_file_watched` sends `chunk_len` as each
  `READ`'s length and does not reach for `max_read_len` on its own; `write_file_watched` passes
  `chunk_len` to `next` and writes what comes back unsplit, so the caller reads exactly what fits
  from its source instead of the crate re-splitting it.
- [Transfer lengths](../territory/transfer-lengths.md) — `Config::max_outbound_packet` refuses;
  `max_read_len` and `WriteFile::max_chunk` report what fits so a caller can split.
- [Request pairing](../territory/request-pairing.md) — `enqueue` returns the reply receiver without
  awaiting, the seam a pipelining policy would use; it stays private until one is measured.
- [Errors](../territory/errors.md) — `Status.message` is handed over verbatim; whether it may be
  shown, and after what sanitising, is the caller's.
- [Paths are bytes](paths-are-bytes.md) — drawing a name is the caller's.

## What a violation looks like

A caller's choice stops working with no error: a chunk size that is silently re-split, a stop that is
overridden, a server's message or a name changed before the caller sees it. PenTerm holds the other
side of this line — its own read retry, chunk sizing and message sanitising
(`sftp-transfer`, PenTerm `2aec7cd40`, 2026-11-06) — and would keep doing so on top of a second answer
here.

## Discovery history

- The refuse-versus-chunk split answers one of the eight upstream traps
  ([reference](../reference.md#the-eight-upstream-traps)). The rest was carried from PenTerm's comments
  (commit `88f4108`, 2026-09-30).

## Where it will recur

Any new constant that decides something a caller might decide differently. A decision about depth is
the next one: [#1](https://github.com/kihyun1998/justsftp/issues/1) asks who chooses it.
