<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The sun and the Moon — discs as light sources of the one atmosphere

A design study. **Nothing here is a decision and no code changes with it.**
It asks how the sun's disc and the Moon's should be drawn better —
limb darkening and its colour, the air's colour across the disc, refraction,
the Moon's surface and its photometry, eclipses, glare — **inside the existing
atmosphere model**, not as an overlay or a special path, and how the result
is shown (HDR, antialiasing of small discs, glare and bloom) the same way in
the film and in the viewer. It ends on an incremental PR plan with acceptance
scenes, and lists what needs a decision.

**The two constraints that shape it.**

1. *One engine.* The generalisation study (PR #141,
   `docs/explorations/atmosphere-generalisation.md`) makes the atmosphere a
   single general engine of which the Earth with its Sun and Moon is one
   configuration. A disc is therefore **the visible face of a light source of
   that engine** — a body with an angular size, a spectrum, a limb-darkening
   law (an emitter) or a phase law and an albedo map (a reflector) — and every
   improvement below is a property of that description, read by the one WGSL
   (`seen.wgsl`, `hillaire.wgsl`) the film and the viewer share. Where the
   Earth configuration's output changes, the change is deliberate, listed in
   §8, and the goldens are re-frozen with the reason.
2. *No black ground, and no lies by counters* (`CLAUDE.md`). Every PR ends on
   renders opened, not on a digest alone.

Read at `origin/main` (`a7ea3462`) and on `feat/viewer-hillaire-atmosphere`
(PR #137, draft; `seen.wgsl`, `look.rs`, the viewer's sky pass live there).
Neither is edited.

---

## 1. What runs today

| Piece | Where (#137) | What it does |
|---|---|---|
| Sun disc | `seen.wgsl` `sky_seen` | radius `SUN_ANGULAR_RADIUS = 0.004652 rad × SkyLook.sun_scale`; radiance `sunlight(h, μ_s) × limb / Ω☉` with `sunlight = transmittance(h, μ_s) × sun_visible(h, μ_s)` **at the disc's centre**, `limb = 1 − 0.6 (1 − √(1 − x²))`, grey |
| Moon disc | `hillaire.wgsl` `moon_seen` | Lommel-Seeliger `μ₀ / (μ₀ + μ)` + earthshine `1.5·10⁻⁴ × (1 − cos α)/2`, divided by its mean over the disc so the disc carries `moon_irradiance` (Allen's magnitude law, opposition surge included in the integral); uniform albedo; transmittance at the Moon's centre; a smoothstep edge 4 % of the radius wide |
| Stars | `stars_seen(n, σ)` | hash stars as Gaussians of `σ ≈ 0.8 px`, flux-conserving |
| Glare | `hillaire.wgsl` `glare_psf`, `glare_of` | two normalised Lorentzian rings (4 % of the light at θ₀ = 0.026 rad, 1 % at 0.1 rad) and a faint six-ray star, × `discs.z` |
| Film present | `tuile-film-gpu/src/shaders/present.wgsl` | averages k×k HDR samples, adds glare **at the pixel centre** for each disc not `hidden()` (a visibility-buffer lookup at the disc's centre), the Moon's glare × 0.2, then exposure, white balance, contrast, saturation, `roll_off`, OETF |
| Viewer | `tuile-wgpu/src/sky.wgsl`, `shader.wgsl` | `graded(sky_seen(..))` and `graded(ground_seen(..))` **per fragment**, into an 8-bit sRGB MSAA target; the sky is one full-screen triangle, shaded **once per pixel**; no post pass, no glare (`discs.z` unused) |
| Refraction | — | none, for any body; Hillaire's rays are straight (assumption A7 of #141) |

### 1.1 Defects found while reading

These are independent of any new feature and are worth fixing first (PR S1):

1. **The setting sun is dimmed twice.** `sky_seen` returns early wherever the
   pixel's ray meets the planet — the disc is already cut per pixel by the
   geometric horizon — and then multiplies the part left by
   `sun_visible(h, μ_s)`, the fraction of the disc above that same horizon.
   A half-set sun shows its upper half at half its brightness.
2. **The limb law is not normalised.** The mean of `1 − u(1 − μ)` over the
   disc is `1 − u/3`, here 0.8: the drawn disc integrates to **80 %** of the
   irradiance the ground is lit with. The Moon does normalise
   (`moon_shade.x`); the sun does not.
3. **One transmittance for the whole disc.** Near the horizon the air mass
   changes fast across the disc's 0.53° (Kasten & Young 1989: 37.9 at 0°,
   31.0 at 0.53° — §2.2), so the lower limb should be several times dimmer
   and redder than the upper one. Today the whole disc takes its centre's.
4. **The viewer's disc edge is a hard step per pixel.** MSAA does not
   supersample a full-screen triangle's shading; the sun (≈ 8 px across at
   35° vfov on 540 px) and the Moon are drawn as stair-cased discs that
   crawl as they move. The film's k×k average hides it there.
5. **The viewer clips before it resolves.** Values thousands of times above
   white are rolled off and written to 8 bits per sample, then MSAA averages
   the clipped samples. Nothing downstream can know how bright the disc was:
   no glare, no bloom is possible after that point.
6. **The Moon does not hide the sun.** Both are added (`beyond += …`): in a
   solar eclipse the sun's disc shows through the Moon's.
7. **The Moon's horizon uses the sun's radius.** `sunlight(h, μ_m)` calls
   `sun_visible`, whose smoothstep spans `±SUN_ANGULAR_RADIUS`; the Moon's
   own radius varies from 0.245° to 0.279°. Small, but it is a body's
   property read from the wrong body.
8. **The core of the sun is yellow-white, not white, and shows no colour.**
   `roll_off` keeps channel ratios while bending luminance, so a disc at
   thousands of times white keeps a ratio like (1, 0.89, 0.53) whose red
   channel then clips: (255, 227, 136) at sunset. The limb darkening and the
   reddening are lost in the clip. This one is colorimetry (§5.6), not a bug.

---

## 2. Physics

### 2.1 The sun's radiance and limb darkening

The sun's mean radiance is its irradiance over its solid angle:
`L̄ = E☉ / Ω☉`, `Ω☉ = 2π(1 − cos ρ☉) ≈ 6.8·10⁻⁵ sr` — in the engine's units
(irradiance outside the air = 1) about 14 700, i.e. ≈ 4 600 times the
radiance of a white Lambertian surface under the same sun. Its luminance at
the top of the air is ≈ 2·10⁹ cd/m² (Frostbite takes 1.6·10⁹ at the ground,
Lagarde & de Rousiers 2014).

Limb darkening is the centre-to-limb variation `I(μ)/I(1)`, `μ = cos θ` of
the emergent angle on the photosphere (`μ = √(1 − x²)` with `x` the
fraction of the disc's radius):

| Law | Form | Notes |
|---|---|---|
| Linear | `1 − u(1 − μ)` | today, `u = 0.6`, grey; mean `1 − u/3` |
| Quadratic | `1 − u₁(1 − μ) − u₂(1 − μ)²` | the usual stellar law; mean `1 − u₁/3 − u₂/6` |
| Power (Hestroffer & Magnan 1998) | `μ^α(λ)`, `α ≈ −0.023 + 0.292 / λ[µm]` (416–1099 nm), fitted on Pierce & Slaughter 1977 and Neckel & Labs 1994 | one parameter per channel; mean `2/(α + 2)`; exact for the Sun to a few % |
| Fifth-order polynomial (Neckel & Labs 1994) | `Σ aᵢ μⁱ`, 30 wavelengths 303–1099 nm; Neckel 2005 gives the `aᵢ` as functions of λ | the reference; the data source of the fits above |

Evaluated at the engine's three wavelengths (680, 550, 440 nm):

| Channel | α | Disc mean `2/(α+2)` | Centre / mean | Limb (x = 0.95, μ = 0.31) / mean |
|---|---|---|---|---|
| R 680 | 0.406 | 0.831 | 1.20 | 0.75 |
| G 550 | 0.508 | 0.797 | 1.25 | 0.69 |
| B 440 | 0.641 | 0.757 | 1.32 | 0.63 |

**The colour of the limb follows.** Normalised per channel so the *disc*
keeps the Sun's colour (its integral is the irradiance, `[1, 1, 1]` in the
engine's units — the unit #141 §3.1 states), the centre is a little bluer
than the mean (B/R 1.10) and the limb redder (B/R 0.84). Outside the air
this is a faint warm rim; it matters most in a telephoto view and in the
film, and it is what keeps a large sun from looking like a flat sticker.

What the engine needs is a `LimbDarkening` value per emitter — `Linear(u:
[f32; 3])`, `Quadratic(u1, u2: [f32; 3])`, `Power(α: [f32; 3])` — and the
CPU uploading its **disc mean per channel**, so that the shader divides by it
and the disc integrates to the light that lights the ground, whatever the
law. A star of another type gets its own coefficients (Claret's tables for
stellar atmospheres are the standard source, Claret 2000) — the same field.

### 2.2 The sun's colour through the air, per pixel of the disc

The transmittance along the ray to each point of the disc is the
transmittance of *that pixel's* direction, not of the disc's centre. The
engine already reads it per pixel elsewhere (`transmittance(h, μ)`); the disc
only has to use the pixel's `μ = dot(up, n)` instead of the centre's.

How much it matters at sea level (Kasten & Young 1989 air mass, zenith
optical depths τ_B ≈ 0.35, τ_R ≈ 0.08 for a clear day):

| Apparent altitude of the point | 0° | 0.27° | 0.53° | 1° | 2° |
|---|---|---|---|---|---|
| Air mass | 37.9 | 34.2 | 31.0 | 26.3 | 19.4 |

Across a disc sitting on the horizon, blue transmittance changes by
`exp(−0.35 × 6.9) ≈ 0.09` from upper to lower limb, red by `≈ 0.58`: the
lower limb is an order of magnitude dimmer in blue and visibly redder. With
refraction (§2.3) the disc is squeezed vertically and the gradient is
steeper still. From the Pic du Midi (2 900 m, ≈ 0.72 atm) the depths are
smaller and the effect weaker but present.

### 2.3 Refraction

**What it does.** Rays curve towards the denser air below; a body is seen
higher than it is, most at the horizon. Standard formulas (Meeus ch. 16;
[Wikipedia summary](https://en.wikipedia.org/wiki/Atmospheric_refraction)):

- Bennett 1982, from the *apparent* altitude `a` (degrees), result in
  arcminutes: `R = cot(a + 7.31 / (a + 4.4))` — 0.07′ accuracy above 5°,
  better than 1′ at the horizon.
- Saemundsson 1986, from the *true* altitude `h`: `R = 1.02 cot(h + 10.3 /
  (h + 5.11))` — its inverse to ~0.1′.
- Both for 101.0 kPa and 10 °C, scaled by `(P / 101) × 283 / (273 + T)`.

| Apparent altitude | 0° | 0.25° | 0.5° | 1° | 2° | 5° |
|---|---|---|---|---|---|---|
| Bennett R | 34.5′ | 31.4′ | 28.8′ | 24.3′ | 18.2′ | 9.9′ |

**Flattening.** The lower limb is lifted ≈ 5.7′ more than the upper limb when
the disc sits on the horizon: the vertical diameter goes from 32′ to ≈ 26′, an
oblateness of ≈ 0.8, with the familiar flat-bottomed, slightly irregular
setting sun. At the Pic du Midi (P ≈ 72 kPa, T ≈ 5 °C) every refraction is
scaled by ≈ 0.72.

**The formulas do not cover the cases tuile meets.** An eye at 2 900 m sees
the sea-level horizon 1.73° *below* the horizontal; Bennett diverges at
`a = −4.4°` and is not valid well before. An airborne camera, a film from
orbit, Mars or Venus are not described by a sea-level formula at all. The
honest engine-level model is **a refraction table computed from the medium**:

- refractivity `n − 1` proportional to the gas density the engine already
  has (`(n − 1)₀ ≈ 2.93·10⁻⁴` for dry air at 0 °C, 101.325 kPa, 550 nm;
  Ciddor 1996 gives the dispersion per channel), following the Rayleigh
  density profile — so Mars, Titan and Venus get theirs from their own
  `Medium` (#141 §4);
- rays traced in f64 on the CPU through the spherically symmetric shell
  (Bouguer's invariant `n(r) r sin z = const` makes it a 1D quadrature per
  ray), once per medium and eye-height bucket, tabulated as
  `true zenith = f(apparent zenith, eye height)` (e.g. 256 angles near the
  horizon, packed densely there, × 32 heights; ≈ 32 KB as f32);
- held to Bennett/Saemundsson at sea level as a test (§7), and to the
  pressure/temperature scaling at altitude.

Where it is used: **disc pixels** (each pixel's apparent direction mapped to
the true one before the disc test — flattening falls out with no special
case), the **light direction for lighting** (a sun whose centre is
geometrically 0.5° below the horizon still lights mountain tops; day lasts a
few minutes longer — deliberate), **stars** (the same mapping), and the
**horizon cut** of the discs (at the apparent horizon). The sky's own
scattering stays straight-ray (A7), the error being sub-degree near the
horizon, under the sky-view table's resolution. This is #141's planned "PR
3b, refraction"; this study proposes its content.

**Green flash (stretch).** Dispersion makes the blue and green images of the
upper limb sit slightly higher than the red; with blue removed by Rayleigh
extinction the last sliver is green. With per-channel refractivity the table
yields per-channel true directions; the disc test done per channel draws
the rim for free. The separation is ≈ 10–20″ at the horizon — a hundredth
of the disc, below a pixel except in a telephoto shot. The spectacular
flashes need mirages (temperature inversions, Young's "mock mirage"): out of
scope; the separation alone is honest. Reference: Andrew T. Young's pages,
the definitive treatment.

### 2.4 The Moon

**Photometry.**

| Model | Form | Use |
|---|---|---|
| Lambert | `μ₀` | wrong for the Moon: a full Moon would be dark at the limb; it is not |
| Lommel-Seeliger | `μ₀ / (μ₀ + μ)` | today; flat full Moon, correct to first order for a dark, rough regolith |
| Lunar-Lambert (McEwen 1991) | `A(α) · 2μ₀/(μ₀ + μ) + (1 − A(α)) μ₀`, `A` tabulated with phase | a cheap blend fitted to lunar data |
| Hapke (1981, 2012) | single-scattering albedo `w`, phase `p(g)`, shadow-hiding `B_SH(g)` and coherent-backscatter `B_CB(g)` opposition surges, roughness θ̄ | the reference; LROC's own normalisation uses it (Sato et al. 2014) |
| ROLO (Kieffer & Stone 2005) | an empirical *disc-integrated* irradiance model from 8 years of observations, function of phase, libration and wavelength | the calibration standard for the Moon's brightness |

What the engine needs is not full Hapke per pixel — the surge and the
roughness are first a *disc-integrated* effect, and the disc-integrated
brightness is already anchored (`moon_irradiance`, Allen's law, surge
included). Proposal: **Lommel-Seeliger × a per-pixel phase factor `f(α)`**
(Hapke's `B(α) = 1 + B₀ / (1 + tan(α/2) / h)`, `B₀ ≈ 1`, `h ≈ 0.05` for the
shadow-hiding part) **× an albedo map**, all divided by their mean over the
lit disc, so that the disc integrates exactly to `moon_irradiance` — the
number the ground is lit by. The surge's visible effect per pixel is small;
what this changes is that the brightness distribution over a gibbous disc
stays right with the texture on. ROLO is the candidate to replace Allen's
law as the anchor later (it has libration and wavelength), not needed for
the visible result.

**Albedo map and its licence.** Sources, all lunar mosaics of NASA missions:

| Product | Coverage, resolution | Status for shipping |
|---|---|---|
| LROC WAC Hapke-normalised 7-band mosaic (`WAC_HAPKE`, 321–689 nm, normalised to i = g = 60°, e = 0) | 70° N–70° S, 400 m | **PDS archive: public domain** per the LROC terms ("data products distributed through the PDS … are public domain"); the copies on the LROC web site and its curated products are copyrighted, commercial use requires permission. Download **from the PDS volume only**. |
| LROC WAC global morphology mosaic, 643 nm (`WAC_GLOBAL`) | global, 100 m | PDS: public domain; also on USGS Astrogeology (US government work) |
| NASA SVS CGI Moon Kit (4720) | global, colour, derived from `WAC_HAPKE` (643/566/415 nm) with poles filled, colour adjusted "for human vision", up to float16 EXR | NASA media guidelines: NASA content, incl. texture maps for 3D models, generally not subject to copyright in the US; credit "NASA's Scientific Visualization Studio". **Not verified for the specific item** (the page could not be fetched for this study) and it is a derived product partly from ASU-processed data: needs a check of the item's own credit block before shipping |
| Clementine UVVIS global mosaic (750 nm, 5 bands) | global, 100 m | PDS / USGS: public domain; older, more seams, phase-dependent brightness |

Recommendation: build tuile's own lunar albedo texture from **PDS
`WAC_HAPKE` 415/566/643 nm**, poles from `WAC_GLOBAL` (643 nm scaled), with a
documented script — the provenance is then the PDS's, public domain, and the
look is ours (no "adjusted for human vision" from someone else). Licence to
be confirmed by Laurent (§8) — the code must be commercially compatible and
so must shipped data (`licence-lab-vs-shipped`). Sizes: 1024×512 (≈ 3.5 km
per texel at the equator, ≈ 2 texels per pixel of a 540-px-high telephoto
Moon) is enough for wide shots; 2048×1024 for telephoto; 4096×2048 for film
close-ups. RGB8 sRGB: 1.5 / 6 / 24 MB uncompressed (+ mips ⅓). The texture
is mostly luminance; storing a luminance map + one RGB tint (the Moon's
colour, ≈ constant) is a 3× saving — a measurement for the PR. Compression
is the client's choice and off by default (`compression-is-the-clients-choice`).

**Orientation: libration and position angle.** The texture is mapped in
selenographic coordinates and turned by the Moon's orientation at the
instant: optical + physical libration in longitude and latitude, and the
position angle of the axis (Meeus ch. 53). The `astro` crate already used for
the Moon's place has them (`lunar::total_libr`, `lunar::physical_libr`,
`lunar::pos_angl_of_axis_of_rot`, `lunar::topocent_libr_by_diff_corrections`),
in the pinned lapoule-dev fork; the IAU rotation model (Archinal et al.
2018) is the cross-check. Output: a 3×3 rotation from ECEF to selenographic,
uploaded per frame (36 bytes). Libration shows up as ±7.9° in longitude and
±6.9° in latitude: visible in Mare Crisium's distance to the limb.

**Earthshine.** Today `1.5·10⁻⁴ × (1 − cos α)/2`, i.e. proportional to the
Earth's lit fraction seen from the Moon (the Earth's phase is the
supplement of the Moon's). Physical form: Earth as a Lambert sphere of
albedo `A⊕ ≈ 0.30` (Goode et al. 2001; Pallé's Big Bear series), its phase
function `[sin β + (π − β) cos β] / π` with `β = π − α`, its solid angle seen
from the Moon. This ties earthshine to the configuration — the body's own
albedo in #141's `Body` — so that a world with another albedo or another
moon gets its planetshine from the same formula (#141 §3.5). The constant
is kept as the Earth's lowering if the reduction must hold; replacing it is
a deliberate, documented change (§8).

**Colour of the Moon.** The Moon's reflectance rises towards the red
(its B−V ≈ 0.92 against the Sun's 0.65): moonlight is slightly redder than
sunlight. Today the Moon light is the sun's colour × a grey weight
(`atmosphere.moon.w`). The WAC bands give its mean RGB reflectance ratio
directly; making the Moon a reflector with an RGB albedo (as #141 §3.5's
`Reflector { albedo: [f64; 3] }` already proposes) changes the Moon's disc
*and* the colour of moonlit ground — deliberate (§8). Whether moonlit
ground should then be *shown* bluish (the Purkinje shift, the "day for
night" convention) is a tone question (§5.6), not physics.

**Colour through the air.** Same as the sun: per-pixel transmittance
across the disc, refraction, the Moon's own radius for its horizon. A
rising full Moon goes orange-red for the same reason a setting sun does.

**Moonlight on the ground.** Already in the model: the Moon is the second
light of the engine (`moon.w` in the sky-view, the march and the ground
light). What changes with this study is only its colour (above) and, in an
eclipse, its weight (below).

### 2.5 Eclipses

- **Lunar eclipse.** The Moon's weight is multiplied by the fraction of the
  sun's disc seen from each point of the Moon past the Earth's limb
  (penumbra, closed-form disc overlap, #141 §3.4), and in the umbra by the
  light **refracted and filtered through the Earth's limb**: rays grazing
  the Earth at tangent heights of ~5–30 km cross the transmittance of twice
  the slant path to space and are bent by ≈ 2× the horizontal refraction
  (≈ 1.2°) into the umbra. The colour of the umbra therefore comes out of the
  **same transmittance table** (`T(r = R + h_t, μ = 0)²`) weighted over
  tangent heights by the refraction table of §2.3 — no new data, and the
  Danjon brightness (L = 0 … 4) emerges from the aerosol load
  (García Muñoz & Pallé 2011 is the full radiative-transfer treatment). Per
  pixel of the Moon's disc (the penumbra's gradient crosses the disc),
  per frame on the CPU for the Moon's light on the ground. The 2026‑08‑28
  partial lunar eclipse, seen from Europe at dawn, is a free acceptance
  scene.
- **Solar eclipse.** The Moon's disc hides the sun's per pixel (near body
  in front of far body: defect 6), and the sun's light on the ground and in
  the air is multiplied by the visible fraction of its disc, limb-darkened
  (#141 §3.4, per fragment since the umbra is ≈ 100–270 km). The 2026‑08‑12
  total eclipse over northern Spain, near sunset, is the acceptance scene.
  The corona is out of scope.

---

## 3. Display

### 3.1 Where the disc must be resolved

A disc is a high-radiance, small-area source. Two quantities must be right:
its **flux** (the integral over the pixels, which glare and bloom spread)
and its **shape** (only when it spans several pixels).

- **The pixel's footprint, analytically.** For each body, compute in the
  shader the overlap of the disc with the pixel's footprint rather than a
  point test: a disc of angular radius ρ seen through a pixel of angular
  size p, at distance d from its centre, covers
  `c = clamp((ρ − d)/p + ½, 0, 1)` to first order (an exact box-disc area is
  a few more ALU and not needed). Limb darkening and transmittance are
  evaluated at the clamped point. The disc then integrates to its flux at
  any size ≥ 1 px, and its edge no longer crawls in the viewer (defect 4).
- **Sub-pixel discs.** When `ρ < σ` (σ the star footprint, ≈ 0.8 px — the
  same σ the stars use), draw the body as the stars are drawn: a Gaussian of
  radius σ carrying the disc's flux `L̄ · Ω`. A 3-px sun on a phone at 120°
  fov and a sun drawn at `sun_scale < 1` keep their energy; the star code
  already proves the method. One law with a continuous transition (blend on
  `ρ/σ`) — never a switch.
- **The film** already integrates by k×k supersampling; with the analytic
  footprint the film needs fewer samples for the same disc edge, and the two
  renderers converge on the same disc. The digest moves (§8).

### 3.2 HDR, and the order of operations

The film's order is right: **average in linear HDR, add glare, then expose,
grade, roll off, encode.** The viewer's order is wrong for bright sources:
it grades per fragment and then averages clipped samples (defect 5). The
viewer needs an HDR intermediate:

| | Today (viewer) | Proposed (viewer, own frame) |
|---|---|---|
| Colour target | sRGB 8-bit, MSAA ×4 | **`Rgba16Float`**, MSAA ×4, linear, *pre-exposed* (`atmosphere.illuminance` already pre-exposes) |
| Resolve | hardware, of graded values | hardware, of linear values (box filter: energy-conserving) |
| Post | none | one full-screen pass: glare, (bloom), white balance, `roll_off`, OETF → swapchain |
| Shader change | `graded()` per fragment | the fragment returns linear; `graded()` moves to the post pass |

f16 holds up to 65 504: the sun's disc at noon pre-exposed is ≈ 10³–10⁴ ×
white, inside the range; the *pre-exposure* is what makes it fit (no
fragment writes absolute radiance). The film's `present` and the viewer's
post pass become **one WGSL module** (`present_common.wgsl` in
`tuile-atmosphere`, next to `seen.wgsl`): `glare`, `roll_off`, `oetf`, the
order of operations. The film keeps its k×k average and its visibility
buffer; the viewer gets its average from the MSAA resolve and its visibility
from depth (§3.4).

### 3.3 Glare and bloom

| PSF | What | Reference | Cost |
|---|---|---|---|
| Spencer et al. 1995 | the eye's PSF: a Gaussian core + two inverse-power wings (f₁, f₂) + the lenticular halo (f₃), weighted by pupil size (scotopic vs photopic) | SIGGRAPH 95 | analytic per light: ~30 ALU / pixel / light |
| Ritschel et al. 2009, temporal glare | the eye's aperture (pupil, lashes, lens fibres) as a time-varying 2D mask; PSF by FFT of it, per wavelength; convolution of the image | EG 2009 | FFT of the frame: 1–3 ms desktop; too much for mobile |
| Camera lens (today's film) | normalised Lorentzian core and halo, six-ray star of a six-bladed iris | `glare_psf` | analytic, as Spencer |
| Mip-chain bloom (Jimenez 2014) | 13-tap downsample, tent upsample, energy-conserving; spreads *any* highlight (speculars, city lights) | SIGGRAPH 2014 Advances | ~6 mips: 0.2–0.4 ms desktop, 0.6–1.5 ms mobile at 1080p |

**Recommendation.** Keep the **analytic glare** of today's film as the disc
glare — it is physically a PSF applied to known point-like sources, exact,
cheap, deterministic, needs no HDR blur — and share it (it already lives in
`hillaire.wgsl`). Its PSF stays a camera's (a film is a camera; Spencer's eye
PSF is a different look). Bloom of arbitrary highlights is a second, separate
step (PR S6) and a look decision: a globe at noon has few highlights besides
the sun; cities at night and the specular glint of water do.

The Moon's glare × 0.2 in `present` is a look factor with no physical basis
(the glare is a share of the flux; the Moon's flux is already the Moon's).
Under an exposure set for the night, the Moon is bright and its glare is
what a camera shows. Proposal: the 0.2 becomes a documented look value or
goes; renders decide (§8).

### 3.4 Occlusion of the disc for glare

Glare depends on how much of the disc is *seen*, not whether its centre is.
Today's film tests one sample (`hidden(s)`): a sun half behind a ridge
flashes its whole glare on and off as the centre crosses.

The fraction seen is `V = Σ coverage(sample) × limb(sample) × [sample is
sky] / Σ coverage × limb` over the disc's footprint in the depth or
visibility buffer, which also weights by transmittance per sample for a
setting sun. A disc of ≤ 16 px across is ≤ 256 samples: one 16×16 workgroup
(compute) or one fragment of a 1×1 `R16Float` target looping over the
footprint (WebGL2), per light, per frame. Output: one float per light,
read by the post pass (and, for the film, by `present`). Cost: negligible
(< 0.01 ms). A disc larger than 32 px (telephoto) is subsampled on a fixed
grid. Same code for film and viewer: the input buffer differs (visibility
vs depth == far).

**Determinism.** V is computed from the current frame's depth, after the
opaque pass and before the post pass — no lag, no temporal accumulation.

### 3.5 Exposure

The disc never enters the metering: the viewer's light (`LutAtmosphere::light`)
meters the sky and ground, not the disc (today too). Keep it so — the sun in
frame must not darken the landscape, a camera on auto-exposure with spot
metering does the same. Glare is added *before* exposure and so scales with
it. Nothing changes here; it is a constraint for the PRs.

### 3.6 Tone: the colour of highlights (proposal only)

Defect 8 is a tone-mapping question and Laurent's decision
(`colorimetry-deferred`). The candidates, for the record:

| Option | Sun core | Ground | Note |
|---|---|---|---|
| Today: hue-keeping roll-off, then clip | yellow-white, no limb | unchanged | |
| **Path to white** above the knee: desaturate towards the luminance as it rises (e.g. blend to `vec3(Y)` with `(Y − knee)/(peak − knee)`), the hue kept below | white core, colour only where the disc is not saturated (limb, glare) — what a camera shows | unchanged below the knee | the smallest change; the knee is already a parameter |
| AgX (Sobotka; Blender 4, Bevy) or ACES 1.x / 2.0 | white core, filmic shoulder | changes mid-tones too | a look change for the whole picture |
| HDR display (WebGPU `toneMapping: "extended"`, Apple EDR) | brighter-than-white sun | — | later; the `Rgba16Float` post pass is the prerequisite |

No recommendation is made beyond: the HDR pass (§3.2) is a prerequisite for
all of them, and renders of the four acceptance scenes under each are what
the decision should be taken on.

---

## 4. How others do it

| Engine / work | Sun disc | Moon | Refraction | Glare / bloom | Source |
|---|---|---|---|---|---|
| **Unreal Engine SkyAtmosphere** (Hillaire 2020) | from each atmosphere directional light: angular size from `SourceAngle`, luminance = illuminance / solid angle × `AtmosphereSunDiskColorScale`, transmittance applied; per-pixel transmittance on opaque meshes optional (`bPerPixelAtmosphereTransmittance`); two atmosphere lights (sun + moon) | a second directional light; the Moon disc itself is a user material | none | post-process bloom (mip chain, or FFT convolution "Convolution bloom" with a kernel image) | [component docs](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-in-unreal-engine), [properties](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-properties-in-unreal-engine), [per-pixel transmittance](https://docs.unrealengine.com/4.27/en-US/API/Runtime/Engine/Components/UDirectionalLightComponent/bPerPixelAtmosph-/index.html) |
| **Hillaire's sample** (the paper's code) | a constant 10⁶ disc of 0.505°, cut by the planet — "arbitrary, not used when comparing" | — | — | — | [RenderSkyCommon.hlsl](https://github.com/sebh/UnrealEngineSkyAtmosphere/blob/master/Resources/RenderSkyCommon.hlsl) |
| **Bruneton 2017 demo** | `transmittance × solar_irradiance / (π ρ²)` where `dot(v, s) > cos ρ`, transmittance **along the view ray, per pixel** — no limb darkening | — | — | — | [demo.glsl](https://github.com/ebruneton/precomputed_atmospheric_scattering/blob/master/atmosphere/demo/demo.glsl) |
| **Frostbite** (Hillaire 2016; Lagarde & de Rousiers 2014) | the sun as a disc area light (≈ 0.545°) for specular and as a disc in the sky; physical units (1.6·10⁹ cd/m²); exposure handles the range | — | — | bloom | [sky course](https://www.ea.com/frostbite/news/physically-based-sky-atmosphere-and-cloud-rendering), [PBR course](https://seblagarde.wordpress.com/2015/07/14/siggraph-2014-moving-frostbite-to-physically-based-rendering/) |
| **Jensen et al. 2001, "A Physically-Based Night Sky Model"** | — | Hapke-like lunar BRDF, albedo texture (Clementine), earthshine, Moon position from ephemeris, scotopic tone mapping (blue shift) | — | — | [SIGGRAPH 2001](https://doi.org/10.1145/383259.383306) — the closest prior work to this study's Moon |
| **Stellarium** (GPL: inspiration only, no code) | limb-darkened textured sun, extinction by air mass | textured Moon, libration, phase, earthshine; lunar eclipses with a coloured umbra | Bennett / Saemundsson with pressure and temperature, applied to every object, continuous below the horizon | eye-like glare halo | [RefractionExtinction.cpp](https://github.com/Stellarium/stellarium/blob/master/src/core/RefractionExtinction.cpp) |
| **Celestia** (GPL) / **SpaceEngine** (closed) | stars as blackbody-coloured discs by spectral type / temperature, limb darkening; multiple stars; eclipse shadows | textured bodies, eclipses | SpaceEngine: weak | stars as glare sprites sized by magnitude (flux-conserving point rendering) | [Celestia](https://github.com/CelestiaProject/Celestia), [SpaceEngine](https://spaceengine.org/) |
| **Microsoft Flight Simulator (2020/2024)** | physically based sky and sun disc; no public technical write-up found of its disc, limb or refraction | textured Moon with phases | not documented | bloom, lens flares | — |
| **Bevy** (the tuile-bevy host) | `Atmosphere` component (Hillaire-based) with a sun disc from the directional light's angular size | — | — | `Bloom` after Jimenez 2014, energy-conserving; tonemappers incl. AgX | [Bevy bloom example](https://bevy.org/examples/3d-rendering/bloom-3d/) |

What the survey says: production engines get the sun's **energy** right
(luminance = illuminance / solid angle, transmittance applied) and its
**colour through the air** right per pixel; few draw a wavelength-dependent
limb, none of the real-time ones refract the disc. The planetarium programs
(Stellarium) refract and eclipse but are not physically lit. Doing the disc
as a body of the scattering engine — per-pixel transmittance, normalised
limb law per channel, refraction from the medium, eclipse colour from the
same tables — is not done anywhere we found in one engine, and costs little
on top of what tuile already computes.

---

## 5. Design in tuile

### 5.1 The body description (extends #141 §5.4)

```rust
pub struct Star {                       // #141's, with the disc fields made explicit
    pub colour: Spectrum,               // → RGB relative to the Sun's: [1, 1, 1] for the Sun
    pub irradiance: f64,                // 1 for the Sun at 1 AU (the engine's unit)
    pub angular_radius: f64,            // 0.004652 rad for the Sun
    pub limb: LimbDarkening,            // Linear { u: [f32; 3] } | Quadratic { .. } | Power { alpha: [f32; 3] }
    pub variability: Variability,
    pub motion: Motion,
}
pub struct Reflector {
    pub body_radius: f64,
    pub albedo: [f64; 3],               // mean RGB reflectance: the light's colour on the ground
    pub law: Reflectance,               // LommelSeeliger { surge: Option<(b0, h)> } | Lambert | LunarLambert { .. }
    pub albedo_map: Option<AlbedoMap>,  // a texture handle, mean-normalised; None: uniform
    pub orientation: Orientation,       // Ephemeris (libration, position angle) | Fixed(DMat3) | TidallyLocked
    pub disc_integral: DiscIntegral,    // Magnitudes { .. } (Allen, today) | Rolo | FromAlbedo
    pub motion: Motion,
    pub lit_by: StarIndex,
}
pub struct Medium { /* #141 */ pub refractivity: Option<Refractivity> } // None: straight rays
```

What crosses to the GPU per light (#141's `LightGpu`, here its disc part):
direction (true, from the ephemeris), RGB weight, angular radius, `cos ρ`
and `Ω` (from the CPU, so the shader folds nothing — #141 §5.2's caveat),
the limb law's coefficients and its **per-channel disc mean**, for a
reflector its lighting direction, phase angle, surge, the selenographic
rotation and a texture layer index. ≈ 96 B per light. The refraction table
is one more small texture (256 × 32 RG32F ≈ 64 KB) plus the eye-height
index in the uniform.

The disc code in WGSL becomes **one function over the lights**
(`disc_seen(n, light, footprint)`), sorted near to far so a nearer body hides
a farther one, each disc: apparent→true via the refraction table, footprint
coverage, limb or reflectance law × albedo map, ÷ its mean, × per-pixel
transmittance × occluding-body visibility. The sun and the Moon are its
lights 0 and 1.

### 5.2 Where it lives

| Piece | Crate / file |
|---|---|
| Body description, lowering, limb means, libration matrix, refraction table (f64), eclipse fractions for lighting | `tuile-atmosphere` (Rust, no GPU) |
| `disc_seen`, glare PSF, visibility fraction, `present_common` (glare + grade + roll-off + OETF in order) | `tuile-atmosphere` WGSL (`seen.wgsl`, `hillaire.wgsl`, new `present.wgsl`), feature `wgpu` |
| Moon albedo texture build (from PDS products) | a `tools/` script + a baked asset fetched like other data (not committed if large), with its provenance file |
| HDR target, resolve, post pass, depth→visibility pass | `tuile-wgpu` (`targets.rs`, `renderer.rs`, a new `post.rs`) |
| Film | `tuile-film-gpu` `present.wgsl` reduced to its k×k average + the shared module |

`tuile-core` gains nothing (rule 1).

### 5.3 HostPass and tuile-bevy

`HostPass` draws into a colour target the host owns. Today it gets graded
sRGB values whatever the target's format. With the HDR path:

- **The behaviour follows from the host's colour format, like the depth path
  follows from its depth format** (`host.rs` doc): a **float** target
  (`Rgba16Float`, `Rg11b10Ufloat` — Bevy with `Hdr`) receives *scene-linear,
  pre-exposed* radiance, and the host's own tone mapper and bloom finish it
  (Bevy's `Tonemapping`, `Bloom`), or the host calls tuile's post pass
  (`tuile_wgpu::Present`) itself; an **8-bit** target receives today's
  per-fragment `graded()` output, with no glare — the only thing an 8-bit
  target can hold.
- This is not a switch: it is what each target can represent; the code that
  computes the light is the same, only where `graded()` is applied differs.
  But it is a **change of contract** for an HDR host: a tuile-bevy app with
  `Hdr` and `Tonemapping::None` sees a brighter, unclipped picture. tuile-bevy
  must then set the exposure contract (tuile pre-exposes; Bevy's camera
  exposure must be neutral or tuile's pre-exposure off). Laurent's decision
  (§8); the tuile-bevy headless tests (`hdr` true/false) are the proof.
- Glare in a host's frame needs depth (which tuile already reads for the
  companion crossing) and a post pass the host runs; tuile-bevy can register
  it as a render-graph node after the main pass.

### 5.4 WebGL2

- `Rgba16Float` as a render target needs `EXT_color_buffer_float` (WebGL2;
  very widely available, not universal on old mobile); MSAA of a float
  renderbuffer is allowed with it. wgpu's GL backend reports it through
  `get_texture_format_features` — to be checked on wgpu 30 for MSAA + float
  on WebGL2 (#142's WebGL2 Bevy path is the place to test).
- Where it is missing: the 8-bit path (graded per fragment, no glare) —
  again what the target can hold, not a setting.
- No compute: the visibility fraction is a fragment pass into a 1×1 target
  looping over the footprint; the refraction table is a texture; the albedo
  map a texture; nothing else needs compute.

### 5.5 Budget (estimates, to be measured in each PR)

| Item | Desktop (native / WebGPU) | Mobile (WebGPU, tiled) | WebGL2 | Film |
|---|---|---|---|---|
| `disc_seen` per sky pixel (2 lights: refraction fetch, footprint, limb, transmittance fetch per pixel) | +20–40 ALU and 2 fetches **only within the discs' bounding cones**; ≈ 0 elsewhere (early cone test) | idem | idem | idem, × k² samples |
| Moon albedo map | 6 MB (2k RGB8 + mips) | 1.5 MB (1k) | 1.5 MB | 24 MB (4k) |
| Refraction table | 64 KB, CPU ~5–20 ms once per medium (f64 quadrature) | idem | idem | idem |
| HDR target `Rgba16Float` MSAA ×4 at 1920×1080 | +33 MB vs 16 MB for RGBA8 (MSAA) + 16 MB resolve | MSAA kept in tile memory if wgpu exposes transient attachments (`TextureUsages::TRANSIENT`, check on wgpu 30); else 1170×2532: +47 MB — the main mobile cost | +33 MB | already HDR |
| Post pass (glare, grade, roll-off, OETF) | ~0.1 ms | ~0.3–0.6 ms | ~0.2 ms | already paid |
| Visibility fraction | < 0.01 ms | < 0.05 ms | < 0.05 ms | < 0.01 ms |
| Bloom (optional, S6) | 0.2–0.4 ms | 0.6–1.5 ms | 0.5 ms | ~1 ms at 4K |
| Libration, eclipse fractions, earthshine | CPU µs per frame | idem | idem | idem |

The disc work is confined to a few hundred pixels per frame; the cost of
this study is almost entirely the HDR target on mobile.

### 5.6 Determinism

The refraction table and the libration rotation are f64 CPU, from the scene
instant, never accumulated; bit-identical on native, an ulp of platform libm
apart on wasm (as #141 §5.5 says, the `libm` crate if a cross-target digest
must match). Glare and visibility are computed from the current frame only.
GPU pixels are deterministic per adapter, as the existing digests already
assume.

---

## 6. PR plan

Each PR: shared WGSL first (film and viewer in the same PR), the reduction
or the deliberate change stated, the acceptance scenes rendered **in both
renderers and opened**, before/after side by side, digests re-frozen with
the reason in the commit when they move.

| PR | Content | Changes Earth output? | Proof |
|---|---|---|---|
| **S0** | Instruments, no pixel change: (a) the four acceptance scenes (§6.1) as a film plan and viewer `control` scripts in the repo; (b) a disc-energy Observer in `tuile-film-native` (integral of the disc's radiance over the pixels vs `E × T`, per body); (c) a digest with the atmosphere on and discs in frame, frozen (with #141 PR 1's) | no | digests frozen; the Observer reports today's 0.8 (defect 2) and the half-set dimming (defect 1) — the failing tests the next PR turns green |
| **S1** | Disc correctness in `seen.wgsl`/`hillaire.wgsl`: per-pixel transmittance across the disc, no double horizon dimming, limb law normalised by its mean, the Moon's own radius at its horizon, near body hides far body, analytic pixel footprint with flux-conserving sub-pixel discs (σ shared with the stars). Needs #141 PR 1 (disc radius and limb law as uniforms with CPU-derived `cos ρ`, Ω, mean) or carries its sun/Moon part | **yes**: sunsets redder at the lower limb, disc +25 % energy, smoother edges | Observer: disc energy within 1 % at 3, 8 and 50 px; failing-first by reverting the normalisation; Pic du Midi and sun-behind-ridge renders opened |
| **S2** | Limb darkening as body data: `LimbDarkening` with `Power(α)` per channel for the Sun (Hestroffer & Magnan); `Linear(0.6)` stays available | **yes** (deliberate: coloured limb) | radial profile of a high-sun telephoto disc vs Neckel & Labs ratios at x = 0, 0.5, 0.9, 0.95; disc integral = `[1,1,1] × T` |
| **S3** (= #141's 3b) | Refraction: refractivity in `Medium`, f64 ray-traced table per eye height, apparent→true for disc pixels, stars and the light direction; per-channel dispersion (green rim as a consequence) | **yes**: sun up ~2–4 min longer, flattened discs, lit peaks after geometric sunset | table vs Bennett ≤ 0.1′ above 5°, ≤ 1′ at 0°, Saemundsson inverse ≤ 0.2′; pressure scaling at 2 900 m; oblateness 0.8 ± 0.03 at the horizon; Pic du Midi render opened |
| **S4** | HDR in the viewer: `Rgba16Float` MSAA own target, linear fragments, shared `present` module (glare + grade + roll-off + OETF) for film and viewer; depth→visibility fraction for glare (film: from its visibility buffer, replacing `hidden()`); HostPass by colour format | viewer: **yes** (glare appears, resolve in HDR); film: only the glare's occlusion becomes fractional | sun-behind-ridge: glare fades continuously over ≥ 10 frames as the disc sets behind the ridge (no step); tuile-bevy headless tests in both `hdr` modes; a frame-time bench desktop and phone |
| **S5** | Moon: Lommel-Seeliger × surge × albedo map, mean-normalised to `moon_irradiance`; libration and position angle from `astro`; RGB albedo (moonlight colour); earthshine from the Earth's phase and albedo | **yes** (textured Moon, redder moonlight) | disc integral = `moon_irradiance` within 1 % at α = 0°, 30°, 90°, 150°; position angle of the bright limb vs `astro::lunar::bright_limb` within 1°; Mare Crisium's limb distance vs a libration chart; full-moon rise and crescent renders opened |
| **S6** | Bloom (mip chain, energy-conserving), film and viewer, look value; or not — a look decision | yes if adopted | energy before/after the blur equal to 0.1 %; city-at-night render |
| **S7** | Eclipses: Earth's shadow on the Moon (penumbra overlap, umbra colour from the transmittance table and the refraction table), Moon's shadow on the ground and the sun's disc (#141 PR 3's overlap) | only during eclipses | 2026‑08‑28 partial lunar eclipse and 2026‑08‑12 total solar eclipse renders; umbra luminance in Danjon's range for today's aerosols |
| — | Tone of highlights (§3.6): a decision, then its own PR on top of S4 | yes | the four scenes under each candidate |

S1 and S3 can run in parallel after S0; S4 is independent of S1–S3 in code
but should land after S1 so the glare's input is the right energy; S5 after
S1 (footprint, horizon) and before S7.

### 6.1 Acceptance scenes

| Scene | Parameters | What it checks |
|---|---|---|
| **Pic du Midi sunset** | 2026‑06‑21 19:41:54 UTC, lon 0.1418, lat 42.9365, alt 2 900 m, heading 290, pitch 3. The sun is then ≈ 0.8° below the geometric horizontal (az ≈ 304°, computed for this study with a low-precision algorithm — tuile's `Sun` is the reference), above the sea-level horizon's 1.73° dip; refraction at 0.72 atm lifts it by ≈ 0.4–0.5°, about one disc | per-pixel reddening, flattening, the lower limb dimmer than the upper, ground lit by a refracted sun, glare |
| **Full-moon rise** | the full Moon of 2026‑10‑26, moonrise at the Pic du Midi (instant from `Moon::at_unix_seconds`, frozen in the scene), heading to the moonrise azimuth, a 10° vfov telephoto and a 35° one | Moon colour through the air, flattening, albedo map orientation, opposition brightness, exposure under a twilight sky |
| **Thin crescent at dusk** | ≈ 1.5 days after the new Moon of 2026‑11‑09, at civil dusk, west-south-west, 10° vfov | earthshine on the dark side, crescent orientation (bright limb towards the sun), sub-pixel horn tips, Moon above a coloured horizon |
| **Sun behind a ridge** | Pic du Midi or a valley site facing a ridge with the sun 0.5–2° above the geometric horizon, clock running so the disc crosses the ridge line over ~2 min of scene time | glare occlusion fractional and continuous, no flash, no black sky or ground at the ridge |
| Free extras | 2026‑08‑12 total solar eclipse (northern Spain, near sunset); 2026‑08‑28 partial lunar eclipse (Europe, dawn) | S7 |

Every scene: film render + viewer screenshot, opened, side by side with the
previous main; mp4s moved to `videos/` with their parameters in
`videos/README.md`.

---

## 7. Tests

- **Reduction.** Where a PR does not mean to move the Earth's output, the
  digests of S0 and #141 PR 1 are unchanged. Where it does (S1–S5), the
  digest moves and the commit says which pixels and why; the *other* digests
  (atmosphere off, discs out of frame) must not move.
- **Disc energy** (S0 Observer, then a unit test on the WGSL through the CPU
  twin where one exists): ∑ disc radiance × pixel solid angle = irradiance ×
  transmittance × visible fraction, within 1 %, at 1, 3, 8 and 50 px across.
  Failing first: revert the normalisation, watch it fail at 0.8.
- **Limb law**: per-channel mean = `2/(α + 2)` analytically; the drawn
  profile against the law at five radii.
- **Refraction**: the table against Bennett and Saemundsson (above), against
  the pressure-temperature factor, monotonicity, continuity below the
  horizontal for elevated eyes, and a reverse check (true → apparent →
  true).
- **Moon**: disc integral vs `moon_irradiance` at four phase angles; bright-limb
  position angle and illuminated fraction vs `astro` (`bright_limb`,
  `illum_frac_frm_ecl_coords`); libration vs Meeus's worked example (ch. 53,
  1992 April 12); earthshine ratio vs Goode et al.'s order of magnitude.
- **Eclipses**: disc-overlap fraction vs the closed form; Moon occluding the
  sun per pixel (a sun pixel under the Moon is the Moon's).
- **Glare occlusion**: V goes from 1 to 0 monotonically as a synthetic depth
  edge sweeps the disc.
- **Screenshots opened** for every scene in every PR — the counter is not the
  picture.

What these do not cover: absolute colours under a real camera (no
radiometric reference of a sunset is used — that is the colorimetry work),
and any adapter other than the one each digest was frozen on.

---

## 8. Decisions for Laurent

1. **Colorimetry / tone of highlights (§3.6).** Keep the hue-keeping
   roll-off, add a path to white above the knee, or adopt a filmic curve
   (AgX/ACES). Only proposed; decide on renders of the four scenes.
2. **Goldens that move.** S1 (disc energy +25 %, per-pixel transmittance,
   smoother edges), S2 (coloured limb), S3 (refraction: discs and lighting
   near the horizon), S4 (fractional glare occlusion in the film), S5 (Moon
   texture, moonlight colour) each re-freeze the digests with the atmosphere
   on. Agree that the Earth configuration's reduction holds *except* for these
   documented changes, or ask for any of them to be a separate configuration.
3. **Moon texture licence.** Build from PDS `WAC_HAPKE` / `WAC_GLOBAL`
   (public domain per the LROC terms for PDS data) with our own script, or use
   the NASA SVS CGI Moon Kit (simpler, likely unrestricted under NASA's media
   guidelines, but its item page must be checked, and it is a derived
   product). Shipping either needs your go; the lab may use either.
4. **HostPass contract (§5.3).** A float host target receives scene-linear
   pre-exposed radiance and finishes it itself (or calls tuile's post pass);
   an 8-bit target keeps today's graded output. It changes what tuile-bevy
   with `Hdr` shows.
5. **The Moon's glare factor 0.2** in the film's `present`: a documented look
   value, or removed.
6. **Bloom (S6)** at all, and in which renderers.
7. **Earthshine and moonlight colour** derived from the configuration (Earth
   albedo, Moon RGB albedo) rather than today's constants — deliberate
   changes to the Earth's output.

---

## Sources

- Neckel H., Labs D. 1994, *Solar limb darkening 1986–1990 (λλ 303–1099 nm)*,
  Solar Physics 153, 91 — [doi:10.1007/BF00712494](https://doi.org/10.1007/BF00712494)
- Neckel H. 2005, analytical wavelength functions of the limb-darkening
  coefficients, Solar Physics 229 — via [ADS](https://ui.adsabs.harvard.edu/)
- Hestroffer D., Magnan C. 1998, *Wavelength dependency of the Solar limb
  darkening*, A&A 333, 338 — [ADS](https://ui.adsabs.harvard.edu/abs/1998A%26A...333..338H);
  the α(λ) relation as quoted in [arXiv:2407.21583](https://arxiv.org/abs/2407.21583)
- Pierce A. K., Slaughter C. D. 1977, *Solar limb darkening*, Solar Physics 51, 25
- [Limb darkening (overview)](https://en.wikipedia.org/wiki/Limb_darkening)
- Claret A. 2000, *A new non-linear limb-darkening law for LTE stellar
  atmosphere models*, A&A 363, 1081 — [ADS](https://ui.adsabs.harvard.edu/abs/2000A%26A...363.1081C)
- Kasten F., Young A. T. 1989, *Revised optical air mass tables and
  approximation formula*, Applied Optics 28, 4735 — [doi:10.1364/AO.28.004735](https://doi.org/10.1364/AO.28.004735)
- Bennett G. G. 1982, *The calculation of astronomical refraction in marine
  navigation*, J. Navigation 35, 255 — [doi:10.1017/S0373463300022037](https://doi.org/10.1017/S0373463300022037);
  Sæmundsson Þ. 1986, Sky & Telescope 72, 70; Meeus J., *Astronomical
  Algorithms*, ch. 16 (refraction), ch. 48 (illuminated fraction, bright
  limb), ch. 53 (physical ephemeris of the Moon);
  [Atmospheric refraction](https://en.wikipedia.org/wiki/Atmospheric_refraction)
- Ciddor P. E. 1996, *Refractive index of air: new equations for the visible
  and near infrared*, Applied Optics 35, 1566 — [doi:10.1364/AO.35.001566](https://doi.org/10.1364/AO.35.001566)
- Young A. T., *Green flashes and mirages* — [aty.sdsu.edu/explain](https://aty.sdsu.edu/explain/)
- Hapke B. 1981, *Bidirectional reflectance spectroscopy 1. Theory*, JGR 86,
  3039 — [doi:10.1029/JB086iB04p03039](https://doi.org/10.1029/JB086iB04p03039);
  Hapke B. 2012, *Theory of Reflectance and Emittance Spectroscopy*, 2nd ed.,
  Cambridge University Press
- McEwen A. S. 1991, *Photometric functions for photoclinometry and other
  applications*, Icarus 92, 298 — [doi:10.1016/0019-1035(91)90053-V](https://doi.org/10.1016/0019-1035(91)90053-V)
- Kieffer H. H., Stone T. C. 2005, *The spectral irradiance of the Moon*, AJ
  129, 2887 — [doi:10.1086/430185](https://doi.org/10.1086/430185)
- Sato H. et al. 2014, resolved Hapke parameter maps of the Moon — [LROC post](https://www.lroc.asu.edu/posts/801)
- Goode P. R. et al. 2001, *Earthshine observations of the Earth's
  reflectance*, GRL 28, 1671 — [doi:10.1029/2000GL012580](https://doi.org/10.1029/2000GL012580)
- García Muñoz A., Pallé E. 2011, *Lunar eclipse theory revisited*, JQSRT
  112, 1609 — [doi:10.1016/j.jqsrt.2011.03.018](https://doi.org/10.1016/j.jqsrt.2011.03.018)
- Archinal B. A. et al. 2018, *Report of the IAU WGCCRE: 2015*, Celestial
  Mechanics and Dynamical Astronomy 130, 22 — [doi:10.1007/s10569-017-9805-5](https://doi.org/10.1007/s10569-017-9805-5)
- LROC terms of use — [lroc.im-ldi.com/about/terms](https://lroc.im-ldi.com/about/terms);
  `WAC_HAPKE` — [data.lroc.im-ldi.com/lroc/view_rdr/WAC_HAPKE](https://data.lroc.im-ldi.com/lroc/view_rdr/WAC_HAPKE),
  [README](https://pds.lroc.im-ldi.com/data/LRO-L-LROC-5-RDR-V1.0/LROLRC_2001/DATA/MDR/WAC_HAPKE/WAC_HAPKE_README.TXT);
  `WAC_GLOBAL` — [README](https://pds.lroc.im-ldi.com/data/LRO-L-LROC-5-RDR-V1.0/LROLRC_2001/DATA/BDR/WAC_GLOBAL/WAC_GLOBAL_README.TXT);
  USGS — [WAC global morphology mosaic 100 m](https://astrogeology.usgs.gov/search/map/moon_lro_lroc_wac_global_morphology_mosaic_100m);
  Clementine UVVIS — [PDS volume](https://pds.nasa.gov/data/clem1-l-u-5-dim-uvvis-v1.0/cl_4001/document/volinfo.htm);
  NASA SVS CGI Moon Kit — [svs.gsfc.nasa.gov/4720](https://svs.gsfc.nasa.gov/4720);
  NASA media guidelines — [nasa.gov](https://www.nasa.gov/nasa-brand-center/images-and-media/)
- Hillaire S. 2020, *A Scalable and Production Ready Sky and Atmosphere
  Rendering Technique*, EGSR — [PDF](https://sebh.github.io/publications/egsr2020.pdf),
  [code](https://github.com/sebh/UnrealEngineSkyAtmosphere)
- Bruneton E. 2017, *Precomputed atmospheric scattering* (new
  implementation) — [code](https://github.com/ebruneton/precomputed_atmospheric_scattering)
- Hillaire S. 2016, *Physically Based Sky, Atmosphere and Cloud Rendering in
  Frostbite* — [EA](https://www.ea.com/frostbite/news/physically-based-sky-atmosphere-and-cloud-rendering);
  Lagarde S., de Rousiers C. 2014, *Moving Frostbite to PBR* — [notes](https://seblagarde.wordpress.com/2015/07/14/siggraph-2014-moving-frostbite-to-physically-based-rendering/)
- Jensen H. W. et al. 2001, *A Physically-Based Night Sky Model*, SIGGRAPH —
  [doi:10.1145/383259.383306](https://doi.org/10.1145/383259.383306)
- Spencer G., Shirley P., Zimmerman K., Greenberg D. 1995, *Physically-based
  glare effects for digital images*, SIGGRAPH — [doi:10.1145/218380.218466](https://doi.org/10.1145/218380.218466)
- Ritschel T., Ihrke M., Frisvad J. R., Coppens J., Myszkowski K., Seidel
  H.-P. 2009, *Temporal Glare: Real-Time Dynamic Simulation of the Scattering
  in the Human Eye*, Computer Graphics Forum 28(2) — [doi:10.1111/j.1467-8659.2009.01357.x](https://doi.org/10.1111/j.1467-8659.2009.01357.x)
- Jimenez J. 2014, *Next Generation Post Processing in Call of Duty: Advanced
  Warfare*, SIGGRAPH Advances — [iryoku.com](https://www.iryoku.com/next-generation-post-processing-in-call-of-duty-advanced-warfare)
- Unreal Engine — [Sky Atmosphere](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-in-unreal-engine),
  [properties](https://dev.epicgames.com/documentation/unreal-engine/sky-atmosphere-component-properties-in-unreal-engine),
  [`bPerPixelAtmosphereTransmittance`](https://docs.unrealengine.com/4.27/en-US/API/Runtime/Engine/Components/UDirectionalLightComponent/bPerPixelAtmosph-/index.html)
- Stellarium — [RefractionExtinction.cpp](https://github.com/Stellarium/stellarium/blob/master/src/core/RefractionExtinction.cpp) (GPL: read, never copied);
  Celestia — [repository](https://github.com/CelestiaProject/Celestia) (GPL: idem)
- WebGL `EXT_color_buffer_float` — [Khronos registry](https://registry.khronos.org/webgl/extensions/EXT_color_buffer_float/)
