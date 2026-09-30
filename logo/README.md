# JustSFTP brand assets

This pack is based on the approved supplied JustSFTP raster mark. It preserves the original black, interlocking symbol as transparent PNG exports and monochrome light/dark variants.

- Use `readme/justsftp-readme-light.png` on white or light README backgrounds; use `readme/justsftp-readme-dark.png` on near-black surfaces.
- Use the matching files in `social/` for 1280 × 640 social previews.
- `favicon/favicon.ico` and the adjacent 16, 32, and 48 px PNGs are the favicon set.
- `icons/` contains transparent light (black) and dark (white) icons at 16–1024 px.
- `icons/justsftp-icon-tile-{size}.png` is the black symbol on a white rounded tile, for a surface
  whose theme is not known in advance.

Where each one is used:

| Surface | File |
|---|---|
| Repository README, and crates.io, which renders it | `png/justsftp-lockup-black.png`, `png/justsftp-lockup-white.png` under `prefers-color-scheme: dark` |
| docs.rs sidebar logo and favicon (`#![doc(html_logo_url, html_favicon_url)]` in `src/lib.rs`) | `icons/justsftp-icon-tile-256.png`, `icons/justsftp-icon-tile-32.png` |
| GitHub social preview (uploaded by hand in repository settings) | `social/justsftp-social-light.png` |

Do not stretch, crop, recolour, add effects to, or otherwise alter the mark. The `svg/` files are compatibility wrappers around the raster source: the approved master supplied for this pack is not vector artwork. Replace them with true vector exports only if an approved vector master is supplied.
