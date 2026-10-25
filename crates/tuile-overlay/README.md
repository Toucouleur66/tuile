<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-overlay

What a host adds to the scene: shapes, placed in the world.

An overlay is geometry — coloured triangles in Earth coordinates — drawn
in the same scene as the ground and through the same camera. It is not a
picture laid over the frame afterwards: a shape that passes behind a
ridge is hidden by the ridge, pixel by pixel.

Nothing here knows what a shape stands for. The host decides what to
draw, where and in what colour, frame by frame; the renderer draws
triangles.

# Why these types are their own crate

They began in `tuile-film`, because a film was the only thing that drew
them. An interactive viewer draws them too — a host that is replaying a
camera, or previewing one, wants the same trail and the same badge — and
it has no film: no pack, no baked view, no look. Everything here is data
and arithmetic, so it costs that host nothing.

What stayed behind is what a film's overlays are *made for*:
`OverlayCamera` carries a `BakedView` and the frame's own camera, which
are a film's own vocabulary.

## Where it sits

No workspace dependency. Used by the renderers that draw overlays —
`tuile-wgpu`, `tuile-film`, `tuile-film-gpu` — and by `tuile-viewer`.

## Example

```rust
use tuile_overlay::OverlayMesh;

// One red triangle, ten metres across, on the equator at the prime meridian.
let mesh = OverlayMesh {
    origin_ecef: [6_378_137.0, 0.0, 0.0],
    positions: vec![[0.0, 0.0, 0.0], [0.0, 10.0, 0.0], [0.0, 0.0, 10.0]],
    colors: vec![[1.0, 0.0, 0.0, 1.0]; 3],
    indices: vec![0, 1, 2],
    ..Default::default()
};
assert_eq!(mesh.fault(), None);
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/16-embedding-the-viewer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/16-embedding-the-viewer.md)
