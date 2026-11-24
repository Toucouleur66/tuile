// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The preload, end to end, on the session's own need (`Session::need`:
//! the neighbourhood, the chain of ancestors, the floor) over a flat
//! quadtree of draped ground: what the view will want next is resident —
//! the mesh **and** its imagery — before it wants it
//! (`docs/explorations/preload-audit.md`).

use super::*;
use crate::budget::BudgetStats;
use std::sync::atomic::AtomicBool;

/// The quadtree's depth: levels `0..DEPTH`.
const DEPTH: u32 = 18;
/// The side of the level-0 tile, metres.
const SIDE: f64 = 1_048_576.0;
/// The ground's height above the frame's origin: far enough that the
/// origin-to-tile direction, the session's "up", is the plane's normal.
const GROUND: f64 = 6_378_137.0;

fn side(level: u32) -> f64 {
    SIDE / f64::from(1u32 << level)
}

/// Flat ground, every tile with content, the geometric error an
/// eighth of the tile's side over four.
struct Quad;

impl TileTree for Quad {
    fn roots(&self) -> Vec<TileId> {
        vec![TileId::from_terrain(0, 0, 0)]
    }
    fn children(&self, id: TileId) -> Vec<TileId> {
        let (l, x, y) = id.terrain_coord();
        if l + 1 >= DEPTH {
            return Vec::new();
        }
        [(0, 0), (1, 0), (0, 1), (1, 1)]
            .into_iter()
            .map(|(dx, dy)| TileId::from_terrain(l + 1, 2 * x + dx, 2 * y + dy))
            .collect()
    }
    fn parent(&self, id: TileId) -> Option<TileId> {
        let (l, x, y) = id.terrain_coord();
        (l > 0).then(|| TileId::from_terrain(l - 1, x / 2, y / 2))
    }
    fn level(&self, id: TileId) -> u32 {
        id.terrain_coord().0
    }
    fn revision(&self) -> Option<u64> {
        Some(0)
    }
    fn properties(&self, id: TileId) -> crate::source::TileProperties {
        let (l, x, y) = id.terrain_coord();
        let s = side(l);
        crate::source::TileProperties {
            bounding_volume: crate::math::BoundingVolume::Sphere(crate::math::Sphere {
                center: dvec3((x as f64 + 0.5) * s, (y as f64 + 0.5) * s, GROUND),
                radius: s * std::f64::consts::FRAC_1_SQRT_2,
            }),
            geometric_error: if l + 1 >= DEPTH { 0.0 } else { s / 32.0 },
            refine: crate::tileset::Refine::Replace,
            has_content: true,
        }
    }
}

/// Serves the quad's meshes and drapes at once, until the network is cut:
/// then every load waits for ever.
#[derive(Default)]
struct QuadLoader {
    cut: Arc<AtomicBool>,
}

impl QuadLoader {
    fn rect(id: TileId) -> GeoRect {
        let (l, x, y) = id.terrain_coord();
        let size = 1.0e-2 / f64::from(1u32 << l);
        GeoRect {
            west: x as f64 * size,
            east: (x + 1) as f64 * size,
            south: y as f64 * size,
            north: (y + 1) as f64 * size,
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl TileLoader for QuadLoader {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        if self.cut.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let (l, x, y) = id.terrain_coord();
        let s = side(l) as f32;
        let origin = dvec3(
            (x as f64 + 0.5) * side(l),
            (y as f64 + 0.5) * side(l),
            GROUND,
        );
        Ok(Loaded::Content(crate::content::DecodedTileContent {
            meshes: vec![crate::content::DecodedMesh {
                positions: vec![
                    [-s / 2.0, -s / 2.0, 0.0],
                    [s / 2.0, -s / 2.0, 0.0],
                    [s / 2.0, s / 2.0, 0.0],
                    [-s / 2.0, s / 2.0, 0.0],
                ],
                normals: None,
                uvs: Some(vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]),
                indices: vec![0, 1, 2, 0, 2, 3],
                material: Default::default(),
            }],
            textures: Vec::new(),
            imagery: Vec::new(),
            shed: Vec::new(),
            apart: None,
            withheld_drape: None,
            copyright: None,
            local_origin_ecef: origin,
            transform_local: glam::Mat4::IDENTITY,
        }))
    }
    fn drapes(&self, id: TileId) -> Option<DrapeReach> {
        Some(DrapeReach {
            texel_l0_m: SIDE / 512.0,
            rect: Self::rect(id),
            layers: vec![LayerReach {
                layer: LayerId(0),
                max_level: 22,
            }],
            substitutes: Vec::new(),
        })
    }
    async fn load_drape(
        &self,
        _id: TileId,
        layer: LayerId,
        level: u32,
    ) -> Result<DrapeContent, LoadError> {
        if self.cut.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let texture = Arc::new(crate::content::DecodedTexture::from_rgba8(
            4,
            4,
            vec![level as u8; 64],
        ));
        let unit = crate::raster::GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        Ok(DrapeContent {
            layer,
            level,
            tiles: vec![crate::raster::ImageryLayer::placed(
                crate::raster::ImageryCoord { level, x: 0, y: 0 },
                texture,
                &unit,
                &unit,
            )],
            floor_tiles: 0,
            identity: u64::from(level),
            withheld: false,
        })
    }
}

/// Every step's statistics, and the last step's need.
#[derive(Default)]
struct Steps {
    stats: Vec<BudgetStats>,
    need: Option<NeedSet>,
}

struct Watch(Arc<Mutex<Steps>>);

impl BudgetObserver for Watch {
    fn step(&mut self, s: &StepRecord<'_>) {
        let mut w = self.0.lock().expect("steps");
        w.stats.push(s.decisions.stats.clone());
        w.need = Some(s.need.clone());
    }
}

/// The viewer's budget: the floor pinned to level 2, a radius of four
/// tiles, room for everything.
fn viewer_budget() -> BudgetConfig {
    let mut b = BudgetConfig::interactive(Bytes::mib(512));
    b.pinned_level = Some(2);
    b.reach_tiles = 4;
    b
}

/// A session over the quad, the switch that cuts its network, and its
/// steps.
fn quad_session() -> (
    InProcessStream,
    GeometryServer,
    Arc<AtomicBool>,
    Arc<Mutex<Steps>>,
) {
    let loader = QuadLoader::default();
    let cut = Arc::clone(&loader.cut);
    let (stream, server) = in_process_with(
        Box::new(Quad),
        Arc::new(loader),
        Config::default(),
        viewer_budget(),
    )
    .expect("budget");
    let steps = Arc::new(Mutex::new(Steps::default()));
    let server = server.with_observer(Box::new(Watch(Arc::clone(&steps))));
    (stream, server, cut, steps)
}

/// Looking straight down at `(fx, fy)` of the level-0 tile, from `height`
/// metres over the ground.
fn nadir(fx: f64, fy: f64, height: f64) -> ViewState {
    ViewState::perspective(
        dvec3(fx * SIDE, fy * SIDE, GROUND + height),
        dvec3(0.0, 0.0, -1.0),
        dvec3(0.0, 1.0, 0.0),
        dvec2(960.0, 720.0),
        45f64.to_radians(),
    )
}

/// The finest level drawn as itself.
fn finest(consumer: &Consumer) -> u32 {
    consumer
        .exact()
        .iter()
        .map(|t| t.terrain_coord().0)
        .max()
        .unwrap_or(0)
}

/// The tiles whose mesh and a drape the consumer holds.
fn dressed(consumer: &Consumer) -> HashSet<TileId> {
    let draped: HashSet<TileId> = consumer
        .held
        .keys()
        .filter(|r| matches!(r, ResourceId::Drape { .. }))
        .map(|r| r.tile())
        .collect();
    consumer
        .held
        .keys()
        .filter_map(|r| match r {
            ResourceId::Mesh(t) if draped.contains(t) => Some(*t),
            _ => None,
        })
        .collect()
}

const F: (f64, f64) = (0.3712, 0.4137);
/// High enough that the view needs level 12 or so.
fn start_height() -> f64 {
    2.5 * side(12)
}

/// **At rest, the neighbourhood is resident with its imagery**: once a
/// still view has settled, at every level from the first above the floor
/// to one under the view's, at least `K(ρ)` tiles (or the whole level)
/// hold their mesh and a drape, and every preload the session lists there
/// — the ring around the view, the chain above it — does. Red with the
/// preloads' drapes never asked (the meshes alone come in).
#[test]
fn at_rest_the_neighbourhood_is_resident_with_its_imagery() {
    let (mut stream, server, _cut, steps) = quad_session();
    let mut server = Box::pin(server.run());
    let mut consumer = Consumer::default();
    look(&stream, nadir(F.0, F.1, start_height()), 1);
    assert!(
        settle(&mut server, &mut stream, &mut consumer),
        "never settled"
    );
    let n = finest(&consumer);
    assert!(n >= 9, "the fixture is wrong: the view needs level {n}");
    let dressed = dressed(&consumer);
    let k = crate::neighbourhood::per_level(4);
    for l in 3..=n + 1 {
        let at = dressed.iter().filter(|t| t.terrain_coord().0 == l).count();
        let whole = 1usize << (2 * l.min(16));
        assert!(
            at >= k.min(whole),
            "level {l}: {at} tiles hold their mesh and a drape, K(ρ) = {k}"
        );
    }
    let need = steps.lock().expect("steps").need.clone().expect("a step");
    let mut missing = Vec::new();
    for (t, _) in need.ring.iter().chain(&need.ancestors) {
        let l = t.terrain_coord().0;
        if (3..=n + 1).contains(&l) && !dressed.contains(t) {
            missing.push((l, *t));
        }
    }
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "{} preloads without their mesh and a drape: {:?}",
        missing.len(),
        &missing[..missing.len().min(12)]
    );
}

/// **A zoom-out finds its ground dressed**: a still view settled at level
/// `n`, then the network cut and the view pulled back four levels, a
/// quarter level a frame: at every step the ground is drawn by its own
/// tiles (no fallback, nothing lost, nothing bare), each wearing a drape
/// within one level of its need — the preloads held both. Red with the
/// preloads' drapes never asked: the coarser tiles come in on their mesh
/// alone and wear the floor's imagery.
#[test]
fn a_zoom_out_of_four_levels_finds_its_ground_dressed_with_the_network_cut() {
    let (mut stream, server, cut, steps) = quad_session();
    let mut server = Box::pin(server.run());
    let mut consumer = Consumer::default();
    let h = start_height();
    look(&stream, nadir(F.0, F.1, h), 1);
    assert!(
        settle(&mut server, &mut stream, &mut consumer),
        "never settled"
    );
    let n = finest(&consumer);
    assert!(n >= 9, "the fixture is wrong: the view needs level {n}");
    cut.store(true, Ordering::SeqCst);
    let from = steps.lock().expect("steps").stats.len();
    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    for i in 1..=16u64 {
        look(
            &stream,
            nadir(F.0, F.1, h * 2f64.powf(i as f64 / 4.0)),
            1 + i,
        );
        for _ in 0..4 {
            let _ = server.as_mut().poll(&mut cx);
            consumer.take(&mut stream, &mut cx);
        }
    }
    for _ in 0..16 {
        let _ = server.as_mut().poll(&mut cx);
        consumer.take(&mut stream, &mut cx);
    }
    let now = finest(&consumer);
    assert!(
        now + 3 <= n,
        "the fixture is wrong: the view went from level {n} to {now}"
    );
    let steps = steps.lock().expect("steps");
    assert!(steps.stats.len() > from + 16, "the zoom-out made no steps");
    for (i, st) in steps.stats[from..].iter().enumerate() {
        assert_eq!(
            (st.lost, st.bare, st.fallback, st.underdraped),
            (0, 0, 0, 0),
            "step {i} of the zoom-out: (lost, bare, fallback, underdraped)"
        );
    }
}
