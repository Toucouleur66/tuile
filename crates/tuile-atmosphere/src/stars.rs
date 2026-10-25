// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The stars: where they are, how much light each sends, its colour, and
//! the table a shader reads them from.
//!
//! # The catalogue
//!
//! The Bright Star Catalogue, 5th Revised Edition (Hoffleit & Warren 1991,
//! CDS catalogue V/50): every star of the sky to about magnitude 6.5, the
//! naked eye's limit — 9 096 with a position and a magnitude: their J2000
//! places, visual magnitudes V and colour indices B−V. It is kept as typed
//! Parquet by the `tuile-star-catalogue` tool, which also bins it into the
//! table this crate embeds (`stars.table`): Parquet never reaches a
//! renderer or a wasm build. The provenance and terms are in `NOTICE`.
//! Proper motion is left out of the table: over the decades either side of
//! 2000 the fastest of these stars moves by an arcminute, a pixel of a wide
//! picture.
//!
//! # Light
//!
//! A star of magnitude V sends `10^(−0.4 (V − V☉))` of the sun's
//! irradiance outside the air, V☉ = −26.76 the sun's own (Willmer 2018,
//! *ApJS* 236, 47) — the unit every radiance of the atmosphere is counted
//! in. V is the eye's band, as near as photometry has it to luminance: the
//! star's colour is laid on with a luminance of one, so that the colour
//! moves no light between the bands the eye weighs.
//!
//! Its colour is a black body's: B−V to an effective temperature by
//! Ballesteros's formula (2012, *EPL* 97, 34008), the black body's light
//! seen through the CIE 1931 observer — Wyman, Sloan & Shirley's analytic
//! fit to its functions (2013, *JCGT* 2(2)) — in linear sRGB, divided by
//! the sun's (5 772 K, IAU 2015 B3): the sun's own colour is white in units
//! of the sun.
//!
//! # Seen
//!
//! What the eye sees of it is that irradiance through the air — the same
//! transmittance the sun's light comes through — spread by the camera into
//! a Gaussian of a pixel's size, `∫ = 1`: energy-conserving at any place a
//! star falls on the pixel grid. Nothing hides a star but what stands
//! before it, the planet, the terrain, the sun's or the Moon's disc: by day
//! it is there, a few millionths of the blue sky's light in its pixel, and
//! the camera's response decides whether it shows.
//!
//! # The table
//!
//! [`StarTable`] lays the stars on a cube about the celestial sphere,
//! [`STAR_CELLS`]² cells a face, and lists in each cell every star within
//! [`STAR_REACH`] of it — so that a line of sight reads one cell and finds
//! every star whose image reaches it. One `Rgba32Float` texture holds the
//! cells' lists and the stars, read by `stars_seen` in `hillaire.wgsl`.

use std::sync::OnceLock;

use glam::{DMat3, DVec3};

/// The sun's apparent visual magnitude (Willmer 2018).
pub const SUN_V: f64 = -26.76;

/// The sun's effective temperature, kelvin (IAU 2015 Resolution B3).
pub const SUN_TEMPERATURE: f64 = 5_772.0;

/// Cells along each edge of a face of the cube the table is laid on.
pub const STAR_CELLS: u32 = 32;

/// How far a star's image reaches, radians: half a degree, four times its
/// radius in a picture 540 pixels high of a 90° field. Beyond it a star is
/// not drawn, and a cell lists every star within it of any of its lines of
/// sight.
pub const STAR_REACH: f64 = 0.5 * std::f64::consts::PI / 180.0;

/// The colour indices the table holds a colour for: B−V from −0.4 in steps
/// of [`STAR_COLOUR_STEP`], one texel each.
pub const STAR_COLOURS: u32 = 54;
pub const STAR_COLOUR_STEP: f64 = 0.05;
const STAR_COLOUR_FIRST: f64 = -0.4;
/// The B−V a star without one is drawn with: the sun's.
const SUN_B_V: f64 = 0.65;

/// Texels across the table.
pub const STAR_TABLE_WIDTH: u32 = 256;

/// The instant of J2000.0, Unix seconds UTC (2000-01-01 11:58:55.816 UTC,
/// 12:00 TT).
pub const J2000_UNIX: f64 = 946_727_935.816;

const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// One star of the catalogue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Star {
    /// Its Bright Star Catalogue number.
    pub hr: u32,
    /// Towards it, J2000 equatorial axes: `x` to the equinox, `z` to the
    /// north pole.
    pub direction: DVec3,
    /// Its visual magnitude.
    pub v: f64,
    /// Its colour index, where the catalogue has one.
    pub b_v: Option<f64>,
}

impl Star {
    /// Its irradiance outside the air, against the sun's.
    pub fn irradiance(&self) -> f64 {
        irradiance_of(self.v)
    }

    /// Its colour in units of the sun's, luminance one, as the table holds
    /// it: its colour index to the nearest [`STAR_COLOUR_STEP`]. A star
    /// without a colour index is given the sun's.
    pub fn colour(&self) -> [f64; 3] {
        colour_of(temperature_of(colour_index(self.colour_bucket())))
    }

    fn colour_bucket(&self) -> u32 {
        let b = self.b_v.unwrap_or(SUN_B_V);
        ((b - STAR_COLOUR_FIRST) / STAR_COLOUR_STEP)
            .round()
            .clamp(0.0, f64::from(STAR_COLOURS - 1)) as u32
    }

    /// Its magnitude to the hundredth, as the table holds it: `V + 2` in
    /// hundredths, from 0 to 1023.
    fn magnitude_code(&self) -> u32 {
        ((self.v + 2.0) * 100.0).round().clamp(0.0, 1023.0) as u32
    }
}

/// The colour index of the table's colour `bucket`.
fn colour_index(bucket: u32) -> f64 {
    STAR_COLOUR_FIRST + f64::from(bucket) * STAR_COLOUR_STEP
}

/// The light of magnitude `v` outside the air, against the sun's.
pub fn irradiance_of(v: f64) -> f64 {
    10f64.powf(-0.4 * (v - SUN_V))
}

/// The effective temperature, kelvin, of a star of colour index `b_v`
/// (Ballesteros 2012).
pub fn temperature_of(b_v: f64) -> f64 {
    let b = b_v.clamp(-0.4, 2.2);
    4_600.0 * (1.0 / (0.92 * b + 1.7) + 1.0 / (0.92 * b + 0.62))
}

/// A black body's colour at `kelvin`, linear sRGB, in units of the sun's
/// and of luminance one.
pub fn colour_of(kelvin: f64) -> [f64; 3] {
    let star = black_body_rgb(kelvin);
    let sun = black_body_rgb(SUN_TEMPERATURE);
    let c = [0, 1, 2].map(|i| (star[i] / sun[i]).max(0.0));
    let l: f64 = (0..3).map(|i| c[i] * LUMA[i]).sum();
    c.map(|v| v / l.max(1e-12))
}

/// A black body's light at `kelvin` through the CIE 1931 observer, in
/// linear sRGB, at any scale.
fn black_body_rgb(kelvin: f64) -> [f64; 3] {
    // Wyman, Sloan & Shirley 2013, the multi-lobe fit: a piecewise Gaussian
    // `g(λ; μ, σ below, σ above)`.
    fn g(l: f64, mu: f64, below: f64, above: f64) -> f64 {
        let s = if l < mu { below } else { above };
        (-0.5 * ((l - mu) / s).powi(2)).exp()
    }
    let mut xyz = [0.0f64; 3];
    let mut l = 380.0;
    while l <= 780.0 {
        let x = 1.056 * g(l, 599.8, 37.9, 31.0) + 0.362 * g(l, 442.0, 16.0, 26.7)
            - 0.065 * g(l, 501.1, 20.4, 26.2);
        let y = 0.821 * g(l, 568.8, 46.9, 40.5) + 0.286 * g(l, 530.9, 16.3, 31.1);
        let z = 1.217 * g(l, 437.0, 11.8, 36.0) + 0.681 * g(l, 459.0, 26.0, 13.8);
        // Planck's law, its constants dropped: only ratios are kept.
        let m = l * 1e-9;
        let planck = 1.0 / (m.powi(5) * ((1.438_776_9e-2 / (m * kelvin)).exp() - 1.0));
        xyz[0] += x * planck;
        xyz[1] += y * planck;
        xyz[2] += z * planck;
        l += 5.0;
    }
    let [x, y, z] = xyz;
    [
        3.2406 * x - 1.5372 * y - 0.4986 * z,
        -0.9689 * x + 1.8758 * y + 0.0415 * z,
        0.0557 * x - 0.2040 * y + 1.0570 * z,
    ]
}

/// The turn from J2000 equatorial axes — the catalogue's — to the Earth's
/// at a UTC instant: precession from J2000 to the date (Lieske's angles, as
/// `astro` evaluates them, Meeus ch. 21), then the apparent sidereal time
/// ([`crate::celestial_to_ecef`]). Nutation's few arcseconds are left in
/// the sidereal time alone.
pub fn j2000_to_ecef(utc: f64) -> DMat3 {
    const UNIX_EPOCH_JD: f64 = 2_440_587.5;
    const J2000_JD: f64 = 2_451_545.0;
    let jd = UNIX_EPOCH_JD + utc / 86_400.0;
    let jde = jd + crate::moon::delta_t(jd) / 86_400.0;
    let precessed = |ra: f64, dec: f64| {
        let (a, d) = astro::precess::precess_eq_coords(ra, dec, J2000_JD, jde);
        DVec3::new(d.cos() * a.cos(), d.cos() * a.sin(), d.sin())
    };
    let half_pi = std::f64::consts::FRAC_PI_2;
    let precession = DMat3::from_cols(
        precessed(0.0, 0.0),
        precessed(half_pi, 0.0),
        precessed(0.0, half_pi),
    );
    crate::celestial_to_ecef(utc) * precession
}

/// The cell of the table's cube a unit direction falls in. `cell_of` in
/// `hillaire.wgsl` is the same function.
pub fn cell_of(c: DVec3) -> u32 {
    let a = c.abs();
    let (face, major, u, v) = if a.x >= a.y && a.x >= a.z {
        (u32::from(c.x <= 0.0), a.x, c.y, c.z)
    } else if a.y >= a.z {
        (2 + u32::from(c.y <= 0.0), a.y, c.x, c.z)
    } else {
        (4 + u32::from(c.z <= 0.0), a.z, c.x, c.y)
    };
    let n = f64::from(STAR_CELLS);
    let at = |t: f64| ((t / major * 0.5 + 0.5) * n).floor().clamp(0.0, n - 1.0) as u32;
    face * STAR_CELLS * STAR_CELLS + at(v) * STAR_CELLS + at(u)
}

const STAR_TABLE_MAGIC: &[u8; 8] = b"STARS1\0\0";

/// The texels before the stars: one a cell, its first entry and how many.
pub const STAR_INDEX_TEXELS: u32 = 6 * STAR_CELLS * STAR_CELLS;

/// The first texel of the cells' entries, past the cells and the colours.
pub const STAR_ENTRY_FIRST: u32 = STAR_INDEX_TEXELS + STAR_COLOURS;

/// The stars laid out for a shader: an `Rgba32Float` texture
/// [`STAR_TABLE_WIDTH`] texels across, read in order. Texel `i <`
/// [`STAR_INDEX_TEXELS`] is cell `i`'s `(first, count, 0, 0)`; then
/// [`STAR_COLOURS`] colours, one a colour index; then one texel an entry
/// of a cell's list: the star's J2000 direction, and in `w`, exactly as an
/// integer, its magnitude code plus 1024 times its colour's.
#[derive(Debug, Clone, PartialEq)]
pub struct StarTable {
    pub width: u32,
    pub height: u32,
    pub texels: Vec<[f32; 4]>,
}

impl StarTable {
    /// The table of `stars`.
    pub fn of(stars: &[Star]) -> Self {
        let cells = STAR_INDEX_TEXELS as usize;
        let mut lists: Vec<Vec<usize>> = vec![Vec::new(); cells];
        for (i, star) in stars.iter().enumerate() {
            let s = star.direction.normalize();
            let a = s.any_orthonormal_vector();
            let b = s.cross(a);
            let mut touched = vec![cell_of(s)];
            for (ring, steps) in [(STAR_REACH, 36), (STAR_REACH * 0.5, 18)] {
                for k in 0..steps {
                    let phi = std::f64::consts::TAU * f64::from(k) / f64::from(steps);
                    let d = s * ring.cos() + (a * phi.cos() + b * phi.sin()) * ring.sin();
                    touched.push(cell_of(d));
                }
            }
            touched.sort_unstable();
            touched.dedup();
            for c in touched {
                lists[c as usize].push(i);
            }
        }
        let mut texels = Vec::new();
        let mut entries = Vec::new();
        for list in &lists {
            texels.push([entries.len() as f32, list.len() as f32, 0.0, 0.0]);
            entries.extend_from_slice(list);
        }
        for bucket in 0..STAR_COLOURS {
            let c = colour_of(temperature_of(colour_index(bucket)));
            texels.push([c[0] as f32, c[1] as f32, c[2] as f32, 0.0]);
        }
        for &i in &entries {
            let star = &stars[i];
            let d = star.direction.normalize().as_vec3();
            let code = star.magnitude_code() + 1024 * star.colour_bucket();
            texels.push([d.x, d.y, d.z, code as f32]);
        }
        let width = STAR_TABLE_WIDTH;
        let height = (texels.len() as u32).div_ceil(width).max(1);
        texels.resize((width * height) as usize, [0.0; 4]);
        Self {
            width,
            height,
            texels,
        }
    }

    /// An entry's star: its direction, irradiance and colour.
    fn entry(&self, i: usize) -> ([f32; 3], f64, [f32; 3]) {
        let t = self.texels[i];
        let code = t[3] as u32;
        let v = f64::from(code % 1024) * 0.01 - 2.0;
        let colour = self.texels[(STAR_INDEX_TEXELS + code / 1024) as usize];
        (
            [t[0], t[1], t[2]],
            irradiance_of(v),
            [colour[0], colour[1], colour[2]],
        )
    }

    /// The catalogue's table, as `tuile-star-catalogue` made it from the
    /// Bright Star Catalogue: read once.
    pub fn catalogue() -> &'static Self {
        static TABLE: OnceLock<StarTable> = OnceLock::new();
        TABLE.get_or_init(|| {
            Self::from_bytes(include_bytes!("stars.table")).expect("stars.table, as committed")
        })
    }

    /// The table as bytes: `STARS1\0\0`, its width and height (u32), then
    /// its texels, four f32 each — all little-endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 16 * self.texels.len());
        out.extend_from_slice(STAR_TABLE_MAGIC);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        for t in &self.texels {
            for v in t {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    /// A table [`Self::to_bytes`] wrote.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let rest = bytes
            .strip_prefix(STAR_TABLE_MAGIC)
            .ok_or("not a star table")?;
        let word = |i: usize| -> Result<[u8; 4], String> {
            rest.get(4 * i..4 * i + 4)
                .and_then(|w| w.try_into().ok())
                .ok_or_else(|| "a star table cut short".to_owned())
        };
        let width = u32::from_le_bytes(word(0)?);
        let height = u32::from_le_bytes(word(1)?);
        let n = (width as usize) * (height as usize);
        if rest.len() != 8 + 16 * n {
            return Err(format!(
                "a star table of {width} x {height} in {} bytes",
                bytes.len()
            ));
        }
        let texels = (0..n)
            .map(|t| {
                let mut texel = [0.0f32; 4];
                for (c, v) in texel.iter_mut().enumerate() {
                    *v = f32::from_le_bytes(word(2 + 4 * t + c).expect("length checked"));
                }
                texel
            })
            .collect();
        Ok(Self {
            width,
            height,
            texels,
        })
    }

    /// The stars it holds, each once: its direction (J2000), its
    /// irradiance over the sun's and its colour.
    pub fn stars(&self) -> Vec<([f32; 3], f64, [f32; 3])> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        let entries: u32 = (0..STAR_INDEX_TEXELS)
            .map(|c| self.texels[c as usize][1] as u32)
            .sum();
        for e in 0..entries {
            let i = (STAR_ENTRY_FIRST + e) as usize;
            // Some stars of a multiple share a place: told apart by their
            // magnitude and colour.
            if seen.insert(self.texels[i].map(f32::to_bits)) {
                out.push(self.entry(i));
            }
        }
        out
    }

    /// The stars' light along the unit `c` (J2000 axes) outside the air, as
    /// `stars_seen` reads the table, before the air: a radiance in units of
    /// the sun's irradiance, `sigma` the radius of a star's image, radians.
    /// Every star within [`STAR_REACH`] (as a chord) is counted, and no
    /// other.
    pub fn radiance(&self, c: DVec3, sigma: f64) -> [f64; 3] {
        let cell = self.texels[cell_of(c) as usize];
        let k = 1.0 / (2.0 * sigma * sigma);
        let mut sum = [0.0; 3];
        for e in 0..cell[1] as u32 {
            let (d, irradiance, colour) =
                self.entry((STAR_ENTRY_FIRST + cell[0] as u32 + e) as usize);
            let d2 = (c - DVec3::new(f64::from(d[0]), f64::from(d[1]), f64::from(d[2])))
                .length_squared();
            if d2 > STAR_REACH * STAR_REACH {
                continue;
            }
            let w = irradiance * (-d2 * k).exp() * k / std::f64::consts::PI;
            for ch in 0..3 {
                sum[ch] += w * f64::from(colour[ch]);
            }
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The brightest star of the table: Sirius.
    fn brightest() -> ([f32; 3], f64, [f32; 3]) {
        StarTable::catalogue()
            .stars()
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .expect("stars")
    }

    #[test]
    fn the_committed_table_holds_the_naked_eye_sky() {
        // 9 096 stars, two of which — the components of a double — share
        // their place, magnitude and colour to the catalogue's precision.
        let stars = StarTable::catalogue().stars();
        assert_eq!(stars.len(), 9_095);
        // Sirius, V −1.46, at α 101.287°, δ −16.716°.
        let (d, e, _) = brightest();
        assert!((e / irradiance_of(-1.46) - 1.0).abs() < 1e-12);
        let ra = d[1].atan2(d[0]).to_degrees();
        let dec = d[2].asin().to_degrees();
        assert!(
            (ra - 101.2871).abs() < 1e-3 && (dec + 16.7161).abs() < 1e-3,
            "{ra} {dec}"
        );
    }

    #[test]
    fn the_shader_reads_the_table_as_it_is_laid_out() {
        let wgsl = crate::hillaire::WGSL;
        for line in [
            format!("const STAR_CELLS: u32 = {STAR_CELLS}u;"),
            format!("const STAR_TABLE_WIDTH: u32 = {STAR_TABLE_WIDTH}u;"),
            format!("const STAR_INDEX_TEXELS: u32 = {STAR_INDEX_TEXELS}u;"),
            format!("const STAR_ENTRY_FIRST: u32 = {STAR_ENTRY_FIRST}u;"),
            format!("const SUN_V: f32 = {SUN_V};"),
        ] {
            assert!(wgsl.contains(&line), "hillaire.wgsl lacks `{line}`");
        }
        let reach: f64 = wgsl
            .split("const STAR_REACH: f32 = ")
            .nth(1)
            .and_then(|r| r.split(';').next())
            .and_then(|r| r.parse().ok())
            .expect("STAR_REACH in hillaire.wgsl");
        assert!((reach - STAR_REACH).abs() < 1e-9);
    }

    #[test]
    fn a_table_reads_back_what_it_wrote() {
        let t = StarTable::catalogue();
        assert_eq!(&StarTable::from_bytes(&t.to_bytes()).expect("read"), t);
        assert!(StarTable::from_bytes(&t.to_bytes()[..100]).is_err());
    }

    #[test]
    fn magnitude_zero_is_the_documented_fraction_of_the_sun() {
        // Of the sun's 127 klx outside the air, a zero-magnitude star sends
        // 2.5 µlx: the classic 2.54e-6 lx (Allen).
        let lux = irradiance_of(0.0) * 1.27e5;
        assert!((lux - 2.54e-6).abs() < 0.05e-6, "{lux}");
        // Five magnitudes are a hundred times.
        assert!((irradiance_of(1.0) / irradiance_of(6.0) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn colour_follows_the_colour_index_and_the_sun_is_white() {
        // The sun's B−V, 0.65: its own temperature, and white.
        assert!((temperature_of(0.65) - SUN_TEMPERATURE).abs() < 30.0);
        let sun = colour_of(SUN_TEMPERATURE);
        assert!(sun.iter().all(|c| (c - 1.0).abs() < 1e-9), "{sun:?}");
        // Rigel (B−V −0.03) blue, Betelgeuse (1.85) red, both of luminance
        // one.
        let rigel = colour_of(temperature_of(-0.03));
        let betelgeuse = colour_of(temperature_of(1.85));
        assert!(rigel[2] > rigel[0] && betelgeuse[0] > betelgeuse[2] * 2.0);
        for c in [rigel, betelgeuse] {
            let l = c[0] * LUMA[0] + c[1] * LUMA[1] + c[2] * LUMA[2];
            assert!((l - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn precession_turns_the_sky_by_fifty_arcseconds_a_year() {
        // At J2000 the turn is the sidereal time's alone.
        let at = j2000_to_ecef(J2000_UNIX);
        let sidereal = crate::celestial_to_ecef(J2000_UNIX);
        assert!((at * DVec3::Z).angle_between(sidereal * DVec3::Z) < 1e-9);
        // 26.5 years on, the pole has moved 20.04″ a year.
        let t = J2000_UNIX + 26.5 * 365.25 * 86_400.0;
        let p = crate::celestial_to_ecef(t).transpose() * j2000_to_ecef(t);
        let pole = (p * DVec3::Z).angle_between(DVec3::Z).to_degrees() * 3_600.0;
        assert!((pole - 531.0).abs() < 3.0, "{pole}″");
    }

    #[test]
    fn a_star_keeps_its_light_wherever_it_falls_on_the_pixels() {
        // Pixels of a milliradian, a star's image 0.8 of one: the light
        // summed over the pixels is the star's, at any offset.
        let table = StarTable::catalogue();
        let (d, e, _) = brightest();
        let s = DVec3::new(f64::from(d[0]), f64::from(d[1]), f64::from(d[2])).normalize();
        let right = s.any_orthonormal_vector();
        let up = s.cross(right);
        let pixel = 1e-3;
        for (dx, dy) in [(0.0, 0.0), (0.5, 0.5), (0.31, 0.77), (0.5, 0.0)] {
            let mut sum = 0.0;
            for j in -6..=6 {
                for i in -6..=6 {
                    let x = (f64::from(i) + dx) * pixel;
                    let y = (f64::from(j) + dy) * pixel;
                    let c = (s + right * x + up * y).normalize();
                    let l = table.radiance(c, 0.8 * pixel);
                    sum += (l[0] * LUMA[0] + l[1] * LUMA[1] + l[2] * LUMA[2]) * pixel * pixel;
                }
            }
            let ratio = sum / e;
            assert!((ratio - 1.0).abs() < 0.01, "offset ({dx}, {dy}): {ratio}");
        }
    }
}
