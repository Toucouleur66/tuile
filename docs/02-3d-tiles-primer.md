# 02 — 3D Tiles primer (what to implement)

Normative reference: OGC 3D Tiles 1.1 — https://docs.ogc.org/cs/22-025r4/22-025r4.html
Behavioral reference (gray areas): the open source reference implementations of the standard, as local clones in `references/` (gitignored, never committed) — on the web side, the traversals under `packages/engine/Source/Scene/`; on the C++ side, the 3D Tiles selection module. (Apache-2.0 — free inspiration, attribution if literal port.)

**What tuile supports today (v0.14.0):** tileset.json 1.0 and 1.1 with external tilesets grafted in; glb and b3dm content, with JPEG/PNG textures; SSE traversal with REPLACE hold-until-ready and ADD; the three bounding volumes. Implicit tiling parses but does not traverse (no subtree decoding), and no glTF compression extension is read yet (§2). 3D Tiles content is drawn by the offscreen snapshot (`cargo run -p tuile-wgpu --example snapshot -- <tileset.json> <out.png>`); the interactive viewer draws the terrain-and-imagery globe, which is not 3D Tiles (quantized-mesh terrain + raster imagery, `tuile-terrain` / `tuile-planetary`) but goes through the same traversal and geometry server.

## 1. tileset.json

Tile tree. Fields to model (serde):

- `asset.version` ("1.0" / "1.1"), root `geometricError`.
- `root`: recursive. Each `Tile`: `boundingVolume`, `geometricError`, `refine` ("REPLACE" | "ADD", inherited if absent), `transform` (column-major mat4, optional, composes with parents), `content.uri` (optional — an internal tile without content is possible), `children`.
- `content.uri` can point to **another tileset.json** (external tileset) → tree grafting at load time. Must be supported from M1 (very common).
- Bounding volumes, the three variants:
  - `box`: 12 floats (center + 3 half-axes) — OBB.
  - `region`: 6 floats (west, south, east, north in **radians**, minHeight, maxHeight) — WGS84.
  - `sphere`: 4 floats (center + radius).
- **Implicit tiling (1.1)**: `implicitTiling` on root — `subdivisionScheme` (QUADTREE/OCTREE), `subtreeLevels`, `availableLevels`, URI templates `{level}/{x}/{y}[/{z}]`. Availability is encoded in **binary subtree** files (availability bitstreams: tile/content/childSubtree). Status: the serde types exist (`core::implicit`), so such tilesets parse; subtree decoding and implicit traversal are not implemented.

## 2. Content formats

- **glTF/GLB direct (1.1)**: the content IS a glb. `gltf` crate (`import_slice`). Main path.
- **b3dm (1.0 legacy)**: 28-byte header — magic "b3dm", version u32, byteLength u32, featureTableJSONByteLength, featureTableBinaryByteLength, batchTableJSONByteLength, batchTableBinaryByteLength — then the tables, then an embedded glb → same glTF path afterwards. Read `RTC_CENTER` from the feature table if present (translation to compose).
- **i3dm, pnts, cmpt**: out of scope for v1. Return a typed `UnsupportedContent` error, not a panic.
- glTF extensions met in real tilesets: `EXT_meshopt_compression` / `KHR_meshopt_compression` with `KHR_mesh_quantization`, `KHR_draco_mesh_compression`, `KHR_texture_basisu` (KTX2). Status:
  - none is read today: uncompressed meshes + JPEG/PNG textures;
  - **meshopt is planned built into the core's glTF decoding, always on — no feature flag** — on a pure-Rust decoder (a lapoule-dev fork of `meshoptimizer-rs`, pinned by revision, wasm-clean), see `10-crate-core.md`;
  - Draco and `KHR_texture_basisu` are not scheduled. (KTX2 is already read for *imagery tiles*, `core::stored` — not for glTF textures.)

## 3. Coordinates and up-axis

- Tileset frame: ECEF (Z-up). glTF frame: Y-up, meters. The spec mandates an implicit Y-up→Z-up rotation applied to glTF content (+90° rotation around X). Don't forget it — classic mistake #1 (models lying on their side).
- Tile `transform`s compose from root to leaves: `world = parent_world * tile.transform`.
- WGS84 (lat/lon/h) ↔ ECEF conversions are needed for bounding regions and USD export: implement in `core::geo` (standard WGS84 ellipsoid formulas, in f64).

## 4. Screen-Space Error and traversal (the core)

For each visible tile:

```
sse = (geometricError * viewportHeight) / (distanceToCamera * 2 * tan(fovY / 2))
```

- `distanceToCamera`: distance from camera → bounding volume (not to the center: to the volume; clamp to ~epsilon if the camera is inside).
- If `sse > maximumScreenSpaceError` (default: 16) → the tile is too coarse → refine (descend to children).
- **REPLACE**: children replace the parent. Subtlety: only hide the parent when ALL selected children are resident (otherwise visual holes). Implement "hold until children ready".
- **ADD**: children add to the parent (the parent stays displayed).
- Frustum culling on the bounding volume before any computation (frustum planes vs OBB/sphere/region — for region, conversion to an approximating OBB is enough in v1).
- Traversal output: `selected` (tiles to render, with their SSE), `requests: Vec<ContentRequest>` sorted by priority group (Preload/Normal/Urgent) then by intra-group priority (v1: increasing camera distance; foveation/depth = post-v1 refinements).
- Cancellation: an in-flight request for a tile that has left the selection must be cancellable (the caller drops the future; the core just marks the state).

## 5. Lifecycle and memory budget

Tile states: `Unloaded → Loading → Decoded → Resident → (Evicted)`.

The spec leaves memory to the client. In tuile it is the **graphics budget** (`core::budget`, `10-crate-core.md`): per memory class, a target, an overflow and a hard limit derived from the memory the device or the consumer declares (not a fixed 512 MiB); a decoded tier on the server and a GPU tier on the consumer, charged with the bytes the consumer acknowledges. Eviction never removes what is drawn, nor the ancestor chain and pinned coarse floor a fallback lands on; among the rest, priority decides, not recency alone.

## 6. Test fixtures

Status: there is no top-level `fixtures/` directory. The traversal and content tests build their tilesets in code (a hand-crafted mini-tileset, b3dm/glb pairs encoded by `tuile-b3dm`), and the fixtures that exist live with the crate that reads them (`crates/*/tests/fixtures/`, each with a README naming its provenance and licence).

Still to do, when 3D Tiles conformance work resumes:
- The official sample tilesets from the 3D Tiles spec repo (`Samples/` — permissive license, verify and note in NOTICE): at minimum a simple b3dm tileset, a glTF 1.1, an external tileset, an implicit tiling.
- NO data from proprietary services (tile SaaS, commercial imagery) in the repository, ever.

## 7. Known pitfalls (review checklist)

- [ ] Y-up/Z-up rotation applied to glTF content.
- [ ] `geometricError = 0` on leaves → never any refinement attempt.
- [ ] `refine` inherited from the parent when absent.
- [ ] Regions in radians, not degrees.
- [ ] `transform` column-major (like glTF).
- [ ] b3dm RTC_CENTER composed into the transform.
- [ ] f64 everywhere before rebasing.
- [ ] External tilesets: junction geometricError = that of the referencing tile.
