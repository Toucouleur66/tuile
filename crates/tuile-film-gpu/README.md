<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-film-gpu

Renders a pre-baked film on wgpu.

Nothing here is interactive and nothing is decided: the pack already holds
every frame's selection, meshes and imagery. So the GPU does as little
rasterisation as it can — one pass that records which triangle of which
tile covers each pixel — and everything else in compute:

1. **mips** of every entering texture, averaged in linear light;
2. **visibility** raster into `(slot, triangle)` and reverse-Z depth;
3. **binning**: pixels counted, prefix-summed and scattered by tile;
4. **resolve**, one indirect dispatch per visible tile with its geometry
   and texture bound: exact barycentrics and UV gradients from each pixel's
   own ray, anisotropic sampling, the film's look;
5. **present**: supersampling box filter, exposure, sRGB curve;
6. optionally **I420**, so a native encoder reads half the bytes.

Two things the look may ask for, and by default does not. **Shadows**
([`tuile_film::Look::shadow`]): one more raster, of depth alone, of the
frame's tiles as the sun sees them, which the resolve reads to take the
sun — and only the sun — from ground that other ground stands before.
A supersampled frame draws one such map for each sample of a pixel, up
to four, each on its grid moved by a fraction of a texel, and each
sample reads its own: a pixel sees a thin shadow averaged over as many
grids, which holds down its flicker as the sun moves.
**Air** ([`tuile_film::Look::haze`]): what a ray crosses on its way to
the eye, in closed form in the resolve, and a sky that runs from the
horizon's colour to the zenith's in `present`.

**The physical atmosphere** ([`tuile_film::Look::atmosphere`],
Hillaire 2020): the frame's light worked out from the air, once a frame;
a sky-view table made in compute and read by `present` for the sky; and,
in the resolve, a short march of the air between the eye and each
pixel's ground over the transmittance and multiple-scattering tables.

A frame may also carry a host's **overlays** ([`tuile_film::OverlayMesh`]):
coloured triangles placed in the world, lit when they have normals. They
are the one thing rastered besides visibility — after it, into a target
of their own with a depth of their own, opaque ones first, each tested
against the ground's depth, the overlays' or nothing as it asks — and
`present` lays them over the picture once its tone is done. Those that
cast are drawn into the sun's map with the ground; those that receive
read it as the ground does. Those that project theirs
([`tuile_film::OverlayShadow::Project`]) stay out of the map: each of
their triangles is drawn as the volume its penumbra sweeps down the
sun's rays, and each pixel of ground in it works out how much of the
sun's disc, seen from there, the triangle hides — an exact shadow with
its penumbra ([`tuile_film::Look::penumbra_scale`]), that no grid of
texels makes crawl as the sun moves. A frame without any costs nothing
more than before.

A stereo frame is drawn once an eye ([`Shot`]): the sun's map is fitted
to the frame's own camera, so both eyes are shaded by the same map, and
an overlay meant for one eye is left out of the other's picture.

Binning is what makes this portable: WebGPU has no bindless textures, so
the resolve cannot pick a tile's texture per pixel — but it can run once
per tile over exactly that tile's pixels.

The device is the caller's. A browser hands in a WebGPU device, a binary a
headless one; this crate never creates either.

## Where it sits

Depends on `tuile-film`, `tuile-core`, `tuile-terrain`, `tuile-atmosphere` and
`tuile-overlay`. Used by `tuile-film-native` on a machine's adapter and by the
film pages through WebGPU.

## Example

```rust,no_run
use tuile_film_gpu::{FilmGpu, Settings};

fn renderer(device: wgpu::Device, queue: wgpu::Queue, settings: Settings) -> FilmGpu {
    // The device is the caller's: headless on a farm, WebGPU in a page.
    FilmGpu::new(device, queue, settings)
}
```

## Features

None. Builds for wasm32.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/19-sky-atmosphere.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/19-sky-atmosphere.md)
