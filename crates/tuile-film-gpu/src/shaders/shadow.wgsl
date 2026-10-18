// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The frame's tiles as the sun sees them, depth alone: what stands between
// the ground and the sun. The same vertices as the visibility raster,
// through the sun's view in place of the camera's.

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> tiles: array<TileFrame>;

@group(1) @binding(0) var<storage, read> positions: array<f32>;
@group(1) @binding(3) var<storage, read> indices: array<u32>;

// Which of the sun's maps this pass draws.
struct SunMapPass { map: u32 }
@group(2) @binding(0) var<uniform> sun_map_pass: SunMapPass;

@vertex
fn vs(@builtin(vertex_index) v: u32, @builtin(instance_index) slot: u32) -> @builtin(position) vec4f {
    let i = indices[v];
    let p = vec3f(positions[3u * i], positions[3u * i + 1u], positions[3u * i + 2u]);
    var clip = frame.sun_view_proj * vec4f(p + tiles[slot].offset, 1.0);
    // On the map's own grid: see `sun_map_shift`.
    clip = vec4f(clip.xy + sun_map_shift(sun_map_pass.map, frame.size.z, frame.shadow.z) * clip.w, clip.zw);
    // The map's planes hold the ground that receives. Ground nearer the
    // sun than the near one casts all the same: it is laid flat on that
    // plane, where clipping would have cut it away.
    clip.z = max(clip.z, 0.0);
    return clip;
}
