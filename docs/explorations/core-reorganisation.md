<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Reorganising the workspace around a minimal core

Status: **design approved with the decisions below** (2026-11-27); moves in
progress (see Status at the end). The survey it rests on is
[`core-reorganisation-map.md`](core-reorganisation-map.md). Phase 2
(`tuile-volume`) waits until phase 1 has landed.

## Decisions (Laurent, 2026-11-27)

1. **Option A** (§2): dependents change their `use` lines. The minimal core
   keeps the name `tuile-core`.
2. **Step 0 is approved as its own tests-only PR.**
   - The traversal tests move onto a test tree built in Rust: same volumes,
     same pre-order ids, not a JSON reader.
   - The runtime tests that load real glb content stay in the core. They run
     on that tree with a synthetic in-memory loader that returns exactly what
     `decode(test_glb)` returns. A **committed** test proves that equivalence
     bit for bit, so costs and the budget stay checked.
   - What is really 3D Tiles moves to `tuile-3dtiles`: grafts, glb/b3dm
     decoding, the tileset and its loader. That crate keeps at least one
     end-to-end test that drives a `GeometryServer` through the public API
     with a real glb and `FsFetcher`.
   - No `test-support` feature: production code is never compiled
     differently for tests of another crate.
3. **Imagery gets its own crate, `tuile-imagery`**, below `tuile-bing` and
   `tuile-planetary`, which both depend on it. The graph must stay acyclic.
4. **The 3D Tiles crate is named `tuile-3dtiles`.**
5. **The move order is free, provided every PR stays green and small.**
   Moves that touch files other PRs are changing (`raster.rs`, `surface`'s
   `height_at`, `traversal.rs`, `tuile-planetary`, `tuile-terrain/src/mesh.rs`,
   `tuile-budget`) wait until those PRs are merged.

## 0. Summary

1. **The selection is already generic; the residency loop is not.**
   `traversal` names no content: its one content-specific import is
   `Refine`, defined in the 3D Tiles module. `runtime` (`GeometryServer`)
   prices and schedules drapes, decides CPU copies by `is_ground()`, relaxes
   ground under covering sources (`cover`), counts `Tiles3dMesh` and builds
   3D Tiles sources (`in_process`). Its globe and 3D Tiles knowledge comes
   out by changing code, not by moving files.
2. **No move above the seam can be re-exported by `tuile-core`.** The
   `tuile-budget` precedent worked because the budget is *below* the core. A
   3D Tiles crate or a terrain module implements `TileTree`/`TileLoader`, so
   it depends on the core, and the core cannot re-export it (a dependency
   cycle). Either dependents change their `use` lines in the same PR
   (**option A, recommended**) or the minimal core gets a new name and
   `tuile-core` becomes a facade on top (option B). See §2.
3. **Under the brief's definition ("no behaviour change, dependents compile
   unchanged through re-exports"), none of the upward moves is pure.** Under
   a looser one ("no behaviour change, only `use` paths change in
   dependents, digests byte-identical"), some are, and the 3D Tiles move,
   which the brief puts first, is **not** the most isolated: about thirty
   core unit tests use `Tileset` as their fixture tree (§3.3). As the brief
   asks, I stopped here: no move PR is open.
4. **The content type and the budget class open in phase 1.5**, after this
   page. The class follows the registry already designed in
   [`budget-extensible-resource-classes.md`](budget-extensible-resource-classes.md)
   §4.1 (`ClassId(u8)`, the four classes of today pre-registered at 0..3,
   digests unchanged). The content opens with one type-erased variant beside
   the mesh form (§5). Drapes and ground coverage leave the generic traits
   for capability traits (§6), but `tuile-budget` itself knows drapes
   (`ResourceId::Drape`, `ImageryNeed`), so they stay in the budget until
   the registry lands.

## 1. Target layering

| Layer | Crates | Contents |
|---|---|---|
| 0 foundations | `tuile-budget` (exists); inside `tuile-core` for now: `geo`, `math`, `determinism`, `fetch`, `storage`, `offload`, the metrics primitives | f64 geodesy, bounding volumes, clock and determinism, I/O seam, store seam, CPU off-load |
| 1 the seam = reduced `tuile-core` | `tuile-core` | `source` (`TileTree`, `TileLoader`, `Composite`, `Refine`), `traversal`, `runtime` (residency), `protocol` (`GeometryStream`), `drive`, `report`, `neighbourhood`, `content` (the container types: `DecodedTileContent`, `DecodedMesh`, `DecodedTexture`, `MaterialDesc`, `TileContent`), `texels` (the texture payload) |
| 2 contents | `tuile-terrain`, `tuile-planetary` (exist; **no `tuile-globe`**), a new 3D Tiles crate, later `tuile-volume` | each a source on the seam |
| 3 renderers | `tuile-wgpu`, `tuile-bevy`, `tuile-film*`, `tuile-usd`, `tuile-hydra` | |
| 4 hosts | `tuile-viewer`, `tuile-web`, `tuile-bake`, `tuile-tile-server`, examples | |

Splitting the foundations into a crate of their own (`geo` alone has 104
dependent files) buys nothing until something needs them without the seam.
`tuile-volume` does not (it aligns with tile identifiers, which live in
`tuile-budget`): leave them in core.

### 1.1 What moves where

| From `tuile-core` | To | Notes |
|---|---|---|
| `tileset`, `implicit`, `tiles3d`, `content::decode` + `sniff` + `ContentFormat` + `ContentHints` + `ContentError` (glTF and b3dm decoding) | new crate, proposed **`tuile-tiles3d`** (the OGC standard's name, no brand) | takes the `gltf` and `tuile-b3dm` dependencies out of core; `Refine` stays in core (`source`) |
| `runtime::in_process(fetcher, tileset)` | `tuile-tiles3d` | the generic `in_process_with` / `in_process_composite` stay |
| `surface` | `tuile-terrain` | only users (`tuile-wgpu`, `tuile-bevy`) already depend on `tuile-terrain` |
| `stitch`, `seam` | `tuile-terrain` | `stitch::without_skirts` stays in core (`content::steepest_face` needs it) until `ShedMesh::steepest` leaves the content type; `tuile-film-native` gains a direct `tuile-terrain` dependency |
| `raster`: tiling schemes, mosaics, reprojection, providers (`ImageryProvider`, `TemplateProvider`, `CachedImagery`), decoding; `stored` (KTX2); `empty_imagery` | `tuile-planetary`, or a new imagery crate under it (see §8 Q3) | `tuile-bing` uses `empty_imagery` and sits *under* `tuile-planetary`, so planetary is not a home for it. The placement types (`ImageryCoord`, `ImageryLayer`, `Placement`, `drape_identity`, slot constants) stay in core as long as `DecodedTileContent.imagery` holds them |
| `cover` | `tuile-planetary` | only after the runtime takes coverage as a plug (phase 1.5) |
| `ground` (`GroundHeight`) | stays | `tuile-camera` depends on core only; long-term successor is `tuile-ground` (no dependent yet) |
| metric fields named after a content | stay | until metrics become a registry (phase 1.5 or later) |

## 2. The re-export problem, and the choice it forces

`tuile-budget` was extracted *downward*: core depends on it and re-exports it
(`pub use tuile_budget as budget;`). Every move in this programme goes
*upward*: the moved code implements the core's traits. Cargo forbids
`tuile-core → tuile-tiles3d → tuile-core`, so `tuile_core::tileset` cannot
keep existing as a re-export.

- **Option A — dependents re-import (recommended).** Each move PR edits the
  `use` lines of its dependents in the same commit. Volume of the edits:
  3D Tiles: 6 files (viewer ×2, cesium-ion ×2, the wgpu `snapshot` example,
  plus `Refine` users unchanged because `Refine` stays); `surface`: 4 files;
  `stitch` + `seam`: 18 files; imagery: up to 58 files. The core ends up
  being the minimal seam under its own name, as wanted.
- **Option B — facade.** The minimal seam becomes a new crate (it needs a
  name) and `tuile-core` becomes a facade that depends on the seam and on
  every content crate and re-exports them. Nothing outside changes. But
  `tuile-terrain`, `tuile-planetary` and `tuile-bing` would have to depend on
  the new seam crate rather than on `tuile-core`, the core keeps `gltf` and
  `image` in its graph through the facade, and "`tuile-core` is minimal"
  stops being true of the crate called `tuile-core`.

Under A, "dependents compile unchanged" becomes "dependents compile with only
their `use` paths changed". Behaviour is unchanged either way; that is what
the digests prove.

## 3. Why the moves are not pure as briefed

### 3.1 `Refine`

`source::TileProperties` and `traversal` use `tileset::Refine`. Moving it
into `source` (and `pub use crate::source::Refine` from `tileset`) is a
pure, in-crate move with zero dependent changes. It is the necessary first
step of the 3D Tiles move.

### 3.2 `in_process(fetcher, tileset)`

A 3D Tiles constructor living in the runtime. Moving it changes its path
(`tuile_tiles3d::in_process`), used by the `snapshot` example and the core's
own tests.

### 3.3 The core's tests use `Tileset` as their fixture tree

About thirty unit tests and helpers in `runtime.rs` (18), `traversal.rs`
(11) and `drive.rs` (2) build their trees from tileset JSON. Two integration
tests do too.

- Integration tests (`crates/tuile-core/tests/`) can keep them through a
  `[dev-dependencies] tuile-tiles3d`: Cargo links one `tuile-core` library
  for both.
- **Unit tests cannot.** In a dev-dependency cycle the unit-test build of
  `tuile-core` is a second copy of the crate: `tuile_tiles3d::Tileset`
  implements the library's `TileTree`, not the test crate's
  `crate::source::TileTree`, and nothing compiles.

So the 3D Tiles move needs one of:

1. move those tests to `tuile-tiles3d` as integration tests (possible only
   where they use the public API; `runtime` tests reach
   `crate::traversal::tests::BandTree` and `pub(crate)`
   `protocol::in_process_pair`, which would need a `pub` test-support
   surface);
2. rewrite them on a core-internal synthetic tree (the existing `BandTree`),
   which changes what they test and must be done test by test, each one
   watched failing against a reverted behaviour;
3. keep a minimal tileset JSON reader under `#[cfg(test)]` in core: that is
   the duplicated reference code `no-reference-engine-copies` forbids.

None of them is a pure move. **My recommendation: 1 where it fits, 2 for the
rest, each test's change reviewed on its own, in a PR that changes tests
only and leaves `src/` production code alone**, landed *before* the move so
that the move PR itself touches no test.

### 3.4 What actually is the most isolated

| Move | In-core dependents (prod) | External files to re-import | Tests that move | Collides with |
|---|---|---|---|---|
| `Refine` → `source` | none (re-exported in place) | 0 | 0 | — |
| `surface` → `tuile-terrain` | none | 4 | its own (193 lines, public API only) | — |
| 3D Tiles → `tuile-tiles3d` | `runtime` (`in_process`), `traversal`/`source` (`Refine`) | 6 | ~30 core tests + its own | — |
| `stitch` + `seam` → `tuile-terrain` | `content` (`without_skirts`), `surface` | 18 | their own | #334 maybe |
| imagery → planetary / imagery crate | `content`, `texels`, `metrics`, `runtime` | up to 58 | their own | #335, #336 |
| `cover` → planetary | `runtime` | 0 | its own | needs phase 1.5 |

## 4. The seam contract: what a source provides

What the generic traversal and residency need from any source, and nothing
else:

| Need | Today | Stays generic? |
|---|---|---|
| identity | `TileId` (tag + payload, in `tuile-budget`), `roots`, `children`, `parent`, `level`, `revision` | yes |
| bounding volume | `TileProperties::bounding_volume` (`math::BoundingVolume`, f64) | yes |
| geometric error and screen-space error | `TileProperties::geometric_error`, `sse_factor`, `ViewState::screen_space_error` | yes |
| refinement | `Refine` (`Add`/`Replace`), `has_content` | yes, once `Refine` lives in `source` |
| occlusion | `occluder`, `occluder_of` | yes (shape is generic) |
| cost | `DecodedTileContent::cost` / `cost_held`, `DrapeContent::cost` (`budget::cost::Cost`) | the content's, through the class |
| class | `mesh_class -> budget::Class` | **closed: opens in phase 1.5** |
| decoded content | `TileLoader::load -> Loaded::{Content(DecodedTileContent), Expanded}` | **closed: opens in phase 1.5** |
| presence in a store | `holds`, `store_changes`, `stored`, `epoch`, `warm_up` | yes |
| deferred grafts | `defer_edits`, `apply_deferred_edits` | yes (3D Tiles external tilesets use it; generic in shape) |
| globe hooks | `covers_ground`, `drapes`, `load_drape` | **no: leave for capability traits (§6)** |

## 5. Opening the content type (phase 1.5)

- `DecodedTileContent` stays the **mesh form**, unchanged, so every renderer
  and the wire (`glb`) are untouched.
- `Loaded` and `ResourceContent` gain one variant appended last, carrying a
  type-erased payload with its class and cost:
  `Other { class: ClassId, cost: Cost, payload: Arc<dyn Any + Send + Sync> }`.
  `Apart` already exists as that kind of handle and can be reused. A
  renderer that does not know the class ignores it; the budget prices it by
  its class descriptor.
- The globe fields of the mesh form (`imagery`, `shed[].steepest`,
  `withheld_drape`) do **not** move in 1.5: they are read by every renderer
  and by the bake. They leave only when terrain gets its own content form,
  if ever. Say it plainly: in the medium term the mesh form stays
  globe-aware.

## 6. Taking `drapes` and `covers_ground` out of the generic traits

- `TileLoader::drapes` / `load_drape` → a `Drapes` capability trait, asked
  for by `TileLoader::as_drapes(&self) -> Option<&dyn Drapes>` (default
  `None`). `CompositeLoader` forwards it per tag. Same calls, same order:
  the digests do not move.
- `TileTree::covers_ground` → the runtime takes a list of need shapers
  given by the host; `cover::Cover` becomes the planetary's shaper. That is
  the change that lets `cover` leave the core.
- `is_ground()` (CPU copy kept, steepest measured, prefetch neighbourhood)
  → the class descriptor's coverage kind (`Hole`), per
  `budget-extensible-resource-classes.md` §0.3.
- What does not leave: `ResourceId::Drape`, `ImageryNeed`, `Class::Drape`
  are in `tuile-budget`. They stay until the registry's generic resource
  variant exists.

## 7. Clouds and `tuile-volume`: keeping the seam open enough

Laurent's direction: clouds will likely not go through the traversal
(a missing cloud is clear sky, not a hole). `tuile-volume` will therefore
not depend on the core. What it needs from this reorganisation:

- **its own residency interface**, neutral: "here are the bricks wanted,
  with a priority; tell me which to load and which to drop", drivable either
  by a cascade policy around the camera (first) or by an adapter that
  implements `TileTree`/`TileLoader` over it (later, in a crate that depends
  on both);
- **a budget class** (registry, phase 1.5) so its bytes share the one pool,
  coverage kind `Continuity`;
- **tile identifiers** aligned with `budget::TileId`, which live in
  `tuile-budget` (no core dependency needed).

Nothing in phases 1 / 1.5 closes that door. The one requirement on the seam:
the runtime must accept resources that do not come from a `TileTree` (the
registry's generic `ResourceId` variant), or volumes keep their own
residency and only report to the budget. The second is the simpler start.

## 8. Move order

Each step is its own PR, stacked with `gh stack`, bottom on `main`.

0. **Tests off the `Tileset` fixture** (tests only, §3.3). Not a move;
   needs Laurent's go because it changes tests.
1. `Refine` → `source` (pure, in-crate).
2. `surface` → `tuile-terrain` (pure behaviour; 4 `use` paths change).
3. 3D Tiles → `tuile-tiles3d` (after 0 and 1; 6 `use` paths change; `gltf`
   and `tuile-b3dm` leave the core).
4. `stitch` + `seam` → `tuile-terrain` (`without_skirts` stays).
5. Imagery out of `raster` (after #335 lands; decide the home first).
6. Phase 1.5: class registry, open content variant, capability traits,
   `cover` out.

Each PR: no behaviour change; goldens, pattern goldens, `det!` digests and
film goldens byte-identical (no re-bless); the wasm check for core and every
new crate; SPDX headers; `docs/01-architecture.md`, `docs/10-crate-core.md`,
workspace `Cargo.toml` and the `ci.yml` wasm list updated. Runs go through
`scripts/test-on-builder.sh` (branch `ci/tests-on-k8s-builder`, not on
`main` yet), never on the Mac.

What the tests prove for a move: that the moved code compiles where it now
is and that every dependent compiles; that the selection, the decisions and
the rendered goldens are bit-identical. What they do not prove: anything
about code no test reaches (the viewer's native and web launch paths,
`tuile-cesium-ion`'s live test, which needs a token and the network).

## 9. Risks

- **Open PRs on the same files**: #333 (`traversal.rs`), #335 (`raster.rs`),
  #334 (terrain skirts), #336 (planetary). Moves are rebased after them, not
  the other way round.
- **Test rewrites (step 0) can weaken what is tested.** Each one must be
  watched failing against a reverted behaviour, as for any new test.
- **Visibility changes**: moved code may use `pub(crate)` items of the core
  (`protocol::in_process_pair`, test trees). Each such item becomes `pub`,
  which is API surface; list them in the PR.
- **Doc links** (`[`crate::tileset::Tileset`]` in `source.rs`, `runtime.rs`):
  rustdoc breaks with `-D warnings` once the target leaves; fix in the move.
- **Metrics** named after moved contents stay in core: a reader of
  `metrics.rs` will find `tiles3d_*` fields with no 3D Tiles code beside
  them until the metrics registry exists.

## 10. Open questions for Laurent

1. Option A (dependents re-import, minimal crate keeps the name
   `tuile-core`) or option B (facade)?
2. Step 0: may the ~30 core tests that use `Tileset` be moved to the 3D
   Tiles crate or rewritten on `BandTree`, in a tests-only PR before the
   move?
3. Home of the imagery code: `tuile-planetary` cannot host what
   `tuile-bing` uses (`empty_imagery`, `raster` types). A new imagery crate
   under both, or `tuile-bing` depending on something else?
4. Name of the 3D Tiles crate: `tuile-tiles3d` (matches the module and the
   `Tiles3dMesh` class) or `tuile-3d-tiles`?
5. Should the first move PR be `surface` (the most isolated) rather than
   3D Tiles, as the brief ordered?

## 11. What I could not determine

- Nothing was compiled: every edge comes from grep. Glob imports
  (`use super::*`) and macros (`det!`) can hide edges; a move's first CI run
  is the real check.
- Whether each of the ~30 `Tileset`-based core tests can be expressed on
  `BandTree` without losing what it tests: decided test by test, not here.
- Whether `tuile-terrain`'s wasm build stays clean once it takes `surface`
  and `stitch` (it is on the wasm list today, and the moved code is pure
  Rust, so likely, unverified).
- The builder path (`scripts/test-on-builder.sh`) exists only on
  `ci/tests-on-k8s-builder`; I did not run it.

## Status

- 2026-11-27: map and design page written on `refactor/core-reorganisation`.
  Stopped before any move, as the brief asks when moves are not pure.
- 2026-11-27, later: §10 answered (see Decisions). Stack #343 opened:
  - #341: `Refine` → `source`.
  - #342: step 0, the traversal side. 40 traversal tests and 2 runtime
    tests run on `traversal::tests::TestTree`. A throwaway check found the
    trees bit-identical, except the region fixture: 13 of its 63 OBBs differ
    by up to 9.6e-10 m, from serde_json's float parsing. Mutation run: the
    rewritten tests still fail when the SSE factor or ADD refinement is
    broken.
- Builder caveat found on the way: `git archive` dates files by their
  commit, and the target cache is shared by slot. cargo can then reuse
  another run's build of a crate and report results for the wrong tree.
  Every run now starts by touching the tree.
- Next: step 0b (the runtime tests on the synthetic loader, with its
  committed equivalence test), then `tuile-3dtiles`. `surface`, imagery and
  `stitch`/`seam` wait for the PRs that touch their files.
