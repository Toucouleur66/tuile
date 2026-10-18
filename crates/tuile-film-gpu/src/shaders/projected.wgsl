// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The shadows of shapes that project theirs (`OverlayShadow::Project`):
// for each pixel of ground, whether its own ray to the sun meets one of
// their triangles. No map, no texel: the answer is exact for the pixel's
// ground, and the picture's own samples are its only grid.
//
// Each triangle is drawn as the prism it sweeps down its sun rays, from
// where it stands to below any ground: whatever ground it shades lies
// inside the prism, and the prism's far side — never nearer the eye than
// that ground — covers its pixel. Nothing is drawn of the prism but the
// question it asks: no depth is tested or written, and the ground is read,
// never fought with. A pixel whose ground the triangle shades is marked;
// the resolve takes the sun from it as it takes it from shaded ground.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var ground: texture_depth_2d;
// Three corners a triangle, eye-relative; w: how far the corner is swept
// down the sun's rays, past any ground under it.
@group(0) @binding(2) var<storage, read> corners: array<vec4f>;

struct Out {
    @builtin(position) clip: vec4f,
    @location(0) @interpolate(flat) triangle: u32,
}

// The prism's faces: (corner, swept) for each vertex of its 8 triangles —
// the triangle, its image down the rays, and a quad a side.
const PRISM = array<vec2u, 24>(
    vec2u(0u, 0u), vec2u(1u, 0u), vec2u(2u, 0u),
    vec2u(0u, 1u), vec2u(2u, 1u), vec2u(1u, 1u),
    vec2u(0u, 0u), vec2u(1u, 0u), vec2u(1u, 1u),
    vec2u(0u, 0u), vec2u(1u, 1u), vec2u(0u, 1u),
    vec2u(1u, 0u), vec2u(2u, 0u), vec2u(2u, 1u),
    vec2u(1u, 0u), vec2u(2u, 1u), vec2u(1u, 1u),
    vec2u(2u, 0u), vec2u(0u, 0u), vec2u(0u, 1u),
    vec2u(2u, 0u), vec2u(0u, 1u), vec2u(2u, 1u),
);

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) triangle: u32) -> Out {
    let at = PRISM[v];
    let corner = corners[3u * triangle + at.x];
    let swept = corner.xyz - normalize(frame.to_sun.xyz) * (corner.w * f32(at.y));
    var out: Out;
    out.clip = frame.view_proj * vec4f(swept, 1.0);
    out.triangle = triangle;
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4f {
    // Reverse-Z, infinitely far: no ground is 0, and ground at depth d
    // along the view's axis is at near / d. The view is a rotation, so
    // near is where the projection put it.
    let z = textureLoad(ground, vec2i(in.clip.xy), 0);
    if (z <= 0.0) {
        discard;
    }
    let d = frame.view_proj[3].z / z;
    let size = vec2f(frame.size.xy);
    let ndc = vec2f(in.clip.x / size.x * 2.0 - 1.0, 1.0 - in.clip.y / size.y * 2.0);
    let p = d * (frame.fwd.xyz + ndc.x * frame.right.xyz + ndc.y * frame.up.xyz);

    // The ground's ray to the sun against the triangle (Möller–Trumbore).
    let s = normalize(frame.to_sun.xyz);
    let a = corners[3u * in.triangle].xyz;
    let e1 = corners[3u * in.triangle + 1u].xyz - a;
    let e2 = corners[3u * in.triangle + 2u].xyz - a;
    let q = cross(s, e2);
    let det = dot(e1, q);
    if (abs(det) < 1e-12) {
        discard;
    }
    let inv = 1.0 / det;
    let t0 = p - a;
    let u = dot(t0, q) * inv;
    let r = cross(t0, e1);
    let v = dot(s, r) * inv;
    let t = dot(e2, r) * inv;
    if (u < 0.0 || v < 0.0 || u + v > 1.0 || t <= 0.0) {
        discard;
    }
    return vec4f(1.0);
}
