<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Procedural terrain with unbounded detail — an exploration

Terrain, and the imagery on it, computed from a formula and a seed instead of
downloaded: sharp at any zoom, a few hundred bytes of parameters instead of
gigabytes of tiles, and the same on every machine that runs the formula. Games
want it for invented worlds. Simulators want it to put believable detail
*under* real data that runs out at 30 m or 1 m. This note surveys how the
industry does it and proposes how tuile could. **Nothing here is a decision.**
The figures marked *(est.)* are orders of magnitude, not measurements.

## 1. The building blocks

### 1.1 Noise and fractal sums

| Block | What it is | Notes for tuile |
|---|---|---|
| Value noise | Random values on a lattice, smoothly interpolated | The cheapest kind. Its blocky look shows in the derivatives |
| Gradient (Perlin) noise | Random gradients on a lattice, quintic fade ([Perlin 2002]) | The workhorse. Analytic derivatives are cheap ([IQ derivatives]) |
| Simplex / OpenSimplex | Simplex lattice, fewer corners in 3D/4D ([Gustavson]) | Worth it in 3D (sphere evaluation). In 2D, gradient noise is enough |
| fBm | Σ octaves, frequency × lacunarity (≈2) and amplitude × gain (≈0.5) per octave ([IQ fBm]) | Each octave maps onto one quadtree level (§3.3) |
| Ridged / billow multifractals | `1-\|n\|` squared, or `\|n\|`, with each octave's weight driven by the previous one ([Musgrave], in [Ebert et al.]) | Gives sharp crests and smooth valleys: real mountain statistics |
| Derivative-damped fBm | Each octave is attenuated by the slope accumulated so far ([IQ derivatives]) | An erosion-like look at almost no cost |
| Domain warping | `f(p + g(p))`, with g itself an fBm ([IQ warp]) | Breaks up the lattice's isotropy. It roughly doubles the cost |
| Cellular / Worley | Distance to random feature points | Craters, plateaus, terraces, rock fields |

**Band-limiting.** An octave whose wavelength is shorter than about twice the
sample spacing aliases: it adds sparkle, not detail. Every engine drops it,
either by stopping the sum (a hard cut, which pops) or by fading the last
octave with a weight that goes from 1 to 0 as its wavelength nears the
Nyquist limit (a smooth cut). The same applies to imagery pixels and to
shading. Shader noise sums often stop where `fwidth(p)` says they should.

### 1.2 Determinism

Same seed, same terrain, on every client and on the server. That is the
requirement for multiplayer and for any cache.

- **Hashing.** Hash integer lattice coordinates and the seed with integer
  arithmetic, which is exact everywhere, WGSL included: PCG / xxhash-style
  ([Jarzynski & Olano]) or the "noise-based RNG" idea ([Eiserloh]). Avoid
  permutation tables tied to an RNG implementation.
- **Floats.** IEEE `+ − × ÷ sqrt` are correctly rounded, so on x86-64,
  aarch64 and wasm32 a Rust f64 expression gives bit-identical results
  ([Wasm numerics], [Rust float semantics]). Rust never contracts to FMA on
  its own. What breaks determinism: platform `sin/cos/exp/pow` (different
  libms), `mul_add` used on some paths and not others, fast-math flags, and
  **every GPU**: WGSL has no f64 and its f32 is not correctly rounded
  ([WGSL accuracy]). See also [Gaffer determinism].
- **Consequence.** Authoritative heights (gameplay, collision, the served
  tile) come from a CPU f64 path with no transcendental function, or with
  the pure-Rust [`libm`] crate on every target. GPU-evaluated noise is for
  display only (detail below the gameplay resolution, normals, splatting).
- **Fixed point as an alternative.** Lattice cell indices are integers and
  the position inside a cell is a dyadic fraction. That is exactly what the
  quadtree vertex grid already provides (§3.2).

### 1.3 Spherical / geodetic evaluation

| Domain | Pro | Con |
|---|---|---|
| Lon/lat 2D lattice | Aligned with the geographic quadtree, so the lattice can be the vertex grid itself. Exact, cheap | Stretches toward the poles. Wraps at ±180° only if the longitude period is a whole number of lattice cells |
| 3D noise on the unit sphere / ECEF (NMS, Elite, Space Engine) | Isotropic, no poles, no wrap | Needs `sin/cos` (use the pure-Rust libm), and the lattice no longer coincides with the tiles. 3D costs about 2× more than 2D |
| Cube-sphere faces, 2D per face (Outerra, KSP PQS) | Near-uniform | 12 face seams to stitch, and they do not match tuile's EPSG:4326 tiling |

## 2. The industry

### 2.1 Planet-scale engines

| Engine | Source of the ground | Generation | LOD / streaming | What it still downloads |
|---|---|---|---|---|
| **Outerra** | Real DEM (≈90–150 m, 1 km over oceans) refined by fractal detail down to centimetres. Land class drives the fractal | GPU, per tile. Textures per pixel from elevation, slope and curvature | Cube-sphere quadtree. Wavelet-compressed DEM streamed on demand ([Outerra news], [Outerra simflight]) | The coarse DEM and land class only |
| **Flight Simulator 2020/2024** | Real DEM + Bing/Azure imagery + photogrammetry + ML-classified buildings and vegetation | Hybrid: real data where it exists, procedural for vegetation, buildings and detail. 2024 adds displacement from aerial texture | Streamed from the cloud. Terrain LOD tied to photogrammetry | Almost everything (petabytes server-side) ([Asobo GDC], [MSFS displacement]) |
| **No Man's Sky** | Pure seed (64-bit per universe) | CPU, voxel density, then polygonization, texturing and population ([NMS GDC]) | Chunks around the player | Nothing for terrain. Player edits sync as deltas |
| **Elite Dangerous** (Stellar Forge) | The seed + a physical simulation of the star system decide parameters per planet | Parameters packed into a GPU buffer. Surface patches computed on the GPU and subdivided as the camera approaches ([Elite 80.lv]) | Patch quadtree on the sphere | Nothing. Heightfield only, so no overhangs |
| **Star Citizen** (Planet Tech v4) | Authored height maps + temperature/humidity climate maps, which select biomes | Biome blending, object scattering ([SC PT4], [SC Terra Firmer]) | Streamed planets | Authored base maps and assets |
| **Space Engine** | Catalogue data where it exists, procedural elsewhere | GPU shaders per patch ([Space Engine blog]) | Quadtree on a cube-sphere | Catalogues, and real DEMs for known bodies |
| **KSP 2** (PQS+) | Authored planets + procedural modifiers | Compute shaders. Vertices never return to the CPU, and they are generated relative to the floating origin ([KSP2 #12]) | Procedural quad-sphere | Base maps. Terrain was its most expensive GPU pass ([KSP2 #18]) |
| **Infinity: Battlescape** (I-Novae) | Pure procedural, true-scale system | Procedural surfaces, seamless from space to ground ([IB 80.lv]) | Quadtree | Nothing for terrain |
| **Proland** (research, INRIA) | DEM or procedural residuals | Each level = the parent upsampled + residuals. Random residuals with shrinking amplitude give a fractal ([Proland]) | GPU producers per quadtree tile | Residual tiles, or nothing |

LOD schemes behind them: chunked LOD ([Ulrich]), geometry clipmaps
([Losasso & Hoppe]) and CDLOD with per-vertex morphing ([Strugar CDLOD]).
All of them refine where the screen-space error asks, which is the job tuile's
traversal already does.

### 2.2 What they share

1. **The coarse levels are faithful, and detail is added on top.** Outerra
   and Proland are explicit about it: each level is the parent plus a residual.
   A procedural world is the special case where the root is flat.
2. **Generation is local to the consumer.** The seed and parameters travel.
   Geometry is made where it is drawn, mostly on the GPU, and kept there.
3. **Gameplay does not trust the GPU.** Pure-seed multiplayer games
   (NMS, Elite) keep a CPU path for the heights that matter, or accept small
   divergences in cosmetic detail.
4. **Edits are deltas.** Terraforming and player edits are stored and synced
   as differences on top of the seed.

### 2.3 Erosion and realism

| Approach | Cost | Runtime? |
|---|---|---|
| Hydraulic/thermal simulation on a grid ([Mei et al.], [Jákó & Tóth]), [Gaea], [World Machine], [Houdini heightfields] | Seconds to minutes per km². It is a global pass with no per-point evaluation | **No** (it needs neighbours over a growing radius). Bake offline, then serve as a host grid (§3.5) |
| Derivative-damped fBm ([IQ derivatives]) | ≈ fBm + 10 % | Yes |
| Point-evaluable "erosion filter": gullies as stripes aligned to the gradient, per cell, over octaves ([runevision erosion]) | A few × fBm *(est.)*. Each point is independent | Yes. Chunk- and GPU-friendly. Drainage lines are not physically continuous |
| ML terrain (below) | Heavy | Rarely |

### 2.4 Texturing

- **Splatting rules**: materials weighted by altitude, slope, curvature and a
  moisture/biome noise (Outerra per pixel, Star Citizen's climate maps).
  **Triplanar** projection on steep slopes, **detail textures** that tile at
  close range on top of a coarse colour, biome blending with noise-perturbed
  boundaries.
- **Imagery tiles vs shading.** Generating imagery tiles (pixels baked per
  tile) works with every existing pipeline (caches, compression, film and
  USD export), but its resolution is bounded by the deepest level made.
  Shading in the ground shader has unbounded resolution and no texture
  memory, but every backend has to implement it, and it is invisible to
  anything that only reads tiles.

### 2.5 Machine learning

DEM super-resolution with CNNs/GANs ([FEEDBACK DEM SR], [AFN]) and diffusion
([DEM-SDE], [Guided DEM SR]) learns realistic detail, often ×2–×4 per pass.
PLAYERUNKNOWN Productions describes multistep learned upsampling to planetary
scale ([PUP upsampling]). The runtime cost is high (a GPU inference per tile).
The output is not bit-reproducible across GPUs, drivers or runtimes, and the
models and training data carry licences. Usable to **bake** a host grid
offline. Not for client-side, deterministic, wasm generation.

### 2.6 Formats

No standard describes procedural terrain. 3D Tiles 1.1 implicit tiling
describes availability, not content ([3D Tiles 1.1]). glTF has no procedural
geometry extension ([glTF extensions]). quantized-mesh carries data (normals,
water mask, metadata extensions), not generators ([quantized-mesh]). Engines
ship their own parameter blobs. So the open interop path is still **tiles**,
and a descriptor would be a tuile extension that falls back to tiles.

## 3. Mapped onto tuile

### 3.1 A separate capability: `tuile-procedural`

Procedural unbounded-detail terrain is **its own capability**, not a mode of
an existing source. It is a new crate next to `tuile-terrain`, wasm-clean and
with no tokio, like the core. `HeightfieldSource` and
`examples/heightfield-serve` are another subject and stay as they are. The new
crate reuses only tuile-terrain's shared vocabulary: the `TerrainSource`
trait, `TileCoord` / `GeographicTilingScheme`, the quantized-mesh encoder
(`Header`, edges, oct normals), `LayerJson`, and the `Heights` trait so that
a `Grid` can serve as a host.

```rust
// tuile-procedural (sketch, names not settled)
pub struct Generator { version: GeneratorVersion, seed: u64, params: Params }
impl Generator {
    /// Authoritative height, CPU f64, bit-exact on every target.
    pub fn height(&self, lon_deg: f64, lat_deg: f64, level: u32) -> f64;
    /// Same, with analytic slope (normals without neighbours).
    pub fn height_and_gradient(&self, lon_deg: f64, lat_deg: f64, level: u32) -> (f64, DVec2);
}

/// A coarse host the detail is added over; any `Heights` (a `Grid`, a DEM).
pub struct Detailed<H: Heights> { host: H, generator: Generator, masks: Masks }

/// The terrain source: tiles generated on request, unbounded `max_level`.
pub struct ProceduralTerrainSource<H = Flat> { /* Detailed<H> + vertices per side */ }
impl<H: Heights> TerrainSource for ProceduralTerrainSource<H> { /* generate, encode */ }
impl<H: Heights> ProceduralTerrainSource<H> {
    pub fn layer(&self) -> LayerJson;              // availability down to max_level
    pub fn descriptor(&self) -> ProceduralDescriptor; // seed + params + host URL
    pub fn ground_height(&self, lon_deg: f64, lat_deg: f64) -> f64; // physics level
}

/// Procedural imagery, same contract as the other imagery providers.
pub struct ProceduralImagery { /* Generator + colour rules */ }
```

**How a host uses it.**

- *A game client* (native or wasm) builds a `ProceduralTerrainSource` from a
  `ProceduralDescriptor` (generator version, seed, parameters, optional host
  grid URL, physics level) and hands it to the viewer as its terrain source,
  exactly where an `HttpTerrainSource` would go. Tiles are generated in the
  decode workers, so nothing is downloaded but the descriptor and the host
  grid. Gameplay asks `ground_height` and gets the same bits on server and
  clients.
- *A game server* links the same crate for authoritative heights, and never
  serves geometry.
- *A third-party quantized-mesh client*, if one is ever needed, gets tiles
  from any HTTP front that mounts the same `TerrainSource`. That is a later,
  optional example of the new crate. It is not a change to
  `heightfield-serve`.

**What it inherits from the existing design.**

- **Shared edges are shared samples**: as in tuile-terrain, a vertex position
  depends only on the level and the global vertex index, so tiles are
  seam-free by construction.
- The `Heights::mean_height(…, span)` idea, a value band-limited to what a
  vertex stands for, becomes the generator's `level` argument (§3.3).
- `Ellipsoid` fallback, the graphics budget, the imagery pipeline (and its
  GPU compression, when the client turns it on): unchanged, the new sources
  plug into them.

### 3.2 Deterministic noise design

- **Lattice on the quadtree grid.** Octave *k* uses as its lattice the
  global vertex grid of some level *L(k)*. Cell indices are integers (u64)
  and the position inside a cell is a dyadic fraction `i / 2^m`, exact in
  f64. Hashing is integer (`seed`, `k`, `ix`, `iy` → u32). Gradients come
  from that hash. Fade and interpolation use only `+ − ×`. Result: the same
  bits on native, wasm and server, with no libm.
- **Longitude wrap.** Each octave's lattice has a whole number of cells
  around 360°, so ±180° is seamless.
- **Poles.** In v1, the lon/lat lattice is weighted by `cos φ` in the
  longitude amplitude and the octave count is capped near the poles (a known,
  stated limitation). In v2, an optional 3D (simplex) noise on the ellipsoid
  normal, with pure-Rust `libm` on every target, for worlds where the poles
  are played.
- **Crate.** Per the "established crates for numerics" rule, start from an
  established noise crate ([`noise`] / fastnoise-lite ports). Adopt it only
  if its golden values match bit for bit on native and wasm32 and it accepts
  integer-lattice input. Otherwise fork it (lapoule-dev, pinned rev) for the
  integer-lattice adaptor. No porting of Shadertoy code (its default licence
  is NC), and the MPL erosion code is inspiration only.
- **Versioned generator.** `generator: "tuile-fbm/1"`. Any change to the
  output bits bumps the version. Golden-value tests pin it.

### 3.3 Octave per level, and continuity

- Level *L* has vertex spacing `Δ_L = 180° / 2^L / (n−1)` (n vertices per
  side). The octaves with wavelength `λ ≥ 2 Δ_L` are summed in full. The one
  straddling the limit is weighted by `smoothstep(2Δ_L, 4Δ_L, λ)`. Finer
  octaves are dropped. This is what `mean_height(span)` returns for a vertex
  of level L. The parent is then the band-limited child: refining adds
  detail, never moves the ground already shown.
- **Popping.** The weight of the cut octave jumps between L and L+1 at shared
  vertices. A client-side vertex morph (CDLOD) or a short cross-fade during
  the swap hides it. Ordering per the black-ground rule: the child is
  resident and active before the parent leaves.
- **"Unbounded".** `max_level` becomes a declaration, not a storage limit.
  At 65 vertices per side, L = 20 gives ≈ 0.3 m between vertices and L = 24
  gives ≈ 2 cm. Beyond the geometry, shading takes over (§3.6).

### 3.4 Where generation runs

| Where | For | Against | Verdict |
|---|---|---|---|
| Server (an HTTP front mounting `ProceduralTerrainSource`, with a cache) | Any quantized-mesh client works, including third-party ones | Downloads come back. Server CPU | **Optional interop**, later |
| Client decode workers, CPU f64 | Zero download. Bit-exact. Same code as the game server. wasm OK. Fits the existing `TerrainSource` path | ~1 ms of CPU per tile *(est.)* | **Primary path** |
| GPU compute (wgpu) | ~100× throughput. KSP2/Elite do it | f32, not bit-exact. Only for backends with compute. Heights are needed on the CPU for collision and picking | **Display-only detail** beyond the gameplay level, later |

### 3.5 "Detail over a host grid"

`Detailed<H: Heights>`: `height = host_smooth(p) + a(p) · Σ_{octaves above the host's Nyquist} n_k(p)`.

- `host_smooth`: a **bicubic** (Catmull-Rom or B-spline) reading of the host,
  not bilinear. Bilinear leaves creases that fractal detail amplifies (Proland
  makes the same remark about upsampling filters).
- **Faithful coarse levels**: the detail is high-pass, so its mean over a
  host cell is ≈ 0. The test asserts `|mean over a host cell − host| < ε`.
  Below the host's level, `mean_height` returns the host.
- `a(p)`: amplitude driven by the host itself (local roughness and slope:
  steep and rough ground gets more detail, flat ground less) and by masks
  (water = 0, runways and roads = 0, land class). The masks are the
  "procedural detail on satellite elevation" knob that Outerra exposes.
- The host can be a DEM, an offline-eroded grid (Gaea/World Machine/ML
  bake), or a game's coarse authored grid. One grid of a few MB plus a seed
  replaces the whole pyramid.

### 3.6 Procedural imagery

1. **A procedural imagery provider** (same interface as `TemplateProvider`):
   a 256² tile per (level, x, y), coloured from height, slope, curvature,
   altitude bands and a moisture noise. It is generated in the worker and
   then goes through the existing budget, and through GPU compression when the
   client has chosen it. It works
   unchanged for the film, USD and Hydra. Over real imagery, the same rules
   only modulate the satellite colour (detail variation), never replace it.
2. **Later, close-range shading** in the ground shader: a small texture array
   of detail materials (triplanar on steep slopes) blended in by distance
   beyond the deepest imagery level. It is optional per backend and never
   load-bearing for coverage.

### 3.7 Costs per tile *(est.)*

| Item | Downloaded today | Procedural |
|---|---|---|
| 65×65 terrain tile | 10–20 KB gzip | 0 B. ≈4.2 k vertices × 8–12 octaves of 2D gradient noise ≈ 0.5–1.5 ms single core, f64. ×2–4 with warp/erosion |
| 256² imagery tile | 15–40 KB | 0 B. 65 k px × ~10 octaves ≈ 10–40 ms CPU, < 0.2 ms GPU compute |
| A world | GB–TB of pyramid | ~200 B of parameters, plus a host grid if any (MB) |

Imagery is the dominant cost on the CPU. That argues for generating it at
mid-resolution (heights interpolated from the mesh tile, noise only for
colour variation), then GPU compute where it is available.

### 3.8 Gameplay height vs displayed detail

`Heights` at a declared **physics level** (e.g. L = 18, ≈1.2 m) is the
authority: CPU f64, bit-exact, the same on the server. Anything finer
(GPU detail, normal maps, displacement) is cosmetic and bounded in amplitude
(e.g. ≤ 10 cm), so objects never visibly float or sink. Precision stays f64
until rebasing (rule 7). GPU-side noise takes integer cell offsets computed
per tile on the CPU plus an f32 local fraction, never absolute f32
coordinates.

## 4. Incremental PR plan

Each PR comes with its test written first and seen to fail. Each runs
`cargo check --target wasm32-unknown-unknown`.

1. **New crate `tuile-procedural`, noise kernel**: integer hash, 2D gradient
   noise on an integer lattice, fBm/ridged/billow, analytic derivatives,
   versioned `Generator`. Golden values checked bit for bit on native and on
   wasm32 (wasm-bindgen-test or a golden file).
2. **`ProceduralTerrainSource`** (flat host): a `TerrainSource` with an
   unbounded `max_level`, band-limited per level, `layer()`, `ground_height()`.
   Tests: no seam between neighbours, parent equals band-limited child,
   longitude wrap, and identical tile bytes across two runs and targets.
3. **Host integration**: `ProceduralDescriptor` (JSON: generator version, seed,
   parameters, host URL, physics level). The viewer (native and web worker)
   accepts a procedural terrain source in place of an HTTP one. No change to
   `heightfield-serve` or `HeightfieldSource`.
4. **`Detailed<Host>`**: bicubic host + high-pass + masks. Faithfulness test
   on a real DEM fixture.
5. **`ProceduralImagery`** in the same crate (CPU worker), plugged into the
   budget and, when the client turns it on, compression. Render it and open the film.
6. **Erosion-like octaves** (derivative damping, then a point-evaluable gully
   filter, written in-house). Visual check against a baked Gaea/World Machine
   host.
7. **GPU display detail** (compute, wgpu) and ground-shader splatting:
   optional, cosmetic only.

## 5. Risks

| Risk | Mitigation |
|---|---|
| Seams between tiles | Shared samples by construction. Same octave weight on both sides of an edge (the weight depends on the level, not the tile) |
| Seams between levels (popping, cracks) | Skirts (existing), vertex morph or cross-fade. Activate the new tile before dropping the old one |
| Non-determinism (libm, GPU, compiler flags) | Integer lattice, no transcendental functions in v1, versioned generator, native = wasm golden tests in CI |
| Gameplay vs display mismatch | Physics level is authoritative. Cosmetic detail is bounded in amplitude |
| Budget: CPU generation competes with decode, GPU generation with drawing | Generation goes through the existing worker pool and graphics budget, with a priority by SSE |
| Poles stretched in v1 | Stated limitation. 3D noise in v2 |
| "Too perfect" fractal look | Ridged + warp + erosion octaves. Hosts baked by real erosion |
| Detail that contradicts real data (fake cliffs at an airport) | Masks, and amplitude driven by the host's roughness. Zero detail below the host's resolution |
| Licences | No port of Shadertoy (NC) or MPL code. ML models are for lab baking only |

## Sources

[Perlin 2002]: https://mrl.cs.nyu.edu/~perlin/paper445.pdf
[Gustavson]: https://weber.itn.liu.se/~stegu/simplexnoise/simplexnoise.pdf
[IQ fBm]: https://iquilezles.org/articles/fbm/
[IQ derivatives]: https://iquilezles.org/articles/morenoise/
[IQ warp]: https://iquilezles.org/articles/warp/
[Musgrave]: https://www.sciencedirect.com/book/9781558608481/texturing-and-modeling
[Ebert et al.]: https://www.sciencedirect.com/book/9781558608481/texturing-and-modeling
[Jarzynski & Olano]: https://jcgt.org/published/0009/03/02/
[Eiserloh]: https://gdcvault.com/play/1024365/Math-for-Game-Programmers-Noise
[Wasm numerics]: https://webassembly.github.io/spec/core/exec/numerics.html
[Rust float semantics]: https://rust-lang.github.io/rfcs/3514-float-semantics.html
[WGSL accuracy]: https://www.w3.org/TR/WGSL/#floating-point-accuracy
[Gaffer determinism]: https://gafferongames.com/post/floating_point_determinism/
[`libm`]: https://crates.io/crates/libm
[`noise`]: https://crates.io/crates/noise
[Outerra news]: https://www.moddb.com/engines/outerra/news/outerra-engine-progress
[Outerra simflight]: https://simflight.com/?p=5974
[Asobo GDC]: https://www.asobostudio.com/files/inline-images/Designing_Terrain_System_Fuentes_Lionel.pdf
[MSFS displacement]: https://devsupport.flightsimulator.com/t/sdk-control-over-displacement-mapping-in-msfs-2024/10165
[NMS GDC]: https://gdcvault.com/play/1024265/Continuous-World-Generation-in-No
[Elite 80.lv]: https://80.lv/articles/generating-the-universe-in-elite-dangerous/
[SC PT4]: https://starcitizen.tools/Planet_Tech_v4
[SC Terra Firmer]: https://starcitizen.tools/CitizenCon_2019_-_Terra_Firmer
[Space Engine blog]: https://spaceengine.org/news/blog100531/
[KSP2 #12]: https://www.kerbalspaceprogram.com/news/dev-diaries-developer-insights-12-planet-tech
[KSP2 #18]: https://forum.kerbalspaceprogram.com/topic/214806-developer-insights-18-graphics-of-early-access-ksp2
[IB 80.lv]: https://80.lv/articles/infinity-battlescape-the-game-with-no-loading-screens
[Proland]: https://proland.inrialpes.fr/doc/proland-4.0/terrain/html/index.html
[Ulrich]: http://tulrich.com/geekstuff/chunklod.html
[Losasso & Hoppe]: https://hhoppe.com/proj/geomclipmap/
[Strugar CDLOD]: https://github.com/fstrugar/CDLOD
[Mei et al.]: https://hgpu.org/?p=2256
[Jákó & Tóth]: https://diglib.eg.org/handle/10.2312/EG2011.short.057-060
[runevision erosion]: https://blog.runevision.com/2026/03/
[Gaea]: https://quadspinner.com/
[World Machine]: https://www.world-machine.com/
[Houdini heightfields]: https://www.sidefx.com/docs/houdini/model/heightfields.html
[FEEDBACK DEM SR]: https://arxiv.org/abs/2007.01940
[AFN]: https://arxiv.org/abs/2010.01626
[DEM-SDE]: https://arxiv.org/abs/2407.01908
[Guided DEM SR]: https://arxiv.org/abs/2609.11886
[PUP upsampling]: https://playerunknownproductions.net/news/a-multistep-approach-to-upsampling-for-planetary-scale
[3D Tiles 1.1]: https://docs.ogc.org/cs/22-025r4/22-025r4.html
[glTF extensions]: https://github.com/KhronosGroup/glTF/tree/main/extensions
[quantized-mesh]: https://github.com/CesiumGS/quantized-mesh

- Noise: [Perlin 2002], [Gustavson], [IQ fBm], [IQ derivatives], [IQ warp], [Ebert et al.] (Musgrave's chapters on multifractal terrain)
- Determinism: [Jarzynski & Olano], [Eiserloh], [Wasm numerics], [Rust float semantics], [WGSL accuracy], [Gaffer determinism], [`libm`], [`noise`]
- Engines: [Outerra news], [Outerra simflight], [Asobo GDC], [MSFS displacement], [NMS GDC], [Elite 80.lv], [SC PT4], [SC Terra Firmer], [Space Engine blog], [KSP2 #12], [KSP2 #18], [IB 80.lv], [Proland]
- LOD: [Ulrich], [Losasso & Hoppe], [Strugar CDLOD]
- Erosion: [Mei et al.], [Jákó & Tóth], [runevision erosion], [Gaea], [World Machine], [Houdini heightfields]
- ML: [FEEDBACK DEM SR], [AFN], [DEM-SDE], [Guided DEM SR], [PUP upsampling]
- Formats: [3D Tiles 1.1], [glTF extensions], [quantized-mesh]
