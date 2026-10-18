// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimGpu`: the consumer. It receives deliveries, uploads the plan in
//! order, drops evictions after applying the draw set, and keeps byte
//! counters per class. It refuses what a real GPU could not do: upload what
//! it was not given, evict what it does not hold, draw what is not uploaded.

use std::collections::BTreeMap;

use tuile_core::budget::{Bytes, Class, Decisions, Generation, PerClass, ResourceId};

use super::world::cost;

/// The consumer's GPU.
#[derive(Default)]
pub struct SimGpu {
    /// Uploaded resources and their bytes.
    pub drawable: BTreeMap<ResourceId, Bytes>,
    /// Delivered, not yet uploaded.
    pub pending: BTreeMap<ResourceId, Generation>,
    /// Bytes per class.
    pub bytes: PerClass<Bytes>,
    /// Uploads, total.
    pub uploads: u64,
}

fn class(r: ResourceId) -> Class {
    match r {
        ResourceId::Mesh(_) => Class::TerrainMesh,
        ResourceId::Drape(..) => Class::Drape,
    }
}

impl SimGpu {
    /// Applies one step's decisions; returns the acknowledgements to send.
    pub fn apply(&mut self, d: &Decisions) -> Result<Vec<(ResourceId, Generation, Bytes)>, String> {
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
            if let Some(dr) = i.drape {
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
        let mut acks = Vec::new();
        for r in &d.upload_plan.items {
            let Some(g) = self.pending.remove(r) else {
                return Err(format!("{r:?} in the plan but never delivered"));
            };
            let b = cost(*r).gpu;
            self.drawable.insert(*r, b);
            let c = class(*r);
            self.bytes[c] = self.bytes[c].plus(b);
            self.uploads += 1;
            acks.push((*r, g, b));
        }
        Ok(acks)
    }
}
