# Release

## What it is

How a version gets from this repository to crates.io: the `vX.Y.Z` tag, the `publish` workflow,
crates.io Trusted Publishing, and the `CHANGELOG.md` entry written before it. What the published crate
holds is [package](package-and-release.md).

## Governing decisions

**None** in this repository. Publishing it at all was the maintainer's call when it moved out of
PenTerm (PenTerm ADR-0089, *Amendment — the crate is published as `justsftp`*, at PenTerm `2aec7cd40`,
2026-11-06).

## Design model

- **A version is published by pushing its `vX.Y.Z` tag**, with its `CHANGELOG.md` entry written
  and the version bumped in `Cargo.toml` first. `.github/workflows/publish.yml` refuses a tag that
  does not match the crate version, runs the tests, and publishes. A crates.io version cannot be
  re-published, only yanked.
- **The tag's job runs `cargo test --locked` and nothing else from the CI gate.** Format, clippy and
  the doc build run on pull requests and pushes to `main` ([verification](verification.md)), so a tag
  on a commit that never passed them still publishes.
- **The workflow triggers on any tag matching `v*`**, and the version check is what stops a tag that
  is not a release.
- **Publishing uses crates.io Trusted Publishing, so no token is stored.** The job's GitHub OIDC
  token is traded for a crates.io token that `rust-lang/crates-io-auth-action` revokes when the job
  ends. crates.io accepts a trusted publisher only for a crate that already exists, so the first
  version is published by hand with a token scoped to this crate, deleted afterwards. The trusted
  publisher on crates.io names this repository and the workflow file `publish.yml`; renaming the
  file breaks publishing until that entry is updated. That configuration lives on crates.io and was
  not read in this repository.
- **Third-party actions are pinned by commit**, with the version in a trailing comment.

## Code

- `.github/workflows/publish.yml`
- `CHANGELOG.md`

## Reference behaviour

**None.**

## Cross-cutting invariants

**None.**

## Blast radius

- [Package](package-and-release.md) — the job reads the version from `cargo metadata`, and what it
  uploads is what `include` lets through.
- [Verification](verification.md) — `cargo test --locked` is the only gate on this path; a check moved
  out of the suite is no longer checked before publishing.

## Known holes / open

**None.**
