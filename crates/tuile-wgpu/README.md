<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-wgpu

Reference wgpu render backend for `tuile-core`: correct and readable
before fast — this is the core's shop window, not a game engine
(`docs/11-crate-wgpu.md`).

- [`GpuContext`] — device/queue + shared layouts ([`GpuContext::headless`]
  for tests and offscreen rendering)
- [`prepare`]/[`PreparedTile`] — decoded content → GPU resources
  (interleaved buffers, mipmapped sRGB textures)
- [`TileRenderer`] — one minimal PBR pipeline (+ wireframe when the
  device supports it, + debug lines for bounding volumes)
- [`ContentPump`] — the async→frame bridge over any
  [`tuile_core::protocol::GeometryStream`]
- [`Atmosphere`] — the physical atmosphere a host configures
  ([`AtmosphereConfig`]: on or off, the air, the look, the clock) and
  draws each frame ([`TileRenderer::draw_atmosphere`]); [`SkyFrame`] —
  the same for one stated instant: the sky, the sun,
  the Moon and the stars past the ground, and the air before it
  ([`TileRenderer::set_sky`]), the film's own model and code
- [`OverlayRenderer`] — pixel-space triangles over the scene, for the
  on-screen controls a host builds with `tuile-ui`
- [`FrameTargets`] — the multisampled colour and depth a frame is drawn
  into, with the sample count that has to match the pipelines
- [`Readback`] — a drawn frame back on the CPU, rows correctly unpadded
- [`draw_to_surface`] — one frame onto a surface the host created, with the
  four ways acquiring a drawable can fail already handled
- [`draw_to_texture`] — the same frame into a texture of the host's, for a
  headless run
- [`HostPass`] — tuile's pass inside a frame the host owns: its device,
  its colour and depth (any sample count, forward or reverse-Z, with or
  without a stencil), its encoder; nothing cleared unless asked, nothing
  submitted ([`TargetLayout`], [`TileRenderer::for_layout`])

The crate never creates a window or a surface; the host owns the event
loop and the render pass.

## Where it sits

Depends on `tuile-core`, `tuile-terrain`, `tuile-planetary`,
`tuile-atmosphere` and `tuile-overlay`. Used by `tuile-viewer` and the
`globe-bulk` and `orbit-probe` examples.

## Example

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use tuile_wgpu::{GpuContext, TileRenderer};

let gpu = GpuContext::headless().await?;
let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
# let _ = renderer;
# Ok(())
# }
```

## Features

None. Builds for wasm32 (WebGPU and WebGL2).

## Documentation

- [`docs/11-crate-wgpu.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/11-crate-wgpu.md)
- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
