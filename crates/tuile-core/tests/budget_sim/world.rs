// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimWorld`: a flat square world with terrain and imagery pyramids of
//! different depths — sea in the west, a coastal plain, mountains in the
//! east — a camera, and a traversal that builds the budget's `NeedSet` the
//! way the real one does: SSE refinement with hysteresis on what was drawn,
//! hold-until-ready on the budget's residency, ancestors preloaded, the
//! floor always listed, a coarse ring around the frustum.

use std::collections::BTreeSet;

use tuile_core::budget::cost::{drape_bytes, TextureFormat};
use tuile_core::budget::{
    Bytes, Class, GeoRect, ImageryNeed, Need, NeedSet, ResourceId, TexelFootprint, TileId,
};

/// The world's side, metres.
pub const SIDE: f64 = 2_097_152.0;
/// The focal length of a 1920-pixel, 60° view, in pixels per radian.
pub const FOCAL: f64 = 1_663.0;
/// The SSE threshold, pixels.
pub const TAU: f64 = 2.0;

/// A region of the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// Shallow terrain, shallow imagery.
    Sea,
    /// Shallow terrain, deep imagery: imagery must outrun terrain here.
    Plain,
    /// Deep terrain, imagery as deep.
    Mountains,
}

impl Region {
    /// The region at ground point `(x, y)`.
    pub fn at(x: f64, _y: f64) -> Region {
        if x < 0.45 * SIDE {
            Region::Sea
        } else if x < 0.7 * SIDE {
            Region::Plain
        } else {
            Region::Mountains
        }
    }

    /// The deepest terrain level.
    pub fn terrain_max(self) -> u32 {
        match self {
            Region::Sea => 7,
            Region::Plain => 11,
            Region::Mountains => 15,
        }
    }

    /// The deepest imagery level.
    pub fn imagery_max(self) -> u32 {
        match self {
            Region::Sea => 9,
            Region::Plain => 17,
            Region::Mountains => 15,
        }
    }

    /// Geometric-error multiplier.
    pub fn roughness(self) -> f64 {
        match self {
            Region::Sea => 0.5,
            Region::Plain => 1.0,
            Region::Mountains => 4.0,
        }
    }

    /// A mesh's bytes.
    pub fn mesh_bytes(self) -> Bytes {
        match self {
            Region::Sea => Bytes::kib(40),
            Region::Plain => Bytes::kib(120),
            Region::Mountains => Bytes::kib(300),
        }
    }
}

/// A camera over the flat world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// Ground position, metres.
    pub x: f64,
    /// Ground position, metres.
    pub y: f64,
    /// Height, metres.
    pub h: f64,
    /// Heading, radians (0 = +x).
    pub heading: f64,
    /// Pitch below the horizon, radians.
    pub pitch: f64,
}

/// The tile `(level, x, y)`.
pub fn tile(level: u32, x: u64, y: u64) -> TileId {
    TileId::from_terrain(level, x, y)
}

/// Its parent.
pub fn parent(id: TileId) -> Option<TileId> {
    let (l, x, y) = id.terrain_coord();
    (l > 0).then(|| tile(l - 1, x / 2, y / 2))
}

/// Its children.
pub fn children(id: TileId) -> [TileId; 4] {
    let (l, x, y) = id.terrain_coord();
    [
        tile(l + 1, 2 * x, 2 * y),
        tile(l + 1, 2 * x + 1, 2 * y),
        tile(l + 1, 2 * x, 2 * y + 1),
        tile(l + 1, 2 * x + 1, 2 * y + 1),
    ]
}

/// Its ground square `[x0, x1] × [y0, y1]`, metres.
pub fn square(id: TileId) -> [f64; 4] {
    let (l, x, y) = id.terrain_coord();
    let s = SIDE / f64::from(1u32 << l);
    [
        x as f64 * s,
        (x + 1) as f64 * s,
        y as f64 * s,
        (y + 1) as f64 * s,
    ]
}

/// Its footprint in the budget's radians (the world is the unit square).
pub fn rect(id: TileId) -> GeoRect {
    let [x0, x1, y0, y1] = square(id);
    GeoRect {
        west: x0 / SIDE,
        east: x1 / SIDE,
        north: 1.0 - y0 / SIDE,
        south: 1.0 - y1 / SIDE,
    }
}

/// Its region, by its centre.
pub fn region(id: TileId) -> Region {
    let [x0, x1, y0, y1] = square(id);
    Region::at((x0 + x1) / 2.0, (y0 + y1) / 2.0)
}

/// Whether it has children.
pub fn has_children(id: TileId) -> bool {
    id.terrain_coord().0 < region(id).terrain_max()
}

/// What a resource costs: meshes by region, drapes by mosaic size.
pub fn cost(r: ResourceId) -> Bytes {
    match r {
        ResourceId::Mesh(t) => region(t).mesh_bytes(),
        ResourceId::Drape(t, level) => {
            let up = level.saturating_sub(t.terrain_coord().0).min(3);
            let tiles = 1u64 << (2 * up);
            Bytes(drape_bytes(256, 256, TextureFormat::Rgba8).0 * tiles)
        }
    }
}

/// How a camera sees one tile.
#[derive(Clone, Copy, Debug)]
pub struct Sight {
    /// 3D distance to the closest point, metres.
    pub distance: f64,
    /// Whether it is in view.
    pub visible: bool,
    /// `1 − off-axis`, `[0, 1]`.
    pub centrality: f64,
    /// Cosine of the view ray against the ground normal at the closest point.
    pub cos_obliquity: f64,
}

/// How `cam` sees `id`, with `extra` radians of half field of view.
pub fn sight(cam: &Camera, id: TileId, extra: f64) -> Sight {
    let [x0, x1, y0, y1] = square(id);
    let cx = cam.x.clamp(x0, x1);
    let cy = cam.y.clamp(y0, y1);
    let dh = ((cx - cam.x).powi(2) + (cy - cam.y).powi(2)).sqrt();
    let distance = (dh * dh + cam.h * cam.h).sqrt();
    let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let dc = ((mx - cam.x).powi(2) + (my - cam.y).powi(2))
        .sqrt()
        .max(1e-6);
    let half_diag = (x1 - x0) * std::f64::consts::FRAC_1_SQRT_2;
    let angle = (my - cam.y).atan2(mx - cam.x);
    let mut off = (angle - cam.heading).rem_euclid(std::f64::consts::TAU);
    if off > std::f64::consts::PI {
        off = std::f64::consts::TAU - off;
    }
    let spread = (half_diag / dc).min(1.0).asin();
    let half_fov = 30f64.to_radians() + 0.1 + extra;
    let far = (60.0 * cam.h + 30_000.0).min(800_000.0);
    let under = dc - half_diag < cam.h * 0.5;
    let visible = distance <= far && (under || off <= half_fov + spread);
    Sight {
        distance,
        visible,
        centrality: (1.0 - off / std::f64::consts::PI).clamp(0.0, 1.0),
        cos_obliquity: (cam.h / distance).clamp(0.05, 1.0),
    }
}

/// The SSE of `id` at distance `d`, pixels.
pub fn sse(id: TileId, d: f64) -> f64 {
    let l = id.terrain_coord().0;
    let error = SIDE / (128.0 * f64::from(1u32 << l)) * region(id).roughness();
    error * FOCAL / d.max(1.0)
}

/// The traversal's view of the budget's residency.
pub trait Residency {
    /// Whether `t` may be treated as resident.
    fn resident(&self, t: TileId) -> bool;
}

/// The traversal: builds this step's need.
pub fn traverse(
    cam: &Camera,
    residency: &dyn Residency,
    sse_scale: f64,
    drawn_last: &BTreeSet<TileId>,
    pinned: u32,
) -> NeedSet {
    let mut need = NeedSet::new();
    let tau = TAU * sse_scale;
    // The floor, always listed.
    for l in 0..=pinned {
        let n = 1u64 << l;
        for x in 0..n {
            for y in 0..n {
                let id = tile(l, x, y);
                need.ancestors.push((id, need_of(cam, id, tau)));
            }
        }
    }
    let mut seen = BTreeSet::new();
    visit(
        cam,
        residency,
        tau,
        drawn_last,
        tile(0, 0, 0),
        &mut need,
        &mut seen,
    );
    ring(cam, tau, tile(0, 0, 0), &mut need, &seen);
    need
}

fn need_of(cam: &Camera, id: TileId, tau: f64) -> Need {
    let s = sight(cam, id, 0.0);
    let l = id.terrain_coord().0;
    let excess = (sse(id, s.distance) / tau).log2().clamp(-8.0, 8.0);
    let mut n = Need::new(l, Class::TerrainMesh)
        .with_rect(rect(id))
        .with_sse_excess(excess as f32)
        .with_centrality(s.centrality as f32)
        .with_distance(s.distance as f32);
    if let Some(p) = parent(id) {
        n = n.with_parent(p);
    }
    if let Ok(fp) = TexelFootprint::new(s.distance, s.cos_obliquity, FOCAL, SIDE / 256.0, 1.0) {
        n = n.with_imagery(ImageryNeed {
            footprint: fp,
            max_level: region(id).imagery_max(),
        });
    }
    n
}

fn visit(
    cam: &Camera,
    res: &dyn Residency,
    tau: f64,
    drawn_last: &BTreeSet<TileId>,
    id: TileId,
    need: &mut NeedSet,
    seen: &mut BTreeSet<TileId>,
) {
    let s = sight(cam, id, 0.0);
    if !s.visible {
        return;
    }
    seen.insert(id);
    let kids_drawn_last = children(id).iter().any(|k| drawn_last.contains(k));
    let threshold = if kids_drawn_last { tau / 1.25 } else { tau };
    let refine = has_children(id) && sse(id, s.distance) > threshold;
    if !refine {
        need.drawn.push((id, need_of(cam, id, tau)));
        return;
    }
    let visible_kids: Vec<TileId> = children(id)
        .into_iter()
        .filter(|&k| sight(cam, k, 0.0).visible)
        .collect();
    let missing: Vec<TileId> = visible_kids
        .iter()
        .copied()
        .filter(|&k| !res.resident(k))
        .collect();
    if missing.is_empty() {
        need.ancestors.push((id, need_of(cam, id, tau)));
        for k in visible_kids {
            visit(cam, res, tau, drawn_last, k, need, seen);
        }
    } else {
        need.drawn.push((id, need_of(cam, id, tau)));
        for k in missing {
            need.blockers.push((k, need_of(cam, k, tau)));
        }
    }
}

fn ring(cam: &Camera, tau: f64, id: TileId, need: &mut NeedSet, seen: &BTreeSet<TileId>) {
    let s = sight(cam, id, 0.5);
    if !s.visible {
        return;
    }
    let coarse = sse(id, s.distance) <= 8.0 * tau || !has_children(id);
    if coarse {
        if !seen.contains(&id) {
            need.ring.push((id, need_of(cam, id, tau)));
        }
        return;
    }
    for k in children(id) {
        ring(cam, tau, k, need, seen);
    }
}
