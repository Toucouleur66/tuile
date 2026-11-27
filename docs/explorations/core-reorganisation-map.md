<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# `tuile-core` today: a map for its reorganisation

Status: **read-only survey** (2026-11-27), at `main` = `843b5a06`. No code was
changed to write it. The design that follows from it is
[`core-reorganisation.md`](core-reorganisation.md).

Method: `grep`/`rg` over `crates/tuile-core/src` and the rest of the
workspace, plus reading the `Cargo.toml` files. No `cargo` was run (no local
builds, rule of 2026-11-24), so nothing here was checked by the compiler:
an edge seen by grep can be missed when it goes through a glob import
(`use super::*`) or a macro, and a dependent counted by grep can be a doc
comment. Where that matters it is said.

"Production lines" are the lines above the module's `mod tests`; "test lines"
the rest. Internal dependencies count production code only, comments left
out.

## 1. The modules at a glance

| Module | Prod / test lines | Depends on (inside the crate, production code) | Kind |
|---|---|---|---|
| `traversal` | 1548 / 2134 | `budget`, `det!`/`determinism`, `math`, `source`, `tileset` (**`Refine` only**) | **generic seam** |
| `runtime` | 2307 / 2664 | `budget`, `content`, `cover`, `fetch`, `metrics`, `neighbourhood`, `offload`, `protocol`, `source`, `tiles3d`, `tileset`, `traversal` | **seam, globe-shaped** |
| `source` | 1076 / 417 | `budget`, `content`, `math`, `tileset` (**`Refine` only**) | **seam, globe hooks in the traits** |
| `protocol` | 365 / 48 | `budget`, `content`, `source`, `traversal` | seam (wire messages) |
| `drive` | 344 / 277 | `budget`, `content`, `protocol`, `source`, `traversal` | seam (bulk driver) |
| `report` | 184 / 49 | `content`, `source` | seam (tooling) |
| `neighbourhood` | 229 / 126 | `math`, `source` | seam, with one `is_ground()` filter |
| `offload` | 407 / 274 | `budget` (`ResourceId`) | foundation (CPU off-loading) |
| `fetch` | 322 / 110 | — | foundation (I/O seam: `TileFetcher`) |
| `storage` | 168 / 48 | `budget`, `fetch` | foundation (`ContentStore`) |
| `determinism` | 167 / 97 | — | foundation (`det!`, digests) |
| `metrics` | 1113 / 96 | `budget`, `raster` (`SUBSTITUTE_SOURCE`) | foundation infra + **named per-content fields** |
| `math` | 437 / 454 | — | foundation (OBB, sphere, frustum, occluder) |
| `geo` | 309 / 153 | `math` | foundation (f64 WGS84 geodesy) |
| `content` | 785 / 582 | `budget`, `raster`, `stitch`, `texels` | **the closed content type** + glTF/b3dm decoding |
| `texels` | 506 / 473 | `budget`, `content`, `raster` | texture payload (in `DecodedTexture`) |
| `raster` | 2096 / 1115 | `budget`, `content`, `fetch`, `geo`, `math`, `metrics`, `storage`, `stored`, `texels` | **globe** (imagery) |
| `stored` | 192 / 303 | `content`, `raster`, `texels` | globe (KTX2 imagery) |
| `empty_imagery` | 123 / 91 | — | globe (imagery heuristics) |
| `stitch` | 942 / 689 | `content`, `geo`, `seam` | **globe** (terrain crack stitching) |
| `seam` | 584 / 257 | `content`, `geo` | **globe** (seam residuals) |
| `surface` | 539 / 193 | `content`, `geo`, `ground`, `stitch` | **globe** (height/pick over resident terrain) |
| `ground` | 76 / 49 | `fetch`, `geo` | globe (`GroundHeight`) |
| `cover` | 211 / 194 | `budget` (`GeoRect`), `geo`, `math`, `source` | **globe** (ground under a covering source) |
| `tileset` | 529 / 341 | `budget`, `geo`, `implicit`, `math`, `source` | **3D Tiles** |
| `tiles3d` | 158 / 41 | `budget`, `content`, `fetch`, `source`, `tileset` | **3D Tiles** (`TilesetTree`/`TilesetLoader`) |
| `implicit` | 55 / 30 | — | **3D Tiles** |

Crate dependencies used only by content-specific code: `gltf` and
`tuile-b3dm` (only `content::decode`, called only by `tiles3d`); `image` and
`texcomp` (imagery decoding in `raster`, `stored`, `texels`, and the glTF
image path in `content`); `url` (`tileset`, `fetch`, `raster`).

### Cycles inside the crate (production code)

- `content → raster → content`, `content → texels → content`,
  `raster → stored → raster`, `content → stitch → content`,
  `stitch → seam → content`: the content type, imagery and terrain stitching
  are one strongly connected block.
- `source → tileset → source`: only through `Refine`.
- `runtime → tiles3d → runtime` (doc links only) and `source → runtime`
  (doc links only).

## 2. The main unknown, answered: traversal and runtime

**`traversal` is generic.** Its production code names no content, no class,
no drape, no ground hook. Its only content-specific import is
`crate::tileset::Refine` (the `Add`/`Replace` refinement), which is a generic
tree concept that happens to be defined in the 3D Tiles module.
`Config::interactive_globe()` is a preset name, not a globe dependency.

**`runtime` (`GeometryServer`) is the residency/scheduler and is globe- and
3D-Tiles-shaped.** In its production code:

- it builds the need with `Need::new(level, tree.mesh_class(tile))` and adds
  `ImageryNeed` from `loader.drapes(tile)`;
- it schedules `ResourceId::Drape` loads through `loader.load_drape`;
- `ground = class.is_ground()` decides whether a load keeps a CPU copy
  (`cost_held(ground)`) and whether `steepest_ground` is measured;
- `Cover` / `Covered` (from `cover`) relax the ground's SSE under a covering
  source, using `tree.covers_ground(tile)`;
- `Class::Drape` (×4) starts epochs and prices drape resources,
  `Class::Tiles3dMesh` (×3) feeds `tiles3d_*` metrics, `Class::ALL` loops the
  ledger;
- `in_process(fetcher, tileset)` is a 3D Tiles convenience constructor
  (builds `TilesetTree` + `TilesetLoader`);
- `Loaded::Expanded` is answered by building an empty `DecodedTileContent`
  with its `imagery`/`withheld_drape` fields.

So the selection (traversal) can be the reduced core as is; the residency
loop around it cannot be separated from drapes and ground without changing
code. That is phase 1.5, not a move.

## 3. Every place the core names a content or a budget class (production code)

### 3.1 Budget classes (`budget::Class`, closed enum of 4 in `tuile-budget`)

| Where | What |
|---|---|
| `source.rs` `TileTree::mesh_class` (default) | `Class::TerrainMesh` |
| `source.rs` `CompositeTileTree::mesh_class` | forwards |
| `tileset.rs` `impl TileTree for Tileset` | `Class::Tiles3dMesh` |
| `tiles3d.rs` `impl TileTree for TilesetTree` | `Class::Tiles3dMesh` |
| `runtime.rs` | `Class::Drape` ×4, `Class::Tiles3dMesh` ×3, `Class::ALL` ×1, `is_ground()` ×3 |
| `neighbourhood.rs` | `mesh_class(t).is_ground()` ×2 (the prefetch neighbourhood is ground-only) |
| `protocol.rs` | `PerClass<Bytes>` (`hard_limit`), fixed-size array |
| `tuile-budget` itself | `Class::is_ground() = !Tiles3dMesh`, `ResourceId::{Mesh, Drape{tile,layer,level}}`, `ImageryNeed`, `GeoRect`, `DrapeCandidate` |

### 3.2 The content type

`source::Loaded::Content(DecodedTileContent)` is the only content a loader
can answer; `protocol::ResourceContent` is `Mesh(Arc<DecodedTileContent>)` or
`Drape(Arc<DrapeContent>)`. `DecodedTileContent` is mesh-shaped and carries
globe fields:

- `imagery: Vec<raster::ImageryLayer>` — imagery placement (globe);
- `shed: Vec<ShedMesh>` with `steepest` = steepest face against the
  **globe's** up, skirts removed by `stitch::without_skirts` (terrain);
- `withheld_drape: Option<u64>` (globe);
- `apart: Option<Apart>` — already a type-erased escape hatch
  (`Arc<dyn Any + Send + Sync>`).

`DecodedTexture.texels: texels::Texels` ties the texture payload to the
imagery compression plans.

### 3.3 Globe hooks in the generic traits

- `TileTree::covers_ground`, `SourceOptions::covers_ground`, forwarded by
  `CompositeTileTree` (14 globe-hook sites in `source.rs`).
- `TileLoader::drapes -> Option<DrapeReach>`, `TileLoader::load_drape ->
  DrapeContent`, `DrapeReach` (imagery texel size and substitute layers).
- `TileTree::occluder` (horizon culling: generic in shape, globe in use).

### 3.4 Metrics

`metrics::Metrics` is one static struct whose fields name contents:
`tiles3d_selected`, `tiles3d_bytes`, `tiles3d_loads_started`,
`imagery_bytes`, `imagery_*`, `terrain_absent`, `terrain_offline`,
`stitch_*`, `drapes_withheld`, `tiles_without_imagery`, `layer` (imagery
layer roles, via `raster::SUBSTITUTE_SOURCE`). Moving a content out leaves
its counters in core unless the metrics become a registry.

## 4. External dependents of each module

Files outside `crates/tuile-core` that name `tuile_core::<module>` (grep,
`.rs` only; examples included, `references/` excluded). Root re-exports
(`tuile_core::{Tileset, TilesetTree, TilesetLoader, DecodedTileContent,
Config, ViewState, …}`) are counted separately below.

| Module | Files | Crates |
|---|---|---|
| `geo` | 104 | bevy-viewer, globe-bulk, tuile-atmosphere, -bake, -bevy, -camera, -film, -film-gpu, -film-native, -planetary, -record, -tape, -terrain, -usd, -viewer, -wgpu, wgpu-viewer |
| `budget` (re-export) | 67 | globe-bulk, orbit-probe, -bake, -bevy, -metrics, -planetary, -storage-foyer, -telemetry, -terrain, -ui, -viewer, -web, -wgpu |
| `raster` | 58 | orbit-probe, -bake, -bevy, -bing, -cesium-ion, -film, -film-gpu, -film-native, -pack-api, -planetary, -terrain, -viewer, -web, -wgpu, warm-cache |
| `source` | 52 | globe-bulk, orbit-probe, -bake, -cesium-ion, -film, -film-gpu, -film-native, -planetary, -terrain, -viewer, -web, -wgpu |
| `metrics` | 48 | -bake, -film, -metrics, -planetary, -telemetry, -terrain, -tile-server, -ui, -viewer, -wgpu, warm-cache, wgpu-viewer |
| `content` | 47 | orbit-probe, -bake, -film, -film-gpu, -film-native, -pack-api, -planetary, -terrain, -viewer, -web, -wgpu |
| `fetch` | 37 | -bake, -bevy, -bing, -cesium-ion, -native-fetchers, -planetary, -terrain, -tile-server, -viewer, -web, -wgpu |
| `traversal` | 34 | globe-bulk, orbit-probe, -bake, -bevy, -camera, -hydra, -planetary, -terrain, -viewer, -web, -wgpu |
| `texels` | 27 | -bake, -bevy, -planetary, -viewer, -web, -wgpu |
| `protocol` | 25 | orbit-probe, -bake, -bevy, -viewer, -web, -wgpu |
| `storage` | 25 | orbit-probe, -bake, -pack-api, -planetary, -repository, -storage-foyer, -terrain, -tile-server, -viewer, warm-cache |
| `runtime` | 16 | globe-bulk, orbit-probe, -bake, -bevy, -planetary, -viewer, -wgpu |
| `offload` | 11 | globe-bulk, orbit-probe, -bake, -bevy, -planetary, -viewer, -web, warm-cache |
| `stitch` | 11 | -film, -film-gpu, -film-native, -wgpu |
| `tileset` | 9 | -bake, -cesium-ion, -planetary, -terrain, -viewer, -wgpu (most of them only `Refine`) |
| `math` | 7 | -bake, -terrain, -viewer, -wgpu |
| `seam` | 7 | -film, -film-gpu, -film-native, -planetary, -terrain |
| `determinism` | 5 | -bake, -hydra, -wgpu |
| `drive` | 4 | globe-bulk, orbit-probe, -bake |
| `ground` | 4 | -camera, -terrain, -viewer |
| `surface` | 4 | -bevy, -wgpu |
| `empty_imagery` | 2 | -bing, -planetary |
| `report` | 1 | globe-bulk |
| `cover`, `implicit`, `stored`, `tiles3d` | 0 | — |

(`neighbourhood` and `traversal` also show up in `tuile-budget`, in doc
comments only: `tuile-budget` cannot depend on the core.)

### 4.1 The 3D Tiles surface, outside the core

Everything outside the core that uses the 3D Tiles reader (not just
`Refine`):

- `tuile-viewer/src/web/worker.rs` and `src/native/launch.rs`:
  `Tileset::from_json_bytes`, `tuile_core::TilesetLoader`,
  `tuile_core::TilesetTree`;
- `tuile-cesium-ion/src/lib.rs` (`Tileset`) and
  `tests/live_external_tileset.rs` (`ContentKind`, `Tileset`);
- `tuile-wgpu/examples/snapshot.rs`: `Tileset` and `runtime::in_process`.

`Refine` users: `tuile-terrain/src/tree.rs`, `tuile-viewer/src/app/stall.rs`,
`tuile-bake/src/session.rs` (×3).

`content::decode` / `sniff` / `ContentHints` / `ContentFormat`: no user
outside the core; inside, only `tiles3d`.

### 4.2 The 3D Tiles reader inside the core's own tests

The `Tileset` is the fixture tree of many core tests, not only of the
3D Tiles tests:

- `runtime.rs` tests: 18 test functions or helpers (`tileset_source`,
  `deep_tileset`, `grounded_scene`, `draped_session_of`, …);
- `traversal.rs` tests: 11 (`mini_tileset`, `deep_tileset`, `run`,
  `converge_like_the_server`, `add_refinement_keeps_parent_selected`, …);
- `drive.rs` tests: 2;
- integration tests `tests/loop_instruments.rs` and
  `tests/the_frame_does_not_collapse.rs`.

This is what makes the 3D Tiles move non-pure (see the design page, §3).

## 5. Dependencies between the crates around the core

| Crate | Depends on (workspace crates) |
|---|---|
| `tuile-terrain` | core |
| `tuile-bing` | core |
| `tuile-planetary` | core, terrain, bing |
| `tuile-camera` | core |
| `tuile-cesium-ion` | core, bing, planetary, terrain |
| `tuile-wgpu` | core, atmosphere, overlay, planetary, tape, terrain |
| `tuile-bevy` | core, camera, planetary, terrain, wgpu, web, … |
| `tuile-film` | core, atmosphere, overlay, pack, terrain |
| `tuile-film-gpu` | core, atmosphere, film, overlay, tape, terrain |
| `tuile-film-native` | core, film, film-gpu, bake, … (no direct `tuile-terrain`) |
| `tuile-ground` | (nothing in tuile; no dependent yet) |

wasm CI list (`ci.yml`, `cargo check --target wasm32-unknown-unknown`):
budget, ground, core, web, wgpu, planetary, terrain, camera, atmosphere,
film-gpu, ui, film, bevy.

## 6. Open PRs touching the same files (2026-11-27)

#333 (`traversal.rs`), #335 (`raster.rs`), #336 (planetary caches), #334
(terrain skirts), #332 (wgpu underlay), #282 (viewer prediction), #244
(mist). The 3D Tiles stack #173 → #191 is merged. Any move of `raster.rs` or
`traversal.rs` collides with #335 / #333.
