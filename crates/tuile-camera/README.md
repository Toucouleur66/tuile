<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-camera

A render-agnostic globe camera and input controller. Everything is f64
ECEF; the only outputs are a [`tuile_core::traversal::ViewState`] (for the
SSE traversal) and a rebased f32 view-projection (for any renderer). There
is **no** dependency on a windowing or graphics library: a façade feeds
pixel-space gestures in and reads the camera out, so the very same control
logic drives winit, an egui panel, a web front-end or touch.

The gestures follow Cesium's `ScreenSpaceCameraController`:
- **drag** = pick the ellipsoid under the cursor at the gesture's start and
  end, then rotate the camera about the geocenter so the start point lands
  under the end cursor — exact 1:1 surface dragging, independent of zoom
  (Cesium `pan3D`);
- **zoom** scales the *altitude* toward the point under the cursor, clamped
  above the surface (never through it) and under a ceiling it slows into
  (`max_altitude`, by default twice the distance at which the globe fits
  the view: 36 043 km — see `default_max_altitude`);
- **tilt** / **heading** orbit the look-at point on the surface.

Cameras that follow a subject along a route known in advance — smoothed,
with bounded turns — are in [`path`].

## Where it sits

Depends on `tuile-core`. Used by `tuile-ui`, `tuile-viewer`, `tuile-tape` and
the `orbit-probe` example.

## Example

```rust
use tuile_camera::{CameraController, GlobeCamera};

let camera = GlobeCamera::from_geodetic(
    45.83_f64.to_radians(), // latitude
    6.86_f64.to_radians(),  // longitude
    6_000.0,                // altitude, m
    0.0,                    // heading: north
    std::f64::consts::FRAC_PI_4, // pitch: 45° below the horizon
    60_f64.to_radians(),    // vertical field of view
);
let mut controller = CameraController::new(camera);
controller.zoom(1.0, (400.0, 300.0), (800.0, 600.0));
controller.update(1.0);
assert!(controller.target().altitude() > 0.0);
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/11-crate-wgpu.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/11-crate-wgpu.md)
