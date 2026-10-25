<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Generalising the atmosphere — other worlds, other suns, other lights

A design study. **Nothing here is a decision and no code changes with it.**
It asks what it would take for `tuile-atmosphere` to draw skies other than
the Earth's: other planets and moons, several suns, stars of other colours
and sizes, exotic lights (black holes, pulsars, nebulae), thinner and
thicker air, rings and eclipses. It costs each step against what runs
today and proposes a PR plan.

**The constraint that shapes everything below.** There is one engine, the
general one, and today's Hillaire model is *one configuration of it* — not a
fast path, not a branch selected when the configuration "fits". With the
Earth configuration (one star, the Moon, Rayleigh + Mie + ozone in
exponential and tent layers, Hillaire's multiple-scattering approximation),
the general engine must produce **the same tables to the bit and the same
pixels to the byte** as today's implementation: the film's goldens and the
viewer do not move. Every generalisation is therefore designed as a
*parameter* whose Earth value makes the generalised arithmetic collapse onto
today's, operation for operation (§5.2), and each is costed as what it adds
to the Earth configuration's frame (§5.6).

Read at `origin/main` (`c9b5a6c1`) and on `feat/viewer-hillaire-atmosphere`
(PR #137, draft, including its work in progress: `Lights`, `Bodies`,
`AtmosphereConfig` in `crates/tuile-wgpu/src/sky.rs`). Neither is edited.

---

## 1. What runs today

| Piece | Where | What it fixes |
|---|---|---|
| The air | `params.rs` `AtmosphereParams` (Copy, f64) | Rayleigh RGB + one scale height; one grey aerosol (scattering, absorption, scale height, Cornette-Shanks *g*, albedo); ozone RGB absorption in a **tent** (centre, half width); thickness; grey ground albedo |
| The body | `params.rs` `AtmosphereFrame` | a sphere osculating the WGS84 ellipsoid under the eye (Gaussian radius) — the air is laid on it |
| The tables | `hillaire.rs` `LutAtmosphere::new` (CPU, **f64**, "seconds even optimised") | transmittance 256×64, multiple scattering 32×32, level irradiance 256×16; a clear day's baked in `today.tables` (252 KB) and held to `new()` by `todays_tables_are_what_the_model_computes` |
| Layout constants | `hillaire.rs:71,73`, `hillaire.wgsl:95,100` | `LUT_RADIUS = 6 360 000 m` and `SUN_ANGULAR_RADIUS = 0.004652 rad` are **compile-time constants**, in Rust and WGSL |
| Per frame | `sky_view.wgsl` (compute, or a render pass for WebGL2 on #137: `SkyViewPass`) | **two** sky-view tables 192×108, one per light (sun, Moon), azimuth *relative* to that light (`cos` of the azimuth: half the sphere, mirrored), 30 steps |
| Per fragment | `hillaire.wgsl` `air_between`, `ground_light` | aerial perspective **marched per fragment** (12 steps under 10 km, 24 above), sun and Moon in the same march (`air_step(…, sun, moon, …)`); direct sun and Moon + sky irradiance (L1 harmonic) on the ground |
| Discs | `seen.wgsl`, `hillaire.wgsl` `moon_seen` | sun disc with linear limb darkening (u = 0.6, grey); Moon with Lommel-Seeliger and earthshine (`EARTHSHINE = 1.5e-4`); glare PSF; procedural hash stars turned by `celestial_to_ecef` |
| Lights (#137) | `sky.rs` `Lights::{Ephemeris{moon}, Orbit{day_length, year_length, axial_tilt, phase}, Fixed{to_sun, to_moon}}` → `Bodies { to_sun, moon: Option<Moonlight>, stars: Option<DMat3> }` | one sun, at most one Moon; #137's own doc says "the tables are laid out for an Earth-sized planet — a planet of another radius is not offered" |
| Shadows | `resolve.wgsl` | one shadow map, **the sun only** |
| Units | everywhere | a radiance is per unit of **the sun's irradiance outside the air**; `atmosphere.illuminance` maps it to the renderer's units |

So the engine is already a two-light Hillaire (sun + reflected Moon),
spatially exact about the body's centre, with the tables computed on the
CPU in f64. That is the base the generalisation extends.

---

## 2. Hillaire 2020's assumptions, and which scenario breaks which

Hillaire's model ([paper](https://sebh.github.io/publications/egsr2020.pdf),
building on Bruneton & Neyret 2008,
[HAL](https://inria.hal.science/inria-00288758), and Bruneton 2017,
[code](https://github.com/ebruneton/precomputed_atmospheric_scattering)):

| # | Assumption | Why it matters |
|---|---|---|
| A1 | **One spherical body**, atmosphere a spherically symmetric shell | every table is indexed by height and zenith cosines only |
| A2 | **One dominant directional light**, a distant small disc | sky-view azimuth is relative to it; the light's colour factors out of every table (they are *per unit of light*) |
| A3 | Media = **Rayleigh + Mie + an absorber (ozone)**, three channels | phase functions fixed per component; absorption only via the extinction |
| A4 | Densities **depend on height only**, exponential (+ tent for ozone) | 1D profile; no clouds, no horizontal structure, no time variation |
| A5 | **Multiple scattering** of order ≥ 2 approximated: isotropic phase for orders ≥ 2, second-order luminance and transfer factor *f_ms*, infinite series summed as 1/(1 − f_ms) | cheap 32×32 table; validated against a path-traced reference for Earth-like air, degrading as the medium gets optically thick |
| A6 | **RGB** (three wavelengths: 680, 550, 440 nm in Bruneton 2017) | colours of products of spectra are approximated by products of RGB |
| A7 | **Straight rays**, no refraction, no polarisation, no emission | |
| A8 | Ground = **Lambertian, uniform albedo** | single bounce into the multiple-scattering table |

| Scenario | Breaks | How badly |
|---|---|---|
| Other planet, Earth-like air (Mars thin, Titan, a super-Earth) | A1 only in its *constants* (radius, thickness) | none: parameters |
| Moon / airless body | nothing (degenerate: zero medium) | trivial, but the night side then has only secondary lights (§3.5) |
| Several moons | A2 (secondary lights) | linear in their count; each is the Moon's case |
| Two or more suns | A2 | the tables that are per unit of light are **shared**; only sky-view and per-fragment terms grow with N (§3.3) |
| Red / white / brown dwarf | A6 (spectrum far from the Sun's), A2 (disc size) | RGB error grows with how far the spectrum is from the Sun's (§4.6) |
| Eclipses, ring shadows | A1, A2 (occluders between light and air) | an extra visibility factor per light, per point (§3.4) |
| Thick air (Venus, Titan's haze) | A5, and A7 (refraction) | the multiple-scattering approximation is the weak point (§4.4) |
| Dusty air (Mars), coloured haze | A3 (grey, one *g*) | RGB albedo and per-channel phase are needed (§4.2) |
| Clouds, dust storms | A4 | out of scope (§6) |
| Black hole, accretion disc | A7 (curved rays), A2 (extended emitter) | not an atmosphere problem: a background and a light (§3.6) |
| Pulsar | A2 (time-varying intensity) | trivial for the air, a determinism question (§3.7) |
| Nebula / galactic background | A2 (extended, faint emitter) | an emissive background and an ambient term (§3.8) |

The table says something structural: **most of the generality is in the
lights, not in the air.** The medium generalisations are parameters of the
same tables; the lights are where the arithmetic multiplies.

---

## 3. Light sources

### 3.1 A star

| Property | Model | Earth value (reduction) |
|---|---|---|
| Spectrum | blackbody of `T_eff`, or a tabulated spectrum, integrated against the CIE 1931 functions → linear sRGB, **normalised to the Sun's** | the Sun: RGB `[1, 1, 1]` exactly — stated, not computed (§5.2) |
| Irradiance at the body | `L / (4π d²)`, `d` from the orbit; expressed in units of the Sun's irradiance at 1 AU | 1 (the current unit) |
| Angular radius | `asin(R★ / d)` | `0.004652` exactly (today's constant) |
| Limb darkening | linear `1 − u(1 − μ)` (today: u = 0.6, grey), or quadratic `u1, u2` per channel | linear, `u = 0.6` grey |
| Variability | `Steady`, `Periodic { period, profile, phase }`, `Flares { seed, rate }` | `Steady` |

Blackbody colours relative to the Sun's (5772 K), linear sRGB, green = 1
(computed for this study with the Wyman et al. 2013 analytic CIE fit,
[JCGT](https://jcgt.org/published/0002/02/01/)):

| `T_eff` | Example | RGB / Sun's |
|---|---|---|
| 2600 K | late M dwarf (TRAPPIST-1-like) | 2.19, 1, 0.22 |
| 3050 K | Proxima Centauri | 1.78, 1, 0.36 |
| 4450 K | Kepler-16 A (K dwarf) | 1.21, 1, 0.72 |
| 5772 K | the Sun | 1, 1, 1 |
| 10 000 K | A star, warm white dwarf | 0.77, 1, 1.54 |
| 30 000 K | hot white dwarf, O/B star | 0.64, 1, 2.15 |

Real M-dwarf spectra are far from blackbodies (TiO and other molecular
bands); a tabulated spectrum (e.g. a PHOENIX model,
[Husser et al. 2013](https://phoenix.astro.physik.uni-goettingen.de/)) is
the honest input when it matters. Brown dwarfs (T < 2000 K) emit mostly in
the infrared: in the visible they are dim and deep red — a light of tiny
irradiance, not a different model. A white dwarf is a point-like hot star:
small disc, blue-white, the same model.

### 3.2 How a light enters the lighting

Today one light is *the* unit and the Moon is a `weight` against it
(`Light { rayleigh, mie, weight }`, `source(...) * l.weight`). The
generalisation is to make every light a weight **and a colour**:
`weight_rgb = irradiance × colour × visibility`. For the Earth's sun this
is `[1, 1, 1]` and the product is an exact identity in IEEE arithmetic
(`x × 1 = x`), which is what makes the reduction free (§5.2).

### 3.3 How the tables scale with N lights

| Table | Depends on | With N lights |
|---|---|---|
| Transmittance | medium, body | **shared**, unchanged |
| Multiple scattering (Ψ_ms) | medium, body, ground albedo — per unit of light | **shared**: linear in the light, colour multiplies afterwards (exact in RGB; approximate for spectra, §4.6) |
| Level irradiance | medium, body — per unit of light | **shared** |
| Sky-view | medium, eye height, **the light's zenith angle**, azimuth relative to it | **one per light** (a texture array layer each), or one combined table in absolute azimuth |
| Aerial perspective (per fragment) | everything | the march samples the medium once per step; each light adds two table reads and two phase evaluations per step |
| Shadow map | light direction | one per shadowing light |

Per-light sky-view layers versus one combined table:

| | Per-light layers | Combined, absolute azimuth |
|---|---|---|
| Reduction to today | **exact**: layer 0 is today's `sky_view`, layer 1 today's `moon_view` | breaks it (different parametrisation, Mie peaks between texels) |
| Cost per frame | N × (192×108 × 30 steps) | 1 × full-azimuth table, march shared, + N lookups per step |
| Memory | N × 192×108 × 16 B ≈ N × 330 KB | 2× one table (full azimuth) |
| Mie halo | centred on the light (c = 1 row), sharp | at ~1.9°/texel wherever the light falls |

**Recommendation: per-light layers**, because the Earth configuration
reduces to today's two tables and the cost is linear and small (§5.6). The
*count* of lights that get a layer, a shadow map and a place in the
per-fragment march is a budget `K` per backend (§5.5). Lights past the K
brightest are not dropped: they enter only through the shared irradiance
table (ground light) and a disc — never a hole, never a switch: a rule on
ranks, and the Earth's two lights are always within it.

### 3.4 Eclipses and occlusion

- **Disc–disc overlap** (a moon in front of a star): visible fraction from
  the area of two intersecting circles (closed form, a few `acos`), weighted
  by limb darkening if wanted. Per light, per frame for the sky; **per
  fragment** for the ground, because an umbra is local (≈ 100–270 km on the
  Earth). Cost: a handful of ALU per occluder per light.
- **Mutual eclipses of stars** (an eclipsing binary): the same overlap,
  star on star.
- **Ring shadow**: intersect the ray to the light with the ring plane; read
  the ring's radial optical depth (a 1D texture); multiply by
  `exp(−τ / |μ_ring|)`. Per fragment, per light.
- **The planet's own shadow on its rings and moons**: the same sphere test
  the sun's visibility already does (`sun_visible`).
- **The Earth's shadow on the Moon** (lunar eclipse): the umbra is red from
  light refracted through the limb (A7); the cheap model is a reddened
  umbra colour taken from the transmittance table at grazing incidence.

With no occluders the loops are empty and the visibility is exactly 1:
the Earth configuration today has no eclipse model, so the reduction keeps
it at 1 (an Earth-with-eclipses configuration is a *different*
configuration, and is welcome to differ).

### 3.5 Secondary lights: moonlight, planetshine, ringshine

A moon or a planet is a **reflector**: a light whose colour is its star's
colour × its albedo, whose irradiance is its phase-integrated reflectance
(`moon_irradiance` today, from magnitudes), whose disc is drawn with a
reflectance law (Lommel-Seeliger for dusty bodies, Lambert for clouds). The
Moon is exactly this today. Generalised:

| Secondary | Order of magnitude at full phase, vs the primary star |
|---|---|
| Full Moon from the Earth | ~2.5 × 10⁻⁶ |
| Saturn from Titan (angular diameter ≈ 5.5°) | ~10⁻³ (derived: albedo ~0.5, solid angle ~7 × 10⁻³ sr) |
| Jupiter from Europa (≈ 12°) | a few 10⁻³ |
| Earthshine on the Moon | today's `EARTHSHINE = 1.5e-4` of the full Moon |

Large planets in the sky are no longer "small discs": their angular radius
enters the irradiance (a lit sphere's integral) and their disc must be
drawn with its terminator. On an **airless body** these are the only night
light, together with stars; the existing `least` floor (no black ground,
ever — `CLAUDE.md`) stays as the floor under them.

### 3.6 Black holes

Not an atmosphere problem but a background-and-light one; three levels:

| Level | Technique | Cost | Reference |
|---|---|---|---|
| Stylised | screen-space distortion of the background around the hole's projected position: deflection `α ≈ 4GM/(c²b)` (weak field), clamped near the photon ring (`b = 3√3 GM/c²`), a black disc inside the shadow | one full-screen pass, ~20 ALU/pixel | — |
| Precomputed Schwarzschild | the deflection as a 1D table of the impact parameter (geodesics integrated once), lookups per sky pixel; accretion disc intersections tabulated too; filtered stars | real time in WebGL2 | Bruneton 2020, [arXiv:2010.08735](https://arxiv.org/abs/2010.08735), [shader](https://github.com/ebruneton/black_hole_shader) |
| Film | Kerr geodesics, ray bundles for filtering, Doppler and gravitational shifts | offline | James, von Tunzelmann, Franklin, Thorne 2015 (DNGR, *Interstellar*), [arXiv:1502.03808](https://arxiv.org/abs/1502.03808) |

The **accretion disc** is an emitter: temperature `T(r) ∝ r^(−3/4)` (thin
disc, Shakura & Sunyaev 1973, [ADS](https://ui.adsabs.harvard.edu/abs/1973A%26A....24..337S)),
blackbody colours per radius, a stylised Doppler brightening of the
approaching side. As a light on the planet it is an *extended* source:
approximate it by 2–4 directional lights (its brightest arcs) or by its
irradiance projected on an L1 harmonic, both of which the light model above
already takes. Lensing of the light *reaching the planet* is out of scope.

### 3.7 Pulsars

A pulsar is a point light with `Periodic { period, profile, phase }`. Two
facts shape it:

- **Real periods are faster than frames**: PSR B1257+12 (the first
  planets found outside the Solar System,
  [Wolszczan & Frail 1992](https://www.nature.com/articles/355145a0)) turns
  in 6.2 ms — 160 flashes per second. Sampled at 60 fps it aliases. The
  physical choice is the **time average** over the frame's exposure (the
  profile's integral over `[t, t + shutter]`, closed form for a box or
  Gaussian profile); a stylised choice slows the beam down for the eye.
- Its optical output is tiny; its planets are lit mostly by X-rays and the
  pulsar wind. A "pulsar world" with a visible sky light is stylised by
  construction.

The beam itself (a rotating cone sweeping across the sky) is a background
effect: two cones of emissive glow in the background pass, phase from the
clock. **Determinism**: `phase = frac((t − t₀) / P)` computed in f64 from the
scene clock, never accumulated frame to frame.

### 3.8 Nebulae and galactic background

An emissive background: a cubemap or HEALPix map (e.g. NASA SVS Deep Star
Maps, [svs.gsfc.nasa.gov/4851](https://svs.gsfc.nasa.gov/4851); ESA Gaia's
sky map), seen through the air's transmittance like today's hash stars, and
optionally a tiny constant ambient (its irradiance on an L1 harmonic). The
hash stars stay the Earth configuration's background.

---

## 4. Media

### 4.1 Gas mixtures

Rayleigh scattering per species is
`σ_R(λ) = 24π³ / (λ⁴ N_s²) · ((n_s² − 1)/(n_s² + 2))² · F_K(λ)` — at a fixed
wavelength it scales with the **number density** and the species'
refractivity. All molecules share the Rayleigh phase, so a mixture is one
Rayleigh component with summed coefficients, provided all species share a
density profile (well-mixed below the homopause — true on Mars, Venus,
Titan, the Earth). Cross-sections: N₂ ≈ air; CO₂ ≈ 2.4–2.6 × air in the
visible (Sneep & Ubachs 2005,
[doi:10.1016/j.jqsrt.2004.07.025](https://doi.org/10.1016/j.jqsrt.2004.07.025)).

Absorption bands become per-channel absorption coefficients with their own
profile — ozone's Chappuis band today; **methane** on Titan and the giants
(bands at 619, 727, 890 nm: absorbs red, a blue-green tint); the unknown
UV/blue absorber in Venus's clouds (yellow). A band narrower than a channel
is averaged over that channel's weight function — an approximation that
gets worse the narrower the band (§4.6).

### 4.2 Aerosols and dust

Today one grey aerosol with one `g`. Real cases need:

| Need | Why | Extension |
|---|---|---|
| RGB extinction and albedo | Mars dust absorbs blue (ω ≈ 0.97 red → lower in blue), Titan's haze absorbs blue strongly | `extinction: [f64; 3]`, `albedo: [f64; 3]` |
| Per-channel phase | Mars' blue sunset comes from dust's forward peak being stronger in blue | `g: [f64; 3]` (Cornette-Shanks per channel) |
| Two lobes | haze and dust have a back-scatter lobe | double Henyey-Greenstein `(g₁, g₂, w)` |
| Several aerosol layers | Titan's detached haze, Venus's cloud deck at 45–70 km | a list of aerosol components, each with a profile |

Schneegans et al. 2024 ([CGF](https://diglib.eg.org/handle/10.1111/cgf15010),
[PDF](https://cgvr.cs.uni-bremen.de/papers/cgf24/v43i2_15_15010.pdf)) show
that Mars' blue sunset needs Mie-theory phase functions rather than an
Earth-derived approximation; they precompute phase tables per wavelength.
The per-channel Cornette-Shanks is the cheap end of that spectrum; a
tabulated phase (a 1D texture per aerosol, 3 channels) is the general end.

### 4.3 Density profiles

Generalise A4 as a sum of **typed terms** per component, each a kind the
shader knows: `Exponential { scale_height }` (today's Rayleigh and Mie,
evaluated as `exp(−h × (1/H))` exactly as `medium()` does),
`Tent { centre, half_width }` (today's ozone, `max(1 − |h − c| / w, 0)` —
the division kept, not a reciprocal, so the Earth's bits are kept),
`Layer { bottom, top, exponential within }` (a haze or cloud deck), and
`Tabulated` (a 1D texture of 64–256 heights, for a non-isothermal
atmosphere such as Venus's, where the scale height falls from ~16 km at the
surface to ~5 km near the clouds). Bruneton 2017's `DensityProfile` (two
layers of `a·exp(b·h) + c·h + d`) is the reference; it is *not* adopted
literally because its operations do not reproduce today's ozone tent to the
bit.

Gravity enters only through scale heights: `H = k T / (m g)` (isothermal);
a preset helper derives them from `T`, mean molecular mass and `g`. A
low-gravity world (Titan, g = 1.35 m/s²) has a tall atmosphere relative to
its radius (thickness ≈ 600 km over 2 575 km): the transmittance table's
parametrisation (Bruneton's `ρ / H` mapping) handles it, as long as the
**body radius stops being a constant** (`LUT_RADIUS`).

### 4.4 Thin, thick: optical-depth regimes

Vertical optical depth τ (green, zenith):

| Regime | τ | Examples | What dominates | Model consequence |
|---|---|---|---|---|
| None | 0 | Moon, Mercury | direct light only | black sky, stars by day; tables trivial |
| Thin | ≲ 0.1 | Mars molecules (≈ 0.003), Pluto | single scattering | Hillaire is overkill but exact |
| Earth-like | 0.1–1 | Earth (0.1 Rayleigh + aerosols), Mars with dust (0.3–1) | single + a significant second order | Hillaire's target |
| Hazy | 1–5 | Titan (haze), Mars dust storms (up to ~5+), polluted Earth | multiple scattering | A5 under strain: validate against a reference (§5.7); a few % to tens of % low |
| Opaque | ≫ 5 | Venus (clouds τ ≈ 25–40, Rayleigh ≈ 20 in green below them) | diffusion | under the deck the sky is a near-uniform dome, the sun invisible; Hillaire's series is the wrong tool |

For the opaque regime the honest model is a **two-stream / diffusion
solution** precomputed per height (a 1D table of up- and down-welling
irradiance), drawn as a near-isotropic sky below the deck and as Hillaire
above it. That is a new *term* in the general engine, whose contribution is
zero for media whose optical depth is under its range — again a derived
weight, not a switch. It is the most expensive item of the study to get
right and the least needed; it closes the plan (§5.8, PR 7).

Refraction (A7) matters for thick air: on Venus the surface is close to
"super-refraction" — the horizon appears to rise. Out of scope beyond a
mention.

### 4.5 Body radius, thickness, ground

`Body { radius, thickness, ground_albedo: [f64; 3] }`; an ellipsoid stays a
host's affair, as `AtmosphereFrame` does today (an osculating sphere under
the eye). The ground albedo goes RGB: a red Martian ground reddens the
multiple scattering it feeds. Ocean: a uniform albedo is all the tables
take; glitter is the surface shader's business.

### 4.6 Spectral versus RGB

| Approach | Accuracy | Cost |
|---|---|---|
| RGB (today) | exact for linear terms (single scattering × light colour); **error in transmittance of non-white light** through coloured media (e^(−τ(λ)) integrated vs at three λ), largest for red dwarfs and narrow absorption bands | 1× |
| RGB with *effective* coefficients per light (coefficients pre-integrated against the star's spectrum) | better for one dominant non-solar star; per-light coefficients break the shared transmittance table | + one transmittance table per distinct spectrum |
| 4–8 spectral bins, converted to XYZ at the end | good for sunsets, dwarfs, bands | 1.3–2.7× table memory and march ALU; two RGBA textures per table |
| Full spectral (≥ 15 bins, Bruneton 2017 demo) | reference | offline only |

Elek & Kmoch 2010 discuss real-time spectral scattering trade-offs
([PDF](https://cgg.mff.cuni.cz/~tomas/publications/pub-cescg-real-time-scattering/paper.pdf)).
**Recommendation**: RGB in the engine, spectral only at *configuration
lowering* (blackbody → RGB, cross-sections → three wavelengths), with a CPU
spectral reference in tests to measure the RGB error per preset (ΔE). The
Earth preset passes today's RGB constants verbatim — never recomputed from
a spectrum, which would move their last bits. An N-bin engine is a later,
separate decision, after the error has been measured.

---

## 5. Presets, and the shape of the engine

Values at the surface unless stated; RGB = 680/550/440 nm, per metre. Derived
values (marked †) are computed for this study from the cited sources with the
formulas above and need checking against a radiative-transfer reference
before shipping.

### 5.1 Table

| Preset | Body radius, g | Air: β_R (×10⁻⁶) | H | Aerosols | Star(s) | Sources |
|---|---|---|---|---|---|---|
| **Earth** (today) | 6 360 km (table), 9.81 | 5.802, 13.558, 33.1 | 8 km | grey, τ from turbidity 2, H 1.2 km, g 0.8, ω 0.9 | Sun: 1 at 1 AU, 0.2665° radius | `params.rs`; Hillaire 2020; Bruneton 2017 |
| **Mars** | 3 389.5 km, 3.71 | 0.12, 0.27, 0.67 † (CO₂ at 610 Pa, 210 K, ×2.45 air) | 11.1 km | dust τ 0.3–0.9 (typical), H ≈ 11 km, r_eff ≈ 1.5 µm, g ≈ 0.65–0.7, ω red ≈ 0.97, lower in blue | Sun at 1.524 AU: 0.43, 0.175° radius † | [NASA Mars fact sheet](https://nssdc.gsfc.nasa.gov/planetary/factsheet/marsfact.html); Wolff et al. 2009 [doi:10.1029/2009JE003350](https://doi.org/10.1029/2009JE003350); Lemmon et al. 2015 [doi:10.1016/j.icarus.2014.03.029](https://doi.org/10.1016/j.icarus.2014.03.029) |
| **Venus** | 6 051.8 km, 8.87 | 505, 1 180, 2 880 † (CO₂, 92 bar, 737 K) | 15.9 km (surface; tabulated profile) | sulfuric-acid cloud deck 45–70 km, τ ≈ 25–40, ω ≈ 0.999 red/green, UV-blue absorber | Sun at 0.723 AU: 1.91 | [NASA Venus fact sheet](https://nssdc.gsfc.nasa.gov/planetary/factsheet/venusfact.html); Knollenberg & Hunten 1980 [doi:10.1029/JA085iA13p08039](https://doi.org/10.1029/JA085iA13p08039) |
| **Titan** | 2 574.7 km, 1.352 | 25.7, 60.2, 147 † (N₂, 1.47 bar, 94 K) | ~20 km at the surface, larger aloft | organic haze to ~500 km, τ ≈ 3–5 in the visible (more in blue), forward-peaked, blue-absorbing; methane bands at 619/727/890 nm | Sun at 9.58 AU: 0.011, 0.028° radius †; Saturn as a secondary light | Tomasko et al. 2005 [Nature](https://www.nature.com/articles/nature04126); Tomasko et al. 2008 [doi:10.1016/j.pss.2007.11.019](https://doi.org/10.1016/j.pss.2007.11.019); [NASA Saturnian satellites](https://nssdc.gsfc.nasa.gov/planetary/factsheet/saturniansatfact.html) |
| **Airless** (Moon) | 1 737.4 km, 1.62 | 0 | — | none | Sun: 1 | `moon.rs` |
| **Red-dwarf world** (Proxima b-like) | ~1.1 R⊕ (assumed rocky, Earth-like air) | Earth's | 8 km | Earth's | Proxima: 3 050 K (RGB 1.78, 1, 0.36), 0.65 S⊕, **0.85° radius** (≈ 3.2× the Sun's) †; flares; tidally locked (fixed star in the sky) | Anglada-Escudé et al. 2016 [Nature](https://www.nature.com/articles/nature19106) |
| **Binary-star world** (Kepler-16 b-like, "Tatooine") | assumed Earth-like | Earth's | 8 km | Earth's | A: 4 450 K, 0.649 R☉; B: 3 311 K, 0.226 R☉; at 0.705 AU: A 0.30 S⊕, 0.25° radius; B 0.011 S⊕ (3.7 % of A), 0.085° radius †; binary period 41 d, mutual eclipses | Doyle et al. 2011 [arXiv:1109.3432](https://arxiv.org/abs/1109.3432) |
| **Pulsar world** (stylised, PSR B1257+12-like) | Earth-sized, thin or no air | — | — | — | pulsar 6.2 ms, point-like, time-averaged; a dim companion star or a nebula background for visible light | Wolszczan & Frail 1992 |
| **Black-hole world** (stylised) | any | any | — | — | accretion disc as 2–4 lights + emissive background; lensed starfield | Bruneton 2020; James et al. 2015 |

A habitable world in a red dwarf's light is dimmer in the blue it scatters:
its sky stays bluer than its star but the whole scene is peach-to-orange,
and the irradiance-weighted Rayleigh colour is where RGB's error is largest
(§4.6) — the first preset to measure against a spectral reference.

### 5.2 The reduction to today, by construction

The Earth preset is **not** "the default `AtmosphereParams`": it is a full
`SkyConfig` whose lowering yields exactly what today's code consumes, and
the general engine's arithmetic, on those values, executes today's
operations:

| Generalisation | Earth value | Why the bits are kept |
|---|---|---|
| Star colour × irradiance × visibility | `[1, 1, 1]` × 1 × 1 | IEEE: `x × 1 = x` exactly |
| Lights loop | `[sun, moon]` in that order | `(0 + a) + b = a + b`: same sum, same order as `source(sun) + source(moon)` |
| Sky-view layers | 2: sun, Moon | layer i is today's table i, same texels |
| Density terms | `Exponential(8 km)`, `Exponential(1.2 km)`, `Tent(25 km, 15 km)` | each kind is evaluated with today's expression, operand for operand |
| Body radius for the tables | `6 360 000` | a uniform holding the constant's value — **see the GPU caveat below** |
| Disc radius | `0.004652` | idem |
| Limb darkening | linear, u = 0.6, grey | `1 − 0.6 (1 − μ)` kept as written |
| Aerosol RGB albedo, g | grey (three equal values) | per-channel evaluation of an equal triple equals the scalar one |
| Occluders, rings, background | none | empty loops; visibility exactly 1 |
| Tables | CPU f64, unchanged code | bit-exact by construction (same Rust operations) |

The **GPU caveat**: replacing a WGSL `const` by a uniform of the same value
lets the shader compiler fold less (e.g. `cos(SUN_ANGULAR_RADIUS)` folded at
compile time versus computed at run time), which can move a last bit on a
given adapter. That is why the plan's first PR (§5.8, PR 1) is a
**constants-to-uniforms refactor whose only acceptance criterion is the
golden digest**: any such drift is caught alone, where it can be fixed (by
uploading precomputed derived values from the CPU — `cos ρ`, the disc's
solid angle — instead of letting the shader compute them).

### 5.3 Testing the reduction

1. **Tables**: `LutAtmosphere::new(lower(SkyConfig::earth()))` byte-equal to
   `today.tables` — the existing `todays_tables_are_what_the_model_computes`,
   run on the lowered configuration.
2. **Lowering**: `lower(SkyConfig::earth()) == (AtmosphereParams::default(),
   today's Bodies)` with `PartialEq` on the f64 values.
3. **Uniform**: the `AtmosphereUniform` bytes of an Earth frame equal to
   today's, field for field (the new fields past today's are zero or one
   as §5.2 says), frozen as a digest before the refactor.
4. **Pixels**: a new film digest **with the atmosphere on** (Earth, a fixed
   instant, sun and Moon up), frozen on `main` before PR 1, alongside the
   existing `a_default_films_pictures_are_the_same_to_the_byte` — scoped to
   the adapter it was frozen on (Apple GPU through Metal), as that test
   already is. Plus the viewer's sky tests (`crates/tuile-wgpu/tests/sky.rs`
   on #137).
5. **Failing first**: change one Earth value by one ulp (the disc radius),
   confirm the pixel digest moves; revert.

What these do not cover: other adapters (digests are per adapter), and the
*correctness* of the non-Earth configurations — that is §5.7's job.

### 5.4 Data model

```rust
pub struct SkyConfig {          // what a host states; plain, Copy where it can be
    pub body: Body,             // the planet the camera stands on
    pub medium: Medium,         // its air
    pub sources: Sources,       // stars, reflectors, exotic emitters
    pub background: Background, // Stars(hash) | Map(handle) | Lensed { .. }
}
pub struct Body { radius: f64, thickness: f64, ground_albedo: [f64; 3], rings: Option<Rings> }
pub struct Medium {
    rayleigh: Component,                 // summed species, one profile
    absorbers: Slots<Absorber, 2>,       // ozone, methane: RGB absorption + profile
    aerosols: Slots<Aerosol, 2>,         // RGB extinction, RGB albedo, Phase, profile
}
pub enum Profile { Exponential { scale_height }, Tent { centre, half_width },
                   Layer { bottom, top, scale_height }, Tabulated(Handle) }
pub enum Phase { CornetteShanks([f64; 3]), DoubleHg { g1, g2, w }, Tabulated(Handle) }
pub struct Star { colour: Spectrum, irradiance: f64, angular_radius: f64,
                  limb: LimbDarkening, variability: Variability, motion: Motion }
pub struct Reflector { body_radius: f64, albedo: [f64; 3], law: Reflectance, motion: Motion, lit_by: StarIndex }
pub enum Motion { Ephemeris, Kepler(Elements), Spin { .. }, Fixed(DVec3) }
```

`Slots<T, N>` is a fixed-capacity array (`[Option<T>; N]`) so the
configuration stays `Copy` like `AtmosphereParams` and `AtmosphereConfig`
are. Species and spectra live in the *description*; the engine only ever
sees the lowered RGB components. Today's `AtmosphereParams` becomes the
lowered form of `Medium` with one aerosol and one absorber; today's
`Lights` variants become `Motion`s of one `Star` and one `Reflector`.

**Fits #137 as extensions** — the one request to make on #137 before it
lands: mark `Lights`, `Bodies`, `AtmosphereConfig` and `AtmosphereParams`
`#[non_exhaustive]` (or document construction through constructors only),
so that adding a `Lights::System(..)` variant, a `Bodies::extra` slot array
or an `AtmosphereParams::body_radius` field is not a breaking change. Then:

| #137 today | Extension |
|---|---|
| `Lights::{Ephemeris, Orbit, Fixed}` | + `Lights::System(SystemConfig)`: N stars and reflectors with `Motion`s; the three existing variants are lowered to a one-star system |
| `Bodies { to_sun, moon, stars }` | + `extra: Slots<LightSeen, K>` (direction, RGB weight, angular radius, visibility) |
| `AtmosphereConfig { params, .. }` | + `body: Body` (default: today's 6 360 km); `params` stays the lowered medium |
| `AtmosphereUniform` (`sun`, `moon`, `moon_disc`, `moon_shade`, `discs`) | + `lights: array<LightGpu, K>` with `count`; `sun`/`moon` become lights 0 and 1 (kept as fields until the refactor PR proves identity) |
| `sky_view` + `moon_view` textures | a `texture_2d_array` of K layers |

### 5.5 Determinism and budgets

**Determinism**: the sky is a pure function of `(SkyConfig, scene instant,
camera)`, as doc 19 §1.6 requires and #137's `SceneClock` provides. Orbits
are closed-form (Kepler's equation by a fixed number of Newton iterations,
never an integrator stepping frame to frame); pulsar phases and flares are
functions of the instant (flares from a seeded hash of the instant's
bucket). The CPU tables are f64 Rust, bit-identical across native targets
for the same operations; transcendental functions come from the platform's
libm, which can differ by an ulp between native and wasm — if a digest must
match across them, the tables use the `libm` crate. GPU pixels are
deterministic per adapter only (as the existing digests already say).
"Same sky on every client" means the same configuration and instant, within
a measured tolerance, not the same bits.

**Budgets** (estimates, to be measured — 192×108 = 20 736 texels):

| Backend | Sky-view per light (30 steps) | Per-fragment march | K (lights with a layer, a march slot) | Table build for a new medium |
|---|---|---|---|---|
| Desktop WebGPU / native, compute | ~0.05–0.1 ms | 12/24 steps × K lights | 4 | GPU compute ~0.1 ms, or baked |
| Web WebGPU | similar | idem | 4 | GPU, or baked presets (CPU would block the page's thread for seconds) |
| WebGL2 (render-pass sky-view, #137) | ~0.1–0.2 ms | idem, fragment | 2 | baked presets only, or fragment-pass builders |
| Mobile | ~0.3–0.5 ms | the dominant cost | 2 | baked presets |

Precompute vs per frame: **per medium** (once, or when the host changes the
air): transmittance, multiple scattering, irradiance — baked per preset
(≈ 252 KB each, as `today.tables`), computed in f64 for any other medium,
or on the GPU once a GPU builder exists and has been held to the f64 one
within a tolerance (the f64 one stays the reference and the Earth bake).
**Per frame**: sky-view layers (only those whose light moved more than a
threshold — today's `sky_view_stale`), light directions, eclipse fractions.
**Per fragment**: aerial perspective, ground light, eclipse/ring visibility.

### 5.6 What each generalisation costs at the Earth configuration

| Generalisation | Added work at Earth's configuration | Keeping it in budget |
|---|---|---|
| Lights array, loop of `count = 2` | loop control instead of two inlined calls; ~0 | unroll to `K` with a uniform-bounded break |
| RGB light weight | 3 multiplications by 1 per light per step | negligible |
| Typed density terms | one uniform-coherent branch on the term's kind per component | no divergence: kind is uniform |
| Uniform body radius, disc radius | a uniform read instead of an immediate | upload derived values (`cos ρ`, solid angle) from the CPU |
| Per-channel aerosol phase | 3 phase evaluations instead of 1 when g differs; with grey g, evaluated once (a uniform test on equality) | idem |
| Occluder / ring loops | empty | — |
| Sky-view texture array | same texels, array-layer addressing | — |
| Uniform size | + K × ~48 B | negligible |
| **Total** | **target: ≤ 3 % of the frame's atmosphere passes, measured** | a bench on the film renderer before/after each PR, beside the digests |

Costs that only non-Earth configurations pay: each extra light ≈ +1 sky-view
table and +2 reads and 2 phases per march step (≈ +35–45 % of the march per
light); each eclipse occluder a few `acos` per fragment; a ring a
plane-intersection and a 1D fetch per fragment per light; a tabulated
profile a 1D fetch per march step; a lensed background a full-screen pass;
spectral bins (if ever) ≈ +30–170 % on everything.

### 5.7 Validating the non-Earth configurations

A CPU **Monte-Carlo path tracer** of the spherical medium (f64, in
`tuile-atmosphere`'s tests, a few hundred lines, no new dependency) gives
the reference against which each preset's tables and sky are checked:
error per optical-depth regime (§4.4), RGB versus spectral (§4.6). Photos
are the other reference: Mars (Curiosity and Perseverance Mastcam sky
images), Titan (Huygens DISR descent images), Venus (Venera 13/14 panoramas).
Collienne et al. 2013 adapted Bruneton 2008 to Mars by matching sky colours
to rover images ([DLR elib](https://elib.dlr.de/86477)); Schneegans et al.
2024 go further with Mie theory.

### 5.8 PR plan

Each PR keeps the Earth digests (§5.3) unchanged and says so in its test
plan; each is reviewed before merge.

| PR | Content | Proof |
|---|---|---|
| 0 | On #137 (by its owner): `#[non_exhaustive]` on the four public types | API only |
| 1 | **Constants to uniforms**: `LUT_RADIUS`, `SUN_ANGULAR_RADIUS`, limb `u` become uniform fields; CPU-side derived values | Earth table bytes, uniform digest, film digest (atmosphere on) frozen before — identical |
| 2 | **N lights**: lights array, sky-view texture array, `Slots` in `Bodies`, RGB weights; sun and Moon as lights 0 and 1; second shadow map out of scope | Earth digests identical; a two-sun test against two one-sun renders summed (linearity) |
| 3 | **Stars as data**: blackbody → RGB (relative to the Sun), irradiance from distance, angular size, quadratic limb darkening; `Lights::System` with Kepler motions; eclipse overlap per light (sky and per fragment) | Kepler-16-like and Proxima-like presets; eclipse fraction tests against closed form |
| 4 | **Media presets**: `Body` radius, RGB albedo/extinction, per-channel `g`, typed profiles; Mars, Titan, airless; baked tables per preset; CPU path-tracer reference | per-preset error vs the reference, opened renders of each preset (the film opened, not a counter) |
| 5 | **Secondary lights and rings**: planetshine with large discs, ring shadows and ringshine | Saturn-from-Titan render |
| 6 | **Exotic**: emissive background maps, pulsar (time-averaged periodic light), stylised lensing, accretion disc as lights; then Bruneton 2020's precomputed lensing | stylised presets, rendered and opened |
| 7 | **Opaque regime**: two-stream term for τ ≫ 5 (Venus below the deck), only if wanted | Venus render vs Venera panorama colours |

A GPU table builder (compute + WebGL2 fragment fallback), held to the f64
builder, can slot in between 3 and 4 if hosts need live air changes; baked
presets cover the plan without it.

---

## 6. Out of scope

- Volumetric clouds (Hillaire's and Schneider's cloud work is a separate
  system; Venus's deck is handled as a layered aerosol, not clouds).
- Refraction (flattened suns, super-refraction, red lunar umbra beyond the
  cheap colour), polarisation.
- Relativistic effects beyond a stylised or precomputed Schwarzschild
  lensing of the background: no Kerr, no lensing of the light reaching the
  planet, no time dilation, no redshift of the planet's own light.
- Gas giants' interiors (flying *into* Jupiter): a deep, non-spherical,
  banded atmosphere is another model.
- Horizontal structure (dust storms, plumes, aurorae — though an aurora is
  a cheap emissive layer to revisit later).
- Full spectral rendering in the real-time engine (§4.6).

---

## 7. How others do it

| Engine / work | Lights | Media | Notes | Source |
|---|---|---|---|---|
| **Unreal Engine Sky Atmosphere** | **two** atmospheric directional lights (`AtmosphereSunLightIndex` 0/1, e.g. sun and moon) | Rayleigh + Mie (absorption, anisotropy) + an absorption layer with a tent profile, planet radius and height parameters | Hillaire's own production implementation; sky-view and aerial-perspective LUTs | [docs](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-in-unreal-engine), [API](https://docs.unrealengine.com/4.26/en-US/API/Runtime/Engine/FLightSceneProxy/AtmosphereSunLightIndex/index.html) |
| **Unity HDRP Physically Based Sky** | the main directional lights | two-part atmosphere (air + aerosols), planetary radius and centre | precomputed tables after Bruneton | [manual](https://docs.unity3d.com/Packages/hdrp/manual/Override-Physically-Based-Sky.html) |
| **Bruneton 2008 / 2017** | one sun | Rayleigh, Mie, absorption; 2-layer density profiles; spectral precompute converted to RGB | the reference implementation tuile's tables descend from | [2008](https://inria.hal.science/inria-00288758), [2017](https://github.com/ebruneton/precomputed_atmospheric_scattering) |
| **SpaceEngine** | multiple lights for multiple-star systems | moved from precomputed tables to **on-the-fly integration** in the shader to allow custom density profiles, haze layers (Venus, Titan) | eclipses and refraction historically weak points | [dev blog](https://spaceengine.org/author/admin/page/18/), [forum](https://forum.spaceengine.org/viewtopic.php%3Fp=36071.html) |
| **Celestia** | the system's stars | per-body Rayleigh RGB, Mie coefficient and asymmetry, scale heights, absorption in `.ssc` files | simple, data-driven per body | [forum: Titan block](https://celestiaproject.space/forum/viewtopic.php?p=135950) |
| **OpenSpace** | sun | per-planet Rayleigh/Mie tables, atmosphere height | Bruneton-derived, used for Mars, Titan | [RenderableAtmosphere](https://docs.openspaceproject.com/releases-v0.22/reference/asset-components/Renderable/RenderableAtmosphere.html) |
| **Kerbal Space Program 2** | the star | Rayleigh, Mie, ozone, tuned per body (Laythe heavier Mie); transmittance on sunlight | precomputed tables needed precision and sampling rework at small-planet scale; red sunsets on Kerbin, blue on Duna | [Developer Insights #22](https://kerbalspaceprogram.com/news/developer-insights-22-skys-the-limit) |
| **Elite Dangerous** | multiple stars | thin atmospheres on landable bodies (Odyssey) | lensing around black holes, white dwarfs and neutron stars is a visual effect, not documented by the studio | [player discussion](https://forums.frontier.co.uk/threads/black-hole-query.331928) |
| **No Man's Sky** | one star | stylised per-planet colour palettes rather than physical coefficients | art direction over physics | GDC 2017, "Building Worlds Using Math(s)", [GDC Vault](https://gdcvault.com/play/1024514/Building-Worlds-Using) |
| **Outerra** | sun | Bruneton-style precomputed scattering at planetary scale | Earth only | [outerra.com](https://www.outerra.com/) |
| **Universe Sandbox** | any number of stars (physics first) | simplified | simulation, not sky realism | [universesandbox.com](https://universesandbox.com/) |
| **Collienne et al. 2013** | sun | Bruneton 2008 adapted to Mars | colours matched to rover images | [DLR elib](https://elib.dlr.de/86477) |
| **Schneegans et al. 2024** | sun | Mie-theory phase tables per wavelength; Earth, Mars | blue Martian sunset; real-time | [CGF](https://diglib.eg.org/handle/10.1111/cgf15010) |
| **Bruneton 2020** | accretion disc, stars | — | real-time Schwarzschild lensing from precomputed tables | [arXiv:2010.08735](https://arxiv.org/abs/2010.08735) |
| **DNGR (Interstellar)** | disc, stars | — | Kerr lensing, ray bundles, offline | [arXiv:1502.03808](https://arxiv.org/abs/1502.03808) |

What the survey says for tuile: the industrial engines stop at **two
directional lights** in the atmosphere (Unreal) and get there by per-light
terms on shared tables — the same choice as §3.3. The space engines that
went furthest on media (SpaceEngine) gave up precomputation for custom
profiles; tuile can keep its tables because its typed profiles stay 1D and
its CPU builder already takes any medium — the cost is moved to "once per
medium", not "every frame".

---

## 8. Summary

- **Lights are where the generality is.** Transmittance, multiple scattering
  and irradiance tables are per unit of light and shared by every star;
  sky-view tables and the per-fragment march grow linearly with the number
  of lights, capped by a per-backend `K` (4 desktop, 2 WebGL2/mobile), with
  lights past it entering through the shared irradiance — never dropped.
- **Media are parameters of the same tables**: body radius, RGB aerosols with
  per-channel phase, typed density profiles. Hillaire's multiple-scattering
  approximation is the limit: fine to τ ≈ 1, to be measured up to ≈ 5
  (Titan, Martian storms), wrong for Venus's deck, which needs a separate
  diffusion term at the end of the plan.
- **One engine; Earth is a configuration.** Each generalisation is a value
  whose Earth instance makes the arithmetic collapse onto today's (× 1,
  + 0, same order, same expressions), and the reduction is proved by table
  bytes, uniform bytes and a film digest with the atmosphere on, frozen
  before the first PR.
- **Plan**: `#[non_exhaustive]` on #137's types → constants to uniforms
  (identity only) → N lights → stars as data and eclipses → media presets
  with a CPU path-tracer reference → secondary lights and rings → exotic
  sources → (optional) opaque regime.
