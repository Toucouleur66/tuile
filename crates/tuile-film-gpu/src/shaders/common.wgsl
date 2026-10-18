// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// One frame's camera and look. Eye-relative: the eye is the origin, and
// `right`, `up` and `fwd` span the view frustum (right and up already scaled
// by the half-angle), so a pixel's ray is `fwd + x * right + y * up` for its
// NDC `x`, `y`.
struct Frame {
    view_proj: mat4x4f,
    right: vec4f,
    up: vec4f,
    fwd: vec4f,
    // xyz: world radiance (linear); w: exposure scale.
    world: vec4f,
    // xyz: sun strength — a white surface facing it comes out at this.
    // w: the picture's contrast, a power on luminance about middle grey.
    sun: vec4f,
    // xyz: towards the sun. w: the picture's saturation.
    to_sun: vec4f,
    // x, y: internal (supersampled) size; z: supersampling factor; w: whether
    // the frame drew overlays.
    size: vec4u,
    // Eye-relative world to the sun's map: x, y across it, z from 0 nearest
    // the sun to 1.
    sun_view_proj: mat4x4f,
    // x: how much of the sun a shadow takes, 0 for no shadows at all;
    // y: a texel of the map on the ground, in metres; z: its side in texels;
    // w: how far from the eye shadows reach, in metres.
    shadow: vec4f,
    // x: extinction of the air at the eye, per metre; y: one over its scale
    // height; z: one over twice the eye's distance from the Earth's centre;
    // w: whether there is air at all.
    haze: vec4f,
    // xyz: up at the eye. w: a probe — paint what no eye could see as
    // ground, the far side of the planet through a gap.
    local_up: vec4f,
    // xyz: what the air sends to the eye, and the sky at the horizon.
    horizon: vec4f,
    // xyz: the sky straight up.
    zenith: vec4f,
    // xyz: the white balance, gains on the picture's light with the
    // exposure — never on the overlays.
    white_balance: vec4f,
}

// One resident tile, as this frame places it.
struct TileFrame {
    // Tile origin minus eye, narrowed only after the f64 subtraction.
    offset: vec3f,
    // bit 0: has normals; bit 1: has UVs; bit 2: has a texture.
    flags: u32,
    factor: vec4f,
}

const HAS_NORMALS: u32 = 1u;
const HAS_UVS: u32 = 2u;
const HAS_TEXTURE: u32 = 4u;

// `(1 − e^(−d)) / d`: the mean, along a path that climbs `d` scale heights,
// of air thinning as e^(−height), against the air where the path starts.
// By its series where `d` is small: the difference is then of two numbers
// too near one another.
fn mean_thinning(d: f32) -> f32 {
    if (abs(d) < 2e-2) { return 1.0 - d * (0.5 - d / 6.0); }
    return (1.0 - exp(-d)) / d;
}

// A supersampled frame draws a map of the sun for each sample of a pixel,
// up to four (`SUN_MAPS_MAX`), each on its grid moved by these fractions of
// a texel: a thin caster's shadow, which a grid alone draws on whichever
// texels its centres fall in, is averaged over four grids in each pixel.
// Stratified across, and spread along, so that no two maps share a row or
// a column of offsets.
const SUN_MAP_JITTER = array<vec2f, 4>(
    vec2f(-0.375, 0.0),
    vec2f(-0.125, -0.382),
    vec2f(0.125, 0.236),
    vec2f(0.375, -0.146),
);

// How many maps of the sun a frame of `k` samples a side draws.
fn sun_maps(k: u32) -> u32 {
    return min(max(k, 1u) * max(k, 1u), 4u);
}

// How far map `map` is moved, in its clip space, for a frame of `k`
// samples a side and maps `side` texels across: nowhere for a frame of one.
fn sun_map_shift(map: u32, k: u32, side: f32) -> vec2f {
    if (sun_maps(k) <= 1u) {
        return vec2f(0.0);
    }
    return SUN_MAP_JITTER[map % 4u] * 2.0 / side;
}

// The map the sample at `xy` of the supersampled picture reads: one for
// each sample of its pixel.
fn sun_map_of(xy: vec2u, k: u32) -> u32 {
    let kk = max(k, 1u);
    return ((xy.x % kk) + kk * (xy.y % kk)) % sun_maps(kk);
}
