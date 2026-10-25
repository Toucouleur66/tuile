// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod camera;
mod cursor;
pub mod drape;
pub mod from_store;
pub mod ground;
mod look;
mod mesh;
mod overlay;
mod ranges;
mod slice;
pub mod smoothing;
mod stereo;
pub mod stitching;
mod survey;

pub use camera::{FrameCamera, NEAR_FRACTION};
pub use cursor::{
    referred, store_tiles, Cursor, FilmError, FrameDiff, LayerTiles, StoreAt, TileKey,
};
pub use look::{FrameAir, FrameLight, Haze, HazeAt, Imagery, Look};
pub use mesh::{texture, texture_of_span, Mesh};
pub use overlay::{
    OverlayBlend, OverlayCamera, OverlayDepth, OverlayEyes, OverlayMesh, OverlayShadow, Overlays,
    MIN_SHADOW_TEXELS,
};
pub use ranges::{block_plan, coalesce, file_reads, Fetch};
pub use slice::slice;
pub use stereo::{
    pack_i420, pack_rgba, Convergence, Eye, EyePair, Packing, Stereo, StereoError,
    DEFAULT_DIVERGENCE, MAX_DIVERGENCE,
};
pub use survey::{cameras, frame_tiles, CameraSample, TileInfo};
pub use tuile_atmosphere::{AtmosphereParams, ReferenceIllumination};
pub use tuile_pack::{
    blob_start, fb, refs_of, BakedView, Content, Fnv1a, ImageryPlacement, Pack, PackError,
    StoreTile, TileRefs, PREAMBLE,
};
