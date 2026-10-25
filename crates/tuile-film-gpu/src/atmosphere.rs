// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere on the GPU, as the film draws it: the tables,
//! the sky-view and the uniform are `tuile_atmosphere::gpu`'s — the same
//! the interactive viewer draws with — made in compute here; what is the
//! film's own is what a frame's light is worked out from, and its look.

use glam::DVec3;
pub use tuile_atmosphere::gpu::{
    declarations, layout as shared_layout, AtmosphereGpu, AtmosphereUniform, SkyViewPass,
};
use tuile_atmosphere::AtmosphereParams;

/// What a frame's light was worked out from: the same, to the bit, and the
/// frame's light and its sky-view are the last one's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LightKey {
    pub params: AtmosphereParams,
    pub frame_eye: DVec3,
    /// The frame camera's view: what the light is metered over.
    pub view: [f32; 16],
    pub ground: [f64; 3],
    pub to_sun: DVec3,
    pub moon: Option<tuile_atmosphere::Moonlight>,
    pub instant: Option<f64>,
    pub compensation_ev: f32,
}

/// The film's atmosphere layout: every table, seen by its compute passes.
pub fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    shared_layout(device, wgpu::ShaderStages::COMPUTE, true)
}

/// What the atmosphere reads of a film's look.
pub fn sky_look(look: &tuile_film::Look) -> tuile_atmosphere::SkyLook {
    tuile_atmosphere::SkyLook {
        calibration: tuile_atmosphere::Calibration {
            sun: look.sun.to_array(),
            sky: look.world.to_array(),
            reflectance: look.reflectance,
            camera: look.camera,
        },
        exposure_ev: look.exposure_ev,
        white_balance: look.white_balance.to_array(),
        highlight_knee: look.highlight_knee,
        sun_scale: look.sun_scale,
        moon_scale: look.moon_scale,
        glare: look.glare,
        veil_bound: look.veil_bound,
        march_steps: look.march_steps,
    }
}
