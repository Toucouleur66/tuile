// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Phase 1a experiment, evidence only (it may be removed once the
//! architecture is decided): one budget arbiter for terrain and imagery,
//! against two budgets (terrain, imagery) with the GPU pool split between
//! them, over the simulation's scenarios.
//!
//! - **shared**: today's engine, one `Budget`, one ledger, per-class targets,
//!   joint draw set (mesh + drape), joint protection and swaps.
//! - **split-S/I**: two `Budget`s. The terrain one sees the need with no
//!   imagery and owns the meshes; the imagery one sees the same need with
//!   its imagery and owns the drapes. The pool (`hard_limit` of meshes plus
//!   drapes in the shared profile) is split S % / I %, and so are the upload
//!   bytes per frame, the delivery credit and the decoded tier. The imagery
//!   budget learns which meshes are drawable by a mirror: its mesh requests
//!   never reach the network and are answered once the terrain budget's GPU
//!   holds the mesh (one or two steps late, as a message between two
//!   budgets would be). The frame is the terrain budget's draw set, each
//!   item wearing the imagery budget's drape for it or, failing that, the
//!   finest drape the imagery GPU holds on the tile or its nearest ancestor.
//! - **shared-S/I**, the control: the one arbiter with its per-class
//!   targets re-split S % / I % of the same pool, so that the ratio's effect
//!   is told apart from the architecture's.
//! - **split-drawn**: the matched split, but the imagery budget only sees
//!   the tiles the terrain budget draws (an imagery stream keyed on the
//!   terrain stream's output, not on the view).
//!
//! The split variants are generous to themselves: the imagery budget's
//! request heap is 4096 (the mirrored mesh wants must not starve its
//! drapes) and its mirrored meshes cost one byte.
//!
//! Run: `cargo test --release -p tuile-core --test budget_sim -- --ignored
//! architecture --nocapture`.

use std::collections::{BTreeMap, BTreeSet};

use tuile_core::budget::cost::Cost;
use tuile_core::budget::imagery::{needed_level, LevelClamp};
use tuile_core::budget::{
    Budget, BudgetConfig, Bytes, Class, Decisions, FrameInput, Generation, Micros, NeedSet,
    Presence, PresenceOracle, Pressure, ResourceId, Role, TileId,
};

use super::gpu::SimGpu;
use super::net::{wire_bytes, Arrival, SimNet, SimStore};
use super::scenarios::{approach, back_and_forth, home, turn, zoom};
use super::server::Profile;
use super::world::{cost, parent, tile, traverse, Camera, Residency, View};

/// One drawn item of the frame: mesh, role, the tiles it serves, the drape
/// it wears (resource, level).
type FrameItem = (ResourceId, Role, Vec<TileId>, Option<(ResourceId, u32)>);

/// Which architecture runs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Arch {
    Shared,
    /// The control: one arbiter, its per-class targets re-split at
    /// terrain's percent of the same pool.
    SharedRatio(u64),
    /// Terrain's percent of the pool; the imagery budget sees the view.
    Split(u64),
    /// The matched split; the imagery budget sees only what terrain draws.
    SplitDrawn(u64),
}

impl Arch {
    fn name(self) -> String {
        match self {
            Arch::Shared => "shared".into(),
            Arch::SharedRatio(s) => format!("shared-{s}/{}", 100 - s),
            Arch::Split(s) => format!("split-{s}/{}", 100 - s),
            Arch::SplitDrawn(s) => format!("split-drawn-{s}/{}", 100 - s),
        }
    }
}

/// The imagery budget's GPU: drapes only.
#[derive(Default)]
struct DrapeGpu {
    drawable: BTreeMap<ResourceId, Bytes>,
    pending: BTreeMap<ResourceId, Generation>,
    bytes: Bytes,
}

/// What every architecture is measured on, per run.
#[derive(Clone, Debug, Default)]
struct Metrics {
    /// Tile-steps with nothing to draw, after the floor is in (I1).
    lost: u64,
    /// Drawn items wearing no drape at all, after the floor is in: bare
    /// ground, black in a renderer with no default colour (I1).
    bare: u64,
    /// Drapes evicted in the step they were worn (I11; a hole next frame).
    worn_evicted: u64,
    /// Exact tile-steps whose drape is coarser than the unrelaxed need by
    /// more than a level (measured outside the budgets, the same for all).
    blurry: u64,
    /// Exact tile-steps.
    exact: u64,
    /// Seconds from the stop (or floor) to the start of the converged run
    /// that lasts to the end; `None` if the last step is not converged.
    converged_s: Option<f64>,
    /// Steps the pool's bytes exceed the pool's target (meshes + drapes).
    over_target_steps: u64,
    /// The largest excess over the pool target, MiB.
    over_target_max_mib: f64,
    /// Steps a class crossed its own hard limit.
    over_hard_steps: u64,
    /// The worst level changes of a tile within 10 s while still (I4).
    flapping: usize,
    /// Mean and 99th percentile of the step's `decide` calls, µs.
    decide_mean_us: f64,
    decide_p99_us: f64,
    /// Network megabytes.
    net_mib: f64,
    /// Evict-then-request within 10 s.
    thrash: u64,
    /// Drawn exact tiles whose own drape changed level while the mesh
    /// stayed: under imagery-as-payload each is the mesh sent again.
    redrapes: u64,
    /// The mesh wire bytes those redrapes would resend, MiB.
    redrape_mesh_mib: f64,
}

struct Run {
    arch: Arch,
    profile: Profile,
    terrain: Budget,
    imagery: Option<Budget>,
    net: SimNet,
    gpu: SimGpu,
    dgpu: DrapeGpu,
    /// Imagery budget's mirrored meshes: started, awaiting terrain.
    mirror_pending: BTreeMap<ResourceId, Generation>,
    /// Delivered mirrored meshes, to acknowledge.
    mirror_delivered: BTreeMap<ResourceId, Generation>,
    now: Micros,
    frame: u64,
    sse_scale: f64,
    drawn_last: BTreeSet<TileId>,
    last_cam: Option<Camera>,
    floor_at: Option<Micros>,
    pool_target: Bytes,
    m: Metrics,
    decide_us: Vec<f64>,
    converged_at: Vec<(Micros, bool)>,
    levels: BTreeMap<TileId, (u32, u32, Vec<Micros>)>,
    worn_last: BTreeMap<TileId, ResourceId>,
}

struct TerrainView<'a>(&'a Budget);
impl Residency for TerrainView<'_> {
    fn resident(&self, t: TileId) -> bool {
        self.0.residency().resident(t)
    }
}

/// The store, as the terrain budget sees it: meshes only.
struct MeshesOnly<'a>(&'a SimStore);
impl PresenceOracle for MeshesOnly<'_> {
    fn holds(&self, r: ResourceId) -> Presence {
        match r {
            ResourceId::Mesh(_) => self.0.holds(r),
            ResourceId::Drape(..) => Presence::Absent,
        }
    }
}

/// The store, as the imagery budget sees it: drapes only (its meshes are
/// mirrored, never read).
struct DrapesOnly<'a>(&'a SimStore);
impl PresenceOracle for DrapesOnly<'_> {
    fn holds(&self, r: ResourceId) -> Presence {
        match r {
            ResourceId::Mesh(_) => Presence::Absent,
            ResourceId::Drape(..) => self.0.holds(r),
        }
    }
}

fn pool(cfg: &BudgetConfig, f: impl Fn(&BudgetConfig, Class) -> Bytes) -> Bytes {
    f(cfg, Class::TerrainMesh).plus(f(cfg, Class::Drape))
}

/// The two budgets of a split, from the shared profile's budget.
fn split_configs(shared: &BudgetConfig, terrain_pct: u64) -> (BudgetConfig, BudgetConfig) {
    let s = terrain_pct;
    let i = 100 - terrain_pct;
    let hard = pool(shared, |c, k| c.hard_limit[k]);
    let target = pool(shared, |c, k| c.target[k]);
    let overflow = pool(shared, |c, k| c.overflow[k]);
    let mut t = shared.clone();
    t.hard_limit[Class::TerrainMesh] = hard.scaled(s, 100);
    t.target[Class::TerrainMesh] = target.scaled(s, 100);
    t.overflow[Class::TerrainMesh] = overflow.scaled(s, 100);
    t.upload_bytes_per_frame = shared.upload_bytes_per_frame.scaled(s, 100);
    t.delivery_credit = shared.delivery_credit.scaled(s, 100);
    t.decoded_target = shared.decoded_target.scaled(s, 100);
    let mut m = shared.clone();
    m.hard_limit[Class::Drape] = hard.scaled(i, 100);
    m.target[Class::Drape] = target.scaled(i, 100);
    m.overflow[Class::Drape] = overflow.scaled(i, 100);
    m.upload_bytes_per_frame = shared.upload_bytes_per_frame.scaled(i, 100);
    m.delivery_credit = shared.delivery_credit.scaled(i, 100);
    m.decoded_target = shared.decoded_target.scaled(i, 100);
    // The mirrored meshes never constrain the imagery budget.
    m.hard_limit[Class::TerrainMesh] = Bytes::gib(64);
    m.target[Class::TerrainMesh] = Bytes::gib(64);
    m.overflow[Class::TerrainMesh] = Bytes::ZERO;
    m.max_in_flight[Class::TerrainMesh] = 4_096;
    m.request_heap = 4_096;
    (t, m)
}

fn strip_imagery(need: &NeedSet) -> NeedSet {
    let mut n = need.clone();
    for list in [&mut n.drawn, &mut n.blockers, &mut n.ancestors, &mut n.ring] {
        for (_, x) in list.iter_mut() {
            x.imagery = None;
        }
    }
    n
}

impl Run {
    fn new(arch: Arch, profile: Profile, seed: u64) -> Self {
        let shared = profile.budget.clone();
        let (terrain, imagery) = match arch {
            Arch::Shared => (Budget::new(shared.clone()).expect("valid"), None),
            Arch::SharedRatio(s) => {
                let (t, i) = split_configs(&shared, s);
                let mut one = shared.clone();
                for (c, from) in [(Class::TerrainMesh, &t), (Class::Drape, &i)] {
                    one.hard_limit[c] = from.hard_limit[c];
                    one.target[c] = from.target[c];
                    one.overflow[c] = from.overflow[c];
                }
                (Budget::new(one).expect("valid"), None)
            }
            Arch::Split(s) | Arch::SplitDrawn(s) => {
                let (t, i) = split_configs(&shared, s);
                (
                    Budget::new(t).expect("valid terrain"),
                    Some(Budget::new(i).expect("valid imagery")),
                )
            }
        };
        Run {
            arch,
            pool_target: pool(&shared, |c, k| c.target[k]),
            terrain,
            imagery,
            net: SimNet::new(profile.net, seed),
            gpu: SimGpu::default(),
            dgpu: DrapeGpu::default(),
            mirror_pending: BTreeMap::new(),
            mirror_delivered: BTreeMap::new(),
            now: Micros::ZERO,
            frame: 0,
            sse_scale: 1.0,
            drawn_last: BTreeSet::new(),
            last_cam: None,
            floor_at: None,
            m: Metrics::default(),
            decide_us: Vec::new(),
            converged_at: Vec::new(),
            levels: BTreeMap::new(),
            worn_last: BTreeMap::new(),
            profile,
        }
    }

    fn pressure(&mut self, p: Pressure) {
        self.terrain.set_pressure(p);
        if let Some(i) = &mut self.imagery {
            i.set_pressure(p);
        }
    }

    fn run(&mut self, secs: f64, script: &dyn Fn(f64) -> Camera) {
        let end = self.now.plus(Micros((secs * 1e6) as u64));
        while self.now < end {
            let t = self.now.0 as f64 / 1e6;
            self.step(script(t));
        }
    }

    fn timed(us: &mut f64, f: impl FnOnce() -> Decisions) -> Decisions {
        let c = std::time::Instant::now();
        let d = f();
        *us += c.elapsed().as_secs_f64() * 1e6;
        d
    }

    fn step(&mut self, cam: Camera) {
        let split = self.imagery.is_some();
        for a in self.net.poll(self.now) {
            let to_imagery = match a {
                Arrival::Loaded(r, ..) | Arrival::Failed(r, _) | Arrival::StoreMiss(r, _) => {
                    split && matches!(r, ResourceId::Drape(..))
                }
            };
            let b = if to_imagery {
                self.imagery.as_mut().expect("split")
            } else {
                &mut self.terrain
            };
            match a {
                Arrival::Loaded(r, g, c) => b.on_loaded(r, g, c),
                Arrival::Failed(r, g) => b.on_failed(r, g, tuile_core::budget::Failure::Transient),
                Arrival::StoreMiss(r, g) => b.on_store_miss(r, g),
            }
        }
        // The mirror: the imagery budget's mesh loads complete once the
        // terrain GPU holds the mesh.
        if let Some(img) = &mut self.imagery {
            let ready: Vec<_> = self
                .mirror_pending
                .iter()
                .filter(|(r, _)| self.gpu.drawable.contains_key(r))
                .map(|(r, g)| (*r, *g))
                .collect();
            for (r, g) in ready {
                self.mirror_pending.remove(&r);
                img.on_loaded(r, g, Cost::flat(Bytes(1)));
            }
        }
        let views = [View {
            cam,
            lens: self.profile.eyes[0].lens,
        }];
        let need = traverse(
            &views,
            &TerrainView(&self.terrain),
            self.sse_scale,
            &self.drawn_last,
            self.profile.budget.pinned_level.unwrap_or(0),
        );
        let mut us = 0.0;
        let dt = if split {
            let tn = strip_imagery(&need);
            let store = MeshesOnly(&self.net.store);
            let input = FrameInput::new(self.frame, self.now, &tn).with_presence(&store);
            let t = &mut self.terrain;
            Self::timed(&mut us, || t.decide(input))
        } else {
            let input = FrameInput::new(self.frame, self.now, &need).with_presence(&self.net.store);
            let t = &mut self.terrain;
            Self::timed(&mut us, || t.decide(input))
        };
        let di = if let Some(img) = &mut self.imagery {
            let ineed = if let Arch::SplitDrawn(_) = self.arch {
                // Only what the terrain stream draws.
                let mut n = NeedSet::new();
                let all: BTreeMap<TileId, _> =
                    need.entries().map(|(_, t, x)| (t, x.clone())).collect();
                for i in &dt.draw.items {
                    let t = i.mesh.tile();
                    if let Some(x) = all.get(&t) {
                        n.drawn.push((t, x.clone()));
                    } else if let Some(x) = i.serves.iter().find_map(|s| all.get(s)) {
                        let mut x = x.clone();
                        x.level = t.terrain_coord().0;
                        x.parent = parent(t);
                        n.drawn.push((t, x));
                    }
                }
                n
            } else {
                need.clone()
            };
            let store = DrapesOnly(&self.net.store);
            let input = FrameInput::new(self.frame, self.now, &ineed).with_presence(&store);
            Some(Self::timed(&mut us, || img.decide(input)))
        } else {
            None
        };
        self.decide_us.push(us);

        // Network.
        for (r, g) in &dt.cancel {
            self.net.cancel(*r, *g);
        }
        for s in &dt.start {
            self.net.start(s, self.now);
        }
        if let Some(di) = &di {
            for (r, g) in &di.cancel {
                match r {
                    ResourceId::Mesh(_) => {
                        if self.mirror_pending.get(r) == Some(g) {
                            self.mirror_pending.remove(r);
                        }
                    }
                    ResourceId::Drape(..) => self.net.cancel(*r, *g),
                }
            }
            for s in &di.start {
                match s.resource {
                    ResourceId::Mesh(_) => {
                        self.mirror_pending.insert(s.resource, s.gen);
                    }
                    ResourceId::Drape(..) => self.net.start(s, self.now),
                }
            }
        }

        // The frame: terrain's draw set, each item wearing its drape.
        let mut frame: Vec<FrameItem> = Vec::new();
        let mut from_search = 0u64;
        for i in &dt.draw.items {
            let drape = match &di {
                None => i.drape.map(|d| (d.resource, d.level)),
                Some(di) => match di.draw.get(i.mesh).and_then(|x| x.drape) {
                    Some(d) if self.dgpu.drawable.contains_key(&d.resource) => {
                        Some((d.resource, d.level))
                    }
                    _ => {
                        if i.role == Role::Exact {
                            from_search += 1;
                        }
                        self.search_drape(i.mesh.tile())
                    }
                },
            };
            frame.push((i.mesh, i.role, i.serves.clone(), drape));
        }
        let floor_in = self
            .gpu
            .drawable
            .contains_key(&ResourceId::Mesh(tile(0, 0, 0)));
        if floor_in && self.floor_at.is_none() {
            self.floor_at = Some(self.now);
        }
        let bare = frame.iter().filter(|f| f.3.is_none()).count() as u64;
        // The imagery engine's own verdict, for convergence.
        let underdraped = match &di {
            None => u64::from(dt.stats.underdraped),
            Some(d) => u64::from(d.stats.underdraped) + from_search,
        };
        if floor_in {
            self.m.lost += u64::from(dt.stats.lost);
            self.m.bare += bare;
        }
        // Quality, measured outside the budgets: each exact tile's
        // unrelaxed need.
        let wants: BTreeMap<TileId, &tuile_core::budget::Need> =
            need.drawn.iter().map(|(t, n)| (*t, n)).collect();
        let cap = self.profile.budget.imagery_cap();
        let floor = self.profile.budget.pinned_level.unwrap_or(0);
        for (mesh, role, _, drape) in &frame {
            if *role != Role::Exact {
                continue;
            }
            let Some(img) = wants.get(&mesh.tile()).and_then(|n| n.imagery) else {
                continue;
            };
            self.m.exact += 1;
            let clamp = LevelClamp {
                floor,
                terrain_level: mesh.tile().terrain_coord().0,
                over_terrain_cap: cap,
                source_max: img.max_level,
            };
            let n = needed_level(None, img.footprint, clamp);
            if drape.is_none_or(|d| d.1 + 1 < n) {
                self.m.blurry += 1;
            }
        }
        let worn: BTreeSet<ResourceId> = frame.iter().filter_map(|f| f.3.map(|d| d.0)).collect();

        // GPUs. Terrain (or the shared one) through SimGpu.
        let acks = self
            .gpu
            .apply(&dt)
            .unwrap_or_else(|e| panic!("{} frame {}: {e}", self.arch.name(), self.frame));
        for (r, g, b) in acks {
            self.terrain.on_drawable(r, g, b);
        }
        // A worn drape evicted is a hole next frame (I11).
        for e in &dt.evict {
            if worn.contains(e) {
                self.m.worn_evicted += 1;
            }
        }
        if let (Some(di), Some(img)) = (&di, &mut self.imagery) {
            for (r, g) in &di.cancel {
                if self.dgpu.pending.get(r) == Some(g) {
                    self.dgpu.pending.remove(r);
                }
            }
            for (r, g) in &di.deliver {
                match r {
                    ResourceId::Mesh(_) => {
                        self.mirror_delivered.insert(*r, *g);
                    }
                    ResourceId::Drape(..) => {
                        self.dgpu.pending.insert(*r, *g);
                    }
                }
            }
            for r in &di.evict {
                if let ResourceId::Drape(..) = r {
                    if worn.contains(r) {
                        self.m.worn_evicted += 1;
                    }
                    if let Some(b) = self.dgpu.drawable.remove(r) {
                        self.dgpu.bytes = self.dgpu.bytes.minus(b);
                    }
                }
            }
            for r in &di.upload_plan.items {
                match r {
                    ResourceId::Mesh(_) => {
                        if let Some(g) = self.mirror_delivered.remove(r) {
                            img.on_drawable(*r, g, Bytes(1));
                        }
                    }
                    ResourceId::Drape(..) => {
                        let Some(g) = self.dgpu.pending.remove(r) else {
                            continue;
                        };
                        let b = cost(*r).gpu;
                        self.dgpu.drawable.insert(*r, b);
                        self.dgpu.bytes = self.dgpu.bytes.plus(b);
                        img.on_drawable(*r, g, b);
                    }
                }
            }
        }

        // Bytes.
        let pool_bytes = self.gpu.bytes[Class::TerrainMesh]
            .plus(self.gpu.bytes[Class::Drape])
            .plus(self.dgpu.bytes);
        if pool_bytes > self.pool_target {
            self.m.over_target_steps += 1;
            let mib = pool_bytes.minus(self.pool_target).0 as f64 / 1_048_576.0;
            self.m.over_target_max_mib = self.m.over_target_max_mib.max(mib);
        }
        let mut over_hard = false;
        for c in [Class::TerrainMesh, Class::Drape] {
            over_hard |= dt.stats.gpu[c] > self.terrain.config().hard_limit[c];
            if let (Some(di), Some(img)) = (&di, &self.imagery) {
                over_hard |= c == Class::Drape && di.stats.gpu[c] > img.config().hard_limit[c];
            }
        }
        self.m.over_hard_steps += u64::from(over_hard);
        self.m.thrash +=
            u64::from(dt.stats.thrash) + di.as_ref().map_or(0, |d| u64::from(d.stats.thrash));

        // Redrapes (Q1 evidence) and flapping (I4) on the frame as drawn.
        for (mesh, role, _, drape) in &frame {
            if *role != Role::Exact {
                continue;
            }
            let t = mesh.tile();
            if let Some(d) = drape {
                if let Some(prev) = self.worn_last.insert(t, d.0) {
                    if prev != d.0 && prev.tile() == t && d.0.tile() == t {
                        self.m.redrapes += 1;
                        self.m.redrape_mesh_mib +=
                            wire_bytes(*mesh, cost(*mesh).gpu.0) as f64 / 1_048_576.0;
                    }
                }
            }
        }
        let exact_now: BTreeSet<TileId> = frame
            .iter()
            .filter(|f| f.1 == Role::Exact)
            .map(|f| f.0.tile())
            .collect();
        self.worn_last.retain(|t, _| exact_now.contains(t));
        let still = self.last_cam == Some(cam);
        if still {
            for (mesh, _, serves, drape) in &frame {
                let ml = mesh.tile().terrain_coord().0;
                let dl = drape.map(|d| d.1).unwrap_or(0);
                for t in serves {
                    let e = self.levels.entry(*t).or_insert((ml, dl, Vec::new()));
                    if (e.0, e.1) != (ml, dl) {
                        e.0 = ml;
                        e.1 = dl;
                        e.2.push(self.now);
                    }
                }
            }
        } else {
            self.levels.clear();
        }

        // Convergence, the same definition for every architecture.
        let converged = dt.stats.converged
            && need.blockers.is_empty()
            && dt.stats.fallback == 0
            && dt.stats.held == 0
            && dt.stats.lost == 0
            && bare == 0
            && underdraped == 0;
        self.converged_at.push((self.now, converged));

        self.sse_scale = f64::from(dt.sse_scale_milli) / 1000.0;
        self.drawn_last = frame
            .iter()
            .filter(|f| f.1 != Role::Fallback)
            .map(|f| f.0.tile())
            .collect();
        self.last_cam = Some(cam);
        self.now = self.now.plus(self.profile.dt);
        self.frame += 1;
    }

    /// The finest drape the imagery GPU holds on `t` or its nearest
    /// ancestor that holds any.
    fn search_drape(&self, t: TileId) -> Option<(ResourceId, u32)> {
        let mut cur = Some(t);
        while let Some(x) = cur {
            let lo = ResourceId::Drape(x, 0);
            let hi = ResourceId::Drape(x, u32::MAX);
            if let Some((r, _)) = self.dgpu.drawable.range(lo..=hi).next_back() {
                if let ResourceId::Drape(_, l) = r {
                    return Some((*r, *l));
                }
            }
            cur = parent(x);
        }
        None
    }

    fn finish(mut self, stop: Option<Micros>) -> Metrics {
        let from = stop.map(|s| s.max(self.floor_at.unwrap_or(Micros::NEVER)));
        self.m.converged_s = from.and_then(|f| {
            if !self.converged_at.last().is_some_and(|l| l.1) {
                return None;
            }
            let last_bad = self
                .converged_at
                .iter()
                .rev()
                .find(|(_, c)| !*c)
                .map(|(t, _)| t.plus(self.profile.dt));
            Some(last_bad.map_or(0.0, |t| t.since(f).0 as f64 / 1e6))
        });
        self.m.flapping = self
            .levels
            .values()
            .map(|(_, _, ch)| {
                ch.iter()
                    .enumerate()
                    .map(|(i, t)| {
                        ch[i..]
                            .iter()
                            .take_while(|u| u.since(*t) < Micros::from_secs(10))
                            .count()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        let n = self.decide_us.len().max(1);
        self.m.decide_mean_us = self.decide_us.iter().sum::<f64>() / n as f64;
        let mut s = self.decide_us.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        self.m.decide_p99_us = s.get(n * 99 / 100).copied().unwrap_or(0.0);
        self.m.net_mib = self.net.network_bytes as f64 / 1_048_576.0;
        self.m
    }
}

/// One scenario: builds a run, plays it, returns the metrics.
type Scenario = fn(Arch, u64) -> Metrics;

fn quiet(mut p: Profile) -> Profile {
    for s in [&mut p.net.meshes, &mut p.net.imagery] {
        s.stall_permille = 0;
    }
    p
}

fn still(a: Arch, seed: u64) -> Metrics {
    let mut r = Run::new(a, quiet(Profile::desktop()), seed);
    r.run(5.0, &approach);
    let stop = r.now;
    r.run(10.0, &|_| home());
    r.finish(Some(stop))
}

fn half_turn(a: Arch, seed: u64) -> Metrics {
    let mut r = Run::new(a, quiet(Profile::desktop()), seed);
    r.run(5.0, &approach);
    r.run(1.0, &|_| home());
    r.run(1.0, &turn);
    let stop = r.now;
    r.run(6.0, &|_| Camera {
        heading: std::f64::consts::PI,
        ..home()
    });
    r.finish(Some(stop))
}

fn zoom_out_back(a: Arch, seed: u64) -> Metrics {
    let mut r = Run::new(a, quiet(Profile::desktop()), seed);
    r.run(17.0, &zoom);
    let stop = r.now;
    r.run(6.0, &zoom);
    r.finish(Some(stop))
}

fn back_forth(a: Arch, seed: u64) -> Metrics {
    let mut r = Run::new(a, quiet(Profile::desktop()), seed);
    r.run(48.0, &back_and_forth);
    let stop = r.now;
    let end = back_and_forth(48.0);
    r.run(6.0, &|_| end);
    r.finish(Some(stop))
}

fn memory_warning(a: Arch, seed: u64) -> Metrics {
    let mut r = Run::new(a, quiet(Profile::handheld()), seed);
    r.run(8.0, &zoom);
    r.pressure(Pressure::Warning);
    r.run(9.0, &|t| zoom(t));
    let stop = r.now;
    r.run(8.0, &|t| zoom(t));
    r.finish(Some(stop))
}

fn low_clamp(a: Arch, seed: u64) -> Metrics {
    let mut r = Run::new(a, quiet(Profile::handheld_low()), seed);
    r.run(5.0, &approach);
    let stop = r.now;
    r.run(10.0, &|_| home());
    r.finish(Some(stop))
}

/// The comparison table, printed. Evidence for
/// `docs/explorations/graphics-budget-architecture.md`; it asserts only
/// that every run ran.
#[test]
#[ignore = "Phase 1a experiment: cargo test --release -p tuile-core --test budget_sim -- --ignored architecture --nocapture"]
fn architecture_shared_arbiter_against_split_budgets() {
    let archs = [
        Arch::Shared,
        Arch::SharedRatio(60),
        Arch::SharedRatio(40),
        Arch::Split(44),
        Arch::Split(60),
        Arch::Split(40),
        Arch::SplitDrawn(44),
    ];
    let scenarios: [(&str, Scenario); 6] = [
        ("still", still),
        ("turn", half_turn),
        ("zoom-out-back", zoom_out_back),
        ("back-and-forth", back_forth),
        ("memory-warning", memory_warning),
        ("low-clamp", low_clamp),
    ];
    let seeds = [101u64, 202, 303];
    println!(
        "| scenario | arch | lost | bare | worn evicted | blurry % | converged s (max of seeds) | over-target steps | over-target max MiB | over-hard steps | flapping | decide mean µs | decide p99 µs | net MiB | thrash | redrapes | redrape mesh MiB |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for (name, sc) in scenarios {
        for a in archs {
            let ms: Vec<Metrics> = seeds.iter().map(|s| sc(a, *s)).collect();
            let sum = |f: &dyn Fn(&Metrics) -> u64| ms.iter().map(f).sum::<u64>();
            let mean =
                |f: &dyn Fn(&Metrics) -> f64| ms.iter().map(f).sum::<f64>() / ms.len() as f64;
            let conv = if ms.iter().all(|m| m.converged_s.is_some()) {
                format!(
                    "{:.2}",
                    ms.iter()
                        .filter_map(|m| m.converged_s)
                        .fold(0.0f64, f64::max)
                )
            } else {
                format!(
                    "never ({}/{})",
                    ms.iter().filter(|m| m.converged_s.is_none()).count(),
                    ms.len()
                )
            };
            println!(
                "| {name} | {} | {} | {} | {} | {:.1} | {conv} | {} | {:.1} | {} | {} | {:.0} | {:.0} | {:.1} | {} | {} | {:.1} |",
                a.name(),
                sum(&|m| m.lost),
                sum(&|m| m.bare),
                sum(&|m| m.worn_evicted),
                100.0 * sum(&|m| m.blurry) as f64 / sum(&|m| m.exact).max(1) as f64,
                sum(&|m| m.over_target_steps),
                ms.iter().map(|m| m.over_target_max_mib).fold(0.0, f64::max),
                sum(&|m| m.over_hard_steps),
                ms.iter().map(|m| m.flapping).max().unwrap_or(0),
                mean(&|m| m.decide_mean_us),
                mean(&|m| m.decide_p99_us),
                mean(&|m| m.net_mib),
                sum(&|m| m.thrash),
                sum(&|m| m.redrapes),
                mean(&|m| m.redrape_mesh_mib),
            );
        }
    }
}
