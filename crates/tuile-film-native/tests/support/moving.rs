// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A moving film of references, and the tile store it refers to, written on
//! a disk: what a render reads when it reads a farm's film, at a size a test
//! can afford.
//!
//! The camera flies east over a strip of terrain tiles of level 13: each
//! frame draws `across × down` of them, and a column of `down` new ones
//! enters with every frame while the column furthest west leaves. Every
//! tile's mesh comes from the store — from its own terrain tile, or cut out
//! of its parent's, one tile in two — and its drape from `cells × cells`
//! imagery tiles of level 16, JPEGs of 256 × 256 texels: the tiles a frame
//! decodes, and builds, before it can be drawn.

#![allow(dead_code)]

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use glam::DVec3;
use object_store::memory::InMemory;
use object_store::{ObjectStore, ObjectStoreExt};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_film_native::{Film, Sources};
use tuile_pack::{BakedTile, BakedView, ImageryPlacement, PackWriter, StoreTile, TextureFormat};
use tuile_terrain::{GeographicTilingScheme, TileCoord};
use tuile_tile_server::{catalog, Catalog, Grid, LayerDef, StoreConfig, TileStore};

pub const TERRAIN: &str = "terrain";
pub const IMAGERY: &str = "imagery";

/// The terrain tiles' level, and the imagery's.
const LEVEL: u32 = 13;
const IMAGERY_LEVEL: u32 = 16;

/// The size of the film.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    pub frames: u32,
    /// Tiles drawn east to west, and north to south.
    pub across: u32,
    pub down: u32,
    /// Imagery tiles a side of a drape.
    pub cells: u32,
    /// Vertices a side of a terrain tile.
    pub grid: u32,
    /// The picture.
    pub viewport: (u32, u32),
    /// The side a drape is composed at.
    pub composed_side: u32,
    /// How high the camera flies, in metres.
    pub height: f64,
}

impl Shape {
    /// What a test renders: a few frames, small pictures, drapes small
    /// enough that none is ever kept smaller for being far.
    pub fn small() -> Self {
        Self {
            frames: 6,
            across: 3,
            down: 2,
            cells: 2,
            grid: 17,
            viewport: (96, 64),
            composed_side: 32,
            height: 9000.0,
        }
    }

    /// The same, flown low and wide with large drapes: a tile enters far,
    /// its drape kept smaller, and is entered again larger once the camera
    /// is over it.
    pub fn regrowing() -> Self {
        Self {
            across: 5,
            composed_side: 512,
            height: 1500.0,
            ..Self::small()
        }
    }
}

fn layer(name: &str, grid: Grid, tile_type: &str, zone_level: u8, content_type: &str) -> LayerDef {
    LayerDef {
        name: name.into(),
        grid,
        tile_type: tile_type.into(),
        tile_compression: "none".into(),
        zone_level,
        expiry_days: None,
        content_type: content_type.into(),
        index: false,
        licence: None,
        attribution: None,
        derived_from: None,
    }
}

/// The terrain tile `coord`: a `grid × grid` mesh of hills, with its edges
/// and a normal a vertex, as a terrain source serves it.
fn terrain_tile(coord: TileCoord, grid: u32) -> Vec<u8> {
    let n = grid as usize;
    let rect = GeographicTilingScheme::default().tile_rect(coord);
    let (mut u, mut v, mut height, mut normals) = (vec![], vec![], vec![], vec![]);
    for j in 0..n {
        for i in 0..n {
            let (fu, fv) = (i as f64 / (n - 1) as f64, j as f64 / (n - 1) as f64);
            // Hills over the whole strip, not a tile at a time: the same
            // ground on both sides of an edge.
            let lon = rect.west + fu * rect.width();
            let lat = rect.south + fv * rect.height();
            let h = 0.5 + 0.25 * (lon * 4000.0).sin() * (lat * 3000.0).cos();
            u.push(fu);
            v.push(fv);
            height.push(h);
            normals.push([0.0f32, 0.0, 1.0]);
        }
    }
    let mut indices = Vec::new();
    for j in 0..n - 1 {
        for i in 0..n - 1 {
            let a = (j * n + i) as u32;
            let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
            indices.extend_from_slice(&[a, b, c, c, b, d]);
        }
    }
    // Renumbered into first use, as the format codes its indices.
    let mut renumbered = vec![u32::MAX; n * n];
    let mut order = Vec::new();
    for &i in &indices {
        if renumbered[i as usize] == u32::MAX {
            renumbered[i as usize] = order.len() as u32;
            order.push(i as usize);
        }
    }
    let pick = |values: &[f64]| order.iter().map(|&i| values[i]).collect::<Vec<_>>();
    let normals: Vec<[f32; 3]> = order.iter().map(|&i| normals[i]).collect();
    let indices: Vec<u32> = indices.iter().map(|&i| renumbered[i as usize]).collect();
    let line =
        |f: &dyn Fn(usize) -> usize| -> Vec<u32> { (0..n).map(|k| renumbered[f(k)]).collect() };
    let (lon, lat) = rect.center();
    let centre = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 500.0,
    });
    tuile_terrain::decode::encode(
        &tuile_terrain::Header {
            center: centre.to_array(),
            min_height: 200.0,
            max_height: 800.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: 1.0e4,
            horizon_occlusion: [0.0; 3],
        },
        &pick(&u),
        &pick(&v),
        &pick(&height),
        &indices,
        Some(&normals),
        &[
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + n - 1),
            line(&|k| (n - 1) * n + k),
        ],
    )
}

/// An imagery tile: 256 × 256 texels of fields and noise, as a JPEG — what
/// an aerial source serves, and as long to decode.
fn imagery_tile(x: u32, y: u32) -> Vec<u8> {
    let mut seed = u64::from(x).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(y);
    let picture = image::RgbImage::from_fn(256, 256, |i, j| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let noise = (seed >> 58) as u8;
        let field = ((i / 32 + j / 32 + x + y) % 4) as u8 * 30;
        image::Rgb([
            60 + field + noise,
            90 + field / 2 + noise,
            50 + noise + (i % 7) as u8,
        ])
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
        .encode_image(&picture)
        .expect("jpeg");
    bytes
}

/// The imagery tile of level 16 under the point `lon`, `lat` (radians).
fn mercator(lon: f64, lat: f64) -> (u32, u32) {
    let n = f64::from(1u32 << IMAGERY_LEVEL);
    let x = (lon.to_degrees() + 180.0) / 360.0 * n;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x as u32, y as u32)
}

/// What was written: the sources a render reads, the film, and what the
/// store holds.
pub struct Written {
    pub sources: Sources,
    pub film: Film,
    pub terrain_tiles: usize,
    pub imagery_tiles: usize,
    pub imagery_bytes: u64,
}

/// Writes the film and its store under `dir`, and opens them as a render
/// does.
pub async fn write(dir: &std::path::Path, shape: Shape) -> Written {
    let scheme = GeographicTilingScheme::default();
    // The strip's south-west corner, in the Pyrenees.
    let (lon, lat) = (0.5f64.to_radians(), 42.8f64.to_radians());
    let tile_w = 2.0 * std::f64::consts::PI / scheme.tiles_x(LEVEL) as f64;
    let tile_h = std::f64::consts::PI / scheme.tiles_y(LEVEL) as f64;
    let x0 = ((lon + std::f64::consts::PI) / tile_w) as u64;
    let y0 = ((lat + std::f64::consts::FRAC_PI_2) / tile_h) as u64;

    let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    catalog::write(
        memory.as_ref(),
        &Catalog {
            layers: vec![
                layer(
                    TERRAIN,
                    Grid::Geographic,
                    "other",
                    9,
                    "application/vnd.quantized-mesh",
                ),
                layer(IMAGERY, Grid::WebMercator, "jpeg", 10, "image/jpeg"),
            ],
        },
    )
    .await
    .expect("catalog");
    let store = TileStore::open(memory.clone(), StoreConfig::default())
        .await
        .expect("store");

    // Every tile the film draws, west to east.
    let columns = shape.across + shape.frames - 1;
    let mut tiles = std::collections::BTreeMap::new();
    let (mut terrain_put, mut imagery_put) = (std::collections::BTreeMap::new(), 0usize);
    let mut imagery_bytes = 0u64;
    for column in 0..columns {
        for row in 0..shape.down {
            let coord = TileCoord::new(LEVEL, x0 + u64::from(column), y0 + u64::from(row));
            // One tile in two cut out of its parent's.
            let from = if (column + row) % 2 == 0 {
                coord
            } else {
                TileCoord::new(LEVEL - 1, coord.x >> 1, coord.y >> 1)
            };
            let terrain = terrain_put
                .entry((from.level, from.x, from.y))
                .or_insert_with(|| terrain_tile(from, shape.grid))
                .clone();
            let rect = scheme.tile_rect(coord);
            let (mx, my) = mercator(rect.west, rect.north);
            let cells = shape.cells;
            let mut imagery = Vec::new();
            for j in 0..cells {
                for i in 0..cells {
                    let (ix, iy) = (mx + i, my + j);
                    let bytes = imagery_tile(ix, iy);
                    let digest = tuile_film::from_store::digest(&bytes);
                    imagery_bytes += bytes.len() as u64;
                    store
                        .put(IMAGERY, IMAGERY_LEVEL as u8, ix, iy, Bytes::from(bytes))
                        .await
                        .expect("imagery");
                    imagery_put += 1;
                    let (w, s) = (i as f32 / cells as f32, j as f32 / cells as f32);
                    let side = 1.0 / cells as f32;
                    imagery.push(ImageryPlacement {
                        tile: StoreTile {
                            level: IMAGERY_LEVEL as u8,
                            x: ix,
                            y: iy,
                            digest,
                        },
                        coverage: [w, s, w + side, s + side],
                        translation: [-(i as f32), -(j as f32)],
                        scale: [cells as f32; 2],
                    });
                }
            }
            let refs = tuile_pack::TileRefs {
                terrain: StoreTile {
                    level: from.level as u8,
                    x: from.x as u32,
                    y: from.y as u32,
                    digest: tuile_film::from_store::digest(&terrain),
                },
                skirt_height: 50.0,
                imagery,
                composed_side: shape.composed_side,
            };
            tiles.insert(
                (column, row),
                (TileId::from_terrain(coord.level, coord.x, coord.y).0, refs),
            );
        }
    }
    for ((level, x, y), bytes) in &terrain_put {
        store
            .put(
                TERRAIN,
                *level as u8,
                *x as u32,
                *y as u32,
                Bytes::from(bytes.clone()),
            )
            .await
            .expect("terrain");
    }
    store.flush_all().await.expect("flush");

    // Copied out, object for object, to the directory the render reads.
    let listed: Vec<_> = memory.list(None).try_collect().await.expect("list");
    for meta in listed {
        let bytes = memory
            .get(&meta.location)
            .await
            .expect("get")
            .bytes()
            .await
            .expect("bytes");
        let path = dir.join(meta.location.as_ref());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }

    // The pack: frame `f` draws columns `f - 1 .. f - 1 + across`.
    let origin = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let mut writer = PackWriter::new("moving", origin.to_array(), dir.join("blob"))
        .expect("writer")
        .with_content(tuile_pack::Content::References, TERRAIN, IMAGERY);
    let stub = |id: u64, refs: &tuile_pack::TileRefs| {
        let le = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
        BakedTile {
            id,
            drape: 0,
            origin_ecef: origin.to_array(),
            positions: le(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
            normals: le(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0]),
            uvs: le(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0]),
            indices: [0u32, 1, 2].iter().flat_map(|v| v.to_le_bytes()).collect(),
            vertex_count: 3,
            index_count: 3,
            base_color_factor: [1.0; 4],
            texture: None,
            texture_format: TextureFormat::None,
            refs: Some(refs.clone()),
        }
    };
    for f in 1..=shape.frames {
        // Over the middle of what the frame draws, high enough to see it.
        let mid_x = x0 as f64 + f64::from(f - 1) + f64::from(shape.across) / 2.0;
        let mid_y = y0 as f64 + f64::from(shape.down) / 2.0;
        let at = Geodetic {
            lon: -std::f64::consts::PI + mid_x * tile_w,
            lat: -std::f64::consts::FRAC_PI_2 + mid_y * tile_h,
            height: 0.0,
        };
        let enu = enu_frame(at);
        let eye: DVec3 = geodetic_to_ecef(at) + enu.z_axis * shape.height;
        let view = BakedView {
            position: eye.to_array(),
            direction: (-enu.z_axis).to_array(),
            up: enu.y_axis.to_array(),
            viewport_px: [f64::from(shape.viewport.0), f64::from(shape.viewport.1)],
            fovy_rad: 1.0,
        };
        let drawn: Vec<BakedTile> = (f - 1..f - 1 + shape.across)
            .flat_map(|column| (0..shape.down).map(move |row| (column, row)))
            .map(|at| {
                let (id, refs) = &tiles[&at];
                stub(*id, refs)
            })
            .collect();
        writer.frame(f, view, drawn);
    }
    let key = "packs/moving/1.tuilepack";
    let path = dir.join(key);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("packs dir");
    writer.finish_to(&path).expect("finish");

    let objects: Arc<dyn tuile_repository::Objects> = Arc::new(
        tuile_farm::ObjectRunStore::local(dir, tuile_farm::Tuning::default()).expect("objects"),
    );
    let sources = Sources::open(objects.clone(), objects, &dir.join("cache"))
        .await
        .expect("sources");
    let film = Film::of(sources.packs.objects.as_ref(), &[key])
        .await
        .expect("film");
    Written {
        sources,
        film,
        terrain_tiles: terrain_put.len(),
        imagery_tiles: imagery_put,
        imagery_bytes,
    }
}

/// FNV-1a over a picture: what two renders are compared by.
pub fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}
