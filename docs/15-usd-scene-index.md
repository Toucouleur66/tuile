# 15 — The OpenUSD scene index

The Hydra 2.0 core of tuile's OpenUSD story, and the trajectory it serves.
Decisions recorded here were taken with Laurent on 2026-08-24, on
`feat/usd-scene-index`; the status below is as of v0.14.0.

## Vision — what pulls this

**The video product pulls the trajectory.** The deterministic recorder
(wgpu, exact-or-die contract) has an OpenUSD twin: a track becomes a
stage — an animated `UsdGeomCamera` plus one `Globe` prim (`tuile-usd`,
doc 13) — and a Hydra host renders the flight. The DCC ecosystem (Solaris,
Omniverse) is a consequence of the same core, not the first target. Film is
one render mode among the others: the selection that feeds Hydra is
`tuile-bake`'s, the same that feeds the wgpu film renderer, so the two
pictures are of the same ground.

Renderers, in order: **Storm** (the GPU rasterizer that ships with OpenUSD —
open source, zero friction), then **Cycles** (the open-source path tracer)
for photorealism. Commercial renderers plug into the same core later; nothing
in the design may depend on any one of them.

## Architecture — the hdGp procedural is the core

The industry's asset-portable mechanism for generated content is the
`GenerativeProcedural` prim (doc 14): stages travel between studios, plugin
installations do not. In Hydra 2.0 a generative procedural already speaks
the scene-index language — its children are `HdContainerDataSource`s — so
the procedural *is* the scene-index-shaped core; no separate scene-index
class was needed.

- **The core (shipped)**: `TuileGlobeProcedural`
  (`integrations/hydra/src/procedural.cpp`) — C++ over the `tuile-hydra`
  C ABI, in-process: the geometry server runs inside the host, no socket.
  The ABI is frame-at-a-time: `tuile_session_frame` blocks until the frame's
  selection is resident (bulk, batch budget) and hands back its tiles in
  traversal order; the procedural turns them into child mesh prims with
  baked-imagery materials, adding, dirtying and removing children as the
  selection changes between cooks.
- **Facade 1 (shipped)**: the `Globe` prim of doc 14, resolved by hdGp —
  the asset that works in usdview and usdrecord.
- **The manifest scene index (shipped)**: for a host that builds its own
  scene and exports no stage (Blender's Hydra path), a scene index plugin
  merges the manifest stage named by `TUILE_MANIFEST` upstream of hdGp's
  resolver (`manifest.cpp`, doc 14).
- **Facade 2 (planned)**: direct insertion of the same core where a host
  offers it (Omniverse extension, Solaris scene index plugin).

Camera dependency follows doc 14's ladder (the host's render camera, then
the explicit `primvars:tuile:cameras`, the render product's camera,
`primaryCameraPrim` on 25.05+, and a fixed view as the view-independent
floor).

## Imagery — bake the mosaic, one texture per tile (shipped)

The wgpu renderer drapes imagery layers as separate resources — a rasterizer
arrangement no offline renderer wants. The portable form is to **bake each
tile's imagery mosaic into a single texture**: `tuile-bake` composes the
drape and encodes it once per tile (`encode_baked`), exposed as
`UsdUVTexture → UsdPreviewSurface` and served to the host through the
`tuile://` resolver. MaterialX only if a renderer needs more (not done).

## The eager contract — identical, fatal

The recorder's decree transposes verbatim: a frame is never delivered soft.
What is in place: a bulk frame is exact — every selected tile resident, no
stand-ins — and the procedural sets `fail_on_tile_errors` and a frame
timeout, so a tile that failed (and would have been covered, plausibly, by
its ancestor) fails the frame, and a dead source fails the job loudly rather
than stalling a node. A slow source slows the farm. **Not yet wired**: a
pixel guard on the Hydra output equivalent to the wgpu film's black-frame
scan.

## Trajectory

| M | Deliverable | Status |
|---|---|---|
| M1 | `usdview`/`usdrecord` + Storm or Embree: a stage with one `Globe` prim streams | **Done** — the core, the C ABI, the camera ladder (`run-spike.sh`) |
| M2 | batch: track → stage (animated camera) → video, eager-fatal | **Done, not yet fast** — Blender farm image, live or pre-baked pack; each frame re-cooks (see below) |
| M3 | Solaris, then Omniverse | Planned; no core changes allowed |

**M2's batch harness is Blender headless, decided 2026-09-07**
(`integrations/blender/`): Blender imports the stage natively, carries both
tiers (Cycles photoreal, EEVEE fast) and encodes video itself. Stock Blender
links OpenUSD statically and exports no symbols, so no third-party procedural
can load into it; the farm uses a Blender built against a **shared** OpenUSD
(`Dockerfile.blender-shared-usd`, Blender 5.2.2, OpenUSD 26.03) with the
plugin compiled against that SDK (`Dockerfile.globe`). `usdrecord` remains
the conformity check: a control frame proves the stage is standard USD, not
Blender-flavoured.

Measured when the harness was chosen: 0.31 s/frame at full quality on
2×RTX 5090 (the product budget is 0.47 s/frame). With the globe procedural
in the loop, Blender re-creates it and it re-cooks every frame — 60–65 s of
CPU per frame on the farm.
`docs/17-hdgp-incremental.md` analyses the incremental cook that removes that
cost; selecting for the host's render camera rather than a re-exported copy
(doc 14) is its prerequisite and is in. The rest is planned.

Throughout: `tuile-usd` (doc 13) writes the manifest stage in pure Rust and
stays free of any OpenUSD dependency by design. Its USDZ export, the
zero-plugin compatibility floor, is planned.

## Backlog

1. The incremental cook: keep the procedural alive across frames, re-select
   only when the camera moved (doc 17).
2. A pixel guard on Hydra output, the twin of the wgpu film's black scan.
3. Run the amd64 builds that are written but not yet executed
   (`Dockerfile.openusd` on amd64, `build-ubuntu.sh`).
4. Facade 2 and M3 hosts.

## Version posture

Develop pinned (26.08 for the standalone OpenUSD builds, 26.03 in the Blender
farm image — doc 14); ≥ 25.05 for `primaryCameraPrim`. The camera ladder
degrades gracefully on 24.x hosts, and nothing else may assume a newer API
without a note here.
