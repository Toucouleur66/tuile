// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a program that renders sets up before its first frame, the same for
//! every one of them: the stores as the environment names them, the film's
//! encoder, the sun where the film is.

use std::path::Path;
use std::sync::Arc;

use glam::{DVec3, Vec3};
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_farm::{BucketConfig, ObjectRunStore, Tuning};
use tuile_repository::Objects;

use crate::sink::{Av1Film, Sink};
use crate::Error;

/// The bucket `name`, at the endpoint and with the credentials of the
/// environment's `TUILE_STORE_*`.
pub fn bucket(name: &str) -> Result<Arc<dyn Objects>, Error> {
    let config = BucketConfig {
        bucket: name.to_string(),
        ..BucketConfig::from_env()?
    };
    Ok(Arc::new(ObjectRunStore::bucket(
        &config,
        Tuning::from_env(),
    )?))
}

/// The tile store a render reads, and what keeps its blocks.
pub struct TileStore {
    pub objects: Arc<dyn Objects>,
    /// The blocks of a remote store, counted where they cross the network.
    pub remote_blocks: Option<Arc<tuile_repository::Kept<tuile_repository::HttpGet>>>,
    /// Their store, to be closed on the way out: what it has not written by
    /// then is lost, and downloaded again by the next render.
    pub block_store: Option<tuile_storage_foyer::FoyerStore>,
}

/// The tile store as the environment names it: somebody else's server of
/// the store routes (`TUILE_TILES_REMOTE`, see
/// [`tuile_repository::HttpGet::from_env`]), its blocks kept under `cache`,
/// or the bucket `TUILE_TILES_BUCKET`.
pub async fn tile_store(cache: &Path) -> Result<TileStore, Error> {
    match tuile_repository::HttpGet::from_env()? {
        Some(get) => {
            println!("tile store: {}/store, by its routes", get.root());
            // A block is downloaded once: kept in a store of its own beside
            // the film's other caches — memory over disk — for this render
            // and the next. Sized well above a film's blocks: what the store
            // gives up is downloaded again.
            let blocks =
                tuile_storage_foyer::FoyerStore::keeping(tuile_storage_foyer::StoreConfig {
                    dir: cache.join("store-blocks"),
                    memory_bytes: 256 << 20,
                    disk_bytes: 32 << 30,
                    default_ttl: None,
                })
                .await?;
            let kept = Arc::new(tuile_repository::Kept::new(get, Arc::new(blocks.clone())));
            Ok(TileStore {
                objects: Arc::new(tuile_repository::RemoteStore::new(kept.clone(), "store")),
                remote_blocks: Some(kept),
                block_store: Some(blocks),
            })
        }
        None => Ok(TileStore {
            objects: bucket(&std::env::var("TUILE_TILES_BUCKET").map_err(|_| {
                "no tile store: neither TUILE_TILES_REMOTE nor TUILE_TILES_BUCKET is set"
            })?)?,
            remote_blocks: None,
            block_store: None,
        }),
    }
}

/// `r,g,b`: a colour as three linear values, none below zero.
pub fn rgb(name: &str, colour: &str) -> Result<Vec3, Error> {
    let parts: Vec<f32> = colour
        .split(',')
        .map(|part| part.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{name} is <r>,<g>,<b>, not {colour}"))?;
    match parts[..] {
        [r, g, b] if parts.iter().all(|c| c.is_finite() && *c >= 0.0) => Ok(Vec3::new(r, g, b)),
        _ => Err(format!("{name} is <r>,<g>,<b>, each zero or more, not {colour}").into()),
    }
}

/// The direction of a sun at `azimuth` from north towards east and
/// `elevation` above the horizon, both in degrees, where `eye` (ECEF) is.
pub fn sun_at(eye: [f64; 3], azimuth: f64, elevation: f64) -> Vec3 {
    let (azimuth, elevation) = (azimuth.to_radians(), elevation.to_radians());
    // East, north and up where the eye is.
    let local = enu_frame(ecef_to_geodetic(DVec3::from_array(eye)));
    let level = local.x_axis * azimuth.sin() + local.y_axis * azimuth.cos();
    (level * elevation.cos() + local.z_axis * elevation.sin()).as_vec3()
}

/// The film's encoder: the one asked for, or the best this build has — the
/// machine's own before software.
pub fn film_sink(path: String, bitrate: u32, codec: Option<&str>) -> Result<Box<dyn Sink>, Error> {
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    if matches!(codec, Some("h264") | None) {
        return Ok(Box::new(crate::H264Film::at(path, bitrate)));
    }
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    if codec == Some("mv-hevc") {
        return Ok(Box::new(crate::SpatialFilm::at(path, bitrate)));
    }
    #[cfg(all(target_os = "linux", feature = "nvenc"))]
    {
        use crate::{NvencCodec, NvencFilm};
        let device = std::env::var("TUILE_CUDA_DEVICE")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0);
        match codec {
            Some("nvenc-av1") | None => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::Av1).on_device(device),
                ))
            }
            Some("nvenc-h264") | Some("h264") => {
                return Ok(Box::new(
                    NvencFilm::at(path, bitrate, NvencCodec::H264).on_device(device),
                ))
            }
            _ => {}
        }
    }
    match codec {
        Some("av1") | None => Ok(Box::new(Av1Film::at(path, bitrate))),
        Some(other) => Err(format!("no {other} encoder was built into this binary").into()),
    }
}
