<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-planetary

Planetary scene assembly: crosses a **terrain source** with an **imagery
provider** into the [`TileTree`] + [`TileLoader`] pair the geometry server
drives, so the globe streams through the same runtime as any other source.

It is generic over [`tuile_terrain::TerrainSource`] and
[`tuile_core::raster::ImageryProvider`] — it knows **nothing** about ion,
Bing, HTTP or any transport. The host decides those at injection time
(resolve the sources, then call [`globe`]); a wasm host and a native host
plug in different backends behind the same two traits. This crate is pure
and wasm-able: no I/O of its own beyond the injected sources.

[`PlanetaryLoader`] is the geometry↔imagery crossing point: per terrain tile
it decodes the quantized mesh and names the imagery tiles that cover it, each
reprojected once onto its own rectangle and **shared** with every other
terrain tile it drapes.

## Where it sits

Depends on `tuile-core` and `tuile-terrain`. Used by `tuile-cesium-ion` (which
resolves the sources for it), `tuile-wgpu`, `tuile-viewer`, `tuile-bake` and
the globe examples.

## Example

The host resolves a terrain source, an imagery provider and the terrain's
`layer.json`, then hands all three over:

```rust,no_run
use tuile_core::raster::ImageryProvider;
use tuile_planetary::{globe, GlobeOptions};
use tuile_terrain::{LayerJson, TerrainSource};

fn scene<T: TerrainSource + 'static, I: ImageryProvider + 'static>(
    terrain: T,
    imagery: I,
    layer: LayerJson,
) {
    let (tree, loader, heights) = globe(terrain, imagery, layer, GlobeOptions::default());
    // `tree` and `loader` go to `tuile_core::runtime::in_process_with`,
    // `heights` to the camera controller.
    # let _ = (tree, loader, heights);
}
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/03-roadmap.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/03-roadmap.md)
