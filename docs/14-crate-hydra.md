# 14 — `tuile-hydra` spec

Streams tiles into any OpenUSD scene, so a scripted-camera flight can be rendered on a farm and, later, so anyone can drop a prim into a stage and get a streaming globe.

> **Doc 15 carries the vision and trajectory** (`docs/15-usd-scene-index.md`).
> In Hydra 2.0 a generative procedural's children already are scene-index
> data sources, so the procedural below *is* the scene-index-shaped core; no
> separate scene-index class exists or is needed for it.

**Status (v0.14.0).** Shipped: the C ABI, the `tuileGlobe` procedural, the
manifest scene-index plugin, the camera ladder, live (ion) and pre-baked
(pack) sources, and the farm images under `integrations/blender/` that render
it in Blender (Cycles, EEVEE) and check it with `usdrecord`. Measured, not yet
solved: each frame re-cooks from scratch — the incremental cook is analysed in
`docs/17-hdgp-incremental.md` (planned).

Three parts, deliberately separate:

| Where | What | Language |
|---|---|---|
| `crates/tuile-bake` | The work: session, globe, traversal on the batch budget, pack reading, `Frame` | Rust, no OpenUSD |
| `crates/tuile-hydra` | A C ABI over that session (`src/ffi.rs`), built as a staticlib | Rust |
| `integrations/hydra` | The OpenUSD plugin that consumes it (`src/procedural.cpp`, `manifest.cpp`, `hostpaths.cpp`, `view.cpp`, `tiles.cpp`, `resolver.cpp`) | C++ |

`tuile-usd` (doc 13) is separate and stays that way: it *writes* the manifest stage as plain `.usda` text, with no OpenUSD dependency. This crate *renders inside* OpenUSD and needs the real thing.

## Why a camera dependency is legitimate

The engine picks tiles by screen-space error, which makes the scene depend on the view, and Hydra's contract is that a scene can be re-rendered from any camera without changing. The worry is misplaced.

OpenUSD 25.05 added `primaryCameraPrim` to `HdSceneGlobalsSchema` with the release note: *"The primary camera is intended for use by scene indexes that want to do camera-dependent scene transformations, filtering, or generation."* And `HdGpGenerativeProcedural::UpdateDependencies` returns a map keyed by arbitrary `SdfPath`, so depending on a camera prim's transform is a first-class feature rather than a trick.

What keeps a frame reproducible is that the camera we refine for is **named by the scene**, not taken from a viewport. The geometry stays a function of (stage, time).

### Which camera — the ladder

Each step down is one step further from the camera the image is actually made by (`TuileGlobeProcedural::_ResolveCamera`, order between the first two held by `TuileSelectCamera` in `hostpaths.h`, tested by `tests/cameraRungTest.cpp`):

1. the camera the host renders **through** — `TUILE_RENDER_CAMERA`, else `/freeCamera` (Blender's render-task camera, moved every frame without rebuilding the stage) — then `primvars:tuile:cameras` on the procedural prim, the path a pipeline pins. Render camera first because a host may hold only a re-exported copy of the authored one; selecting for the copy is selecting for a camera that is only usually the right one. The authored path is also the only way to refine for several views at once (a stereo pair must share one selection or the eyes disagree at LOD boundaries);
2. the camera of the render product being rendered — the product is also where `resolution` lives, which a real screen-space error needs (`primvars:tuile:viewportPx` is the fallback);
3. `HdSceneGlobalsSchema::GetPrimaryCameraPrim()` — what `usdrecord` sets, and what a viewport points at its free camera, so deterministic in batch and not in a viewport;
4. nothing — fall back to a fixed geometric error. That mode is view-*independent*, which makes the asset usable by someone who opens the stage knowing nothing about us.

## The prim

```usda
def GenerativeProcedural "Globe" (
    prepend apiSchemas = ["HydraGenerativeProceduralAPI"]
)
{
    token  primvars:hdGp:proceduralType = "tuileGlobe"
    token  proceduralSystem = "hydraGenerativeProcedural"
    string primvars:tuile:cameras = "/World/ShotCam"
    double3 primvars:tuile:renderOrigin = (…)
    # also: tuile:terrainAssetId, tuile:imageryAssetId, tuile:maxSse, tuile:viewportPx
}
```

This is what `tuile-usd`'s `write_manifest` emits (doc 13). Three details that are each silent when wrong:

- **`apiSchemas` is not optional, and `proceduralSystem` is authored anyway.** Without the schema, `proceduralSystem` is unauthored and has no fallback, `UsdProcImagingGenerativeProceduralAdapter::_GetHydraPrimType` returns `inertGenerativeProcedural`, and the prim produces nothing. The schema's fallback `"hydraGenerativeProcedural"` was not delivered on every host (the Blender build, 2026-09-08), so the manifest authors it explicitly.
- **Arguments are primvars, and the camera is a string attribute.** It was a `rel primvars:tuile:cameras`; the procedural never saw it, ran down its ladder, and fell back to the view-independent mode — every baked frame 8000 m from its pose, the orbit's radius. A path is now written as a `string` primvar and read by name.
- **`proceduralType` matches `displayName`** in our `plugInfo.json` first, and only then the registered `TfType` name (`HdGpGenerativeProceduralPluginRegistry::ConstructProcedural`).

## Environment — the one that costs an afternoon

```sh
export HDGP_INCLUDE_DEFAULT_RESOLVER=1
```

`hdGp`'s scene index plugin is registered unconditionally but gated on this `TF_DEFINE_ENV_SETTING`, which **defaults to false**. Unset, the procedural-resolving scene index is never inserted: no warning, no error, a valid and completely empty image. A plugin can neither detect nor fix its own absence.

Also required, pointing at the directory that holds `plugInfo.json`:

```sh
export PXR_PLUGINPATH_NAME=<build>/plugin/tuileHydra/resources
```

The farm image sets both (`integrations/blender/Dockerfile.globe`). The plugin reads the rest of its configuration from the environment, never from the stage — a token or a pack path does not belong in an asset:

| Variable | Read by | Meaning |
|---|---|---|
| `TUILE_PACK` | `procedural.cpp` | A pre-baked pack: the whole scene, no network, no token |
| `TUILE_SCENE` | `procedural.cpp` | The scene digest the job expects; a pack of another shot is refused |
| `TUILE_ION_TOKEN` | `procedural.cpp` | Live sources, when there is no pack |
| `TUILE_CACHE_DIR`, `TUILE_FRAME_TIMEOUT` | `procedural.cpp` | Tile cache; how long a frame may take to converge |
| `TUILE_MANIFEST` | `manifest.cpp` | A manifest stage merged into the host's scene (below); inert when unset |
| `TUILE_RENDER_CAMERA` | `hostpaths.cpp` | The host's render camera (default `/freeCamera`) |
| `TUILE_TEXTURE_SPILL_DIR` | `tiles.cpp` | Where the per-process drape spill file lives |

**The manifest scene index.** A host that builds its own scene (Blender's Hydra path) never exports a stage, so a `GenerativeProcedural` prim has no way in. `manifest.cpp` registers a scene index plugin for every renderer at insertion phase 1 — upstream of hdGp's resolver at phase 2 — that merges the stage named by `TUILE_MANIFEST` into the render index's chain, so the procedural is visible to what cooks it, and the host's own prims (`/freeCamera`) are visible to the procedural. Tested by `tests/manifestRungTest.cpp`.

## The C ABI

A specialised facade, consistent with `docs/01-architecture.md` ("no generic FFI facade: each facade is specialised for its host"). Entry points: `tuile_session_new`, `tuile_session_frame`, `tuile_session_free`, `tuile_frame_tile_count`, `tuile_frame_tile`, `tuile_frame_texture`, `tuile_frame_free`; configuration in `TuileGlobeConfig` (ion asset ids, cache directory, screen-space error, frame timeout, `fail_on_tile_errors`, `pack_path`, `scene_digest`), views as `TuileViewState` (ECEF f64). The session itself is `tuile_bake::Session`: the core's `runtime::in_process_with` on the **batch budget**, driven until the frame is complete. With `pack_path` set, nothing else in the config is consulted — the bake decided it.

Three rules, and the reason the ABI (`tuile-hydra/src/ffi.rs`) is separate from the session (`tuile-bake/src/session.rs`):

1. **Nothing unwinds.** A panic crossing `extern "C"` aborts the process, so every entry point catches and returns a `TuileStatus`.
2. **Rust owns every allocation.** C++ borrows pointers valid until it releases the frame that produced them.
3. **Null is checked, never assumed.**

Nothing is copied on the way out: `Vec<[f32;3]>`, `Vec<u32>` and tightly packed `rgba8` are already contiguous.

**Precision.** `origin_ecef` is `f64`; positions are `f32` relative to it. A consumer computes `origin_ecef - render_origin` in **double** and only then narrows. Doing that subtraction in `f32` reintroduces exactly the jitter the arrangement removes — do not route it through `geo::rebased_model`, which casts.

**Ordering.** Tiles come out in traversal order, from `BulkFrame::selected`. The driver's `contents` is a `HashMap` with a per-process seed; walking it emits prims in a different order every run, and a farm comparing two renders of the same frame reports a change that is not there.

**Textures.** Tiles decode to raw RGBA and `Hio_StbImage` decodes encoded bytes only, so the boundary PNG-encodes on demand and caches per (tile, texture) — the cache is what makes the borrow valid for the frame's life (`tuile_frame_texture`). The re-encode is real CPU per texture per frame; it is the cost of draping a private copy per tile and it goes away when imagery is referenced rather than baked. On the C++ side the drapes are spilled once into a single per-process file and served through an `mmap` window (`tiles.cpp`), not held as one buffer per texture.

The URI is `tuile://<dataset>/tile/<id>/texture/<n>.<drape>.png` — `<drape>` a 16-digit hex name of the drape's bytes, so a tile re-draped at another level is a different asset and no cache serves yesterday's sharpness — built by `tuile_bake::Frame::texture_uri` and by nothing else — the material names it and the resolver is handed it back, so two derivations would eventually drift and every texture would silently resolve to nothing.

**The dataset in that path is load-bearing.** A `TileId` is unique within one *tree*, not globally: two sessions — two ion assets, or a terrain-only globe beside a textured one — hand out the same ids for different tiles, and a resolver keyed on the id alone would serve one session's texture to the other. Silently, because the bytes are a valid PNG either way and nothing errors; the wrong imagery simply appears.

It is a **name, not a counter**. The globe derives it from the ion asset ids (`ion-1-2`, or `ion-1-noimagery`; `tuile-bake/src/globe.rs`), not from an open-order counter: a counter would make two farm nodes emit different asset paths for identical data, and a comparison would report a difference that is not there. Same reasoning as iterating `selected` rather than the `HashMap`. The token, the cache directory and the screen-space error are deliberately *not* part of it — they change how the same tiles are fetched, so two sessions differing only in them serve identical imagery and sharing a name is correct.

**Determinism.** A frame has a timeout, because a bulk frame blocks until every selected tile is resident and both ways that can fail — a fetch that hangs without failing, a resident budget too small for the working set — look identical on a farm: a node that stopped. `fail_on_tile_errors` defaults to true, because a failed tile leaves no hole (its ancestor stands in) and the frame renders plausibly at the wrong level of detail.

## Versioning

Pinned per host. OpenUSD's internal namespace is version-stamped and there is **no ABI stability guarantee**: a plugin is only loadable by the exact release it was built against, so a rebuild per OpenUSD major.minor is unavoidable and an unpinned base image produces a plugin that silently fails to load. Two pins exist today, and C++ that builds against one can break against the other:

- **v26.08** — `Dockerfile.openusd`, `build-macos-metal.sh` and `build-ubuntu.sh` (OpenUSD plus the Cycles Hydra delegate, patched by `patches/cycles-usd2608-hydra.patch`);
- **v26.03** — the farm's Blender image (`integrations/blender/Dockerfile.blender-shared-usd`, Blender 5.2.2 built against a shared OpenUSD 26.03 in Blender's `pxrBlender` namespace), which `Dockerfile.globe` compiles the plugin against.

## Build

`integrations/hydra/CMakeLists.txt` drives cargo and links the resulting staticlib. The link hides every symbol (`--exclude-libs,ALL`, or `-load_hidden` on Apple): a Rust staticlib carries its whole runtime, and exporting it would let the dynamic linker resolve someone else's call into our copy. Nothing here is meant to be visible — the plugin is reached through OpenUSD's registries, never by symbol.

### Images

The OpenUSD base is built **once and tagged**, and the plugin image starts `FROM` it. That separation is not tidiness: the base takes the better part of an hour and the plugin takes seconds, so folding them into one build means every plugin edit risks re-entering an hour-long layer, and one interrupted session throws the hour away.

| File | Produces | For |
|---|---|---|
| `integrations/hydra/Dockerfile.openusd` | the base: OpenUSD and nothing else | one recipe, CUDA-capable by default (Storm and Embree); `--build-arg BASE_IMAGE=ubuntu:24.04` for an arm64 developer machine (Embree only) |
| `integrations/hydra/Dockerfile` | the plugin | `FROM` a base tag (`--build-arg BASE=`); rebuilds in seconds |
| `integrations/blender/Dockerfile.globe` | the farm image | the Blender fork's base, the `tuile-hydra` staticlib and the plugin, linked against the fork's own USD SDK |

```sh
docker build -f integrations/hydra/Dockerfile.openusd --build-arg BASE_IMAGE=ubuntu:24.04 \
    -t tuile-openusd:26.08-local-arm64 .                                             # once, ~1h
docker build -f integrations/hydra/Dockerfile --build-arg BASE=tuile-openusd:26.08-local-arm64 \
    -t tuile-hydra:local .                                                           # seconds
docker run --rm tuile-hydra:local integrations/hydra/tests/run-spike.sh
```

There is no separate CPU or GPU recipe any more: a CUDA base only *offers* CUDA, Embree renders with no device at all, and two recipes meant to produce the same OpenUSD drift. The renderer is a run-time flag, not a build.

The base grows by **appending** to `Dockerfile.openusd`, never by editing the existing instructions. The text of a `RUN` is its cache key, so changing the apt list, the `-j` expression, or the order of the first three build steps discards an hour of compilation. A component added later is a new leaf layer that reuses everything before it: `build_usd.py` keeps its dependency sources and builds under the install prefix, so a second run detects boost, TBB, OpenSubdiv and Embree as already installed and builds only what is new.

The pipeline is not only 3D Tiles — shading, atmosphere, animation, imported imagery, volumes and materials are all in scope — so the base carries **MaterialX, OpenImageIO, OpenColorIO, OpenVDB, Alembic, Ptex and Draco**.

No HDF5: the option was removed in v26.08, which builds Alembic with `USE_HDF5=OFF`. Ogawa-backed `.abc` files load; the historical HDF5-backed ones do not.

Worth knowing before assuming any of them is present: six are **opt-in** in `build_usd.py` — `--ptex`, `--openvdb`, `--openimageio`, `--opencolorio`, `--alembic` and `--draco` all default to off. Only `--materialx` and `--usdview` default to on. Omitting them from a command line is the silent default, not a decision.

`usdview` stays off. It is the most expensive item by a wide margin (Qt + PySide) and useless on a headless farm node; inspecting a stage by hand belongs in a separate development image rather than in every farm pull.

**To verify when OpenImageIO lands, rather than assume:** `hioOiio` may take precedence over `Hio_StbImage` when decoding images. The `tuile://` texture path serves PNG bytes from memory and is only proven against stb.

Storm compiles against GL headers and needs a driver only at run time, so one recipe yields both Storm and Embree and what the CUDA base adds is the driver stack underneath. The amd64 variant of `Dockerfile.openusd` is **not yet executed** (it needs an amd64 host with enough memory); the arm64 variant is built and proven. An arm64 image validates the plugin locally but cannot ship to an x86_64 farm without a `--platform linux/amd64` build.

## What the spike proved, and the four traps it found

`integrations/hydra/tests/run-spike.sh` renders two frames of one stage whose camera moves from 12 units to 4, in a **single** `usdrecord` process. The procedural refines from 16 divisions to 50 and the images go from 5.1% to 45.6% coverage. That establishes the whole chain: the plugin loads, `hdGp` resolves the procedural, the camera dependency fires, the re-cook produces new geometry, and Embree renders it.

Each of the following was found by running it, and each fails silently or misleadingly.

**A refinement change must be a new prim, not a dirtied one.** Dirtying a mesh whose *vertex count* changed is not enough. Traced against hdEmbree: the procedural re-cooked correctly and produced the finer grid, the renderer re-read the topology, and the frame came out **empty** — a 289-vertex topology indexing a 4-vertex point buffer it never re-read. The child prim's path therefore encodes its refinement, so hdGp removes one prim and adds another and no stale buffer survives to disagree. That is also the honest model for real tiles, which appear and disappear rather than mutating in place.

**`LibraryPath` in `plugInfo.json` is relative to `Root`, not to `plugInfo.json`.** `Root` is relative to the file; `LibraryPath` is then relative to `Root`. One `../` too many yields `cannot open shared object file`, which reads like a missing build rather than a wrong path.

**OpenUSD installs its Python modules under a version-stamped directory** — `/opt/usd/lib/python3.12/site-packages`, not `/opt/usd/lib/python`. The wrong `PYTHONPATH` surfaces as `usdrecord: ModuleNotFoundError: No module named 'pxr'`. The image registers a `.pth` with the system interpreter instead, and asserts the import at build time.

**`usdrecord` writes RGBA with a fully transparent background.** A viewer composites it over white, so a blank frame *looks* white while every background pixel reads as `(0,0,0,0)`. Measure colour and you conclude the image is entirely covered when it is entirely empty — which is exactly what the first version of the assertion did.

That first assertion was `cmp -s`: "the frames differ". Two blank frames differing by three anti-aliased pixels satisfied it, so it reported PASS on a render that showed nothing. `tests/compare_frames.py` now measures alpha coverage per frame first and the difference second, because a blank frame makes the difference meaningless.

Set `TF_DEBUG=TUILE_HYDRA_PROCEDURAL` to trace cooks and the resolved camera. It stays in the shipped plugin: a procedural that cooks once and then quietly stops produces a plausible image and no error at all.

## Verification

`integrations/hydra/tests/run-spike.sh` renders two frames of one stage whose camera moves, and fails if the images are identical. Both frames are rendered in a **single** `usdrecord` process on purpose: two processes would each cook from scratch and differ even if invalidation were completely broken, which is the one failure the test exists to catch.

Beside it: `cameraRungTest.cpp` (the order of the camera ladder's first rung), `manifestRungTest.cpp` (the manifest scene index merges upstream of the resolver), `frameProbe.cpp`, and `compare_frames.py` (alpha coverage first, difference second). On the farm side, `integrations/blender/test_job_scripts.py` covers the job scripts.

## Open risks

- Depending on `/` + `GetCurrentFrameLocator()` to force a re-cook per frame is **inferred, not proven** — no test or doc exercises it. If it does not hold, the camera's own animated transform is the dirtying signal, which is enough for a moving-camera shot and weaker in general.
- `renderSettings.active` may not be populated unless the host inserts `HdsiRenderSettingsFilteringSceneIndex`. Ladder step 2 degrades to step 3 rather than assuming.
- Cost: under Blender the procedural is rebuilt and re-cooks every frame — measured at 60–65 s of CPU per frame on the farm. `docs/17-hdgp-incremental.md` lays out the incremental cook (keep the procedural alive across frames); planned, not done.
- Bulk mode can hang if the resident budget cannot hold the frame's working set — eviction during convergence prevents the request count reaching zero. The frame timeout turns that into a loud failure instead of a stuck node.
