// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The imagery need, independent of the terrain level (spec §4, plan Step 2,
//! invariant I10, checklist C21).

#![allow(clippy::unwrap_used, clippy::panic)]

use tuile_core::budget::imagery::{best_drape, ideal_level, needed_level, FootprintError};
use tuile_core::budget::{DrapeCandidate, GeoRect, LevelClamp, TexelFootprint, UvTransform};

const RED: &str = "the budget core regressed";

/// A footprint at `distance` metres and `cos` of obliquity, through a
/// 1000 px/rad lens, level-0 texels of 40 000 m, a 1 px target.
fn fp(distance: f64, cos: f64) -> TexelFootprint {
    TexelFootprint::new(distance, cos, 1_000.0, 40_000.0, 1.0).unwrap()
}

const OPEN: LevelClamp = LevelClamp {
    floor: 0,
    terrain_level: 30,
    over_terrain_cap: 3,
    source_max: 30,
};

/// §4: the ideal level is the coarsest level whose texel is at most the
/// target on screen.
#[test]
fn the_ideal_level_is_the_coarsest_sharp_enough() {
    // ratio = 1000 × 40 000 × √1 / (d × 1) = 4·10⁷ / d.
    assert_eq!(ideal_level(fp(4.0e7, 1.0)), 0, "ratio 1: level 0");
    assert_eq!(ideal_level(fp(2.0e7, 1.0)), 1, "{RED}: ratio 2 is level 1");
    assert_eq!(
        ideal_level(fp(1.9e7, 1.0)),
        2,
        "ratio just above 2: level 2"
    );
    assert_eq!(ideal_level(fp(4.0e7 / 1024.0, 1.0)), 10);
}

/// I10: the need falls as distance grows.
#[test]
fn the_need_falls_as_distance_grows() {
    let levels: Vec<u32> = (0..40)
        .map(|i| ideal_level(fp(100.0 * 1.4f64.powi(i), 1.0)))
        .collect();
    assert!(levels.windows(2).all(|w| w[1] <= w[0]));
    assert!(levels[0] > levels[39] + 10, "{RED}: {levels:?}");
}

/// I10: the need falls as the view grows more oblique.
#[test]
fn the_need_falls_as_obliquity_grows() {
    let levels: Vec<u32> = (1..=100)
        .rev()
        .map(|k| ideal_level(fp(1_000.0, f64::from(k) / 100.0)))
        .collect();
    assert!(levels.windows(2).all(|w| w[1] <= w[0]));
    assert!(levels[0] > levels[99], "{RED}: {levels:?}");
}

/// §4: the need does not flap within half a level of where it is.
#[test]
fn the_need_does_not_flap_within_half_a_level() {
    // Level 10 is ideal for ratios in (2^9, 2^10]; hysteresis keeps it over
    // (2^8.5, 2^10.5].
    let at = |ratio: f64| fp(4.0e7 / ratio, 1.0);
    assert_eq!(needed_level(None, at(1000.0), OPEN), 10, "{RED}");
    for ratio in [370.0, 600.0, 1000.0, 1400.0] {
        assert_eq!(needed_level(Some(10), at(ratio), OPEN), 10, "ratio {ratio}");
    }
}

/// §4: past half a level, the need moves.
#[test]
fn past_half_a_level_the_need_moves() {
    let at = |ratio: f64| fp(4.0e7 / ratio, 1.0);
    assert_eq!(needed_level(Some(10), at(1500.0), OPEN), 11, "{RED}");
    assert_eq!(needed_level(Some(10), at(350.0), OPEN), 9);
}

/// I10, C21, D5: imagery stays within three levels above terrain.
#[test]
fn imagery_stays_within_three_levels_above_terrain() {
    let clamp = LevelClamp {
        terrain_level: 7,
        ..OPEN
    };
    let near = fp(1.0, 1.0);
    assert_eq!(needed_level(None, near, clamp), 10, "{RED}");
}

/// §4: never below the pinned floor, never above the source.
#[test]
fn imagery_stays_between_the_floor_and_the_source_maximum() {
    let clamp = LevelClamp {
        floor: 4,
        terrain_level: 12,
        over_terrain_cap: 3,
        source_max: 13,
    };
    assert_eq!(needed_level(None, fp(4.0e7, 1.0), clamp), 4);
    assert_eq!(needed_level(None, fp(1.0, 1.0), clamp), 13, "{RED}");
}

/// C21, plan Step 2: a plain at a shallow terrain level wears imagery
/// deeper than its terrain.
#[test]
fn a_plain_at_a_shallow_terrain_level_wears_imagery_deeper_than_its_terrain() {
    let plain = LevelClamp {
        terrain_level: 9,
        ..OPEN
    };
    // 2 km away, looking down: the texel wants about level 15.
    let level = needed_level(None, fp(2_000.0, 1.0), plain);
    assert!(level > 9, "{RED}: {level}");
}

/// C21: the need comes from the footprint alone, not the terrain level
/// (within the cap).
#[test]
fn the_need_does_not_depend_on_the_terrain_level() {
    let f = fp(5_000.0, 0.6);
    let levels: Vec<u32> = (11..20)
        .map(|terrain_level| {
            needed_level(
                None,
                f,
                LevelClamp {
                    terrain_level,
                    ..OPEN
                },
            )
        })
        .collect();
    assert!(levels.iter().all(|l| *l == levels[0]), "{levels:?}");
    assert!(levels[0] > 0, "{RED}");
}

/// Engineering bar: invalid inputs are refused with a typed error.
#[test]
fn invalid_footprints_are_refused_with_a_typed_error() {
    assert_eq!(
        TexelFootprint::new(f64::NAN, 1.0, 1.0, 1.0, 1.0),
        Err(FootprintError::NotPositive("distance"))
    );
    assert_eq!(
        TexelFootprint::new(1.0, 0.0, 1.0, 1.0, 1.0),
        Err(FootprintError::Obliquity)
    );
    assert_eq!(
        TexelFootprint::new(1.0, 1.0, 1.0, -1.0, 1.0),
        Err(FootprintError::NotPositive("texel"))
    );
}

fn quad(level: u32, x: u32, y: u32) -> GeoRect {
    let s = 1.0 / f64::from(1u32 << level);
    GeoRect {
        west: f64::from(x) * s,
        east: f64::from(x + 1) * s,
        north: 1.0 - f64::from(y) * s,
        south: 1.0 - f64::from(y + 1) * s,
    }
}

/// §4 fallback: a quadrant of its parent maps to half scale and offset;
/// `v` grows southward.
#[test]
fn a_quadrant_maps_to_half_its_parents_drape() {
    let parent = quad(3, 2, 5);
    let se = quad(4, 5, 11);
    let uv = UvTransform::between(se, parent).expect(RED);
    assert_eq!(uv.scale, [0.5, 0.5]);
    assert_eq!(uv.offset, [0.5, 0.5]);
    let nw = UvTransform::between(quad(4, 4, 10), parent).expect(RED);
    assert_eq!(nw.offset, [0.0, 0.0]);
}

/// §4: a tile's own drape at another level needs no transform.
#[test]
fn a_tiles_own_drape_is_the_identity() {
    let r = quad(6, 17, 40);
    assert_eq!(
        UvTransform::between(r, r),
        Some(UvTransform::IDENTITY),
        "{RED}"
    );
}

/// §4: two levels down composes to a quarter.
#[test]
fn two_levels_down_is_a_quarter() {
    let uv = UvTransform::between(quad(5, 7, 2), quad(3, 1, 0)).expect(RED);
    assert_eq!(uv.scale, [0.25, 0.25]);
    assert_eq!(uv.offset, [0.75, 0.5]);
}

/// §4: a drape that does not contain the tile cannot dress it.
#[test]
fn a_drape_that_does_not_contain_the_tile_has_no_transform() {
    assert_eq!(UvTransform::between(quad(4, 0, 0), quad(3, 1, 1)), None);
    let r = quad(6, 17, 40);
    assert_eq!(
        UvTransform::between(quad(3, 2, 5), r),
        None,
        "the cover is smaller"
    );
    // Not vacuous: a containing cover does map.
    assert!(
        UvTransform::between(quad(4, 2, 2), quad(3, 1, 1)).is_some(),
        "{RED}"
    );
}

fn c(level: u32, up: u32) -> DrapeCandidate {
    DrapeCandidate { level, up }
}

/// I9, C20: the best drape is never blurrier than necessary: the coarsest
/// at or above the need, else the sharpest below it.
#[test]
fn the_best_drape_is_never_blurrier_than_necessary() {
    assert_eq!(
        best_drape(5, [c(3, 1), c(7, 0), c(6, 0)]),
        Some(c(6, 0)),
        "{RED}"
    );
    assert_eq!(best_drape(5, [c(3, 1), c(2, 2), c(4, 0)]), Some(c(4, 0)));
    assert_eq!(best_drape(5, [c(5, 2), c(9, 0)]), Some(c(5, 2)));
    assert_eq!(best_drape(5, []), None);
}

/// §4: at equal level the closest owner wins, the tile's own first.
#[test]
fn at_equal_level_the_own_drape_wins() {
    assert_eq!(
        best_drape(4, [c(4, 2), c(4, 0), c(4, 1)]),
        Some(c(4, 0)),
        "{RED}"
    );
    assert_eq!(best_drape(9, [c(3, 2), c(3, 1)]), Some(c(3, 1)));
}
