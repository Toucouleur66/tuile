// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-terrain
//!
//! Quantized-mesh terrain decoding and geographic tiling — the data side of
//! the globe pipeline (`docs/03-roadmap.md`, MG).
//!
//! - [`decode`](mod@decode) — quantized-mesh-1.0 → [`decode::QuantizedMesh`] (normalized
//!   vertices, high-water-mark indices, oct-decoded normals, skirt edges),
//!   and the encoder back ([`decode::encode`], [`Header::for_vertices`])
//! - [`tiling`] — the EPSG:4326 geographic quadtree (Cesium World Terrain's
//!   scheme): tile → rectangle, per-level geometric error
//! - [`mesh`] — [`decode::QuantizedMesh`] + tile rectangle → a render-ready
//!   [`tuile_core::content::DecodedTileContent`] (ECEF-rebased, skirts),
//!   the same type the glTF path produces
//! - [`heightfield`] — a relief (a [`heightfield::Grid`] of samples, or any
//!   [`heightfield::Heights`]) served as quantized-mesh tiles, the
//!   [`ellipsoid`] around it
//! - [`http`] — terrain from any `layer.json` URL, through a
//!   [`tuile_core::fetch::TileFetcher`]
//!
//! Wasm-clean: the only I/O goes through the core's `TileFetcher`. Sources
//! that need a token (ion) live in their connector crates.

pub mod availability;
pub mod decode;
pub mod ellipsoid;
pub mod heightfield;
mod heights;
pub mod http;
pub mod layer;
pub mod mesh;
pub mod source;
pub mod tiling;
pub mod tree;
mod upsample;

pub use availability::Availability;
pub use decode::{decode, DecodeError, Header, QuantizedMesh};
pub use ellipsoid::{Ellipsoid, Nothing};
pub use heightfield::{Grid, HeightfieldSource, Heights, Outside};
pub use heights::TerrainHeights;
pub use http::HttpTerrainSource;
pub use layer::{AvailabilityRange, LayerError, LayerJson, TileUrlError};
pub use mesh::{
    fill_content, globe_shell, skirt_depth, skirt_depth_cut, skirt_height, surface_uvs, to_decoded,
    to_ground,
};
pub use source::{CachedTerrain, TerrainSource, TerrainSourceError};
pub use tiling::{level_geometric_error, GeoRect, GeographicTilingScheme, TileCoord};
pub use tree::TerrainTree;
pub use upsample::upsample;
