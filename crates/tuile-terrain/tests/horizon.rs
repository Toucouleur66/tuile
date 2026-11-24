// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **What the planet hides is not selected; what the eye sees is.**
//!
//! The horizon test used to shrink the planet by the radius of a tile's
//! bounding sphere. For a coarse tile that radius is hundreds of kilometres,
//! and from 18 km up the horizon it tested against stood past 2 000 km
//! instead of 486: level-4 and level-5 tiles wholly behind the planet were
//! selected, fetched, draped and drawn, the depth buffer hiding them (the
//! level views of 2026-11-24 wrote `T5 · I4` across the foreground). A box
//! is now hidden when its eight corners are.
//!
//! Two claims, on the real terrain quadtree offline (see `reach.rs`) and a
//! synthetic view — 18.5 km up, 22.7° down, 35° high, the pose of that
//! picture over another place:
//!
//! | claim | what breaking it looks like |
//! |---|---|
//! | no selected tile's box is wholly behind the planet | coarse tiles fetched and drawn for nothing |
//! | every pixel that sees the ground sees a selected tile | a black hole where a tile was culled that showed |
//!
//! The second is checked by casting a ray through a grid of pixels onto the
//! ellipsoid, here and from an eye looking at the horizon itself.

use std::collections::HashSet;

use glam::{dvec2, DVec3};
use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef, Geodetic, WGS84_A, WGS84_B};
use tuile_core::math::BoundingVolume;
use tuile_core::source::{TileId, TileTree};
use tuile_core::traversal::{traverse, Config, Residency, ResidentSet, TraversalOutput, ViewState};
use tuile_terrain::layer::LayerJson;
use tuile_terrain::tiling::{GeographicTilingScheme, TileCoord};
use tuile_terrain::tree::TerrainTree;

const VIEWPORT: (f64, f64) = (2320.0, 2340.0);
const FOV_DEG: f64 = 35.0;

fn offline_terrain() -> TerrainTree {
    let layer: LayerJson = serde_json::from_str(
        r#"{"format":"quantized-mesh-1.0","minzoom":0,"maxzoom":15,
            "tiles":["{z}/{x}/{y}.terrain"]}"#,
    )
    .expect("layer.json");
    TerrainTree::new(layer).with_max_level(19)
}

/// An eye `height` metres over 40° N 10° E, heading 290°, `pitch` degrees
/// down.
fn view(height: f64, pitch: f64) -> ViewState {
    let (lon, lat) = (10f64.to_radians(), 40f64.to_radians());
    let eye = geodetic_to_ecef(Geodetic { lon, lat, height });
    let east = DVec3::new(-lon.sin(), lon.cos(), 0.0);
    let north = DVec3::new(-lat.sin() * lon.cos(), -lat.sin() * lon.sin(), lat.cos());
    let up = DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
    let (h, p) = (290f64.to_radians(), (-pitch).to_radians());
    let ahead = (east * h.sin() + north * h.cos()) * p.cos() + up * p.sin();
    ViewState::perspective(
        eye,
        ahead.normalize(),
        up,
        dvec2(VIEWPORT.0, VIEWPORT.1),
        FOV_DEG.to_radians(),
    )
}

/// The selection once every load it asks for has landed, as `reach.rs`
/// drives it.
fn converged(tree: &TerrainTree, view: &ViewState) -> Vec<TileId> {
    // The interactive session's settings, as the viewer runs them.
    let config = Config::interactive_globe();
    let views = [*view];
    let scheme = GeographicTilingScheme::default();
    let mut residency = ResidentSet::default();
    // The pinned floor, resident for the life of a session.
    for x in 0..scheme.tiles_x(5) {
        for y in 0..scheme.tiles_y(5) {
            residency.insert(TileId::from_terrain(5, x, y));
        }
    }
    let mut out = TraversalOutput::default();
    let mut rendered: HashSet<TileId> = HashSet::new();
    let chosen_before: HashSet<TileId> = HashSet::new();
    for _ in 0..80 {
        traverse(
            tree,
            &residency,
            &views,
            &config,
            0,
            &rendered,
            &chosen_before,
            &mut out,
        );
        rendered = out.selected.iter().map(|(t, _)| *t).collect();
        rendered.extend(out.awaiting.iter().copied());
        let pending: Vec<_> = out
            .requests
            .iter()
            .take(512)
            .map(|r| r.tile)
            .filter(|t| !residency.is_resident(*t))
            .collect();
        if pending.is_empty() {
            break;
        }
        for tile in pending {
            residency.insert(tile);
        }
    }
    assert!(out.requests.is_empty(), "the selection did not converge");
    out.selected.iter().map(|(t, _)| *t).collect()
}

/// Whether the straight line from `eye` to `p` passes through the sphere
/// inscribed in the planet before reaching `p` — computed apart from the
/// test under test, by the closest approach of the segment.
fn behind_the_planet(eye: DVec3, p: DVec3) -> bool {
    let d = p - eye;
    let len = d.length();
    let dir = d / len;
    let t = (-eye).dot(dir).clamp(0.0, len);
    (eye + dir * t).length() < WGS84_B
}

/// Where the ray from `eye` along `dir` meets the ellipsoid first, if it
/// does.
fn ground_hit(eye: DVec3, dir: DVec3) -> Option<DVec3> {
    let s = DVec3::new(1.0 / WGS84_A, 1.0 / WGS84_A, 1.0 / WGS84_B);
    let (e, d) = (eye * s, dir * s);
    let (a, b, c) = (d.dot(d), 2.0 * e.dot(d), e.dot(e) - 1.0);
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let t = (-b - disc.sqrt()) / (2.0 * a);
    (t > 0.0).then(|| eye + dir * t)
}

/// The rays through a grid of pixels of `view`.
fn pixel_rays(view: &ViewState, n: u32) -> Vec<DVec3> {
    let fwd = view.direction().normalize();
    let right = fwd.cross(view.position().normalize()).normalize();
    let up = right.cross(fwd);
    let ty = (FOV_DEG.to_radians() / 2.0).tan();
    let tx = ty * VIEWPORT.0 / VIEWPORT.1;
    let mut out = Vec::new();
    for i in 0..=n {
        for j in 0..=n {
            let x = (f64::from(i) / f64::from(n) * 2.0 - 1.0) * tx * 0.999;
            let y = (f64::from(j) / f64::from(n) * 2.0 - 1.0) * ty * 0.999;
            out.push((fwd + right * x + up * y).normalize());
        }
    }
    out
}

fn rect_of(id: TileId) -> tuile_terrain::tiling::GeoRect {
    let (level, x, y) = id.terrain_coord();
    GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y))
}

/// No selected tile's box is wholly behind the planet: none of the level
/// 4–6 tiles the picture of 2026-11-24 drew for nothing.
#[test]
fn no_tile_wholly_behind_the_planet_is_selected() {
    let tree = offline_terrain();
    let view = view(18_500.0, 22.7);
    let selected = converged(&tree, &view);
    let behind: Vec<(u32, u64, u64)> = selected
        .iter()
        .filter(|id| match tree.properties(**id).bounding_volume {
            BoundingVolume::Obb(b) => b
                .corners()
                .iter()
                .all(|c| behind_the_planet(view.position(), *c)),
            BoundingVolume::Sphere(_) => false,
        })
        .map(|id| id.terrain_coord())
        .collect();
    let mut levels: Vec<u32> = behind.iter().map(|t| t.0).collect();
    levels.sort_unstable();
    let mut by_level = std::collections::BTreeMap::<u32, usize>::new();
    for id in &selected {
        *by_level.entry(id.terrain_coord().0).or_default() += 1;
    }
    eprintln!(
        "{} tiles selected {by_level:?}, {} wholly behind the planet, at levels {levels:?}",
        selected.len(),
        behind.len()
    );
    assert!(
        behind.is_empty(),
        "{} of {} selected tiles are wholly behind the planet (levels {levels:?})",
        behind.len(),
        selected.len()
    );
}

/// Every pixel that sees the ground sees a selected tile — from the pose
/// of the picture, and from an eye looking at the horizon itself, where the
/// tiles straddling it are the ones a tighter test could lose.
#[test]
fn every_pixel_that_sees_the_ground_sees_a_selected_tile() {
    let tree = offline_terrain();
    for (height, pitch) in [(18_500.0, 22.7), (18_500.0, 2.0), (1_500.0, 0.5)] {
        let view = view(height, pitch);
        let rects: Vec<_> = converged(&tree, &view).into_iter().map(rect_of).collect();
        let mut seen = 0;
        for dir in pixel_rays(&view, 48) {
            let Some(p) = ground_hit(view.position(), dir) else {
                continue;
            };
            seen += 1;
            let g = ecef_to_geodetic(p);
            assert!(
                rects.iter().any(|r| g.lon >= r.west
                    && g.lon <= r.east
                    && g.lat >= r.south
                    && g.lat <= r.north),
                "from {height} m, {pitch}° down: ground at {:.3}°, {:.3}° is seen and \
                 covered by nothing selected",
                g.lon.to_degrees(),
                g.lat.to_degrees()
            );
        }
        assert!(seen > 500, "only {seen} pixels see the ground");
    }
}
