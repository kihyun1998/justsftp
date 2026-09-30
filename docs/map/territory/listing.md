# Listing

## What it is

Reading a directory: `open_dir` and `read_dir` one batch at a time, `list_dir_watched` walking to
the end with the caller told after every batch and able to stop, `list_dir` the same walk with
nobody watching. Plus `real_path`, which canonicalises a path and is how the remote home directory
is learned.

## Governing decisions

**None.**

## Design model

- **A directory is read in batches and the count is the server's choice**, so one `read_dir` is not
  a listing. `EOF` arrives as a `STATUS`, not as an empty `NAME`, which is why `read_dir` returns
  `Option` rather than a `Vec` that happens to be empty.
- **The crate owns the walk; the caller owns the decision**
  ([mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md),
  [every handle is closed](../invariant/every-handle-is-closed.md)).
- **One callback rather than two.** Reporting progress and being able to stop have the same answer
  moment: a batch has just landed, so it is both the only new thing to report and the only place the
  walk can be interrupted. The callback gets the batch that just landed and the **running total** —
  a bar fed batch sizes would restart at every round trip.
- **There is no total.** SFTP has no verb that answers how many entries a directory holds;
  `READDIR` returns batches until `EOF`. A caller wanting *"340 of 1,284"* cannot have it, and a
  progress bar with a denominator would have to invent one.
- **A stop is answered only when a batch lands**, so it waits up to one round trip on a slow link;
  answering `Walk::Stop` sends no further `READDIR`
  ([every handle is closed](../invariant/every-handle-is-closed.md)).
- **`list_dir` is `list_dir_watched` with a callback that always continues**, so the two cannot
  drift: one close discipline, one batching loop.
- **`.` and `..` are not filtered.** They are real entries the server sent; this crate addresses
  files, it does not present them.
- **`real_path` returns bytes, not a `DirEntry`.** Its `NAME` reply carries one entry whose
  attributes are a dummy — the draft says so in § 6.11, and `russh-sftp` has a
  `FileAttributes::dummy()` constructor for exactly this (E3). Only the filename is meaningful. v3
  servers resolve `.` to the home directory. `read_link` has the same shape.

## Code

- `src/client.rs` — `Walk`, `Listing`, `Session::real_path`, `Session::open_dir`,
  `Session::read_dir`, `Session::list_dir`, `Session::list_dir_watched`
- `tests/walk.rs`

## Reference behaviour

**None.** No other client's walk was read.

## Cross-cutting invariants

- [Every handle is closed](../invariant/every-handle-is-closed.md)
- [A stopped result says so](../invariant/a-stopped-result-says-so.md)
- [Mechanism here, policy in the caller](../invariant/mechanism-here-policy-in-the-caller.md)
- [Paths are bytes](../invariant/paths-are-bytes.md)

## Blast radius

- [File transfer](file-transfer.md) — the download loop mirrors this one.
- [Path verbs](path-verbs.md) — `close_handle` ends every walk.

## Known holes / open

**None.**
