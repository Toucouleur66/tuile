// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Terrain made from a height field: a relief known as a function of
//! longitude and latitude, served as quantized-mesh tiles.
//!
//! - [`Heights`]: the relief — metres above the WGS84 ellipsoid at a point,
//!   zero outside its footprint.
//! - [`Grid`]: a relief held as a regular grid of samples, read bilinearly,
//!   and box-filtered where a tile's vertices are farther apart than its
//!   samples so that a coarse mesh does not alias.
//! - [`HeightfieldSource`]: the [`TerrainSource`] that cuts a relief into
//!   the tiles of the geographic quadtree, each a regular grid of vertices
//!   with its full header ([`Header::for_vertices`]), its four edges and its
//!   oct-encoded normals. Around the footprint it answers the
//!   [`Ellipsoid`]'s tiles, so that a client always has ground to draw.
//!
//! **Shared edges are shared samples.** A vertex's longitude and latitude
//! are a function of its level and its index in that level's global vertex
//! grid, never of the tile it is in, so two neighbouring tiles ask the
//! relief for the very same points along their common edge and get
//! bit-identical heights — and, from the same neighbours, the same normals.
//! The quantization of each tile to its own height range still differs by
//! up to a 65 534th of that range; the skirts a client hangs from the edges
//! cover it.
//!
//! Usable outside any viewer: a server bakes the whole pyramid by walking
//! [`HeightfieldSource::tiles`] and writing [`HeightfieldSource::tile`]
//! beside [`HeightfieldSource::layer`].

use glam::DVec3;
use tuile_core::fetch::Fetched;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic, WGS84_A};

use crate::decode::{encode, Header};
use crate::ellipsoid::{Ellipsoid, ELLIPSOID_LEVELS};
use crate::layer::{AvailabilityRange, LayerJson};
use crate::source::{TerrainSource, TerrainSourceError};
use crate::tiling::{GeoRect, GeographicTilingScheme, TileCoord};

/// A relief: heights in metres above the WGS84 ellipsoid.
pub trait Heights: Send + Sync {
    /// The height at a point, longitude and latitude in **degrees**.
    fn height(&self, lon_deg: f64, lat_deg: f64) -> f64;

    /// The rectangle (radians) outside which every height is zero: the
    /// tiles a [`HeightfieldSource`] makes are those covering it.
    fn footprint(&self) -> GeoRect;

    /// The mean height over the box `span_lon_deg × span_lat_deg` centred on
    /// the point: what a vertex standing for that much ground should have.
    /// The default samples the centre only — right for a relief smooth at
    /// the scale of the box; a sampled one overrides it ([`Grid`]).
    fn mean_height(&self, lon_deg: f64, lat_deg: f64, span_lon_deg: f64, span_lat_deg: f64) -> f64 {
        let _ = (span_lon_deg, span_lat_deg);
        self.height(lon_deg, lat_deg)
    }
}

impl<H: Heights + ?Sized> Heights for std::sync::Arc<H> {
    fn height(&self, lon_deg: f64, lat_deg: f64) -> f64 {
        (**self).height(lon_deg, lat_deg)
    }
    fn footprint(&self) -> GeoRect {
        (**self).footprint()
    }
    fn mean_height(&self, lon_deg: f64, lat_deg: f64, span_lon_deg: f64, span_lat_deg: f64) -> f64 {
        (**self).mean_height(lon_deg, lat_deg, span_lon_deg, span_lat_deg)
    }
}

/// Why a [`Grid`] could not be made.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GridError {
    #[error("a grid needs at least 2×2 samples, not {columns}×{rows}")]
    TooSmall { columns: usize, rows: usize },
    #[error("{columns}×{rows} samples, but {len} heights")]
    Length {
        columns: usize,
        rows: usize,
        len: usize,
    },
    #[error("a grid's footprint must have an area")]
    EmptyFootprint,
}

/// A relief held as a regular grid of samples, in metres above the
/// ellipsoid.
///
/// Row-major, **row 0 along the north edge**, column 0 along the west edge,
/// as an image is: sample `(column, row)` lies at
/// `west + column · width / (columns − 1)`,
/// `north − row · height / (rows − 1)` — the first and last samples on the
/// footprint's edges. Outside the footprint the height is zero.
#[derive(Debug, Clone)]
pub struct Grid {
    columns: usize,
    rows: usize,
    heights: Vec<f32>,
    footprint: GeoRect,
    /// The footprint in degrees: what the samples are found by.
    west: f64,
    north: f64,
    /// Degrees between two samples.
    step_lon: f64,
    step_lat: f64,
}

impl Grid {
    /// `columns × rows` heights (metres), row 0 the northernmost, over
    /// `footprint` (radians).
    pub fn new(
        columns: usize,
        rows: usize,
        heights: Vec<f32>,
        footprint: GeoRect,
    ) -> Result<Self, GridError> {
        if columns < 2 || rows < 2 {
            return Err(GridError::TooSmall { columns, rows });
        }
        if heights.len() != columns * rows {
            return Err(GridError::Length {
                columns,
                rows,
                len: heights.len(),
            });
        }
        if !(footprint.east > footprint.west && footprint.north > footprint.south) {
            return Err(GridError::EmptyFootprint);
        }
        Ok(Self {
            columns,
            rows,
            heights,
            footprint,
            west: footprint.west.to_degrees(),
            north: footprint.north.to_degrees(),
            step_lon: footprint.width().to_degrees() / (columns - 1) as f64,
            step_lat: footprint.height().to_degrees() / (rows - 1) as f64,
        })
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The sample at `(column, row)`, or zero off the grid.
    fn at(&self, column: i64, row: i64) -> f64 {
        if column < 0 || row < 0 || column >= self.columns as i64 || row >= self.rows as i64 {
            return 0.0;
        }
        f64::from(self.heights[row as usize * self.columns + column as usize])
    }

    /// Where a point falls among the samples, in fractional columns and rows.
    fn position(&self, lon_deg: f64, lat_deg: f64) -> (f64, f64) {
        (
            (lon_deg - self.west) / self.step_lon,
            (self.north - lat_deg) / self.step_lat,
        )
    }
}

impl Heights for Grid {
    /// Bilinear between the four samples around the point; zero outside
    /// the footprint.
    fn height(&self, lon_deg: f64, lat_deg: f64) -> f64 {
        let (c, r) = self.position(lon_deg, lat_deg);
        let last_column = (self.columns - 1) as f64;
        let last_row = (self.rows - 1) as f64;
        if !(0.0..=last_column).contains(&c) || !(0.0..=last_row).contains(&r) {
            return 0.0;
        }
        // The cell's corner, kept inside the grid so the last sample is
        // reached with a weight of one rather than read past.
        let c0 = c.floor().min(last_column - 1.0);
        let r0 = r.floor().min(last_row - 1.0);
        let (fc, fr) = (c - c0, r - r0);
        let (c0, r0) = (c0 as i64, r0 as i64);
        let top = self.at(c0, r0) * (1.0 - fc) + self.at(c0 + 1, r0) * fc;
        let bottom = self.at(c0, r0 + 1) * (1.0 - fc) + self.at(c0 + 1, r0 + 1) * fc;
        top * (1.0 - fr) + bottom * fr
    }

    fn footprint(&self) -> GeoRect {
        self.footprint
    }

    /// Bilinear where the box is no wider than a sample spacing; otherwise
    /// the mean of the samples inside it, those off the grid counting as
    /// zero — so a coarse vertex carries its ground's mean height, not
    /// whichever sample it happens to land on.
    fn mean_height(&self, lon_deg: f64, lat_deg: f64, span_lon_deg: f64, span_lat_deg: f64) -> f64 {
        let across = span_lon_deg / self.step_lon;
        let down = span_lat_deg / self.step_lat;
        if across <= 1.0 && down <= 1.0 {
            return self.height(lon_deg, lat_deg);
        }
        let (c, r) = self.position(lon_deg, lat_deg);
        // The lattice points inside the box, the grid extended past its
        // edges with zeros: their count is the box's whatever lies under it.
        let span = |centre: f64, width: f64| {
            let half = width.max(1.0) / 2.0;
            (
                (centre - half).ceil() as i64,
                (centre + half).floor() as i64,
            )
        };
        let (c_first, c_last) = span(c, across);
        let (r_first, r_last) = span(r, down);
        let count = (c_last - c_first + 1).max(0) * (r_last - r_first + 1).max(0);
        if count == 0 {
            return self.height(lon_deg, lat_deg);
        }
        let mut sum = 0.0;
        for row in r_first.max(0)..=r_last.min(self.rows as i64 - 1) {
            for column in c_first.max(0)..=c_last.min(self.columns as i64 - 1) {
                sum += self.at(column, row);
            }
        }
        sum / count as f64
    }
}

/// What a [`HeightfieldSource`] answers for a tile outside its footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outside {
    /// The [`Ellipsoid`]'s tile, down to [`ELLIPSOID_LEVELS`], and no tile
    /// below: ground everywhere around the relief.
    #[default]
    Ellipsoid,
    /// No tile at all.
    NoSuchTile,
}

/// Vertices along a tile's side unless told otherwise: 76 spacings, about
/// 8 m apart in a level-15 tile at the equator.
pub const DEFAULT_VERTICES_PER_SIDE: usize = 77;

/// A relief served as quantized-mesh terrain.
///
/// Every tile covering any part of the footprint down to `max_level` is a
/// regular grid of `vertices_per_side²` vertices sampled from the relief;
/// below `max_level` there is no tile, and a client refines by cutting the
/// deepest one. Outside the footprint, what [`Outside`] says.
pub struct HeightfieldSource<H> {
    pub heights: H,
    /// The deepest level of the relief's tiles.
    pub max_level: u32,
    /// Vertices along a tile's side (at least 2).
    pub vertices_per_side: usize,
    pub outside: Outside,
}

impl<H: Heights> HeightfieldSource<H> {
    /// `heights` down to `max_level`, [`DEFAULT_VERTICES_PER_SIDE`] a side,
    /// the ellipsoid around it.
    pub fn new(heights: H, max_level: u32) -> Self {
        Self {
            heights,
            max_level,
            vertices_per_side: DEFAULT_VERTICES_PER_SIDE,
            outside: Outside::default(),
        }
    }

    /// Whether tile `coord` is one of the relief's own: no deeper than
    /// `max_level`, and covering part of the footprint.
    pub fn covers(&self, coord: TileCoord) -> bool {
        coord.level <= self.max_level
            && AvailabilityRange::covering(self.heights.footprint(), coord.level)
                .is_some_and(|r| r.contains(coord.x, coord.y))
    }

    /// Tile `coord`'s quantized mesh — the relief's, the ellipsoid's, or
    /// [`TerrainSourceError::no_such_tile`].
    pub fn tile(&self, coord: TileCoord) -> Result<Vec<u8>, TerrainSourceError> {
        let scheme = GeographicTilingScheme::default();
        let absent = || {
            TerrainSourceError::no_such_tile(format!(
                "{}/{}/{} is not served",
                coord.level, coord.x, coord.y
            ))
        };
        if coord.level > self.max_level
            || coord.x >= scheme.tiles_x(coord.level)
            || coord.y >= scheme.tiles_y(coord.level)
        {
            return Err(absent());
        }
        if self.covers(coord) {
            return Ok(self.relief_tile(coord));
        }
        match self.outside {
            Outside::Ellipsoid if coord.level <= ELLIPSOID_LEVELS => Ok(Ellipsoid::tile(coord)),
            _ => Err(absent()),
        }
    }

    /// The `layer.json` the tiles are served under: the footprint's tiles to
    /// `max_level`, with oct-encoded normals, and every tile down to the
    /// ellipsoid's levels when the ellipsoid surrounds it.
    pub fn layer(&self) -> LayerJson {
        let mut layer = LayerJson::for_footprint(
            self.heights.footprint(),
            self.max_level,
            &["octvertexnormals"],
        );
        if self.outside == Outside::Ellipsoid {
            let everywhere = Ellipsoid::layer();
            for (level, ranges) in layer.available.iter_mut().enumerate() {
                if let Some(all) = everywhere.available.get(level) {
                    ranges.clone_from(all);
                }
            }
        }
        layer
    }

    /// Every tile of the relief, from the root down to `max_level`: what a
    /// server bakes. The ellipsoid's tiles around it are not listed — there
    /// are hundreds of millions of them, each made in microseconds by
    /// [`Self::tile`] when asked for.
    pub fn tiles(&self) -> impl Iterator<Item = TileCoord> + '_ {
        let footprint = self.heights.footprint();
        (0..=self.max_level).flat_map(move |level| {
            AvailabilityRange::covering(footprint, level)
                .into_iter()
                .flat_map(move |r| {
                    (r.start_y..=r.end_y)
                        .flat_map(move |y| (r.start_x..=r.end_x).map(move |x| (x, y)))
                })
                .map(move |(x, y)| TileCoord::new(level, x, y))
        })
    }

    /// The heights of tile `coord`'s vertices and of a ring of one more
    /// vertex around them, the neighbours its edge normals are taken from.
    fn samples(&self, coord: TileCoord) -> Samples {
        let n = self.vertices_per_side.max(2);
        let steps = (n - 1) as u64;
        let scheme = GeographicTilingScheme::default();
        // The level's global vertex grid: a vertex's place is its index in
        // it, whichever tile asks.
        let across = scheme.tiles_x(coord.level) * steps;
        let down = scheme.tiles_y(coord.level) * steps;
        let span_lon = 360.0 / across as f64;
        let span_lat = 180.0 / down as f64;
        let lon = |g: i64| (g as f64 * span_lon - 180.0).clamp(-180.0, 180.0);
        let lat = |g: i64| (g as f64 * span_lat - 90.0).clamp(-90.0, 90.0);
        let (x0, y0) = ((coord.x * steps) as i64, (coord.y * steps) as i64);
        let side = n + 2;
        let mut points = Vec::with_capacity(side * side);
        for j in -1..=n as i64 {
            for i in -1..=n as i64 {
                let (lo, la) = (lon(x0 + i), lat(y0 + j));
                let h = self.heights.mean_height(lo, la, span_lon, span_lat);
                points.push((lo, la, h));
            }
        }
        Samples {
            n,
            points,
            span_lon,
            span_lat,
        }
    }

    /// A tile of the relief: its grid of vertices, header, edges, normals.
    fn relief_tile(&self, coord: TileCoord) -> Vec<u8> {
        let samples = self.samples(coord);
        let n = samples.n;
        let steps = (n - 1) as f64;
        let count = n * n;
        let (mut lowest, mut highest) = (f64::INFINITY, f64::NEG_INFINITY);
        for j in 0..n {
            for i in 0..n {
                let h = samples.get(i as i64, j as i64).2;
                lowest = lowest.min(h);
                highest = highest.max(h);
            }
        }
        // The header's f32 bounds hold every height: rounded outwards.
        let mut min_height = lowest as f32;
        if f64::from(min_height) > lowest {
            min_height = min_height.next_down();
        }
        let mut max_height = highest as f32;
        if f64::from(max_height) < highest {
            max_height = max_height.next_up();
        }
        let range = f64::from(max_height) - f64::from(min_height);

        let mut u = Vec::with_capacity(count);
        let mut v = Vec::with_capacity(count);
        let mut height = Vec::with_capacity(count);
        let mut ecef = Vec::with_capacity(count);
        let mut normals = Vec::with_capacity(count);
        for j in 0..n {
            for i in 0..n {
                let (lon, lat, h) = samples.get(i as i64, j as i64);
                u.push(i as f64 / steps);
                v.push(j as f64 / steps);
                height.push(if range > 0.0 {
                    ((h - f64::from(min_height)) / range).clamp(0.0, 1.0)
                } else {
                    0.0
                });
                ecef.push(geodetic_to_ecef(Geodetic {
                    lon: lon.to_radians(),
                    lat: lat.to_radians(),
                    height: h,
                }));
                normals.push(samples.normal(i as i64, j as i64));
            }
        }
        let header = Header::for_vertices(&ecef, min_height, max_height);
        let grid = GridTopology::new(n);
        let u = grid.reorder(&u);
        let v = grid.reorder(&v);
        let height = grid.reorder(&height);
        let normals = grid.reorder(&normals);
        encode(
            &header,
            &u,
            &v,
            &height,
            &grid.indices,
            Some(&normals),
            &grid.edges,
        )
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl<H: Heights> TerrainSource for HeightfieldSource<H> {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        self.tile(coord).map(Fetched::undated)
    }
}

/// A tile's sampled vertices and the ring around them: `(lon°, lat°, h)`,
/// row-major from the south-west, indices from −1 to `n`.
struct Samples {
    n: usize,
    points: Vec<(f64, f64, f64)>,
    span_lon: f64,
    span_lat: f64,
}

impl Samples {
    fn get(&self, i: i64, j: i64) -> (f64, f64, f64) {
        let side = (self.n + 2) as i64;
        self.points[((j + 1) * side + i + 1) as usize]
    }

    /// The unit normal (ECEF) at vertex `(i, j)`: the slope of the heights
    /// either side of it, east and north, in its local frame.
    fn normal(&self, i: i64, j: i64) -> [f32; 3] {
        let (lon, lat, _) = self.get(i, j);
        let (lon, lat) = (lon.to_radians(), lat.to_radians());
        let east_metres = 2.0 * self.span_lon.to_radians() * WGS84_A * lat.cos();
        let north_metres = 2.0 * self.span_lat.to_radians() * WGS84_A;
        let rise_east = self.get(i + 1, j).2 - self.get(i - 1, j).2;
        let rise_north = self.get(i, j + 1).2 - self.get(i, j - 1).2;
        // At a pole the east step is no distance: no slope that way.
        let slope_east = if east_metres > 1e-9 {
            rise_east / east_metres
        } else {
            0.0
        };
        let slope_north = rise_north / north_metres;
        let local = DVec3::new(-slope_east, -slope_north, 1.0).normalize();
        let world = enu_frame(Geodetic {
            lon,
            lat,
            height: 0.0,
        }) * local;
        world.as_vec3().to_array()
    }
}

/// The triangles and edges of an `n × n` grid of vertices, numbered in the
/// order its triangles first name them — the high-water-mark coding of
/// quantized-mesh indices requires it.
pub(crate) struct GridTopology {
    pub indices: Vec<u32>,
    pub edges: [Vec<u32>; 4],
    /// `order[row-major index] = vertex number`.
    order: Vec<u32>,
}

impl GridTopology {
    /// Vertices row-major from the south-west; two triangles a cell.
    pub fn new(n: usize) -> Self {
        let steps = n - 1;
        let mut indices = Vec::with_capacity(steps * steps * 6);
        for j in 0..steps {
            for i in 0..steps {
                let a = (j * n + i) as u32;
                let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
                indices.extend_from_slice(&[a, b, c, c, b, d]);
            }
        }
        let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
        let edges = [
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + steps),
            line(&|k| (n - 1) * n + k),
        ];
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
        let edges = edges.map(|e| e.into_iter().map(|i| order[i as usize]).collect());
        Self {
            indices,
            edges,
            order,
        }
    }

    /// Per-vertex values, row-major, put in vertex-number order.
    pub fn reorder<T: Copy + Default>(&self, values: &[T]) -> Vec<T> {
        let mut out = vec![T::default(); values.len()];
        for (old, &new) in self.order.iter().enumerate() {
            out[new as usize] = values[old];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{decode, QuantizedMesh};
    use tuile_core::geo::WGS84_B;

    /// 16 km on the equator, east of the prime meridian.
    fn footprint() -> GeoRect {
        GeoRect::from_degrees(0.01, -0.072, 0.154, 0.072)
    }

    /// A deterministic relief over the footprint: ridges and a peak, from 0
    /// to about 1 800 m, 257 samples a side (about 62 m apart).
    fn relief() -> Grid {
        let side = 257;
        let mut heights = Vec::with_capacity(side * side);
        for row in 0..side {
            for column in 0..side {
                let (x, y) = (
                    column as f64 / (side - 1) as f64,
                    row as f64 / (side - 1) as f64,
                );
                let ridges = 400.0 * (x * 23.0).sin() * (y * 17.0).cos();
                let peak = 1200.0 * (-((x - 0.4).powi(2) + (y - 0.6).powi(2)) * 30.0).exp();
                heights.push((600.0 + ridges + peak) as f32);
            }
        }
        Grid::new(side, side, heights, footprint()).expect("grid")
    }

    fn source() -> HeightfieldSource<Grid> {
        HeightfieldSource::new(relief(), 15)
    }

    /// A tile of `level` inside the footprint, away from its edges.
    fn inside(level: u32) -> TileCoord {
        tile_at(level, 0.07, 0.01)
    }

    /// The tile of `level` holding a point (degrees).
    fn tile_at(level: u32, lon_deg: f64, lat_deg: f64) -> TileCoord {
        let (lon, lat) = (lon_deg.to_radians(), lat_deg.to_radians());
        let s = GeographicTilingScheme::default();
        let x = ((lon + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
            * s.tiles_x(level) as f64) as u64;
        let y = ((lat + std::f64::consts::FRAC_PI_2) / std::f64::consts::PI
            * s.tiles_y(level) as f64) as u64;
        TileCoord::new(level, x, y)
    }

    /// The decoded vertices, back in ECEF.
    fn positions(mesh: &QuantizedMesh, coord: TileCoord) -> Vec<DVec3> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let (lo, hi) = (
            f64::from(mesh.header.min_height),
            f64::from(mesh.header.max_height),
        );
        (0..mesh.vertex_count())
            .map(|k| {
                geodetic_to_ecef(Geodetic {
                    lon: rect.west + mesh.u[k] * rect.width(),
                    lat: rect.south + mesh.v[k] * rect.height(),
                    height: lo + mesh.height[k] * (hi - lo),
                })
            })
            .collect()
    }

    /// Whether `q` is hidden from `eye` by the unit sphere (both in the
    /// ellipsoid-scaled frame): the segment between them passes through it.
    fn hidden(eye: DVec3, q: DVec3) -> bool {
        let d = q - eye;
        let (a, b, c) = (d.dot(d), 2.0 * eye.dot(d), eye.dot(eye) - 1.0);
        let disc = b * b - 4.0 * a * c;
        if disc <= 0.0 {
            return false;
        }
        let t = (-b - disc.sqrt()) / (2.0 * a);
        t > 1e-12 && t < 1.0 - 1e-9
    }

    /// The tile decodes to the grid it was made of: every vertex, its height
    /// within the quantization, the header bounding every height, the
    /// sphere every vertex, edges of a full side each, a normal a vertex.
    #[test]
    fn a_relief_tile_round_trips_with_a_header_that_holds_it() {
        let source = source();
        for level in [4u32, 9, 12, 15] {
            let coord = inside(level);
            let bytes = source.tile(coord).expect("covered");
            let mesh = decode(&bytes).expect("decodes");
            let n = DEFAULT_VERTICES_PER_SIDE;
            assert_eq!(mesh.vertex_count(), n * n, "{coord:?}");
            assert_eq!(mesh.indices.len(), (n - 1) * (n - 1) * 6);
            assert!(mesh.edges.iter().all(|e| e.len() == n));
            assert_eq!(mesh.normals.as_ref().map(Vec::len), Some(n * n));

            let samples = source.samples(coord);
            let (lo, hi) = (mesh.header.min_height, mesh.header.max_height);
            assert!(hi > lo + 1.0, "{coord:?} has relief: {lo}..{hi}");
            let quantum = f64::from(hi - lo) / 32767.0;
            let mut exact = Vec::new();
            for j in 0..n {
                for i in 0..n {
                    let (lon, lat, h) = samples.get(i as i64, j as i64);
                    assert!(f64::from(lo) <= h && h <= f64::from(hi));
                    exact.push(geodetic_to_ecef(Geodetic {
                        lon: lon.to_radians(),
                        lat: lat.to_radians(),
                        height: h,
                    }));
                }
            }
            // The south-west corner is vertex 0 in both orders.
            let (_, _, h0) = samples.get(0, 0);
            let decoded0 = f64::from(lo) + mesh.height[0] * f64::from(hi - lo);
            assert!((decoded0 - h0).abs() <= quantum, "{decoded0} vs {h0}");

            let centre = DVec3::from_array(mesh.header.bounding_sphere_center);
            let radius = mesh.header.bounding_sphere_radius;
            for p in &exact {
                assert!(p.distance(centre) <= radius + 1e-6, "{coord:?}");
            }
            // Decoded positions move by a quantum of the tile at most.
            let rect = GeographicTilingScheme::default().tile_rect(coord);
            let slack = rect.width() * WGS84_A / 32767.0 + quantum;
            for p in positions(&mesh, coord) {
                assert!(p.distance(centre) <= radius + slack, "{coord:?}");
            }
            assert_eq!(mesh.header.center, mesh.header.bounding_sphere_center);
        }
    }

    /// The horizon occlusion point keeps its promise: from wherever it is
    /// below the horizon, so is every vertex of the tile. Brute force over
    /// eyes around the tile, near the ground and far out; and the promise
    /// is not empty — the point is hidden from some of them.
    #[test]
    fn whatever_hides_the_horizon_point_hides_the_tile() {
        let source = source();
        let radii = DVec3::new(WGS84_A, WGS84_A, WGS84_B);
        for level in [3u32, 8, 13] {
            let coord = inside(level);
            let mesh = decode(&source.tile(coord).expect("covered")).expect("decodes");
            let point = DVec3::from_array(mesh.header.horizon_occlusion);
            assert!(point.length() >= 1.0, "{coord:?}: {point}");
            let vertices: Vec<DVec3> = positions(&mesh, coord)
                .into_iter()
                .map(|p| p / radii)
                .collect();
            let mut hiding = 0;
            for lon_step in 0..36 {
                for lat_step in -8..=8 {
                    let (lon, lat) = (
                        f64::from(lon_step) * 10f64.to_radians(),
                        f64::from(lat_step) * 10f64.to_radians(),
                    );
                    let dir = DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
                    for altitude in [1.000_05, 1.001, 1.01, 1.1, 1.5, 3.0, 10.0] {
                        let eye = dir * altitude;
                        if !hidden(eye, point) {
                            continue;
                        }
                        hiding += 1;
                        for (k, q) in vertices.iter().enumerate() {
                            // A vertex sits a quantization step from where
                            // the header was computed: lifted a hair.
                            let q = *q * (1.0 + 1e-9);
                            assert!(
                                hidden(eye, q),
                                "{coord:?}: vertex {k} seen from {eye} though the point is hidden"
                            );
                        }
                    }
                }
            }
            assert!(
                hiding > 100,
                "{coord:?}: the point hid from only {hiding} eyes"
            );
        }
    }

    /// Two neighbouring tiles ask for the same points along the edge they
    /// share, and get the very same heights and normals — at coarse levels
    /// (box-filtered) and fine ones (bilinear) alike.
    #[test]
    fn neighbours_share_their_edge_bit_for_bit() {
        let source = source();
        let n = DEFAULT_VERTICES_PER_SIDE as i64;
        for level in [2u32, 6, 9, 11, 13, 15] {
            // Just south of the equator: the edge it shares with its
            // northern neighbour crosses the footprint at every level.
            let a = tile_at(level, 0.07, -0.001);
            let east = TileCoord::new(level, a.x + 1, a.y);
            let north = TileCoord::new(level, a.x, a.y + 1);
            let (sa, se, sn) = (
                source.samples(a),
                source.samples(east),
                source.samples(north),
            );
            let mut varied = false;
            for k in 0..n {
                let (p, q) = (sa.get(n - 1, k), se.get(0, k));
                assert_eq!(p.0.to_bits(), q.0.to_bits(), "{level} east lon {k}");
                assert_eq!(p.1.to_bits(), q.1.to_bits(), "{level} east lat {k}");
                assert_eq!(p.2.to_bits(), q.2.to_bits(), "{level} east height {k}");
                assert_eq!(
                    sa.normal(n - 1, k),
                    se.normal(0, k),
                    "{level} east normal {k}"
                );
                let (p, q) = (sa.get(k, n - 1), sn.get(k, 0));
                assert_eq!(
                    (p.0.to_bits(), p.1.to_bits(), p.2.to_bits()),
                    (q.0.to_bits(), q.1.to_bits(), q.2.to_bits()),
                    "{level} north {k}"
                );
                assert_eq!(
                    sa.normal(k, n - 1),
                    sn.normal(k, 0),
                    "{level} north normal {k}"
                );
                varied |= p.2 != 0.0;
            }
            assert!(varied, "level {level}: the shared edge has relief to share");

            // And decoded, the edges meet within a quantum of each tile
            // (the northern neighbour: the relief's at every level, where
            // the eastern one may lie off the footprint, on the ellipsoid).
            let (ma, mn) = (
                decode(&source.tile(a).expect("a")).expect("a"),
                decode(&source.tile(north).expect("north")).expect("north"),
            );
            let (pa, pn) = (positions(&ma, a), positions(&mn, north));
            let slack =
                |m: &QuantizedMesh| f64::from(m.header.max_height - m.header.min_height) / 32767.0;
            // Heights a quantum of each range; positions a quantum of the
            // tile's width along the edge.
            let rect = GeographicTilingScheme::default().tile_rect(a);
            let tolerance = slack(&ma) + slack(&mn) + rect.width() * WGS84_A / 32767.0;
            let mut south_of_north: Vec<DVec3> =
                mn.edges[1].iter().map(|&i| pn[i as usize]).collect();
            for &i in &ma.edges[3] {
                let p = pa[i as usize];
                let (nearest, at) = south_of_north
                    .iter()
                    .enumerate()
                    .map(|(k, q)| (q.distance(p), k))
                    .fold((f64::MAX, 0), |m, d| if d.0 < m.0 { d } else { m });
                assert!(nearest <= tolerance, "{level}: {nearest} m apart");
                south_of_north.swap_remove(at);
            }
        }
    }
    /// A coarse vertex stands for the mean of its ground: a grid of
    /// alternating ±100 m reads as about zero, not as one of its samples.
    /// A fine one reads the samples themselves.
    #[test]
    fn coarse_vertices_are_box_filtered_and_fine_ones_bilinear() {
        let side = 129;
        let heights: Vec<f32> = (0..side * side)
            .map(|k| {
                if (k / side + k % side) % 2 == 0 {
                    100.0
                } else {
                    -100.0
                }
            })
            .collect();
        let grid = Grid::new(side, side, heights, footprint()).expect("grid");
        let source = HeightfieldSource::new(grid.clone(), 15);
        for level in [8u32, 10] {
            let s = source.samples(inside(level));
            for j in 1..DEFAULT_VERTICES_PER_SIDE as i64 - 1 {
                let h = s.get(5, j).2;
                assert!(h.abs() < 30.0, "level {level}: {h}");
            }
        }
        // On a sample, bilinear gives the sample.
        let (step_lon, step_lat) = (grid.step_lon, grid.step_lat);
        let lon = grid.west + 10.0 * step_lon;
        let lat = grid.north - 7.0 * step_lat;
        let sample = 100.0 * if (10 + 7) % 2 == 0 { 1.0 } else { -1.0 };
        assert!((grid.height(lon, lat) - sample).abs() < 1e-9);
        assert_eq!(
            grid.mean_height(lon, lat, step_lon / 2.0, step_lat / 2.0),
            grid.height(lon, lat)
        );
        // Halfway between two samples, their mean.
        assert!(grid.height(lon + step_lon / 2.0, lat).abs() < 1e-9);
        // Off the footprint, zero.
        assert_eq!(grid.height(grid.west - step_lon, lat), 0.0);
        assert_eq!(grid.mean_height(-10.0, 0.0, 1.0, 1.0), 0.0);
    }

    #[test]
    fn a_grid_is_refused_when_its_heights_do_not_fit() {
        assert_eq!(
            Grid::new(3, 3, vec![0.0; 8], footprint()).err(),
            Some(GridError::Length {
                columns: 3,
                rows: 3,
                len: 8
            })
        );
        assert!(Grid::new(1, 3, vec![0.0; 3], footprint()).is_err());
        assert!(Grid::new(
            2,
            2,
            vec![0.0; 4],
            GeoRect::from_degrees(1.0, 0.0, 1.0, 1.0)
        )
        .is_err());
    }

    /// Around the footprint, the ellipsoid's own tiles down to its levels,
    /// then nothing; past `max_level`, nothing even inside; and with
    /// [`Outside::NoSuchTile`], nothing anywhere outside.
    #[test]
    fn outside_the_footprint_is_the_ellipsoid_then_nothing() {
        let source = source();
        let far = TileCoord::new(5, 3, 7);
        assert!(!source.covers(far));
        assert_eq!(source.tile(far).expect("ellipsoid"), Ellipsoid::tile(far));
        let far_deep = TileCoord::new(ELLIPSOID_LEVELS, 100, 200);
        assert_eq!(
            source.tile(far_deep).expect("ellipsoid"),
            Ellipsoid::tile(far_deep)
        );
        let beyond = TileCoord::new(ELLIPSOID_LEVELS + 1, 100, 200);
        assert!(source.tile(beyond).expect_err("none").is_absence());
        let deeper = inside(16);
        assert!(source.tile(deeper).expect_err("none").is_absence());
        assert!(source
            .tile(TileCoord::new(0, 2, 0))
            .expect_err("off the scheme")
            .is_absence());

        let bare = HeightfieldSource {
            outside: Outside::NoSuchTile,
            ..source
        };
        assert!(bare.tile(far).expect_err("none").is_absence());
        assert!(bare.tile(inside(15)).is_ok());
    }

    /// A tile straddling the footprint's edge is the relief's, and its part
    /// outside lies on the ellipsoid.
    #[test]
    fn a_tile_straddling_the_edge_is_the_relief_s_and_zero_outside() {
        let source = source();
        // The tile holding the footprint's west edge at level 12.
        let range = AvailabilityRange::covering(footprint(), 12).expect("covered");
        let edge = TileCoord::new(12, range.start_x, range.start_y + 1);
        let rect = GeographicTilingScheme::default().tile_rect(edge);
        assert!(rect.west < footprint().west && footprint().west < rect.east);
        assert!(source.covers(edge));
        let s = source.samples(edge);
        let n = DEFAULT_VERTICES_PER_SIDE as i64;
        assert_eq!(s.get(0, n / 2).2, 0.0, "west of the footprint");
        assert!(s.get(n - 1, n / 2).2 > 0.0, "inside it");
        decode(&source.tile(edge).expect("tile")).expect("decodes");
    }

    /// The pyramid baked is the pyramid the layer lists, and every tile of
    /// it is the relief's.
    #[test]
    fn the_tiles_walked_are_those_the_layer_lists() {
        let source = HeightfieldSource {
            max_level: 11,
            outside: Outside::NoSuchTile,
            ..source()
        };
        let layer = source.layer();
        let tiles: Vec<TileCoord> = source.tiles().collect();
        let listed: u64 = layer
            .available
            .iter()
            .flatten()
            .map(|r| (r.end_x - r.start_x + 1) * (r.end_y - r.start_y + 1))
            .sum();
        assert_eq!(tiles.len() as u64, listed);
        assert!(tiles
            .iter()
            .all(|&t| layer.is_available(t) && source.covers(t)));
        assert_eq!(tiles[0], TileCoord::new(0, 1, 0));
        assert_eq!(layer.maxzoom, 11);
        assert!(layer.has_extension("octvertexnormals"));

        // With the ellipsoid around it, the layer lists the whole globe down
        // to the ellipsoid's levels, and the footprint below.
        let surrounded = HeightfieldSource::new(relief(), 15).layer();
        assert!(surrounded.is_available(TileCoord::new(ELLIPSOID_LEVELS, 0, 0)));
        assert!(!surrounded.is_available(TileCoord::new(15, 0, 0)));
        assert!(surrounded.is_available(inside(15)));
    }

    /// The ellipsoid's grid, numbered the same way: its tiles are made by
    /// the same topology.
    #[test]
    fn the_grid_topology_numbers_vertices_in_first_use_order() {
        let grid = GridTopology::new(4);
        let mut highest = 0;
        for &i in &grid.indices {
            assert!(i <= highest);
            if i == highest {
                highest += 1;
            }
        }
        assert_eq!(highest, 16);
        assert_eq!(grid.reorder(&(0..16).collect::<Vec<u32>>())[0], 0);
    }
}
