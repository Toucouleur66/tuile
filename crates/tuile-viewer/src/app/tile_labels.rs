// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The levels written on each tile on screen: its terrain level and the
//! imagery level its base layer wears — `T12 · I11` — and the level that
//! layer asked for this step when it is another — `T12 · I11→13`.
//!
//! Shown with the diagnostic views that colour tiles by their levels
//! (`levels`, `lag`, `cap`), or alone with `tuile://view?labels=on`. Each
//! label sits at its tile's projected centre and is left out where it would
//! cover one already placed, coarser tiles first: readable at any zoom, a
//! tile too small on screen for its label simply has none. The text is the
//! renderer's own [`tuile_wgpu::Levels`] — what the colours are made of —
//! drawn by the overlay as coverage masks, the same on every shell.

use std::collections::HashMap;

use glam::{DVec3, Mat4, Vec4};
use tuile_wgpu::Levels;

/// The diagnostic views the labels come with (`app::DIAGNOSTICS`):
/// `levels`, `lag` and `cap`.
pub(super) const WITH_LEVEL_VIEWS: std::ops::RangeInclusive<usize> = 4..=6;

/// The label's em at density one.
const TEXT_SIZE: f64 = 11.0;
/// The room around a label's text, and between two labels.
const PAD: f64 = 3.0;
/// The background behind each label, linear: dark, the picture showing
/// through.
const BACKGROUND: [f32; 4] = [0.0, 0.0, 0.0, 0.62];
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
            let Some((x, y)) = project(view_proj, origin - eye, size) else {
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

    /// Two tiles whose labels would cover each other: the coarser keeps its
    /// label, the finer has none.
    #[test]
    fn labels_never_cover_each_other() {
        let vp = glam::camera::rh::proj::directx::perspective(1.0, 2.0, 1.0, 1e6)
            * glam::camera::rh::view::look_at_mat4(
                glam::Vec3::ZERO,
                glam::Vec3::NEG_Z,
                glam::Vec3::Y,
            );
        let mut l = TileLabels {
            tiles: vec![
                (DVec3::new(0.1, 0.0, -100.0), levels(14, 13, 13)),
                (DVec3::new(0.0, 0.0, -100.0), levels(12, 11, 11)),
                (DVec3::new(60.0, 0.0, -100.0), levels(13, 12, 14)),
            ],
            ..TileLabels::default()
        };
        l.place(&vp, DVec3::ZERO, (800.0, 400.0), 1.0);
        assert_eq!(l.placed.len(), 2, "the fine one gives way");
        assert_eq!(l.background.len(), 12);
        let first = &l.placed[0];
        assert!(
            (f64::from(first.position[0]) + f64::from(first.mask.width) / 2.0 - 400.0).abs() < 1.0
        );
    }
}
