// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The compressions in place the graphics budget asks for (CB9: display at
//! once, compress what lasts), in the frames' spare GPU time.
//!
//! A drape the budget names ([`ServerMessage::Compress`]) is compressed a
//! tile at a time, in the budget's order, while the frame's encode lane has
//! blocks left — its measured headroom, after the frame's uploads, which
//! never wait for it. Each imagery tile is read straight from its resident
//! RGBA8 texture into a new block texture (one compute pass for the
//! frame's tiles); the new texture replaces the old one in the shared pool
//! and in **every** drape that names it, whose wearers are dressed again
//! with it — the drawn ones in this very frame, after the pass that fills
//! it, in the same submit — and the RGBA8 texture is let go once nothing
//! binds it: new before old, never a gap, never black. A tile the encoder
//! refuses stays RGBA8.
//!
//! A drape whose every charged tile is done is acknowledged
//! ([`ClientMessage::Compressed`]) with what it now holds, once the GPU has
//! finished the frame's work.
//!
//! [`ServerMessage::Compress`]: tuile_core::protocol::ServerMessage::Compress
//! [`ClientMessage::Compressed`]: tuile_core::protocol::ClientMessage::Compressed

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tuile_core::budget::{EncodeBucket, Generation, ResourceId};
use tuile_core::raster::ImageryCoord;
use tuile_core::texels::ImageryFormat;

use crate::compress::InPlace;
use crate::context::{GpuContext, GpuImagery};
use crate::prepare::{next_drape_serial, PreparedDrape};

/// The compressions asked and not yet done, in the budget's order.
#[derive(Default)]
pub(crate) struct Queue {
    jobs: VecDeque<(ResourceId, Generation)>,
}

/// What one frame's compressions did.
#[derive(Default)]
pub(crate) struct Done {
    /// The drapes compressed whole, their generation and what they hold.
    pub(crate) drapes: Vec<(ResourceId, Generation, u64)>,
    /// The drapes whose textures changed: their wearers are dressed again.
    pub(crate) rebound: Vec<ResourceId>,
    /// Tiles encoded and the blocks they wrote.
    pub(crate) tiles: u32,
    pub(crate) blocks: u64,
    /// Compressions dropped: their drape left the GPU or was replaced.
    pub(crate) dropped: u32,
}

impl Queue {
    /// Asks for `jobs`, after those asked before.
    pub(crate) fn ask(&mut self, jobs: impl IntoIterator<Item = (ResourceId, Generation)>) {
        self.jobs.extend(jobs);
    }

    /// Compressions waiting.
    pub(crate) fn len(&self) -> usize {
        self.jobs.len()
    }

    /// One frame's compressions: from the front, a tile at a time while
    /// `bucket` has blocks (into debt by one tile), a drape whose tiles
    /// are all done acknowledged. `gen_of` says the generation of a drape
    /// held; a job for a drape no longer held at its generation is dropped.
    pub(crate) fn run(
        &mut self,
        gpu: &GpuContext,
        drapes: &mut HashMap<ResourceId, PreparedDrape>,
        gen_of: impl Fn(ResourceId) -> Option<Generation>,
        bucket: &mut EncodeBucket,
    ) -> Done {
        let mut done = Done::default();
        if self.jobs.is_empty() {
            return done;
        }
        let Some(format) = gpu
            .compressor
            .as_ref()
            .and_then(|c| c.lock().ok().map(|c| c.format()))
        else {
            // No encoder any more (the plan was switched off): what was
            // asked stays as it is, acknowledged so.
            for (r, g) in self.jobs.drain(..) {
                if let Some(d) = drapes.get(&r).filter(|_| gen_of(r) == Some(g)) {
                    done.drapes.push((r, g, held(d)));
                }
            }
            return done;
        };
        // The tiles to encode this frame, each once however many drapes
        // name it, and the drapes they finish.
        let mut picked: Vec<(u32, ImageryCoord, Arc<GpuImagery>)> = Vec::new();
        let mut finishing: Vec<(ResourceId, Generation)> = Vec::new();
        while let Some(&(r, g)) = self.jobs.front() {
            let Some(drape) = drapes.get(&r).filter(|_| gen_of(r) == Some(g)) else {
                self.jobs.pop_front();
                done.dropped += 1;
                continue;
            };
            let mut all = true;
            for l in drape.layers.iter().filter(|l| l.charge.0 > 0) {
                let key = (l.source, l.coord);
                if !l.texture.compresses_into(format)
                    || picked.iter().any(|p| (p.0, p.1) == key)
                {
                    continue;
                }
                if bucket.carry <= 0 {
                    all = false;
                    break;
                }
                let t = l.texture.texture();
                let blocks = crate::compress::tile_blocks(format, t.width(), t.height());
                bucket.carry = bucket
                    .carry
                    .saturating_sub(i64::try_from(blocks).unwrap_or(i64::MAX));
                picked.push((l.source, l.coord, Arc::clone(&l.texture)));
            }
            if !all {
                // The lane is spent: the rest of this drape, and everything
                // behind it, waits for a frame with room.
                break;
            }
            finishing.push((r, g));
            self.jobs.pop_front();
        }
        if !picked.is_empty() {
            let made = encode(gpu, &picked);
            let mut pool = gpu.imagery.lock().ok();
            for ((source, coord, old), made) in picked.into_iter().zip(made) {
                let Some(new) = made else {
                    // Refused: it stays RGBA8, and is not tried again.
                    old.refused
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    continue;
                };
                done.tiles += 1;
                let t = old.texture();
                done.blocks += crate::compress::tile_blocks(format, t.width(), t.height());
                let new = Arc::new(new);
                if let Some(pool) = pool.as_mut() {
                    pool.replace(source, coord, &new);
                }
                // Every drape naming the old texture takes the new one: the
                // old goes once nothing binds it.
                for (r, d) in drapes.iter_mut() {
                    let mut moved = false;
                    for l in d.layers.iter_mut() {
                        if Arc::ptr_eq(&l.texture, &old) {
                            l.texture = Arc::clone(&new);
                            moved = true;
                        }
                    }
                    if moved {
                        d.serial = next_drape_serial();
                        done.rebound.push(*r);
                    }
                }
            }
        }
        for (r, g) in finishing {
            if let Some(d) = drapes.get_mut(&r) {
                let bytes = held(d);
                d.gpu_bytes = bytes;
                done.drapes.push((r, g, bytes));
            }
        }
        done.rebound.sort_unstable();
        done.rebound.dedup();
        done
    }
}

/// What `drape` holds now: each charged tile's share, compressed where its
/// texture is blocks.
fn held(drape: &PreparedDrape) -> u64 {
    drape
        .layers
        .iter()
        .map(|l| match l.texture.format {
            ImageryFormat::Rgba8 => l.charge.0,
            _ => l.charge.1,
        })
        .sum()
}

/// The compressed copies of `tiles`, in one compute pass recorded in the
/// frame's uploads: `None` for a tile the encoder refused.
fn encode(gpu: &GpuContext, tiles: &[(u32, ImageryCoord, Arc<GpuImagery>)]) -> Vec<Option<GpuImagery>> {
    let Some(compressor) = gpu.compressor.as_ref() else {
        return tiles.iter().map(|_| None).collect();
    };
    // The scratch must not hold a record not yet submitted.
    if compressor.lock().is_ok_and(|c| c.busy()) {
        gpu.flush_uploads();
    }
    let Ok(mut c) = compressor.lock() else {
        return tiles.iter().map(|_| None).collect();
    };
    let format = c.format();
    let inputs: Vec<InPlace<'_>> = tiles
        .iter()
        .map(|(_, _, t)| {
            let tex = t.texture();
            InPlace {
                texture: tex,
                width: tex.width(),
                height: tex.height(),
                levels: tex.mip_level_count(),
            }
        })
        .collect();
    let results =
        gpu.record(|cmd| c.record_in_place(&gpu.device, &gpu.ops, &gpu.queue, cmd, &inputs));
    results
        .into_iter()
        .zip(&inputs)
        .map(|(r, i)| {
            r.ok().map(|texture| {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                let bytes = (0..i.levels)
                    .map(|l| {
                        tuile_core::texels::BlockFormat::level_bytes(
                            (i.width >> l).max(1),
                            (i.height >> l).max(1),
                        )
                    })
                    .sum();
                GpuImagery::new(texture, view, bytes, format.into())
            })
        })
        .collect()
}
