// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere in the interactive renderer: the sky past the
//! ground and the air between the eye and it, lit by the sun and the Moon
//! of a stated instant.
//!
//! The model and the code that draws it are `tuile_atmosphere`'s — its
//! tables, its sky-view pass, its uniform, and `sky_seen`/`ground_seen` in
//! WGSL — the very ones a film renders with. What is this renderer's own is
//! when the light is worked out again:
//!
//! - **the sun and the Moon** are one direction each at the instant (ECEF),
//!   from their ephemerides. Every ground fragment and every line of sight
//!   is lit by them where it is, so one frame holds a day side and a night
//!   side, a terminator, a midnight sun near the pole;
//! - **the frame's light** — the exposure, the white balance, the sky's
//!   harmonic, all metered over what the camera sees
//!   ([`tuile_atmosphere::LutAtmosphere::light`], some milliseconds of CPU)
//!   — is worked out again when the sun or the Moon has moved by
//!   [`LIGHT_SUN_STEP`], the eye by a share of its height, or the view has
//!   turned, and never more often than [`LIGHT_INTERVAL`]: a sun hurried a
//!   thousandfold by a host's clock still costs four of them a second;
//! - **the sky-view tables** (Hillaire's per-eye table) are made again on
//!   the GPU with each light, and in between when the eye has climbed,
//!   descended or travelled enough that the horizon it holds would be off;
//! - the tables of the air itself are a clear day's, read rather than
//!   computed ([`tuile_atmosphere::LutAtmosphere::of`]).
//!
//! Between those, a frame writes one uniform: the air's sphere placed from
//! its eye, and the stars turned to its instant.

use glam::{DMat4, DVec3, Mat4};
use tuile_atmosphere::gpu::{AtmosphereGpu, AtmosphereUniform, SkyViewPass};
use tuile_atmosphere::hillaire::{metering, Moment};
use tuile_atmosphere::{
    AtmosphereFrame, AtmosphereParams, LutAtmosphere, Moon, Moonlight, SkyLook, Sun,
};
use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef, Geodetic};

/// How far, in radians, the sun or the Moon moves before a frame's light
/// is worked out again: a fifth of the sun's disc, twelve seconds of a real
/// sun's day.
pub const LIGHT_SUN_STEP: f64 = 0.05 * std::f64::consts::PI / 180.0;

/// The shortest time between two lights, however fast the sun moves or the
/// eye travels: a quarter of a second.
pub const LIGHT_INTERVAL: web_time::Duration = web_time::Duration::from_millis(250);

/// How far the view may turn, in radians, before the light is metered
/// again over what it now sees.
const LIGHT_VIEW_STEP: f64 = 4.0 * std::f64::consts::PI / 180.0;

/// The share of its height over the ground the eye may move before the
/// light — and the sky-view, its horizon — is worked out again.
const EYE_STEP: f64 = 0.02;

/// How far the sun moves in one frame, radians, for a jump: a degree,
/// four minutes of a real sun — what a host setting its clock does, and no
/// clock running does from one frame to the next.
const JUMP_SUN: f64 = std::f64::consts::PI / 180.0;

/// How many times [`EYE_STEP`] the eye moves in one frame for a jump.
const JUMP_EYE: f64 = 10.0;

/// What the sky of one frame is drawn for: the instant, the eye and what
/// it looks at, in the frame tiles were rebased onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyFrame {
    /// The instant, Unix seconds UTC: where the sun, the Moon and the stars
    /// stand.
    pub instant: f64,
    /// The eye, ECEF metres.
    pub eye: DVec3,
    /// The origin tiles were rebased onto ([`crate::ContentPump::render_origin`]).
    pub render_origin: DVec3,
    /// Unit vectors, ECEF: where the camera looks, and its up.
    pub forward: DVec3,
    pub up: DVec3,
    /// The vertical field of view, radians, and width over height.
    pub fov_y: f64,
    pub aspect: f64,
    /// The picture's height in pixels: how large a star's image is.
    pub height_px: u32,
    /// The ground's height under the eye, metres over the ellipsoid, as far
    /// as it is known: where the light is worked out and what the meter
    /// meets.
    pub ground_height: f64,
}

/// What a frame's light was worked out from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LightKey {
    to_sun: DVec3,
    to_moon: Option<DVec3>,
    eye: DVec3,
    forward: DVec3,
    clearance: f64,
    at: web_time::Instant,
}

/// What the sky-view was made from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewKey {
    eye: DVec3,
    clearance: f64,
}

pub(crate) struct Sky {
    pub(crate) atmosphere: AtmosphereGpu,
    params: AtmosphereParams,
    look: SkyLook,
    light_key: Option<LightKey>,
    view_key: Option<ViewKey>,
    /// The last frame's sun and eye: what tells a jump — a host's clock set
    /// forward, a camera sent across the globe — from motion.
    last: Option<(DVec3, DVec3)>,
    exposure: f32,
    on: bool,
    /// How many times the light was worked out, and the sky-view made: what
    /// a test or a profile reads to see the budget kept.
    pub(crate) lights: u64,
    pub(crate) views: u64,
}

impl Sky {
    pub(crate) fn new(device: &wgpu::Device, full_layout: &wgpu::BindGroupLayout) -> Self {
        // Made in compute where the device has compute; drawn where it has
        // not (WebGL2). The same texel sums either way.
        let pass = if device_has_compute(device) {
            SkyViewPass::Compute
        } else {
            SkyViewPass::Render
        };
        Self {
            atmosphere: AtmosphereGpu::new(device, full_layout, pass),
            params: AtmosphereParams::default(),
            look: SkyLook::default(),
            light_key: None,
            view_key: None,
            last: None,
            exposure: 1.0,
            on: false,
            lights: 0,
            views: 0,
        }
    }

    pub(crate) fn is_on(&self) -> bool {
        self.on
    }

    pub(crate) fn set_air(&mut self, params: AtmosphereParams, look: SkyLook) {
        if (params, look) != (self.params, self.look) {
            self.params = params;
            self.look = look;
            self.light_key = None;
        }
    }

    pub(crate) fn off(&mut self, queue: &wgpu::Queue) {
        if self.on {
            self.on = false;
            queue.write_buffer(
                &self.atmosphere.uniform,
                0,
                bytemuck::bytes_of(&AtmosphereUniform::zeroed_off()),
            );
        }
    }

    /// The frame's uniform, and — when the sun, the Moon or the eye moved
    /// enough — its light and its sky-view made again.
    pub(crate) fn update(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &SkyFrame) {
        let now = web_time::Instant::now();
        let to_sun = Sun::at_unix_seconds(frame.instant).direction_ecef;
        if !to_sun.is_finite() || !frame.eye.is_finite() {
            self.off(queue);
            return;
        }
        let at_eye = ecef_to_geodetic(frame.eye);
        let moon = Moon::seen_from(frame.instant, at_eye);
        let to_moon = moon.map(|m| m.direction_ecef);
        let clearance = (at_eye.height - frame.ground_height).max(1.0);
        let moved = |key: &LightKey| {
            let angle = |a: DVec3, b: DVec3| a.angle_between(b);
            let moon_moved = match (key.to_moon, to_moon) {
                (Some(a), Some(b)) => angle(a, b) > LIGHT_SUN_STEP,
                (None, None) => false,
                _ => true,
            };
            angle(key.to_sun, to_sun) > LIGHT_SUN_STEP
                || moon_moved
                || (frame.eye - key.eye).length() > EYE_STEP * key.clearance.min(clearance)
                || angle(key.forward, frame.forward) > LIGHT_VIEW_STEP
        };
        // A jump since the last frame is answered at once; motion, however
        // fast, once an interval.
        let jumped = self.last.is_some_and(|(sun, eye)| {
            sun.angle_between(to_sun) > JUMP_SUN
                || (frame.eye - eye).length() > JUMP_EYE * EYE_STEP * clearance
        });
        self.last = Some((to_sun, frame.eye));
        let due = match &self.light_key {
            None => true,
            Some(key) => moved(key) && (jumped || now.duration_since(key.at) >= LIGHT_INTERVAL),
        };
        self.atmosphere
            .hold_with(queue, self.params, LutAtmosphere::of);
        if due {
            let model = self.atmosphere.model.as_ref().expect("held above");
            let reference = self.atmosphere.reference.as_ref().expect("held above");
            let ground = Geodetic {
                height: frame.ground_height.clamp(0.0, at_eye.height.max(0.0)),
                ..at_eye
            };
            let (right, up) = view_axes(frame);
            let metered = metering(
                frame.eye,
                frame.forward,
                right,
                up,
                &AtmosphereFrame::at(ground),
                ground.height,
            );
            let light = model.light(
                reference,
                ground,
                Moment {
                    to_sun_ecef: to_sun,
                    moon: moon.map(|m| Moonlight::of(&m)),
                    compensation_ev: 0.0,
                },
                &metered,
                self.look.calibration,
            );
            self.exposure = self.look.exposure_scale(&light);
            self.atmosphere.light = Some(light);
            self.light_key = Some(LightKey {
                to_sun,
                to_moon,
                eye: frame.eye,
                forward: frame.forward,
                clearance,
                at: now,
            });
            self.view_key = None;
            self.lights += 1;
        }
        let light = self.atmosphere.light.expect("worked out above");
        // The sky-view's eye: the light's, or a newer one where the eye has
        // gone far enough from it that the table's horizon would be off.
        let view_stale = match &self.view_key {
            None => true,
            Some(key) => (frame.eye - key.eye).length() > EYE_STEP * key.clearance.min(clearance),
        };
        if view_stale {
            self.view_key = Some(ViewKey {
                eye: frame.eye,
                clearance,
            });
        }
        let view_eye = self.view_key.expect("set above").eye;
        let stars = Some(tuile_atmosphere::celestial_to_ecef(frame.instant).transpose());
        let uniform = AtmosphereUniform::new(
            &self.params,
            &light,
            view_eye,
            self.exposure,
            stars,
            &self.look,
        );
        if view_stale {
            // Written for the table's own pass, from its own eye, then made.
            queue.write_buffer(
                &self.atmosphere.uniform,
                0,
                bytemuck::bytes_of(&uniform.seen_from(&light, view_eye)),
            );
            self.atmosphere.sky_view_stale = true;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tuile sky view"),
            });
            self.atmosphere.record_sky_view(&mut encoder);
            queue.submit([encoder.finish()]);
            self.views += 1;
        }
        let placed = uniform
            .seen_from(&light, frame.eye)
            .with_eye_from_origin((frame.eye - frame.render_origin).as_vec3());
        queue.write_buffer(&self.atmosphere.uniform, 0, bytemuck::bytes_of(&placed));
        self.atmosphere.frame_uniform = placed;
        self.on = true;
    }
}

/// The camera's right and up, ECEF, each scaled to the half-extent of the
/// picture at unit distance — what the meter's rays are spread over.
fn view_axes(frame: &SkyFrame) -> (DVec3, DVec3) {
    let forward = frame.forward.normalize_or(DVec3::X);
    let right = forward
        .cross(frame.up)
        .normalize_or(forward.any_orthonormal_vector());
    let up = right.cross(forward);
    let half = (frame.fov_y * 0.5).tan();
    (right * half * frame.aspect, up * half)
}

/// The radius of a star's image, radians: about a pixel.
pub(crate) fn star_sigma(frame: &SkyFrame) -> f32 {
    let pixel = 2.0 * (frame.fov_y * 0.5).tan() / f64::from(frame.height_px.max(1));
    (0.8 * pixel) as f32
}

/// Whether the device runs compute shaders: everything but WebGL2.
pub(crate) fn device_has_compute(device: &wgpu::Device) -> bool {
    // A device does not report its adapter's downlevel flags; the backend
    // that lacks compute is GL, and its limits say so: no workgroup at all.
    device.limits().max_compute_workgroups_per_dimension > 0
}

/// `view_proj`, inverted: from a pixel back to its line of sight. The
/// identity for a projection that has none — the sky then draws nothing
/// rather than not a number.
pub(crate) fn inverse_view_proj(view_proj: &[f32; 16]) -> Mat4 {
    let inverse = DMat4::from_cols_array(&view_proj.map(f64::from)).inverse();
    let inverse = inverse.as_mat4();
    if inverse.is_finite() {
        inverse
    } else {
        Mat4::IDENTITY
    }
}

/// Where `geodetic` is, for a test or a host placing a camera.
pub fn ecef_of(lon_deg: f64, lat_deg: f64, height: f64) -> DVec3 {
    geodetic_to_ecef(Geodetic {
        lon: lon_deg.to_radians(),
        lat: lat_deg.to_radians(),
        height,
    })
}
