// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How much of a drape a frame can show.
//!
//! The bake composes every tile's drape at one size, whatever the tile's
//! distance. Seen from far, a tile covers a few pixels and the sampler only
//! ever reads the small end of its mip chain: the large levels are memory
//! nothing looks at. A renderer may keep the chain from the first level a
//! frame can read, and take the others back when the tile comes closer.

use glam::DVec3;

/// The smallest side a drape is kept at.
pub const SMALLEST_SIDE: u32 = 4;

/// A sphere around a mesh, from the box its vertices fill: its centre in
/// ECEF, its radius in metres. `positions` are little-endian `f32` triples
/// relative to `origin_ecef`.
pub fn sphere(origin_ecef: [f64; 3], positions: &[u8]) -> (DVec3, f64) {
    let (mut low, mut high) = (DVec3::INFINITY, DVec3::NEG_INFINITY);
    for vertex in positions.chunks_exact(12) {
        let at = DVec3::from_array(std::array::from_fn(|axis| {
            f64::from(f32::from_le_bytes(std::array::from_fn(|byte| {
                vertex[4 * axis + byte]
            })))
        }));
        (low, high) = (low.min(at), high.max(at));
    }
    let origin = DVec3::from_array(origin_ecef);
    if low.is_finite() && high.is_finite() {
        (origin + (low + high) / 2.0, (high - low).length() / 2.0)
    } else {
        (origin, 0.0)
    }
}

/// The most pixels a sphere of `radius` metres can span, its centre
/// `distance` metres from the eye, in a picture `height_px` high with a
/// vertical field of `fovy_rad`. Measured where the sphere is nearest, so
/// never under what any part of it spans; infinite when the eye is in it.
pub fn on_screen_px(radius: f64, distance: f64, height_px: f64, fovy_rad: f64) -> f64 {
    let nearest = distance - radius;
    if nearest <= 0.0 {
        return f64::INFINITY;
    }
    2.0 * radius * height_px / (nearest * 2.0 * (fovy_rad / 2.0).tan())
}

/// The levels at the large end of a drape's mip chain that a tile spanning
/// `px` pixels cannot be read at: what is kept, `side >> spared`, is still at
/// least a texel a pixel, and never under [`SMALLEST_SIDE`].
pub fn spared(side: u32, px: f64) -> u32 {
    let mut spared = 0;
    while (side >> (spared + 1)) >= SMALLEST_SIDE && f64::from(side >> (spared + 1)) >= px {
        spared += 1;
    }
    spared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_filling_the_picture_keeps_its_whole_drape() {
        assert_eq!(spared(256, 2160.0), 0);
        assert_eq!(spared(256, 256.0), 0);
        assert_eq!(spared(256, 129.0), 0);
        assert_eq!(spared(256, f64::INFINITY), 0);
    }

    #[test]
    fn each_halving_on_screen_spares_one_more_level() {
        assert_eq!(spared(256, 128.0), 1);
        assert_eq!(spared(256, 64.0), 2);
        assert_eq!(spared(256, 33.0), 2);
        assert_eq!(spared(256, 32.0), 3);
        assert_eq!(spared(2048, 100.0), 4);
    }

    #[test]
    fn what_is_kept_is_never_under_a_texel_a_pixel_nor_the_smallest_side() {
        for side in [1u32, 2, 4, 96, 256, 1000, 2048] {
            for px in [0.0, 0.3, 1.0, 3.0, 7.9, 8.0, 50.0, 255.0, 256.0, 5000.0] {
                let kept = side >> spared(side, px);
                assert!(
                    f64::from(kept) >= px.min(f64::from(side)),
                    "{side} at {px} px keeps {kept}"
                );
                assert!(
                    kept >= SMALLEST_SIDE.min(side),
                    "{side} at {px} px keeps {kept}"
                );
            }
        }
    }

    #[test]
    fn a_sphere_twice_as_far_spans_half_and_one_the_eye_is_in_spans_everything() {
        let fovy = 0.8f64;
        // 100 m of radius, nearest point 1 000 m then 2 000 m away.
        let near = on_screen_px(100.0, 1100.0, 2160.0, fovy);
        let far = on_screen_px(100.0, 2100.0, 2160.0, fovy);
        assert!((near / far - 2.0).abs() < 1e-12);
        // By hand: 200 m across at 1 000 m, in a picture whose 2 160 px
        // cover 2·1000·tan(0.4) m.
        let by_hand = 200.0 * 2160.0 / (2.0 * 1000.0 * 0.4f64.tan());
        assert!((near - by_hand).abs() < 1e-9);
        assert_eq!(on_screen_px(100.0, 100.0, 2160.0, fovy), f64::INFINITY);
        assert_eq!(on_screen_px(100.0, 30.0, 2160.0, fovy), f64::INFINITY);
    }

    #[test]
    fn a_meshs_sphere_is_around_the_box_of_its_vertices() {
        let positions: Vec<u8> = [[-30.0f32, 0.0, 0.0], [30.0, 40.0, 0.0], [0.0, 10.0, 0.0]]
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (centre, radius) = sphere([1000.0, 2000.0, 3000.0], &positions);
        assert_eq!(centre, DVec3::new(1000.0, 2020.0, 3000.0));
        assert!((radius - 36.055_512_754_639_89).abs() < 1e-9);
        // No vertex: the origin, and nothing around it.
        assert_eq!(
            sphere([1.0, 2.0, 3.0], &[]),
            (DVec3::new(1.0, 2.0, 3.0), 0.0)
        );
    }
}
