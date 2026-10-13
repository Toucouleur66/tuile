// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! PROTOTYPE (exploration `terrain-smoothing`): a tile's mesh made to read
//! as smooth ground, two ways, to be looked at side by side. Nothing here is
//! a decision; see `docs/explorations/terrain-smoothing.md`.
//!
//! - **Shading** gives a mesh a normal a vertex — the mean of the faces
//!   round it, weighed by their area — for the resolve to interpolate. No
//!   vertex moves: 0 m from the source's surface.
//! - **Geometry** cuts every triangle in four and lifts the new vertices off
//!   their chords onto the curved patch the vertex normals describe (a
//!   point-normal triangle's edge). The source's vertices stay where they
//!   are; a new one moves by no more than a bound, in metres; an edge on the
//!   tile's border stays straight, so that two tiles still meet on a line.
//!
//! Both are a step at tile load, on the CPU: a draw-time shader cannot add
//! triangles on this renderer's targets, which have no tessellation stage.

use std::collections::HashMap;

use glam::{DVec3, Vec3};
use tuile_core::content::DecodedMesh;
use tuile_core::stitch;

/// How far, in metres, geometry smoothing lifts a new vertex off its chord at
/// most, unless told otherwise: what a camera kept over the ground must
/// allow for, since the ground it sees is the smoothed one.
pub const DEFAULT_BOUND_M: f32 = 2.0;

/// How a film's ground is smoothed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Smooth {
    /// As the mesh comes.
    #[default]
    Off,
    /// A normal a vertex; the surface is the source's.
    Shading,
    /// Triangles cut in four on curved patches, a new vertex no further
    /// than this many metres from the source's flat triangle; and normals.
    Geometry { bound: f32 },
}

/// What smoothing did to a mesh.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Smoothed {
    /// The furthest a vertex is from the source's surface, in metres, and
    /// the mean over the vertices added.
    pub furthest: f32,
    pub mean: f32,
    pub triangles_before: usize,
    pub triangles_after: usize,
    /// Whether the mesh came with normals of its own.
    pub had_normals: bool,
}

/// A normal a vertex: the area-weighted mean of the surface's faces round
/// it. Vertices at one place share one — a mesh cut from another states
/// some places twice — and a skirt's foot takes its edge's.
pub fn vertex_normals(mesh: &DecodedMesh, origin: DVec3) -> Vec<[f32; 3]> {
    let surface = stitch::without_skirts(mesh, origin);
    let place = |p: [f32; 3]| [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()];
    let mut sums: HashMap<[u32; 3], Vec3> = HashMap::new();
    for t in surface.indices.as_chunks::<3>().0 {
        let p = |i: u32| Vec3::from(mesh.positions[i as usize]);
        let face = (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0]));
        for i in t {
            *sums.entry(place(mesh.positions[*i as usize])).or_default() += face;
        }
    }
    // Up, for a vertex no face of the surface touches.
    let up = |p: [f32; 3]| (origin + Vec3::from(p).as_dvec3()).normalize().as_vec3();
    let mut normals: Vec<Option<Vec3>> = mesh
        .positions
        .iter()
        .map(|p| sums.get(&place(*p)).and_then(|n| n.try_normalize()))
        .collect();
    // A skirt's foot: the normal of the surface's vertex at its place in
    // the tile.
    if let Some(uvs) = &mesh.uvs {
        let mut of_place: HashMap<[u32; 2], Vec3> = HashMap::new();
        for (uv, n) in uvs.iter().zip(&normals) {
            if let Some(n) = n {
                of_place
                    .entry([uv[0].to_bits(), uv[1].to_bits()])
                    .or_insert(*n);
            }
        }
        for (uv, n) in uvs.iter().zip(&mut normals) {
            if n.is_none() {
                *n = of_place.get(&[uv[0].to_bits(), uv[1].to_bits()]).copied();
            }
        }
    }
    normals
        .iter()
        .zip(&mesh.positions)
        .map(|(n, p)| n.unwrap_or_else(|| up(*p)).to_array())
        .collect()
}

/// Whether both ends of an edge are on one side of the tile: an edge of
/// its border, which stays straight.
fn on_the_border(a: [f32; 2], b: [f32; 2]) -> bool {
    const ON: f32 = 1.0e-6;
    (a[0] < ON && b[0] < ON)
        || (a[0] > 1.0 - ON && b[0] > 1.0 - ON)
        || (a[1] < ON && b[1] < ON)
        || (a[1] > 1.0 - ON && b[1] > 1.0 - ON)
}

/// Smooths `mesh`; the mesh a renderer is handed, and what was done.
pub fn smooth(mesh: &DecodedMesh, origin: DVec3, how: Smooth) -> (DecodedMesh, Smoothed) {
    let mut done = Smoothed {
        triangles_before: mesh.indices.len() / 3,
        triangles_after: mesh.indices.len() / 3,
        had_normals: mesh.normals.is_some(),
        ..Smoothed::default()
    };
    let bound = match how {
        Smooth::Off => return (mesh.clone(), done),
        Smooth::Shading => None,
        Smooth::Geometry { bound } => Some(bound),
    };
    let mut out = mesh.clone();
    let normals = vertex_normals(mesh, origin);
    let (Some(bound), Some(uvs)) = (bound, mesh.uvs.clone()) else {
        out.normals = Some(normals);
        return (out, done);
    };
    let mut normals = normals;
    let mut uvs = uvs;
    // Skirts are left as they are: only the surface is cut.
    let surface = stitch::without_skirts(mesh, origin);
    let is_surface: std::collections::HashSet<[u32; 3]> =
        surface.indices.as_chunks::<3>().0.iter().copied().collect();
    let mut middles: HashMap<(u32, u32), u32> = HashMap::new();
    let mut moved: Vec<f32> = Vec::new();
    let mut indices = Vec::with_capacity(mesh.indices.len() * 4);
    for t in mesh.indices.as_chunks::<3>().0 {
        if !is_surface.contains(t) {
            indices.extend_from_slice(t);
            continue;
        }
        let mut middle = |a: u32, b: u32| -> u32 {
            *middles.entry((a.min(b), a.max(b))).or_insert_with(|| {
                let (pa, pb) = (
                    Vec3::from(out.positions[a as usize]),
                    Vec3::from(out.positions[b as usize]),
                );
                let (na, nb) = (
                    Vec3::from(normals[a as usize]),
                    Vec3::from(normals[b as usize]),
                );
                let chord = (pa + pb) / 2.0;
                // The middle of the patch's edge: a cubic through the two
                // ends, leaving each in its tangent plane.
                let near_a = (2.0 * pa + pb - (pb - pa).dot(na) * na) / 3.0;
                let near_b = (2.0 * pb + pa - (pa - pb).dot(nb) * nb) / 3.0;
                let curve = (pa + 3.0 * near_a + 3.0 * near_b + pb) / 8.0;
                let lift = if on_the_border(uvs[a as usize], uvs[b as usize]) {
                    Vec3::ZERO
                } else {
                    (curve - chord).clamp_length_max(bound)
                };
                moved.push(lift.length());
                let index = out.positions.len() as u32;
                out.positions.push((chord + lift).to_array());
                normals.push((na + nb).normalize_or_zero().to_array());
                let (ua, ub) = (uvs[a as usize], uvs[b as usize]);
                uvs.push([(ua[0] + ub[0]) / 2.0, (ua[1] + ub[1]) / 2.0]);
                index
            })
        };
        let (ab, bc, ca) = (middle(t[0], t[1]), middle(t[1], t[2]), middle(t[2], t[0]));
        indices.extend_from_slice(&[t[0], ab, ca, ab, t[1], bc, ca, bc, t[2], ab, bc, ca]);
    }
    done.triangles_after = indices.len() / 3;
    done.furthest = moved.iter().copied().fold(0.0, f32::max);
    done.mean = moved.iter().sum::<f32>() / moved.len().max(1) as f32;
    out.indices = indices;
    out.uvs = Some(uvs);
    out.normals = Some(normals);
    (out, done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::content::MaterialDesc;

    /// A ridge: two squares of four triangles, folded along the line they
    /// share, with the texture coordinates of a tile.
    fn ridge() -> DecodedMesh {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 30.0],
            [200.0, 0.0, 0.0],
            [0.0, 100.0, 0.0],
            [100.0, 100.0, 30.0],
            [200.0, 100.0, 0.0],
            [0.0, 200.0, 0.0],
            [100.0, 200.0, 30.0],
            [200.0, 200.0, 0.0],
        ];
        let uvs = (0..9)
            .map(|i| [(i % 3) as f32 / 2.0, 1.0 - (i / 3) as f32 / 2.0])
            .collect();
        DecodedMesh {
            positions,
            normals: None,
            uvs: Some(uvs),
            indices: vec![
                0, 1, 3, 3, 1, 4, 1, 2, 4, 4, 2, 5, 3, 4, 6, 6, 4, 7, 4, 5, 7, 7, 5, 8,
            ],
            material: MaterialDesc::default(),
        }
    }

    /// Far from the Earth's centre along +z, so that up is +z.
    const ORIGIN: DVec3 = DVec3::new(0.0, 0.0, 6.4e6);

    #[test]
    fn off_is_the_mesh_as_it_came() {
        let mesh = ridge();
        let (same, done) = smooth(&mesh, ORIGIN, Smooth::Off);
        assert_eq!(same.positions, mesh.positions);
        assert_eq!(same.indices, mesh.indices);
        assert!(same.normals.is_none());
        assert_eq!(done.triangles_after, 8);
    }

    #[test]
    fn shading_moves_nothing_and_gives_every_vertex_a_normal() {
        let mesh = ridge();
        let (shaded, done) = smooth(&mesh, ORIGIN, Smooth::Shading);
        assert_eq!(shaded.positions, mesh.positions);
        assert_eq!(shaded.indices, mesh.indices);
        assert_eq!(done.furthest, 0.0);
        let normals = shaded.normals.expect("normals");
        // On the ridge the two slopes' mean: straight up.
        assert!(
            Vec3::from(normals[4]).dot(Vec3::Z) > 0.999,
            "{:?}",
            normals[4]
        );
        // On a slope, the slope's own.
        assert!(normals[3][0] < -0.2, "{:?}", normals[3]);
    }

    #[test]
    fn geometry_keeps_the_sources_vertices_and_its_bound() {
        let mesh = ridge();
        let (round, done) = smooth(&mesh, ORIGIN, Smooth::Geometry { bound: 1.5 });
        assert_eq!(round.positions[..9], mesh.positions[..]);
        assert_eq!(done.triangles_after, 32);
        assert!(
            done.furthest > 0.5 && done.furthest <= 1.5 + 1.0e-4,
            "{done:?}"
        );
        // Unbounded, the ridge's flanks bulge by more: the bound is what holds.
        let (_, free) = smooth(&mesh, ORIGIN, Smooth::Geometry { bound: 1.0e9 });
        assert!(free.furthest > 1.5, "{free:?}");
        // An edge of the tile's border is cut where its chord is.
        let uvs = round.uvs.expect("uvs");
        for (p, uv) in round.positions.iter().zip(&uvs).skip(9) {
            if uv[1] == 1.0 {
                assert_eq!(p[1], 0.0);
                let on_chord = if p[0] < 100.0 {
                    0.3 * p[0]
                } else {
                    0.3 * (200.0 - p[0])
                };
                assert!((p[2] - on_chord).abs() < 1.0e-3, "{p:?}");
            }
        }
    }
}
