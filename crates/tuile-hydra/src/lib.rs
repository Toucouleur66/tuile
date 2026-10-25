// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod ffi;

// Re-exported so the plugin side has one crate to name, and so an existing
// `tuile_hydra::Session` keeps resolving.
pub use tuile_bake::{
    exact_traversal, packed, EncodedTexture, Frame, FrameError, GlobeConfig, GlobeError,
    PackedError, Session, SessionConfig, TileGeometry, BING_AERIAL, CESIUM_WORLD_TERRAIN,
};
