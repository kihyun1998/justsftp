# justsftp

## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues for `kihyun1998/justsftp` via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Map

Before changing code, read `docs/map/MAP.md` and the territory note for what you are touching; its `## Blast radius` and `## Cross-cutting invariants` are a checklist. An upstream fact is a row in `docs/map/reference.md`, pinned to a version.

## Comments

A comment says what the code is. Why it is this way, what it deliberately leaves out, the trap and the measured value go to the territory note under `docs/map/`. Where none seems to hold it, search the folder and the code symbol first; only then start a new note, named for what the system does there, not for the file. History goes to the commit message. Comments written before this rule still carry the rest: never delete one whose content the map does not yet hold — move it first (`decant`).
