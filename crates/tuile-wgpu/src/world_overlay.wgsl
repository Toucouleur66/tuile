// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// A host's shapes in the viewer: coloured triangles through the frame's own
// camera, into the frame it is already drawing.
//
// Their vertices come relative to a packing origin near the eye — each
// mesh's origin met it in f64 before anything was narrowed — and their colours
// display-linear and premultiplied, which is how they are blended here.
//
// The flags a vertex carries and `overlay_lit` come from
// `tuile_overlay::gpu::LIGHT_WGSL`, concatenated ahead of this. The film
// draws these meshes with the same text, so a shape comes out of both alike.
//
// What the film does and this does not: the sun's maps. A shape that casts
// or receives a shadow is drawn here with neither — the viewer has no map of
// the sun — so `OVERLAY_RECEIVES` is read and left alone. Depth is the
// frame's own, in the viewer's convention: nearer is *less*, and what a mesh
// is tested against is the pipeline's, not the shader's.

struct WorldOverlayView {
    // World to clip, relative to the origin the meshes were packed against.
    view_proj: mat4x4f,
    // The camera's own axes, for the light that goes with it. w unused.
    right: vec4f,
    up: vec4f,
    // The eye, relative to that same origin: zero when the meshes were
    // packed where the eye now stands. w unused.
    eye: vec4f,
}
@group(0) @binding(0) var<uniform> view: WorldOverlayView;

struct Out {
    @builtin(position) clip: vec4f,
    @location(0) color: vec4f,
    @location(1) normal: vec3f,
    @location(2) at: vec3f,
    @location(3) @interpolate(flat) flags: u32,
}

@vertex
fn vs(
    @location(0) position: vec3f,
    @location(1) color: vec4f,
    @location(2) normal: vec3f,
    @location(3) flags: u32,
) -> Out {
    var out: Out;
    out.clip = view.view_proj * vec4f(position, 1.0);
    out.color = color;
    out.normal = normal;
    // Eye-relative for the light, whatever origin the vertex was packed at.
    out.at = position - view.eye.xyz;
    out.flags = flags;
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4f {
    if ((in.flags & OVERLAY_LIT) != 0u) {
        return overlay_lit(in.color, in.normal, in.at, view.right.xyz, view.up.xyz);
    }
    return in.color;
}
