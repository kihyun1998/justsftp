# MAP — justsftp

<!-- grill-map build stamp: a89807d -->
<!-- grill-map synced at: c713019 (2026-11-06) -->

The layer that answers the two questions no other artifact here can, because every other artifact is
indexed by an event — a commit, an issue — and none is indexed by what the crate does.

| Question | Open |
|---|---|
| **If I touch this, what else moves?** | the territory note for what you are touching, then its `## Blast radius` — read it as a **checklist**. Opening a listed territory and finding nothing to do is a correct outcome; not opening it is the failure this layer exists to prevent. Then its `## Cross-cutting invariants`, the same way |
| **What is this derived from?** | the same note's `## Governing decisions` (who decided) and `## Reference behaviour` (what it was checked against), and the rows it names in [reference](reference.md). A `**None.**` there is the answer, not an omission |

Written in English, as the README and `CLAUDE.md` are: an agent reads this at the start of a task.

## The crate in one paragraph

justsftp is an SFTP v3 client over any async byte stream. It opens a session (handshake, version gate,
limits query), pairs each request with its reply through a reader task and a writer task on two
clocks, encodes packets with a hand-rolled cursor, keeps attributes as a read-only type and an update
type, and offers verbs: listing, whole-file transfer (pull, push, watched loops, resume, several
requests outstanding) and path verbs. The caller sizes chunks within lengths the crate reports and
chooses any depth. It ships to crates.io from a tag, and
its known consumer is PenTerm. **The largest gap is that nothing here has met a real server**: every
server in the repository is a fixture ([verification](territory/verification.md)).

## Why this layer exists

The crate was written inside PenTerm and moved here in one commit. Its reasoning lived in 1,400 lines
of comments that cited PenTerm's tickets and records, and in upstream file-and-line citations that
nobody had re-read since they were written. Moving the reasoning here found what the comments could
not show about themselves: one citation stated the opposite of what the upstream source says (#89,
[reference](reference.md#russh-sftp)), three pointed at the wrong lines, two doc comments were
attached to the wrong function, and three comments pointed at notes or checks that did not exist.
(Commits `c8f2453` and `88f4108`, 2026-09-30.)

## PenTerm provenance

- **The design came from PenTerm's ADR-0089** ("PenTerm owns its SFTP implementation") and its ticket
  notes. `gh repo view kihyun1998/penterm` does not resolve (2026-11-06), so it is cited as text and
  cannot be a link. What the code still needs from those records is in the notes here.
- **The consumer's side was read at PenTerm `2aec7cd40`, 2026-11-06**, from the maintainer's local
  checkout: `src-tauri/Cargo.toml`, its uses of this crate, ADR-0089's amendment and its
  `sftp-transfer` note. A citation with that commit is that reading.
- **"(PenTerm)" after a number** means it was measured in PenTerm's app or suite and has not been
  re-measured here. It is evidence for the rule, not a property of this crate's tests.
- **PenTerm keeps its consumer side** — how it opens the channel, how it sanitises a server's STATUS
  message, how it draws a byte filename — in its own `sftp-transfer` note.

## Measured

Commands, not numbers: a number stored here competes with the tree that produces it.

```sh
grep -c '^pub use' src/lib.rs                       # the public surface, by re-export line
ls docs/adr 2>/dev/null | wc -l                     # records governing it (none: no docs/adr/)
wc -l src/*.rs | sort -rn                           # where the code's mass sits
grep -rn 'docs/map/' src tests                      # source comments citing a note by path
cargo test 2>&1 | grep '^test result'               # the suite, per target
```

## How this is read and written

- **Read** — before designing a change: this hub, then the territory for what you are touching, then
  its blast radius and invariants as a checklist.
- **Write** — `CLAUDE.md` § Comments sends a comment's why, trap and measured value here; a change
  that alters a rule updates the note in the same change.
- **Promotion** — at the first fix, ask whether the fact holds at another site that shares the same
  assumption. Here the shared assumptions are few and nameable: *does this code read a length or a
  count from the wire? does it hold a server handle? does it decide something a caller should?* If
  yes, the fact goes in an invariant note before the fix lands.
- **Upstream facts go in [reference](reference.md)**, one row each, pinned to a version; a note or a
  comment names the row id rather than a file and line.

## Conventions

- **Empty sections stay**, and `**None.**` is the sentinel. It marks different holes — nobody
  decided, nobody checked against a reference, nothing is open — so every query for it names its
  heading:

  ```sh
  grep -lzP '## Governing decisions\r?\n\r?\n\*\*None\.\*\*' docs/map/territory/*.md
  grep -lzP '## Reference behaviour\r?\n\r?\n\*\*None\.\*\*' docs/map/territory/*.md
  ls docs/map/territory/ docs/map/invariant/        # what exists; the folder is the roster
  ```

- **Territories overlap.** A fact that holds in several is an invariant note, not a copy.
- **Symbols, never line numbers** — except in [reference](reference.md), where each line number is
  pinned to a named version.
- **Plain relative markdown links.**
- **Published docs carry no map paths.** A public item's pointer to its note is a plain `//` line
  ([package](territory/package-and-release.md)).
- **Provenance.** A claim names its evidence: a symbol, a test, a reference row, a commit, or a source
  with its date. *"An earlier draft"* or *"an earlier version"* in a note is history carried from
  PenTerm's comments (commit `88f4108`, 2026-09-30); it cannot be checked here, unlike the rule it
  explains, which was checked against the code at this file's synced-at commit. Reference rows were
  not re-fetched for that check.

## What this map cannot answer

- **Consumer behaviour.** What a caller does with a byte name, a status message or a stopped result
  is the caller's; a note here says where the crate stops.
- **Real servers.** Every server in this repository is a fixture
  ([verification](territory/verification.md)).
- **Which file or issue depends on what.** Source files and issues are text inside notes, not nodes.

## Scope

**Complete for `src/`, `tests/`, and the package and CI surface.** Every file is named under some
note's `## Code`. `docs/` is this map and the agent setup in `docs/agents/`, neither of which the crate
ships or reads. Open work is in the notes it would bite, as `Tracked:` lines.

The territories:
[wire cursor](territory/wire-cursor.md) · [packets](territory/packets.md) ·
[file attributes](territory/file-attributes.md) · [errors](territory/errors.md) ·
[request pairing](territory/request-pairing.md) · [session lifetime](territory/session-lifetime.md) ·
[transfer lengths](territory/transfer-lengths.md) · [listing](territory/listing.md) ·
[file transfer](territory/file-transfer.md) · [path verbs](territory/path-verbs.md) ·
[verification](territory/verification.md) · [package](territory/package-and-release.md) ·
[release](territory/release.md).

The invariants: [paths are bytes](invariant/paths-are-bytes.md) ·
[the far end sizes nothing](invariant/the-far-end-sizes-nothing.md) ·
[every handle is closed](invariant/every-handle-is-closed.md) ·
[a stopped result says so](invariant/a-stopped-result-says-so.md) ·
[mechanism here, policy in the caller](invariant/mechanism-here-policy-in-the-caller.md).

**What an absent note means.** A new file under `src/` either belongs to a territory above — add it
to that note's `## Code` — or it is a new thing the crate does, and then a note is owed in the same
change, written from the code as it then stands.
