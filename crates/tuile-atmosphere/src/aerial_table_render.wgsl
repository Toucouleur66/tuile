// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The aerial table drawn: one triangle over the table, a fragment a texel —
// the veil's half and the half of what the air lets through each marching
// their node. The same sums as the compute pass's (`aerial_node`), for
// devices that have no compute: WebGL2. Included after `aerial_table.wgsl`.

@vertex
fn aerial_vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4f {
    let xy = vec2f(f32((index << 1u) & 2u), f32(index & 2u)) * 2.0 - 1.0;
    return vec4f(xy, 0.0, 1.0);
}

@fragment
fn aerial_fragment(@builtin(position) at: vec4f) -> @location(0) vec4f {
    // A fragment's position is its texel's centre: the texel is its floor.
    let texel = vec2u(at.xy);
    let rows = aerial_half_rows();
    if (texel.y < rows) { return aerial_veil(aerial_node(texel)); }
    return aerial_through(aerial_node(vec2u(texel.x, texel.y - rows)));
}
