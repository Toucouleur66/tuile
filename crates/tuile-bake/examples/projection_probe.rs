// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a projection would fetch for a pack's cameras, read from the tile
//! store's manifests and directories alone — no tile is fetched.
//!
//! For each layer: the zones the cameras reach and what they weigh whole.
//! For the targeted layer (the terrain): at each factor asked, the tiles a
//! footprint keeps, the bytes and requests of fetching them by coalesced
//! ranges, and how many of the tiles the pack selected it would leave out.
//! For every tile the pack selected: the smallest factor that keeps it.
//!
//! ```text
//! TUILE_TILES_BUCKET=… TUILE_STORE_ENDPOINT=… TUILE_STORE_ACCESS_KEY_ID=… \
//! TUILE_STORE_SECRET_ACCESS_KEY=… cargo run --release -p tuile-bake \
//!     --example projection_probe -- <pack.tuilepack> [factor…]
//! ```
//!
//! With `--reads <log> <first>:<last>` instead of factors: the smallest
//! factor each read logged by a bake of those frames needed
//! (`TILES-REMOTE` and `TILES-PROJECTION-MISS` lines, at the debug level of
//! `tuile_tile_server`), by layer and level — what the footprint must reach.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use futures_util::StreamExt;
use object_store::aws::AmazonS3Builder;
use tuile_tile_server::projection::{bounds, coalesce, HIGHEST_GROUND_M};
use tuile_tile_server::{Eye, Footprint, Grid, StoreConfig, TileStore, Zone};

const EARTH_RADIUS_M: f64 = 6_371_008.8;
const M_PER_DEG: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

fn distance(a_lon: f64, a_lat: f64, b_lon: f64, b_lat: f64) -> f64 {
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dp = p2 - p1;
    let dl = (b_lon - a_lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

/// The smallest factor at which some eye keeps the tile, as the footprint
/// measures (no slack).
fn needed(eyes: &[Eye], grid: Grid, level: u8, x: u32, y: u32) -> f64 {
    let [w, s, e, n] = bounds(grid, level, x, y);
    let mid_lat = ((s + n) / 2.0).to_radians();
    let width = ((e - w) * M_PER_DEG * mid_lat.cos()).max((n - s) * M_PER_DEG);
    eyes.iter()
        .map(|eye| {
            let ground = distance(eye.lon, eye.lat, eye.lon.clamp(w, e), eye.lat.clamp(s, n));
            let above = (eye.height - HIGHEST_GROUND_M).max(0.0);
            (ground * ground + above * above).sqrt() / width
        })
        .fold(f64::INFINITY, f64::min)
}

fn quantiles(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "none".into();
    }
    v.sort_by(f64::total_cmp);
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!(
        "n={} p50={:.2} p90={:.2} p99={:.2} p99.9={:.2} max={:.2}",
        v.len(),
        q(0.5),
        q(0.9),
        q(0.99),
        q(0.999),
        q(1.0)
    )
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .first()
        .ok_or("usage: projection_probe <pack> [factor…]")?;
    if args.get(1).map(String::as_str) == Some("--reads") {
        let (log, frames) = (
            args.get(2).ok_or("--reads <log> <first>:<last>")?,
            args.get(3).ok_or("--reads <log> <first>:<last>")?,
        );
        return reads(path, log, frames);
    }
    let factors: Vec<f64> = if args.len() > 1 {
        args[1..]
            .iter()
            .map(|f| f.parse())
            .collect::<Result<_, _>>()?
    } else {
        vec![2.0, 4.0, 6.0, 8.0, 12.0, 16.0, 20.0, 24.0]
    };

    // The pack: its cameras and the tiles it selected.
    let bytes = std::fs::read(path)?;
    let pack = tuile_pack::Pack::open(&bytes)?;
    let (terrain_layer, imagery_layer) = pack.store_layers().ok_or("a pack with no references")?;
    let views = pack.views();
    let eyes: Vec<Eye> = views
        .iter()
        .map(|(_, v)| {
            let g = tuile_core::geo::ecef_to_geodetic(glam::DVec3::from_array(v.position));
            Eye {
                lon: g.lon.to_degrees(),
                lat: g.lat.to_degrees(),
                height: g.height,
            }
        })
        .collect();
    let (h_min, h_max) = eyes.iter().fold((f64::MAX, f64::MIN), |(a, b), e| {
        (a.min(e.height), b.max(e.height))
    });
    let v0 = views.first().ok_or("no frame")?.1;
    println!(
        "PACK frames={} viewport={}x{} fovy={:.4} eyes_height={h_min:.0}..{h_max:.0} m terrain={terrain_layer} imagery={imagery_layer}",
        views.len(),
        v0.viewport_px[0],
        v0.viewport_px[1],
        v0.fovy_rad
    );
    let mut terrain: BTreeSet<(u8, u32, u32)> = BTreeSet::new();
    let mut imagery: BTreeSet<(u8, u32, u32)> = BTreeSet::new();
    let mut selections = 0usize;
    for (frame, _) in &views {
        for tile in pack.frame(*frame)? {
            selections += 1;
            if let Some(r) = tuile_pack::refs_of(&tile) {
                terrain.insert((r.terrain.level, r.terrain.x, r.terrain.y));
                for p in r.imagery {
                    imagery.insert((p.tile.level, p.tile.x, p.tile.y));
                }
            }
        }
    }
    println!(
        "SELECTED selections={selections} terrain_tiles={} imagery_tiles={}",
        terrain.len(),
        imagery.len()
    );

    // The store.
    let s3 = AmazonS3Builder::new()
        .with_bucket_name(var("TUILE_TILES_BUCKET")?)
        .with_endpoint(var("TUILE_STORE_ENDPOINT")?)
        .with_region("auto")
        .with_access_key_id(var("TUILE_STORE_ACCESS_KEY_ID")?)
        .with_secret_access_key(var("TUILE_STORE_SECRET_ACCESS_KEY")?)
        .build()?;
    let store = Arc::new(TileStore::open(Arc::new(s3), StoreConfig::default()).await?);
    let t_grid = store.layer(terrain_layer)?.grid;
    let i_grid = store.layer(imagery_layer)?.grid;

    // The factor each selected tile needs, by level.
    for (name, grid, tiles) in [
        (terrain_layer, t_grid, &terrain),
        (imagery_layer, i_grid, &imagery),
    ] {
        let mut by_level: BTreeMap<u8, Vec<f64>> = BTreeMap::new();
        for &(l, x, y) in tiles {
            by_level
                .entry(l)
                .or_default()
                .push(needed(&eyes, grid, l, x, y));
        }
        let all: Vec<f64> = by_level.values().flatten().copied().collect();
        println!("NEEDED {name} {}", quantiles(all));
        for (l, v) in by_level {
            println!("NEEDED {name} level={l} {}", quantiles(v));
        }
    }

    // Zones reached, and what they weigh whole (the footprint's horizon; the
    // factor does not choose zones of an untargeted layer).
    let all_zones = store.zones().await?;
    let horizon = Footprint::from_eyes(eyes.iter().copied(), 1.0);
    let reached: Vec<(String, Zone)> = all_zones
        .into_iter()
        .filter(|(l, z)| {
            store.layer(l).is_ok_and(|layer| {
                // Reaching by the horizon: a footprint of no target does it.
                matches!(z, Zone::Top) || {
                    let Zone::Cell { x, y } = *z else {
                        unreachable!()
                    };
                    let [w, s, e, n] = bounds(layer.grid, layer.zone_level, x, y);
                    horizon.eyes().iter().any(|eye| {
                        let ground =
                            distance(eye.lon, eye.lat, eye.lon.clamp(w, e), eye.lat.clamp(s, n));
                        let h = (2.0 * EARTH_RADIUS_M * eye.height.max(0.0) + eye.height.powi(2))
                            .sqrt()
                            + (2.0 * EARTH_RADIUS_M * HIGHEST_GROUND_M).sqrt();
                        ground <= h
                    })
                }
            })
        })
        .collect();
    let weights: Vec<(String, Zone, u64, usize)> = futures_util::stream::iter(reached.clone())
        .map(|(l, z)| {
            let store = store.clone();
            async move {
                let layer = store.layer(&l).expect("layer").clone();
                let m = tuile_tile_server::manifest::read(
                    store.object_store().as_ref(),
                    &layer.zone_prefix(z),
                )
                .await
                .expect("manifest")
                .manifest;
                let now = store.now();
                let live: Vec<_> = m
                    .archives
                    .iter()
                    .filter(|a| !layer.is_expired(&a.epoch, now))
                    .collect();
                (l, z, live.iter().map(|a| a.bytes).sum::<u64>(), live.len())
            }
        })
        .buffer_unordered(32)
        .collect()
        .await;
    let mut per_layer: BTreeMap<String, (usize, u64, usize, u64)> = BTreeMap::new();
    for (l, z, bytes, archives) in &weights {
        let e = per_layer.entry(l.clone()).or_default();
        e.0 += 1;
        e.1 += bytes;
        e.2 += archives;
        if *z == Zone::Top {
            e.3 += bytes;
        }
    }
    let mut total = 0;
    for (l, (zones, bytes, archives, top)) in &per_layer {
        total += bytes;
        println!(
            "WHOLE layer={l} zones={zones} archives={archives} mb={:.0} top_mb={:.0}",
            *bytes as f64 / 1e6,
            *top as f64 / 1e6
        );
    }
    println!("WHOLE total_mb={:.0}", total as f64 / 1e6);

    // The terrain's cell zones, listed.
    let layer = store.layer(terrain_layer)?.clone();
    let cells: Vec<Zone> = reached
        .iter()
        .filter(|(l, z)| l == terrain_layer && *z != Zone::Top)
        .map(|(_, z)| *z)
        .collect();
    let listings: Vec<(Zone, tuile_tile_server::ZoneListing)> = futures_util::stream::iter(cells)
        .map(|z| {
            let (store, layer) = (store.clone(), layer.clone());
            async move { (z, store.zone_listing(&layer, z).await.expect("listing")) }
        })
        .buffer_unordered(16)
        .collect()
        .await;
    let (list_req, list_bytes, listed): (u64, u64, u64) =
        listings.iter().fold((0, 0, 0), |a, (_, l)| {
            (a.0 + l.requests, a.1 + l.bytes, a.2 + l.tiles.len() as u64)
        });
    println!(
        "LISTED {terrain_layer} zones={} tiles={listed} requests={list_req} mb={:.2}",
        listings.len(),
        list_bytes as f64 / 1e6
    );
    let held: HashSet<u64> = listings
        .iter()
        .flat_map(|(_, l)| l.tiles.keys().copied())
        .collect();
    let cell_selected: Vec<(u8, u32, u32)> = terrain
        .iter()
        .copied()
        .filter(|&(l, _, _)| l >= layer.zone_level)
        .collect();
    let unheld = cell_selected
        .iter()
        .filter(|&&(l, x, y)| !held.contains(&layer.grid.to_id(l, x, y).expect("id")))
        .count();
    println!(
        "SELECTED-IN-CELLS {} of which not in the reached zones: {unheld}",
        cell_selected.len()
    );

    for f in factors {
        let fp = Footprint::from_eyes(eyes.iter().copied(), f).with_targeted_layer(terrain_layer);
        let (mut zones, mut kept, mut bytes, mut requests, mut fetched) =
            (0, 0u64, 0u64, 0u64, 0u64);
        let mut levels: BTreeMap<u8, u64> = BTreeMap::new();
        for (z, l) in &listings {
            let Zone::Cell { x, y } = *z else { continue };
            if !fp.keeps_with(f, layer.grid, layer.zone_level, x, y) {
                continue;
            }
            zones += 1;
            requests += l.requests;
            fetched += l.bytes;
            let mut spans: BTreeMap<usize, Vec<(u64, u64)>> = BTreeMap::new();
            for (id, (archive, range)) in &l.tiles {
                let Some((lv, tx, ty)) = layer.grid.from_id(*id) else {
                    continue;
                };
                if fp.keeps_with(f, layer.grid, lv, tx, ty) {
                    kept += 1;
                    bytes += range.end - range.start;
                    *levels.entry(lv).or_default() += 1;
                    spans
                        .entry(*archive)
                        .or_default()
                        .push((range.start, range.end - range.start));
                }
            }
            for (_, s) in spans {
                for r in coalesce(s) {
                    requests += 1;
                    fetched += r.end - r.start;
                }
            }
        }
        let missed = cell_selected
            .iter()
            .filter(|&&(l, x, y)| !fp.keeps_with(f, layer.grid, l, x, y))
            .count();
        println!(
            "FACTOR {f} zones={zones} tiles={kept} tile_mb={:.1} fetched_mb={:.1} requests={requests} selected_left_out={missed} levels={levels:?}",
            bytes as f64 / 1e6,
            fetched as f64 / 1e6
        );
    }
    Ok(())
}

/// The eyes of a pack's frames `first..=last`.
fn eyes_of(pack: &tuile_pack::Pack<'_>, first: u32, last: u32) -> Vec<Eye> {
    pack.views()
        .into_iter()
        .filter(|(n, _)| (first..=last).contains(n))
        .map(|(_, v)| {
            let g = tuile_core::geo::ecef_to_geodetic(glam::DVec3::from_array(v.position));
            Eye {
                lon: g.lon.to_degrees(),
                lat: g.lat.to_degrees(),
                height: g.height,
            }
        })
        .collect()
}

/// The factor each logged read needed.
fn reads(path: &str, log: &str, frames: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let pack = tuile_pack::Pack::open(&bytes)?;
    let (terrain_layer, imagery_layer) = pack.store_layers().ok_or("a pack with no references")?;
    let (first, last) = frames.split_once(':').ok_or("<first>:<last>")?;
    let eyes = eyes_of(&pack, first.parse()?, last.parse()?);
    let field = |line: &str, name: &str| -> Option<String> {
        let at = line.find(&format!(" {name}="))? + name.len() + 2;
        let rest = &line[at..];
        Some(rest.split(' ').next()?.trim_matches('"').to_string())
    };
    let mut by: BTreeMap<(String, String, u8), Vec<f64>> = BTreeMap::new();
    let mut seen: HashSet<(String, u8, u32, u32)> = HashSet::new();
    for line in std::fs::read_to_string(log)?.lines() {
        let kind = if line.contains("TILES-REMOTE") {
            "remote"
        } else if line.contains("TILES-PROJECTION-MISS") {
            "miss"
        } else {
            continue;
        };
        let (Some(layer), Some(level), Some(x), Some(y)) = (
            field(line, "layer"),
            field(line, "level"),
            field(line, "x"),
            field(line, "y"),
        ) else {
            continue;
        };
        let (level, x, y): (u8, u32, u32) = (level.parse()?, x.parse()?, y.parse()?);
        if !seen.insert((layer.clone(), level, x, y)) {
            continue;
        }
        let grid = if layer == terrain_layer {
            Grid::Geographic
        } else if layer == imagery_layer {
            Grid::WebMercator
        } else {
            continue;
        };
        by.entry((layer, kind.to_string(), level))
            .or_default()
            .push(needed(&eyes, grid, level, x, y));
    }
    let mut all: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for ((layer, kind, level), v) in &by {
        println!(
            "READS {layer} {kind} level={level} {}",
            quantiles(v.clone())
        );
        all.entry((layer.clone(), kind.clone()))
            .or_default()
            .extend(v);
    }
    for ((layer, kind), v) in all {
        println!("READS {layer} {kind} all {}", quantiles(v));
    }
    Ok(())
}
