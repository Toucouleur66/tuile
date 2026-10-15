// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The sun's map, read: by the resolve for the ground and by overlays that
// receive shadows, the same way. A module that includes this declares
// `shadow_map: texture_depth_2d` and `shadow_samp: sampler_comparison`.

// How much of the sun reaches `p` (eye-relative, its normal `n`): 1 where
// nothing in the sun's map is nearer the sun, over 3×3 texels each read
// through the comparison's own bilinear filter.
//
// The point is moved off its surface by two texels before it is looked up,
// so that ground does not shade itself across the texel it falls in. And
// what the map does not hold is lit: beyond it nothing is known to stand
// before the sun, and unknown is never dark.
fn sunlit(p: vec3f, n: vec3f) -> f32 {
    let at = frame.sun_view_proj * vec4f(p + n * (2.0 * frame.shadow.y), 1.0);
    let uv = vec2f(at.x, -at.y) * 0.5 + 0.5;
    if (any(uv < vec2f(0.0)) || any(uv > vec2f(1.0)) || at.z < 0.0 || at.z > 1.0) {
        return 1.0;
    }
    let texel = 1.0 / frame.shadow.z;
    var lit = 0.0;
    for (var j = -1; j <= 1; j++) {
        for (var i = -1; i <= 1; i++) {
            let to = uv + vec2f(f32(i), f32(j)) * texel;
            lit += textureSampleCompareLevel(shadow_map, shadow_samp, to, at.z);
        }
    }
    // Towards where they stop, shadows fade: no line across the ground
    // where the map ends.
    let fade = smoothstep(0.8 * frame.shadow.w, frame.shadow.w, length(p));
    return lit / 9.0 * (1.0 - fade) + fade;
}
