// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Terrain made here, of no data: the WGS84 ellipsoid itself, and no
//! ground at all.
//!
//! - [`Ellipsoid`]: every tile of the geographic quadtree down to
//!   [`ELLIPSOID_LEVELS`], a regular grid at height zero. What a globe is
//!   drawn on when no relief is wanted or known — from orbit its curvature
//!   is the only relief there is to see — and what a
//!   [`HeightfieldSource`](crate::heightfield::HeightfieldSource) answers
//!   outside its footprint, so that no ground is ever missing around it.
//! - [`Nothing`]: the two root tiles, each a mesh of no triangle, and nothing
//!   under them.
//!
//! Both are pure: a tile's bytes are a function of its coordinate alone.

use tuile_core::fetch::Fetched;
use tuile_core::geo::{geodetic_to_ecef, Geodetic, WGS84_A};

use crate::decode::{encode, Header};
use crate::heightfield::GridTopology;
use crate::layer::{AvailabilityRange, LayerJson};
use crate::source::{TerrainSource, TerrainSourceError};
use crate::tiling::{GeographicTilingScheme, TileCoord};

/// The deepest level of the ellipsoid's tiles: past it a tile would be
/// flatter than any camera that wants it can tell.
pub const ELLIPSOID_LEVELS: u32 = 14;
/// Spacings a side of an ellipsoid tile: enough that its curve is smooth
/// from orbit at level 0.
pub const ELLIPSOID_STEPS: usize = 32;

/// The ellipsoid as terrain.
pub struct Ellipsoid;

impl Ellipsoid {
    /// Its `layer.json`: quantized mesh, TMS, geographic, every tile there.
    pub fn layer() -> LayerJson {
        let available = (0..=ELLIPSOID_LEVELS)
            .map(|level| {
                vec![AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: (2u64 << level) - 1,
                    end_y: (1u64 << level) - 1,
                }]
            })
            .collect();
        LayerJson {
            format: "quantized-mesh-1.0".into(),
            scheme: Some("tms".into()),
            projection: Some("EPSG:4326".into()),
            tiles: vec!["{z}/{x}/{y}".into()],
            version: Some("1".into()),
            minzoom: 0,
            maxzoom: ELLIPSOID_LEVELS,
            metadata_availability: None,
            extensions: Vec::new(),
            available,
            attribution: None,
        }
    }

    /// Tile `coord`'s quantized mesh: a grid on the ellipsoid at height 0.
    ///
    /// Its header is its own, not [`Header::for_vertices`]: a sphere around
    /// eight points of the tile, and a horizon point that is merely the
    /// centre scaled down. Rough, and kept so on purpose — these bytes are
    /// what bakes have digested since the ellipsoid first served them, and
    /// a header made exact would move every open-earth digest for no
    /// picture's sake (the tile is flat; nothing tuile draws reads the
    /// horizon point).
    pub fn tile(coord: TileCoord) -> Vec<u8> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let n = ELLIPSOID_STEPS + 1;
        let (mut u, mut v) = (Vec::with_capacity(n * n), Vec::with_capacity(n * n));
        for j in 0..n {
            for i in 0..n {
                u.push(i as f64 / ELLIPSOID_STEPS as f64);
                v.push(j as f64 / ELLIPSOID_STEPS as f64);
            }
        }
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        // A sphere that holds the tile: its corners and edges' middles.
        let mut radius = 0.0f64;
        for (fu, fv) in [
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (1.0, 1.0),
            (0.5, 0.0),
            (0.5, 1.0),
            (0.0, 0.5),
            (1.0, 0.5),
        ] {
            let p = geodetic_to_ecef(Geodetic {
                lon: rect.west + fu * rect.width(),
                lat: rect.south + fv * rect.height(),
                height: 0.0,
            });
            radius = radius.max(p.distance(centre));
        }
        let header = Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: radius.max(1.0),
            horizon_occlusion: (centre / WGS84_A).to_array(),
        };
        let grid = GridTopology::new(n);
        let heights = vec![0.0; n * n];
        encode(
            &header,
            &grid.reorder(&u),
            &grid.reorder(&v),
            &heights,
            &grid.indices,
            None,
            &grid.edges,
        )
    }
}

/// Terrain of no ground: the two root tiles, each a mesh of no triangle,
/// and nothing under them.
pub struct Nothing;

impl Nothing {
    pub fn layer() -> LayerJson {
        LayerJson {
            available: vec![vec![AvailabilityRange {
                start_x: 0,
                start_y: 0,
                end_x: 1,
                end_y: 0,
            }]],
            maxzoom: 0,
            ..Ellipsoid::layer()
        }
    }

    /// A root tile's mesh: its header, and no vertex.
    pub fn tile(coord: TileCoord) -> Vec<u8> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        let header = Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: WGS84_A,
            horizon_occlusion: (centre / WGS84_A).to_array(),
        };
        encode(
            &header,
            &[],
            &[],
            &[],
            &[],
            None,
            &[Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        )
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl TerrainSource for Nothing {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        Ok(Fetched::undated(Self::tile(coord)))
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl TerrainSource for Ellipsoid {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        Ok(Fetched::undated(Self::tile(coord)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The implementations as they stood in `tuile-bake` before the move,
    // verbatim: what bakes digested. Compared on the platform that runs the
    // test, since the trigonometry under both is the platform's own.
    fn legacy_ellipsoid_tile(coord: TileCoord) -> Vec<u8> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let n = ELLIPSOID_STEPS + 1;
        let (mut u, mut v) = (Vec::with_capacity(n * n), Vec::with_capacity(n * n));
        for j in 0..n {
            for i in 0..n {
                u.push(i as f64 / ELLIPSOID_STEPS as f64);
                v.push(j as f64 / ELLIPSOID_STEPS as f64);
            }
        }
        let mut indices = Vec::with_capacity(ELLIPSOID_STEPS * ELLIPSOID_STEPS * 6);
        for j in 0..ELLIPSOID_STEPS {
            for i in 0..ELLIPSOID_STEPS {
                let a = (j * n + i) as u32;
                let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
                indices.extend_from_slice(&[a, b, c, c, b, d]);
            }
        }
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        // A sphere that holds the tile: its corners and edges' middles.
        let mut radius = 0.0f64;
        for (fu, fv) in [
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (1.0, 1.0),
            (0.5, 0.0),
            (0.5, 1.0),
            (0.0, 0.5),
            (1.0, 0.5),
        ] {
            let p = geodetic_to_ecef(Geodetic {
                lon: rect.west + fu * rect.width(),
                lat: rect.south + fv * rect.height(),
                height: 0.0,
            });
            radius = radius.max(p.distance(centre));
        }
        let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
        let header = Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: radius.max(1.0),
            horizon_occlusion: (centre / WGS84_A).to_array(),
        };
        let edges = [
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + ELLIPSOID_STEPS),
            line(&|k| (n - 1) * n + k),
        ];
        // Vertices renumbered in the order the triangles first name them:
        // the high-water-mark coding of the indices requires it.
        let mut order = vec![u32::MAX; n * n];
        let mut next = 0u32;
        for i in &mut indices {
            let slot = &mut order[*i as usize];
            if *slot == u32::MAX {
                *slot = next;
                next += 1;
            }
            *i = *slot;
        }
        let mut uu = vec![0.0; n * n];
        let mut vv = vec![0.0; n * n];
        for (old, &new) in order.iter().enumerate() {
            uu[new as usize] = u[old];
            vv[new as usize] = v[old];
        }
        let edges = edges.map(|e| e.into_iter().map(|i| order[i as usize]).collect());
        let heights = vec![0.0; n * n];
        encode(&header, &uu, &vv, &heights, &indices, None, &edges)
    }

    fn legacy_nothing_tile(coord: TileCoord) -> Vec<u8> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        let header = Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: WGS84_A,
            horizon_occlusion: (centre / WGS84_A).to_array(),
        };
        encode(
            &header,
            &[],
            &[],
            &[],
            &[],
            None,
            &[Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        )
    }

    /// The ellipsoid's and the empty ground's tiles are the very bytes the
    /// bake produced before they moved here: a bake's digests are taken over
    /// them, and a byte moved is every open-earth digest moved.
    #[test]
    fn tiles_are_the_bytes_they_were_before_the_move() {
        let mut coords = vec![
            TileCoord::new(0, 0, 0),
            TileCoord::new(0, 1, 0),
            TileCoord::new(3, 0, 7),
            TileCoord::new(6, 64, 50),
            TileCoord::new(14, 16_390, 12_000),
        ];
        for level in 1..=ELLIPSOID_LEVELS {
            let (nx, ny) = (2u64 << level, 1u64 << level);
            coords.push(TileCoord::new(
                level,
                (level as u64 * 7919) % nx,
                (level as u64 * 104_729) % ny,
            ));
        }
        for coord in coords {
            let tile = Ellipsoid::tile(coord);
            assert_eq!(tile.len(), 19_198, "{coord:?}");
            assert!(tile == legacy_ellipsoid_tile(coord), "ellipsoid {coord:?}");
        }
        for coord in [TileCoord::new(0, 0, 0), TileCoord::new(0, 1, 0)] {
            assert!(
                Nothing::tile(coord) == legacy_nothing_tile(coord),
                "nothing {coord:?}"
            );
        }
    }

    #[test]
    fn layers_name_every_ellipsoid_tile_and_only_the_roots_of_nothing() {
        let layer = Ellipsoid::layer();
        assert_eq!(layer.maxzoom, ELLIPSOID_LEVELS);
        assert!(layer.is_available(TileCoord::new(
            ELLIPSOID_LEVELS,
            (2 << 14) - 1,
            (1 << 14) - 1
        )));
        assert!(!layer.is_available(TileCoord::new(ELLIPSOID_LEVELS + 1, 0, 0)));
        let nothing = Nothing::layer();
        assert!(nothing.is_available(TileCoord::new(0, 1, 0)));
        assert!(!nothing.is_available(TileCoord::new(1, 0, 0)));
    }
}
