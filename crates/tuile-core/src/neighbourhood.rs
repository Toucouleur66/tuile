// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The neighbourhood preloaded around the view
//! (`docs/explorations/budget-progressive-coarse-preload.md` §2, §4, §5):
//! per level between the floor and one level under the eye's, the
//! `⌊π(ρ+1)²⌋` ground tiles nearest the view's footprint, each with its
//! distance from there in tiles of its own level. The budget weighs them by
//! the progressive score; this module only finds them.
//!
//! `F`, the footprint, is the centre of the finest drawn ground tile nearest
//! the eye, and `n` its level: the level the view needs at its nearest
//! point. A tile's distance `d` is its volume's distance to `F` over `s_ℓ`,
//! the size of the level-`ℓ` tile nearest `F`. The walk is taken again only
//! when `n`, the tile under `F`, `ρ`, the floor or the tree's shape change.
//!
//! The ring is isotropic: any direction is possible. Where the view is
//! going is the predictions' to say — the views ahead
//! ([`crate::protocol::ClientMessage::Ahead`]), recorded inputs whose
//! tiles join the ring — never an extrapolation of this module's.

use std::collections::HashMap;

use glam::DVec3;

use crate::math::BoundingVolume;
use crate::source::{TileId, TileTree};

/// The tiles listed per level at radius `rho`: `⌊π(ρ+1)²⌋`.
pub fn per_level(rho: u32) -> usize {
    let r = f64::from(rho) + 1.0;
    // Truncation is the floor of a small positive number.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let k = (std::f64::consts::PI * r * r).floor() as usize;
    k
}

/// The size of a tile's footprint on the ground, metres: twice the
/// geometric mean of its box's two largest half-axes (a square tile's
/// side), or a sphere's inscribed square's side.
pub fn tile_size(v: &BoundingVolume) -> f64 {
    match v {
        BoundingVolume::Obb(b) => {
            let mut l = [
                b.half_axes.col(0).length(),
                b.half_axes.col(1).length(),
                b.half_axes.col(2).length(),
            ];
            l.sort_by(|a, b| b.total_cmp(a));
            2.0 * (l[0] * l[1]).sqrt()
        }
        BoundingVolume::Sphere(s) => s.radius * std::f64::consts::SQRT_2,
    }
}

/// What the walk was taken for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    need_level: u32,
    under: TileId,
    rho: u32,
    floor: Option<u32>,
    revision: Option<u64>,
}

/// The neighbourhood of the last walk. See the module.
#[derive(Debug, Default)]
pub struct Neighbourhood {
    key: Option<Key>,
    /// The footprint `F`.
    footprint: DVec3,
    /// `s_ℓ` by level.
    sizes: Vec<f64>,
    /// The tiles listed, with their distance in tiles of their level, in
    /// level then distance order.
    tiles: Vec<(TileId, f32)>,
    index: HashMap<TileId, f32>,
}

impl Neighbourhood {
    /// The level the view needs at its nearest point, once a walk was
    /// taken.
    pub fn need_level(&self) -> Option<u32> {
        self.key.map(|k| k.need_level)
    }

    /// The tiles listed, with their distance in tiles of their own level.
    pub fn tiles(&self) -> &[(TileId, f32)] {
        &self.tiles
    }

    /// Updates it for this step's drawn tiles seen from `eye`: walks the
    /// tree again only when what the walk depends on changed. Returns
    /// whether it did.
    pub fn update(
        &mut self,
        tree: &dyn TileTree,
        drawn: impl IntoIterator<Item = TileId>,
        eye: DVec3,
        rho: u32,
        floor: Option<u32>,
    ) -> bool {
        // The finest drawn ground tile nearest the eye.
        let mut under: Option<(u32, f64, TileId)> = None;
        for t in drawn {
            if !tree.mesh_class(t).is_ground() {
                continue;
            }
            let level = tree.level(t);
            let d = tree.properties(t).bounding_volume.distance_to_point(eye);
            let better = match under {
                None => true,
                Some((l, dd, id)) => level > l || (level == l && (d < dd || (d == dd && t < id))),
            };
            if better {
                under = Some((level, d, t));
            }
        }
        let Some((need_level, _, under)) = under else {
            let changed = self.key.is_some();
            *self = Neighbourhood::default();
            return changed;
        };
        let key = Key {
            need_level,
            under,
            rho,
            floor,
            revision: tree.revision(),
        };
        if self.key == Some(key) && key.revision.is_some() {
            return false;
        }
        self.key = Some(key);
        self.walk(tree, under, rho, floor);
        true
    }

    fn walk(&mut self, tree: &dyn TileTree, under: TileId, rho: u32, floor: Option<u32>) {
        let need_level = self.key.map_or(0, |k| k.need_level);
        self.footprint = tree.properties(under).bounding_volume.center();
        self.tiles.clear();
        self.index.clear();
        self.sizes.clear();
        if rho == 0 {
            return;
        }
        let f = self.footprint;
        let k = per_level(rho);
        let rho = f64::from(rho);
        let mut frontier: Vec<TileId> = tree
            .roots()
            .into_iter()
            .filter(|r| tree.mesh_class(*r).is_ground())
            .collect();
        let mut level_tiles: Vec<(f64, TileId)> = Vec::new();
        while !frontier.is_empty() {
            // One depth at a time: a frontier's tiles share a level.
            let level = tree.level(frontier[0]);
            if level > need_level + 1 {
                break;
            }
            let near: Vec<(f64, TileId, f64)> = frontier
                .iter()
                .map(|t| {
                    let v = tree.properties(*t).bounding_volume;
                    (v.distance_to_point(f), *t, tile_size(&v))
                })
                .collect();
            // `s_ℓ`: the size of the tile nearest `F`.
            let size = near
                .iter()
                .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
                .map_or(1.0, |n| n.2)
                .max(1e-6);
            let at = usize::try_from(level).unwrap_or(usize::MAX);
            if self.sizes.len() <= at {
                self.sizes.resize(at + 1, 0.0);
            }
            self.sizes[at] = size;
            level_tiles.clear();
            let mut next = Vec::new();
            for &(dist, t, _) in &near {
                let d = dist / size;
                if d <= rho {
                    level_tiles.push((d, t));
                }
                // A child within ρ tiles of its level lies in a parent
                // within ρ/2 of its own.
                if d <= rho / 2.0 + 0.5 {
                    next.extend(tree.children(t));
                }
            }
            if floor.is_none_or(|p| level > p) {
                level_tiles.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                for &(d, t) in level_tiles.iter().take(k) {
                    if tree.properties(t).has_content {
                        // Distances of a few tiles: f32 is plenty.
                        #[allow(clippy::cast_possible_truncation)]
                        let d = d as f32;
                        self.tiles.push((t, d));
                        self.index.insert(t, d);
                    }
                }
            }
            frontier = next;
        }
    }

    /// The distance of `t` from the footprint in tiles of its own level:
    /// listed, or measured against its level's size.
    pub fn away(&self, tree: &dyn TileTree, t: TileId) -> Option<f32> {
        if let Some(d) = self.index.get(&t) {
            return Some(*d);
        }
        self.key?;
        let level = usize::try_from(tree.level(t)).ok()?;
        let size = self.sizes.get(level).copied().filter(|s| *s > 0.0)?;
        let d = tree
            .properties(t)
            .bounding_volume
            .distance_to_point(self.footprint)
            / size;
        #[allow(clippy::cast_possible_truncation)]
        Some(d as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Obb;
    use crate::source::{Refine, TileProperties};
    use glam::DMat3;

    /// A flat quadtree over a square of 2²⁰ m: level `l`'s tiles are
    /// `2^(20−l)` m on a side.
    struct Plane;

    const SIDE: f64 = 1_048_576.0;

    fn size(level: u32) -> f64 {
        SIDE / f64::from(1u32 << level)
    }

    impl TileTree for Plane {
        fn roots(&self) -> Vec<TileId> {
            vec![TileId::from_terrain(0, 0, 0)]
        }
        fn children(&self, id: TileId) -> Vec<TileId> {
            let (l, x, y) = id.terrain_coord();
            if l >= 20 {
                return Vec::new();
            }
            (0..4)
                .map(|i| TileId::from_terrain(l + 1, 2 * x + (i & 1), 2 * y + (i >> 1)))
                .collect()
        }
        fn properties(&self, id: TileId) -> TileProperties {
            let (l, x, y) = id.terrain_coord();
            let s = size(l);
            TileProperties {
                bounding_volume: BoundingVolume::Obb(Obb {
                    center: DVec3::new((x as f64 + 0.5) * s, (y as f64 + 0.5) * s, 0.0),
                    half_axes: DMat3::from_cols(
                        DVec3::new(s / 2.0, 0.0, 0.0),
                        DVec3::new(0.0, s / 2.0, 0.0),
                        DVec3::new(0.0, 0.0, 1.0),
                    ),
                }),
                geometric_error: s / 64.0,
                refine: Refine::Replace,
                has_content: true,
            }
        }
        fn parent(&self, id: TileId) -> Option<TileId> {
            let (l, x, y) = id.terrain_coord();
            (l > 0).then(|| TileId::from_terrain(l - 1, x / 2, y / 2))
        }
        fn level(&self, id: TileId) -> u32 {
            id.terrain_coord().0
        }
        fn revision(&self) -> Option<u64> {
            Some(0)
        }
    }

    /// The tile of level `l` under the point `(px, py)`.
    fn under(l: u32, px: f64, py: f64) -> TileId {
        let s = size(l);
        TileId::from_terrain(l, (px / s) as u64, (py / s) as u64)
    }

    #[test]
    fn the_per_level_count_is_the_disc_of_rho_plus_one() {
        assert_eq!(per_level(4), 78);
        assert_eq!(per_level(2), 28);
        assert_eq!(per_level(0), 3);
    }

    /// The bound and the coverage of the document (§4, §5): per level at
    /// most `⌊π(ρ+1)²⌋` tiles, levels from the floor to `n + 1` only; and
    /// every tile meeting the disc of radius `(ρ + 1 − √2) s_ℓ` around the
    /// footprint listed, at every level — the ground a zoom-out or a turn
    /// within that radius finds.
    #[test]
    fn the_neighbourhood_is_bounded_and_covers_its_disc() {
        for (rho, n, px, py) in [
            (4u32, 13u32, 523_411.0, 301_877.0),
            (2, 13, 523_411.0, 301_877.0),
            (4, 9, 101_003.0, 977_123.0),
            (3, 16, 700_000.5, 700_000.5),
        ] {
            let mut h = Neighbourhood::default();
            let drawn = [under(n, px, py)];
            assert!(h.update(&Plane, drawn, DVec3::new(px, py, 50.0), rho, Some(3)));
            let k = per_level(rho);
            let f = Plane.properties(drawn[0]).bounding_volume.center();
            for l in 0..=n + 3 {
                let at: Vec<_> = h
                    .tiles()
                    .iter()
                    .filter(|(t, _)| t.terrain_coord().0 == l)
                    .collect();
                if l <= 3 || l > n + 1 {
                    assert!(at.is_empty(), "ρ {rho} level {l}: listed outside the range");
                    continue;
                }
                assert!(at.len() <= k, "ρ {rho} level {l}: {} > {k}", at.len());
                let r = (f64::from(rho) + 1.0 - std::f64::consts::SQRT_2) * size(l);
                let listed: std::collections::BTreeSet<TileId> =
                    at.iter().map(|(t, _)| *t).collect();
                let s = size(l);
                let (cx, cy) = ((f.x / s) as i64, (f.y / s) as i64);
                let span = rho as i64 + 2;
                let last = (1i64 << l) - 1;
                for x in (cx - span).max(0)..=(cx + span).min(last) {
                    for y in (cy - span).max(0)..=(cy + span).min(last) {
                        let id = TileId::from_terrain(l, x as u64, y as u64);
                        let d = Plane.properties(id).bounding_volume.distance_to_point(f);
                        if d <= r {
                            assert!(
                                listed.contains(&id),
                                "ρ {rho} level {l}: {x},{y} at {d} m not listed"
                            );
                        }
                    }
                }
            }
            // Still: the walk is not taken again.
            assert!(!h.update(&Plane, drawn, DVec3::new(px, py, 60.0), rho, Some(3)));
        }
    }
}
