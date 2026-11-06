# A stopped result says so

## The fact

Every operation a caller can stop part-way returns a flag that says whether it ran to the end —
`Listing::stopped`, `Download::stopped`, `Upload::stopped` — and the answers that stop it are an
enum, never a `bool` or an `Option`.

## Why it is cross-cutting

The loops build their results separately and share no loop. A caller treats them the same way, so one that stopped reporting would read as complete while the others did not.

- **Without the flag a caller cannot tell "this folder holds twenty things" from "I stopped after
  twenty"**, and drawing the second as the first is a listing that lies about the server.
- **`Walk` is an enum because both answers are ordinary.** A walk that runs to the end and a walk
  the user stopped are equally correct outcomes, and `false` at a call site reads as a failure.
- **`Feed` has three answers** because "no more bytes" and "stop, the user cancelled" are different
  outcomes, and a caller must not have to encode one as the other.
- **Stopping stops asking.** A loop that ran to the end and merely *reported* that it stopped would
  buy the user nothing — the round trips, or the 4 GB, still cross the link.

## Territories it holds in

- [Listing](../territory/listing.md) — `Walk`, `Listing`.
- [File transfer](../territory/file-transfer.md) — `Download`, `Feed`, `Upload`.
  `read_file_ahead_watched` reports through `Download` too. A stop there sends no further request and
  waits for what was already sent, discarding it, so a slow link does not turn it into `Timeout`.

## What a violation looks like

- **A short listing** draws as a folder with fewer rows.
- **A short file does more damage.** It opens as a config that stops in the middle of a line, and
  nothing on screen says so.
- **A short upload cannot be taken back.** A short local copy can be deleted; bytes a server has
  already accepted cannot be unwritten, so a stopped upload leaves a partial file on somebody else's
  machine. A caller that resumes writes into exactly that, which is why the count is reported rather
  than rounded away.

## Discovery history

- Carried from PenTerm's comments (commit `88f4108`, 2026-09-30). `tests/walk.rs`, `download.rs` and
  `upload.rs` each pin a stop that keeps what arrived and says so.

## Where it will recur

Any new operation a caller can end early: if it returns partial work, it returns the flag too.
