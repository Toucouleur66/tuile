// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-wgpu
//!
//! Reference wgpu render backend for `tuile-core`: correct and readable
//! before fast — this is the core's shop window, not a game engine
//! (`docs/11-crate-wgpu.md`).
//!
//! - [`GpuContext`] — device/queue + shared layouts ([`GpuContext::headless`]
//!   for tests and offscreen rendering)
//! - [`prepare`]/[`PreparedTile`] — decoded content → GPU resources
//!   (interleaved buffers, mipmapped sRGB textures)
//! - [`TileRenderer`] — one minimal PBR pipeline (+ wireframe when the
//!   device supports it, + debug lines for bounding volumes)
//! - [`ContentPump`] — the async→frame bridge over any
//!   [`tuile_core::protocol::GeometryStream`]
//! - [`OverlayRenderer`] — pixel-space triangles over the scene, for the
//!   on-screen controls a host builds with `tuile-ui`
//! - [`FrameTargets`] — the multisampled colour and depth a frame is drawn
//!   into, with the sample count that has to match the pipelines
//! - [`Readback`] — a drawn frame back on the CPU, rows correctly unpadded
//! - [`draw_to_surface`] — one frame onto a surface the host created, with the
//!   four ways acquiring a drawable can fail already handled
//! - [`draw_to_texture`] — the same frame into a texture of the host's, for a
//!   headless run
//! - [`HostPass`] — tuile's pass inside a frame the host owns: its device,
//!   its colour and depth (any sample count, forward or reverse-Z, with or
//!   without a stencil), its encoder; nothing cleared unless asked, nothing
//!   submitted ([`TargetLayout`], [`TileRenderer::for_layout`])
//!
//! The crate never creates a window or a surface; the host owns the event
//! loop and the render pass.

mod compress;
mod context;
mod encode;
mod host;
mod overlay;
mod prepare;
mod pump;
mod readback;
mod renderer;
mod surface;
mod targets;
#[cfg(test)]
mod webgl2;
mod world_overlay;

pub use compress::{
    adapter_plan, caps_of, compression_named, format_name, plan_features, plan_for,
    stored_features, texture_caps, EncodePace, EncodeStats, ImageryCompression,
};
pub use context::{
    ContextError, GpuContext, ImageryTextures, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};
pub use host::{view_proj_from_world, DepthDirection, HostFrame, HostPass, TargetLayout};
pub use overlay::{OverlayRenderer, OverlayVertex};
pub use prepare::{
    prepare, prepare_drape, PreparedDrape, PreparedMesh, PreparedTile, Vertex, STITCH_ROWS,
};
pub use pump::{ContentPump, Drawn, Resolution, STITCH_SLIDE_FRAMES, UPLOAD_SHARE_PERMILLE};
pub use readback::Readback;
pub use renderer::{LineVertex, TileRenderer, ViewUniform};
pub use surface::{
    draw_to_surface, draw_to_texture, preferred_format, surface_format, FrameOnSurface,
    FrameOnTexture, FramePass, Presented, SurfaceFormat,
};
pub use targets::FrameTargets;
pub use world_overlay::{Picture, WorldOverlayError, WorldOverlayRenderer, WorldOverlayView};
