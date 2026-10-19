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
        let (top, down) = line_down(lon, lat);
        self.tiles
            .iter()
            .filter(|t| t.holds(lon, lat))
            .filter_map(|t| first_hit(&t.positions, &t.indices, top - t.origin, down))
            .map(|along| ABOVE_M - along)
            .reduce(f64::max)
    }
}

/// How far outside its rectangle a place still reads a tile, in radians: a
/// place on the line two tiles share reads both.
const ON: f64 = 1.0e-12;

impl Drawn {
    /// Whether the place `lon`, `lat` reads this tile.
    fn holds(&self, lon: f64, lat: f64) -> bool {
        lon >= self.rect.west - ON
            && lon <= self.rect.east + ON
            && lat >= self.rect.south - ON
            && lat <= self.rect.north + ON
    }
}

/// The vertical line down the place `lon`, `lat`: where it starts, above
/// any ground, and which way is down — earth-centred.
fn line_down(lon: f64, lat: f64) -> (DVec3, DVec3) {
    let top = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: ABOVE_M,
    });
    let down = -DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
    (top, down)
}

/// How far along `dir` from `from` the ray first meets one of the
/// triangles `indices` makes of `positions` (both sides), in their frame.
fn first_hit(positions: &[[f32; 3]], indices: &[u32], from: DVec3, dir: DVec3) -> Option<f64> {
    indices
        .as_chunks::<3>()
        .0
        .iter()
        .filter_map(|t| hit(positions, t, from, dir))
        .reduce(f64::min)
}

/// How far along `dir` from `from` the ray meets the triangle `t` makes of
/// `positions` (either side), in their frame: Möller–Trumbore.
fn hit(positions: &[[f32; 3]], t: &[u32; 3], from: DVec3, dir: DVec3) -> Option<f64> {
    let p = |i: u32| DVec3::from(positions[i as usize].map(f64::from));
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
    if u < EDGE || v < EDGE || u + v > 1.0 - EDGE {
        return None;
    }
    let along = e2.dot(q) / det;
    (along >= 0.0).then_some(along)
}

/// A hair over a triangle's edges, in its own barycentric coordinates: the
/// vertices are f32 about the tile's origin, so a line down a corner can
/// miss both triangles there by a fraction of a millimetre.
const EDGE: f64 = -1.0e-6;

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

    /// A deterministic stream of numbers (splitmix64): the same places
    /// every run, so a mismatch found once is found again.
    struct Dice(u64);

    impl Dice {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }

        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.next()
        }

        fn pick<T: Copy>(&mut self, of: &[T]) -> T {
            of[((self.next() * of.len() as f64) as usize).min(of.len() - 1)]
        }
    }

    /// The height under a place read by brute force: every tile whose
    /// rectangle holds it, every triangle of each. What the ground has to
    /// answer, bit for bit, however it finds the triangles.
    fn brute_force(ground: &Ground, lon: f64, lat: f64) -> Option<f64> {
        let (top, down) = line_down(lon, lat);
        ground
            .tiles
            .iter()
            .filter(|t| t.holds(lon, lat))
            .filter_map(|t| first_hit(&t.positions, &t.indices, top - t.origin, down))
            .map(|along| ABOVE_M - along)
            .reduce(f64::max)
    }

    /// Metres over the ellipsoid at `lon`, `lat` (radians): hills a couple
    /// of kilometres across and 120 m high, smaller ones on them, and a
    /// cliff — the slopes a terrain source's triangles take.
    fn relief(lon: f64, lat: f64) -> f64 {
        let (x, y) = (lon * 6.378e6 * 0.6, lat * 6.378e6);
        let tau = std::f64::consts::TAU;
        let cliff = if (x / 900.0).sin() > 0.7 { 60.0 } else { 0.0 };
        300.0
            + 120.0 * (tau * x / 1700.0).sin() * (tau * y / 2300.0).cos()
            + 35.0 * (tau * x / 430.0 + 1.0).sin() * (tau * y / 510.0).sin()
            + cliff
    }

    /// Tile `level/x/y` as a terrain source states one and a renderer gets
    /// it: an irregular network of `n` spacings a side — every vertex but
    /// the corners moved at random, along its side on a side; each cell cut
    /// along a diagonal chosen at random, slivers and all — over
    /// [`relief`], decoded with its skirts by the one way a terrain tile
    /// becomes ground (`tuile_terrain::mesh::to_ground`).
    fn tin(level: u32, x: u64, y: u64, n: usize, dice: &mut Dice) -> (TileKey, Mesh) {
        use tuile_terrain::{Header, QuantizedMesh};
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y));
        let m = n + 1;
        let (mut u, mut v, mut metres) = (Vec::new(), Vec::new(), Vec::new());
        for j in 0..m {
            for i in 0..m {
                let mut at = |k: usize| {
                    let moved = if k == 0 || k == n {
                        0.0
                    } else {
                        dice.between(-0.3, 0.3)
                    };
                    (k as f64 + moved) / n as f64
                };
                let (fu, fv) = (at(i), at(j));
                u.push(fu);
                v.push(fv);
                metres.push(relief(
                    rect.west + fu * rect.width(),
                    rect.south + fv * rect.height(),
                ));
            }
        }
        let mut indices = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let a = (j * m + i) as u32;
                let (b, c, d) = (a + 1, a + m as u32, a + m as u32 + 1);
                if dice.next() < 0.5 {
                    indices.extend_from_slice(&[a, b, c, c, b, d]);
                } else {
                    indices.extend_from_slice(&[a, b, d, a, d, c]);
                }
            }
        }
        let low = metres.iter().copied().fold(f64::INFINITY, f64::min);
        let high = metres.iter().copied().fold(f64::NEG_INFINITY, f64::max) + 1.0;
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: (low + high) / 2.0,
        });
        let line = |f: &dyn Fn(usize) -> usize| (0..m).map(|k| f(k) as u32).collect::<Vec<u32>>();
        let source = QuantizedMesh {
            header: Header {
                center: centre.to_array(),
                min_height: low as f32,
                max_height: high as f32,
                bounding_sphere_center: centre.to_array(),
                bounding_sphere_radius: rect.height() * 6.4e6,
                horizon_occlusion: [0.0; 3],
            },
            u,
            v,
            height: metres
                .iter()
                .map(|h| (h - low) / (f64::from(high as f32) - f64::from(low as f32)))
                .collect(),
            indices,
            normals: None,
            edges: [
                line(&|k| k * m),
                line(&|k| k),
                line(&|k| k * m + n),
                line(&|k| (m - 1) * m + k),
            ],
            metadata_available: None,
            cut: 0,
        };
        let content = tuile_terrain::mesh::to_ground(&source, &rect);
        (
            TileKey {
                id: TileId::from_terrain(level, x, y).0,
                drape: 0,
            },
            encoded(
                &content.meshes[0],
                content.local_origin_ecef.to_array(),
                [1.0; 4],
            ),
        )
    }

    /// The tiles of a frame over [`relief`], as a selection holds them: a
    /// coarse irregular one; east of it four of the next level, finer and
    /// irregular, whose seams with it and with each other are stitched;
    /// north of those a regular 64 × 64 grid and an irregular 64 × 64 one.
    fn landscape(dice: &mut Dice) -> Vec<(TileKey, Mesh)> {
        let rough = |u: f64, north: f64| {
            let (lon, lat) = place(14, 9000, 6201, u, north);
            relief(lon, lat)
        };
        vec![
            tin(14, 9000, 6200, 40, dice),
            tin(15, 18002, 12400, 32, dice),
            tin(15, 18003, 12400, 24, dice),
            tin(15, 18002, 12401, 28, dice),
            tin(15, 18003, 12401, 32, dice),
            tile(14, 9000, 6201, 64, rough),
            tin(14, 9001, 6201, 64, dice),
        ]
    }

    /// The places a height is asked at over `ground`: anywhere around it,
    /// and wherever finding the triangles could go wrong — every vertex,
    /// and a hair off it; along the tiles' sides, on them, inside and
    /// outside the hair a side is read within; across the edges of the
    /// triangles, on them and on either side of the hair a triangle is
    /// widened by.
    fn places(ground: &Ground, dice: &mut Dice) -> Vec<(f64, f64)> {
        use tuile_core::geo::ecef_to_geodetic;
        let mut out = Vec::new();
        let (mut west, mut south, mut east, mut north) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for t in &ground.tiles {
            west = west.min(t.rect.west);
            south = south.min(t.rect.south);
            east = east.max(t.rect.east);
            north = north.max(t.rect.north);
        }
        let (w, h) = (east - west, north - south);
        for _ in 0..4000 {
            out.push((
                dice.between(west - 0.05 * w, east + 0.05 * w),
                dice.between(south - 0.05 * h, north + 0.05 * h),
            ));
        }
        let hairs = [
            0.0,
            1.0e-15,
            -1.0e-15,
            5.0e-13,
            -5.0e-13,
            ON,
            -ON,
            1.0001e-12,
            -1.0001e-12,
            3.0e-12,
            -3.0e-12,
        ];
        for t in &ground.tiles {
            let r = t.rect;
            for _ in 0..120 {
                let (lon, lat) = (dice.between(r.west, r.east), dice.between(r.south, r.north));
                let side = dice.pick(&[r.west, r.east]);
                out.push((side + dice.pick(&hairs), lat));
                let side = dice.pick(&[r.south, r.north]);
                out.push((lon, side + dice.pick(&hairs)));
            }
            for corner in [
                (r.west, r.south),
                (r.east, r.south),
                (r.west, r.north),
                (r.east, r.north),
            ] {
                for _ in 0..4 {
                    out.push((corner.0 + dice.pick(&hairs), corner.1 + dice.pick(&hairs)));
                }
            }
            let at = |p: DVec3| {
                let g = ecef_to_geodetic(t.origin + p);
                (g.lon, g.lat)
            };
            let vertex = |i: u32| DVec3::from(t.positions[i as usize].map(f64::from));
            for _ in 0..400 {
                let p = dice.pick(&t.positions);
                let (lon, lat) = at(DVec3::from(p.map(f64::from)));
                out.push((lon, lat));
                out.push((lon + dice.pick(&hairs), lat + dice.pick(&hairs)));
            }
            let triangles = t.indices.as_chunks::<3>().0;
            // On an edge, and a whisker either side of the hair `EDGE`
            // widens a triangle by: the barycentric coordinates the hit
            // test reads, set where it decides.
            let rims = [0.0, EDGE, EDGE * 0.999, EDGE * 1.001, EDGE * 2.0, 1.0e-9];
            for _ in 0..300 {
                let tri = dice.pick(triangles);
                let (a, b, c) = (vertex(tri[0]), vertex(tri[1]), vertex(tri[2]));
                let along = dice.next();
                let rim = dice.pick(&rims);
                let (u, v) = match dice.pick(&[0, 1, 2]) {
                    0 => (rim, along),
                    1 => (along, rim),
                    _ => (along * (1.0 - rim), (1.0 - along) * (1.0 - rim)),
                };
                out.push(at(a + (b - a) * u + (c - a) * v));
            }
        }
        out
    }

    /// Wherever it is asked, the ground answers exactly what reading every
    /// triangle of every tile there answers: the same `Option`, the same
    /// bits — on irregular and regular tiles, with skirts and without,
    /// smoothed or not, stitched across levels or not.
    #[test]
    fn the_height_is_every_triangle_s_answer_bit_for_bit() {
        let mut dice = Dice(0x5EED_6E0D);
        let tiles = landscape(&mut dice);
        let selection: Vec<TileKey> = tiles.iter().map(|(k, _)| *k).collect();
        let grounds = [
            (
                "as the render service draws it",
                Prepare {
                    skirts: true,
                    smooth: Smooth::Geometry {
                        bound: smoothing::DEFAULT_BOUND_M,
                    },
                },
                true,
            ),
            ("skirts dropped, unstitched", Prepare::default(), false),
            (
                "skirts kept, stitched",
                Prepare {
                    skirts: true,
                    smooth: Smooth::Off,
                },
                true,
            ),
        ];
        for (name, how, stitch) in grounds {
            let mut stitching = Stitching::default();
            for (key, mesh) in &tiles {
                stitching.enter(*key, None, &prepared(mesh.clone(), how).0);
            }
            let ground = if stitch {
                stitching.frame(&selection);
                Ground::of(&stitching, &selection)
            } else {
                Ground::unstitched(&stitching, &selection)
            };
            assert_eq!(ground.len(), tiles.len());
            let (mut asked, mut found, mut differ) = (0usize, 0usize, Vec::new());
            for (lon, lat) in places(&ground, &mut dice) {
                let (indexed, brute) = (ground.height_at(lon, lat), brute_force(&ground, lon, lat));
                asked += 1;
                found += usize::from(brute.is_some());
                if indexed.map(f64::to_bits) != brute.map(f64::to_bits) {
                    differ.push((lon, lat, indexed, brute));
                }
            }
            assert!(found > asked / 2, "{name}: {found} of {asked} found ground");
            assert!(
                differ.is_empty(),
                "{name}: {} of {asked} places differ, first {:?}",
                differ.len(),
                &differ[..differ.len().min(5)]
            );
        }
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
