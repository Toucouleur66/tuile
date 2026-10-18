// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimServer`: drives the budget exactly as the runtime will — traverse →
//! need → decide → load → deliver → upload → ack — on a stepped clock, and
//! checks the invariants at every step.

use std::collections::{BTreeMap, BTreeSet};

use tuile_core::budget::{
    Budget, BudgetConfig, BudgetStats, Bytes, Class, Decisions, FrameInput, Generation, Micros,
    NeedSet, Policy, Pressure, ResourceId, Role, Source, Thermal, TileId,
};

use super::gpu::SimGpu;
use super::net::{Arrival, NetProfile, SimNet};
use super::world::{children, parent, traverse, Camera, Residency};

/// A device profile: the budget, the network, the frame rate.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Its name, for messages.
    pub name: &'static str,
    /// The budget.
    pub budget: BudgetConfig,
    /// The network.
    pub net: NetProfile,
    /// The step length.
    pub dt: Micros,
    /// The convergence bound of I3 after the camera stops.
    pub converge_within: Micros,
}

impl Profile {
    /// A desktop: 2 GiB, a good network, 60 fps, 2 s to converge.
    pub fn desktop() -> Self {
        Profile {
            name: "desktop",
            budget: BudgetConfig::interactive(Bytes::gib(2)),
            net: NetProfile::desktop(),
            dt: Micros(16_667),
            converge_within: Micros::from_secs(2),
        }
    }

    /// A phone: 512 MiB, cellular, 60 fps, 4 s to converge.
    pub fn handheld() -> Self {
        let mut b = BudgetConfig::interactive(Bytes::mib(512));
        b.max_in_flight = tuile_core::budget::PerClass([8, 4, 8, 2, 2]);
        b.upload_bytes_per_frame = Bytes::mib(4);
        b.delivery_credit = Bytes::mib(16);
        Profile {
            name: "handheld",
            budget: b,
            net: NetProfile::handheld(),
            dt: Micros(16_667),
            converge_within: Micros::from_secs(4),
        }
    }

    /// The handheld profile at its low clamp: 160 MiB (an older, small
    /// device under `os_proc_available_memory`).
    pub fn handheld_low() -> Self {
        let mut p = Profile::handheld();
        p.name = "handheld-low";
        let quotas = p.budget.max_in_flight;
        let upload = p.budget.upload_bytes_per_frame;
        p.budget = BudgetConfig::interactive(Bytes::mib(160));
        p.budget.max_in_flight = quotas;
        p.budget.upload_bytes_per_frame = upload;
        p.budget.delivery_credit = Bytes::mib(8);
        p
    }

    /// A film: Batch, a clean network.
    pub fn film() -> Self {
        Profile {
            name: "film",
            budget: BudgetConfig::batch(),
            net: NetProfile::desktop().clean(),
            dt: Micros(16_667),
            converge_within: Micros::NEVER,
        }
    }
}

/// What one step looked like.
#[derive(Clone, Debug)]
pub struct StepLog {
    /// The step's time.
    pub now: Micros,
    /// The decision digest.
    pub digest: u64,
    /// The budget's statistics.
    pub stats: BudgetStats,
    /// The view is converged in the sense of I3.
    pub converged: bool,
}

struct View<'a>(&'a Budget);
impl Residency for View<'_> {
    fn resident(&self, t: TileId) -> bool {
        self.0.residency().resident(t)
    }
}

/// The simulation.
pub struct Sim {
    /// The profile.
    pub profile: Profile,
    /// The budget under test.
    pub budget: Budget,
    /// The network.
    pub net: SimNet,
    /// The GPU.
    pub gpu: SimGpu,
    /// The clock.
    pub now: Micros,
    /// The frame.
    pub frame: u64,
    /// Every step's log.
    pub log: Vec<StepLog>,
    /// Decoded resources the server holds.
    decoded: BTreeSet<ResourceId>,
    /// The latest fresh generation loaded for each resource.
    fresh: BTreeMap<ResourceId, Generation>,
    drawn_last: BTreeSet<TileId>,
    sse_scale: f64,
    last_cam: Option<Camera>,
    floor_in: bool,
    /// Per tile: (mesh level drawn, drape level worn, last change time),
    /// tracked while the camera is still (I4).
    levels: BTreeMap<TileId, (u32, u32, Vec<Micros>)>,
}

impl Sim {
    /// A simulation on `profile`, seeded.
    pub fn new(profile: Profile, seed: u64) -> Self {
        Sim {
            budget: Budget::new(profile.budget.clone()).expect("a valid profile"),
            net: SimNet::new(profile.net, seed),
            gpu: SimGpu::default(),
            now: Micros::ZERO,
            frame: 0,
            log: Vec::new(),
            decoded: BTreeSet::new(),
            fresh: BTreeMap::new(),
            drawn_last: BTreeSet::new(),
            sse_scale: 1.0,
            last_cam: None,
            floor_in: false,
            levels: BTreeMap::new(),
            profile,
        }
    }

    fn pinned(&self) -> u32 {
        self.profile.budget.pinned_level.unwrap_or(0)
    }

    /// One step with the camera at `cam`. Panics, with the scenario context,
    /// on any invariant violation.
    pub fn step(&mut self, cam: Camera) -> (NeedSet, Decisions) {
        for a in self.net.poll(self.now) {
            match a {
                Arrival::Loaded(r, g, b) => {
                    self.fresh.insert(r, g);
                    self.decoded.insert(r);
                    self.budget.on_loaded(r, g, b);
                }
                Arrival::Failed(r, g) => {
                    self.budget
                        .on_failed(r, g, tuile_core::budget::Failure::Transient)
                }
                Arrival::StoreMiss(r, g) => self.budget.on_store_miss(r, g),
            }
        }
        let need = traverse(
            &cam,
            &View(&self.budget),
            self.sse_scale,
            &self.drawn_last,
            self.pinned(),
        );
        let input = FrameInput::new(self.frame, self.now, &need).with_presence(&self.net.store);
        let d = self.budget.decide(input);
        self.check(&need, &d);
        for (r, g) in &d.cancel {
            self.net.cancel(*r, *g);
        }
        for s in &d.start {
            self.net.start(s, self.now);
        }
        for r in &d.release {
            self.decoded.remove(r);
        }
        let acks = self
            .gpu
            .apply(&d)
            .unwrap_or_else(|e| panic!("{} frame {}: {e}", self.profile.name, self.frame));
        for (r, g, b) in acks {
            self.budget.on_drawable(r, g, b);
        }
        self.sse_scale = f64::from(d.sse_scale_milli) / 1000.0;
        self.drawn_last = d
            .draw
            .items
            .iter()
            .filter(|i| i.role != Role::Fallback)
            .map(|i| i.mesh.tile())
            .collect();
        let still = self.last_cam == Some(cam);
        self.track_levels(still, &d);
        let converged = d.stats.converged
            && need.blockers.is_empty()
            && d.stats.fallback == 0
            && d.stats.held == 0
            && d.stats.lost == 0
            && d.stats.underdraped == 0;
        self.log.push(StepLog {
            now: self.now,
            digest: d.digest(),
            stats: d.stats.clone(),
            converged,
        });
        self.last_cam = Some(cam);
        self.floor_in = self
            .gpu
            .drawable
            .contains_key(&ResourceId::Mesh(super::world::tile(0, 0, 0)));
        self.now = self.now.plus(self.profile.dt);
        self.frame += 1;
        (need, d)
    }

    fn fail(&self, what: &str) -> ! {
        panic!(
            "{} frame {} t={:?}: {what}",
            self.profile.name, self.frame, self.now
        )
    }

    /// The per-step invariants.
    fn check(&self, need: &NeedSet, d: &Decisions) {
        let s = &d.stats;
        let cfg = &self.profile.budget;
        // I1: once the floor is in, nothing is lost and the ground is covered.
        if self.floor_in {
            if s.lost != 0 {
                self.fail(&format!("I1: {} wanted tiles lost", s.lost));
            }
            for (t, _) in &need.drawn {
                if !covered(d, *t) {
                    self.fail(&format!("I1: {t:?} uncovered"));
                }
            }
        }
        // I2: what is not protected fits in the target.
        for c in Class::ALL {
            if s.gpu[c].minus(s.protected[c]) > s.target[c] {
                self.fail(&format!("I2: {c:?} unprotected bytes over target"));
            }
        }
        // I5: only the fresh generation is delivered.
        for (r, g) in &d.deliver {
            if self.fresh.get(r) != Some(g) {
                self.fail(&format!("I5: {r:?} delivered at stale {g:?}"));
            }
        }
        // I7: bounded state.
        if s.heap_len > cfg.request_heap {
            self.fail("I7: request heap over its cap");
        }
        if s.duplicates_in_flight != 0 {
            self.fail("I7: duplicates in flight");
        }
        let bound = 4
            * (self.gpu.drawable.len()
                + self.gpu.pending.len()
                + need.drawn.len()
                + need.blockers.len()
                + need.ancestors.len()
                + need.ring.len())
            + 64;
        if s.tracked as usize > bound {
            self.fail(&format!(
                "I7: tracking {} resources, bound {bound}",
                s.tracked
            ));
        }
        // I11: nothing protected is evicted.
        for e in &d.evict {
            let drawn = d
                .draw
                .items
                .iter()
                .any(|i| i.mesh == *e || i.drape.map(|x| x.resource) == Some(*e));
            if drawn
                || self.gpu.pending.contains_key(e)
                || e.tile().terrain_coord().0 <= self.pinned() && cfg.pinned_level.is_some()
            {
                self.fail(&format!("I11: protected {e:?} evicted"));
            }
        }
        // I14: nothing held is fetched again.
        for st in &d.start {
            if st.source == Source::Network {
                let held = self.gpu.drawable.contains_key(&st.resource)
                    || self.decoded.contains(&st.resource)
                    || self.net.store.held.contains(&st.resource);
                if held {
                    self.fail(&format!("I14: {:?} fetched while held", st.resource));
                }
            }
            if self.net.in_flight(st.resource) {
                self.fail(&format!("I14: {:?} started twice", st.resource));
            }
        }
        // I13: a ready batch frame is exact.
        if cfg.policy == Policy::Batch {
            if !d.evict.is_empty() {
                self.fail("I13: Batch evicted inside a frame");
            }
            if d.frame_ready {
                for (t, _) in &need.drawn {
                    let exact =
                        d.draw.get(ResourceId::Mesh(*t)).map(|i| i.role) == Some(Role::Exact);
                    if !exact {
                        self.fail(&format!("I13: ready frame draws {t:?} inexactly"));
                    }
                }
                if !need.blockers.is_empty() {
                    self.fail("I13: ready frame with blockers");
                }
            }
        }
    }

    fn track_levels(&mut self, still: bool, d: &Decisions) {
        if !still {
            self.levels.clear();
            return;
        }
        for i in &d.draw.items {
            let mesh = i.mesh.tile().terrain_coord().0;
            let drape = i.drape.map(|x| x.level).unwrap_or(0);
            for t in &i.serves {
                let e = self.levels.entry(*t).or_insert((mesh, drape, Vec::new()));
                if (e.0, e.1) != (mesh, drape) {
                    e.0 = mesh;
                    e.1 = drape;
                    e.2.push(self.now);
                }
            }
        }
    }

    /// I4: the most level changes of any tile within any 10 s window while
    /// still.
    pub fn worst_flapping(&self) -> usize {
        self.levels
            .values()
            .map(|(_, _, changes)| {
                changes
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        changes[i..]
                            .iter()
                            .take_while(|u| u.since(*t) < Micros::from_secs(10))
                            .count()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0)
    }

    /// Runs `script` from `t0` for `secs` seconds of simulated time.
    pub fn run(&mut self, secs: f64, script: &dyn Fn(f64) -> Camera) {
        let end = self.now.plus(Micros((secs * 1e6) as u64));
        while self.now < end {
            let t = self.now.0 as f64 / 1e6;
            self.step(script(t));
        }
    }

    /// Applies a memory signal.
    pub fn pressure(&mut self, p: Pressure) {
        self.budget.set_pressure(p);
    }

    /// Applies a thermal state.
    pub fn thermal(&mut self, t: Thermal) {
        self.budget.set_thermal(t);
    }

    /// Seconds from `since` to the first converged step, if any.
    pub fn converged_after(&self, since: Micros) -> Option<Micros> {
        self.log
            .iter()
            .find(|l| l.now >= since && l.converged)
            .map(|l| l.now.since(since))
    }

    /// The digest of the whole run.
    pub fn run_digest(&self) -> u64 {
        self.log.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, l| {
            (h ^ l.digest).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// A Batch frame: steps with the camera at `cam` until the frame is ready;
    /// returns the draw set's (mesh, drape) pairs and the steps it took.
    pub fn film_frame(
        &mut self,
        cam: Camera,
        max_steps: usize,
    ) -> (Vec<(ResourceId, Option<ResourceId>)>, usize) {
        for k in 0..max_steps {
            let (_, d) = self.step(cam);
            if let Some(e) = &d.fatal {
                self.fail(&format!("I13: the film ended: {e}"));
            }
            if d.frame_ready {
                let drawn = d
                    .draw
                    .items
                    .iter()
                    .map(|i| (i.mesh, i.drape.map(|x| x.resource)))
                    .collect();
                let release = self.budget.release_beyond(Bytes::mib(256));
                for r in &release {
                    if let Some(b) = self.gpu.drawable.remove(r) {
                        let c = match r {
                            ResourceId::Mesh(_) => Class::TerrainMesh,
                            ResourceId::Drape(..) => Class::Drape,
                        };
                        self.gpu.bytes[c] = self.gpu.bytes[c].minus(b);
                    }
                }
                return (drawn, k + 1);
            }
        }
        self.fail("I13: the frame never became ready")
    }
}

/// Covered: the tile or an ancestor drawn, or all its children covered
/// (three levels down at most).
fn covered(d: &Decisions, t: TileId) -> bool {
    let mut up = Some(t);
    while let Some(x) = up {
        if d.draw.draws(x) {
            return true;
        }
        up = parent(x);
    }
    down(d, t, 3)
}

fn down(d: &Decisions, t: TileId, depth: u32) -> bool {
    depth > 0
        && children(t)
            .iter()
            .all(|&k| d.draw.draws(k) || down(d, k, depth - 1))
}
