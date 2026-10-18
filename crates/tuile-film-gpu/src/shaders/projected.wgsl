// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The shadows of shapes that project theirs (`OverlayShadow::Project`):
// for each pixel of ground, how much of the sun's disc their triangles
// hide. No map, no texel: the answer is worked out for the pixel's own
// ground, and the picture's own samples are its only grid.
//
// The sun seen from a point of ground is a disc `penumbra.x` (the tangent
// of its angular radius) across: at a caster `t` metres up the ray, a disc
// of radius `t · penumbra.x` about where the ray meets it. The share of
// that disc a triangle covers is the shadow it casts there — whole under
// the triangle, none past its edges by more than the radius, and a
// penumbra between, as wide as the caster is high. It is exact for a
// uniform disc, worked out in the plane across the ray: the sum, edge by
// edge, of the disc's part in the wedge from its centre to the edge.
// Summed over triangles that abut, the inner edges' wedges cancel, so a
// mesh's union is covered as one shape: the target adds, and the resolve
// takes at most the whole sun. With no disc (`penumbra.x` 0) the shadow is
// the exact, hard one: whether the ray meets the triangle.
//
// Each triangle is drawn as the volume its penumbra sweeps down the sun's
// rays, from where it stands to below any ground: a box across the rays
// about the triangle, widening down them as its penumbra does. Whatever
// ground the triangle shades lies inside, and the box's far side covers
// its pixel once, whether the eye is outside the box or in it: only the
// faces turned away from the eye are drawn. Nothing of the box is drawn
// but the question it asks: no depth is tested or written, and the ground
// is read, never fought with.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var ground: texture_depth_2d;
// Three corners a triangle, eye-relative; w: how far the corner is swept
// down the sun's rays, past any ground under it.
@group(0) @binding(2) var<storage, read> corners: array<vec4f>;
// x: the tangent of the angular radius of the sun's disc the penumbra is
// cut to; 0 for a hard shadow.
@group(0) @binding(3) var<uniform> penumbra: vec4f;

const PI: f32 = 3.14159265358979;

struct Out {
    @builtin(position) clip: vec4f,
    @location(0) @interpolate(flat) triangle: u32,
}

// The box's faces, each corner `(corner of the rectangle, level)`, wound
// anticlockwise seen from outside: the top towards the sun, the bottom
// down the rays, and a quad a side.
const BOX = array<vec2u, 36>(
    vec2u(0u, 0u), vec2u(1u, 0u), vec2u(2u, 0u),
    vec2u(0u, 0u), vec2u(2u, 0u), vec2u(3u, 0u),
    vec2u(0u, 1u), vec2u(2u, 1u), vec2u(1u, 1u),
    vec2u(0u, 1u), vec2u(3u, 1u), vec2u(2u, 1u),
    vec2u(0u, 0u), vec2u(0u, 1u), vec2u(1u, 1u),
    vec2u(0u, 0u), vec2u(1u, 1u), vec2u(1u, 0u),
    vec2u(1u, 0u), vec2u(1u, 1u), vec2u(2u, 1u),
    vec2u(1u, 0u), vec2u(2u, 1u), vec2u(2u, 0u),
    vec2u(2u, 0u), vec2u(2u, 1u), vec2u(3u, 1u),
    vec2u(2u, 0u), vec2u(3u, 1u), vec2u(3u, 0u),
    vec2u(3u, 0u), vec2u(3u, 1u), vec2u(0u, 1u),
    vec2u(3u, 0u), vec2u(0u, 1u), vec2u(0u, 0u),
);

// Two unit vectors across `s`, the first along `hint` as `s` sees it;
// with `s`, right-handed.
fn across(s: vec3f, hint: vec3f) -> mat2x3f {
    var u = hint - s * dot(hint, s);
    if (dot(u, u) < 1e-12) {
        u = select(vec3f(0.0, 1.0, 0.0), vec3f(1.0, 0.0, 0.0), abs(s.x) < 0.9);
        u = u - s * dot(u, s);
    }
    u = normalize(u);
    return mat2x3f(u, cross(s, u));
}

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) triangle: u32) -> Out {
    let s = normalize(frame.to_sun.xyz);
    let c0 = corners[3u * triangle];
    let c1 = corners[3u * triangle + 1u];
    let c2 = corners[3u * triangle + 2u];
    // Across the rays, along the triangle's longest edge as they see it:
    // the box about a sliver is then as tight as a box can be.
    let e0 = c1.xyz - c0.xyz;
    let e1 = c2.xyz - c1.xyz;
    let e2 = c0.xyz - c2.xyz;
    let l0 = length(e0 - s * dot(e0, s));
    let l1 = length(e1 - s * dot(e1, s));
    let l2 = length(e2 - s * dot(e2, s));
    var hint = e0;
    if (l1 > l0 && l1 >= l2) {
        hint = e1;
    } else if (l2 > l0 && l2 > l1) {
        hint = e2;
    }
    let uv = across(s, hint);
    let q0 = vec2f(dot(c0.xyz, uv[0]), dot(c0.xyz, uv[1]));
    let q1 = vec2f(dot(c1.xyz, uv[0]), dot(c1.xyz, uv[1]));
    let q2 = vec2f(dot(c2.xyz, uv[0]), dot(c2.xyz, uv[1]));
    let lo = min(min(q0, q1), q2);
    let hi = max(max(q0, q1), q2);
    // From the corner nearest the sun to the end of the longest sweep.
    let z = vec3f(dot(c0.xyz, s), dot(c1.xyz, s), dot(c2.xyz, s));
    let top = max(max(z.x, z.y), z.z);
    let reach = max(max(top - z.x + c0.w, top - z.y + c1.w), top - z.z + c2.w);

    let at = BOX[v];
    let level = f32(at.y);
    // The penumbra's radius where the box ends, and a tenth of a metre
    // past the triangle: no ray is lost to the box's edge.
    let grow = level * reach * penumbra.x + 0.1;
    let x = select(lo.x - grow, hi.x + grow, at.x == 1u || at.x == 2u);
    let y = select(lo.y - grow, hi.y + grow, at.x >= 2u);
    let p = uv[0] * x + uv[1] * y + s * (top - level * reach);
    var out: Out;
    out.clip = frame.view_proj * vec4f(p, 1.0);
    out.triangle = triangle;
    return out;
}

// The signed angle from `a` to `b`, none when either is the origin.
fn angle(a: vec2f, b: vec2f) -> f32 {
    let c = a.x * b.y - a.y * b.x;
    let d = dot(a, b);
    if (abs(c) + abs(d) < 1e-20) {
        return 0.0;
    }
    return atan2(c, d);
}

// Of the disc of radius `r` about the origin, the signed area within the
// triangle from the origin to `a` and `b`: a sector where the edge is
// outside the disc, a triangle where it is inside.
fn wedge(a: vec2f, b: vec2f, r: f32) -> f32 {
    let d = b - a;
    let dd = dot(d, d);
    if (dd < 1e-12) {
        return 0.0;
    }
    let r2 = r * r;
    let ad = dot(a, d);
    let disc = ad * ad - dd * (dot(a, a) - r2);
    if (disc <= 0.0) {
        return 0.5 * r2 * angle(a, b);
    }
    let root = sqrt(disc);
    let t0 = clamp((-ad - root) / dd, 0.0, 1.0);
    let t1 = clamp((-ad + root) / dd, 0.0, 1.0);
    let p0 = a + d * t0;
    let p1 = a + d * t1;
    return 0.5 * r2 * (angle(a, p0) + angle(p1, b)) + 0.5 * (p0.x * p1.y - p0.y * p1.x);
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

    let s = normalize(frame.to_sun.xyz);
    let a = corners[3u * in.triangle].xyz;
    let b = corners[3u * in.triangle + 1u].xyz;
    let c = corners[3u * in.triangle + 2u].xyz;
    let e1 = b - a;
    let e2 = c - a;

    if (penumbra.x <= 0.0) {
        // The ground's ray to the sun against the triangle
        // (Möller–Trumbore): the hard shadow.
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
        return vec4f(1.0, 0.0, 0.0, 0.0);
    }

    // How far up the ray the triangle is: where the ray meets its plane,
    // kept between its corners' heights — a triangle the sun sees edge on
    // has no plane to meet, and its corners' mean stands in.
    let h = vec3f(dot(a - p, s), dot(b - p, s), dot(c - p, s));
    let highest = max(max(h.x, h.y), h.z);
    if (highest <= 0.0) {
        discard;
    }
    let n = cross(e1, e2);
    let facing = dot(n, s);
    var t = (h.x + h.y + h.z) / 3.0;
    if (abs(facing) > 1e-3 * length(n)) {
        t = dot(n, a - p) / facing;
    }
    t = clamp(t, max(min(min(h.x, h.y), h.z), 0.0), highest);
    let radius = t * penumbra.x;

    // The triangle across the ray, the ray at the origin.
    let uv = across(s, e1);
    let qa = vec2f(dot(a - p, uv[0]), dot(a - p, uv[1]));
    let qb = vec2f(dot(b - p, uv[0]), dot(b - p, uv[1]));
    let qc = vec2f(dot(c - p, uv[0]), dot(c - p, uv[1]));
    var covered = 0.0;
    if (radius < 1e-3) {
        // A caster at the ground: no penumbra a sample could show.
        let w0 = qa.x * qb.y - qa.y * qb.x;
        let w1 = qb.x * qc.y - qb.y * qc.x;
        let w2 = qc.x * qa.y - qc.y * qa.x;
        let inside = (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0)
            || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0);
        covered = select(0.0, 1.0, inside && w0 + w1 + w2 != 0.0);
    } else {
        let area = wedge(qa, qb, radius) + wedge(qb, qc, radius) + wedge(qc, qa, radius);
        covered = clamp(abs(area) / (PI * radius * radius), 0.0, 1.0);
    }
    if (covered <= 0.0) {
        discard;
    }
    return vec4f(covered, 0.0, 0.0, 0.0);
}
