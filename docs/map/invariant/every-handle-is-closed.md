# Every handle is closed

## The rule

Every loop this crate runs over a handle closes it whichever way the loop ends — at `EOF`, on a
failure, **and on a stop**. Where the caller drives the handle through `ReadFile` or `WriteFile`,
the close is a request the caller must make, and forgetting it fails a debug build. A raw `Handle`
from `open_dir`, `open_file` or `open_file_with` is the caller's to close with `close_handle`.

## Why

- **A server has a finite number of open handles**, and leaking one per listing or transfer
  exhausts them. A user who stops several slow listings or cancels several large downloads would
  strand one each time — invisible from the client's side, and it surfaces on the server as a limit
  nobody can attribute.
- **Rust has no async drop.** `close_handle` is `async`, so `Drop` cannot do the work. What `Drop`
  can do is refuse to be quiet: `ReadFile` and `WriteFile` fire a `debug_assert!`, so forgetting is
  a failing test rather than a server that stops opening files after a few hundred transfers. In
  release it is a leak, which is what it would have been anyway.
- **A stop is answered only when a batch or chunk lands**, so it waits up to one round trip;
  ending the loop sooner would mean dropping the future, which skips the close. Once answered, no
  further request goes out.

## Sites

- [Listing](../territory/listing.md) — `list_dir_watched` closes after the walk, then reports the
  walk's error, then the close's.
- [File transfer](../territory/file-transfer.md) — `read_file_watched` and `write_file_watched` are
  built on `ReadFile` / `WriteFile`, so there is **one** place each kind of handle is closed.
- [Session lifetime](../territory/session-lifetime.md) — `Session::close` drains the writer's queue
  rather than discarding it; the packet most likely to be sitting there is the `SSH_FXP_CLOSE` a
  listing queued.
- [Verification](../territory/verification.md) — the fake servers keep serving after `EOF`, because
  a stopped walk or read sends `CLOSE` without asking for the last batch.
