// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Every tuning value of the air reaches what the shaders read: read once
//! by a host ([`AirTuning::from_lookup`], as `from_env` does), laid on the
//! look ([`Look::tuned`]), worked into the frame's light and packed into the
//! uniforms. No GPU: the uniforms are plain data.

use std::collections::HashMap;

use glam::DVec3;
use tuile_atmosphere::hillaire::Moment;
use tuile_atmosphere::{AirTuning, AtmosphereParams, Calibration, LutAtmosphere};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::{BakedView, FrameCamera, Look};
use tuile_film_gpu::{AtmosphereUniform, FrameUniform};

fn tuning(pairs: &[(&str, &str)]) -> AirTuning {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    AirTuning::from_lookup(move |k| map.get(k).cloned()).expect("in range")
}

/// The uniforms of one frame 1 000 m over the Garonne, the sun 40° up,
/// under `air`: assembled as the renderer assembles them.
fn uniforms(air: &AirTuning) -> (AtmosphereUniform, FrameUniform, Look) {
    let look = Look {
        atmosphere: Some(AtmosphereParams::default()),
        ..Look::default()
    }
    .tuned(air);
    let params = look.atmosphere.expect("drawn");
    let at = Geodetic {
        lon: 1.44f64.to_radians(),
        lat: 43.6f64.to_radians(),
        height: 150.0,
    };
    let eye = geodetic_to_ecef(Geodetic {
        height: 1150.0,
        ..at
    });
    let e = 40f64.to_radians();
    let to_sun = enu_frame(at) * DVec3::new(0.0, e.cos(), e.sin());
    let model = LutAtmosphere::new(params);
    let reference = LutAtmosphere::new(AtmosphereParams::default());
    let sky = model.light(
        &reference,
        at,
        Moment {
            to_sun_ecef: to_sun,
            moon: None,
            compensation_ev: 0.0,
        },
        &[],
        Calibration {
            sun: look.sun.to_array(),
            sky: look.world.to_array(),
            reflectance: look.reflectance,
            camera: look.camera,
        },
    );
    let atmosphere = AtmosphereUniform::new(
        &params,
        &sky,
        eye,
        1.0,
        None,
        &tuile_film_gpu::sky_look(&look),
    );
    let view = BakedView {
        position: eye.to_array(),
        direction: (-eye.normalize()).to_array(),
        up: [0.0, 0.0, 1.0],
        viewport_px: [64.0, 48.0],
        fovy_rad: 0.8,
    };
    let frame = FrameUniform::new(&FrameCamera::of(&view, 64.0 / 48.0), &look, [64, 48, 1, 0]);
    // The light's own camera, as the look reads it.
    assert_eq!(
        sky.illumination.adaptation, look.camera.white_adaptation,
        "the white balance's adaptation"
    );
    (atmosphere, frame, look)
}

#[test]
fn every_value_of_the_air_reaches_the_shaders_uniforms() {
    let today = uniforms(&AirTuning::default());
    let air = tuning(&[
        ("TUILE_AIR_AOD_550", "0.06"),
        ("TUILE_AIR_MIE_SCALE_HEIGHT", "2000"),
        ("TUILE_AIR_MIE_ALBEDO", "0.95"),
        ("TUILE_AIR_MIE_G", "0.7"),
        ("TUILE_AIR_RAYLEIGH_SCATTERING", "5e-6,13e-6,30e-6"),
        ("TUILE_AIR_RAYLEIGH_SCALE_HEIGHT", "8500"),
        ("TUILE_AIR_OZONE_ABSORPTION", "1e-6,2e-6,1e-7"),
        ("TUILE_AIR_GROUND_ALBEDO", "0.12"),
        ("TUILE_AIR_WHITE_ADAPTATION", "0.6"),
        ("TUILE_AIR_VEIL_BOUND", "3"),
        ("TUILE_AIR_MARCH_STEPS", "16"),
        ("TUILE_AIR_MARCH_STEPS_HIGH", "40"),
        ("TUILE_LOOK_HIGHLIGHT_KNEE", "0.7"),
        ("TUILE_LOOK_EXPOSURE_EV", "0.5"),
        ("TUILE_LOOK_CONTRAST", "1.2"),
        ("TUILE_LOOK_SATURATION", "1.1"),
    ]);
    let (a, f, look) = uniforms(&air);
    let close = |x: f32, y: f64| ((f64::from(x) - y) / y).abs() < 1e-6;
    // The aerosols: their extinction from the AOD over the scale height,
    // their scattering its albedo's share, their thinning and anisotropy.
    assert!(close(a.mie[1], 0.06 / 2000.0), "{:?}", a.mie);
    assert!(close(a.mie[0], 0.95 * 0.06 / 2000.0), "{:?}", a.mie);
    assert!(close(a.mie[2], 1.0 / 2000.0), "{:?}", a.mie);
    assert!(close(a.mie[3], 0.7), "{:?}", a.mie);
    assert_eq!(&a.rayleigh[..3], &[5e-6, 13e-6, 30e-6]);
    assert!(close(a.rayleigh[3], 1.0 / 8500.0));
    assert_eq!(&a.ozone[..3], &[1e-6, 2e-6, 1e-7]);
    assert!(close(a.shape[2], 0.12));
    // The renderer's own settings.
    assert_eq!(a.tuning, [3.0, 16.0, 40.0, 0.0]);
    assert_eq!(f.white_balance[3], 0.7, "the knee");
    assert_eq!(f.world[3], 0.5f32.exp2(), "the exposure");
    assert_eq!(f.sun[3], 1.2, "the contrast");
    assert_eq!(f.to_sun[3], 1.1, "the saturation");
    assert_eq!(look.camera.white_adaptation, 0.6);
    // And every one of them moved something.
    let (ta, tf, _) = today;
    assert_ne!(ta.mie, a.mie);
    assert_ne!(ta.tuning, a.tuning);
    assert_ne!(tf.white_balance, f.white_balance);
}

#[test]
fn todays_air_is_todays_uniforms() {
    let (a, f, look) = uniforms(&AirTuning::default());
    assert_eq!(a.tuning, [6.0, 12.0, 24.0, 0.0]);
    assert_eq!(f.white_balance[3], 0.5);
    assert_eq!(look.atmosphere, Some(AtmosphereParams::default()));
    let p = AtmosphereParams::default();
    assert_eq!(a.mie[0], p.mie_scattering as f32);
    assert_eq!(a.mie[1], (p.mie_scattering + p.mie_absorption) as f32);
}

#[test]
fn the_meter_and_the_exposure_follow_the_camera() {
    // A meter that takes the scene for brighter ground counts the air's own
    // light for less; a camera that follows less of a bright light leaves
    // it brighter. Both reach the frame's exposure.
    let at = Geodetic {
        lon: 1.44f64.to_radians(),
        lat: 43.6f64.to_radians(),
        height: 150.0,
    };
    let eye = geodetic_to_ecef(Geodetic {
        height: 1150.0,
        ..at
    });
    let e = 40f64.to_radians();
    let to_sun = enu_frame(at) * DVec3::new(0.0, e.cos(), e.sin());
    let model = LutAtmosphere::new(AtmosphereParams::default());
    let level = enu_frame(at);
    let metering = tuile_atmosphere::hillaire::metering(
        eye,
        level * DVec3::new(0.0, 1.0, -0.2),
        level * DVec3::new(1.0, 0.0, 0.0),
        level * DVec3::new(0.0, 0.2, 1.0),
        &tuile_atmosphere::AtmosphereFrame::at(at),
        150.0,
    );
    let ev = |air: &AirTuning| {
        let look = Look::default().tuned(air);
        let sky = model.light(
            &model,
            at,
            Moment {
                to_sun_ecef: to_sun,
                moon: None,
                compensation_ev: 0.0,
            },
            &metering,
            Calibration {
                sun: look.sun.to_array(),
                sky: look.world.to_array(),
                reflectance: look.reflectance,
                camera: look.camera,
            },
        );
        (
            sky.illumination.ev100,
            sky.illumination.exposure_offset_ev_under(&look.camera),
        )
    };
    let today = ev(&AirTuning::default());
    let meter = ev(&tuning(&[("TUILE_AIR_METER_REFLECTANCE", "0.3")]));
    assert!(meter.0 < today.0, "{meter:?} against {today:?}");
    let follows = ev(&tuning(&[("TUILE_AIR_EXPOSURE_FOLLOWS", "0.5")]));
    assert_eq!(follows.0, today.0, "the light is the light");
    assert_ne!(follows.1, today.1, "the exposure is the camera's");
}
