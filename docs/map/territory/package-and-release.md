# Package and release

## What it is

What ships and how: `Cargo.toml` and its dependency fence, the README and the crate-level docs that
docs.rs renders, the licence files, the pinned toolchain, and publishing to crates.io.

## Governing decisions

**None** in this repository. Publishing it at all, under `MIT OR Apache-2.0`, was the maintainer's
call when it moved out of PenTerm (PenTerm ADR-0089, *Amendment — the crate is published as
`justsftp`*).

## Design model

- **`tokio` is the only dependency, and three absences are deliberate.** Each is held only by being
  absent, so it comes off without an error.
  - **No SSH dependency, not even under `[dev-dependencies]`.** The crate is driven over any
    `AsyncRead + AsyncWrite + Unpin + Send + 'static`; `russh::Channel::into_stream()` happens to
    yield one,
    and that coincidence is the whole relationship. A dependency added "just for a test" is how it
    stops being true.
  - **No encoding crate** ([paths are bytes](../invariant/paths-are-bytes.md)).
  - **No `serde`** — encoding with it is what forces a text field to be `String`. As a side effect
    nothing here can `#[derive(Serialize)]`, so `Status.message` cannot reach a serialized payload
    through this crate's types; a consumer that must keep server prose off its screen holds that
    with its own check, not with this absence.
- **`tokio`'s `io-util` is the test seam**: `tokio::io::duplex` lives behind it
  ([verification](verification.md)).
- **Every public item is documented**: `#![warn(missing_docs)]` in `lib.rs`, and CI's
  `-D warnings` turns a missing doc into a failure.
- **The README's Rust examples compile.** `lib.rs` includes the README as a `#[cfg(doctest)]` item,
  so `cargo test` checks them; an example that needs another crate (`russh`) is marked `ignore`.
- **Published docs carry no repository pointers.** The crate root's `//!` and every public item's
  `///` are rendered on docs.rs, where `docs/map/...` points nowhere. A pointer from a public item to
  its note is a plain `//` line; private modules' `//!` headers are not published.
- **The logo is referenced by absolute `raw.githubusercontent.com/.../main/logo/...` URLs and is not
  packaged.** `logo/` is outside `include`, so the `.crate` carries no images. crates.io rewrites a
  relative `src` against `repository` but leaves `srcset` alone (crates.io's sanitizer is ammonia,
  whose `is_url_attr` does not list `srcset`; rust-lang/crates.io `0498d51e`, rust-ammonia/ammonia
  `4c7a8527`), so the README's dark-mode
  `<picture>` source must be absolute; docs.rs renders the crate root rather than the README, so its
  logo comes from `#![doc(html_logo_url, html_favicon_url)]`. The README uses the transparent
  lockups, not the opaque `logo/readme/` banners, whose `#0d0d0d` background shows as a box on
  GitHub's `#0d1117`. docs.rs uses the white-tile icon, because the plain black mark vanishes in its
  dark and ayu themes. An image shows only once the commit is on `main`.
- **The toolchain is pinned** (`rust-toolchain.toml`, 1.96.0) so local and CI builds match and a new
  Rust release cannot turn CI red on its own. `rust-version` in `Cargo.toml` states the same floor.
- **A version is published with `cargo publish`** after the CI gates pass, with its `CHANGELOG.md`
  entry written first; a crates.io version cannot be re-published, only yanked.

## Code

- `Cargo.toml`, `Cargo.lock`, `README.md`, `CHANGELOG.md`, `LICENSE-MIT`, `LICENSE-APACHE`,
  `rust-toolchain.toml`, `.gitignore`
- `logo/` — the brand pack; its `README.md` says which file each surface uses
- `src/lib.rs` — the crate docs and the public re-exports

## Reference behaviour

**None.**

## Cross-cutting invariants

- [Paths are bytes](../invariant/paths-are-bytes.md)

## Blast radius

- [Verification](verification.md) — CI and the toolchain pin.
- Consumers — PenTerm depends on this by version from crates.io.

## Known holes / open

- **No publish workflow.** A version is published by hand.
- **The GitHub social preview has no API.** It is uploaded by hand under Settings → Social preview.
