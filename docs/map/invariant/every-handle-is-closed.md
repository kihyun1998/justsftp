# Every handle is closed

## The fact

Every loop this crate runs over a handle closes it whichever way the loop ends — at `EOF`, on a
failure, **on a stop, and when its future is dropped**. `ReadFile`, `WriteFile`, `ReadAhead` and
`WriteBehind` close on `close` or, failing that, on drop, exactly once, so `close` is how a caller learns
the result rather than what prevents a leak. A raw `Handle` from `open_dir`, `open_file` or
`open_file_with` stays the caller's to close with `close_handle`: it is a public, cloneable value, and
nothing can tell which copy is the last.

**It holds while an `OPEN` or `OPENDIR` is due and nobody is left to receive its reply**: a future
dropped then, or one that gave up with `Timeout` or `WriteTimeout`. The session keeps the request's id
and queues `CLOSE` when the reply brings a handle; a reply that had already landed unread is closed from
`Sent`'s `Drop` ([request pairing](../territory/request-pairing.md)). Measured 2026-11-09 against
OpenSSH 9.6p1's `sftp-server` in WSL2 over a pipe, not through `sshd`, stating a limit of 10,235: after
12,000 such drops on one session, the next opens succeeded 0 times on `dc64f87` and 10,236 and 10,237 times, on two runs, with
this. By reading, a burst larger than the limit can still see some of its own opens refused, since each
`CLOSE` follows its reply behind the `OPEN`s already queued. `russh-sftp` leaves the handle open (R58).
It does not hold once the reader has ended while the stream stays up, as after an oversized reply:
`Outbox::send` refuses then, here as for `OpenHandle`'s `Drop`, and the handle stays until the session goes.

## Why it is cross-cutting

`list_dir_watched`, `read_file_watched`, `read_file_ahead_watched` and `write_file_watched` do not
call each other, and
`close_handle` cannot hold this: the fact is that each loop reaches it on every exit path. `OpenHandle`
holds the drop path and the once-only rule for all of them; what each loop still arranges is reaching
`close` and reporting its result. `Session::close` is the far end of the same fact, in another territory.

- **A server has a finite number of open handles**, and leaking one per listing or transfer
  exhausts them. A user who stops several slow listings or cancels several large downloads would
  strand one each time — invisible from the client's side, and it surfaces on the server as a limit
  nobody can attribute.
- **Rust has no async drop, and closing does not need one.** `Drop` cannot wait for a reply, but
  `Session::enqueue` is synchronous and the writer task sends the packet, so `Drop` queues `CLOSE` and
  the reply is discarded (maintainer, 2026-11-06,
  [#3](https://github.com/kihyun1998/justsftp/issues/3#issuecomment-6678954862); R55). A debug assertion
  used to stand in its place, which panicked when a caller's future was dropped and leaked in release.
  What this gives up: forgetting `close` is no longer loud, and a failure of that `CLOSE` goes unseen.
- **A stop is answered only when a batch or chunk lands**, so it waits up to one round trip, and on
  `ReadAhead` also for the replies already due. Ending the loop sooner by dropping the future also
  closes, but reports nothing. Once answered, no further request goes out.

## Territories it holds in

- [Listing](../territory/listing.md) — `list_dir_watched` closes after the walk, then reports the
  walk's error, then the close's.
- [File transfer](../territory/file-transfer.md) — `read_file_watched` and `write_file_watched` are
  built on `ReadFile` / `WriteFile`, and `read_file_ahead_watched` on `ReadAhead`; all of them hold an
  `OpenHandle`, the one place a handle is closed. `WriteBehind`'s `close` closes whichever way its flush
  ends.
- [Request pairing](../territory/request-pairing.md) — an open nobody is left to answer is closed from
  the reply path.
- [Session lifetime](../territory/session-lifetime.md) — `Session::close` drains the writer's queue
  rather than discarding it; the packet most likely to be sitting there is the `SSH_FXP_CLOSE` a
  listing queued.
- [Verification](../territory/verification.md) — the fake servers keep serving after `EOF`, because
  a stopped walk or read sends `CLOSE` without asking for the last batch.

## What a violation looks like

The server refuses new opens after enough transfers or stopped listings, with nothing on the client
to say why. Two `CLOSE`s for one handle would be the opposite failure, and `tests/dropping.rs` counts
them. PenTerm's walk stops by answering `Walk::Stop` rather than dropping the future, which remains the
way to get a result (`sftp-transfer`, PenTerm `2aec7cd40`, 2026-11-06); its commands' futures are also
dropped when a pane or window closes (`explorer_remote_open.rs`, same commit).

## Discovery history

- Carried from PenTerm's comments (commit `88f4108`, 2026-09-30). `the_ceiling_leaves_room_for_this_file_s_handle`
  once forgot its close and the drop assertion caught it, before that assertion went for
  [#3](https://github.com/kihyun1998/justsftp/issues/3#issuecomment-6678954862), which reproduced the leak
  in a release build.

## Where it will recur

Any new request whose reply carries a handle (it joins `OPEN` and `OPENDIR` where `Outbox::send` sets
`opening`), any new loop that holds a server handle, and any change that keeps requests outstanding when a loop
ends. A read's late reply needs no draining for the session's sake, because a reply nobody waits for
is dropped, but `CLOSE`'s reply comes after it on a server answering in order, so its clock would count
them; a write's must be taken before the close, since that is how a failure is learned
([file transfer](../territory/file-transfer.md)).
