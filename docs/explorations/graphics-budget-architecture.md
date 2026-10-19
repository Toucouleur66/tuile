<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — terrain and imagery, together or apart; is the core API right

Phase 1a of the budget plan: a decision gate. The spec
(`graphics-budget-to-be-and-spec.md`) is amended only once the owner has decided
on this document.

Three questions:

1. **Streams.** Is imagery a stream of its own beside the terrain geometry
   stream, or a payload of it?
2. **Budgets.** One arbiter over one GPU pool, or two budgets (terrain,
   imagery) with a split of the pool?
3. **Core API.** Does `Budget` (`decide(FrameInput)` → `Decisions`, the `on_*`
   events, `residency()`, `PresenceOracle`), together with `GeometryStream`,
   `ClientMessage` and `ServerMessage`, fit every host?

The criteria are the plan's: no black ground, the mesh + drape swap atomic
(I9), GPU memory one physical pool, imagery independent of the terrain level
(I10), several imagery layers, a per-frame deadline (I18), wasm32 with no
tokio or wgpu in the core, one path and no switch.

Two kinds of evidence: a simulation of both budget shapes over the existing
scenarios (§3), and a walk-through of each host's call sequence against the
candidate signatures (§4).

## 0. Summary

- **The plan's leaning is confirmed for the budget, and refined for the
  stream.** One arbiter wins on every measure that matters for the ground;
  two budgets lose even at the same split of the pool. Imagery travels as its
  own resources and messages, but it is not a second, independent producer:
  its need comes from the same traversal and its decisions from the same
  arbiter. An imagery stream that only learns what the terrain stream drew is
  refuted by the data (a hundred times more bare ground).
- **What the data says, in four numbers** (three seeds per cell, §3):
  - drapes evicted while the frame wears them: **0** for the one arbiter in
    every scenario, **4 to 11** per scenario for two budgets under memory
    pressure;
  - convergence at the phone's low clamp: **3.3 s** for the one arbiter,
    **8.0 s or never** for two budgets (never in 1/3 runs at 44/56, 3/3 at
    60/40);
  - bytes over the pool's target at the low clamp: **0** steps for the one
    arbiter, **536 to 956** steps, up to **10.5 MiB**, for two budgets;
  - `decide` cost: two budgets cost **1.5 to 4.6 times** one (desktop still
    1.1 ms → 1.7 ms; low clamp 0.22 ms → 0.71–0.99 ms).
- **The ratio matters in both shapes.** The one arbiter's own per-class split
  moves the low-clamp result between 2.0 s and 3.3 s and the blur under a
  memory warning between 19 % and 49 %. A fixed split is fragile; the
  arbiter is not, but its per-class targets are a split too. Whether to pool
  the ledger with per-class floors and ceilings is an open decision (D-A3).
- **The API is close.** `Budget` is host-neutral already (pure, wasm32,
  time as an input, the stereo union in the traversal). What does not fit is
  the stream around it: `ClientMessage`/`ServerMessage` speak of tiles, not
  resources; `Ack` carries no bytes or generation; imagery has no message of
  its own; the viewport is symmetric only (no headset eye); one imagery
  layer per drape; and the per-frame upload cap is decided where the clock is
  not. §5 lists the signatures.
- **A gap the experiment found in the spec.** At start-up every
  architecture draws a few floor meshes with no drape at all (4 to 8
  tile-steps per scenario, scenario-independent): I1 counts meshes, not
  drapes. Bare ground is black ground. D-A1 proposes the fix.

## 1. Today

**The budget module** (`feat/budget-core`, `crates/tuile-core/src/budget/`)
is one engine, one ledger, classes with quotas (`Class::{TerrainMesh,
Tiles3dMesh, Drape, Overlay, StandIn}`, each with `target`, `overflow`,
`hard_limit`, `max_in_flight`), and two kinds of resource,
`ResourceId::{Mesh(TileId), Drape(TileId, level)}`. Imagery is already a
resource of its own in the budget: a drape is loaded, charged, protected and
evicted on its own, its level chosen per tile from a texel footprint, and
`DrawItem { mesh, role, drape: Option<DrapeChoice>, serves }` pairs a mesh
with the drape it wears (its own, or an ancestor's through a UV transform).
It is plugged into nothing.

**On `main`** the terrain stream carries imagery inside the terrain tile:

- `GeometryServer` (`crates/tuile-core/src/runtime.rs` 382) runs one session:
  traversal, `ResidentCache`, loads, `ServerMessage::Content { tile, ancestry,
  content }`.
- `DecodedTileContent::imagery: Vec<ImageryLayer>` (`content.rs` 87) is the
  drape, "referenced rather than owned" and charged separately by
  `ImageryCoord` in the cache (`runtime.rs` 1447-1460), but it arrives in the
  same message as the mesh, and its level is frozen at load time.
- That level is chosen by `tuile-planetary` when the tile loads
  (`lib.rs` 1170-1186): from the terrain tile's geometric error, raised by
  the host's global altitude target `ImageryDetail` (`lib.rs` 298), set every
  frame by the viewer (`app/frame.rs` 111-123), by the web page through a
  side channel beside the stream (`ToWorker::Detail`, `web/control.rs` 35,
  `web/worker.rs` 148), and by the bake session (`session.rs` 1098-1117).
- Changing a tile's imagery therefore means loading the tile again
  (`TileLoader::epoch` refresh, `runtime.rs` 1270), mesh included.

So the budget already separates them; the stream couples them.

## 2. The candidates

| | Streams | Budgets |
|---|---|---|
| **A** (plan's leaning) | imagery as its own resources and messages, one session | one arbiter: one ledger, per-class quotas, one priority order, joint draw set |
| **B** | the same | two `Budget`s, terrain and imagery, the pool split between them |
| **B′** | an imagery stream fed by what the terrain stream draws | two `Budget`s |
| **C** (today on `main`) | imagery a payload of the terrain tile | one cache, imagery charged by coord |

C cannot meet I10 (imagery independent of the terrain level, changing
without the mesh) without resending the mesh; it is measured below only for
what that resend costs.

## 3. Evidence A: the simulation

### 3.1 Method

`crates/tuile-core/tests/budget_sim/architecture.rs` on `feat/budget-core`
(commit `b9eda88`, test-only, `#[ignore]`d; run with
`cargo test --release -p tuile-core --test budget_sim -- --ignored architecture --nocapture`).
It replays the simulation's world, network, GPU and traversal unchanged, with:

- **shared**: today's engine (A).
- **shared-60/40, shared-40/60**: the control, the same one arbiter with its
  per-class targets re-split at those shares of the same pool. The pool is
  the shared profile's `hard_limit[TerrainMesh] + hard_limit[Drape]` (35 % +
  45 % of the device budget, so the default split is 44/56).
- **split-S/I** (B): two `Budget`s. The terrain one sees the need with its
  imagery removed and owns the meshes; the imagery one sees the same need
  and owns the drapes. Pool, upload bytes per frame, delivery credit and
  decoded tier are split S % / I %. The imagery budget learns which meshes
  are drawable by a mirror: its mesh requests never reach the network and
  are answered once the terrain GPU holds the mesh, one or two steps late,
  as a message between two budgets would be. The frame is the terrain
  budget's draw set, each item wearing the imagery budget's drape for it
  or, failing that, the finest drape the imagery GPU holds on the tile or its
  nearest ancestor.
- **split-drawn-44/56** (B′): the imagery budget only sees what the terrain
  budget draws.

The split variants are given every benefit of the doubt: a 4096-entry request
heap for the imagery budget (so its mirrored mesh wants cannot starve its
drapes), mirrored meshes costing one byte, and a frame that may wear any drape
the imagery GPU holds.

Scenarios (from `scenarios.rs`, stalls off so convergence is measurable,
failures on): **still** (5 s approach, 10 s still, desktop), **turn** (180°
in 1 s, desktop), **zoom-out-back** (to 200 km and back, desktop),
**back-and-forth** (48 s, desktop), **memory-warning** (handheld, a warning
mid-zoom), **low-clamp** (handheld at 160 MiB). Three seeds each.

Measures, the same outside the budgets for every variant:

- **lost**: tile-steps with nothing to draw once the floor is in (I1);
- **bare**: drawn items wearing no drape at all once the floor is in;
- **worn evicted**: drapes evicted in the step the frame wears them (I11; a
  hole next frame, or a use-after-free on a real GPU);
- **blurry %**: exact tile-steps whose drape is more than a level coarser
  than the tile's *unrelaxed* imagery need;
- **converged s**: from the stop to the start of the converged run that
  lasts to the end (max of seeds; "never" if a seed ends unconverged);
- **over-target steps / MiB**: steps the pool's bytes exceed the pool's
  target, and the worst excess; **over-hard**: a class over its hard limit;
- **flapping** (I4), **decide µs** (mean, p99; wall clock of all `decide`
  calls of a step, release build, this Mac), **net MiB**, **thrash**;
- **redrapes**: exact tiles whose own drape changed level while their mesh
  stayed, and the mesh bytes that would have been resent under C.

### 3.2 Key results

| scenario | variant | worn evicted | bare | blurry % | converged s | over-target steps (max MiB) | decide mean / p99 µs | net MiB |
|---|---|---|---|---|---|---|---|---|
| still | shared | 0 | 8 | 3.0 | 0.22 | 0 | 1085 / 1407 | 116.6 |
| still | split-44/56 | 0 | 7 | 4.9 | 0.72 | 0 | 1704 / 2216 | 117.1 |
| still | split-drawn | 0 | **1290** | 8.8 | 0.70 | 0 | 1544 / 2183 | 116.7 |
| turn | shared | 0 | 8 | 2.7 | 1.55 | 0 | 1057 / 1523 | 152.0 |
| turn | split-44/56 | 0 | 7 | 3.7 | 1.62 | 0 | 1657 / 2442 | 153.1 |
| turn | split-60/40 | 0 | 7 | 6.1 | 1.62 | 0 | 1681 / 2900 | 152.9 |
| zoom-out-back | shared | 0 | 8 | 5.2 | 0.00 | 0 | 2250 / 6887 | 301.6 |
| zoom-out-back | split-44/56 | 0 | 7 | 2.8 | 0.05 | 0 | 3410 / 9951 | 307.9 |
| zoom-out-back | split-60/40 | 0 | 7 | 4.1 | 0.00 | 9 (9.9) | 4034 / 10846 | 360.7 |
| memory-warning | shared | 0 | 4 | 24.7 | 0.00 | 0 | 263 / 614 | 51.9 |
| memory-warning | shared-40/60 | 0 | 4 | 18.6 | 0.00 | 0 | 262 / 621 | 51.5 |
| memory-warning | shared-60/40 | 0 | 4 | 49.2 | 0.00 | 0 | 220 / 474 | 44.6 |
| memory-warning | split-44/56 | **11** | 4 | 34.6 | 0.07 | 0 | 649 / 1839 | 60.4 |
| memory-warning | split-60/40 | **4** | 4 | 61.6 | 2.75 | 0 | 861 / 2606 | 65.2 |
| memory-warning | split-40/60 | **8** | 4 | 26.9 | 0.22 | 0 | 616 / 1691 | 59.0 |
| low-clamp | shared | 0 | 4 | 10.5 | **3.33** | 0 | 218 / 374 | 18.8 |
| low-clamp | shared-60/40 | 0 | 4 | 30.2 | 2.02 | 0 | 173 / 272 | 16.0 |
| low-clamp | shared-40/60 | 0 | 4 | 8.7 | 3.22 | 434 (0.4) | 220 / 385 | 19.3 |
| low-clamp | split-44/56 | **7** | 4 | 35.4 | **never (1/3)** | **956 (10.5)** | 780 / 1764 | 26.7 |
| low-clamp | split-60/40 | **5** | 4 | 51.6 | **never (3/3)** | **536 (7.4)** | 1005 / 2639 | 28.7 |
| low-clamp | split-40/60 | **10** | 4 | 29.7 | 8.02 | **918 (6.2)** | 708 / 1663 | 25.9 |
| low-clamp | split-drawn | 4 | 63 | 6.7 | 8.50 | 806 (10.5) | 426 / 896 | 24.5 |

The full table, every variant in every scenario, is in the appendix. Lost
was 0 everywhere, no class ever crossed its hard limit, thrash was 0 and
flapping at most 2 in every run.

### 3.3 What the numbers say

**Two budgets lose, and not because of the ratio.** At the same 44/56 split
as the one arbiter, two budgets:
- evict drapes the frame is wearing under pressure (11 in the memory warning,
  7 at the low clamp; the one arbiter: 0 everywhere). The imagery budget
  protects what *it* thinks is drawn; when the terrain budget draws a tile
  through a held child or a fallback ancestor the imagery budget has not yet
  learnt about, the drape it wears is unprotected. That is the coupling I9 and
  I11 exist for, and it cannot be had across two ledgers without telling each
  what the other draws, every step — which is one arbiter, split in two.
- overshoot the pool at the low clamp (956 steps, up to 10.5 MiB over), because
  each budget relaxes on its own pressure: the terrain budget's `sse_scale`
  rises, the imagery budget's does not follow (§11.4 of the spec relaxes
  imagery *with* terrain for exactly this reason), and the drape it holds for
  a now-coarser cut is still charged;
- converge three times slower when still (0.72 s against 0.22 s) and, at the
  low clamp, late or never;
- cost 1.6 times the decision time on a desktop and 3.6 to 4.6 times at the low
  clamp, since both budgets scan the whole need.

**The ratio matters in both shapes**, which is the honest caveat on "one
pool": the one arbiter is itself a split, by per-class targets. Re-split
60/40 it blurs half the ground under a memory warning (49 %), and converges
faster at the low clamp only because it gives imagery less (30 % blurry).
40/60 is the better split of the three in both shapes. What the one arbiter
never does, at any ratio, is evict a worn drape, fail to converge, or charge
two ledgers for one frame. Whether its per-class targets should become a
pooled target with per-class floors and ceilings, so the split follows the
view, is the open question D-A3; this experiment does not settle it.

**An imagery stream keyed on the terrain's output is refuted.** split-drawn
draws 911 to 1970 bare tile-steps per desktop scenario (the other variants:
7 or 8). A drape requested only once its mesh is drawn arrives one network
latency after it, and in the meantime the mesh is bare. Its low blur under
pressure is not quality: it ignores relaxation, and pays for it in bytes (806
steps over target) and worn evictions. The drape of a tile must be requested
together with its mesh, from the same need.

**Imagery as a payload costs little in bytes, and much in coupling.** Own-drape
level changes with the mesh unchanged: about 86 per still run, 148 per
zoom-out-and-back, nearly none in back-and-forth or under pressure. Under C
each resends the mesh: 2.5 MiB of 116.6 (2 %) still, 4.7 of 301.6 (1.6 %) in
the zoom. The cost is not the bytes. It is that each is a mesh swap as well as
a texture swap (both GPU uploads, both I9 groups), that the level is frozen at
load time so imagery cannot sharpen or coarsen on its own (I10), that a
budget cannot drop a drape without dropping its mesh (§4, "under pressure, a
drape above `need + 1` may be replaced by a coarser one"), and that N layers
multiply the payload.

**The same start-up gap in every architecture.** Bare = 8 (desktop) or 4
(phone) in every variant and every scenario: the first floor meshes are drawn
in the step they arrive, before any drape exists to wear, own or ancestor's.
I1 counts meshes only, so the simulation's per-step check passes. See D-A1.

**Limits of the experiment.**
- The split variants are simulated with the existing engine; a purpose-built
  imagery budget could do better on mirror lag, but not on the coupling: it
  would need the terrain draw set every step, which is the shared arbiter's
  input.
- Convergence in the moving scenarios (zoom, back-and-forth) is reached
  before the camera stops in every variant and does not discriminate.
- `blurry %` is against the unrelaxed need, so it penalises relaxation, which
  the spec wants; read it together with bytes over target.
- `decide` µs is wall clock on one Mac; the ratios between variants are the
  evidence, not the absolute values.
- Not simulated: several imagery layers, stereo, and a per-render-frame
  deadline. §4 treats them on the API.

## 4. Evidence B: the API, host by host

The signatures used below are the recommended ones of §5. For each host: who
calls what, where the budget lives, what crosses a boundary, who owns the
clock, and what does not fit today.

### 4.1 In-process wgpu (desktop, iPhone, Android)

The server task and the render loop share a process; `InProcessStream` is a
channel pair.

```rust
// Render thread, every frame (tuile-wgpu ContentPump).
stream.send(ClientMessage::ViewerState { views: vec![eye], generation: g })?;
while let Some(msg) = stream.try_message() {
    match msg {
        ServerMessage::Content { resource, gen, ancestry, content } =>
            pump.queue(resource, gen, ancestry, content),   // ordered by plan priority
        ServerMessage::Select { draw, generation, .. } => pump.set_draw(draw),
        ServerMessage::Evict { resources } => pump.drop_after_draw(resources),
        ServerMessage::Cancel { resources } => pump.forget_queued(resources),
        _ => {}
    }
}
for (resource, gen, bytes) in pump.upload_within(frame_deadline) {  // I18, render clock
    stream.send(ClientMessage::Ack { resource, gen, bytes })?;
}
pump.draw(&draw);   // each DrawItem: mesh + its drapes, UV transforms

// Server task, per coalesced client step (GeometryServer).
let need = traverse(tree, &budget.residency(), &views, cfg, frame);    // NeedSet
let d = budget.decide(FrameInput::new(frame, now, &need).with_presence(&store_index));
for s in &d.start { loads.start(s) }            // meshes from the TileLoader, drapes from the composer
for (r, g) in &d.cancel { loads.cancel(*r, *g) }
for (r, g) in &d.deliver { tx.send(Content { resource: *r, gen: *g, .. }) }
tx.send(Select { draw: d.draw, generation, .. }); tx.send(Evict { resources: d.evict });
// arrivals between steps: budget.on_loaded / on_failed / on_store_miss
// client acks between steps: budget.on_drawable / on_discarded
// platform: budget.set_pressure / set_thermal / retarget, sent as ClientMessages
```

- Fits. The budget lives in the session, as the spec says (§5).
- The clock: `FrameInput::now` is the server's time at the step; the render
  clock only paces uploads, on the pump's side (I18). The budget never needs
  the render clock, and must not: a worker or a WebSocket puts a latency
  between the two.
- What does not fit today: `Ack { tile }` has no bytes or generation;
  `Content` and `Evict` name tiles; imagery is not a message; the per-frame
  upload count is a profile field (`UploadBudget`, `UPLOADS_PER_FRAME`)
  rather than a deadline in bytes.

### 4.2 The web worker over `postMessage`

The page renders; the worker runs `GeometryServer` (`crates/tuile-viewer/src/web/worker.rs`
98); messages cross as transferred `ArrayBuffer`s (`crates/tuile-web/src/post.rs`,
the codec `crates/tuile-web/src/wire.rs`).

- The same sequence as §4.1, with `send`/`try_message` going through
  `WorkerStream`. The budget lives in the worker; acks cross back.
- Side channels collapse into the stream: `ToWorker::Detail { metres }`
  disappears (the imagery need is per tile, from the views), and
  `ToWorker::Budget { reported }` becomes `ClientMessage::Retarget`. The
  pause gate and the layer switch stay host messages for now (D-A6).
- The wire codec gains `ResourceId`, `Generation`, `DrawSet` and the drape
  content (a texture plus its rect). A drape is transferred like a mesh's
  buffers.
- Upload pacing must be on the page: a decision arrives a message late, so
  only the page knows its frame budget.

### 4.3 The WebSocket binding (`tuile-server`, docs/12)

Not built yet. Docs/12 runs one core `GeometryServer` per WebSocket session
on the server, with "client-side residency tracked via `Ack`/`Evict`".

- Read that way, the budget lives in the server's session, exactly as in the
  worker: the call sequence is §4.2's over a socket, and what crosses is the
  same messages, serialised (glb for meshes; a compressed texture for drapes).
  Acks with bytes are what lets a remote budget know the client's GPU.
- The plan's wording ("budget on the client, server stateless") is a
  different mode, and one that exists already: the **static HTTP mode**,
  where `GeometryServer` runs in-process on the client over an HTTP fetcher
  (protocol.rs, module doc). There the budget is client-side and the server
  is a byte store. No API is needed for it beyond §4.1.
- A stateful WebSocket session cannot see the client's local tiers (an HTTP
  cache or a store on the device): I14 then needs the client to say what it
  holds (`ClientMessage::Held { resources }` at connection, D-A5).
- Pressure, thermal and retarget reach a remote budget as messages, one RTT
  late; the hard limit (`hard_limit`, the jetsam guard) must therefore be
  enforced by the client's executor as well (refuse an upload beyond it, ack
  `Discarded`), never by the remote budget alone.

### 4.4 Hydra / USD (docs/14, docs/15)

`tuile-hydra` drives `tuile_bake::Session` through its C ABI (`ffi.rs` 312,
343); the scene index turns `Select`/`Content`/`Evict` into prim notices
(docs/15), and plans **one baked texture per tile** (docs/15, "Imagery —
bake the mosaic").

```rust
let mut s = Session::globe(cfg)?;                 // BudgetConfig::batch() inside
for frame in frames {
    let f = s.frame(views_for(frame))?;          // steps until Decisions::frame_ready
    for item in f.draw { scene.mesh_prim(item.mesh); scene.material(item.mesh, item.drapes) }
    s.release_beyond(bytes);                      // between frames, deterministic
}
```

- Fits, and argues for imagery as its own content: a drape change dirties a
  material and its texture, not the mesh prim. A mesh prim whose topology
  changes must be dirtied whole (the hdGp lesson: an `EmptyLocator` or the
  topology goes stale), which in an offline renderer means its acceleration
  structure is rebuilt. A drape inside the tile payload turns every imagery
  refinement into that.
- One texture per tile per level is exactly the budget's `Drape` resource.
  With several layers, USD wants them composed; the wgpu path binds them as
  passes. Both are executors of the same draw item (`drapes: Vec<DrapeChoice>`):
  the USD executor composes once every layer of the item is drawable, a swap
  group of N drapes. D-A4.
- The camera ladder of docs/14 (explicit cameras, `primaryCameraPrim`, fixed
  geometric error) feeds `views`; the budget does not care.

### 4.5 Batch: films and bakes (`tuile-bake/src/session.rs`)

```rust
let mut budget = Budget::new(BudgetConfig::batch())?;
loop {
    let d = budget.decide(FrameInput::new(frame, Micros::ZERO, &need));
    if let Some(e) = d.fatal { return Err(e) }   // FailurePolicy::EndSession
    execute(&d);
    if d.frame_ready { render(&d.draw); break }
}
budget.release_beyond(between_frames);
```

- Fits; the simulation's films (`a_300_frame_film_is_exact_every_frame`,
  `a_film_does_not_depend_on_the_network`, `…_on_how_frames_are_split`) are
  this sequence.
- The clock is the frame number; `now` matters only for timeouts
  (`load_timeout` 120 s), which a farm must keep as wall time. That is the one
  place Batch reads real time, and it only ends a session.
- Replaces `ClientMessage::Trim` between frames (`session.rs` 1272-1280),
  `exact_traversal` (1570) and `set_imagery_detail` (1098-1117).

### 4.6 A stereo headset at 72–120 Hz

Two eyes, one budget. The traversal already takes `&[ViewState]` and returns
one union (`traverse` 544, `ClientMessage::ViewerState { views }`).

```rust
stream.send(ClientMessage::ViewerState { views: vec![left, right], generation })?;
// server: one traverse over both views → one NeedSet (union, max need per tile)
//         one decide → one DrawSet; each resource charged once (I16)
// client: both eyes draw the same DrawSet (I17), each culls by its own frustum
for ack in pump.upload_within(FrameDeadline::from_refresh(90.0, share)) { stream.send(ack)? }
```

- The budget fits as it is: stereo is in the traversal, not in the budget. A
  resource is a `ResourceId`, so a tile seen by both eyes is one request, one
  upload, one charge.
- What does not fit: `ViewState::perspective` is symmetric (one `fovy_rad`,
  aspect from `viewport_px`, traversal.rs 30-55). A headset eye has an
  asymmetric frustum (OpenXR's four angles). The texel footprint and the SSE
  must use the eye's own projection. `ViewState::from_fov(position,
  orientation, viewport_px, Fov { left, right, up, down })`.
- The deadline: at 90 Hz a frame is 11.1 ms. The upload cap per frame must
  come from the display rate on the render side (`FrameDeadline`), not from
  a decision taken one message earlier. `upload_bytes_per_frame` stays in
  `BudgetConfig` as the *plan's* cap per step (deterministic, simulated), and
  the executor never exceeds the deadline's share; what it defers it reports
  as a deviation, never feeding the decision (spec §3, "Upload plan").
- `decide` cost: 1.1 ms mean, 1.4 ms p99 desktop still; 2.3 ms mean and
  6.9 ms p99 in the zoom (this Mac, §3.2). It runs on the server task, not
  the render thread, so it does not hold a frame; but on a headset CPU it
  bounds how fresh a decision is. The incremental need of I6
  (O(changes · log N)) becomes a requirement for P6, not a follow-up.

### 4.7 Several imagery layers

Today `ImageryLayer` carries a `source` (`raster.rs` 866), `LayerBudget`
caps the layers one drape binds (`tuile-planetary` lib.rs 329), and a layer
can be switched (`ToWorker::Imagery`). The budget knows one imagery:
`ResourceId::Drape(TileId, u32)`, `Need.imagery: Option<ImageryNeed>`,
`DrawItem.drape: Option<DrapeChoice>`.

- What changes: the drape is keyed by layer, `ResourceId::Drape { tile,
  layer, level }`; the need carries the reach of each layer
  (`ImageryNeed { footprint, layers: Vec<LayerReach> }`, the footprint being
  geometric and common to all); a draw item wears one drape per layer.
- Each layer picks its level independently (an overlay shallower than the
  base imagery), falls back independently, and is refreshed by its own epoch
  (`new_epoch(Class::Drape)` becomes per layer).
- The one arbiter is what keeps this tractable: one ledger, so N layers do not
  need N splits of the pool, and a layer switch (old and new both on screen
  until the new one is drawn, as `ImageryLayer::source` documents) is a swap
  group across layers inside one decision.

### 4.8 wasm32

- `tuile_core::budget` builds for `wasm32-unknown-unknown` (checked on this
  rebase), reads no clock, spawns nothing, iterates no hash map.
- The worker (§4.2) and a page-only static mode run the same `decide`.
  `PresenceOracle` is `&dyn` and synchronous; a browser store answers from an
  in-memory index (an IndexedDB read is asynchronous and is not a presence
  query, as `TileStore::held` is not on the server side).

### 4.9 Where the need comes from

In every host the traversal produces the `NeedSet` and the budget consumes
it: push, once per step. The alternative, the budget pulling need per
resource, would put the tree inside the budget and make the stereo union, the
SSE and the 3D Tiles REPLACE/ADD rules its problem. The data supports push:
the only variant whose imagery need did not come from the traversal (B′) is
the one that left ground bare.

## 5. Recommendation and the API it implies

**Decision: one arbiter (A), imagery as its own resources and messages in the
same session.** Two budgets are refuted by the simulation (worn drapes
evicted, overshoot, non-convergence, double cost); an imagery stream keyed on
the terrain's output is refuted (bare ground); a payload cannot meet I10. The
plan's leaning stands, with one refinement: "two streams" means two families
of content and two producers (the tile loader, the drape composer) behind one
`GeometryStream`, one traversal and one `decide`; not a second
`GeometryStream` with a budget of its own.

**Budget (`tuile_core::budget`)**, small changes:

```rust
pub struct LayerId(pub u16);
pub enum ResourceId {
    Mesh(TileId),
    Drape { tile: TileId, layer: LayerId, level: u32 },
}
pub struct LayerReach { pub layer: LayerId, pub max_level: u32 }
pub struct ImageryNeed { pub footprint: TexelFootprint, pub layers: Vec<LayerReach> }
pub struct DrawItem {
    pub mesh: ResourceId,
    pub role: Role,
    pub drapes: Vec<DrapeChoice>,      // one per layer, in stacking order (was drape: Option<_>)
    pub serves: Vec<TileId>,
}
impl Budget {
    pub fn new_epoch(&mut self, class: Class, layer: Option<LayerId>);   // per layer
    // decide, on_loaded, on_failed, on_store_miss, on_drawable, on_discarded,
    // set_pressure, set_thermal, retarget, release_beyond, residency: unchanged
}
```

- D-A1 (bare ground at start-up), if accepted: a mesh whose need has
  imagery is drawable only with a drape to wear, its own or an ancestor's;
  the floor's mesh and drape form one swap group; I1 becomes "every drawn
  item wears a drape for each layer it needs". A test first: bare = 0 in
  every scenario.

**Protocol (`tuile_core::protocol`)**:

```rust
pub enum ClientMessage {
    ViewerState { views: Vec<ViewState>, generation: u64 },
    Ack { resource: ResourceId, gen: Generation, bytes: u64 },
    Discarded { resource: ResourceId, gen: Generation },
    Pressure(Pressure),
    Thermal(Thermal),
    Retarget { total_bytes: u64 },
    ReleaseBeyond { bytes: u64 },                 // Batch, between frames (replaces Trim)
    Held { resources: Vec<ResourceId> },          // a remote budget learns the client's tiers (D-A5)
}
pub enum ServerMessage {
    Select { draw: DrawSet, ancestry: Vec<(TileId, Ancestry)>, stats: TraversalStats, generation: u64 },
    Content { resource: ResourceId, gen: Generation, ancestry: Ancestry, content: ResourceContent },
    Cancel { resources: Vec<(ResourceId, Generation)> },   // delivered, not yet uploaded: drop it
    Evict { resources: Vec<ResourceId> },                   // after the draw that stops using them
    Error { resource: Option<ResourceId>, message: String },
    Priming(Priming),
}
pub enum ResourceContent {
    Mesh(TileContent),                 // DecodedTileContent without `imagery`
    Drape(DrapeContent),               // one texture, its rect, its layer and level
}
```

- `ClientMessage::Cancel { tile }` and `Trim` go: cancellation is the
  budget's, from the need; a release between frames is `ReleaseBeyond`.
- `Fill`/`Retire` (stand-ins) stay as they are until decision D3 of the spec.
- `DecodedTileContent::imagery` and `withheld_drape` leave the mesh content;
  the drape identity of `raster::drape_identity` keys `DrapeContent`.

**Views**: `ViewState::from_fov(position, orientation, viewport_px, Fov)` for
asymmetric eyes, beside `perspective`.

**Executor (tuile-wgpu, the web page, the USD scene index)**: an upload queue
in the plan's priority order, drained per render frame within
`FrameDeadline { bytes }` derived from the display rate, never beyond the
class hard limits; deviations reported, never fed back into decisions.

**Where the budget lives**: in the `GeometryServer` session, whatever side of
the transport it runs on. Its inputs are messages only, so it runs the same
in-process, in a worker and in a WebSocket session; the static HTTP mode puts
it on the client.

## 6. Decisions for Laurent

| | Question | Recommended |
|---|---|---|
| **D-A0** | One arbiter (A) or two budgets (B)? | **A**, on §3 |
| **D-A1** | Bare floor meshes at start-up (4–8 tile-steps in every scenario, every architecture): make "every drawn item wears a drape" part of I1, the floor mesh and drape one swap group? | Yes, test first |
| **D-A2** | Imagery as its own `Content` in the same session (A), not a payload (C), not a second stream with its own budget (B′)? | Yes |
| **D-A3** | Keep fixed per-class targets, or pool the ledger with per-class floors and ceilings so the split follows the view? The ratio moves the low-clamp convergence 2.0–3.3 s and the blur under a warning 19–49 %. | Open: simulate a pooled ledger in Phase 3 before deciding; until then, 40/60 for the default split looks better than 44/56 under pressure |
| **D-A4** | Several layers: per-layer drapes (`Drape { tile, layer, level }`, composed by the executor; USD composes once all are drawable), or one composed drape per tile keyed by the layer set? | Per layer |
| **D-A5** | The WebSocket binding: budget in the server session (docs/12) with `Ack { bytes }` and `Held`, and "budget client-side, server stateless" understood as the static HTTP mode? | Yes; correct the plan's wording |
| **D-A6** | Host side channels: `ToWorker::Detail` and `Budget` fold into the stream (`ViewerState`, `Retarget`); `Pause` and the layer switch stay host messages for now? | Yes |
| **D-A7** | Headset eyes: `ViewState::from_fov` with an asymmetric frustum, and the incremental need (I6) promoted from follow-up to P6 requirement? | Yes |
| **D-A8** | Per-frame deadline: the executor's `FrameDeadline` from the display rate, with `upload_bytes_per_frame` kept as the plan's per-step cap? | Yes |

## Appendix: the full table

From `cargo test --release -p tuile-core --test budget_sim -- --ignored architecture --nocapture`
on `feat/budget-core` `b9eda88`, rebased on `main` 65b04cb, release build on
the Mac, seeds 101, 202, 303. Counts are summed over the seeds; MiB and µs
are means, except over-target MiB (max) and converged s (max).

| scenario | arch | lost | bare | worn evicted | blurry % | converged s (max of seeds) | over-target steps | over-target max MiB | over-hard steps | flapping | decide mean µs | decide p99 µs | net MiB | thrash | redrapes | redrape mesh MiB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| still | shared | 0 | 8 | 0 | 3.0 | 0.22 | 0 | 0.0 | 0 | 1 | 1085 | 1407 | 116.6 | 0 | 257 | 2.5 |
| still | shared-60/40 | 0 | 8 | 0 | 3.0 | 0.22 | 0 | 0.0 | 0 | 1 | 1084 | 1646 | 116.6 | 0 | 257 | 2.5 |
| still | shared-40/60 | 0 | 8 | 0 | 3.0 | 0.22 | 0 | 0.0 | 0 | 1 | 1075 | 1398 | 116.6 | 0 | 257 | 2.5 |
| still | split-44/56 | 0 | 7 | 0 | 4.9 | 0.72 | 0 | 0.0 | 0 | 1 | 1704 | 2216 | 117.1 | 0 | 217 | 2.1 |
| still | split-60/40 | 0 | 7 | 0 | 4.8 | 0.70 | 0 | 0.0 | 0 | 1 | 1715 | 2581 | 117.2 | 0 | 215 | 2.1 |
| still | split-40/60 | 0 | 7 | 0 | 4.0 | 0.72 | 0 | 0.0 | 0 | 1 | 1715 | 2663 | 117.1 | 0 | 234 | 2.3 |
| still | split-drawn-44/56 | 0 | 1290 | 0 | 8.8 | 0.70 | 0 | 0.0 | 0 | 1 | 1544 | 2183 | 116.7 | 0 | 30 | 0.3 |
| turn | shared | 0 | 8 | 0 | 2.7 | 1.55 | 0 | 0.0 | 0 | 1 | 1057 | 1523 | 152.0 | 0 | 272 | 2.7 |
| turn | shared-60/40 | 0 | 8 | 0 | 2.8 | 1.53 | 0 | 0.0 | 0 | 1 | 1074 | 1777 | 152.5 | 0 | 261 | 2.6 |
| turn | shared-40/60 | 0 | 8 | 0 | 2.6 | 1.55 | 0 | 0.0 | 0 | 1 | 1065 | 1583 | 152.1 | 0 | 269 | 2.6 |
| turn | split-44/56 | 0 | 7 | 0 | 3.7 | 1.62 | 0 | 0.0 | 0 | 1 | 1657 | 2442 | 153.1 | 0 | 249 | 2.4 |
| turn | split-60/40 | 0 | 7 | 0 | 6.1 | 1.62 | 0 | 0.0 | 0 | 1 | 1681 | 2900 | 152.9 | 0 | 225 | 2.2 |
| turn | split-40/60 | 0 | 7 | 0 | 3.3 | 1.62 | 0 | 0.0 | 0 | 1 | 1646 | 2432 | 153.1 | 0 | 269 | 2.6 |
| turn | split-drawn-44/56 | 0 | 1406 | 0 | 4.3 | 1.73 | 0 | 0.0 | 0 | 1 | 1475 | 2810 | 152.0 | 0 | 99 | 1.0 |
| zoom-out-back | shared | 0 | 8 | 0 | 5.2 | 0.00 | 0 | 0.0 | 0 | 0 | 2250 | 6887 | 301.6 | 0 | 445 | 4.7 |
| zoom-out-back | shared-60/40 | 0 | 8 | 0 | 4.3 | 0.05 | 6 | 7.8 | 0 | 0 | 2565 | 6638 | 351.7 | 0 | 416 | 4.7 |
| zoom-out-back | shared-40/60 | 0 | 8 | 0 | 5.7 | 0.00 | 0 | 0.0 | 0 | 0 | 2102 | 6735 | 287.6 | 0 | 505 | 5.8 |
| zoom-out-back | split-44/56 | 0 | 7 | 0 | 2.8 | 0.05 | 0 | 0.0 | 0 | 0 | 3410 | 9951 | 307.9 | 0 | 590 | 6.0 |
| zoom-out-back | split-60/40 | 0 | 7 | 0 | 4.1 | 0.00 | 9 | 9.9 | 0 | 0 | 4034 | 10846 | 360.7 | 0 | 593 | 6.9 |
| zoom-out-back | split-40/60 | 0 | 7 | 0 | 2.7 | 0.00 | 0 | 0.0 | 0 | 0 | 3166 | 9509 | 292.2 | 0 | 626 | 6.5 |
| zoom-out-back | split-drawn-44/56 | 0 | 1970 | 0 | 3.2 | 0.05 | 0 | 0.0 | 0 | 0 | 2870 | 7489 | 299.6 | 0 | 199 | 2.0 |
| back-and-forth | shared | 0 | 8 | 0 | 1.2 | 0.00 | 0 | 0.0 | 0 | 1 | 1227 | 1649 | 304.9 | 0 | 2 | 0.0 |
| back-and-forth | shared-60/40 | 0 | 8 | 0 | 1.3 | 0.00 | 0 | 0.0 | 0 | 1 | 1213 | 1604 | 301.5 | 0 | 2 | 0.0 |
| back-and-forth | shared-40/60 | 0 | 8 | 0 | 1.2 | 0.00 | 0 | 0.0 | 0 | 1 | 1225 | 1618 | 304.9 | 0 | 2 | 0.0 |
| back-and-forth | split-44/56 | 0 | 7 | 0 | 2.0 | 0.00 | 0 | 0.0 | 0 | 1 | 1919 | 2588 | 297.9 | 0 | 2 | 0.0 |
| back-and-forth | split-60/40 | 0 | 7 | 0 | 2.2 | 0.00 | 0 | 0.0 | 0 | 1 | 1924 | 2541 | 301.7 | 0 | 2 | 0.0 |
| back-and-forth | split-40/60 | 0 | 7 | 0 | 1.9 | 0.00 | 0 | 0.0 | 0 | 1 | 1925 | 2570 | 301.6 | 0 | 2 | 0.0 |
| back-and-forth | split-drawn-44/56 | 0 | 911 | 0 | 0.8 | 0.00 | 0 | 0.0 | 0 | 1 | 1643 | 2444 | 301.6 | 0 | 2 | 0.0 |
| memory-warning | shared | 0 | 4 | 0 | 24.7 | 0.00 | 0 | 0.0 | 0 | 0 | 263 | 614 | 51.9 | 0 | 9 | 0.1 |
| memory-warning | shared-60/40 | 0 | 4 | 0 | 49.2 | 0.00 | 0 | 0.0 | 0 | 0 | 220 | 474 | 44.6 | 0 | 5 | 0.0 |
| memory-warning | shared-40/60 | 0 | 4 | 0 | 18.6 | 0.00 | 0 | 0.0 | 0 | 0 | 262 | 621 | 51.5 | 0 | 12 | 0.2 |
| memory-warning | split-44/56 | 0 | 4 | 11 | 34.6 | 0.07 | 0 | 0.0 | 0 | 1 | 649 | 1839 | 60.4 | 0 | 12 | 0.2 |
| memory-warning | split-60/40 | 0 | 4 | 4 | 61.6 | 2.75 | 0 | 0.0 | 0 | 1 | 861 | 2606 | 65.2 | 0 | 4 | 0.0 |
| memory-warning | split-40/60 | 0 | 4 | 8 | 26.9 | 0.22 | 0 | 0.0 | 0 | 1 | 616 | 1691 | 59.0 | 0 | 11 | 0.2 |
| memory-warning | split-drawn-44/56 | 0 | 227 | 3 | 11.4 | 0.57 | 0 | 0.0 | 0 | 2 | 347 | 875 | 63.7 | 0 | 21 | 0.3 |
| low-clamp | shared | 0 | 4 | 0 | 10.5 | 3.33 | 0 | 0.0 | 0 | 2 | 218 | 374 | 18.8 | 0 | 0 | 0.0 |
| low-clamp | shared-60/40 | 0 | 4 | 0 | 30.2 | 2.02 | 0 | 0.0 | 0 | 1 | 173 | 272 | 16.0 | 0 | 0 | 0.0 |
| low-clamp | shared-40/60 | 0 | 4 | 0 | 8.7 | 3.22 | 434 | 0.4 | 0 | 1 | 220 | 385 | 19.3 | 0 | 0 | 0.0 |
| low-clamp | split-44/56 | 0 | 4 | 7 | 35.4 | never (1/3) | 956 | 10.5 | 0 | 2 | 780 | 1764 | 26.7 | 0 | 1 | 0.0 |
| low-clamp | split-60/40 | 0 | 4 | 5 | 51.6 | never (3/3) | 536 | 7.4 | 0 | 2 | 1005 | 2639 | 28.7 | 0 | 1 | 0.0 |
| low-clamp | split-40/60 | 0 | 4 | 10 | 29.7 | 8.02 | 918 | 6.2 | 0 | 2 | 708 | 1663 | 25.9 | 0 | 1 | 0.0 |
| low-clamp | split-drawn-44/56 | 0 | 63 | 4 | 6.7 | 8.50 | 806 | 10.5 | 0 | 1 | 426 | 896 | 24.5 | 0 | 0 | 0.0 |
