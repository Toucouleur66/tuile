<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Incremental traversal: what a pass costs, and the part worth keeping

Step 4 of the plan to speed up `GeometryServer::run`. The question was whether
a pass can cost what changed rather than what the tree holds: re-walk only the
subtrees an arrival touched while the camera stands still, and walk everything
when it moves. The selection must not change.

The study answers no for the subtree walk itself. Measured, the walk is a small
part of a pass. Most of a bake's pass goes to one scene-wide number, the
nearest-content distance that uniform detail prices every tile at. That number
depends only on the camera and the tree's shape, so it can be kept between
passes with a proof that nothing changes. This branch does that and nothing
else.

## Measuring first

The baseline tools of Step 0 (`docs/explorations/geometry-loop-baseline.md`)
were used: the same orbit and dive tapes, `run-bake.sh`, `run-viewer.sh`,
`summarise.py`, and the same M2. On top of them, a probe that was **not
committed** wrote one line per pass from `Session::retraverse`:

- whether the views were bit-identical to the previous pass;
- the `traverse()` time, and the time of the nearest-content descent run
  again on its own;
- `visited`, `culled`, and the sizes of the selection, the requests and
  `awaiting`;
- the residency's symmetric difference since the previous pass;
- the symmetric difference of `rendered_last` with the pass before. That is
  the history the hysteresis reads;
- the **dirty closure**: those two sets with every ancestor up to the root.
  That is what a dirty-subtree pass would have to walk again.

The process was also sampled with `sample` for 15 s in the middle of a bake.

### How many passes have a still camera

| run | passes | views unchanged since the last pass |
|---|---|---|
| local bake, orbit 1–96 | 1376 | 1280 (93 %) |
| native viewer, dive tape | 1094 | 656 (60 %) |
| native viewer, orbit tape | 647 | 287 (44 %) |

The bake sends one camera per frame and then answers about fourteen arrival
batches under it. The viewer's tapes move the camera every frame, so a still
camera there means the arrivals in between two frames.

### Where a bake's pass goes

The bake runs with uniform detail (`exact_traversal`, `TUILE_UNIFORM_RADIUS`
8 by default). With the determinism trace off (see below), on main:

| | mean per pass |
|---|---|
| `traverse()` | 1.84 ms |
| of which the nearest-content descent (`nearest_content_distance`) | 1.63 ms |
| of which the walk that selects (`visit`, ~200 tiles visited, ~55 culled) | ~0.21 ms |
| the rest of `retraverse`: closure, trim, `Select`, starting loads | small (profile: ~5 % of `retraverse`) |

The profile agrees: of 543 samples in `traverse`, 522 are on the line that
calls `nearest_content_distance`. The descent follows every branch of the
terrain quadtree that the line of sight enters, down to level 22. Each step
calls `properties()`, which builds an oriented box from a lat/lon rectangle
(`obb_from_rectangle`, about ten trigonometric conversions). It is repeated on
every pass, and with a still camera it finds the same number every time.

### The determinism trace is on by default in the bake

`tuile-bake`'s default log filter is `info`, and that includes the
`tuile_det` target. Each pass then writes one line per visited tile: 378 000
lines and 136 MB of log for 96 frames. With it on, a pass costs 3.5–3.7 ms
instead of 1.3–1.8 ms, so the trace costs more than the traversal. The farm
asks for the trace explicitly with `--trace` (`TUILE_LOG=tuile_det=info`), so
`info,tuile_det=off` as the bake's default would lose nothing. That is a
change of default, so it is **not made here**. It is flagged for its own small
PR.

Every run below sets `TUILE_LOG=info,tuile_det=off` unless it says "trace on".

### The viewer has no descent to save

The interactive configuration does not use uniform detail. The descent never
runs there, so the walk is the whole pass:

| run | visited (mean) | walk mean | walk total | dirty closure / visited, still camera (mean, p50) |
|---|---|---|---|---|
| dive | 553 | 0.17–0.22 ms | 0.21 s over 693 frames | 0.28, 0.04 |
| orbit | 2361 | 1.05–1.27 ms | 0.76 s over 601 frames | 0.004, 0.003 |

Both runs had a warm cache and are not vsync-paced, so only the per-pass
numbers compare.

## The dirty-subtree pass, designed and set aside

### What a subtree's answer depends on

`visit(id)` returns a `Visit` and appends to `selected`, `requests` and
`awaiting`. Every append lands at or after the positions those vectors had
when `visit(id)` began. The drains in a held REPLACE start from marks taken
inside the subtree. `request()` deduplicates by tile, and in a tree a tile is
reached once. So a subtree's contribution is the tail of each vector from its
entry marks, plus additive stats and `max_depth` combined by `max`. That tail
is a function of:

1. `children` and `properties` of every tile in the subtree;
2. residency membership of every tile in the subtree;
3. `rendered_last` membership of every tile in the subtree. It feeds the
   hysteresis threshold, `Visit::drawn`, and `on_screen_already` for the
   descendant limit;
4. scene-wide values: the views, the config, the occluder, the uniform
   distance, `rendered_last.is_empty()` (which switches the hysteresis off on
   a first pass), and the depth from the root.

### The design

- Keep, for each visited tile, its subtree's tails, `Visit` and stats.
- Before each pass, compute the dirty set. It is the ancestor closure of
  - residency flips since the last pass (arrivals, sweeps, `Trim`),
  - `rendered_last` flips (the selection of pass n−1 against n−2),
  - every tile whose `children` or `properties` changed.
  Drop the cache entry of every dirty tile, **visited or not**. An entry kept
  under an ancestor that does not refine this pass would otherwise survive
  stale and be reused when the ancestor refines again.
- Walk as today. On entering a tile with a cache entry, append its tails, add
  its stats, and return its `Visit`. On leaving a tile that was computed,
  store its tails.
- Run a full pass and clear the cache when any scene-wide input changes:
  views, uniform distance, occluder, or `rendered_last` going empty or
  non-empty.

### Why it would select the same tiles

The ordering survives. `selected` keeps depth-first order because tails are
concatenated in child order. `requests` is sorted at the end by
(group, priority, tile), which is a total order, so the order of
concatenation does not matter. `awaiting` keeps the order of its drains.

Everything downstream reads only the `TraversalOutput`, so it is the same:
the protected closure and ancestor pinning (no black ground), `cache.touch`
order, the trim, `Select`'s ancestry, cancellation, the fetch cap, priming,
and the request priority order.

The hysteresis is the subtle part. A pass is not idempotent: the same
residency under the same camera can select differently on the next pass,
because `rendered_last` moved. The `rendered_last` flips in the dirty set are
exactly what replays that. On the bake they are 2.5 tiles per pass on average
and up to 67.

### What could not be identical

- The per-tile `tuile_det` lines (`look`, `select`, `want`, `cull`) would
  only cover the tiles actually walked. The per-pass line and its digests
  would stay identical.
- Tree edits have no channel today. Inline loads mutate the tree inside the
  load future, and `apply_deferred_edits` reports only failures. The dirty set
  needs the edited tiles, so the `TileTree` contract would have to grow an
  edit log, or every edit would force a full pass.
- The cache holds each tile's subtree tails: memory O(output × depth) in all,
  plus a copy of every tail on each full pass. On the bake's camera passes
  (75 selected, 19 requested) that is negligible. On a viewer orbit with
  2400 visited tiles it is a real cost paid on every camera pass.

### The equivalence check it would need

The check used for what was built below: under `cfg(test)`, every pass the
runtime's tests drive runs again through the reference `traverse()` and is
compared field by field. Add a converge loop driving both paths with arrivals,
sweeps and camera moves. A dirty-subtree pass would need the same, plus a
scenario where a dirty tile sits under an ancestor that stops refining and
then refines again. That is the stale-entry case above.

### Expected gain, and the decision

The closure measurements set an upper bound. If a still-camera pass cost
nothing beyond its dirty closure, the gain would be:

- **bake**: the walk is ~0.21 ms of a pass. Closure / visited is 0.09 on
  average, so ~0.03 ms remains, and ~0.2 s is saved over 96 frames. The bake
  takes 33 s and is load-bound.
- **viewer orbit**: ~0.3 s over 600 frames on the server thread. Camera
  passes, 56 % of the total, stay full.
- **viewer dive**: ~0.1 s over 693 frames.

That does not pay for a per-tile cache, an edit log in the `TileTree`
contract, and the risk of a stale entry turning into a missing fallback.
**The dirty-subtree pass is not built.** It is worth looking at again if a
workload appears where the walk dominates under a still camera. A large,
detailed interactive view that the user leaves still is the likely candidate.

## What was built: keeping the nearest-content distance

`nearest_content_distance(tree, views)` reads only:

- the views' positions and directions;
- the tree's roots;
- `children` and `properties` along the way.

It does not read the residency, `rendered_last`, or the clock. The same inputs
give the same bits. So:

- `traversal::NearestContent` keeps the last distance. Its key is:
  - the views, all twelve construction parameters bit for bit;
  - the roots, as a list;
  - the tree's `revision`.
  On a key match it returns the kept distance. On any change, or when the tree
  has no revision, it descends again.
- `traverse_with(…, &mut NearestContent, …)` is `traverse` with the memo.
  `traverse` is unchanged: it calls `traverse_with` with a fresh memo, and
  stays the reference. The runtime keeps one memo per session and uses
  `traverse_with`.
- `TileTree::revision() -> Option<u64>` is new. `Some(r)` promises that
  `children` and `properties` did not change while `r` did not. It never goes
  down. Roots are outside the contract, because the memo compares them itself.
  Default `None`: the descent runs on every pass, as before.
  - `TerrainTree`: `Some(0)`. Its children are all four down to `max_level`,
    and its properties come from fixed fields. Neither reads the
    availability, which is the only thing that changes while a session runs.
    Only the roots read it.
  - `Tileset`: the number of grafts. A graft is the only edit an arena takes.
    `TilesetTree` forwards it.
  - `CompositeTileTree`: the sum of its sources' revisions, or `None` if any
    source is `None`. Revisions only go up, so any source moving moves the
    sum.

There is no switch. A tree that can vouch for its shape gets the kept
distance. A tree that cannot keeps today's behaviour. Neither choice changes
a selection. `traversal::Config` is untouched, so no pack's scene name moves.

### Tests

- `traversal::a_kept_distance_selects_what_a_fresh_descent_selects` runs
  five cameras, each still for twelve passes, with tiles landing four at a
  time. Every pass runs both `traverse` and `traverse_with`, and the outputs
  must be equal field by field (selection with its SSEs, requests, awaiting,
  stats). There must be one descent per camera. The test also asserts that
  consecutive cameras have different distances, so a memo blind to the camera
  cannot pass it.
- `traversal::a_graft_under_a_still_camera_is_priced_afresh`: a graft under
  the line of sight moves the distance from 70 to 115 under a still camera.
  The grafted root's error sits between the two prices, so a stale distance
  changes the requests. The memo must descend again and agree with the
  reference.
- `traversal::a_tree_without_a_revision_descends_every_pass`.
- `runtime::tests::matches_the_reference_pass` runs under `cfg(test)` on
  **every pass of every session the runtime's tests drive**. It runs the
  reference `traverse` and compares.
  `runtime::tests::a_still_camera_is_priced_once_per_camera` drives a
  uniform-detail session to convergence under two cameras through it, with
  one descent per camera.
- `tuile-terrain`'s `a_reveal_leaves_the_shape_the_revision_vouches_for`
  reveals two levels of availability. Every `children` and `properties`
  answer below the roots must be the same, with the same revision.
- The `Quad` test tree in `tuile-bake` now declares `Some(0)`. `QUAD_FLIGHT`
  (`a_flight_over_the_quad_resolves_as_it_always_has`) therefore runs with the
  memo active, and it holds.

Each was checked by reverting the behaviour:

| reverted | fails |
|---|---|
| the memo ignores the views | `a_kept_distance…` (selection differs), `a_still_camera…`, and `QUAD_FLIGHT` |
| the memo ignores the revision | `a_graft…` (requests differ) |
| no graft counting in `Tileset::graft` | `a_graft…` |
| no memo at all | `a_kept_distance…` and `a_still_camera…` (descent counts) |
| `TerrainTree::revision` back to `None` | `a_reveal_leaves…` |

What the terrain test does not cover: it pins the current `TerrainTree`. A
future change that made `children` or `properties` read the availability would
have to move the revision with it. The test would only catch that if the
change affected the tiles it samples.

All goldens are unchanged:

- `QUAD_FLIGHT` (and its inline twin);
- `the_same_frame_selects_the_same_ground_however_fast_tiles_arrive`;
- `tests/projection_bake.rs` and `tests/ground_memory.rs`, both on a real
  `TerrainTree` with uniform detail, so with the memo active;
- every `runtime.rs` test;
- tuile-wgpu `eager_batches` and `fast_zoom_never_empties`;
- the tuile-core, tuile-terrain, tuile-planetary, tuile-bake, tuile-viewer
  and tuile-wgpu suites.

The wasm guard and clippy (`-D warnings`) pass.

## Measured gain

Local bake, orbit tape frames 1–96, 1280×960, ion sources, a fresh
`TUILE_CACHE_DIR` each run, run back to back on the same M2 (another agent's
builds were running at times):

| run | bake s | traversals | traversal mean / peak | traversal sum | arrival wait mean |
|---|---|---|---|---|---|
| main #1 | 34 | 1551 | 1.27 / 16.2 ms | 1.97 s | 0.70 ms |
| main #2 | 32 | 1577 | 1.32 / 18.6 ms | 2.08 s | 0.60 ms |
| this branch #1 | 33 | 1721 | **0.142** / 3.5 ms | **0.24 s** | 0.43 ms |
| this branch #2 | 34 | 1687 | **0.136** / 2.1 ms | **0.23 s** | 0.48 ms |
| main, trace on | 33 | 1362 | 3.46 / 26.1 ms | 4.72 s | 1.64 ms |
| this branch, trace on | 33 | 1418 | 2.89 / 34.7 ms | 4.10 s | 1.26 ms |

Reading:

- A bake's pass is **nine times cheaper** (1.3 → 0.14 ms). Traversal time over
  96 frames drops from ~2 s to ~0.24 s, and an arrival waits ~30 % less for
  the loop.
- Wall time does not move. This local bake is bound by its ion loads, as in
  #80 and #81. The gain shows where the loop is the bottleneck: the farm,
  whose loads come from the tile store.
- With the trace on, the trace is most of a pass (2.9 of 3.5 ms). The
  traversal gain is then mostly hidden, which is why the default above is
  worth its own PR.
- The traversal count varies by ±10 % between runs of the same binary,
  depending on how arrivals batch.

The selection is unchanged. A real bake is not byte-reproducible from run to
run, on main or here: the baseline doc names the hysteresis and arrival order
as the cause. `tuile-bake --diff`:

| pair | identical frames |
|---|---|
| main #1 vs main #2 | 93 / 96 |
| branch #1 vs branch #2 | 91 / 96 |
| main #1 vs branch #1 | 91 / 96 |
| main #2 vs branch #2 | 94 / 96 |

The differences are of the same kind in all four pairs: a level-17 parent in
one run, its four level-18 children in the other. That is why the proof rests
on the goldens with an instant loader and on the pass-by-pass comparison, not
on real bakes.

## What was not measured

- **The web viewer.** Its configuration has no uniform detail, so this change
  does nothing there, and its timers read zero on wasm.
- **The farm.** The expected effect is fewer CPU-milliseconds per arrival on
  the loop, but that is a forecast, not a measurement.
- **A cold cache.** As in the baseline, the HTTP cache was warm.
