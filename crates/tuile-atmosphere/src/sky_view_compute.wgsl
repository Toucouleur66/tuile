// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The sky-view tables made in compute: one invocation a texel, stored.
// Included after `sky_view.wgsl`, with the two tables declared as
// `sky_view_out` and `moon_view_out`, write-only `rgba32float` storage.

@compute @workgroup_size(8, 8)
fn sky_view(@builtin(global_invocation_id) id: vec3u) {
    let size = vec2u(SKY_VIEW_SIZE);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let texel = sky_view_texel(id.xy);
    textureStore(sky_view_out, id.xy, vec4f(texel.sun, 1.0));
    textureStore(moon_view_out, id.xy, vec4f(texel.moon, 1.0));
}
