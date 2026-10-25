// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the ground the pump draws, read back on the CPU.
//!
//! With [`ContentPump::keep_contents`] the pump keeps each resident mesh's
//! decoded content, and says which it drew. A [`Surface`] made from the two
//! answers heights from **the triangles drawn** — checked here against a
//! brute-force intersection with every triangle of the tile, on real
//! quantized-mesh tiles cut from a height field — falls back to the resident
//! parent with its level when the drawn tile is evicted, and changes the
//! generation it answers with when the tile answering changes.
//!
//! A machine without a usable adapter skips, with a printed reason.

mod support;

use glam::DVec3;
use support::{exact, gpu, mesh, select, Scripted};
use tuile_core::budget::{FrameDeadline, ResourceId};
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef, Geodetic};
use tuile_core::protocol::ServerMessage;
use tuile_core::source::TileId;
use tuile_core::surface::{Resident, Surface};
use tuile_terrain::{
    decode, to_ground, GeoRect, GeographicTilingScheme, HeightfieldSource, Heights, TileCoord,
};
use tuile_wgpu::ContentPump;

const LON: f64 = 10.0;
const LAT: f64 = 0.0;

/// Ridges and a hill, steep enough that a sampled answer would show.
struct Relief;

impl Heights for Relief {
    fn height(&self, lon: f64, lat: f64) -> f64 {
        let (e, n) = ((lon - LON) * 111_319.5, (lat - LAT) * 110_574.3);
        300.0 * (-(e * e + n * n) / (2.0 * 800.0 * 800.0)).exp()
            + 40.0 * (e / 150.0).sin() * (n / 210.0).cos()
    }

    fn footprint(&self) -> GeoRect {
        GeoRect::from_degrees(LON - 0.05, LAT - 0.05, LON + 0.05, LAT + 0.05)
    }
}

/// The terrain tile at `level` over the map's centre.
fn coord(level: u32) -> TileCoord {
    let width = 180.0 / f64::from(1u32 << level);
    TileCoord::new(
        level,
        ((LON + 180.0) / width).floor() as u64,
        ((LAT + 90.0) / width).floor() as u64,
    )
}

fn content(source: &HeightfieldSource<Relief>, c: TileCoord) -> DecodedTileContent {
    let bytes = source.tile(c).expect("a tile");
    let rect = GeographicTilingScheme::default().tile_rect(c);
    to_ground(&decode(&bytes).expect("decoded"), &rect)
}

/// A resident mesh as the Bevy plugin hands it over: a terrain tile, its
/// grid rectangle known.
fn resident(t: TileId, level: u32, content: &std::sync::Arc<DecodedTileContent>) -> Resident<'_> {
    let (z, x, y) = t.terrain_coord();
    let r = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    Resident {
        key: t.0,
        level,
        content,
        rect: Some([r.west, r.south, r.east, r.north]),
    }
}

fn id(c: TileCoord) -> TileId {
    TileId::from_terrain(c.level, c.x, c.y)
}

/// Every triangle of `content`, the highest a vertical line through the
/// place meets: what the drawn surface is there, by brute force.
fn brute_force(content: &DecodedTileContent, lon: f64, lat: f64) -> Option<f64> {
    let ground = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let up = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 1.0,
    }) - ground;
    let from = ground + up * 20_000.0;
    let mut best: Option<f64> = None;
    for m in &content.meshes {
        let p = |i: u32| {
            let v = m.positions[i as usize];
            content.local_origin_ecef
                + DVec3::new(f64::from(v[0]), f64::from(v[1]), f64::from(v[2]))
        };
        for tri in m.indices.as_chunks::<3>().0 {
            let (a, b, c) = (p(tri[0]), p(tri[1]), p(tri[2]));
            let (e1, e2) = (b - a, c - a);
            let h = (-up).cross(e2);
            let det = e1.dot(h);
            if det.abs() < 1e-12 {
                continue;
            }
            let s = from - a;
            let u = s.dot(h) / det;
            let q = s.cross(e1);
            let v = (-up).dot(q) / det;
            if u < 0.0 || v < 0.0 || u + v > 1.0 {
                continue;
            }
            let t = e2.dot(q) / det;
            let height = 20_000.0 - t;
            best = Some(best.map_or(height, |b: f64| b.max(height)));
        }
    }
    best
}

/// Places spread over the inside of tile `c`, radians.
fn places(c: TileCoord) -> Vec<(f64, f64)> {
    let r = GeographicTilingScheme::default().tile_rect(c);
    let mut out = Vec::new();
    for i in 1..20 {
        for j in 1..20 {
            out.push((
                r.west + (r.east - r.west) * f64::from(i) / 20.0,
                r.south + (r.north - r.south) * f64::from(j) / 20.0,
            ));
        }
    }
    out
}

#[test]
fn the_drawn_surface_answers_exactly_falls_back_to_the_parent_and_says_when_it_changed() {
    let Some(gpu) = gpu() else { return };
    let source = HeightfieldSource::new(Relief, 16);
    let (parent, child) = (coord(13), coord(14));
    let (parent_content, child_content) = (content(&source, parent), content(&source, child));
    let child_kept = child_content.clone();

    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.keep_contents(true);
    let mut stream = Scripted::new([
        mesh(id(parent), 1, parent_content),
        mesh(id(child), 2, child_content),
        select(vec![exact(id(child))], 1),
    ]);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    assert_eq!(pump.resident_contents().count(), 2);
    assert!(pump.drawn_tiles().contains(&id(child)));
    assert!(!pump.drawn_tiles().contains(&id(parent)));

    let key = |t: TileId| t.0;
    let drawn: std::collections::HashSet<u64> =
        pump.drawn_tiles().iter().map(|t| key(*t)).collect();
    let started = std::time::Instant::now();
    let surface = Surface::default().with(
        pump.resident_contents()
            .map(|(t, level, c)| resident(t, level, c)),
        &drawn,
    );
    eprintln!("two tiles taken in {:?}", started.elapsed());

    // Exact: the drawn child's triangles, to a millimetre, at every place.
    let mut worst = 0.0f64;
    let mut generations = std::collections::HashSet::new();
    for (lon, lat) in places(child) {
        let got = surface.height_at(lon, lat);
        assert_eq!(got.level, Some(14));
        assert!(got.drawn);
        generations.insert(got.generation);
        let want = brute_force(&child_kept, lon, lat).expect("the child covers its inside");
        worst = worst.max((got.height - want).abs());
        // And the mesh is the relief's, to its sampling.
        let relief = Relief.height(lon.to_degrees(), lat.to_degrees());
        assert!(
            (got.height - relief).abs() < 5.0,
            "{} vs relief {relief}",
            got.height
        );
    }
    assert!(worst < 1e-3, "{worst} m from the drawn triangles");
    // What a host asks every frame: 2 500 places.
    let batch: Vec<(f64, f64)> = places(child).into_iter().cycle().take(2_500).collect();
    let started = std::time::Instant::now();
    let total: f64 = batch
        .iter()
        .map(|&(lon, lat)| surface.height_at(lon, lat).height)
        .sum();
    eprintln!("2 500 heights in {:?} ({total:.0})", started.elapsed());
    assert_eq!(generations.len(), 1, "one tile answered");
    let before = surface.height_at(places(child)[0].0, places(child)[0].1);

    // The child is evicted: its parent answers, says its level, and that it
    // is a resident stand-in rather than what is drawn.
    let mut stream = Scripted::new([ServerMessage::Evict {
        resources: vec![ResourceId::Mesh(id(child))],
    }]);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    assert_eq!(
        pump.resident_contents().count(),
        1,
        "the evicted content is let go"
    );
    let drawn: std::collections::HashSet<u64> =
        pump.drawn_tiles().iter().map(|t| key(*t)).collect();
    let after_surface = surface.with(
        pump.resident_contents()
            .map(|(t, level, c)| resident(t, level, c)),
        &drawn,
    );
    assert!(after_surface.version() > surface.version());
    let after = after_surface.height_at(places(child)[0].0, places(child)[0].1);
    assert_eq!(after.level, Some(13), "{after:?}");
    assert!(!after.drawn);
    assert_ne!(
        after.generation, before.generation,
        "the tile answering changed"
    );
    assert!(
        (after.height - before.height).abs() < 10.0,
        "{after:?} vs {before:?}"
    );

    // A ray from above and to the side, picked on the surface, lands on
    // it — on the parent's triangles, inside the child's square.
    let r = GeographicTilingScheme::default().tile_rect(child);
    let (lon, lat) = (0.5 * (r.west + r.east), 0.5 * (r.south + r.north));
    let target = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let enu = tuile_core::geo::enu_frame(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let eye = target + enu.col(2) * 3_000.0 + enu.col(0) * 1_500.0;
    let hit = after_surface
        .pick(eye, target - eye, 20_000.0)
        .expect("the ray meets the ground");
    let g = ecef_to_geodetic(hit.position);
    let under = after_surface.height_at(g.lon, g.lat);
    assert_eq!(under.level, Some(13));
    assert!(
        (g.height - under.height).abs() < 0.05,
        "{hit:?} over {under:?}"
    );
    assert!(hit.height > 100.0, "on the hill, not under it: {hit:?}");
}

/// Off by default: a pump nobody asked to keep contents keeps none.
#[test]
fn contents_are_kept_only_when_asked() {
    let Some(gpu) = gpu() else { return };
    let source = HeightfieldSource::new(Relief, 16);
    let c = coord(13);
    let mut pump = ContentPump::new(DVec3::ZERO);
    let mut stream = Scripted::new([
        mesh(id(c), 1, content(&source, c)),
        select(vec![exact(id(c))], 1),
    ]);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    assert_eq!(pump.resident_contents().count(), 0);
    assert!(
        pump.drawn_tiles().contains(&id(c)),
        "what is drawn is said either way"
    );
}
