// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The synthetic relief and the pictures made from it.
//!
//! Deterministic: the same heights on every machine and every run, from a
//! hash rather than a random generator, so two renders of it compare.

use std::io::Cursor;

use tuile_terrain::{GeoRect, Grid, Heights};

/// Samples along each side: 2048 spacings of 8 m, 16.384 km.
pub const SAMPLES: usize = 2049;
/// Metres between two samples.
pub const STEP_M: f64 = 8.0;
/// Where the map's centre lies: on the equator, off any level-0 tile edge.
pub const CENTRE_LON: f64 = 10.0;
pub const CENTRE_LAT: f64 = 0.0;
/// Metres over which the relief comes down to 0 m at the map's edges.
pub const EDGE_RAMP_M: f64 = 1500.0;

/// Metres in a degree of longitude and of latitude at the equator, on WGS84.
const M_PER_DEG_LON: f64 = 111_319.490_793;
const M_PER_DEG_LAT: f64 = 110_574.275_822;

/// The footprint, in degrees: west, south, east, north.
pub fn footprint_degrees() -> [f64; 4] {
    let side = (SAMPLES - 1) as f64 * STEP_M;
    let half_lon = side / M_PER_DEG_LON / 2.0;
    let half_lat = side / M_PER_DEG_LAT / 2.0;
    [
        CENTRE_LON - half_lon,
        CENTRE_LAT - half_lat,
        CENTRE_LON + half_lon,
        CENTRE_LAT + half_lat,
    ]
}

/// A deterministic hash of a lattice point, in [0, 1).
fn hash(x: i64, y: i64, seed: u64) -> f64 {
    let mut h = seed ^ 0x9e37_79b9_7f4a_7c15;
    for v in [x as u64, y as u64] {
        h ^= v.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 31)).wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^= h >> 29;
    }
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Smooth value noise at `(x, y)` (lattice units), in [0, 1].
fn value_noise(x: f64, y: f64, seed: u64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let s = |t: f64| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (s(fx), s(fy));
    let (ix, iy) = (x0 as i64, y0 as i64);
    let top = hash(ix, iy, seed) * (1.0 - sx) + hash(ix + 1, iy, seed) * sx;
    let bottom = hash(ix, iy + 1, seed) * (1.0 - sx) + hash(ix + 1, iy + 1, seed) * sx;
    top * (1.0 - sy) + bottom * sy
}

/// Ridges: noise folded about its middle, several octaves.
fn ridged(x: f64, y: f64) -> f64 {
    let mut sum = 0.0;
    let mut amplitude = 1.0;
    let mut frequency = 1.0;
    let mut norm = 0.0;
    for octave in 0..5 {
        let n = value_noise(x * frequency, y * frequency, 17 + octave);
        let ridge = 1.0 - (2.0 * n - 1.0).abs();
        sum += amplitude * ridge * ridge;
        norm += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    sum / norm
}

/// The height, in metres, at `(x, y)` metres from the map's south-west
/// corner.
pub fn height_at(x: f64, y: f64) -> f64 {
    let side = (SAMPLES - 1) as f64 * STEP_M;
    // Ridges about 4 km apart, and gentler hills between them.
    let ridges = ridged(x / 4000.0, y / 4000.0);
    let hills =
        value_noise(x / 1500.0, y / 1500.0, 3) * 0.6 + value_noise(x / 600.0, y / 600.0, 5) * 0.4;
    let raw = 60.0 + 750.0 * ridges.powf(1.6) + 120.0 * hills;
    // Down to 0 m at every edge, smoothly, so a tile across the border meets
    // the ellipsoid around the map without a cliff.
    let edge = x.min(y).min(side - x).min(side - y);
    let t = (edge / EDGE_RAMP_M).clamp(0.0, 1.0);
    raw * t * t * (3.0 - 2.0 * t)
}

/// The relief as a grid of samples, row 0 along the north edge.
pub fn grid() -> Grid {
    let mut heights = Vec::with_capacity(SAMPLES * SAMPLES);
    for row in 0..SAMPLES {
        let y = (SAMPLES - 1 - row) as f64 * STEP_M;
        for column in 0..SAMPLES {
            heights.push(height_at(column as f64 * STEP_M, y) as f32);
        }
    }
    let [west, south, east, north] = footprint_degrees();
    Grid::new(
        SAMPLES,
        SAMPLES,
        heights,
        GeoRect::from_degrees(west, south, east, north),
    )
    .expect("a valid grid")
}

/// Pixels along an imagery tile's side.
pub const TILE_PX: u32 = 256;

/// The colour of the ground at `height` metres: green lowlands, ochre and
/// brown slopes, pale summits.
fn ramp(height: f64) -> [f64; 3] {
    const STOPS: [(f64, [f64; 3]); 6] = [
        (0.0, [62.0, 104.0, 58.0]),
        (150.0, [96.0, 140.0, 66.0]),
        (350.0, [168.0, 160.0, 92.0]),
        (550.0, [150.0, 112.0, 80.0]),
        (750.0, [190.0, 180.0, 170.0]),
        (900.0, [245.0, 245.0, 245.0]),
    ];
    let h = height.max(0.0);
    for pair in STOPS.windows(2) {
        let ((h0, c0), (h1, c1)) = (pair[0], pair[1]);
        if h <= h1 {
            let t = ((h - h0) / (h1 - h0)).clamp(0.0, 1.0);
            return [
                c0[0] + (c1[0] - c0[0]) * t,
                c0[1] + (c1[1] - c0[1]) * t,
                c0[2] + (c1[2] - c0[2]) * t,
            ];
        }
    }
    STOPS[STOPS.len() - 1].1
}

/// The degrees an XYZ tile of the geographic grid covers — two tiles at
/// level 0, **row 0 along the north edge** — as west, south, east, north.
pub fn xyz_tile_degrees(z: u32, x: u64, y: u64) -> [f64; 4] {
    let size = 180.0 / f64::from(1u32 << z.min(30));
    let west = -180.0 + x as f64 * size;
    let north = 90.0 - y as f64 * size;
    [west, north - size, west + size, north]
}

/// Whether two rectangles in degrees overlap.
fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// The PNG of XYZ tile `z/x/y` on the geographic grid: the relief coloured
/// by height and lit from the north-west; plain lowland colour where there
/// is no relief, so the imagery covers the whole globe.
pub fn imagery_png(heights: &Grid, z: u32, x: u64, y: u64) -> Vec<u8> {
    let rect = xyz_tile_degrees(z, x, y);
    let n = TILE_PX as usize;
    let mut pixels = vec![0u8; n * n * 3];
    if overlaps(rect, footprint_degrees()) {
        let px_lon = (rect[2] - rect[0]) / n as f64;
        let px_lat = (rect[3] - rect[1]) / n as f64;
        // The heights at the pixel centres with a ring of one around them,
        // for the slopes at the tile's edges.
        let m = n + 2;
        let mut h = vec![0.0; m * m];
        for j in 0..m {
            let lat = rect[3] - (j as f64 - 0.5) * px_lat;
            for i in 0..m {
                let lon = rect[0] + (i as f64 - 0.5) * px_lon;
                h[j * m + i] = heights.mean_height(lon, lat, px_lon, px_lat);
            }
        }
        let dx = px_lon * M_PER_DEG_LON;
        let dy = px_lat * M_PER_DEG_LAT;
        let light = {
            let l = [-1.0f64, 1.0, 1.4];
            let len = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
            [l[0] / len, l[1] / len, l[2] / len]
        };
        for j in 0..n {
            for i in 0..n {
                let at = |di: usize, dj: usize| h[(j + dj) * m + (i + di)];
                let centre = at(1, 1);
                // East and north slopes; rows run south.
                let gx = (at(2, 1) - at(0, 1)) / (2.0 * dx);
                let gy = (at(1, 0) - at(1, 2)) / (2.0 * dy);
                let len = (gx * gx + gy * gy + 1.0).sqrt();
                let lit = (-gx * light[0] - gy * light[1] + light[2]) / len;
                let shade = 0.55 + 0.45 * (lit / light[2]).clamp(0.0, 1.4);
                let c = ramp(centre);
                let p = (j * n + i) * 3;
                for k in 0..3 {
                    pixels[p + k] = (c[k] * shade).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    } else {
        let c = ramp(0.0);
        let c = c.map(|v| v as u8);
        for p in pixels.as_chunks_mut::<3>().0 {
            *p = c;
        }
    }
    let mut png = Vec::new();
    image::RgbImage::from_raw(TILE_PX, TILE_PX, pixels)
        .expect("a whole tile")
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("an in-memory PNG");
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The edges come down to 0 m; the inside stands up; the same heights
    /// every run.
    #[test]
    fn the_relief_is_ramped_to_zero_and_deterministic() {
        let side = (SAMPLES - 1) as f64 * STEP_M;
        for t in [0.0, 0.25, 0.5, 1.0] {
            assert_eq!(height_at(t * side, 0.0), 0.0);
            assert_eq!(height_at(0.0, t * side), 0.0);
            assert_eq!(height_at(side, t * side), 0.0);
            assert_eq!(height_at(t * side, side), 0.0);
        }
        let middle = height_at(side / 2.0, side / 2.0);
        assert!(middle > 50.0, "{middle}");
        assert_eq!(middle, height_at(side / 2.0, side / 2.0));
        let [west, south, east, north] = footprint_degrees();
        assert!(west < CENTRE_LON && CENTRE_LON < east);
        assert!(south < 0.0 && 0.0 < north, "across the equator");
        assert!((east - west - 0.1472).abs() < 1e-3);
    }

    /// XYZ: row 0 at the north edge, two tiles at level 0.
    #[test]
    fn imagery_tiles_are_xyz_on_the_geographic_grid() {
        assert_eq!(xyz_tile_degrees(0, 0, 0), [-180.0, -90.0, 0.0, 90.0]);
        assert_eq!(xyz_tile_degrees(0, 1, 0), [0.0, -90.0, 180.0, 90.0]);
        assert_eq!(xyz_tile_degrees(1, 0, 0), [-180.0, 0.0, -90.0, 90.0]);
        assert_eq!(xyz_tile_degrees(1, 0, 1), [-180.0, -90.0, -90.0, 0.0]);
    }
}
