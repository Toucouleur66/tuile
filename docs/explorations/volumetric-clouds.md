<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Volumetric clouds — the theory, and what tuile can do

A design study. **Nothing here is a decision and no code changes with it.**
It sets out what rendering clouds takes (the physics at the level a
renderer needs, the real-time and offline techniques, their costs), what a
weather data provider can supply, and how tuile could take both: a
`CloudSource` provider trait, a cloud model, passes in the film renderer
and the interactive renderer, and a PR plan with acceptance scenes.

**The boundary that shapes everything below.** Rendering and modelling
live in tuile; the data comes from outside. A host (a tracking service, a
flight replay, a weather site) supplies meteorological fields through a
trait, as it supplies terrain (`TerrainSource`) and imagery
(`ImageryProvider`) today. tuile never names a provider, never fetches a
weather service of its own, and knows nothing of who supplies the fields
(`CLAUDE.md` rule 3). What *does* belong to tuile is everything between a
gridded field and a pixel: the lowering of meteorological quantities to a
renderable medium, the procedural detail, time interpolation and wind
advection, lighting, shadows, and the composition with the air.

**The second constraint**, inherited from the atmosphere study (PR #141,
`docs/explorations/atmosphere-generalisation.md`): there is one engine.
Clouds are not a separate renderer bolted next to the sky; they are a
participating medium in the same model as the air — same lights, same
integration rule, same phase-function library — with one difference that
cannot be argued away: their density varies horizontally, so they cannot
enter the air's height-only tables (assumption A4 of that study) and need
their own march. With no clouds, the engine must produce today's pixels to
the byte (§6.7).

Read at `origin/main` (`a7ea3462`, tuile-bevy included), on
`feat/viewer-hillaire-atmosphere` (PR #137, draft)
and on `docs/atmosphere-generalisation` (PR #141). Neither branch is edited.

---

## 1. What tuile has today, and where clouds would attach

| Piece | Where | What it gives clouds |
|---|---|---|
| Hillaire 2020 tables | `crates/tuile-atmosphere/src/hillaire.rs` (CPU f64), `hillaire.wgsl` | `sunlight(height, mu_s)` — the sun's light arriving at any height through the air, read from the transmittance table: exactly the light a cloud sample is lit by. `level_irradiance`, `multi_scattering` for ambient |
| The integration rule | `hillaire.wgsl` `air_step` | "the sources over the step's own extinction, exact however long the step" — the energy-conserving step Hillaire 2016 introduced *for clouds*. The cloud march can share it verbatim |
| Air between eye and point | `hillaire.wgsl` `air_between(p)`, 12/24 steps per fragment | aerial perspective in front of a cloud: one call at the cloud's depth (§2.6) |
| Sky irradiance | `Atmosphere.sh_x/sh_y/sh_z` (an L1 harmonic of the sky's light) | the ambient light on a cloud's top and base, from two harmonic evaluations |
| Two lights | sun and Moon (`Light`, `air_step(…, sun, moon, …)`) | moonlit clouds come for free if the cloud march loops over the same lights |
| Sun shadow map | `tuile-film-gpu` (`shadow.wgsl`, `resolve.wgsl`; up to 4 jittered maps) | the place a cloud shadow multiplies the sun's term on the ground |
| Sky-view per frame | `sky_view.wgsl`, compute or render pass (`SkyViewPass::{Compute, Render}` on #137) | the pattern for a pass that has a WebGL2 twin |
| `lib.rs` of `tuile-atmosphere` | "**Clouds.** Deliberately absent rather than stubbed … It wants its own module and its own pass" | the stated intent this study follows |
| Doc 19 §7 | "Nuages : hors périmètre v1. Couche 2D … en v2 ; le volumétrique est incompatible avec le budget Cycles" | the OpenUSD/Cycles path stays out (§7.4); the film renderer is wgpu |
| Provider traits | `ImageryProvider` (`tuile-core/src/raster.rs`), `TerrainSource` (`tuile-terrain/src/source.rs`) | the shape `CloudSource` copies: async fetch of bytes with `Fetched` lifetime, `holds()` for the budget's presence oracle, `generation()`, an `Absence` that tells "nothing here" from "failed" |
| PMTiles | `tuile-repository`, `tuile-tile-server`, `tuile-bake` | a store and a server for cloud tiles without new infrastructure |

So the light, the integration and the aerial perspective exist; what is
missing is a density field, its source, a march through it, and shadows.

---

## 2. Theory, at the level a renderer needs

### 2.1 What a cloud is made of

A cloud is air holding condensed water: liquid droplets (radius 2–20 µm,
effective radius `r_e` ≈ 5–12 µm), ice crystals (10 µm to 1 mm), or both
(mixed-phase, between 0 °C and about −38 °C). The renderer needs three
numbers per point and nothing else:

| Quantity | Symbol | Typical values | Where it comes from |
|---|---|---|---|
| Liquid water content | LWC | stratus 0.05–0.3 g/m³; cumulus 0.2–1 g/m³ (up to 2–3 in cores); fog 0.01–0.2 | NWP `QC` (kg/kg) × air density |
| Ice water content | IWC | cirrus 10⁻⁴–10⁻¹ g/m³ | NWP `QI` × air density |
| Effective radius | `r_e` | liquid: maritime 10–15 µm, continental 5–8 µm; ice: 20–100 µm | rarely in the data: a constant per type, or from droplet number concentration |

**Extinction** of a liquid cloud (Stephens 1978; Hansen & Travis 1974), for
droplets much larger than the wavelength (extinction efficiency ≈ 2):

`σ_t ≈ 3 · LWC / (2 · ρ_w · r_e)`

With LWC = 0.5 g/m³ and `r_e` = 10 µm: σ_t = 3 × 5·10⁻⁴ / (2 × 1000 ×
10⁻⁵) = **0.075 m⁻¹** — a mean free path of 13 m, a visibility inside the
cloud (Koschmieder, 2 % contrast: 3.9/σ) of ~50 m, and an optical depth of
~75 through a 1 km cumulus. Stratus: 0.01–0.05 m⁻¹. Cirrus (Fu 1996:
`σ_t ≈ IWC · (a + b / D_ge)`): 10⁻⁴–10⁻³ m⁻¹, τ ≈ 0.03–3 — often
see-through.

Three consequences for rendering:

1. **Clouds are white and grey, not coloured.** Droplets are 10–20× the
   wavelength: extinction is spectrally flat, and the single-scattering
   albedo in the visible is 0.9999+. One scalar σ_t, σ_s = σ_t. Colour comes
   from the light (a sunset's transmitted sun) and from the air in front
   (aerial perspective), both of which tuile already computes.
2. **The optical depths are huge** — 10 to 100 where the air's is 0.1–0.3.
   Hillaire's air approximation (multiple scattering summed as a geometric
   series over an isotropic second order) does not hold: clouds need their
   own multiple-scattering model (§2.3).
3. **The lengths are tiny.** A free path of 10–50 m inside a structure
   seen from 50 km: the march step, the noise and the LOD have to span
   three orders of magnitude (§3.6).

### 2.2 Phase functions, and the silver lining

Mie theory gives the exact phase function of a sphere; for a droplet
distribution with `r_e` ≈ 10 µm it has:

- a **diffraction peak** a few degrees wide in the forward direction (about
  half the scattered energy) — the **silver lining**: a thin cloud edge
  between the eye and the sun lit brighter than the sun-facing side;
- an asymmetry `g` ≈ 0.85–0.87;
- the **cloudbow** near 138° and the **glory** at 180° (the halo round a
  paraglider's shadow on a cloud top — a scene tuile's users actually see).

| Model | Shape | Cost | Fidelity | Source |
|---|---|---|---|---|
| Henyey-Greenstein, one lobe | `g` ≈ 0.8 | ~10 ALU | no back lobe, peak too broad | Henyey & Greenstein 1941 |
| **Double HG** | `lerp(HG(g₁ ≈ 0.8), HG(g₂ ≈ −0.3…−0.5), w ≈ 0.5…0.8)` | 2× HG | forward peak + back-scatter lobe; the games' standard (Frostbite 2016, Nubis) | Hillaire 2016 |
| Cornette-Shanks | HG with a `(1 + cos²)` factor | ~1.1× HG | what `hillaire.wgsl` uses for the air's aerosols | Cornette & Shanks 1992 |
| **HG + Draine** | blend fitted to Mie for diameters 5–50 µm | ~2.5× HG, analytic sampling | ~95 % of Mie across sizes, no table | Jendersie & d'Eon 2023 |
| Tabulated Mie | 1D texture of θ (≥ 1 800 bins near 0°) | one fetch | exact, including cloudbow and glory | Bohren & Huffman; MiePlot |

Recommendation: **double HG as the real-time default**, tabulated Mie for
the film and the reference. Both fit #141's `Phase::{DoubleHg, Tabulated}`
(its §5.4) — one phase library for air and clouds.

### 2.3 Multiple scattering

With τ ≈ 10–100, most light leaving a cloud has scattered dozens of times
(Wrenninge 2015 notes "upwards of 100 bounces" for thick media). Single
scattering alone gives clouds that are dark grey inside and black under
the base — wrong in the way "black ground" is wrong.

| Approximation | Idea | Cost per sample | Notes | Source |
|---|---|---|---|---|
| **Octaves** (Wrenninge et al. 2013) | sum `N` single-scattering terms, octave `i` with extinction × aⁱ, contribution × bⁱ, phase eccentricity × cⁱ (a, b, c < 1, typically 0.5) | N × (phase + `exp`) on the *same* light march | cheap, art-directable, energy roughly right; Frostbite 2016 and Unreal's `MultiScattering` Octave Count / Contribution / Occlusion / Eccentricity | Wrenninge, Kulla, Lundqvist 2013; Hillaire 2016; Unreal docs |
| **Powder** (Schneider 2015) | darken the edges facing the light: `E = 2·e^{−d}·(1 − e^{−2d})` (Beer × "powder") | 1 `exp` | not physical; reproduces dark edges of cumulus seen with the sun behind the viewer; Nubis³ replaced it with a measured "dark edge / inner glow" model | Schneider & Vos 2015, Schneider 2023 |
| Ambient by height | sky light × gradient from base (dark) to top (bright) | ~0 | Schneider 2015, Hillaire 2016 (sky irradiance top/bottom) | |
| Bouthors et al. 2008 | precomputed transport in slabs, "collector" surfaces | high, precompute | real-time anisotropic multiple scattering, research | Bouthors et al. 2008 |
| Neural ("Deep Scattering") | an MLP predicts the multiply-scattered radiance from a density stencil | offline-ish | production-quality look, research | Kallweit et al. 2017 |
| Path tracing (delta / ratio tracking) | exact | 10²–10³× | the reference | Novák et al. 2018 |

Recommendation: **octaves (N = 2–3) + ambient by height**, with powder as
a tuning value that defaults to off and is kept only if a reference says it
helps; all three held to a **path-traced reference** (§6.8) — the same
Monte-Carlo reference #141 §5.7 proposes for thick air, extended to a 3D
medium.

### 2.4 The light march, and shadows

Each primary sample needs the transmittance towards each light:

| Distance from the sample | Technique | Cost |
|---|---|---|
| Near (0–~1 km) | **cone march**, 6 samples with growing steps (Schneider 2015), plus one far sample | 6 density evaluations |
| Far (whole layer) | **cloud shadow map** from the sun: per texel, the optical depth through the layer — or a **Beer shadow map** (front depth, mean extinction, max optical depth; Unreal, three-clouds) | one fetch |
| Air above the cloud | `sunlight(height, mu_s)` from the transmittance table | one fetch, exists |

The same cloud shadow map gives:

- **Cloud shadows on the ground** — the strongest single cue that clouds
  are *there*, and the cheapest (one fetch in the ground's resolve, a
  multiplication of the sun's term only — the sky's ambient is left with
  its own reduction, and the `least` floor of `ground_light` stays: a cloud
  shadow never makes ground black).
- **God rays** (crepuscular rays through gaps): the per-fragment air march
  (`air_between`, 12–24 steps) multiplies its sun single-scattering term by
  the shadow map at each step. One fetch per step; no new pass.
- **Shadows on host geometry and overlays** (a glider's model in a cloud's
  shade), by the same fetch.

### 2.5 Clouds lit by the atmosphere

At a cloud sample at height `h` and sun zenith cosine `mu_s`:

`L_in(x, ω) = Σ_lights [ sunlight(h, mu_s) · T_cloud(x → light) · p(ω·ω_l) ] · weight_l + ambient(x)`

- `sunlight(h, mu_s)` is the table read the air already does: at sunset it
  is orange and dim, so clouds redden by construction; at night the Moon's
  term, with its weight, lights them (moonlit cumulus).
- `ambient(x)` = sky irradiance from the L1 harmonic, evaluated for the up
  and the down hemisphere, interpolated by the height fraction in the
  cloud; plus the ground's bounce (`ground albedo × level_irradiance`) on
  the base.
- The light is per unit of the sun's irradiance outside the air, as
  everything else (`atmosphere.illuminance` converts) — clouds enter the
  same exposure chain.

What the air's tables cannot know: the **sky under an overcast** is darker
and greyer than the tables' clear sky, and the ground's ambient is reduced.
The cheap model: scale the sky's irradiance on the ground by the local
cloud transmittance towards the zenith, averaged (a mip of the shadow map
taken vertically), never below the floor. The honest model — clouds feeding
back into the multiple-scattering table — is out of scope (§9).

### 2.6 Aerial perspective in front of clouds

For a cloud thin compared with the air between the eye and it, and calling
`A(d)`, `τ(d)` the air's in-scattering and transmittance from the eye to
distance `d`, `C` and `T_c` the cloud's radiance and transmittance, and
`G'` the ground (or sky) with its full air already applied, the exact
composition is:

`L = T_c · G' + (1 − T_c) · A(d_c) + τ(d_c) · C`

where `d_c` is the cloud's **transmittance-weighted mean depth** along the
ray (the depth the march records). It needs one `air_between` at `d_c` per
cloud pixel, at the cloud's resolution, and nothing from the ground pass
but its finished colour. It is the formula Unreal approximates with its
aerial-perspective volume at a mean depth; tuile can afford the exact
per-pixel version because it already marches the air per fragment.

**Identity:** with no cloud, `T_c = 1` and `C = 0`, so `L = G'` exactly in
IEEE arithmetic (`x·1 + 0·a + b·0`) — the reduction §6.7 needs.

### 2.7 Cloud types, altitude bands and shapes

WMO International Cloud Atlas, mid-latitudes:

| Level | Heights | Genera | Density profile in height | Render as |
|---|---|---|---|---|
| Low | surface – 2 km | stratus, stratocumulus, cumulus (base), fog | St/Sc: flat top and base; Cu: rounded top, flat base at the LCL | volume |
| Middle | 2 – 7 km | altocumulus, altostratus, nimbostratus | sheets, cells | volume |
| High | 5 – 13 km | cirrus, cirrocumulus, cirrostratus (ice) | thin, fibrous | **2D layer** with optical depth (Nubis and Unreal do the same) |
| Vertical | 0.5 – 13 km | cumulus congestus, cumulonimbus | towers, anvil at the tropopause | volume, tall |

For the scenes tuile's films show — paragliding and gliding replays — two
facts matter most:

- **The cumulus base is the lifting condensation level**: about
  `125 m × (T − T_d)` above the ground (Espy's rule; Lawrence 2005; exact
  form Romps 2017). Flat bases at one height across a field of cumulus are
  what pilots fly up to; they must be flat and at the right height.
- **The "sea of clouds"** (valley stratus under an inversion, summits
  above it) is the other frequent scene in the Pyrenees and the Alps: a
  flat-topped stratus layer that terrain pierces.

---

## 3. Real-time and offline techniques

### 3.1 The family: ray-marched density from a weather map and noise

Every production system since 2015 has the same skeleton:

1. a **weather map** (2D): coverage, type, sometimes density and
   precipitation, per texel of tens of metres to kilometres;
2. a **height profile** per type (a 2D LUT: type × height fraction →
   density multiplier);
3. **3D noise**: a low-frequency *shape* texture (Perlin-Worley + Worley
   octaves, 128³) and a high-frequency *detail* texture (Worley, 32³),
   distorted by a 2D curl-noise texture for wisps;
4. density = `remap(shape, 1 − coverage·profile, 1) − detail erosion`,
   evaluated per step;
5. a **ray march** through the cloud shell (between two spheres), with
   empty-space skipping, a cone light march, and temporal reuse.

| System | Year | What it added | Cost stated | Source |
|---|---|---|---|---|
| Horizon Zero Dawn (Schneider & Vos) | 2015 | the skeleton; Perlin-Worley; powder; **1/16 pixels per frame** at quarter resolution with reprojection | **≤ 2 ms on PS4** | Advances 2015 |
| Nubis (Schneider) | 2017 | regional authoring, cloud types as a weather map, animation | under the 2 ms target | Advances 2017 |
| Frostbite (Hillaire) | 2016 | energy-conserving integration, octave multiple scattering, sky-lit ambient, physical units | — | PBS course 2016 |
| Red Dead Redemption 2 (Bauer) | 2019 | an integrated atmosphere + clouds + fog system driven by a weather system, at console budgets | not stated in the abstract | Advances 2019 |
| Nubis Evolved (Schneider) | 2022 | Forbidden West: richer cloud shapes, superstorm | — | Advances 2022 |
| **Nubis³** (Schneider) | 2023 | **voxel clouds** for flying through: compressed signed distance fields to skip empty space, simulation-modelled shapes, upscaled dense voxels, light-sampling acceleration, dark-edge / inner-glow approximations | "high frame rates when flown through" | Advances 2023 |
| Unreal Volumetric Cloud (Hillaire) | 2020– | material-driven density, Beer shadow maps on the ground, aerial perspective on clouds, multiple-scattering octaves (one octave recommended for games) | scalable: resolution and step settings | Unreal docs |
| Flight simulators with live weather | 2020– | global weather from an NWP provider (60 vertical levels per the press; 32 density layers per marketing), cloud view distance ~600 km | not published | press and forums only; no official technical source found |
| three-clouds (three-geospatial) | 2024– | **geospatial clouds on WebGL2** with a Bruneton atmosphere: up to 4 layers, Beer shadow maps, temporal upscaling, light shafts, haze; WebGPU rewrite in progress | runs in a browser | MIT, GitHub |

The last row matters for tuile: it is the existence proof that the whole
skeleton — weather-driven layers, shadow maps, light shafts, temporal
upscaling over a globe — runs on **WebGL2** in a fragment pass. Its known
limits (ghosting from basic temporal upscaling, aerial-perspective
artefacts on far sparse clouds, seams in weather tiling and at the poles)
are a list of things to design for from the start (§6.4).

### 3.2 2.5D versus voxels

| | 2.5D (weather map + profile + noise) | Voxels (Nubis³, film) |
|---|---|---|
| Memory | weather maps (MiB) + two noise textures (~8 MiB) | 10²–10³ MiB for a region; sparse/compressed |
| Data it matches | NWP: coverage, base/top, type per column | NWP per-level LWC/IWC profiles; simulations; authored VDBs |
| Flying through | needs a careful near field (§3.6) | natural |
| Planet scale | natural (the weather map is a texture over the globe) | streaming of voxel bricks like tiles |
| Recommendation | **first** | a later form of the same tile (§5.3, "volume" form) |

### 3.3 Temporal reuse

| Technique | How | Risk |
|---|---|---|
| Interleaved (HZD) | one pixel of each 4×4 block a frame, the rest reprojected | 16 frames to converge after a disocclusion |
| Jittered + history blend (Unreal) | every pixel each frame at reduced resolution, blue-noise start offset, exponential history with neighbourhood clamping | ghosting on fast motion |
| Checkerboard 2×2 | a compromise | |

Reprojection uses the cloud's mean depth and **the wind**: clouds move,
and their motion is known analytically (§5.5), so the history is fetched
where the cloud *was*, not where the background was. History is reset on a
jump (the renderer already tells a jump from motion: `JUMP_SUN`,
`JUMP_EYE` in #137's `sky.rs`), and the frame after a reset is drawn with
full steps, never with an empty buffer (§6.4).

**The film has no history.** A film is cut into segments rendered by
separate processes on separate GPUs and concatenated; frame *n* cannot
depend on frame *n − 1*. Every film frame is a pure function of
`(instant, camera, field)`: full resolution, more steps, the jitter taken
from the supersample index (and if needed the frame number), never from a
previous frame.

### 3.4 Froxels

Froxel volumes (Wronski 2014; Hillaire 2015) suit local fog and lights near
the camera; Hillaire 2020 uses a 32×32×32 froxel volume for aerial
perspective. tuile marches the air per fragment instead and has no froxel
grid. Recommendation: **no froxels for clouds**; a froxel grid becomes
interesting for ground fog with local lights, which is not asked for.

### 3.5 Weather-map-driven layering

The weather map has, per layer: coverage (0–1), type (0 = stratus … 1 =
cumulus, continuous), base and top (metres), density scale. Up to three
volumetric layers (low, middle, convective tower) and one 2D cirrus layer
cover the WMO table of §2.7. Each layer's march range is the intersection
of the ray with its spherical shell `[R + base, R + top]` — the same sphere
the air is laid on (`Atmosphere.centre`), clipped at the **scene depth**
(mountains go into clouds; a stratus sea is pierced by summits).

### 3.6 LOD at planet scale

The range: a paraglider 50 m under a cumulus base; an aircraft flying
through it; a camera at 50 km seeing a 400 km field; an orbit seeing the
weather of a continent.

| Range | Pixel footprint vs. features | What is drawn |
|---|---|---|
| In the cloud (0–200 m) | 0.1–1 m ≪ free path (10–50 m) | full noise, smallest steps (2–5 m), detail and curl at full amplitude; fog-like transmittance |
| Near (0.2–5 km) | ≈ detail scale | shape + detail, 32–128 steps |
| Far (5–200 km) | ≫ detail | shape only; detail **filtered to its mean** |
| Orbit (> 200 km) | ≫ shape | weather map only: the column's optical depth, a slab model |

**No switch between them.** Each band is the filtered limit of the one
before: the noise is mipmapped, so as the footprint grows the detail term
converges to its mean and the volume converges to "weather map ×
profile", which integrates in closed form along a ray through a shell. The
step size follows the footprint and the density's distance field (Nubis³
uses SDFs for exactly this). The 2D cirrus layer is a layer at all ranges.

The weather map is resampled into **camera-centred cascades** in a local
frame (e.g. 3 × 512² texels at ~100 m, ~800 m and ~6 km: 51 km, 410 km and
3 000 km across), so that a sample needs no `atan2`/`asin` per step and no
f32 precision is spent on ECEF. A cascade is rebuilt when the camera moves
a fraction of it; the new one is filled and activated before the old one is
released — the swap order of `CLAUDE.md` ("activate the new, then
deactivate the old"), so no frame lacks clouds that the previous one had.

### 3.7 Precision

The noise must be sampled in a frame where f32 is metres-accurate: render
space (eye-relative), plus an offset `(render_origin + wind_offset) mod
noise_period` computed in **f64 on the CPU** and uploaded. A world-space
noise coordinate in f32 at 6.4·10⁶ m has a 0.5 m ulp: a cloud would crawl
by half-metres as the camera moves — the shimmer `CLAUDE.md` rule 7 exists
to prevent. Heights in the shell are carried apart from the radius, as
`hillaire.wgsl` already does.

### 3.8 Budgets

Estimates, to be measured on tuile's own renderers (bench beside the
digests, as #141 §5.6 asks). Anchor: HZD's ≤ 2 ms on a PS4 (1.8 TFLOPS) at
quarter resolution with 1/16 pixels a frame.

**Memory (GPU):**

| Item | Desktop | Mobile / WebGL2 |
|---|---|---|
| Shape noise | 128³ RGBA8 = 8 MiB (+⅐ mips) | 64³ RGBA8 = 1 MiB |
| Detail noise, curl | 32³ RGBA8 + 128² RG8 ≈ 0.2 MiB | same |
| Height-profile LUT | 64 × 64 RGBA8 = 16 KiB | same |
| Weather cascades | 3 × 512² × (3 layers × 8 B) ≈ 18 MiB | 3 × 256² ≈ 4.5 MiB |
| Cloud shadow map | 1024² RGBA16F (Beer) = 8 MiB | 512² = 2 MiB |
| Cloud targets at ½ × ½ resolution, 1080p (colour+transmittance RGBA16F, depth R32F) | 960 × 540 × 12 B ≈ 6 MiB, ×2 with history ≈ 12 MiB | ⅓ × ⅓: ≈ 6 MiB |
| **Total** | **≈ 47 MiB** at 1080p (≈ 90 MiB at 4K) | **≈ 14 MiB** |

The decoded cloud tiles behind the cascades are CPU-side: a regional
flight (4–16 tiles × 2 forecast steps × 1.5 MiB raw) is 12–48 MiB, which
the graphics budget counts like imagery (§6.6).

**Time:**

| Backend | Pass | Steps (primary / light) | Target |
|---|---|---|---|
| Desktop native / WebGPU, compute | ½ × ½ res, all pixels a frame, history | 64–128 / 6 | **1–2 ms** |
| Laptop integrated GPU | ½ × ½, checkerboard | 48–96 / 6 | 2–4 ms |
| Mobile | ⅓ × ⅓, 1/4 a frame | 32–64 / 4 | 3–5 ms |
| WebGL2, fragment | ½ × ½ or ⅓ × ⅓, 1/4 a frame | 32–64 / 4 | as mobile |
| Shadow map | 1024², every N frames or when the sun or wind moved a texel | 16–32 through the layer | 0.1–0.3 ms amortised |
| **Film** (wgpu, compute, no history) | full res × supersamples | 128–256 / 6–8 + octaves 3 | tens of ms a frame on an L4 / RTX 4090 class GPU: to be measured against the film's per-frame budget |

Per in-cloud step the cost is ~3 fetches for density (weather, shape,
detail) and ~2 × 6 for the light march; empty-space skipping (stepping
with the weather map alone where coverage is zero, ×3–×4 the step) is what
keeps a typical pixel at 200–400 fetches instead of ~1000.

### 3.9 Offline quality

| Option | What | Cost | Use |
|---|---|---|---|
| Ray march at film settings | §3.8's film row | tens of ms / frame | **the film's default** |
| Path-traced clouds in compute | delta / ratio tracking, a few hundred paths per pixel, octaves for the tail | seconds a frame | prestige shots, and the **reference** for the octave and ambient approximations |
| Export a VDB to an offline renderer | the field as an OpenVDB volume | renderer's | out of scope: doc 19 excludes volumetrics from the OpenUSD/Cycles budget (~0.47 s/frame) |

Test data for the volume form and the reference: the **Walt Disney
Animation Studios cloud data set** (OpenVDB, several resolutions, CC BY-SA
3.0 — a lab reference only, never in shipped fixtures, per the licence
rule of lab versus shipped) and the voxel pack published with Nubis³.

---

## 4. Data: what a weather provider can supply

### 4.1 Sources a provider would draw on

| Source | Resolution, steps | Cloud fields | Licence / access | Source |
|---|---|---|---|---|
| AROME (Météo-France) | 1.3 km (0.01°) and 2.5 km over France and borders; hourly | cloud cover (low, middle, high, total); on height/pressure levels: cloud fraction, liquid and ice water; wind; T, humidity | Licence Ouverte 2.0 (Etalab), GRIB2 | data.gouv.fr / meteo.data.gouv.fr |
| ICON-D2 / ICON-EU / ICON (DWD) | 2.2 km (central Europe) / 6.5 km / 13 km; hourly | `CLC` per model level, `QC`, `QI`, `CLCL/CLCM/CLCH`, `CLCT`, convective base and top `HBAS_CON`/`HTOP_CON`, `CEILING`, `U/V/W`, `T`, `RELHUM` | open data, GRIB2 | opendata.dwd.de |
| IFS (ECMWF) | 0.25°, 3–6 h steps | `tcc`, `lcc/mcc/hcc` (open subset); full catalogue with per-level fields under charges | CC BY 4.0 | ecmwf.int open data |
| GFS (NOAA) | 0.25°, hourly to 120 h | cloud cover per layer, cloud water mixing ratio per pressure level, wind | public domain | NOMADS, AWS open data |
| HRRR (NOAA) | 3 km, US | per-level cloud water/ice, cloud base/top | public domain | rapidrefresh.noaa.gov |
| ERA5 (reanalysis) | 0.25°, hourly, since 1940 | per-level cloud fraction, `clwc`, `ciwc` | CC BY 4.0 (Copernicus) | cds.climate.copernicus.eu |
| Geostationary imagery (MTG FCI, Meteosat, GOES ABI) | 0.5–2 km, every 10 min | visible/IR radiances; derived **cloud mask, cloud type, cloud top height/temperature** (NWC SAF CT/CTTH, GOES ACHA) | EUMETSAT / NOAA terms | eumetsat.int, nwcsaf.org |
| A processed API | point or gridded | cover low/mid/high, cover on pressure levels | per service | e.g. Open-Meteo |

What these say together:

- **NWP does not resolve individual cumulus.** A 1.3 km grid cell is the
  size of one cumulus; NWP gives the *fraction* of the sky covered, the base
  and the top, and the water content — a statistical description. The
  procedural detail generates clouds consistent with it: **plausible
  clouds, not the clouds that were there.**
- **Satellite imagery does see the actual clouds** (at 0.5–2 km, every 10
  min) but only from the top: coverage and top height, no base. For a
  *replay of a past flight*, satellite coverage + NWP base and profile is
  the best available description of what the pilot saw.
- **Past flights need archives**: reanalysis (ERA5, coarse), or the
  provider's own archive of forecast runs and satellite products at the
  flight's time. Which one is the provider's choice and licence question,
  not tuile's.

### 4.2 From fields to a renderable field

What tuile's lowering does with a provider's fields (a library in tuile,
§5.4, so that every provider lowers alike):

| Input | Lowered to | Rule |
|---|---|---|
| Cloud fraction per level `CLC(z)` | per layer: coverage = max (or random-overlap) of `CLC` over the layer's levels | layers split where `CLC` drops below a threshold between levels |
| `QC`, `QI` (kg/kg) × air density | density scale; σ_t by §2.1 with `r_e` per type | ice where `T < −38 °C` or `QI > QC` |
| Convective base/top (`HBAS_CON`, `HTOP_CON`) or `T`, `T_d` at 2 m | the cumulus layer's base and top; base by the LCL when only T/T_d are given | Espy/Romps |
| Cloud type (satellite CT, or from the fields: convective vs. stratiform share, thickness) | type ∈ [0, 1] (stratus → cumulus), cirrus to the 2D layer | a table, documented |
| `U`, `V` at the layer's mid-height | the layer's wind (advection, §5.5) | |
| Satellite cloud mask / top height | coverage and top, overriding NWP where observed | the observation wins over the forecast at observation times |

### 4.3 Time

Forecast fields come every hour (AROME, ICON-D2, HRRR) or every 3–6 hours
(IFS, GFS); satellite products every 10–15 minutes. Between two steps
`t_A < t < t_B`, a linear cross-fade makes clouds fade in and out in place
— a thing real clouds do not do. Advection-corrected interpolation
(the nowcasting technique: semi-Lagrangian advection, as in pysteps):

`F(x, t) = (1 − w) · F_A(x − u·(t − t_A)) + w · F_B(x + u·(t_B − t))`, `w = (t − t_A)/(t_B − t_A)`

with `u` the layer's wind. The procedural detail moves with the wind
continuously (§5.5). The cumulus life cycle (15–30 min) is a slow
evolution coordinate of the noise, a pure function of time.

---

## 5. A `CloudSource` and a cloud field

### 5.1 Where things live

| Crate | Holds | Depends on |
|---|---|---|
| **`tuile-clouds`** (new) | the `CloudSource` trait, the tile format, the manifest, residency and time interpolation (CPU, f64), the lowering library from neutral gridded fields, two sources shipped with tuile (§5.4) | `tuile-core` (tiling, `Fetched`, storage); **no GPU, no tokio**; compiles to `wasm32-unknown-unknown` |
| **`tuile-atmosphere`** (`clouds` module) | the cloud *medium* — extinction from water content, type height profiles, phase, octave multiple scattering, the light march — CPU reference (f64) and WGSL; behind the `wgpu` feature, the noise textures, cascades, shadow map and march passes, shared by both renderers like #137's `gpu` module | as today |
| `tuile-film-gpu` | the film's cloud pass (compute, full resolution, no history) and its composition in the resolve and present | the above |
| `tuile-wgpu` | the interactive cloud pass (compute, or render pass on WebGL2), temporal reuse, composition | the above |
| `tuile-viewer`, `tuile-bevy` | host configuration (§6.5) | |

Why split: the medium is part of the one engine (#141), so it goes where
the air is; the data plumbing — trait, tiles, time — has nothing to do
with light and is what a server, a baker or a wasm worker needs without a
renderer, the way `tuile-terrain` sits apart from `tuile-wgpu`.

### 5.2 The trait

```rust
/// Where a cloud tile is: a tile of a tiling scheme, at a valid time.
pub struct CloudCoord { pub level: u32, pub x: u32, pub y: u32, pub valid: ValidTime }

/// What a source can serve: its valid times, its tiling, its levels, its
/// bounds, the run it comes from, and its attribution (shown by the host).
pub struct CloudManifest { /* serde, the cloud twin of layer.json */ }

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait CloudSource: Send + Sync {
    /// The manifest; asked again when `generation` changes (a new run).
    async fn manifest(&self) -> Result<Fetched<CloudManifest>, CloudSourceError>;
    /// A tile's bytes, as served (the format of §5.3), with their lifetime.
    async fn fetch_tile(&self, coord: CloudCoord) -> Result<Fetched<Bytes>, CloudSourceError>;
    /// Bumped when the source starts serving another run.
    fn generation(&self) -> u64 { 0 }
    /// The budget's presence oracle, as `ImageryProvider::holds`.
    fn holds(&self, coord: CloudCoord) -> bool { let _ = coord; false }
}
```

`CloudSourceError` carries an `Absence` like `TerrainSourceError`, and the
distinction matters more here: **"no data" is not "clear sky"**. A missing
tile falls back to its parent level, then to the nearest valid time, and
only then to "no clouds" — the identity, which is safe (the scene without
clouds) and never a hole. A tile arriving late fades in over a fixed time
as the new cascade is activated before the old is released.

### 5.3 The tile: a layered column

Version 1, the **layers** form (what NWP resolution supports), per texel
and per layer (up to 3 volumetric + 1 cirrus):

| Field | Encoding | Range |
|---|---|---|
| coverage | u8 | 0–1 |
| type | u8 | 0 stratus → 1 cumulus |
| density scale | u8 | water content, log-encoded |
| ice share | u8 | 0–1 |
| base, top | u16 each | metres above the ellipsoid, 0.5 m units (0–32 km) |
| wind u, v | per tile and layer, f32 (per-texel optional) | m/s |

Little-endian planes after a small header (magic, version, size, layer
count, valid time, run), compressed by the archive (PMTiles supports gzip
and zstd tile compression). 256² × 4 layers × 8 B = 2 MiB raw; smooth
fields compress tens of times. Tiles in the **geographic** tiling of
imagery: level 6 (256-px tiles) reaches ~1.2 km texels, the AROME grid;
levels 0–3 serve global models and the orbit view.

Version 2, the **volume** form: per texel, N height bins (16–32) of
liquid and ice content, for providers that want per-level profiles to
show (towering cumulus, multi-layer decks) — the 2.5D march then reads its
profile from the tile instead of the type LUT.

Storage and transport: **one PMTiles archive per valid time** (or per run
with time in the key), and the manifest as a JSON beside them; served by
`tuile-tile-server` or any HTTP/CDN like other tiles; or packed in the
film's store for a film ("the render reads the store").

### 5.4 Sources and lowering shipped in tuile

| Item | What | Why in tuile |
|---|---|---|
| `TemplateCloudSource` | a manifest URL + a tile URL template (PMTiles or z/x/y), on the `TileFetcher` hosts already pass | the common case: a provider serves tiles |
| `StatedCloudSource` | a field stated by the host: "cumulus, coverage 0.4, base 1 800 m, top 3 200 m, wind 5 m/s from 270°" everywhere or in a region | tests, demos, and hosts without data |
| `lower::GriddedFields` → tiles | the lowering of §4.2 from neutral arrays (lat/lon grid, level heights, CLC, QC, QI, T, T_d, U, V), no file format | every provider lowers alike; deterministic; tested on fixtures |
| GRIB2 reading | **not in tuile's core path**. If wanted, a tool crate on an established GRIB2 decoder (e.g. `grib` / grib-rs) that feeds `GriddedFields` | a WMO format, not a brand; still the provider's job by default |

### 5.5 Advection and determinism

- The detail noise moves by `offset(t) = ∫_{t₀}^{t} u(τ) dτ` per layer, with
  `u` piecewise linear between forecast steps: **closed form**, computed in
  f64 from the scene instant, never accumulated frame to frame (the same
  rule #141 §5.5 states for orbits and pulsar phases). Reduced modulo the
  noise period in f64 before it reaches the GPU (§3.7).
- The noise textures are generated on the CPU by an **established noise
  crate** (Perlin and Worley/cellular), from a fixed seed, using only
  `+ − × ÷ sqrt` (correctly rounded in IEEE) so that native and wasm make the
  same bytes; their digest is tested.
- A film frame is a pure function of `(cloud tiles' digests, instant,
  camera, look)`. The tiles' digests enter the film's scene digest; the
  pack refers to the cloud archive by key and digest.
- GPU pixels are deterministic per adapter, as the existing digests are.

---

## 6. Rendering in tuile

### 6.1 One engine

Following #141's data model, clouds become a fourth kind of medium
component:

| #141's medium | Clouds |
|---|---|
| `rayleigh: Component` (1D profile) | — |
| `absorbers: Slots<Absorber, 2>` (1D) | — |
| `aerosols: Slots<Aerosol, 2>` (1D, `Phase`) | — |
| — | **`fields: Slots<CloudLayer, 4>`**: a horizontally varying medium, its density from a `CloudField` (cascades + noise), its `Phase` from the same enum, σ_s = σ_t |

What is shared: the lights loop (sun and Moon today; N lights after #141
PR 2 — each light adds a term to the cloud's in-scattering, and the light
march is shared between lights only for the shadow-map part), the
`sunlight()` table read, the energy-conserving step (`air_step`'s rule),
the `Phase` evaluation, the units. What is not: the 1D media go into the
tables; the field is marched. The air's tables ignore clouds (they stay
height-only), with the overcast correction of §2.5 on the ground's ambient.

### 6.2 The film renderer (`tuile-film-gpu`)

Passes, all compute, inserted between the resolve and present:

1. **Cascades** (when the camera or time moved enough): resample decoded
   tiles of `t_A`, `t_B` into the local cascades with the advection-corrected
   blend of §4.3.
2. **Cloud shadow map** from the sun (and the Moon at night), Beer form.
3. **Resolve** reads it: the sun's term on the ground × cloud shadow, beside
   the existing sun map; the air march × cloud shadow per step (god rays).
4. **Cloud march** per pixel and per supersample, full resolution, against
   the visibility depth; writes radiance, transmittance and mean depth.
5. **Compose** (§2.6), then `present` as today (exposure, tone, overlays).

Overlays: those drawn in the world are composited *after* clouds or
*before*, by their depth against the cloud's mean depth — an overlay inside
a cloud is fogged by `T_c` and the cloud in front of it; a HUD overlay is
on top as today.

### 6.3 The interactive renderer (`tuile-wgpu`, on #137)

The same passes at ½ × ½ resolution with history (§3.3), the march in
compute where the device has it and in a fragment pass where it has not
(the `SkyViewPass::{Compute, Render}` pattern). A depth-aware upsample
(bilateral on the scene depth) when composing at full resolution, so that
summits against clouds keep their edges.

Composition must come **after all opaque geometry, the host's included.**
In `tuile-bevy`, Bevy's main pass draws the host's entities after the
globe's `HostPass`; clouds composited inside the globe's pass would be
drawn under a glider model that is in fact inside a cloud. So clouds need a
second hook after the host's opaque pass (a Bevy render-graph node before
transparents), reading the host's final depth. Same in the viewer for
world overlays.

### 6.4 WebGL2

| Concern | Answer |
|---|---|
| No compute | fragment passes for cascades, shadow map, march, compose (as `SkyViewPass::Render`) |
| 3D textures | WebGL2 has `sampler3D` |
| Float targets | RGBA16F render targets need `EXT_color_buffer_float`/half-float; where absent, the clouds fall back to the **orbit-limit form** (the 2D layer of §3.6, drawn in the ground and sky shaders: weather map × column optical depth) — never a hole |
| Texture units (16 per fragment stage, held by `webgl2.rs`'s test) | the ground shader gains **one** unit (cloud shadow map); the test is extended to count it |
| Cost | ⅓ × ⅓ resolution, 1/4 pixels a frame, 32–64 steps |
| Temporal artefacts | neighbourhood clamping, wind-aware reprojection, reset on jumps — the list three-clouds publishes as its own limits |

### 6.5 Host configuration

**No switches** (the rule that the new path replaces the old, and that
environment values tune but never toggle):

- **On/off is the presence of a source.** No `CloudSource`, or a source
  with no data at the instant, is an empty field — the identity (§6.7).
- **Quality is a budget**, not a flag: steps, resolution, cascade size and
  temporal share follow the graphics budget's tier for the device, like the
  other GPU resources.
- **Look** is a small set of bounded artistic values recorded in a film's
  digest (as `turbidity` per shot in doc 19): density scale, coverage bias,
  powder strength, octave count. Native hosts read them as tuning values from
  the environment like `AirTuning` (`tuning.rs`), defaulting to the model's
  constants.

| Host | Configuration |
|---|---|
| Viewer (CLI / `ViewerHost` / `FrameHost`) | `--clouds <manifest URL>` or a stated field; the scene clock (#137) drives the valid time |
| `tuile-bevy` | `GlobeSources { …, clouds: Option<CloudSpec> }` with `CloudSpec::{Manifest(Url), Stated(StatedClouds), Source(Arc<dyn CloudSource>)}`; quality from `TuileBudget` |
| Film | the plan names the cloud archive (key + digest) and the look; `Look::clouds: Option<CloudLook>` beside `Look::atmosphere` |

### 6.6 The graphics budget

Cloud tiles are a residency class next to imagery: their bytes counted,
`holds()` as the presence oracle, requests ordered by the cascades' needs
(the camera's neighbourhood at the two bracketing valid times first). GPU
resources (§3.8) are fixed per tier and allocated once. Under memory
pressure the budget shrinks cascade size and target resolution — never
evicts the field the frame is drawing.

### 6.7 The reduction: no clouds is today's picture

| Check | How |
|---|---|
| Empty field ⇒ identity | `T_c = 1`, `C = 0` ⇒ `L = G'` exactly; the shadow factor is exactly 1; the air march's god-ray factor is exactly 1 |
| Film digests | `a_default_films_pictures_are_the_same_to_the_byte` and the atmosphere-on digest (#141 §5.3) **unchanged** with no source and with an empty stated field |
| Viewer | #137's `crates/tuile-wgpu/tests/sky.rs` unchanged |
| Failing first | a stated field of coverage 10⁻³ moves the digest; revert |

What the digests do not cover: the correctness of the clouds themselves —
that is §6.8's job and the opened renders'.

### 6.8 Validation

- **Path-traced reference** (CPU f64 or GPU, in tests and a lab tool): a
  slab, a sphere and a Disney cloud lit by a sun; compare the real-time
  march (octaves, ambient, powder) per view angle — the silver lining
  (towards the sun), the dark base, the back-lit edge. Record the error;
  tune the octave constants against it, not by eye.
- **Physical checks**: visibility inside a stated cumulus (LWC 0.5, r_e 10
  µm) ≈ 50 m; cirrus τ < 1 lets the sun's disc through; shadows on the
  ground move at the wind's speed.
- **Photographs** of the acceptance scenes (pilots' photos of Pyrenean
  cumulus fields, sea-of-clouds webcams) as the eye's reference.
- **Every render opened** (`CLAUDE.md`): each acceptance scene is a film
  in `videos/` with its parameters in `videos/README.md`.

---

## 7. PR plan

Each PR keeps the no-cloud digests unchanged (§6.7) and says so in its test
plan; each is reviewed before merge; each lives in its own worktree and
branch, stacked where they depend.

| PR | Content | Proof / acceptance |
|---|---|---|
| **0** | this study | review |
| **1 — data** | `tuile-clouds`: `CloudSource`, `CloudManifest`, the tile format (encode/decode), `StatedCloudSource`, `TemplateCloudSource`, time interpolation with advection (f64), `lower::GriddedFields` (incl. LCL base); wasm check | round-trip and fixture tests; a small fixture of lowered fields over the Pyrenees committed (from an openly licensed model run, attribution in `fixtures/`); advection test: a stated field shifted by `u·Δt` exactly |
| **2 — medium** | `tuile-atmosphere::clouds`: extinction from water content, type profiles, double-HG and tabulated phase, octaves, ambient by height, light march; CPU f64 reference marcher; Monte-Carlo path tracer of a 3D medium (shared with #141 §5.7); noise generation on an established crate, digest-tested | per-view-angle error vs. the path tracer recorded; noise bytes identical native/wasm |
| **3 — film, clouds** | `tuile-film-gpu`: cascades, march (compute, full res, no history), compose with aerial perspective (§2.6); `Look::clouds` | **Scene A — a cumulus field over the Pyrenees** at a stated forecast time (fixture of PR 1), camera at 2 500 m looking along the range at 15:00 local: flat bases at the lowered LCL, silver linings towards the sun, haze in front of far clouds. Film opened. No-cloud digests unchanged |
| **4 — shadows** | cloud shadow map; ground sun term; overcast ambient; god rays in the air march; overlays shaded | **Scene B — cloud shadows on terrain**: the same field, sun at 30°, shadows moving with the wind over slopes; low sun through gaps for rays. Film opened; ground never below the floor (test) |
| **5 — viewer** | `tuile-wgpu` pass (compute), history with wind-aware reprojection, reset on jumps; viewer and `tuile-bevy` configuration; a post-opaque hook for Bevy | Scene A interactive at ≥ 60 fps on the reference desktop, cost measured; a host entity inside a cloud fogged correctly |
| **6 — WebGL2 and mobile** | fragment passes; orbit-limit fallback without float targets; texture-unit test extended | Scene A in a WebGL2 browser; cost measured on a phone |
| **7 — in the cloud, and planet scale** | near-field detail and steps for flying through; filtered LOD to the orbit limit; cirrus 2D layer; sea of clouds against terrain | **Scene C — flying through a cumulus base**: a replayed climb from 200 m under the base into the cloud and out of its top (visibility ≈ 50 m inside, no banding, no popping). **Scene D — a sea of clouds**: valley stratus at 1 200 m with summits through it at sunrise. **Scene E — orbit**: a continent's weather from 400 km, then a descent to Scene A without a seam |
| **8 — richer data** | the volume form of the tile; satellite-observed coverage taking precedence at observation times | a past flight replayed with satellite coverage; towering cumulus from per-level profiles |
| 9 (optional) | path-traced clouds for prestige film shots | side by side with PR 3's march, opened |

PR 1 and PR 2 do not depend on #137; PR 3 and PR 4 do not either (film
path); PR 5 and PR 6 sit on #137. If #141's N-lights PR lands first, the
cloud march loops over its lights; if not, it takes the sun and the Moon as
`air_step` does, and the loop comes with that PR.

---

## 8. What tuile needs from the data provider

The minimum, to draw Scene A:

1. **A manifest**: valid times, run time, model name and attribution text,
   bounds, tiling levels.
2. **Per valid time, per layer** (low / middle / convective; plus high for
   cirrus): coverage fraction, base and top heights (metres above the
   ellipsoid, or above sea level with the geoid stated), a type or the
   convective/stratiform share, and the wind at the layer.
3. **Hourly steps** (or finer) over the period of the flight or film, and
   the two steps bracketing every instant shown.

Better, when available:

4. Liquid and ice water content per level (`QC`, `QI`), or per-level cloud
   fraction (`CLC`) on height levels — the volume form.
5. 2 m temperature and dew point, to place cumulus bases at the LCL where
   the model's convective base is missing or coarse.
6. For past flights: satellite cloud mask, cloud type and cloud-top height
   at the flight's time.

Either as tiles in tuile's format (§5.3), served from PMTiles or a URL
template — the provider runs tuile's lowering library on its own fields —
or as a `CloudSource` implementation of its own. The provider owns the
choice of models, the licences and attributions, and the archive of past
runs; tuile owns everything from the fields to the pixel.

---

## 9. Out of scope

- Precipitation (rain shafts, virga, snow), lightning, wet ground.
- Clouds feeding back into the air's multiple-scattering table (a cloudy
  sky's colour beyond the overcast ambient of §2.5).
- Ice halos (22°, parhelia) and the glory beyond what a tabulated phase gives.
- Fluid simulation of cloud growth; tuile animates fields by advection and
  noise evolution, not by simulation.
- Volumetric clouds in the OpenUSD/Cycles path (doc 19's budget).
- Fog with local lights (froxels, §3.4).

---

## 10. Summary

- **Physics**: σ_t ≈ 3·LWC/(2ρ_w r_e), ~0.01–0.1 m⁻¹, grey, albedo ≈ 1;
  a forward-peaked phase (double HG; Mie for the film) gives the silver
  lining; multiple scattering by octaves, validated against a path tracer;
  clouds lit by the air's existing transmittance table and sky harmonic;
  an exact composition with the air at the cloud's mean depth.
- **Technique**: the weather map + profile + noise ray march common to every
  engine since 2015, temporal in the viewer, history-free in the film;
  shadows and god rays from one cloud shadow map; LOD as filtering, from in
  the cloud to orbit, with no switch; WebGL2 by fragment passes, proved
  feasible by an open-source geospatial renderer.
- **Data**: NWP gives coverage, base, top, water and wind per column — enough
  for plausible clouds, not the actual ones; satellite products add where
  they were. tuile lowers fields to layered tiles and interpolates in time
  with advection; the provider supplies fields through a `CloudSource`.
- **Budget**: ~47 MiB and 1–2 ms on desktop, ~14 MiB and 3–5 ms on mobile and
  WebGL2 (estimates to measure); tens of ms a frame in the film.
- **Plan**: data crate → medium and reference → film clouds (Pyrenees cumulus)
  → shadows → viewer → WebGL2 → in-cloud and orbit → richer data, each with no
  change to the no-cloud digests and each scene's film opened.

---

## Sources

**Microphysics and optics**
- Stephens 1978, *Radiation profiles in extended water clouds. II*, J. Atmos. Sci. — [doi:10.1175/1520-0469(1978)035<2123:RPIEWC>2.0.CO;2](https://doi.org/10.1175/1520-0469(1978)035%3C2123:RPIEWC%3E2.0.CO;2)
- Hansen & Travis 1974, *Light scattering in planetary atmospheres*, Space Sci. Rev. — [doi:10.1007/BF00168069](https://doi.org/10.1007/BF00168069)
- Fu 1996, *An accurate parameterization of the solar radiative properties of cirrus clouds*, J. Climate — [doi:10.1175/1520-0442(1996)009<2058:AAPOTS>2.0.CO;2](https://doi.org/10.1175/1520-0442(1996)009%3C2058:AAPOTS%3E2.0.CO;2)
- Bohren & Huffman 1983, *Absorption and Scattering of Light by Small Particles*, Wiley; P. Laven, MiePlot — [philiplaven.com/mieplot.htm](http://www.philiplaven.com/mieplot.htm)
- Jendersie & d'Eon 2023, *An Approximate Mie Scattering Function for Fog and Cloud Rendering*, SIGGRAPH Talks — [research.nvidia.com/labs/rtr/approximate-mie](https://research.nvidia.com/labs/rtr/approximate-mie), [supplemental](https://research.nvidia.com/labs/rtr/approximate-mie/publications/approximate-mie-supplemental.pdf)
- WMO International Cloud Atlas — [cloudatlas.wmo.int](https://cloudatlas.wmo.int/)
- Lawrence 2005, *The relationship between relative humidity and the dewpoint temperature*, BAMS — [doi:10.1175/BAMS-86-2-225](https://doi.org/10.1175/BAMS-86-2-225); Romps 2017, *Exact expression for the lifting condensation level*, J. Atmos. Sci. — [doi:10.1175/JAS-D-17-0102.1](https://doi.org/10.1175/JAS-D-17-0102.1)

**Rendering**
- Schneider & Vos 2015, *The Real-time Volumetric Cloudscapes of Horizon Zero Dawn*, SIGGRAPH Advances — [guerrilla-games.com](https://www.guerrilla-games.com/read/the-real-time-volumetric-cloudscapes-of-horizon-zero-dawn), [slides](https://www.advances.realtimerendering.com/s2015/The%20Real-time%20Volumetric%20Cloudscapes%20of%20Horizon%20-%20Zero%20Dawn%20-%20ARTR.pdf)
- Schneider 2017, *Nubis: Authoring Real-Time Volumetric Cloudscapes with the Decima Engine* — [guerrilla-games.com](https://www.guerrilla-games.com/read/nubis-authoring-real-time-volumetric-cloudscapes-with-the-decima-engine), [slides](https://advances.realtimerendering.com/s2017/Nubis%20-%20Authoring%20Realtime%20Volumetric%20Cloudscapes%20with%20the%20Decima%20Engine%20-%20Final%20.pdf)
- Schneider 2022, *Nubis, Evolved* — [guerrilla-games.com](https://www.guerrilla-games.com/read/nubis-evolved), [slides](https://advances.realtimerendering.com/s2022/SIGGRAPH2022-Advances-NubisEvolved-NoVideos.pdf)
- Schneider 2023, *Nubis, Cubed: Methods (and madness) to model and render immersive real-time voxel-based clouds* — [guerrilla-games.com](https://www.guerrilla-games.com/read/nubis-cubed), [course](https://advances.realtimerendering.com/s2023/), [slides](https://advances.realtimerendering.com/s2023/Nubis%20Cubed%20(Advances%202023).pdf)
- Hillaire 2016, *Physically Based Sky, Atmosphere and Cloud Rendering in Frostbite*, SIGGRAPH PBS course — [ea.com](https://www.ea.com/frostbite/news/physically-based-sky-atmosphere-and-cloud-rendering), [PDF](https://media.contentapi.ea.com/content/dam/eacom/frostbite/files/s2016_pbs_frostbite_sky_clouds.pdf)
- Hillaire 2015, *Physically Based and Unified Volumetric Rendering in Frostbite* — [ea.com](https://www.ea.com/news/physically-based-unified-volumetric-rendering-in-frostbite)
- Hillaire 2020, *A Scalable and Production Ready Sky and Atmosphere Rendering Technique*, EGSR — [PDF](https://sebh.github.io/publications/egsr2020.pdf)
- Wronski 2014, *Volumetric Fog*, SIGGRAPH Advances — [advances.realtimerendering.com/s2014](https://advances.realtimerendering.com/s2014/index.html)
- Bauer 2019, *Creating the Atmospheric World of Red Dead Redemption 2: A Complete and Integrated Solution*, SIGGRAPH Advances — [advances.realtimerendering.com/s2019](https://advances.realtimerendering.com/s2019/index.htm)
- Unreal Engine, Volumetric Cloud component — [overview](https://dev.epicgames.com/documentation/unreal-engine/volumetric-cloud-component-in-unreal-engine), [properties](https://dev.epicgames.com/documentation/unreal-engine/volumetric-cloud-component-properties-in-unreal-engine)
- Wrenninge, Kulla, Lundqvist 2013, *Oz: The Great and Volumetric*, SIGGRAPH Talks — [SIGGRAPH history](https://history.siggraph.org/learning/oz-the-great-and-volumetric-by-wrenninge-kulla-and-lundqvist); Wrenninge 2015, *Art-Directable Multiple Volumetric Scattering* — [SIGGRAPH history](https://history.siggraph.org/learning/art-directable-multiple-volumetric-scattering-by-wrenninge)
- Bouthors, Neyret, Max, Bruneton, Crassin 2008, *Interactive multiple anisotropic scattering in clouds*, I3D — [HAL](https://inria.hal.science/inria-00333007)
- Kallweit, Müller, McWilliams, Gross, Novák 2017, *Deep Scattering*, SIGGRAPH Asia — [doi:10.1145/3130800.3130880](https://doi.org/10.1145/3130800.3130880)
- Novák, Georgiev, Hanika, Jarosz 2018, *Monte Carlo Methods for Volumetric Light Transport Simulation*, EG STAR — [project](https://cs.dartmouth.edu/~wjarosz/publications/novak18monte.html)
- Worley 1996, *A Cellular Texture Basis Function*, SIGGRAPH — [doi:10.1145/237170.237267](https://doi.org/10.1145/237170.237267)
- three-geospatial / three-clouds (MIT) — [README](https://cdn.jsdelivr.net/npm/@takram/three-clouds@0.7.4/README.md), [project](https://www.takram.com/projects/three-geospatial)
- Flight-simulator live weather, press account — [PC Gamer](https://www.pcgamer.com/uk/heres-how-microsoft-flight-simulator-creates-its-realistic-weather) (no official technical source found)
- Walt Disney Animation Studios, Cloud Data Set (CC BY-SA 3.0) — [disneyanimation.com/resources/clouds](https://disneyanimation.com/resources/clouds/)

**Data**
- Météo-France AROME open data (Licence Ouverte 2.0) — [data.gouv.fr](https://data.gouv.fr/en/datasets/donnees-du-modele-atmospherique-arome-a-aire-limitee-a-haute-resolution), [0.01° packages](https://data.gouv.fr/es/datasets/paquets-arome-resolution-0-01deg)
- DWD ICON open data — [opendata.dwd.de/weather/nwp](https://opendata.dwd.de/weather/nwp/); ICON database reference — [dwd.de](https://www.dwd.de/DWD/forschung/nwv/fepub/icon_database_main.pdf)
- ECMWF open data (CC BY 4.0) — [ecmwf.int](https://www.ecmwf.int/en/forecasts/datasets/open-data)
- NOAA GFS — [NOMADS](https://nomads.ncep.noaa.gov/), [AWS open data](https://registry.opendata.aws/noaa-gfs-bdp-pds/); HRRR — [rapidrefresh.noaa.gov/hrrr](https://rapidrefresh.noaa.gov/hrrr/)
- Copernicus ERA5 — [cds.climate.copernicus.eu](https://cds.climate.copernicus.eu/)
- EUMETSAT Meteosat Third Generation — [eumetsat.int](https://www.eumetsat.int/meteosat-third-generation); NWC SAF cloud products — [nwcsaf.org](https://www.nwcsaf.org/)
- Open-Meteo API — [open-meteo.com/en/docs](https://open-meteo.com/en/docs)
- pysteps (advection nowcasting) — [pysteps.github.io](https://pysteps.github.io/)
- grib-rs (GRIB2 in Rust) — [github.com/noritada/grib-rs](https://github.com/noritada/grib-rs)
