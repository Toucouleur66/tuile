<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-ui

On-screen controls for a globe viewer, with no renderer attached.

A widget here does three things and draws nothing: it says **where** its
parts sit for a given viewport, it says **what a pixel hits**, and it turns
a gesture into calls on a [`tuile_camera::CameraController`]. Geometry comes
out as plain 2-D triangles in pixel coordinates ([`Vertex`]), which any
backend can upload — wgpu today, a canvas or an SVG tomorrow.

Keeping the widget out of the render backend is what lets the same control
work on every façade, and keeping it out of `tuile-camera` is what keeps
that crate to camera state and gestures. The widgets are pure: no I/O, no
graphics API, wasm-clean.

[`Spinner`] is the loading indicator a viewer shows while the ground under
its first view is still arriving.

One thing here is not a widget and not pure: [`StatusBar`], behind the
`statusbar` feature, which is a **native menu-bar item**. It lives here
because it is a control a viewer offers a person, and it is optional
because nothing else in this crate should have to carry a platform
dependency to get the compass.

## Where it sits

Depends on `tuile-camera` and `tuile-core`. Used by `tuile-viewer`, whose
`tuile-wgpu` overlay renderer draws the triangles.

## Example

```rust
use tuile_ui::NavWidget;

let nav = NavWidget::new();
// What a click at the centre of an 800 × 600 view lands on, if anything.
let _part = nav.hit((400.0, 300.0), (800.0, 600.0));
assert!(!nav.is_active());
```

## Features

- `statusbar` — the native menu-bar readout ([`StatusBar`]). Off by default,
  so the crate stays pure and wasm-clean.

## Documentation

- [`docs/16-embedding-the-viewer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/16-embedding-the-viewer.md)
- [`docs/11-crate-wgpu.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/11-crate-wgpu.md)
