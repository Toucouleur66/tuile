// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground a film draws, read as heights.
//!
//! A camera kept over the ground has to be kept over the ground the picture
//! shows: the terrain's mesh with its skirts dropped, smoothed, then stitched
//! to its neighbours — not the source's heights, nor the highest point of a
//! tile. This module is that ground on the CPU, built by the functions the
//! renderer itself calls:
//!
//! - [`prepared`] is what a renderer does to a tile's mesh before anything
//!   else sees it (skirts, [`crate::smoothing`]);
//! - [`Stitching::drawn`] is the mesh after the stitch pass, the shader's
//!   arithmetic on the CPU (`tuile_core::stitch::Stitch::offset`);
//! - [`Ground`] holds those meshes for a frame's selection and answers the
//!   height under a place: where a vertical line through it meets the drawn
//!   triangles. Nothing is interpolated between samples; the height is the
//!   surface's, where the line crosses it.

use glam::DVec3;
use tuile_core::content::DecodedMesh;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_core::stitch;
use tuile_terrain::{GeoRect, GeographicTilingScheme, TileCoord};

use crate::smoothing::{self, Smooth, Smoothed};
use crate::stitching::{decoded, encoded, Stitching};
use crate::{Mesh, TileKey};

/// How a film's renderer prepares a tile's mesh: what [`prepared`] does.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Prepare {
    /// Skirts kept under the tile's edges.
    pub skirts: bool,
    pub smooth: Smooth,
}

/// A tile's mesh as a film's renderer draws it, before stitching: skirts
/// dropped unless kept, then smoothed; and what smoothing did.
pub fn prepared(mesh: Mesh, how: Prepare) -> (Mesh, Smoothed) {
    let origin = DVec3::from_array(mesh.origin_ecef);
    let mesh = if how.skirts {
        mesh
    } else {
        encoded(
            &stitch::without_skirts(&decoded(&mesh), origin),
            mesh.origin_ecef,
            mesh.base_color_factor,
        )
    };
    let (smoothed, did) = smoothing::smooth(&decoded(&mesh), origin, how.smooth);
    let out = if how.smooth == Smooth::Off {
        mesh
    } else {
        encoded(&smoothed, mesh.origin_ecef, mesh.base_color_factor)
    };
    (out, did)
}

/// Where the line down a place starts: above any ground there is.
const ABOVE_M: f64 = 12_000.0;

/// A tile as drawn, kept for its heights alone: where its vertices are and
/// how they make triangles.
struct Drawn {
    rect: GeoRect,
    origin: DVec3,
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

impl Drawn {
    fn of(key: &TileKey, origin: DVec3, mesh: DecodedMesh) -> Self {
        let (level, x, y) = TileId(key.id).terrain_coord();
        Self {
            rect: GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y)),
            origin,
            positions: mesh.positions,
            indices: mesh.indices,
        }
    }
}

/// The ground of one frame: the meshes of the tiles it draws, as drawn.
#[derive(Default)]
pub struct Ground {
    tiles: Vec<Drawn>,
}

impl Ground {
    /// The ground `stitching` draws for `selection`, as last planned
    /// ([`Stitching::frame`]). Tiles it does not hold are not ground.
    pub fn of(stitching: &Stitching, selection: &[TileKey]) -> Self {
        let tiles = selection
            .iter()
            .filter_map(|key| {
                let (origin, mesh) = stitching.drawn(key)?;
                Some(Drawn::of(key, origin, mesh))
            })
            .collect();
        Self { tiles }
    }

    /// The ground `stitching` holds for `selection`, unstitched: each tile
    /// as it entered, whatever was last planned.
    pub fn unstitched(stitching: &Stitching, selection: &[TileKey]) -> Self {
        let tiles = selection
            .iter()
            .filter_map(|key| {
                let (origin, mesh) = stitching.held(key)?;
                Some(Drawn::of(
                    key,
                    origin,
                    DecodedMesh {
                        positions: mesh.positions.clone(),
                        normals: None,
                        uvs: None,
                        indices: mesh.indices.clone(),
                        material: Default::default(),
                    },
                ))
            })
            .collect();
        Self { tiles }
    }

    /// The tiles this ground is made of.
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// The bytes the meshes it is made of take.
    pub fn heap_bytes(&self) -> usize {
        self.tiles
            .iter()
            .map(|t| {
                std::mem::size_of_val(t.positions.as_slice())
                    + std::mem::size_of_val(t.indices.as_slice())
            })
            .sum()
    }

    /// The height of the drawn ground at `lon`, `lat` (radians): where a
    /// vertical line there meets the highest triangle drawn. `None` when no
    /// drawn tile is there.
    ///
    /// Counted in `height_queries` and timed in `height_query_seconds`
    /// ([`tuile_core::metrics`]), every call. Timing all of them rather than
    /// one in N is measured, not assumed: a stamp pair and its record cost
    /// about 40 ns, a query on one 64×64 tile about 43 µs — a tenth of a
    /// percent. A query off every tile costs 13 ns and pays the same 40, which
    /// is still nothing beside the queries that find ground.
    pub fn height_at(&self, lon: f64, lat: f64) -> Option<f64> {
        let metrics = tuile_core::metrics::metrics();
        metrics.height_queries.inc();
        metrics
            .height_query_seconds
            .time(|| self.height_under(lon, lat))
    }

    /// [`Self::height_at`], uncounted.
    fn height_under(&self, lon: f64, lat: f64) -> Option<f64> {
        const ON: f64 = 1.0e-12;
        let top = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: ABOVE_M,
        });
        let down = -DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
        self.tiles
            .iter()
            .filter(|t| {
                lon >= t.rect.west - ON
                    && lon <= t.rect.east + ON
                    && lat >= t.rect.south - ON
                    && lat <= t.rect.north + ON
            })
            .filter_map(|t| first_hit(&t.positions, &t.indices, top - t.origin, down))
            .map(|along| ABOVE_M - along)
            .reduce(f64::max)
    }
}

/// How far along `dir` from `from` the ray first meets one of the
/// triangles `indices` makes of `positions` (both sides), in their frame.
fn first_hit(positions: &[[f32; 3]], indices: &[u32], from: DVec3, dir: DVec3) -> Option<f64> {
    let p = |i: u32| DVec3::from(positions[i as usize].map(f64::from));
    indices
        .as_chunks::<3>()
        .0
        .iter()
        .filter_map(|t| {
            // Möller–Trumbore.
            let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
            let (e1, e2) = (b - a, c - a);
            let h = dir.cross(e2);
            let det = e1.dot(h);
            if det.abs() < 1.0e-12 {
                return None;
            }
            let s = from - a;
            let u = s.dot(h) / det;
            let q = s.cross(e1);
            let v = dir.dot(q) / det;
            // A hair over the edges: the vertices are f32 about the tile's
            // origin, so a line down a corner can miss both triangles there
            // by a fraction of a millimetre.
            const EDGE: f64 = -1.0e-6;
            if u < EDGE || v < EDGE || u + v > 1.0 - EDGE {
                return None;
            }
            let along = e2.dot(q) / det;
            (along >= 0.0).then_some(along)
        })
        .reduce(f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use tuile_core::content::MaterialDesc;

    /// Tile `level/x/y` as a grid of `n` spacings a side, `height(u, north)`
    /// metres over the ellipsoid at each vertex.
    fn tile(
        level: u32,
        x: u64,
        y: u64,
        n: u32,
        height: impl Fn(f64, f64) -> f64,
    ) -> (TileKey, Mesh) {
        let size = PI / 2f64.powi(level as i32);
        let at = |u: f64, north: f64| {
            geodetic_to_ecef(Geodetic {
                lon: -PI + (x as f64 + u) * size,
                lat: -PI / 2.0 + (y as f64 + north) * size,
                height: height(u, north),
            })
        };
        let origin = at(0.5, 0.5);
        let mut positions = Vec::new();
        let mut uvs = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let (u, north) = (f64::from(i) / f64::from(n), f64::from(j) / f64::from(n));
                positions.push((at(u, north) - origin).as_vec3().to_array());
                uvs.push([u as f32, 1.0 - north as f32]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let k = j * (n + 1) + i;
                indices.extend_from_slice(&[k, k + 1, k + n + 1, k + n + 1, k + 1, k + n + 2]);
            }
        }
        let mesh = DecodedMesh {
            positions,
            normals: None,
            uvs: Some(uvs),
            indices,
            material: MaterialDesc::default(),
        };
        (
            TileKey {
                id: TileId::from_terrain(level, x, y).0,
                drape: 0,
            },
            encoded(&mesh, origin.to_array(), [1.0; 4]),
        )
    }

    fn place(level: u32, x: u64, y: u64, u: f64, north: f64) -> (f64, f64) {
        let size = PI / 2f64.powi(level as i32);
        (
            -PI + (x as f64 + u) * size,
            -PI / 2.0 + (y as f64 + north) * size,
        )
    }

    fn ground_of(tiles: &[(TileKey, Mesh)], how: Prepare) -> Ground {
        let mut stitching = Stitching::default();
        for (key, mesh) in tiles {
            stitching.enter(*key, None, &prepared(mesh.clone(), how).0);
        }
        let selection: Vec<TileKey> = tiles.iter().map(|(k, _)| *k).collect();
        stitching.frame(&selection);
        Ground::of(&stitching, &selection)
    }

    #[test]
    fn flat_ground_is_read_at_its_height_and_nowhere_else() {
        let flat = tile(14, 9000, 6200, 4, |_, _| 812.0);
        let ground = ground_of(&[flat], Prepare::default());
        for (u, north) in [(0.5, 0.5), (0.13, 0.91), (0.0, 0.0), (1.0, 1.0)] {
            let (lon, lat) = place(14, 9000, 6200, u, north);
            let h = ground.height_at(lon, lat).expect("on the tile");
            assert!((h - 812.0).abs() < 0.05, "{u} {north}: {h}");
        }
        let (lon, lat) = place(14, 9000, 6200, 1.5, 0.5);
        assert_eq!(ground.height_at(lon, lat), None, "off every drawn tile");
    }

    /// A ridge across the tile: the height between two vertices is the
    /// drawn triangle's — the flat chord unsmoothed, the bulge of the
    /// smoothed patch otherwise, never more than its bound above.
    #[test]
    fn the_height_is_the_smoothed_surface_s_where_the_ground_is_smoothed() {
        let ridge = |u: f64, _: f64| 1000.0 + 400.0 * (1.0 - (2.0 * u - 1.0).abs());
        let t = tile(14, 9000, 6200, 4, ridge);
        // Halfway between the ridge's line (u = 0.5) and the next vertices.
        let (lon, lat) = place(14, 9000, 6200, 0.5, 0.375);
        let flat = ground_of(std::slice::from_ref(&t), Prepare::default());
        let raw = flat.height_at(lon, lat).expect("ground");
        assert!((raw - 1400.0).abs() < 0.5, "{raw}");
        let bound = 2.0;
        let round = ground_of(
            &[t],
            Prepare {
                smooth: Smooth::Geometry { bound },
                ..Prepare::default()
            },
        );
        let (lon, lat) = place(14, 9000, 6200, 0.4375, 0.4375);
        let chord = flat.height_at(lon, lat).expect("ground");
        let smoothed = round.height_at(lon, lat).expect("ground");
        assert!(
            (smoothed - chord).abs() > 0.1 && (smoothed - chord).abs() <= f64::from(bound) + 0.05,
            "chord {chord}, smoothed {smoothed}"
        );
    }

    /// A coarse tile beside two fine ones 5 m higher: stitched, the fine
    /// tiles' edge comes down to the coarse one, and the ground read on it
    /// is the stitched one.
    #[test]
    fn the_height_is_the_stitched_surface_s_along_a_seam() {
        let coarse = tile(14, 9000, 6200, 4, |_, _| 100.0);
        let south = tile(15, 18002, 12400, 4, |_, _| 105.0);
        let north = tile(15, 18002, 12401, 4, |_, _| 105.0);
        // On the fine tiles' western edge, which meets the coarse tile.
        let (lon, lat) = place(15, 18002, 12400, 0.0, 0.5);
        let alone = ground_of(&[south.clone(), north.clone()], Prepare::default());
        let unsewn = alone.height_at(lon + 1.0e-9, lat).expect("ground");
        assert!((unsewn - 105.0).abs() < 0.05, "{unsewn}");
        let sewn = ground_of(&[coarse, south, north], Prepare::default());
        let at_seam = sewn.height_at(lon + 1.0e-9, lat).expect("ground");
        assert!(
            at_seam < 101.0,
            "the fine edge brought down to the coarse ground: {at_seam}"
        );
        // Inside the fine tile, past the stitching band: its own height.
        let (lon, lat) = place(15, 18002, 12400, 0.6, 0.5);
        let inside = sewn.height_at(lon, lat).expect("ground");
        assert!((inside - 105.0).abs() < 0.05, "{inside}");
    }
}
