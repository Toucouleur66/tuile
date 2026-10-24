// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Decoded content → GPU resources. This is the crate's implementation of
//! the core's `PrepareRenderResources` contract: the core decodes, this
//! materializes.
//!
//! A tile's mesh and its drapes are resources of their own (D-A2): a mesh is
//! uploaded undressed ([`prepare`]), a drape on its own ([`prepare_drape`]),
//! and a mesh is dressed with the drapes the budget's draw set says it wears
//! — its own, or an ancestor's through a uv transform — when it is drawn
//! ([`PreparedTile::dress`]). Dressing binds the drapes' textures; it uploads
//! nothing.

use crate::context::{GpuContext, GpuImagery, IMAGERY_BINDING_0, TEXTURE_FORMAT};
use glam::{DVec3, Mat4, Vec3};
use std::sync::Arc;
use tuile_core::budget::{LayerId, ResourceId, UvTransform};
use tuile_core::content::{DecodedMesh, DecodedTexture, DecodedTileContent, DrapeContent};
use tuile_core::raster::{self, ImageryCoord, Placement};
use wgpu::util::DeviceExt;

/// Bytes per texel in [`TEXTURE_FORMAT`].
const BYTES_PER_TEXEL: u32 = 4;

/// Rows of strips a tile's uniform holds for one state of its stitching:
/// `Stitch::packed`'s ten, and up to 246 knots — more than twice what the
/// fullest tile of the films measured carried (105). A tile whose plan asks
/// for more is drawn as its mesh has it, its skirts under it; see
/// `PreparedTile::restitch`.
pub const STITCH_ROWS: usize = 256;

/// Bytes of a tile's uniform: its model matrix, a row for the stitching's
/// progress, and the strips twice — now, and before.
const TILE_UNIFORM_BYTES: usize = 64 + 16 + 2 * STITCH_ROWS * 16;
/// Where, in it, the progress row and the two sets of strips are.
const STITCH_AT: u64 = 64;
const STRIPS_AT: u64 = 80;

/// Interleaved vertex layout: position, normal, uv.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

impl Vertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
    };
}

pub struct PreparedMesh {
    pub vertex_buf: wgpu::Buffer,
    pub index_buf: wgpu::Buffer,
    pub index_count: u32,
    /// One bind group per imagery pass, in the order they must be drawn.
    ///
    /// Never empty: a mesh with no imagery still has one pass, carrying its base
    /// colour and a table of slots that all mask themselves out. The first is
    /// drawn opaque and writes depth; the rest are the same geometry again, with
    /// the next batch of layers, composed by alpha blending — see
    /// [`crate::TileRenderer::render`] and `raster::MAX_IMAGERY_PASSES`.
    pub material_bgs: Vec<wgpu::BindGroup>,
    _material_bufs: Vec<wgpu::Buffer>,
}

/// One batch of imagery layers: as many as a single draw can bind, and the
/// packed table that places them.
struct ImageryPass {
    textures: Vec<Arc<GpuImagery>>,
    table: wgpu::Buffer,
}

/// What one draw of one mesh is told about its material. Mirrors
/// `MaterialUniform` in `shader.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaterialUniform {
    base_color: [f32; 4],
    /// x = 1 on the first pass, 0 after — the rest is padding the uniform
    /// alignment would cost anyway. Named `flags` rather than `pass` because
    /// WGSL reserves that word, and the two declarations must match.
    flags: [f32; 4],
}

/// GPU-resident tile, ready to draw.
pub struct PreparedTile {
    pub meshes: Vec<PreparedMesh>,
    pub tile_bg: wgpu::BindGroup,
    tile_buf: wgpu::Buffer,
    /// The tile's own rebasing origin (ECEF) and intrinsic transform, kept so
    /// the model matrix can be recomputed against a moving render origin.
    origin_ecef: DVec3,
    transform_local: Mat4,
    /// The tile's sides as its mesh states them, and where it is in the
    /// geographic quadtree by its own corners — `None` for content that is
    /// not a whole tile of it, which is then never stitched.
    pub(crate) edges: tuile_core::stitch::Edges,
    pub(crate) cell: Option<(u32, u64, u64)>,
    /// The meshes as they came, normals included: what a mesh that gains
    /// vertices on an edge is made again from.
    source: Vec<DecodedMesh>,
    /// How the tile is stitched: see [`Self::restitch`].
    stitching: Stitching,
    /// The render origin this tile's uniform currently holds.
    ///
    /// A `Cell` so a tile can bring itself up to date through a shared
    /// reference, at the moment it is about to be drawn, rather than being
    /// swept along with every other resident tile whether or not anyone will
    /// look at it. See [`Self::rebase`].
    rebased_to: std::cell::Cell<DVec3>,
    _textures: Vec<GpuImagery>,
    /// The imagery the content carries itself (a loader that drapes its own
    /// meshes, or a mesh a consumer dressed on the CPU): always worn, under
    /// whatever drapes the tile is dressed with.
    own: Vec<DrapedLayer>,
    /// What this tile wears — its own imagery, then its drapes — batched into
    /// the passes that bind them, held so the shared textures and their
    /// tables outlive the dressing.
    _imagery: Vec<ImageryPass>,
    /// What the tile is dressed with: each drape and the uv transform it is
    /// worn through. Empty while undressed.
    dressing: Vec<(ResourceId, [u64; 4])>,
    /// Approximate GPU memory of this tile, bytes — **excluding draped
    /// imagery**, which is its drapes' own (see [`PreparedDrape`]).
    pub gpu_bytes: usize,
    /// The deepest imagery level of what the tile wears, if anything — the
    /// measure of how sharp its ground can look. A tile whose sharpest layer sits
    /// several levels above its own is a smear: one texel stretched over the
    /// whole surface. A recorder gates on the gap; a viewer tolerates it for
    /// the frames the drape needs to catch up.
    pub sharpest_imagery_level: Option<u32>,
    /// Which imagery set this tile is dressed with, when it is dressed at
    /// all: the newest [`source`](tuile_core::raster::ImageryLayer::source)
    /// among its layers. What a host reads to know how far a switch of
    /// imagery has crossed the screen.
    pub imagery_source: Option<u32>,
}

/// A tile's stitching: what it was last told, and its slide towards it.
#[derive(Default)]
struct Stitching {
    told: tuile_core::stitch::Stitch,
    /// The frame the slide began at, while it lasts.
    sliding_since: Option<u64>,
}

impl PreparedTile {
    /// Stitches the tile to its neighbours as `tuile_core::stitch::plan`
    /// decided for the tiles drawn from `frame` on: the vertex stage moves
    /// its vertices by the plan's strips. `slide` is how many frames the
    /// tile takes to get there from where its last strips had it; 0, or a
    /// tile that was not drawn the frame before (`fresh`), is there at once.
    ///
    /// A plan that gives the tile vertices has its buffers made again from
    /// the mesh as it came: the new ones first, the old let go after — the
    /// tile is drawable at every instant.
    ///
    /// Returns whether anything changed.
    pub(crate) fn restitch(
        &mut self,
        gpu: &GpuContext,
        plan: &tuile_core::stitch::Stitch,
        frame: u64,
        slide: u32,
        fresh: bool,
    ) -> bool {
        // More knots than the uniform holds: the tile keeps its own edges.
        let nothing = tuile_core::stitch::Stitch::default();
        let plan = if plan.packed().len() > STITCH_ROWS {
            tuile_core::metrics::metrics().stitch_overflows.inc();
            &nothing
        } else {
            plan
        };
        let told = &self.stitching.told;
        if told.sides == plan.sides && told.inserts == plan.inserts && told.laps == plan.laps {
            return false;
        }
        if told.inserts != plan.inserts {
            for (prepared, mesh) in self.meshes.iter_mut().zip(&self.source) {
                let cut = tuile_core::stitch::split(mesh, self.origin_ecef, &plan.inserts);
                let (vertex_buf, index_buf, bytes) = mesh_buffers(gpu, &cut);
                self.gpu_bytes = self.gpu_bytes + bytes
                    - (prepared.vertex_buf.size() + prepared.index_buf.size()) as usize;
                prepared.index_count = cut.indices.len() as u32;
                prepared.vertex_buf = vertex_buf;
                prepared.index_buf = index_buf;
            }
        }
        // The strips it had become the ones it slides from.
        let rows = |stitch: &tuile_core::stitch::Stitch| {
            let mut rows = stitch.packed();
            rows.resize(STITCH_ROWS, [0.0; 4]);
            rows
        };
        let (now, was) = (rows(plan), rows(&self.stitching.told));
        let at_once = fresh || slide == 0;
        gpu.queue
            .write_buffer(&self.tile_buf, STRIPS_AT, bytemuck::cast_slice(&now));
        gpu.queue.write_buffer(
            &self.tile_buf,
            STRIPS_AT + (STITCH_ROWS * 16) as u64,
            bytemuck::cast_slice(if at_once { &now } else { &was }),
        );
        let any = f32::from(!(plan.is_nothing() && (at_once || self.stitching.told.is_nothing())));
        let slid = if at_once { 1.0 } else { 0.0 };
        gpu.queue.write_buffer(
            &self.tile_buf,
            STITCH_AT,
            bytemuck::cast_slice(&[slid, any, 0.0, 0.0]),
        );
        self.stitching.told = plan.clone();
        self.stitching.sliding_since = (!at_once).then_some(frame);
        true
    }

    /// Moves a sliding tile on to where `frame` has it. Returns whether it
    /// is still on its way.
    pub(crate) fn slide(&mut self, queue: &wgpu::Queue, frame: u64, slide: u32) -> bool {
        let Some(since) = self.stitching.sliding_since else {
            return false;
        };
        let slid = ((frame - since) as f32 / slide.max(1) as f32).clamp(0.0, 1.0);
        // Eased at both ends: an edge sets off and arrives without a jerk.
        let eased = slid * slid * (3.0 - 2.0 * slid);
        let any = f32::from(!self.stitching.told.is_nothing() || slid < 1.0);
        queue.write_buffer(
            &self.tile_buf,
            STITCH_AT,
            bytemuck::cast_slice(&[eased, any, 0.0, 0.0]),
        );
        if slid >= 1.0 {
            self.stitching.sliding_since = None;
        }
        slid < 1.0
    }

    /// How the tile is stitched: the plan it was last given.
    pub fn stitch(&self) -> &tuile_core::stitch::Stitch {
        &self.stitching.told
    }

    /// What the tile is dressed with: each drape it wears and the bits of the
    /// uv transform it is worn through.
    pub fn dressing(&self) -> &[(ResourceId, [u64; 4])] {
        &self.dressing
    }

    /// Whether the tile wears `drape`.
    pub fn wears(&self, drape: ResourceId) -> bool {
        self.dressing.iter().any(|(r, _)| *r == drape)
    }

    /// Dresses the tile with `drapes`, each worn through its uv transform:
    /// the layers of each, placed in this tile's uv square, in stacking
    /// order. Nothing is uploaded — the drapes' textures are bound — and a
    /// tile already dressed so is left as it is.
    pub fn dress(
        &mut self,
        gpu: &GpuContext,
        drapes: &[(ResourceId, &PreparedDrape, UvTransform)],
    ) {
        let key: Vec<(ResourceId, [u64; 4])> =
            drapes.iter().map(|(r, _, uv)| (*r, uv_bits(*uv))).collect();
        if key == self.dressing && !self._imagery.is_empty() {
            return;
        }
        let mut layers: Vec<(Placement, &Arc<GpuImagery>, ImageryCoord, u32)> = self
            .own
            .iter()
            .map(|l| (l.placement, &l.texture, l.coord, l.source))
            .collect();
        for (_, drape, uv) in drapes {
            for l in &drape.layers {
                let placed = if *uv == UvTransform::IDENTITY {
                    Some(l.placement)
                } else {
                    l.placement.through(*uv)
                };
                if let Some(p) = placed {
                    layers.push((p, &l.texture, l.coord, l.source));
                }
            }
        }
        self.sharpest_imagery_level = layers.iter().map(|(_, _, c, _)| c.level).max();
        self.imagery_source = layers.iter().map(|(_, _, _, s)| *s).max();
        let passes = passes(gpu, &layers);
        for (prepared, mesh) in self.meshes.iter_mut().zip(&self.source) {
            let (bgs, bufs) = materials(gpu, mesh, &self._textures, &passes);
            prepared.material_bgs = bgs;
            prepared._material_bufs = bufs;
        }
        self._imagery = passes;
        self.dressing = key;
    }

    /// Takes every drape off: the tile shows its base colour (and the
    /// textures it owns) until it is dressed again. What an evicted drape's
    /// wearers are left in, so its textures are let go of with it.
    pub fn undress(&mut self, gpu: &GpuContext) {
        if self.dressing.is_empty() {
            return;
        }
        self.dress(gpu, &[]);
        self.dressing.clear();
    }

    /// Whether any mesh here carries more layers than one draw could bind.
    ///
    /// Asked before switching pipelines rather than discovered mesh by mesh: a
    /// scene where nothing needs a second pass must not pay for a pipeline
    /// switch, and most scenes are that scene.
    pub fn needs_more_passes(&self) -> bool {
        self.meshes.iter().any(|m| m.material_bgs.len() > 1)
    }

    /// Recomputes the model matrix relative to a new render origin and rewrites
    /// the tile uniform — the second half of the anti-jitter protocol for a
    /// MOVING camera: keep the render origin near the eye so the f32 the GPU
    /// sees stays small (sub-meter precise), even at planetary ECEF scale.
    pub fn rebase(&self, queue: &wgpu::Queue, render_origin: DVec3) {
        // Already there, to within a metre. The threshold used to live on the
        // caller, which meant it was asked once for the whole resident set: if
        // the eye had moved, *every* tile was rewritten. Asking per tile is what
        // lets a tile nobody is drawing simply not be written.
        //
        // A metre of staleness is invisible. The whole point of the render
        // origin is to keep the f32 the GPU sees small, and a metre out of the
        // tens of kilometres it is allowed to drift costs no precision at all.
        const CLOSE_ENOUGH_METRES: f64 = 1.0;
        if (render_origin - self.rebased_to.get()).length() < CLOSE_ENOUGH_METRES {
            return;
        }
        self.rebased_to.set(render_origin);
        let model =
            tuile_core::geo::rebased_model(self.origin_ecef, self.transform_local, render_origin);
        queue.write_buffer(
            &self.tile_buf,
            0,
            bytemuck::cast_slice(&model.to_cols_array()),
        );
    }
}

/// Uploads a decoded tile. `render_origin` is the f64 world point the view
/// matrix is relative to: the tile's model matrix is the (f32) translation
/// from its own rebasing origin to that render origin — the second half of
/// the anti-jitter protocol.
pub fn prepare(
    gpu: &GpuContext,
    content: &DecodedTileContent,
    render_origin: DVec3,
) -> PreparedTile {
    let model = tuile_core::geo::rebased_model(
        content.local_origin_ecef,
        content.transform_local,
        render_origin,
    );

    // The model, and no stitching: zeros say a tile has none.
    let mut uniform = vec![0u8; TILE_UNIFORM_BYTES];
    uniform[..64].copy_from_slice(bytemuck::cast_slice(&model.to_cols_array()));
    let tile_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile tile uniform"),
            contents: &uniform,
            // COPY_DST so the model can be rewritten on rebase (moving camera).
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
    let tile_bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tuile tile"),
        layout: &gpu.tile_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: tile_buf.as_entire_binding(),
        }],
    });

    // Textures the tile owns, uploaded for it alone.
    let textures: Vec<GpuImagery> = content
        .textures
        .iter()
        .map(|t| upload_texture(gpu, t))
        .collect();
    let mut gpu_bytes: usize = textures.iter().map(|t| t.bytes).sum();

    // The imagery the content carries itself, uploaded once per imagery
    // tile however many tiles name it. Most content carries none: it is
    // drawn undressed — one pass, the base colour, every slot masked out —
    // and the drapes it wears are bound when it is drawn (`dress`).
    let own: Vec<DrapedLayer> = content
        .imagery
        .iter()
        .filter(|l| l.is_visible())
        .map(|l| DrapedLayer {
            placement: l.placement(),
            texture: gpu.shared_imagery(l.source, l.coord, || upload_texture(gpu, &l.texture)),
            coord: l.coord,
            source: l.source,
        })
        .collect();
    let worn: Vec<(Placement, &Arc<GpuImagery>, ImageryCoord, u32)> = own
        .iter()
        .map(|l| (l.placement, &l.texture, l.coord, l.source))
        .collect();
    let passes = passes(gpu, &worn);
    let sharpest_imagery_level = own.iter().map(|l| l.coord.level).max();
    let imagery_source = own.iter().map(|l| l.source).max();
    drop(worn);

    // Each mesh with its normals, made where it has none: what is uploaded,
    // and what a mesh that gains vertices is made again from.
    let source: Vec<DecodedMesh> = content
        .meshes
        .iter()
        .map(|mesh| {
            let mut mesh = mesh.clone();
            if mesh.normals.is_none() {
                mesh.normals = Some(compute_normals(&mesh.positions, &mesh.indices));
            }
            mesh
        })
        .collect();
    let meshes = source
        .iter()
        .map(|m| prepare_mesh(gpu, m, &textures, &passes, &mut gpu_bytes))
        .collect::<Vec<_>>();
    let edges = tuile_core::stitch::Edges::of(content);
    let cell = edges.cell();

    PreparedTile {
        sharpest_imagery_level,
        imagery_source,
        meshes,
        tile_bg,
        tile_buf,
        edges,
        cell,
        source,
        stitching: Stitching::default(),
        origin_ecef: content.local_origin_ecef,
        transform_local: content.transform_local,
        // Built against this origin just above, so it starts up to date — a
        // fresh tile that claimed otherwise would be rewritten on its first
        // frame for nothing.
        rebased_to: std::cell::Cell::new(render_origin),
        _textures: textures,
        own,
        _imagery: passes,
        dressing: Vec::new(),
        gpu_bytes,
    }
}

/// One imagery tile of a drape on the GPU: where it sits in its tile, and
/// the shared texture.
pub(crate) struct DrapedLayer {
    pub(crate) placement: Placement,
    pub(crate) texture: Arc<GpuImagery>,
    pub(crate) coord: ImageryCoord,
    pub(crate) source: u32,
}

/// A drape on the GPU: its imagery tiles' textures, shared with every other
/// drape that names them, and where each sits in its tile.
pub struct PreparedDrape {
    /// The layer it is of.
    pub layer: LayerId,
    /// The budget's imagery level.
    pub level: u32,
    pub(crate) layers: Vec<DrapedLayer>,
    /// What it is charged: the budget's estimate of its textures, the shared
    /// floor excepted — what [`DrapeContent::cost`] says.
    pub gpu_bytes: u64,
}

/// Uploads a drape: each of its imagery tiles once however many drapes name
/// it (keyed by imagery set and coord), and its placements.
pub fn prepare_drape(gpu: &GpuContext, drape: &DrapeContent) -> PreparedDrape {
    let layers = drape
        .tiles
        .iter()
        .filter(|l| l.is_visible())
        .map(|l| DrapedLayer {
            placement: l.placement(),
            texture: gpu.shared_imagery(l.source, l.coord, || upload_texture(gpu, &l.texture)),
            coord: l.coord,
            source: l.source,
        })
        .collect();
    PreparedDrape {
        layer: drape.layer,
        level: drape.level,
        layers,
        gpu_bytes: drape.cost().gpu.0,
    }
}

/// A uv transform's bits, for telling dressings apart.
fn uv_bits(uv: UvTransform) -> [u64; 4] {
    [
        uv.scale[0].to_bits(),
        uv.scale[1].to_bits(),
        uv.offset[0].to_bits(),
        uv.offset[1].to_bits(),
    ]
}

/// The imagery passes that bind `layers`, in stacking order: as many as one
/// draw can bind per pass, at most `MAX_IMAGERY_PASSES`. Never none: content
/// with no imagery still has one pass, carrying its base colour, every slot
/// masking itself out.
fn passes(
    gpu: &GpuContext,
    layers: &[(Placement, &Arc<GpuImagery>, ImageryCoord, u32)],
) -> Vec<ImageryPass> {
    let slots = gpu.imagery_slots as usize;
    let table = |batch: &[(Placement, &Arc<GpuImagery>, ImageryCoord, u32)]| {
        gpu.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile imagery layers"),
                contents: bytemuck::cast_slice(&raster::placement_table(
                    batch.iter().map(|l| l.0),
                    gpu.imagery_slots,
                )),
                usage: wgpu::BufferUsages::UNIFORM,
            })
    };
    let passes: Vec<ImageryPass> = layers
        .chunks(slots.max(1))
        .take(raster::MAX_IMAGERY_PASSES as usize)
        .map(|batch| ImageryPass {
            textures: batch.iter().map(|l| Arc::clone(l.1)).collect(),
            table: table(batch),
        })
        .collect();
    if passes.is_empty() {
        vec![ImageryPass {
            textures: Vec::new(),
            table: table(&[]),
        }]
    } else {
        passes
    }
}

/// Uploads a base-color texture with its full mip chain.
///
/// The chain is built on the CPU ([`raster::mip_chain`]) rather than blitted
/// level-by-level on the GPU. Apple's Metal driver leaks memory in proportion
/// to the number of **render passes** created — `AGX::Compiler::compileProgram`
/// under `renderCommandEncoderWithDescriptor`, see gfx-rs/wgpu#8768; Dawn has
/// it too, and no wgpu-side workaround exists. A GPU chain costs one render
/// pass per level per texture: at eight uploads a frame that is ~80 passes a
/// frame, enough to exhaust the driver in minutes and take the machine down
/// with it. Filtering on the CPU costs zero render passes.
///
/// RGBA8 only. Block-compressed texels have no upload here yet and no path
/// hands them one; if one ever did, the stand-in slate is drawn instead —
/// ground shown flat, never a hole and never black. A debug build stops on
/// it, so a real occurrence fails the tests rather than hide under the slate.
fn upload_texture(gpu: &GpuContext, t: &DecodedTexture) -> GpuImagery {
    let stand_in;
    let t = if t.rgba8().is_some() {
        t
    } else {
        debug_assert!(false, "block-compressed texels reached the RGBA8 upload");
        stand_in = DecodedTexture::stand_in();
        &stand_in
    };
    let mips = raster::mip_chain(t);
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("tuile base color"),
        size: wgpu::Extent3d {
            width: t.width,
            height: t.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1 + mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        // No RENDER_ATTACHMENT: nothing draws into this texture any more.
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    let mut bytes = 0usize;
    for (level, image) in std::iter::once(t).chain(mips.iter()).enumerate() {
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            image.rgba8().unwrap_or_default(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * BYTES_PER_TEXEL),
                rows_per_image: Some(image.height),
            },
            wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );
        bytes += image.texels.byte_len();
    }

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    GpuImagery::new(texture, view, bytes)
}

fn prepare_mesh(
    gpu: &GpuContext,
    mesh: &DecodedMesh,
    textures: &[GpuImagery],
    passes: &[ImageryPass],
    gpu_bytes: &mut usize,
) -> PreparedMesh {
    let (vertex_buf, index_buf, bytes) = mesh_buffers(gpu, mesh);
    *gpu_bytes += bytes;
    let (material_bgs, material_bufs) = materials(gpu, mesh, textures, passes);
    PreparedMesh {
        vertex_buf,
        index_buf,
        index_count: mesh.indices.len() as u32,
        material_bgs,
        _material_bufs: material_bufs,
    }
}

/// One bind group per imagery pass for `mesh`. They differ in two things
/// and only two: which batch of layers is bound, and whether the shader is
/// told this is the first pass — which is what decides that a later pass
/// starts from nothing and writes only where its own layers reached.
fn materials(
    gpu: &GpuContext,
    mesh: &DecodedMesh,
    textures: &[GpuImagery],
    passes: &[ImageryPass],
) -> (Vec<wgpu::BindGroup>, Vec<wgpu::Buffer>) {
    let texture_view = mesh
        .material
        .base_color_texture
        .and_then(|i| textures.get(i))
        .map(|t| &t.view)
        .unwrap_or(&gpu.white_view);
    let mut material_bufs = Vec::with_capacity(passes.len());
    let mut material_bgs = Vec::with_capacity(passes.len());
    for (index, imagery) in passes.iter().enumerate() {
        let first = f32::from(index == 0);
        let uniform = MaterialUniform {
            base_color: mesh.material.base_color_factor,
            flags: [first, 0.0, 0.0, 0.0],
        };
        let material_buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tuile material"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        // Every slot is bound, always. An unused one reads the 1×1 white texture
        // and is masked out by an empty coverage rectangle, so the shader needs
        // no count and no branch — see `raster::imagery_layer_table`.
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: material_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: imagery.table.as_entire_binding(),
            },
        ];
        entries.extend((0..gpu.imagery_slots).map(|slot| {
            wgpu::BindGroupEntry {
                binding: IMAGERY_BINDING_0 + slot,
                resource: wgpu::BindingResource::TextureView(
                    imagery
                        .textures
                        .get(slot as usize)
                        .map(|t| &t.view)
                        .unwrap_or(&gpu.white_view),
                ),
            }
        }));
        material_bgs.push(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile material"),
            layout: &gpu.material_bgl,
            entries: &entries,
        }));
        material_bufs.push(material_buf);
    }
    (material_bgs, material_bufs)
}

/// A mesh's vertex and index buffers, and their size in bytes.
fn mesh_buffers(gpu: &GpuContext, mesh: &DecodedMesh) -> (wgpu::Buffer, wgpu::Buffer, usize) {
    let normals = match &mesh.normals {
        Some(n) => n.clone(),
        None => compute_normals(&mesh.positions, &mesh.indices),
    };
    let vertices: Vec<Vertex> = mesh
        .positions
        .iter()
        .enumerate()
        .map(|(i, p)| Vertex {
            position: *p,
            normal: normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]),
            uv: mesh
                .uvs
                .as_ref()
                .and_then(|uv| uv.get(i))
                .copied()
                .unwrap_or([0.0, 0.0]),
        })
        .collect();
    let vertex_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
    let index_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tuile indices"),
            contents: bytemuck::cast_slice(&mesh.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
    let bytes = vertices.len() * std::mem::size_of::<Vertex>() + mesh.indices.len() * 4;
    (vertex_buf, index_buf, bytes)
}

/// Area-weighted vertex normals for meshes that ship without them.
fn compute_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![Vec3::ZERO; positions.len()];
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (pa, pb, pc) = (
            Vec3::from(positions[a]),
            Vec3::from(positions[b]),
            Vec3::from(positions[c]),
        );
        let n = (pb - pa).cross(pc - pa);
        acc[a] += n;
        acc[b] += n;
        acc[c] += n;
    }
    acc.into_iter()
        .map(|n| n.normalize_or_zero().to_array())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::texels::{BlockFormat, Texels};

    /// Block-compressed texels have no upload here yet. One that arrived
    /// anyway stops a debug build, and a release build draws the stand-in —
    /// a real texture with its chain, never an empty one.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "block-compressed texels reached the RGBA8 upload")
    )]
    fn blocks_upload_the_stand_in() {
        let gpu = match pollster::block_on(GpuContext::headless()) {
            Ok(gpu) => gpu,
            Err(e) => {
                assert!(
                    std::env::var("TUILE_REQUIRE_GPU").is_err(),
                    "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
                );
                eprintln!("SKIP: {e}");
                return;
            }
        };
        let blocks = DecodedTexture {
            width: 4,
            height: 4,
            texels: Texels::Blocks {
                format: BlockFormat::Bc7,
                levels: vec![vec![0; 16]; 3],
            },
        };
        let uploaded = upload_texture(&gpu, &blocks);
        assert_eq!(uploaded.bytes, 4, "one stand-in texel");
        let rgba = upload_texture(&gpu, &DecodedTexture::from_rgba8(2, 2, vec![9; 16]));
        assert_eq!(rgba.bytes, 16 + 4, "RGBA8 uploads its chain as before");
    }
}
