// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The sky-view table of one frame (Hillaire 2020 §5.3): the sky seen from
// the frame's eye in every direction, by its azimuth from the sun and its
// zenith angle, the rows bunched about the horizon. Made once a frame, read
// by `present` for every sky pixel of both eyes. Its texels are
// `hillaire::sky_view_texel_direction`'s, the CPU twin it is tested against.

@compute @workgroup_size(8, 8)
fn sky_view(@builtin(global_invocation_id) id: vec3u) {
    let size = vec2u(SKY_VIEW_SIZE);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let height = atmosphere.sun.w;
    let unit = vec2f(id.xy) / (SKY_VIEW_SIZE - vec2f(1.0));
    let seen = sky_view_direction(unit, height);
    let mu = seen.x;
    let c = seen.y;
    let s = sqrt(max(1.0 - mu * mu, 0.0));
    let dir = vec3f(s * c, s * sqrt(max(1.0 - c * c, 0.0)), mu);
    let mu_s = dot(atmosphere.sun.xyz, atmosphere.up.xyz);
    let sun = vec3f(sqrt(max(1.0 - mu_s * mu_s, 0.0)), 0.0, mu_s);
    textureStore(sky_view_out, id.xy, vec4f(sky_radiance(height, dir, sun), 1.0));
}
