// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The sky-view tables drawn: one triangle over both tables, a fragment a
// texel, the sun's table in the first target and the Moon's in the second.
// The same texel sums as the compute pass's (`sky_view_texel`), for devices
// that have no compute — WebGL2 — and so for every device a viewer runs on.
// Included after `sky_view.wgsl`.

@vertex
fn sky_view_vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    let xy = vec2f(f32((index << 1u) & 2u), f32(index & 2u)) * 2.0 - 1.0;
    return vec4f(xy, 0.0, 1.0);
}

struct SkyViewTargets {
    @location(0) sun: vec4f,
    @location(1) moon: vec4f,
}

@fragment
fn sky_view_fragment(@builtin(position) at: vec4f) -> SkyViewTargets {
    // A fragment's position is its texel's centre: the texel is its floor.
    let texel = sky_view_texel(vec2u(at.xy));
    var out: SkyViewTargets;
    out.sun = vec4f(texel.sun, 1.0);
    out.moon = vec4f(texel.moon, 1.0);
    return out;
}
