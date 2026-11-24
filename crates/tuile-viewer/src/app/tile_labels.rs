// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The levels written on each tile on screen: its terrain level and the
//! imagery level its base layer wears — `T12 · I11` — and the level that
//! layer asked for this step when it is another — `T12 · I11→13`.
//!
//! Shown with the diagnostic views that colour tiles by their levels
//! (`levels`, `lag`, `cap`), or alone with `tuile://view?labels=on`: over
//! the real picture then, imagery and all, with each tile outlined by the
//! shader (`frame::view_params`), so the imagery a tile wears is read
//! beside the level it says. The labels are the overlay's, drawn after the
//! ground, on a dark background that reads on any imagery. Each
//! label sits at its tile's projected centre and is left out where it would
//! cover one already placed, coarser tiles first: readable at any zoom, a
//! tile too small on screen for its label simply has none. The text is the
//! renderer's own [`tuile_wgpu::Levels`] — what the colours are made of —
//! drawn by the overlay as coverage masks, the same on every shell.
//!
//! A label is pinned to the ground over its tile's origin ([`anchor`]), and
//! left out where the planet stands between that point and the eye. The
//! overlay has no depth: a coarse tile kept at the horizon has its origin
//! hundreds of kilometres down the curve, and projected through the planet
//! it lands on the near ground, where finer tiles are drawn — measured on
//! 2026-11-24, `T5 · I4` written all over a `T11` foreground from 18 km up.

use std::collections::HashMap;

use glam::{DVec3, Mat4, Vec4};
use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef, WGS84_B};
use tuile_core::math::{BoundingVolume, Occluder, Sphere};
use tuile_wgpu::Levels;

/// The diagnostic views the labels come with (`app::DIAGNOSTICS`):
/// `levels`, `lag` and `cap`.
pub(super) const WITH_LEVEL_VIEWS: std::ops::RangeInclusive<usize> = 4..=6;

/// The label's em at density one.
const TEXT_SIZE: f64 = 11.0;
/// The room around a label's text, and between two labels.
const PAD: f64 = 3.0;
/// The background behind each label, linear: dark enough for the white
/// text to read over snow and bright rock as over sea — the labels sit over
/// the real picture with `labels=on` — the picture still showing through.
const BACKGROUND: [f32; 4] = [0.0, 0.0, 0.0, 0.78];
const TEXT: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// What a drawn tile's label says.
pub(super) fn text(l: &Levels) -> String {
    let worn = if l.worn == 0 && l.asked == 0 {
        "I–".to_owned()
    } else {
        format!("I{}", l.worn)
    };
    let asked = if l.asked != 0 && l.asked != l.worn {
        format!("→{}", l.asked)
    } else {
        String::new()
    };
    format!("T{} · {worn}{asked}", l.tile)
}

/// The pixel `at` (relative to the eye, metres) lands on in a view of
/// `size` pixels under `view_proj` (built about the eye); `None` behind the
/// eye or off the view.
pub(super) fn project(view_proj: &Mat4, at: DVec3, size: (f64, f64)) -> Option<(f64, f64)> {
    let clip = *view_proj * Vec4::new(at.x as f32, at.y as f32, at.z as f32, 1.0);
    if clip.w <= 1e-6 {
        return None;
    }
    let (x, y) = (clip.x / clip.w, clip.y / clip.w);
    if !(-1.0..=1.0).contains(&x) || !(-1.0..=1.0).contains(&y) {
        return None;
    }
    Some((
        (f64::from(x) + 1.0) / 2.0 * size.0,
        (1.0 - f64::from(y)) / 2.0 * size.1,
    ))
}

/// Where a tile's label is pinned, seen from `eye`: the ground over its
/// origin — a coarse tile's origin is its mesh's centre, kilometres under
/// the curve of the ground it spans — or `None` where the planet hides that
/// point. The planet is the sphere inscribed in it, as the traversal's
/// horizon test takes it: a label is only ever dropped where the real
/// planet hides its point too. Relief is not looked at: a label behind a
/// ridge is still written.
pub(super) fn anchor(origin: DVec3, eye: DVec3) -> Option<DVec3> {
    let mut ground = ecef_to_geodetic(origin);
    ground.height = ground.height.max(0.0);
    let at = geodetic_to_ecef(ground);
    let planet = Occluder {
        center: DVec3::ZERO,
        radius: WGS84_B,
    };
    let point = BoundingVolume::Sphere(Sphere {
        center: at,
        radius: 0.0,
    });
    (!planet.hides(&point, eye)).then_some(at)
}

/// The labels of the tiles drawn last, and the masks of the texts seen.
#[derive(Default)]
pub(super) struct TileLabels {
    /// Shown without a level view (`tuile://view?labels=on`).
    pub(super) forced: bool,
    /// The tiles drawn last: each one's origin (ECEF) and levels.
    pub(super) tiles: Vec<(DVec3, Levels)>,
    masks: HashMap<String, tuile_ui::Mask>,
    /// The labels placed this frame, with their backgrounds.
    pub(super) placed: Vec<tuile_ui::Label>,
    pub(super) background: tuile_ui::Mesh,
}

impl TileLabels {
    /// Places the labels of [`Self::tiles`] seen from `eye` under
    /// `view_proj` in a view of `size` device pixels at density `scale`.
    pub(super) fn place(&mut self, view_proj: &Mat4, eye: DVec3, size: (f64, f64), scale: f64) {
        self.placed.clear();
        self.background.clear();
        // The texts seen are few: a cache that outgrows them starts again.
        if self.masks.len() > 4_096 {
            self.masks.clear();
        }
        let font = tuile_ui::Font::bundled();
        let em = (TEXT_SIZE * scale.max(1.0)) as f32;
        let pad = PAD * scale.max(1.0);
        // Coarse tiles first: they are the larger, and a fine tile's label
        // gives way to its ancestor's rather than the other way round.
        let mut order: Vec<usize> = (0..self.tiles.len()).collect();
        order.sort_by_key(|&i| (self.tiles[i].1.tile, i));
        let mut taken: Vec<(f64, f64, f64, f64)> = Vec::new();
        for i in order {
            let (origin, levels) = self.tiles[i];
            let Some(at) = anchor(origin, eye) else {
                continue;
            };
            let Some((x, y)) = project(view_proj, at - eye, size) else {
                continue;
            };
            let text = text(&levels);
            let mask = self
                .masks
                .entry(text)
                .or_insert_with_key(|t| font.rasterize(t, em));
            let (w, h) = (f64::from(mask.width), f64::from(mask.height));
            let left = (x - w / 2.0).round();
            let top = (y - h / 2.0).round();
            let rect = (left - pad, top - pad, left + w + pad, top + h + pad);
            let inside = rect.0 >= 0.0 && rect.1 >= 0.0 && rect.2 <= size.0 && rect.3 <= size.1;
            let overlaps = taken
                .iter()
                .any(|r| rect.0 < r.2 && r.0 < rect.2 && rect.1 < r.3 && r.1 < rect.3);
            if !inside || overlaps {
                continue;
            }
            taken.push(rect);
            for (vx, vy) in [
                (rect.0, rect.1),
                (rect.2, rect.1),
                (rect.0, rect.3),
                (rect.0, rect.3),
                (rect.2, rect.1),
                (rect.2, rect.3),
            ] {
                self.background
                    .push(tuile_ui::Vertex::new(vx, vy, BACKGROUND));
            }
            self.placed.push(tuile_ui::Label {
                mask: mask.clone(),
                position: [left as f32, top as f32],
                color: TEXT,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(tile: u32, worn: u32, asked: u32) -> Levels {
        Levels {
            worn,
            asked,
            tile,
            role: 0,
            capped: 0,
        }
    }

    /// The text: terrain and imagery levels, and the level asked when it is
    /// another.
    #[test]
    fn a_label_says_the_levels_and_the_one_asked() {
        assert_eq!(text(&levels(12, 11, 11)), "T12 · I11");
        assert_eq!(text(&levels(12, 11, 13)), "T12 · I11→13");
        assert_eq!(text(&levels(12, 11, 0)), "T12 · I11");
        assert_eq!(text(&levels(3, 0, 0)), "T3 · I–");
    }

    /// A point ahead of the eye on its axis is the centre of the view; one
    /// behind it, or off to the side out of the view, is not placed.
    #[test]
    fn a_point_is_projected_onto_the_view() {
        let vp = glam::camera::rh::proj::directx::perspective(1.0, 2.0, 1.0, 1e6)
            * glam::camera::rh::view::look_at_mat4(
                glam::Vec3::ZERO,
                glam::Vec3::NEG_Z,
                glam::Vec3::Y,
            );
        let (x, y) = project(&vp, DVec3::new(0.0, 0.0, -100.0), (800.0, 400.0)).expect("ahead");
        assert!((x - 400.0).abs() < 1e-3 && (y - 200.0).abs() < 1e-3);
        assert!(project(&vp, DVec3::new(0.0, 0.0, 100.0), (800.0, 400.0)).is_none());
        assert!(project(&vp, DVec3::new(1e5, 0.0, -1.0), (800.0, 400.0)).is_none());
    }

    /// The labels alone (`tuile://view?labels=on`) keep the real picture
    /// — the diagnostic view is not touched — and outline the tiles over
    /// it; the level views keep their own colours and edges.
    #[test]
    fn labels_alone_outline_the_tiles_over_the_real_picture() {
        use super::super::frame::view_params;
        let normal = view_params(0, true);
        assert_eq!((normal[1], normal[2]), (0.0, 1.0));
        assert_eq!(view_params(0, false)[2], 0.0, "no labels, no outlines");
        for view in WITH_LEVEL_VIEWS {
            let p = view_params(view, true);
            assert_eq!((p[1], p[2]), (view as f32, 0.0));
        }
    }

    /// An eye `height` metres over 45° N 6° E, looking north and `pitch`
    /// degrees down through a square view `fov` degrees high: the eye, the
    /// view built about it, and the ground point `north` metres ahead.
    fn over_the_ground(
        height: f64,
        pitch: f64,
        fov: f32,
    ) -> (DVec3, Mat4, impl Fn(f64, f64) -> DVec3) {
        let place = |lat: f64, lon: f64, height: f64| {
            geodetic_to_ecef(tuile_core::geo::Geodetic {
                lon: lon.to_radians(),
                lat: lat.to_radians(),
                height,
            })
        };
        let eye = place(45.0, 6.0, height);
        let (lat, lon) = (45f64.to_radians(), 6f64.to_radians());
        let north = DVec3::new(-lat.sin() * lon.cos(), -lat.sin() * lon.sin(), lat.cos());
        let up = DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
        let p = pitch.to_radians();
        let ahead = north * p.cos() - up * p.sin();
        let vp = glam::camera::rh::proj::directx::perspective(fov.to_radians(), 1.0, 1.0, 1e7)
            * glam::camera::rh::view::look_at_mat4(glam::Vec3::ZERO, ahead.as_vec3(), up.as_vec3());
        // Metres north along the meridian, at a height over the ellipsoid.
        let ground = move |metres: f64, height: f64| {
            place(45.0 + (metres / 6_367_000.0).to_degrees(), 6.0, height)
        };
        (eye, vp, ground)
    }

    /// Two tiles whose labels would cover each other: the coarser keeps its
    /// label, the finer has none.
    #[test]
    fn labels_never_cover_each_other() {
        let (eye, vp, ground) = over_the_ground(1_000.0, 20.0, 50.0);
        let ahead = ground(2_700.0, 0.0);
        let east = DVec3::new(-6f64.to_radians().sin(), 6f64.to_radians().cos(), 0.0);
        let mut l = TileLabels {
            tiles: vec![
                (ahead + east * 2.0, levels(14, 13, 13)),
                (ahead, levels(12, 11, 11)),
                (ahead + east * 900.0, levels(13, 12, 14)),
            ],
            ..TileLabels::default()
        };
        l.place(&vp, eye, (800.0, 800.0), 1.0);
        assert_eq!(l.placed.len(), 2, "the fine one gives way");
        assert_eq!(l.background.len(), 12);
    }

    /// A coarse tile kept at the horizon: its origin, the middle of its mesh,
    /// lies far down the curve of the planet. Seen through the planet that
    /// point lands on the near ground, where the finer tiles are drawn, and
    /// its label with it — `T5 · I4` written over a `T11` foreground from
    /// 18 km up (2026-11-24). It has no label; the near tile keeps its own.
    #[test]
    fn a_tile_whose_ground_is_beyond_the_horizon_has_no_label() {
        let (eye, vp, ground) = over_the_ground(18_500.0, 22.7, 35.0);
        // A level-5 tile 2 200 km north, 5.6° of latitude across: its mesh's
        // centre is the middle of the chord under its ground.
        let span = 5.625f64.to_radians() * 6_367_000.0;
        let coarse =
            (ground(2_217_000.0 - span / 2.0, 0.0) + ground(2_217_000.0 + span / 2.0, 0.0)) / 2.0;
        // The point would be on screen, through the planet.
        assert!(project(&vp, coarse - eye, (1000.0, 1000.0)).is_some());
        let near = ground(44_000.0, 300.0);
        let mut l = TileLabels {
            tiles: vec![(coarse, levels(5, 4, 4)), (near, levels(11, 11, 11))],
            ..TileLabels::default()
        };
        l.place(&vp, eye, (1000.0, 1000.0), 1.0);
        assert_eq!(l.placed.len(), 1, "the tile over the horizon has no label");
        assert!(anchor(coarse, eye).is_none());
        let at = anchor(near, eye).expect("the near tile is in sight");
        assert!(
            (at - near).length() < 1e-3,
            "a tile above the ground is pinned where it is"
        );
    }
}
