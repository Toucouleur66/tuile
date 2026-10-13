// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The shared tile store, when the job is given one.
//!
//! `TUILE_TILES_BUCKET` names the bucket every bake and the tile server
//! share; the endpoint and credentials are the job's usual `TUILE_STORE_*`.
//! Unset, a bake fetches every tile from its source, as before.
//! `TUILE_TILES_PREFIX` puts the store under a prefix of the bucket.
//!
//! Source tiles are then read from the store when an earlier run (of any job,
//! or the server) fetched them, and each tile fetched now is offered to it.
//! What is stored is what the source served, so the pack does not depend on
//! whether the store was there.

use std::sync::Arc;

use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use tuile_tile_server::projection::DEFAULT_TILE_FACTOR;
use tuile_tile_server::{Eye, Footprint, Grid, LayerDef, StoreConfig, StoreContent, TileStore};

const DEFAULT_REGION: &str = "auto";
/// Projections kept, in GiB, unless `TUILE_TILES_DIR_GIB` says.
const DEFAULT_PROJECTIONS_GIB: u64 = 40;

/// The files under `dir`, oldest first by when they were last written,
/// removed until what is left holds at most `budget` bytes. A projection is
/// written again by every bake that uses it, so the oldest is the one no
/// bake has used for longest. A file still open by another bake is read
/// to its end: removing it only unlinks it. Returns the files removed and
/// their bytes.
fn evict_oldest(dir: &std::path::Path, budget: u64) -> (u64, u64) {
    fn walk(
        dir: &std::path::Path,
        out: &mut Vec<(std::time::SystemTime, u64, std::path::PathBuf)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            // What a projection is being made of, or into: not ours to take.
            if entry.file_name().to_string_lossy().starts_with(".tmp") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), out);
            } else if let Ok(at) = meta.modified() {
                out.push((at, meta.len(), entry.path()));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    let mut held: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    let (mut removed, mut freed) = (0, 0);
    for (_, bytes, path) in files {
        if held <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            held -= bytes;
            removed += 1;
            freed += bytes;
        }
    }
    (removed, freed)
}

/// Where the scene's projections are written, unless `TUILE_TILES_DIR` says.
const DEFAULT_PROJECTION_DIR: &str = "/tmp/tuile-tiles";
/// Imagery is kept eight times farther than terrain: a 2048² drape composes
/// imagery about three levels finer than the terrain tile it covers.
const IMAGERY_FACTOR: f64 = DEFAULT_TILE_FACTOR * 8.0;
/// A bake has no use for other writers' newest deltas: it reads its scene
/// from its projections, and its own tiles from memory.
const BAKE_MANIFEST_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The store, and the runtime its final flush runs on.
pub struct Tiles {
    runtime: tokio::runtime::Runtime,
    store: Arc<TileStore>,
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is not set"))
}

impl Tiles {
    /// Opens the store named by the environment, or `None` when there is none.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_env_with(None)
    }

    /// The same, with `layer` declared in the store's catalog first if the
    /// store does not know it yet: a store keeps a source's tiles only once
    /// it has a layer for them. A layer the store already has is left as the
    /// store describes it. Says whether the layer was added.
    pub fn from_env_with(layer: Option<LayerDef>) -> Result<Option<Self>, String> {
        let Ok(bucket) = var("TUILE_TILES_BUCKET") else {
            return Ok(None);
        };
        let s3 = AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_endpoint(var("TUILE_STORE_ENDPOINT")?)
            .with_region(
                std::env::var("TUILE_STORE_REGION").unwrap_or_else(|_| DEFAULT_REGION.into()),
            )
            .with_access_key_id(var("TUILE_STORE_ACCESS_KEY_ID")?)
            .with_secret_access_key(var("TUILE_STORE_SECRET_ACCESS_KEY")?)
            // The store's manifests are replaced by conditional writes only.
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("tile store runtime: {e}"))?;
        let s3: Arc<dyn object_store::ObjectStore> = Arc::new(s3);
        // A store of its own inside a bucket that holds another: a trial that
        // shares a real bucket without touching what it serves.
        let s3: Arc<dyn object_store::ObjectStore> = match var("TUILE_TILES_PREFIX") {
            Ok(prefix) => Arc::new(object_store::prefix::PrefixStore::new(s3, prefix)),
            Err(_) => s3,
        };
        if let Some(layer) = layer {
            let added = runtime
                .block_on(async {
                    let mut catalog = tuile_tile_server::catalog::read(s3.as_ref()).await?;
                    if catalog.layers.iter().any(|l| l.name == layer.name) {
                        return Ok::<_, tuile_tile_server::StoreError>(false);
                    }
                    tracing::info!(layer = %layer.name, grid = ?layer.grid, "TILES-LAYER-ADDED");
                    catalog.layers.push(layer);
                    tuile_tile_server::catalog::write(s3.as_ref(), &catalog).await?;
                    Ok(true)
                })
                .map_err(|e| format!("tile bucket {bucket}: its catalog: {e}"))?;
            if !added {
                tracing::info!("TILES-LAYER-KNOWN");
            }
        }
        let store = runtime
            .block_on(TileStore::open(
                s3,
                StoreConfig {
                    manifest_ttl: BAKE_MANIFEST_TTL,
                    ..StoreConfig::default()
                },
            ))
            .map_err(|e| format!("tile bucket {bucket}: {e}"))?;
        tracing::info!(bucket, layers = store.layers().count(), "TILES-OPEN");
        Ok(Some(Self {
            runtime,
            store: Arc::new(store),
        }))
    }

    /// Says loudly when a source has no layer in the store: its tiles would
    /// silently not be kept.
    pub fn check(&self, namespaces: &[String]) {
        for ns in namespaces {
            if self.store.layer(ns).is_err() {
                tracing::warn!(layer = %ns, "TILES-NO-LAYER: this source's tiles are fetched but not kept");
            }
        }
    }

    /// Projects the part of the store the scene's cameras can use into local
    /// archives, read before the bucket for the rest of the bake.
    pub fn project(
        &self,
        poses: &[tuile_tape::Frame],
        imagery_layer: Option<&str>,
    ) -> Result<(), String> {
        let eyes = poses.iter().map(|p| {
            let g = tuile_core::geo::ecef_to_geodetic(glam::DVec3::from_array(p.position));
            Eye {
                lon: g.lon.to_degrees(),
                lat: g.lat.to_degrees(),
                height: g.height,
            }
        });
        let factor = std::env::var("TUILE_TILES_FACTOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TILE_FACTOR);
        let imagery_factor = std::env::var("TUILE_TILES_IMAGERY_FACTOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(IMAGERY_FACTOR);
        let mut footprint = Footprint::from_eyes(eyes, factor);
        if let Some(name) = imagery_layer {
            footprint = footprint.with_layer_factor(name, imagery_factor);
        }
        let dir =
            std::env::var("TUILE_TILES_DIR").unwrap_or_else(|_| DEFAULT_PROJECTION_DIR.into());
        // Projections stay after their bake, for the next one over the same
        // ground; the least recently made go first to keep under a budget.
        let budget = std::env::var("TUILE_TILES_DIR_GIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PROJECTIONS_GIB)
            << 30;
        let (evicted, freed) = evict_oldest(std::path::Path::new(&dir), budget);
        if evicted > 0 {
            tracing::info!(
                evicted,
                mb = freed as f64 / 1e6,
                budget_gib = budget >> 30,
                "TILES-PROJECTIONS-EVICTED"
            );
        }
        let report = self
            .runtime
            .block_on(self.store.project(&footprint, std::path::Path::new(&dir)))
            .map_err(|e| format!("projecting the tile store: {e}"))?;
        tracing::info!(
            eyes = footprint.eyes().len(),
            factor,
            imagery_factor,
            zones_seen = report.zones_seen,
            zones = report.zones_projected,
            tiles_listed = report.tiles_listed,
            tiles = report.tiles_kept,
            mb = report.bytes as f64 / 1e6,
            requests = report.requests,
            seconds = report.millis as f64 / 1e3,
            "TILES-PROJECTED"
        );
        Ok(())
    }

    /// The grid a layer's tiles are addressed on, if the store has the layer.
    pub fn grid(&self, layer: &str) -> Option<Grid> {
        self.store.layer(layer).ok().map(|l| l.grid)
    }

    pub fn cache(&self) -> crate::TileCache {
        crate::TileCache(Arc::new(StoreContent::new(self.store.clone())))
    }

    /// The store as a cache that is read and never written: for a bake whose
    /// sources keep their own tiles (a host's tile server, see
    /// [`crate::Sources`]). Such a bake has nothing to flush.
    pub fn read_only_cache(&self) -> crate::TileCache {
        crate::TileCache(Arc::new(StoreContent::read_only(self.store.clone())))
    }

    /// The tiles `asks` name, gathered into one archive put in the store's
    /// bucket under `key`: an extract, for a server of the bucket to serve
    /// (`tuile_tile_server::TileStore::extract_into`).
    pub fn extract_into(
        &self,
        asks: &[tuile_tile_server::ExtractAsk],
        key: &str,
    ) -> Result<tuile_tile_server::ExtractReport, String> {
        self.runtime
            .block_on(self.store.extract_into(asks, key))
            .map_err(|e| format!("extract {key}: {e}"))
    }

    /// Publishes whatever is still buffered. Called on the way out, success or
    /// not: tiles fetched by a failed bake are still worth keeping.
    pub fn flush(&self) -> Result<(), String> {
        let stats = self.store.stats();
        tracing::info!(
            buffer = stats.buffer,
            projection = stats.projection,
            projection_misses = stats.projection_misses,
            remote = stats.remote,
            absent = stats.absent,
            "TILES-READS"
        );
        let began = std::time::Instant::now();
        let zones = self
            .runtime
            .block_on(self.store.flush_all())
            .map_err(|e| format!("tile store flush: {e}"))?;
        tracing::info!(
            zones,
            seconds = began.elapsed().as_secs_f64(),
            "TILES-FLUSHED"
        );
        Ok(())
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::evict_oldest;

    #[test]
    fn the_oldest_projections_go_until_the_rest_fits() {
        let dir = tempfile::tempdir().expect("dir");
        let zone = |name: &str, bytes: usize, age_s: u64| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, vec![0u8; bytes]).expect("write");
            let at = std::time::SystemTime::now() - std::time::Duration::from_secs(age_s);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(at)
                .expect("time");
            path
        };
        let old = zone("asset-1/zones/z4/1/2.pmtiles", 400, 300);
        let middle = zone("asset-2/zones/z4/1/2.pmtiles", 400, 200);
        let new = zone("asset-2/top.pmtiles", 400, 100);
        // Being written by another bake, however old: left alone.
        let making = zone(".tmpAbC/0.pmtiles", 400, 900);
        let renaming = zone("asset-1/zones/z4/.tmpXyZ", 400, 900);
        assert_eq!(evict_oldest(dir.path(), 1000), (1, 400));
        assert!(!old.exists() && middle.exists() && new.exists());
        assert_eq!(
            evict_oldest(dir.path(), 1000),
            (0, 0),
            "under the budget: nothing goes"
        );
        assert_eq!(evict_oldest(dir.path(), 0), (2, 800));
        assert!(making.exists() && renaming.exists());
        assert_eq!(evict_oldest(&dir.path().join("none"), 0), (0, 0));
    }
}
