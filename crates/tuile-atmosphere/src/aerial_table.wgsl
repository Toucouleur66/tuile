// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The air between the eye and the ground, worked out once a frame at a
// table's nodes rather than once a fragment: an aerial perspective table
// after Hillaire 2020 (§5.5), its nodes the very march a fragment made
// (`air_between`), so that the table agrees with it exactly at every node
// and a fragment reads it between them.
//
// Its nodes are not laid along lines of sight but over the ground: points
// at a height over the air's sphere (AERIAL_SLICES layers, from a sea's
// depth to over the highest summits, closest about the sea), at an azimuth
// about the eye's up (AERIAL_WIDTH columns, all the way round), and at a
// distance from the eye (AERIAL_HEIGHT rows, from straight under or over
// the eye to the farthest a point at that height can be seen: the eye's
// horizon and the point's own, bunched towards it as the sky-view's rows
// are towards its horizon). The air up to a point is a smooth function of
// where the point is, and the ground a fragment draws lies between these
// layers: a fragment reads its air between the eight nodes about it,
// wherever it is, under the eye or on the limb, in a valley or on a
// summit seen over the sea's horizon. Nothing of it is the camera's: it
// holds every direction, and turning the camera reads another part of it.
//
// One 2D texture: its layers side by side, AERIAL_GRID a row of tiles of
// AERIAL_WIDTH by AERIAL_HEIGHT texels; the veil — the light the air
// scatters towards the eye, in the renderer's units — in the top half,
// the optical depth of what it lets through in the bottom half. The depth,
// not the transmittance: along a line of sight that grazes the planet the
// transmittance falls exponentially with the point's height and distance,
// and read linearly between nodes it came out far too clear at the limb
// (0.040 read for 0.023 marched, 21 code values, from 500 km); the depth
// is the sum the march makes, close to linear between nodes, and the
// transmittance is its exponential once read. Filtered by the sampler within
// a layer: one texture unit against the ground's imagery, and half floats
// — the veil is pre-exposed, as the picture is, so a night's air keeps its
// digits.
//
// Prepended by `gpu::aerial_wgsl`: AERIAL_WIDTH, AERIAL_HEIGHT,
// AERIAL_SLICES and AERIAL_GRID. Included after `hillaire.wgsl`; `air_seen`
// reads `aerial_lut` and `lut_sampler`, which only a module that reads the
// table has to bind.

// The layers' heights reach this far under and over the sphere, metres:
// the highest summits, and the ellipsoid's own departure from the sphere
// osculating it under the eye, a few kilometres at the limb.
const AERIAL_REACH: f32 = 12000.0;

// The eye's frame: its up, from the sphere's centre, and an east and a
// north about it — the same for the table's maker and its reader, both
// from the frame's uniform.
struct AerialFrame {
    up: vec3f,
    east: vec3f,
    north: vec3f,
}

fn aerial_frame() -> AerialFrame {
    var f: AerialFrame;
    f.up = -normalize(atmosphere.centre.xyz);
    var axis = vec3f(0.0, 0.0, 1.0);
    if (abs(f.up.z) > 0.9) { axis = vec3f(1.0, 0.0, 0.0); }
    f.east = normalize(cross(axis, f.up));
    f.north = cross(f.up, f.east);
    return f;
}

// The height of the layer coordinate `v` (0 the deepest layer, 1 the
// highest), and its inverse: the layers closest about the sphere, where
// the air is densest and changes fastest with height.
fn aerial_layer_height(v: f32) -> f32 {
    let w = 2.0 * v - 1.0;
    return AERIAL_REACH * w * abs(w);
}

fn aerial_layer_of(height: f32) -> f32 {
    let w = clamp(height / AERIAL_REACH, -1.0, 1.0);
    return 0.5 + 0.5 * sign(w) * sqrt(abs(w));
}

// Where a layer's rows reach, for a layer `height` metres over the sphere,
// seen from the eye: its points from straight under the eye (or over it,
// for a layer over the eye) out to where its lines of sight graze it — the
// first of its rows, down to AERIAL_SPLIT, spread evenly in the angle from
// the eye's nadir (or zenith), and bunched towards that grazing line — then
// past it out to the farthest of its points that can be seen over the
// sphere — the rest, by distance, bunched towards that farthest point.
// The eye's height from the CPU, in f64 there: from the centre's distance
// f32 would lose metres to the radius.
struct AerialLayer {
    // (ρ² − (R + H)²): positive for a layer under the eye.
    c: f32,
    rho: f32,
    // The zenith angles the first rows run between, and the distance of
    // the last of them; the farthest point's distance.
    theta0: f32,
    theta1: f32,
    t1: f32,
    t_far: f32,
}

// The rows' share of a layer spread by angle; the rest by distance.
const AERIAL_SPLIT: f32 = 0.6666667;

fn aerial_layer(height: f32) -> AerialLayer {
    let h = max(atmosphere.sun.w, 0.0);
    let radius = atmosphere.centre.w;
    var l: AerialLayer;
    l.rho = radius + h;
    l.c = (h - height) * (2.0 * radius + h + height);
    let low = max(height, 0.0);
    l.t_far = sqrt(h * (2.0 * radius + h)) + sqrt(low * (2.0 * radius + low));
    if (l.c > 0.0) {
        // Under the eye: from its nadir to where a line of sight grazes it.
        l.theta0 = ATMOSPHERE_PI;
        l.t1 = sqrt(l.c);
        l.theta1 = acos(clamp(-l.t1 / l.rho, -1.0, 1.0));
        l.t_far = max(l.t_far, l.t1 * 1.0001 + 1.0);
    } else {
        // Over it: from its zenith to its farthest point.
        l.theta0 = 0.0;
        l.t_far = max(l.t_far, -l.c / (2.0 * l.rho) + 1.0);
        l.t1 = l.t_far;
        l.theta1 = acos(aerial_cosine(l, l.t1));
    }
    return l;
}

// The cosine of the zenith angle of the layer's point `t` from the eye:
// (R + H)² = ρ² + t² + 2 t ρ μ.
fn aerial_cosine(l: AerialLayer, t: f32) -> f32 {
    return clamp((-l.c - t * t) / (2.0 * max(t, 1e-3) * l.rho), -1.0, 1.0);
}

// The layer's point at row coordinate `u` (0 to 1): its distance from the
// eye and the cosine of its zenith angle.
fn aerial_row_point(l: AerialLayer, u: f32) -> vec2f {
    if (u <= AERIAL_SPLIT) {
        let theta = l.theta0 + (l.theta1 - l.theta0) * aerial_share(u / AERIAL_SPLIT);
        let mu = cos(theta);
        let disc = max(l.rho * l.rho * mu * mu - l.c, 0.0);
        var t: f32;
        if (l.c > 0.0) {
            // The near root as the product of the roots over the far one.
            t = l.c / max(-l.rho * mu + sqrt(disc), 1e-3);
        } else {
            t = -l.rho * mu + sqrt(disc);
        }
        return vec2f(t, mu);
    }
    let b = (u - AERIAL_SPLIT) / (1.0 - AERIAL_SPLIT);
    let t = l.t1 + (l.t_far - l.t1) * aerial_share(b);
    return vec2f(t, aerial_cosine(l, t));
}

// The row coordinate of the layer's point `t` from the eye.
fn aerial_row_at(l: AerialLayer, t: f32) -> f32 {
    if (t <= l.t1) {
        let theta = acos(aerial_cosine(l, t));
        let a = aerial_row_of((theta - l.theta0) / (l.theta1 - l.theta0));
        return a * AERIAL_SPLIT;
    }
    let b = aerial_row_of((t - l.t1) / (l.t_far - l.t1));
    return AERIAL_SPLIT + b * (1.0 - AERIAL_SPLIT);
}

// The share at the coordinate `u` (0 to 1) of a span bunched towards its
// far end, where a line of sight grazes and its air changes fastest — and
// `aerial_row_of` its inverse.
fn aerial_share(u: f32) -> f32 {
    let v = 1.0 - u;
    return 1.0 - v * v;
}

fn aerial_row_of(share: f32) -> f32 {
    return 1.0 - sqrt(1.0 - clamp(share, 0.0, 1.0));
}

// How many rows of texels the veil's half of the table has.
fn aerial_half_rows() -> u32 {
    return AERIAL_HEIGHT * (AERIAL_SLICES / AERIAL_GRID);
}

// The point of the table's node at column `i`, row `j` of layer `k`, from
// the eye: at its azimuth, its distance and its height over the sphere.
fn aerial_point(i: u32, j: u32, k: u32, f: AerialFrame) -> vec3f {
    let l = aerial_layer(aerial_layer_height(f32(k) / f32(AERIAL_SLICES - 1u)));
    let p = aerial_row_point(l, f32(j) / f32(AERIAL_HEIGHT - 1u));
    let c = sqrt(max(1.0 - p.y * p.y, 0.0));
    let phi = ATMOSPHERE_PI * (2.0 * f32(i) / f32(AERIAL_WIDTH - 1u) - 1.0);
    return (f.east * (c * cos(phi)) + f.north * (c * sin(phi)) + f.up * p.y) * p.x;
}

// The air of the table's node at texel `texel` of its veil's half: the
// march a fragment there would have made.
fn aerial_node(texel: vec2u) -> AirRay {
    let tile = texel / vec2u(AERIAL_WIDTH, AERIAL_HEIGHT);
    let k = tile.y * AERIAL_GRID + tile.x;
    return air_between(aerial_point(texel.x % AERIAL_WIDTH, texel.y % AERIAL_HEIGHT, k, aerial_frame()));
}

// The node's two texels: its veil, pre-exposed, and what it lets through.
fn aerial_veil(air: AirRay) -> vec4f {
    return vec4f(air.inscattered * atmosphere.illuminance.xyz, 1.0);
}

// What it lets through as an optical depth (`−ln T`), bounded so that an
// opaque line of sight stays a finite half float.
fn aerial_through(air: AirRay) -> vec4f {
    return vec4f(min(-log(max(air.transmittance, vec3f(1e-30))), vec3f(69.0)), 1.0);
}

// The air between the eye and `ground` (from the eye) as the table holds
// it: what it scatters towards the eye, in the renderer's units, and what
// it lets through.
struct AirSeen {
    veil: vec3f,
    through: vec3f,
}

// The air as the table holds it: the veil and the optical depth, both
// read and blended linearly; the depth becomes a transmittance only once
// blended (`air_seen`).
struct AirDepth {
    veil: vec3f,
    depth: vec3f,
}

// The table's layer `k` read at azimuth coordinate `x` (0 to 1) and
// distance `t`: bilinearly, by the sampler, between its four nodes about
// the point.
fn aerial_read(k: f32, x: f32, t: f32) -> AirDepth {
    let y = aerial_row_at(aerial_layer(aerial_layer_height(k / f32(AERIAL_SLICES - 1u))), t);
    let w = f32(AERIAL_WIDTH);
    let h = f32(AERIAL_HEIGHT);
    let grid = f32(AERIAL_GRID);
    // Texel centres at the nodes: a tile's edges are never reached, so the
    // sampler never blends one layer into the next.
    let at = vec2f((k % grid) * w + x * (w - 1.0) + 0.5, floor(k / grid) * h + y * (h - 1.0) + 0.5);
    let size = vec2f(w * grid, 2.0 * f32(aerial_half_rows()));
    var out: AirDepth;
    out.veil = textureSampleLevel(aerial_lut, lut_sampler, at / size, 0.0).rgb;
    out.depth = textureSampleLevel(aerial_lut, lut_sampler, at / size + vec2f(0.0, 0.5), 0.0).rgb;
    return out;
}

// Where the line of sight from the eye at zenith cosine `mu` crosses the
// layer `l` on its way down — or on its way up, for one that climbs from
// the eye — or a negative number where it never does.
fn aerial_crossing(l: AerialLayer, mu: f32) -> f32 {
    let disc = l.rho * l.rho * mu * mu - l.c;
    if (disc < 0.0) { return -1.0; }
    let root = sqrt(disc);
    if (l.c > 0.0) {
        if (mu >= 0.0) { return -1.0; }
        // The near root as the product of the roots over the far one.
        return l.c / (-l.rho * mu + root);
    }
    return -l.rho * mu + root;
}

// Read between the table's nodes, in the two layers about the point's
// height. Far from the eye, each layer at the point's own azimuth and
// distance: its points there are the ground about the point — where its
// line of sight grazes the planet, the layers' points along it would be
// tens of kilometres apart. Near the eye, along the point's own line of
// sight, between where it crosses the two layers — or the eye itself, for
// a point as high as the eye between them: there the layers' points at
// its distance would be lines of sight tens of degrees apart, one of them
// perhaps into the sun's glare, and along its own the air grows with the
// distance alone. Between the two, a blend over a few layers' spacings.
fn air_seen(ground: vec3f) -> AirSeen {
    let air = air_depth_seen(ground);
    var out: AirSeen;
    out.veil = air.veil;
    out.through = exp(-air.depth);
    return out;
}

// `air_seen` before the depth's exponential: the veil and the optical
// depth, blended between the table's nodes.
fn air_depth_seen(ground: vec3f) -> AirDepth {
    var out: AirDepth;
    out.veil = vec3f(0.0);
    out.depth = vec3f(0.0);
    let t = length(ground);
    if (t <= 0.0) { return out; }
    let d = ground / t;
    let f = aerial_frame();
    let mu = dot(d, f.up);
    // The point's height over the sphere, from the eye's, without the
    // difference of two radii: ((R + H)² − ρ²) over their sum.
    let eye = max(atmosphere.sun.w, 0.0);
    let radius = atmosphere.centre.w;
    let rho = radius + eye;
    let far = sqrt(max(rho * rho + t * t + 2.0 * t * rho * mu, 0.0));
    let height = (t * (t + 2.0 * rho * mu) + eye * (2.0 * radius + eye)) / (far + radius);
    // The first column and the last are one azimuth, all the way round.
    let x = atan2(dot(d, f.north), dot(d, f.east)) / ATMOSPHERE_PI * 0.5 + 0.5;
    let n = f32(AERIAL_SLICES);
    let s = aerial_layer_of(height) * (n - 1.0);
    let k0 = min(floor(s), n - 2.0);
    let k1 = k0 + 1.0;
    let fk = s - k0;
    let low = aerial_read(k0, x, t);
    let high = aerial_read(k1, x, t);
    out.veil = mix(low.veil, high.veil, fk);
    out.depth = mix(low.depth, high.depth, fk);
    // Along the line of sight, near the eye.
    let h0 = aerial_layer_height(k0 / (n - 1.0));
    let h1 = aerial_layer_height(k1 / (n - 1.0));
    let spacing = h1 - h0;
    let near = 1.0 - smoothstep(4.0 * spacing, 16.0 * spacing, t);
    // A point on its way down from the eye, or up from it: not one past
    // the lowest point of its line of sight, which is far.
    let down = mu < 0.0 && height <= eye && t <= -rho * mu;
    let up = mu >= 0.0 && height >= eye;
    if (near <= 0.0 || !(down || up)) { return out; }
    // The layer on the eye's side of the point, and the one past it.
    var k_eye = k1;
    var k_past = k0;
    if (up) {
        k_eye = k0;
        k_past = k1;
    }
    let past = aerial_crossing(aerial_layer(aerial_layer_height(k_past / (n - 1.0))), mu);
    if (past < t) { return out; }
    var t_eye = 0.0;
    var at_eye: AirDepth;
    at_eye.veil = vec3f(0.0);
    at_eye.depth = vec3f(0.0);
    let h_eye = aerial_layer_height(k_eye / (n - 1.0));
    if ((h_eye - eye) * (height - eye) > 0.0) {
        t_eye = max(aerial_crossing(aerial_layer(h_eye), mu), 0.0);
        at_eye = aerial_read(k_eye, x, t_eye);
    }
    let at_past = aerial_read(k_past, x, past);
    let along = clamp((t - t_eye) / max(past - t_eye, 1e-3), 0.0, 1.0);
    out.veil = mix(out.veil, mix(at_eye.veil, at_past.veil, along), near);
    out.depth = mix(out.depth, mix(at_eye.depth, at_past.depth, along), near);
    return out;
}
