// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The aerial table made in compute: one invocation a node, its veil and
// what it lets through stored. Included after `aerial_table.wgsl`, with the
// table declared as `aerial_out`, write-only `rgba16float` storage.

@compute @workgroup_size(8, 8)
fn aerial_table(@builtin(global_invocation_id) id: vec3u) {
    let rows = aerial_half_rows();
    if (id.x >= AERIAL_WIDTH * AERIAL_GRID || id.y >= rows) { return; }
    let air = aerial_node(id.xy);
    textureStore(aerial_out, id.xy, aerial_veil(air));
    textureStore(aerial_out, vec2u(id.x, id.y + rows), aerial_through(air));
}
