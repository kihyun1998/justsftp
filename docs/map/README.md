# Map

The territory map. A comment's why, trap and measured value go to a note here.
Notes are added as they come up, or all at once with `grill-map`, which also
brings hand-written notes into its format.

## Territory

- [Paths are bytes](territory/paths-are-bytes.md) — why every addressing field is `Vec<u8>`
- [Dependency fence](territory/dependency-fence.md) — no SSH, no encoding crate, no `serde`
- [Scope](territory/scope.md) — the verb set, v3 only, mechanism here and policy in the caller
- [Upstream traps](territory/upstream-traps.md) — the `russh-sftp` defects this was written against

## PenTerm provenance

The crate was written inside PenTerm as `crates/penterm-sftp` and moved here with its tests. Its
reasoning lived in PenTerm's ADR-0089 and ticket notes; what the code still needs from them is in the
notes above. PenTerm is not public, so it is cited as text. "(PenTerm)" after a number means it was
measured there and not re-measured here.
