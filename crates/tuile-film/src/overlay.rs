// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a host adds to a film's frames: shapes, placed in the world.
//!
//! An overlay is geometry — coloured triangles in Earth coordinates — drawn
//! in the same scene as the ground and through the same camera. It is not a
//! picture laid over the frame afterwards: a shape that passes behind a
//! ridge is hidden by the ridge, pixel by pixel.
//!
//! Nothing here knows what a shape stands for. The host decides what to
//! draw, where and in what colour, frame by frame; the renderer draws
//! triangles.

use tuile_pack::BakedView;

use crate::{Eye, EyePair, FrameCamera, FrameLight};

/// What a mesh is tested against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayDepth {
    /// Hidden wherever the ground, or an opaque overlay, is nearer: what
    /// lives in the landscape.
    #[default]
    Terrain,
    /// Never hidden: what must stay readable whatever stands before it.
    Always,
    /// Hidden by nearer opaque overlays and never by the ground: shapes
    /// placed before the scene, whose own layers are to stand in front of
    /// one another as their depths say.
    OverlaysOnly,
}

/// How a mesh is laid over what is behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayBlend {
    /// Drawn first, and it hides the overlays behind it: its depth is
    /// written, so any overlay tested against overlays is hidden where it
    /// is nearer.
    Opaque,
    /// Drawn after every opaque mesh, over what is before it in the order
    /// given, and hides nothing: its depth is read, never written.
    #[default]
    Translucent,
}

/// Which eyes of a stereo film draw a mesh. A film of one eye draws every
/// mesh, whatever it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayEyes {
    #[default]
    Both,
    Left,
    Right,
}

impl OverlayEyes {
    /// Whether the picture drawn for `eye` — `None` for a film of one eye —
    /// shows the mesh.
    pub fn shows(self, eye: Option<Eye>) -> bool {
        match (self, eye) {
            (_, None) | (OverlayEyes::Both, _) => true,
            (OverlayEyes::Left, Some(eye)) => eye == Eye::Left,
            (OverlayEyes::Right, Some(eye)) => eye == Eye::Right,
        }
    }
}

/// What a mesh has to do with the sun's shadows, when the film's look has
/// any ([`crate::Look::shadow`]); nothing otherwise.
///
/// A mesh that **casts** is drawn into the same map of the sun as the
/// ground, once a frame and for both eyes: the ground under it, and any
/// mesh that receives, lose the sun where it stands before them. A mesh
/// that **receives** loses the sun where the ground, or a mesh that casts,
/// stands between it and the sun — darkened as shaded ground is, to what
/// the dome alone gives.
///
/// A caster is seen by the sun on a grid of texels
/// ([`OverlayCamera::shadow_texel`] wide, in metres): a shape narrower than
/// [`MIN_SHADOW_TEXELS`] of them, as the sun sees it, casts an outline a
/// texel wide at most — its edges are drawn into the map as well, so that
/// it never vanishes — and only one that wide or wider casts its whole
/// shadow. A host that wants a thin ribbon's shadow whole widens what casts
/// it.
///
/// A mesh that **projects** stays out of the map: its shadow is worked out
/// exactly, for each pixel of ground, by following the ground's own ray to
/// the sun and asking whether it meets one of the mesh's triangles. Nothing
/// of a grid of texels is in it, so a thin shape — a line, a ribbon a few
/// metres wide — casts its whole shadow, with edges as sharp as the
/// picture's, and that shadow stands still while the sun moves, as far
/// along the ground as the shape's height over the tangent of the sun's
/// elevation. Its shadow falls on the ground only: no overlay receives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayShadow {
    #[default]
    None,
    Cast,
    Receive,
    CastAndReceive,
    /// Casts by projection, receives nothing.
    Project,
    /// Casts by projection and receives the map's shadows.
    ProjectAndReceive,
}

/// The least width, in texels of the sun's map, of a caster whose shadow
/// comes out whole: see [`OverlayShadow`].
pub const MIN_SHADOW_TEXELS: f32 = 2.0;

impl OverlayShadow {
    pub fn casts(self) -> bool {
        matches!(self, OverlayShadow::Cast | OverlayShadow::CastAndReceive)
    }

    pub fn receives(self) -> bool {
        matches!(
            self,
            OverlayShadow::Receive
                | OverlayShadow::CastAndReceive
                | OverlayShadow::ProjectAndReceive
        )
    }

    /// Whether its shadow is projected onto the ground rather than drawn
    /// into the sun's map.
    pub fn projects(self) -> bool {
        matches!(
            self,
            OverlayShadow::Project | OverlayShadow::ProjectAndReceive
        )
    }
}

/// One mesh of a frame's overlays.
///
/// Positions are f32 **relative to `origin_ecef`**, which stays in f64: the
/// renderer subtracts the frame's eye from it before anything is narrowed,
/// as it does for a tile. Keep the origin near the mesh — a mesh stored
/// against the Earth's centre has lost its centimetres already.
///
/// Opaque meshes are drawn first, then translucent ones, each in the order
/// given, and so are a mesh's triangles: a later translucent triangle is
/// laid over an earlier one. Geometry that lies *on* the ground, or on
/// another overlay, is the host's to lift clear of it.
///
/// Every setting's default is what a mesh was before it had them: flat,
/// translucent, tested against the ground, in both eyes, out of the sun's
/// map.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OverlayMesh {
    pub origin_ecef: [f64; 3],
    /// A position a vertex, in metres from the origin, along ECEF axes.
    pub positions: Vec<[f32; 3]>,
    /// A colour a vertex: display-linear RGB **premultiplied** by its alpha.
    /// Display-linear is the picture's own light, after the film's exposure
    /// and grade — one is the display's white — so a colour comes out as
    /// given whatever the look; only the output curve is still to come.
    pub colors: Vec<[f32; 4]>,
    /// A normal a vertex, along ECEF axes, or none at all. With normals a
    /// mesh is lit, by a light that goes with the camera — a share of its
    /// colour all round, more where it faces the light, a soft highlight —
    /// so that its relief shows; without, its colours are drawn as given.
    pub normals: Vec<[f32; 3]>,
    /// Triangles, three indices each, either winding.
    pub indices: Vec<u32>,
    pub depth: OverlayDepth,
    pub blend: OverlayBlend,
    pub eyes: OverlayEyes,
    pub shadow: OverlayShadow,
}

impl OverlayMesh {
    /// What is wrong with the mesh, if anything: a renderer refuses it
    /// rather than read past its vertices.
    pub fn fault(&self) -> Option<&'static str> {
        if self.colors.len() != self.positions.len() {
            return Some("an overlay mesh has not one colour a vertex");
        }
        if !self.normals.is_empty() && self.normals.len() != self.positions.len() {
            return Some("an overlay mesh has normals, and not one a vertex");
        }
        if !self.indices.len().is_multiple_of(3) {
            return Some("an overlay mesh's indices are not triangles");
        }
        let vertices = self.positions.len() as u64;
        if self.indices.iter().any(|i| u64::from(*i) >= vertices) {
            return Some("an overlay mesh's index is past its vertices");
        }
        None
    }
}

/// The camera a frame's overlays are made for: the whole of it, once a
/// frame.
///
/// A stereo film draws the frame twice, from two eyes. Overlays are made
/// once, for the frame's own camera between them — the cyclopean eye — and
/// drawn into each eye through that eye's projection: a shape is then one
/// shape, at one depth, seen from two places, which is what relief is.
/// What depends on the camera (a width kept constant on the screen, a face
/// turned to the eye) is computed from `camera`, never from an eye: two
/// eyes computing it each would make two shapes, which the eyes cannot
/// fuse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayCamera {
    /// The view the frame was baked for.
    pub view: BakedView,
    /// The frame's camera at the picture's aspect: its eye, its rotation,
    /// its projection and its near plane ([`FrameCamera::near`]).
    pub camera: FrameCamera,
    /// The film's two eyes for this frame, when it has two.
    pub eyes: Option<EyePair>,
    /// A texel of the sun's map this frame, in metres across, when the
    /// film has shadows and there is ground for them: see
    /// [`OverlayShadow`].
    pub shadow_texel: Option<f32>,
}

impl OverlayCamera {
    /// The camera of a film of one eye, with no shadows.
    pub fn of(view: BakedView, aspect: f32) -> Self {
        Self {
            view,
            camera: FrameCamera::of(&view, aspect),
            eyes: None,
            shadow_texel: None,
        }
    }

    /// The near plane, in metres.
    pub fn near(&self) -> f32 {
        self.camera.near
    }

    /// The camera `eye` draws through: the frame's own for a film of one
    /// eye.
    pub fn eye(&self, eye: Eye) -> FrameCamera {
        match &self.eyes {
            Some(pair) => self.camera.eye(pair, eye),
            None => self.camera,
        }
    }
}

/// The overlays of a film, asked for frame by frame — and, when the host
/// places it, the frame's light.
pub trait Overlays {
    /// Adds to `out` what `frame` shows beside the ground, once a frame —
    /// for both eyes of a stereo film. `camera` is the frame's; `out`
    /// comes in empty.
    fn frame(&mut self, frame: u32, camera: &OverlayCamera, out: &mut Vec<OverlayMesh>);

    /// The light `frame` is lit by, asked once a frame before anything of
    /// it is drawn — before its overlays, and before the sun's map is
    /// fitted, so that the shadows fall from this sun — and shared by both
    /// eyes of a stereo film. `None`, the default: the film's look, as
    /// ordered.
    fn light(&mut self, frame: u32) -> Option<FrameLight> {
        let _ = frame;
        None
    }
}

/// No overlays.
impl Overlays for () {
    fn frame(&mut self, _: u32, _: &OverlayCamera, _: &mut Vec<OverlayMesh>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle() -> OverlayMesh {
        OverlayMesh {
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            colors: vec![[1.0; 4]; 3],
            indices: vec![0, 1, 2],
            ..OverlayMesh::default()
        }
    }

    #[test]
    fn a_host_that_says_nothing_of_the_light_leaves_it_to_the_look() {
        assert_eq!(().light(7), None);
    }

    #[test]
    fn a_whole_mesh_has_no_fault_and_is_drawn_as_before_it_had_settings() {
        let mesh = triangle();
        assert_eq!(mesh.fault(), None);
        assert_eq!(mesh.depth, OverlayDepth::Terrain);
        assert_eq!(mesh.blend, OverlayBlend::Translucent);
        assert_eq!(mesh.eyes, OverlayEyes::Both);
        assert_eq!(mesh.shadow, OverlayShadow::None);
        assert!(mesh.normals.is_empty());
    }

    #[test]
    fn a_mesh_for_one_eye_is_in_that_eye_and_in_a_film_of_one() {
        assert!(OverlayEyes::Left.shows(Some(Eye::Left)));
        assert!(!OverlayEyes::Left.shows(Some(Eye::Right)));
        assert!(!OverlayEyes::Right.shows(Some(Eye::Left)));
        assert!(OverlayEyes::Right.shows(Some(Eye::Right)));
        for eyes in [OverlayEyes::Both, OverlayEyes::Left, OverlayEyes::Right] {
            assert!(eyes.shows(None));
        }
        assert!(OverlayEyes::Both.shows(Some(Eye::Right)));
    }

    #[test]
    fn what_casts_and_what_receives() {
        let cast: Vec<bool> = [
            OverlayShadow::None,
            OverlayShadow::Cast,
            OverlayShadow::Receive,
            OverlayShadow::CastAndReceive,
        ]
        .iter()
        .flat_map(|s| [s.casts(), s.receives()])
        .collect();
        assert_eq!(cast, [false, false, true, false, false, true, true, true]);
    }

    #[test]
    fn a_mesh_that_would_be_read_past_its_end_is_named() {
        let mut short = triangle();
        short.colors.pop();
        assert!(short.fault().is_some());
        let mut open = triangle();
        open.indices.pop();
        assert!(open.fault().is_some());
        let mut past = triangle();
        past.indices[2] = 3;
        assert!(past.fault().is_some());
        let mut normals = triangle();
        normals.normals = vec![[1.0, 0.0, 0.0]; 2];
        assert!(normals.fault().is_some());
        normals.normals.push([0.0, 1.0, 0.0]);
        assert_eq!(normals.fault(), None);
    }
}
