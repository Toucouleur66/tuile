# 03 — Roadmap and acceptance criteria

Implement in order. A milestone is only done when ALL of its criteria pass. Don't anticipate the features of later milestones (YAGNI), but never break the seams that make them possible.

> **Product north star**: one render-agnostic foundation that serves every mode — interactive on desktop, phone, headset and browser, films, offline renderers — on top of the OGC 3D Tiles engine. The structuring target so far has been **globe rendering** (quantized-mesh terrain + raster imagery), interleaved with the 3D Tiles foundation (M1–M3) once its rendering was proven.

## Where things stand (v0.14.0, 2026-10-25)

| Milestone | Status |
|---|---|
| MG — the globe | **Delivered.** Terrain + imagery, interactive on four platforms, token-free sources included. |
| M1 — core + wgpu | **Core delivered; viewer side partial.** 3D Tiles parse/traverse/decode are in the core and drawn offscreen; the interactive viewer draws the globe, not tilesets. No committed sample tilesets. |
| M2 — network middleware | **Not started**, except its storage-and-cache side (`tuile-tile-server`). |
| M2.5 — raster imagery | **Delivered** for terrain (draping, several layers, attributions, budgeted). Draping a 3D Tiles tileset is not done. |
| M3 — USD/USDZ export | **Redirected.** A USD manifest stage and an OpenUSD/Hydra plugin shipped instead; USDZ export is not started. |
| M4+ | `tuile-cesium-ion`, `tuile-hydra` and `tuile-bevy` (native and web) shipped; the rest not started. |

A checked box below is backed by CI or by a test in the workspace; an unchecked one is either not met or not checked by anything automatic, and its note says which.

## MG — Globe: quantized-mesh terrain + imagery (the target) — delivered

Scope: `tuile-terrain`, `core::raster`, `tuile-planetary`, `tuile-cesium-ion` + `tuile-bing`, integration into the geometry server.

Deliverables:
1. **`tuile-terrain` — quantized-mesh-1.0 decoding**: header, u/v/height vertices (zigzag-delta), high-water-mark indices, skirts, `octvertexnormals`. A full encoder (round-trip, and the heightfield source). A `.terrain` tile → `DecodedTileContent` (rebased ECEF, normals) — the type shared with glTF content. *Shipped.*
2. **TMS geographic tiling** (EPSG:4326, 2 root tiles), `layer.json` availability, geometric error per level. *Shipped.*
3. **Source abstraction**: the traversal, the budget and the `GeometryServer` are generic over `TileTree` + `TileLoader` (the plan's `TileSource`); `TilesetTree` (3D Tiles) and the terrain quadtree implement them. *Shipped.*
4. **Connectors**: terrain and imagery assets through `tuile-cesium-ion` (endpoint → layer.json → tiles, token refresh), imagery metadata and quadkeys through `tuile-bing`; attributions displayed. *Shipped.*
5. **Draped imagery**: `core::raster` crossed with the terrain in `tuile-planetary`; drapes are resources of their own, several layers, each falling back on its own. *Shipped.*
6. **Non-connector variant**: `--terrain-url` (any quantized-mesh `layer.json`) and `--imagery-template` (any XYZ template), and `examples/heightfield-serve`, a synthetic relief served over plain HTTP. *Shipped.*

Acceptance criteria:
- [ ] A real `.terrain` tile decodes into a coherent mesh — decoding is tested on encoded tiles in `tuile-terrain`; the test on a real, never-committed tile is manual.
- [ ] The snapshot renders a real globe patch without jitter at any scale — met in the shipped viewers by eye; no automated jitter check.
- [ ] The same patch draped with imagery displays textured; attributions present — met in the viewer (the layer's attribution is shown while it is on screen); manual.
- [x] The traversal refines the terrain by SSE via the source abstraction, with no terrain-specific selection code (`core::source`, `core::traversal`).
- [x] The token is read from the environment (`CESIUM_ION_TOKEN`), never present in the repo; the web page and the APK hold none (`examples/catalog-proxy`); a build without a token works against a free source (`heightfield-serve`).

## M1 — Core + wgpu viewer — core delivered, viewer side partial

Scope: `tuile-core`, `tuile-wgpu`, `examples/wgpu-viewer`, fixtures.

Deliverables:
1. Complete serde parsing of tileset.json (1.0 + 1.1, external tilesets, implicit tiling types). *Shipped; implicit subtree decoding is not.*
2. Content decoding: glb + b3dm → `DecodedTileContent` (positions, normals, UV, indices, RGBA8 textures, composed f64 transform, up-axis rotation). *Shipped.*
3. Pure, tested SSE traversal: frustum culling, REPLACE with hold-until-ready, ADD, request prioritization. *Shipped.*
4. `protocol` module: `ClientMessage`/`ServerMessage`, `TileContent { Raw, Decoded }`, `GeometryStream`, `InProcessStream`. *Shipped*, extended by the graphics budget (`Ack` with bytes, `Discarded`, `Retarget`, `Pressure`, `Thermal`, `Priming`). The planned `HttpPullStream` type was not needed: a consumer-side `GeometryServer` over an HTTP fetcher is the pull mode.
5. `TileFetcher`: `FsFetcher` and `HttpFetcher` (core, `http` feature); `NativeHttp` and `WebFetcher` in the transport crates. *Shipped.*
6. `tuile-wgpu`: GPU preparation (`prepare` → `PreparedTile`; the planned `PrepareRenderResources` trait was folded into the protocol's `Ack`), one ground pipeline, relative-to-center rendering. *Shipped.*
7. A viewer that loads a tileset by path/URL. *Not shipped as an interactive viewer*: tilesets are drawn by the offscreen `cargo run -p tuile-wgpu --example snapshot -- <tileset.json> <out.png>`; `tuile-wgpu-viewer` is the globe viewer (orbit camera, live statistics, record/replay).

Acceptance criteria:
- [x] `cargo test --workspace` green on Linux and macOS (CI `test`), with traversal coverage on a hand-crafted mini-tileset.
- [x] `cargo check --target wasm32-unknown-unknown -p tuile-core` green (CI `wasm-guard`, with every other wasm crate, `-D warnings`).
- [ ] The viewer correctly displays the b3dm AND glTF 1.1 fixtures without jitter at max zoom — no committed fixtures, no interactive tileset viewer.
- [ ] No frame > 33 ms attributable to decode — decode runs off the frame (`Offload`: thread pool, decode workers); no automated frame-time check for tilesets.
- [x] Zero `unwrap()`/`panic!` in the core's production paths — `clippy::unwrap_used` and `clippy::panic` are workspace lints, denied in CI.

## M2 — Network middleware: streaming over a network + static mode + pluggable cache — not started

Scope: `tuile-server` (its storage and cache side, `tuile-tile-server`, exists), a client `WsStream` binding, `tuile-wgpu-viewer --remote`. Design: `docs/12-crate-server.md`.

Deliverables:
1. **Network binding of streaming mode**: one session = one core `GeometryServer` and its graphics budget, for a consumer that declares its budget; interactive devices run theirs locally (D-A5, 2026-10-19); the transport is open (D-T1). Envelope versioned from the first byte, geometry as glb on the wire. Client `WsStream` binding (decodes on the client side) + `tuile-wgpu-viewer --remote`.
2. `tuile` native binary: `tuile serve <dir|s3://|gs://>` via `object_store`. Routes: streaming, tileset.json, contents, health, basic metrics.
3. A removable cache over the core's `ContentStore` (none, memory, memory over disk — `tuile-storage-foyer` exists), selected by config/CLI.
4. Correct HTTP headers (static mode): ETag, immutable Cache-Control on contents, configurable CORS, Range requests on glb.
5. Edge-worker target: static mode (same routes, object storage, the CDN does the caching). A deployment example in `examples/`.
6. Implicit tiling subtree decoding: the server can return a clean 404 on an unavailable tile.

Acceptance criteria:
- [ ] `tuile-wgpu-viewer --remote` matches local mode in its `Select` sequence for a replayed camera trajectory — programmatic test.
- [ ] The main open-source 3D Tiles web viewers consume a tileset served by `tuile serve` without error (manual test documented in `docs/`, test HTML pages committed).
- [ ] A Tuile viewer works against the server in static mode.
- [ ] Integration tests: end-to-end session (+ cancellation, timeout, malformed messages → Error), ETag/304, immutability, the cache within its budget, no cache = passthrough.
- [ ] A session's budget stays within the memory its consumer declared, its ledger exact from the `Ack` bytes.
- [ ] Edge build OK, deployment documented step by step.

## M2.5 — Raster imagery (`core::raster`) — delivered for terrain

1. `ImageryProvider` trait (z/x/y → decoded texture) + impls: URL templates (XYZ, WebMercator or geographic grid), the connectors. *Shipped.*
2. WebMercator/Geographic projections; per geometry tile, the imagery tiles at the right level and their UV. *Shipped* — reprojected once per imagery tile and shared.
3. Drapes as resources of their own (`DrapeContent`), carried by the protocol like the rest. *Shipped.*
4. Draping in `tuile-wgpu` (base color × imagery), several layers. *Shipped.*
5. Imagery stored compressed (KTX2) and block-compressed on the GPU — the consumer's choice, off by default. *Shipped (v0.13–v0.14).*

Criteria: [x] terrain + an imagery layer display draped in the viewer; [x] attributions displayed; [x] imagery memory budgeted (imagery is its own class in `core::budget`, tested in `tests/budget_imagery.rs`). [ ] An *untextured 3D Tiles tileset* + an imagery layer — not done: `TilesetLoader` drapes nothing.

## M3 — USD — redirected

The plan was a USDZ exporter (`tuile-export-usd`, bbox + target error → a packaged archive). What shipped instead, because the target became rendering inside OpenUSD rather than exporting geometry:

- **`tuile-usd`** writes a *manifest stage* as plain `.usda` text — a recorded camera path as an animated camera, plus one `Globe` prim; no geometry, no OpenUSD binding (`tuile-tape-to-usd-stage`). *Shipped.*
- **`tuile-hydra`** + `integrations/hydra`: a C ABI over the geometry server and a Hydra 2.0 generative procedural that resolves the stage's camera and hands terrain and imagery to whichever Hydra renderer draws. *Shipped*; see `13-crate-usd.md`, `14-crate-hydra.md`, `15-usd-scene-index.md`.
- **USDZ export**: *not started.* Its criteria (opens in the standard USD viewer and in mobile AR viewers, animated export plays, deterministic, bounded, works against a served tileset) remain the bar if it is picked up. The server still never knows USD.

## M4 and beyond

Status per item:

1. **`tuile-hydra`** — *shipped*, but not as first imagined: the plugin links the core in-process through the C ABI, one frame at a time, rather than consuming `WsStream`.
2. **A platform-native Apple façade** (`tuile-swift`, RealityKit) — *not started.* The iOS shell of the wgpu viewer covers phones and tablets today; a headset target is what would force it (stereo already has its multi-view traversal: `ViewerState` carries one view per eye).
3. **JS bindings of the core** (`tuile-wasm`) — *not started.* The browser is served by `packages/tuile-viewer` (the whole viewer in wasm, WebGPU or WebGL2).
4. **`tuile-cesium-ion`** — *shipped*: endpoint resolution, bearer, refresh, 3D Tiles, terrain and imagery assets; a `TileFetcher` with zero changes in the core.
5. **A game-engine façade** — **`tuile-bevy`** is *shipped*: the globe in Bevy 0.20 on `tuile-wgpu`'s `HostPass`, native (#136) and on the web (#142). Ground height and picking (4c) are in review. A Unity façade is not started.
6. Connect RPC control plane (auth, index, streamed invalidations) — *not started.* Never a replacement for the HTTP data plane.
7. On-the-fly tile generation from custom sources — *started for terrain*: `HeightfieldSource` serves any relief as quantized-mesh tiles. Columnar geospatial sources: not started.
8. pnts / gaussian splatting — *not started.*

There is NO generic FFI facade: each facade is specialized for its host.

## Built outside the milestones

The milestones above never planned these; they shipped because a consumer needed them, and they are part of the foundation every mode shares:

- **The graphics budget** (`core::budget`): one pure, deterministic decision per step for viewers and films alike; CI simulates it (`budget-sim`). Design and invariants: `docs/explorations/graphics-budget-*.md`.
- **Four viewer shells of one application** (`tuile-viewer`): desktop (with a macOS application bundle), iOS, Android, web (`packages/tuile-viewer`, WebGPU with a WebGL2 fallback, its geometry server in a worker).
- **Embedding**: `ViewerHost` (`16-embedding-the-viewer.md`), `frame_host`, and `tuile-wgpu`'s `HostPass` for engines that own the frame.
- **Films**: record (`tuile-tape`), bake once (`tuile-bake`, `tuile-pack`), render often (`tuile-film`, `tuile-film-gpu`, `tuile-film-native`), on a farm (`tuile-farm`, `22-film-jobs.md`); colour harmonisation (`tuile-radiometry`, `18-color-harmonization.md`); sky, sun and moon (`tuile-atmosphere`, `19-sky-atmosphere.md`, `21-sky-sun-moon.md`).
- **A tile store and its read API**: `tuile-tile-server`, `tuile-repository`, `tuile-pack-api`.
- **GPU-compressed imagery**: `texcomp`; stored KTX2 transcoded, or encoded at upload on the GPU or in the web's decode workers — the consumer's choice, off by default.

## Next

In review or planned, in no promised order; none of it is on `main`:

- **Shipped since v0.14.0**: `tuile-bevy` native (#136) and web (#142); the viewer's physical atmosphere of the current time, shared with the film (#137).
- **In review**: `tuile-bevy` 4c, the drawn ground for the host (#145); the real stars (#147); the sun and the Moon as light sources.
- **Planned**: the compression budget — the encode's GPU time and transient memory accounted inside the core budget (B1–B8).
- **Planned**: meshopt support built into the core's glTF decoding, always on, on a pure-Rust decoder (`10-crate-core.md`).
- **Exploring**: procedural terrain with unbounded detail (draft #132); the graphics budget's own roadmap (#112).
