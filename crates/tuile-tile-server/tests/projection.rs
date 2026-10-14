// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Projections: a scene's slice of the store, fetched once into local
//! archives, read before the bucket.

mod common;

use common::*;
use tuile_tile_server::projection::{bounds, DEFAULT_TILE_FACTOR};
use tuile_tile_server::{Eye, Footprint, Grid, TileStore};

/// The centre of a Web Mercator tile, as an eye position at `height`.
fn eye_over(level: u8, x: u32, y: u32, height: f64) -> Eye {
    let [w, s, e, n] = bounds(Grid::WebMercator, level, x, y);
    Eye {
        lon: (w + e) / 2.0,
        lat: (s + n) / 2.0,
        height,
    }
}

/// A zone filled at `LEVEL` (every tile) and a coarse tile in `top`,
/// published, then a store opened fresh on the same bucket.
async fn published() -> (std::sync::Arc<dyn object_store::ObjectStore>, TestClock) {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 0))
            .await
            .expect("put");
    }
    s.put(IMAGERY, 3, 4, 2, body(3, 4, 2, 0))
        .await
        .expect("put");
    s.put(IMAGERY, 3, 0, 7, body(3, 0, 7, 0))
        .await
        .expect("put");
    s.flush_all().await.expect("flush");
    (objects, clock)
}

#[tokio::test]
async fn a_projection_keeps_what_the_scene_can_see_and_reads_it_locally() {
    let (objects, clock) = published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    // An eye 1 km above the ground, over the zone's first tile.
    let eye = eye_over(LEVEL, X0, Y0, 5_800.0);
    let fp = Footprint::from_eyes([eye], DEFAULT_TILE_FACTOR);
    let report = s.project(&fp, dir.path()).await.expect("project");
    assert!(
        report.zones_projected >= 2,
        "the cell and the top zone: {report:?}"
    );
    assert!(
        report.tiles_kept > 0 && report.tiles_kept == report.tiles_listed,
        "reached zones kept whole: {report:?}"
    );
    assert_eq!(
        report.requests as usize, report.zones_seen,
        "one request per archive, one archive per zone here: {report:?}"
    );

    // The tile under the eye: from the projection, bytes identical.
    assert_eq!(
        s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"),
        Some(body(LEVEL, X0, Y0, 0))
    );
    let stats = s.stats();
    assert_eq!((stats.projection, stats.remote), (1, 0), "{stats:?}");

    // The top zone is kept whole: the coarse tile over the Pacific too, since
    // a bake pins the coarse pyramid of the whole globe.
    assert_eq!(
        s.get(IMAGERY, 3, 4, 2).await.expect("get"),
        Some(body(3, 4, 2, 0))
    );
    assert_eq!(
        s.get(IMAGERY, 3, 0, 7).await.expect("get"),
        Some(body(3, 0, 7, 0))
    );
    let stats = s.stats();
    assert_eq!(stats.projection, 3, "{stats:?}");
    assert_eq!((stats.projection_misses, stats.remote), (0, 0), "{stats:?}");
}

#[tokio::test]
async fn every_tile_of_every_projection_is_the_bucket_s_tile() {
    let (objects, clock) = published().await;
    let reference = store_on(objects.clone(), &clock, eager());
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    let fp = Footprint::from_eyes(
        [eye_over(LEVEL, X0 + 8, Y0 + 8, 20_000.0)],
        DEFAULT_TILE_FACTOR,
    );
    s.project(&fp, dir.path()).await.expect("project");
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        assert_eq!(
            s.get(IMAGERY, LEVEL, x, y).await.expect("get"),
            reference.get(IMAGERY, LEVEL, x, y).await.expect("get")
        );
    }
    assert!(s.stats().projection > 0);
}

#[tokio::test]
async fn the_newest_archive_wins_in_a_projection_too() {
    let (objects, clock) = published().await;
    let w = store_on(objects.clone(), &clock, eager());
    w.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 9))
        .await
        .expect("put");
    w.flush_all().await.expect("flush");
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    s.project(
        &Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], DEFAULT_TILE_FACTOR),
        dir.path(),
    )
    .await
    .expect("project");
    assert_eq!(
        s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"),
        Some(body(LEVEL, X0, Y0, 9))
    );
    assert_eq!(s.stats().projection, 1);
}

#[tokio::test]
async fn projections_are_plain_archives_on_disk() {
    let (objects, clock) = published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    s.project(
        &Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], DEFAULT_TILE_FACTOR),
        dir.path(),
    )
    .await
    .expect("project");
    let local =
        tuile_tile_server::projection::projection_path(dir.path(), &common::imagery(), zone());
    let reader = tuile_tile_server::archive::open_local(&local)
        .await
        .expect("a readable local archive");
    let tile = reader
        .get_tile(
            pmtiles::TileId::new(Grid::WebMercator.to_id(LEVEL, X0, Y0).expect("id")).expect("id"),
        )
        .await
        .expect("get");
    assert_eq!(tile, Some(body(LEVEL, X0, Y0, 0)));
}

#[tokio::test]
async fn a_reached_cell_zone_is_projected_whole() {
    let (objects, clock) = published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    // Low over one corner: the far corner's fine tiles are still kept.
    s.project(
        &Footprint::from_eyes([eye_over(LEVEL, X0, Y0, 5_800.0)], 1.0),
        dir.path(),
    )
    .await
    .expect("project");
    let far = in_zone(ZONE_SIDE * ZONE_SIDE - 1);
    assert_eq!(
        s.get(IMAGERY, LEVEL, far.0, far.1).await.expect("get"),
        Some(body(LEVEL, far.0, far.1, 0))
    );
    let stats = s.stats();
    assert_eq!(
        (stats.projection, stats.projection_misses, stats.remote),
        (1, 0, 0),
        "{stats:?}"
    );
}

#[tokio::test]
async fn a_scene_far_away_projects_nothing_of_the_zone() {
    let (objects, clock) = published().await;
    let s: TileStore = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    // Low over the Pacific.
    let fp = Footprint::from_eyes(
        [Eye {
            lon: -150.0,
            lat: -20.0,
            height: 1_000.0,
        }],
        DEFAULT_TILE_FACTOR,
    );
    let report = s.project(&fp, dir.path()).await.expect("project");
    assert!(!tuile_tile_server::projection::projection_path(
        dir.path(),
        &common::imagery(),
        zone()
    )
    .exists());
    // Only the top zone, whole.
    assert_eq!(report.zones_projected, 1, "{report:?}");
}

// ── A targeted layer: terrain, cut tile by tile ─────────────────────────────

/// The terrain level the targeted tests write at, and the side of one zone
/// at it.
const T_LEVEL: u8 = 13;
const T_SIDE: u32 = 1 << (T_LEVEL - TERRAIN_ZONE_LEVEL);
/// A terrain zone over the Pyrenees, and one 100 km east of it.
const TZ: (u32, u32) = (513, 377);
const TZ_EAST: (u32, u32) = (516, 377);
/// The coarse tiles put in the terrain's `top`: one under the scene, one on
/// the far side of the globe.
const TOP_NEAR: (u8, u32, u32) = (3, 8, 5);
const TOP_FAR: (u8, u32, u32) = (3, 0, 0);

/// The `i`-th tile at [`T_LEVEL`] of the zone `z`.
fn t_tile(z: (u32, u32), i: u32) -> (u32, u32) {
    (z.0 * T_SIDE + i % T_SIDE, z.1 * T_SIDE + i / T_SIDE)
}

/// Tiles the size of real terrain, so that a byte range spans a few of them.
fn fat(level: u8, x: u32, y: u32, version: u32) -> bytes::Bytes {
    let mut v = body(level, x, y, version).to_vec();
    v.extend(std::iter::repeat_n(b'#', 4096));
    bytes::Bytes::from(v)
}

/// An eye at `height` over the centre of a geographic tile.
fn geo_eye_over(level: u8, x: u32, y: u32, height: f64) -> Eye {
    let [w, s, e, n] = bounds(Grid::Geographic, level, x, y);
    Eye {
        lon: (w + e) / 2.0,
        lat: (s + n) / 2.0,
        height,
    }
}

/// Two terrain zones filled at [`T_LEVEL`], and two coarse tiles in `top`.
async fn terrain_published() -> (std::sync::Arc<dyn object_store::ObjectStore>, TestClock) {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    for z in [TZ, TZ_EAST] {
        for i in 0..T_SIDE * T_SIDE {
            let (x, y) = t_tile(z, i);
            s.put(TERRAIN, T_LEVEL, x, y, fat(T_LEVEL, x, y, 0))
                .await
                .expect("put");
        }
    }
    for (l, x, y) in [TOP_NEAR, TOP_FAR] {
        s.put(TERRAIN, l, x, y, fat(l, x, y, 0)).await.expect("put");
    }
    s.flush_all().await.expect("flush");
    (objects, clock)
}

/// One kilometre above the ground, over the first tile of [`TZ`]; a factor
/// of one tile width.
fn corner_footprint() -> Footprint {
    let (x, y) = t_tile(TZ, 0);
    Footprint::from_eyes([geo_eye_over(T_LEVEL, x, y, 5_800.0)], 1.0).with_targeted_layer(TERRAIN)
}

#[tokio::test]
async fn a_targeted_zone_fetches_only_the_tiles_the_footprint_keeps() {
    let (objects, clock) = terrain_published().await;
    let counting = Counting::over(objects);
    let s = store_on(counting.clone(), &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    let fp = corner_footprint();
    let report = s.project(&fp, dir.path()).await.expect("project");
    let terrain = report.layers.get(TERRAIN).expect("the terrain's report");

    let kept: Vec<(u32, u32)> = (0..T_SIDE * T_SIDE)
        .map(|i| t_tile(TZ, i))
        .filter(|&(x, y)| fp.keeps_with(1.0, Grid::Geographic, T_LEVEL, x, y))
        .collect();
    assert!(
        !kept.is_empty() && kept.len() < (T_SIDE * T_SIDE / 8) as usize,
        "a corner of the zone: {}",
        kept.len()
    );
    assert!(terrain.targeted, "{terrain:?}");
    assert_eq!(
        terrain.tiles_kept - terrain.top_tiles,
        kept.len() as u64,
        "the kept tiles and no other: {terrain:?}"
    );
    // The zone weighs a megabyte; what came down is its directory and a few
    // ranges around the kept tiles.
    let cell_bytes = terrain.bytes - terrain.top_bytes;
    let cell_whole = terrain.whole_bytes - terrain.top_bytes;
    assert!(
        cell_bytes * 8 < cell_whole,
        "{cell_bytes} bytes of {cell_whole}: {terrain:?}"
    );
    // Every byte read of an archive is in the report, and every request.
    assert_eq!(counting.archive_bytes(), report.bytes, "{report:?}");
    assert_eq!(counting.archive_gets(), report.requests, "{report:?}");
    // The zone 100 km east is not reached at one width: not even listed.
    assert_eq!(
        terrain.zones_seen, 2,
        "the corner's zone and top: {terrain:?}"
    );

    // What was kept is read from the projection, the bucket's bytes; what
    // was left out is not an error — the bucket answers, and it is counted.
    for i in 0..T_SIDE * T_SIDE {
        let (x, y) = t_tile(TZ, i);
        assert_eq!(
            s.get(TERRAIN, T_LEVEL, x, y).await.expect("get"),
            Some(fat(T_LEVEL, x, y, 0))
        );
    }
    let stats = s.stats();
    assert_eq!(stats.projection, kept.len() as u64, "{stats:?}");
    assert_eq!(
        stats.projection_misses,
        u64::from(T_SIDE * T_SIDE) - kept.len() as u64,
        "{stats:?}"
    );
}

#[tokio::test]
async fn an_untargeted_layer_still_takes_its_zones_whole() {
    let (objects, clock) = terrain_published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    let (x, y) = t_tile(TZ, 0);
    let fp = Footprint::from_eyes([geo_eye_over(T_LEVEL, x, y, 5_800.0)], 1.0);
    let report = s.project(&fp, dir.path()).await.expect("project");
    let terrain = report.layers.get(TERRAIN).expect("the terrain's report");
    assert!(!terrain.targeted);
    assert_eq!(terrain.tiles_kept, terrain.tiles_listed, "{terrain:?}");
    assert_eq!(terrain.bytes, terrain.whole_bytes, "{terrain:?}");
}

#[tokio::test]
async fn the_newest_archive_wins_in_a_targeted_zone() {
    let (objects, clock) = terrain_published().await;
    let (x, y) = t_tile(TZ, 0);
    let w = store_on(objects.clone(), &clock, eager());
    w.put(TERRAIN, T_LEVEL, x, y, fat(T_LEVEL, x, y, 9))
        .await
        .expect("put");
    w.flush_all().await.expect("flush");
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    let report = s
        .project(&corner_footprint(), dir.path())
        .await
        .expect("project");
    assert!(
        report.layers[TERRAIN].tiles_listed > u64::from(T_SIDE * T_SIDE),
        "two archives listed: {report:?}"
    );
    assert_eq!(
        s.get(TERRAIN, T_LEVEL, x, y).await.expect("get"),
        Some(fat(T_LEVEL, x, y, 9))
    );
    let stats = s.stats();
    assert_eq!(
        (stats.projection, stats.projection_misses),
        (1, 0),
        "{stats:?}"
    );
}

#[tokio::test]
async fn the_top_of_a_targeted_layer_stays_whole() {
    let (objects, clock) = terrain_published().await;
    let s = store_on(objects, &clock, eager());
    let dir = tempfile::tempdir().expect("dir");
    let report = s
        .project(&corner_footprint(), dir.path())
        .await
        .expect("project");
    assert_eq!(report.layers[TERRAIN].top_tiles, 2, "{report:?}");
    // The far side of the globe, at one tile width: kept all the same.
    for (l, x, y) in [TOP_NEAR, TOP_FAR] {
        assert_eq!(
            s.get(TERRAIN, l, x, y).await.expect("get"),
            Some(fat(l, x, y, 0))
        );
    }
    let stats = s.stats();
    assert_eq!(
        (stats.projection, stats.projection_misses),
        (2, 0),
        "{stats:?}"
    );
}

#[tokio::test]
async fn a_targeted_projection_is_the_whole_one_where_both_keep_a_tile() {
    let (objects, clock) = terrain_published().await;
    let whole = store_on(objects.clone(), &clock, eager());
    let targeted = store_on(objects, &clock, eager());
    let (x, y) = t_tile(TZ, T_SIDE * T_SIDE / 2 + T_SIDE / 2);
    let eye = geo_eye_over(T_LEVEL, x, y, 9_000.0);
    let (a, b) = (
        tempfile::tempdir().expect("dir"),
        tempfile::tempdir().expect("dir"),
    );
    whole
        .project(&Footprint::from_eyes([eye], 3.0), a.path())
        .await
        .expect("project");
    targeted
        .project(
            &Footprint::from_eyes([eye], 3.0).with_targeted_layer(TERRAIN),
            b.path(),
        )
        .await
        .expect("project");
    for i in 0..T_SIDE * T_SIDE {
        let (x, y) = t_tile(TZ, i);
        assert_eq!(
            targeted.get(TERRAIN, T_LEVEL, x, y).await.expect("get"),
            whole.get(TERRAIN, T_LEVEL, x, y).await.expect("get"),
        );
    }
    let (w, t) = (whole.stats(), targeted.stats());
    assert_eq!(w.projection, u64::from(T_SIDE * T_SIDE), "{w:?}");
    assert!(t.projection > 0 && t.projection < w.projection, "{t:?}");
}
