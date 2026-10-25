# 13 — `tuile-usd` spec

`tuile-usd` writes USD **in pure Rust, without an OpenUSD binding**: USDA is
text. No C++ dependency, and the tile server never depends on it.

| Part | Status |
|---|---|
| The manifest stage: a recorded camera path plus one `Globe` prim, as `.usda` | **Shipped** (`stage.rs`, `tuile-tape-to-usd-stage`) |
| USDZ export of a tileset region (geometry, materials, a trajectory marker) | **Planned**, not started — no exporter, no USDZ writer in the crate |

## Shipped: the manifest stage

A tape (`tuile-tape`, MCAP) becomes a few-kilobyte `.usda` that carries **no
geometry**: it says where the camera is, frame by frame, and that there is a
globe. The tiles are streamed when the stage is rendered, by the
`tuileGlobe` generative procedural of `integrations/hydra` (docs 14, 15).

```text
cargo run -p tuile-usd --bin tuile-tape-to-usd-stage -- flight.mcap flight.usda \
    [--fps 24] [--viewport 1920x1440] [--terrain 0] [--imagery 0] [--sse 0]
```

The library entry point is `write_manifest(&[Frame], &ManifestConfig, &mut impl Write)`.
`ManifestConfig` holds `fps`, `viewport_px`, `terrain_asset_id`,
`imagery_asset_id`, `max_sse`, `first_frame` and an optional fixed `origin`.

What the stage contains, under `/World`:

- `Camera "ShotCam"` — one `matrix4d` time sample per tape frame, time codes
  starting at `first_frame` (1 for a whole shot, the shot's own numbers for a
  stretch of it), so `--frames 1:n` on any driver addresses the instants the
  tape held. Focal length from the tape's vertical field of view; the near plane
  follows the height above the ellipsoid.
- `DomeLight "Sky"` and `DistantLight "Sun"`.
- `GenerativeProcedural "Globe"` with `HydraGenerativeProceduralAPI` applied,
  `proceduralSystem = "hydraGenerativeProcedural"` authored explicitly (the
  schema fallback is not delivered on every host), and its arguments as
  primvars: `hdGp:proceduralType = "tuileGlobe"`, `tuile:cameras` (a **string
  attribute**, not a relationship — hdGp passes arguments as primvars, and a
  `primvars:`-prefixed relationship never reached the procedural),
  `tuile:renderOrigin`, `tuile:terrainAssetId`, `tuile:imageryAssetId`,
  `tuile:maxSse`, `tuile:viewportPx`.

**The render origin.** Every camera sample is written relative to
`primvars:tuile:renderOrigin` — the centroid of the camera path unless the
caller fixes it — because an ECEF camera ~6.4e6 m from the origin would be
quantised into metre-scale jitter by any host holding transforms in f32. The
procedural adds the origin back (to traverse in true ECEF) and subtracts it
from every tile's origin (to place geometry): one subtraction, one addition,
in f64. Every layer rendered with a stage must use the same origin.

Who reads it: `usdrecord` reads it natively; the Blender farm image composes
it into the render through the plugin's manifest scene index
(`TUILE_MANIFEST`, `integrations/hydra/src/manifest.h`) and imports the camera.

Tests (`stage.rs`): rebasing round-trips at ECEF scale, the camera matrix is an
orthonormal USD basis, focal length inverts the field of view, the manifest
text is stable, a stretch keeps the shot's frame numbers and the given origin,
the camera is named by an attribute, an empty tape is refused.

## Planned: USDZ export

Not implemented. The design below is kept as the target for the 3D Tiles →
USDZ bridge (AR viewers, RealityKit, visionOS); when it is built it lands in
this crate as a library plus its own binary.

### Packaging rules (normative, the source of most failures)

1. A **zip** archive where every entry is **stored** (compression 0).
2. Each file's data starts on a **64-byte** boundary from the start of the
   archive (padding through the local header's extra field) — a dedicated
   writer, tested byte by byte.
3. The root layer is the first file in the archive.
4. Textures embedded as PNG/JPEG, referenced by relative path.
5. Validation: `usdview` where available, plus structural assertions
   (alignment, readability by a standard zip reader).

### Content

- USDA text, `metersPerUnit = 1`, `upAxis = "Y"`: content leaves the core in
  local Z-up (rebased ECEF), so the export applies the inverse rotation; the
  origin is the centre of the requested area, ground at y = 0.
- `UsdPreviewSurface` materials only (diffuse from `UsdUVTexture` on `st`,
  metallic/roughness constant); triangle indices → `faceVertexCounts` of 3;
  bounded float precision; an option to merge meshes by material.
- Tile selection: the core's traversal in bounded mode, without a camera — a
  WGS84 area plus a target geometric error, hard limits on tiles and source
  bytes with an explicit error past them, and deterministic output (same
  inputs → same bytes).
- Trajectory replay: a marker `Xform` with `xformOp:translate` time samples
  from a timestamped `[{t, lat, lon, h}]` track, resampled to a cap, plus a
  `BasisCurves` polyline of the whole path; one global timeline, as AR viewers
  require.

### Boundary

The exporter will consume a tileset like any client, through the core's
`TileFetcher`. **The tile server does not depend on this crate and exposes no
USD route** — an architecture boundary, not a detail. If an HTTP export service
is ever needed, it is a separate binary composing the library.
