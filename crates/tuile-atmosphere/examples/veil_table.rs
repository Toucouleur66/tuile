// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How much of what the eye sees of the ground is the air's own light, from
//! 500 m to 15 km over it, under the air the `TUILE_AIR_*` tuning values
//! describe (today's when none is set): a table to compare settings by,
//! before any render.
//!
//! The eye looks 15° down, across the sun's azimuth (the sun 40° up, to the
//! side: neither behind the camera nor into it), at level ground of
//! reflectance 0.08 — dark country, what decoded imagery stands for under
//! the default look. For each height: the ground's light kept (`T`, the
//! contrast left), and the veil's share of the light reaching the eye
//! (`V = air / (air + T·ground)`). Exposure does not enter: it scales both.
//!
//! `cargo run -p tuile-atmosphere --example veil_table`, then again with
//! settings, e.g. `TUILE_AIR_AOD_550=0.05 cargo run …`.
use glam::DVec3;
use tuile_atmosphere::hillaire::LUT_RADIUS;
use tuile_atmosphere::{AirTuning, LutAtmosphere};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tuning = AirTuning::from_env()?;
    let p = tuning.params;
    println!(
        "AOD550 {:.4}  turbidity {:.3}  H_M {} m  albedo {:.3}  g {}  ground albedo {}",
        p.aerosol_depth(),
        p.turbidity(),
        p.mie_scale_height,
        p.mie_albedo,
        p.mie_anisotropy,
        p.ground_albedo
    );
    let air = LutAtmosphere::new(p);
    let lum = |v: [f64; 3]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
    let s = 40f64.to_radians();
    let sun = DVec3::new(s.cos(), 0.0, s.sin());
    let d = (-15f64).to_radians();
    let dir = DVec3::new(0.0, d.cos(), d.sin());
    for (name, ground_height) in [
        ("plain, ground 150 m", 150.0),
        ("mountain, ground 1500 m", 1500.0),
    ] {
        println!("  {name}");
        let ground_e = {
            let b = air.sunlight(LUT_RADIUS + ground_height, s.sin());
            let sky = air.level_irradiance(ground_height, s.sin());
            [0, 1, 2].map(|c| b[c] * s.sin() + sky[c])
        };
        for over in [500.0, 2000.0, 5000.0, 10_000.0, 15_000.0] {
            let length = over / -d.sin();
            let ray = air.march(ground_height + over, dir, sun, Some(length), 64, false);
            let ground =
                [0, 1, 2].map(|c| 0.08 * ground_e[c] / std::f64::consts::PI * ray.transmittance[c]);
            let veil = lum(ray.inscattered) / (lum(ray.inscattered) + lum(ground));
            println!(
                "    {:>6} m over, {:>5.1} km off: T {:.3}  V {:.2}",
                over,
                length / 1000.0,
                lum(ray.transmittance),
                veil
            );
        }
    }
    Ok(())
}
