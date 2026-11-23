// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimGpu`: the consumer, the executor of spec §15.4. It receives
//! deliveries, uploads the plan in order within its frame deadline (I18:
//! what does not fit waits for a later frame, and reaches the budget only
//! as a later acknowledgement), draws the step's draw set once per eye
//! (I17), drops evictions after applying the draw set, and keeps byte
//! counters per class. It refuses what a real GPU could not do: upload what
//! it was not given, evict what it does not hold, draw what is not
//! uploaded, upload what it already holds.

use std::collections::BTreeMap;

use tuile_budget::{Bytes, Class, Decisions, FrameDeadline, Generation, PerClass, ResourceId};

use super::world::{cost_for, SimPlan};

/// The consumer's GPU.
pub struct SimGpu {
    /// Uploaded resources and their bytes.
    pub drawable: BTreeMap<ResourceId, Bytes>,
    /// Delivered, not yet uploaded.
    pub pending: BTreeMap<ResourceId, Generation>,
    /// Bytes per class.
    pub bytes: PerClass<Bytes>,
    /// Uploads, total.
    pub uploads: u64,
    /// The upload cap per render frame (I18).
    pub deadline: FrameDeadline,
    /// The last render frame's uploads: bytes and items.
    pub frame_bytes: Bytes,
    /// Items uploaded the last render frame.
    pub frame_items: usize,
    /// Plan items the deadline deferred, total.
    pub deferred: u64,
    /// For each eye, the frame whose draw set it drew last.
    pub eye_drew: Vec<u64>,
    /// The executor runs behind the budget: a plan may list what it has
    /// uploaded already, its acknowledgement still on its way.
    pub lagging: bool,
    /// How it holds imagery (CB3).
    pub plan: SimPlan,
    /// Of its GPU encodes — compressions in place since CB9 — how many in a
    /// thousand are refused and stay RGBA8: a resource's fate is a hash of
    /// it and its generation, so a run is reproducible.
    pub fallback_permille: u32,
    /// Its encodes that fell back, total.
    pub fallbacks: u64,
    /// Compressions in place begun (CB9): the copy compressed, and what it
    /// held before and will hold after. Finished at the next frame, the
    /// compressed copy made in this one beside the RGBA8 one.
    pub compressing: Vec<(ResourceId, Generation, Bytes, Bytes)>,
    /// The compressions finished at the last frame, to acknowledge: the
    /// copy, and what it was charged and holds now.
    pub compressed: Vec<(ResourceId, Generation, Bytes, Bytes)>,
}

impl SimGpu {
    /// An executor capped by `deadline`, drawing for `eyes` eyes.
    pub fn new(deadline: FrameDeadline, eyes: usize) -> Self {
        SimGpu {
            drawable: BTreeMap::new(),
            pending: BTreeMap::new(),
            bytes: PerClass::splat(Bytes::ZERO),
            uploads: 0,
            deadline,
            frame_bytes: Bytes::ZERO,
            frame_items: 0,
            deferred: 0,
            eye_drew: vec![0; eyes],
            lagging: false,
            plan: SimPlan::Rgba8,
            fallback_permille: 0,
            fallbacks: 0,
            compressing: Vec::new(),
            compressed: Vec::new(),
        }
    }
}

impl SimGpu {
    /// Whether the encode of `r` at `g` falls back: seeded by the pair.
    fn falls_back(&self, r: ResourceId, g: Generation) -> bool {
        if self.fallback_permille == 0 {
            return false;
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in format!("{r:?}{g:?}").bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        h % 1_000 < u64::from(self.fallback_permille)
    }
}

fn class(r: ResourceId) -> Class {
    match r {
        ResourceId::Mesh(t) if super::world::is_city(t) => Class::Tiles3dMesh,
        ResourceId::Mesh(_) => Class::TerrainMesh,
        ResourceId::Drape { .. } => Class::Drape,
    }
}

impl SimGpu {
    /// Applies the decisions of step `frame`, as one render frame for every
    /// eye; returns the acknowledgements to send.
    pub fn apply(
        &mut self,
        d: &Decisions,
        frame: u64,
    ) -> Result<Vec<(ResourceId, Generation, Bytes)>, String> {
        // The compressions begun last frame are done: the RGBA8 copy goes
        // (or, one in a hundred under a fallback rate, the compressed one,
        // the encode refused); a copy evicted meanwhile went whole.
        self.compressed.clear();
        for (r, g, from, to) in std::mem::take(&mut self.compressing) {
            let refused = self.falls_back(r, g);
            let Some(b) = self.drawable.get_mut(&r) else {
                continue;
            };
            let now = if refused {
                self.fallbacks += 1;
                from
            } else {
                to
            };
            let charged = *b;
            *b = now;
            let c = class(r);
            self.bytes[c] = self.bytes[c].minus(charged).plus(now);
            self.compressed.push((r, g, charged, now));
        }
        for (r, g) in &d.cancel {
            if self.pending.get(r) == Some(g) {
                self.pending.remove(r);
            }
        }
        for (r, g) in &d.deliver {
            if self.drawable.contains_key(r) {
                return Err(format!("{r:?} delivered while already on the GPU"));
            }
            self.pending.insert(*r, *g);
        }
        // Draw first: everything drawn must be uploaded.
        for i in &d.draw.items {
            if !self.drawable.contains_key(&i.mesh) {
                return Err(format!("{:?} drawn but not uploaded: black ground", i.mesh));
            }
            for dr in &i.drapes {
                if !self.drawable.contains_key(&dr.resource) {
                    return Err(format!("{:?} worn but not uploaded", dr.resource));
                }
            }
        }
        for r in &d.evict {
            let Some(b) = self.drawable.remove(r) else {
                return Err(format!("{r:?} evicted but not on the GPU"));
            };
            let c = class(*r);
            self.bytes[c] = self.bytes[c].minus(b);
        }
        // I17: every eye draws this step's draw set.
        self.eye_drew.fill(frame);
        let mut acks = Vec::new();
        let plan = &d.upload_plan;
        let queue: Vec<ResourceId> = if self.lagging {
            plan.items
                .iter()
                .copied()
                .filter(|r| !self.drawable.contains_key(r))
                .collect()
        } else {
            plan.items.clone()
        };
        let now = self
            .deadline
            .take(&queue, &plan.groups, Bytes::ZERO, |r| {
                cost_for(r, self.plan).transfer
            });
        self.deferred += (queue.len() - now.len()) as u64;
        self.frame_items = now.len();
        self.frame_bytes = Bytes::ZERO;
        for r in now {
            if self.drawable.contains_key(r) {
                return Err(format!("{r:?} uploaded twice in one residency"));
            }
            let Some(g) = self.pending.remove(r) else {
                return Err(format!("{r:?} in the plan but never delivered"));
            };
            let c = cost_for(*r, self.plan);
            // An encode that fell back is held as it crossed.
            let b = if c.encode_blocks > 0 && self.falls_back(*r, g) {
                self.fallbacks += 1;
                c.transfer
            } else {
                c.gpu
            };
            self.frame_bytes = self.frame_bytes.plus(b);
            self.drawable.insert(*r, b);
            let c = class(*r);
            self.bytes[c] = self.bytes[c].plus(b);
            self.uploads += 1;
            acks.push((*r, g, b));
        }
        // Compressions in place: the compressed copy made now, beside the
        // RGBA8 one it replaces.
        for (r, g) in &d.compress {
            let Some(b) = self.drawable.get_mut(r) else {
                return Err(format!("{r:?} compressed but not on the GPU"));
            };
            let to = cost_for(*r, self.plan).compressed;
            let from = *b;
            *b = from.plus(to);
            let c = class(*r);
            self.bytes[c] = self.bytes[c].plus(to);
            self.compressing.push((*r, *g, from, to));
        }
        Ok(acks)
    }
}
