<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-terrain

Quantized-mesh terrain decoding and geographic tiling — the data side of
the globe pipeline (`docs/03-roadmap.md`, MG).

- [`decode`](mod@decode) — quantized-mesh-1.0 → [`decode::QuantizedMesh`] (normalized
  vertices, high-water-mark indices, oct-decoded normals, skirt edges),
  and the encoder back ([`decode::encode`], [`Header::for_vertices`])
- [`tiling`] — the EPSG:4326 geographic quadtree (Cesium World Terrain's
  scheme): tile → rectangle, per-level geometric error
- [`mesh`] — [`decode::QuantizedMesh`] + tile rectangle → a render-ready
  [`tuile_core::content::DecodedTileContent`] (ECEF-rebased, skirts),
  the same type the glTF path produces
- [`heightfield`] — a relief (a [`heightfield::Grid`] of samples, or any
  [`heightfield::Heights`]) served as quantized-mesh tiles, the
  [`ellipsoid`] around it
- [`http`] — terrain from any `layer.json` URL, through a
  [`tuile_core::fetch::TileFetcher`]

Wasm-clean: the only I/O goes through the core's `TileFetcher`. Sources
that need a token (ion) live in their connector crates.

## Where it sits

Depends on `tuile-core`. Used by `tuile-planetary`, the connectors
(`tuile-cesium-ion`), the renderers (`tuile-wgpu`, `tuile-film`,
`tuile-film-gpu`), `tuile-viewer`, `tuile-bake` and the examples, among them
`heightfield-serve`.

## Example

```rust
use tuile_terrain::tiling::{GeographicTilingScheme, TileCoord};

let scheme = GeographicTilingScheme::default(); // 2 × 1 root tiles
assert_eq!(scheme.tiles_x(3), 16);
let rect = scheme.tile_rect(TileCoord::new(0, 0, 0));
assert!((rect.width() - std::f64::consts::PI).abs() < 1e-12);
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/03-roadmap.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/03-roadmap.md)
- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
