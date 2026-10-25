<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-bing

Bing Maps imagery connector. Resolves the Bing metadata REST document to
get the tile URL template + subdomains, maps a tile to a quadkey, and
implements the core's [`tuile_core::raster::ImageryProvider`] so it drapes
onto terrain (or any geometry) like any other imagery source.

Independent of ion: the Bing key may come from a `tuile-cesium-ion`
`ImageryEndpoint` (`options.key`) or directly. Tile fetching goes through
a [`tuile_core::fetch::TileFetcher`] — wasm-clean, no reqwest here.

## Where it sits

Depends on `tuile-core` only (its raster and fetch seams). Used by
`tuile-cesium-ion`, which resolves the key, by `tuile-bake`, and by the
examples that drape imagery (`wgpu-viewer`, `globe-bulk`, `orbit-probe`,
`warm-cache`).

## Example

```rust
use tuile_bing::BingMetadata;

// Level 3, x = 3, y = 5 in Web Mercator.
assert_eq!(BingMetadata::quadkey(3, 5, 3), "213");
```

`BingImageryProvider::new(fetcher, metadata)` then gives an
`ImageryProvider` for the globe, with `metadata` read from the REST document
by `BingMetadata::from_json`.

## Features

None. Wasm-clean.

## Documentation

- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
- [`docs/03-roadmap.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/03-roadmap.md)
