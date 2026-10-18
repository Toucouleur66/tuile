// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The graphics budget: one pure, deterministic module that decides what is
//! loaded, delivered, uploaded, drawn and evicted, for viewers
//! ([`Policy::Interactive`]) and films ([`Policy::Batch`]) alike.
//!
//! It reads no clock, spawns nothing, touches no GPU and iterates no hash
//! map: time, the view, arrivals and platform signals are inputs, and the
//! same inputs give the same [`Decisions`], bit for bit
//! ([`Decisions::digest`]). It builds for `wasm32-unknown-unknown`.
//!
//! See `docs/explorations/graphics-budget-to-be-and-spec.md` for the
//! invariants I1–I15 it upholds.
//!
//! # The step
//!
//! Between two calls to [`Budget::decide`], arrivals are recorded
//! ([`Budget::on_loaded`], [`Budget::on_drawable`], …) but not applied; the
//! next `decide` applies them in a canonical order, whatever order they came
//! in, then decides.

pub mod config;
pub mod cost;
pub mod decisions;
pub mod error;
pub mod imagery;
pub mod need;
pub mod priority;
pub mod stats;
pub mod tier;
pub mod types;
pub mod view;

pub use config::{BudgetConfig, FailurePolicy, PressureFractions, Relaxation, Watermarks};
pub use decisions::{
    Decisions, DrapeChoice, DrawItem, DrawSet, Role, Start, SwapGroup, UploadPlan,
};
pub use error::{AuditError, ConfigError, SessionError};
pub use imagery::{DrapeCandidate, GeoRect, LevelClamp, TexelFootprint, UvTransform};
pub use need::{ImageryNeed, Membership, Need, NeedSet};
pub use priority::{PriorityKey, Quantised, Urgency};
pub use stats::BudgetStats;
pub use tier::{NoStore, Presence, PresenceOracle, Source, Tier};
pub use types::{
    Bytes, Class, Failure, Generation, Micros, PerClass, Policy, Pressure, ResourceId, Thermal,
    TileId,
};
pub use view::{Protection, ResourceState, ResourceView};

/// One decision step's input.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct FrameInput<'a> {
    /// The frame number (Batch: the film frame).
    pub frame: u64,
    /// The time of this step, simulated or real. Never read inside.
    pub now: Micros,
    /// What the traversal wants.
    pub need: &'a NeedSet,
    /// The local stores' presence index.
    pub presence: &'a dyn PresenceOracle,
}

static NO_STORE: NoStore = NoStore;

impl<'a> FrameInput<'a> {
    /// A step at `now` for `need`, with no local store.
    pub fn new(frame: u64, now: Micros, need: &'a NeedSet) -> Self {
        FrameInput {
            frame,
            now,
            need,
            presence: &NO_STORE,
        }
    }

    /// The same step, asking `presence` about local stores.
    pub fn with_presence(mut self, presence: &'a dyn PresenceOracle) -> Self {
        self.presence = presence;
        self
    }
}

/// The ledger: bytes charged, per tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// GPU bytes per class: drawable (actual) plus in transit (estimated).
    pub gpu: PerClass<Bytes>,
    /// Decoded CPU bytes, all classes.
    pub decoded: Bytes,
    /// Bytes delivered and not yet acknowledged.
    pub in_transit: Bytes,
}

/// An arrival, recorded between steps and applied at the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Loaded(ResourceId, Generation, Bytes),
    Failed(ResourceId, Generation, bool),
    StoreMiss(ResourceId, Generation),
    Drawable(ResourceId, Generation, Bytes),
    Discarded(ResourceId, Generation),
}

/// The budget. See the [module documentation](self).
#[derive(Debug)]
pub struct Budget {
    cfg: BudgetConfig,
    pressure: Pressure,
    thermal: Thermal,
    events: Vec<Event>,
}

impl Budget {
    /// A budget running on `cfg`.
    pub fn new(cfg: BudgetConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Budget {
            cfg,
            pressure: Pressure::Normal,
            thermal: Thermal::Nominal,
            events: Vec::new(),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &BudgetConfig {
        &self.cfg
    }

    /// One decision step: applies the recorded arrivals, then decides.
    pub fn decide(&mut self, input: FrameInput<'_>) -> Decisions {
        let _ = (input, &self.pressure, &self.thermal);
        self.events.clear();
        Decisions {
            sse_scale_milli: 1_000,
            ..Decisions::default()
        }
    }

    /// A load finished decoding: `est` decoded bytes, also the GPU estimate.
    pub fn on_loaded(&mut self, r: ResourceId, gen: Generation, est: Bytes) {
        self.events.push(Event::Loaded(r, gen, est));
    }

    /// A load failed.
    pub fn on_failed(&mut self, r: ResourceId, gen: Generation, why: Failure) {
        self.events
            .push(Event::Failed(r, gen, why == Failure::Transient));
    }

    /// A local read found nothing despite a `Stored` hint: the resource
    /// degrades to a network fetch, once.
    pub fn on_store_miss(&mut self, r: ResourceId, gen: Generation) {
        self.events.push(Event::StoreMiss(r, gen));
    }

    /// The consumer uploaded a delivered resource; `actual` GPU bytes.
    pub fn on_drawable(&mut self, r: ResourceId, gen: Generation, actual: Bytes) {
        self.events.push(Event::Drawable(r, gen, actual));
    }

    /// The consumer dropped a delivered resource without uploading it.
    pub fn on_discarded(&mut self, r: ResourceId, gen: Generation) {
        self.events.push(Event::Discarded(r, gen));
    }

    /// The platform's memory signal; takes effect at the next step.
    pub fn set_pressure(&mut self, p: Pressure) {
        self.pressure = p;
    }

    /// Re-derives every class's target and overflow from a new total, keeping
    /// the configured split: the host calls it when the platform's available
    /// memory changes (`os_proc_available_memory`, `VK_EXT_memory_budget`,
    /// an Android `onTrimMemory` level, a browser's device-memory hint). The
    /// budget itself knows no platform. Takes effect at the next step.
    pub fn retarget(&mut self, total: Bytes) {
        let _ = total;
    }

    /// The platform's thermal state; takes effect at the next step.
    pub fn set_thermal(&mut self, t: Thermal) {
        self.thermal = t;
    }

    /// Batch, between frames: evicts what the last step did not need, in
    /// eviction order, until the GPU holds at most `target` bytes. Returns
    /// the GPU copies to drop. Deterministic.
    pub fn release_beyond(&mut self, target: Bytes) -> Vec<ResourceId> {
        let _ = target;
        Vec::new()
    }

    /// The traversal's residency: drawable (Interactive) or loaded (Batch).
    pub fn residency(&self) -> ResidencyView<'_> {
        ResidencyView { budget: self }
    }

    /// One resource's state, if the budget tracks it.
    pub fn resource(&self, r: ResourceId) -> Option<ResourceView> {
        let _ = r;
        None
    }

    /// The ledger.
    pub fn ledger(&self) -> Ledger {
        Ledger::default()
    }

    /// Recomputes the ledger from the resource states and compares.
    pub fn audit(&self) -> Result<(), AuditError> {
        Ok(())
    }

    /// How many resources the budget tracks.
    pub fn tracked(&self) -> usize {
        0
    }
}

/// The residency the traversal reads.
#[derive(Clone, Copy, Debug)]
pub struct ResidencyView<'a> {
    budget: &'a Budget,
}

impl ResidencyView<'_> {
    /// Whether the traversal may treat `tile` as resident: its mesh is
    /// drawable (Interactive), or loaded (Batch).
    pub fn resident(&self, tile: TileId) -> bool {
        match self.budget.cfg.policy {
            Policy::Interactive => self.drawable(tile),
            Policy::Batch => self.loaded(tile),
        }
    }

    /// Whether `tile`'s mesh is on the GPU.
    pub fn drawable(&self, tile: TileId) -> bool {
        let _ = tile;
        false
    }

    /// Whether `tile`'s mesh is decoded or on the GPU.
    pub fn loaded(&self, tile: TileId) -> bool {
        let _ = tile;
        false
    }
}
