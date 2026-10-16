// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The physical atmosphere, as a shader sees it: the GPU twin of
// `hillaire.rs`, where the derivations and the tests live. Hillaire 2020's
// tables, read and marched; the same parametrisations to the letter, in f32,
// heights carried apart from the planet's radius so that no f32 has to
// subtract two radii.
//
// A module that includes this declares:
//
//   var<uniform> atmosphere: Atmosphere;
//   var transmittance_lut: texture_2d<f32>;
//   var multi_scattering_lut: texture_2d<f32>;
//   var lut_sampler: sampler;   // linear, clamped
//
// Units: a radiance is per unit of the sun's irradiance outside the air;
// `atmosphere.illuminance` says what one is in the renderer's units.

struct Atmosphere {
    // xyz: the centre of the sphere the air is laid on, from the eye;
    // w: its radius, in metres.
    centre: vec4f,
    // xyz: Rayleigh scattering at sea level, per metre; w: one over its
    // scale height.
    rayleigh: vec4f,
    // x: aerosol scattering at sea level; y: its extinction; z: one over
    // its scale height; w: its anisotropy.
    mie: vec4f,
    // xyz: ozone's absorption at its densest; w: the height it is densest.
    ozone: vec4f,
    // x: ozone's half width; y: the air's thickness; z: the ground's
    // albedo; w: 1 when the atmosphere is drawn at all.
    shape: vec4f,
    // xyz: towards the sun; w: the height of the frame's eye.
    sun: vec4f,
    // xyz: up at the frame's eye; w: nothing.
    up: vec4f,
    // xyz: what one unit of the model's radiance is in the renderer's.
    illuminance: vec4f,
    // xyz: the radiance the night floor adds to the sky.
    floor: vec4f,
    // The sky's irradiance on a surface of normal n is the frame's `world`
    // plus `sh_x * n.x + sh_y * n.y + sh_z * n.z` (xyz of each).
    sh_x: vec4f,
    sh_y: vec4f,
    sh_z: vec4f,
}

const ATMOSPHERE_PI: f32 = 3.14159265358979;
// The planet radius the tables are laid out for: `hillaire::LUT_RADIUS`.
const LUT_RADIUS: f32 = 6360000.0;
const TRANSMITTANCE_SIZE: vec2f = vec2f(256.0, 64.0);
const MULTI_SCATTERING_SIZE: vec2f = vec2f(32.0, 32.0);
const SKY_VIEW_SIZE: vec2f = vec2f(192.0, 108.0);
const SUN_ANGULAR_RADIUS: f32 = 0.004652;
// Steps of the air between the eye and the ground: `docs/19` keeps the
// march short; the tables carry the light, the steps only the density.
const AERIAL_STEPS: u32 = 12u;
const SKY_STEPS: u32 = 30u;

struct Medium {
    rayleigh: vec3f,
    mie: f32,
    extinction: vec3f,
}

fn medium(height: f32) -> Medium {
    let h = max(height, 0.0);
    let rayleigh = atmosphere.rayleigh.xyz * exp(-h * atmosphere.rayleigh.w);
    let mie_density = exp(-h * atmosphere.mie.z);
    let ozone = max(1.0 - abs(h - atmosphere.ozone.w) / atmosphere.shape.x, 0.0);
    var m: Medium;
    m.rayleigh = rayleigh;
    m.mie = atmosphere.mie.x * mie_density;
    m.extinction = rayleigh + vec3f(atmosphere.mie.y * mie_density)
        + atmosphere.ozone.xyz * ozone;
    return m;
}

fn uv_from_unit(x: vec2f, size: vec2f) -> vec2f {
    return 0.5 / size + x * (1.0 - 1.0 / size);
}

// Bruneton's parametrisation, as `LutAtmosphere::transmittance_unit`, from
// a height rather than a radius: every difference of two radii is written
// as the product it factors into.
fn transmittance_uv(height: f32, mu: f32) -> vec2f {
    let h = max(height, 0.0);
    let thickness = atmosphere.shape.y;
    let r = LUT_RADIUS + h;
    let top = LUT_RADIUS + thickness;
    let big_h = sqrt(thickness * (2.0 * LUT_RADIUS + thickness));
    let rho = sqrt(h * (2.0 * LUT_RADIUS + h));
    let disc = (top - r) * (top + r) + r * r * mu * mu;
    let d = max(-r * mu + sqrt(max(disc, 0.0)), 0.0);
    let d_min = top - r;
    let d_max = rho + big_h;
    let x_mu = clamp((d - d_min) / (d_max - d_min), 0.0, 1.0);
    return uv_from_unit(vec2f(x_mu, rho / big_h), TRANSMITTANCE_SIZE);
}

fn transmittance(height: f32, mu: f32) -> vec3f {
    return textureSampleLevel(transmittance_lut, lut_sampler, transmittance_uv(height, mu), 0.0).rgb;
}

// How much of the sun's disc stands over the horizon: `sun_visible`.
fn sun_visible(height: f32, mu_s: f32) -> f32 {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let horizon = -sqrt(h * (2.0 * LUT_RADIUS + h)) / r;
    let over = asin(clamp(mu_s, -1.0, 1.0)) - asin(horizon);
    return smoothstep(-SUN_ANGULAR_RADIUS, SUN_ANGULAR_RADIUS, over);
}

fn sunlight(height: f32, mu_s: f32) -> vec3f {
    return transmittance(height, mu_s) * sun_visible(height, mu_s);
}

fn multi_scattering(height: f32, mu_s: f32) -> vec3f {
    let x = vec2f(clamp(mu_s * 0.5 + 0.5, 0.0, 1.0), clamp(height / atmosphere.shape.y, 0.0, 1.0));
    return textureSampleLevel(multi_scattering_lut, lut_sampler, uv_from_unit(x, MULTI_SCATTERING_SIZE), 0.0).rgb;
}

fn rayleigh_phase(c: f32) -> f32 {
    return 3.0 / (16.0 * ATMOSPHERE_PI) * (1.0 + c * c);
}

fn mie_phase(c: f32) -> f32 {
    let g = atmosphere.mie.w;
    let g2 = g * g;
    return 3.0 / (8.0 * ATMOSPHERE_PI) * (1.0 - g2) * (1.0 + c * c)
        / ((2.0 + g2) * pow(max(1.0 + g2 - 2.0 * g * c, 1e-6), 1.5));
}

struct AirRay {
    inscattered: vec3f,
    transmittance: vec3f,
}

// One step of a march, integrated as Hillaire does: the source over the
// step's own extinction, exact however long the step.
fn air_step(ray: ptr<function, AirRay>, height: f32, mu_s: f32, phase_r: f32, phase_m: f32, dt: f32) {
    let m = medium(height);
    let light = sunlight(height, mu_s);
    let multi = multi_scattering(height, mu_s);
    let source = m.rayleigh * phase_r * light + vec3f(m.mie * phase_m) * light
        + (m.rayleigh + vec3f(m.mie)) * multi;
    let e = max(m.extinction, vec3f(1e-12));
    let step = exp(-e * dt);
    (*ray).inscattered += (*ray).transmittance * source * (vec3f(1.0) - step) / e;
    (*ray).transmittance *= step;
}

// The air between the eye and `p`, eye-relative: what it lets through of
// the ground and what it adds, in the model's units. Evenly spaced steps,
// the sphere the frame's own.
fn air_between(p: vec3f) -> AirRay {
    var ray: AirRay;
    ray.inscattered = vec3f(0.0);
    ray.transmittance = vec3f(1.0);
    let length_ = length(p);
    if (length_ <= 0.0) { return ray; }
    let d = p / length_;
    let sun = atmosphere.sun.xyz;
    let c = dot(d, sun);
    let phase_r = rayleigh_phase(c);
    let phase_m = mie_phase(c);
    let o = -atmosphere.centre.xyz;
    let dt = length_ / f32(AERIAL_STEPS);
    for (var i = 0u; i < AERIAL_STEPS; i++) {
        let x = o + d * ((f32(i) + 0.5) * dt);
        let r = length(x);
        air_step(&ray, r - atmosphere.centre.w, dot(x, sun) / r, phase_r, phase_m, dt);
    }
    return ray;
}

// Where a ray from radius `r` at zenith cosine `mu` meets the ground, or a
// negative number.
fn ground_distance(r: f32, mu: f32, height: f32) -> f32 {
    let disc = r * r * mu * mu - height * (2.0 * LUT_RADIUS + height);
    if (mu >= 0.0 || disc < 0.0) { return -1.0; }
    return -r * mu - sqrt(disc);
}

// The sky seen from `height` along `dir`, the sun along `sun`, both unit in
// a frame whose z is up: `LutAtmosphere::sky_radiance`, without the sun's
// disc. Steps spread quadratically; the ground, met, lit and seen.
fn sky_radiance(height: f32, dir: vec3f, sun: vec3f) -> vec3f {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let mu = dir.z;
    let top = LUT_RADIUS + atmosphere.shape.y;
    let ground = ground_distance(r, mu, h);
    let disc = (top - r) * (top + r) + r * r * mu * mu;
    var t_max = max(-r * mu + sqrt(max(disc, 0.0)), 0.0);
    if (ground >= 0.0) { t_max = ground; }
    var ray: AirRay;
    ray.inscattered = vec3f(0.0);
    ray.transmittance = vec3f(1.0);
    let c = dot(dir, sun);
    let phase_r = rayleigh_phase(c);
    let phase_m = mie_phase(c);
    let origin = vec3f(0.0, 0.0, r);
    var t_prev = 0.0;
    for (var s = 0u; s < SKY_STEPS; s++) {
        let a = (f32(s) + 0.3) / f32(SKY_STEPS);
        let b = (f32(s) + 1.0) / f32(SKY_STEPS);
        let t = t_max * a * a;
        let t_next = t_max * b * b;
        let dt = t_next - t_prev;
        t_prev = t_next;
        let x = origin + dir * t;
        let pr = length(x);
        air_step(&ray, pr - LUT_RADIUS, dot(x, sun) / pr, phase_r, phase_m, dt);
    }
    if (ground >= 0.0) {
        let x = origin + dir * t_max;
        let up = normalize(x);
        let mu_s = dot(up, sun);
        ray.inscattered += ray.transmittance * sunlight(0.0, mu_s) * max(mu_s, 0.0)
            * atmosphere.shape.z / ATMOSPHERE_PI;
    }
    return ray.inscattered;
}

// The sky-view table's parametrisation (Hillaire 2020 §5.3): across, the
// cosine of the azimuth from the sun, `1 − 2u²`; down, the zenith angle,
// the horizon at the middle and the rows bunched towards it on both sides.
// `sky_view_direction` and `sky_view_uv` are each other's inverse, and
// `hillaire::sky_view_*` their twins.
fn sky_view_direction(unit: vec2f, height: f32) -> vec2f {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let beta = acos(clamp(sqrt(h * (2.0 * LUT_RADIUS + h)) / r, -1.0, 1.0));
    let zenith_horizon = ATMOSPHERE_PI - beta;
    var theta: f32;
    if (unit.y < 0.5) {
        let k = 1.0 - 2.0 * unit.y;
        theta = zenith_horizon * (1.0 - k * k);
    } else {
        let k = 2.0 * unit.y - 1.0;
        theta = zenith_horizon + beta * k * k;
    }
    // (cosine of the zenith angle, cosine of the azimuth from the sun)
    return vec2f(cos(theta), 1.0 - 2.0 * unit.x * unit.x);
}

fn sky_view_uv(height: f32, mu: f32, cos_azimuth: f32) -> vec2f {
    let h = max(height, 0.0);
    let r = LUT_RADIUS + h;
    let beta = acos(clamp(sqrt(h * (2.0 * LUT_RADIUS + h)) / r, -1.0, 1.0));
    let zenith_horizon = ATMOSPHERE_PI - beta;
    let theta = acos(clamp(mu, -1.0, 1.0));
    var y: f32;
    if (theta < zenith_horizon) {
        y = 0.5 * (1.0 - sqrt(max(1.0 - theta / zenith_horizon, 0.0)));
    } else {
        y = 0.5 + 0.5 * sqrt(max((theta - zenith_horizon) / beta, 0.0));
    }
    let x = sqrt(clamp(0.5 - 0.5 * cos_azimuth, 0.0, 1.0));
    return uv_from_unit(vec2f(x, y), SKY_VIEW_SIZE);
}

// The direction `d` (any length) from the frame's eye as the sky-view
// table indexes it.
fn sky_view_uv_of(d: vec3f) -> vec2f {
    let up = atmosphere.up.xyz;
    let n = normalize(d);
    let mu = dot(n, up);
    let flat_d = n - up * mu;
    let sun = atmosphere.sun.xyz;
    let flat_s = sun - up * dot(sun, up);
    var c = 1.0;
    let ld = dot(flat_d, flat_d);
    let ls = dot(flat_s, flat_s);
    if (ld > 1e-12 && ls > 1e-12) { c = dot(flat_d, flat_s) * inverseSqrt(ld * ls); }
    return sky_view_uv(atmosphere.sun.w, mu, c);
}
