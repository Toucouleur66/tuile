// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`FrameDeadline`]: the executor's upload cap per render frame (I18).
//!
//! The budget's [`UploadPlan`](super::UploadPlan) is capped per decision
//! step by `upload_bytes_per_frame`, deterministically. Only the render side
//! knows its frame: a headset at 90 Hz has 11.1 ms, of which uploads may take
//! a share, at the throughput the executor measures. The executor drains its
//! upload queue in the plan's order through [`FrameDeadline::take`], frame by
//! frame; what it defers it reports as a deviation, and it reaches the
//! budget only as later acknowledgements — never as an input of `decide`
//! (D-A8). Every executor — the wgpu pump, the web page, the USD scene index
//! — applies this one rule, which is why it lives in the core.

use super::decisions::SwapGroup;
use super::types::{Bytes, Micros, ResourceId};

/// How much one render frame may upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameDeadline {
    /// Bytes per frame.
    pub bytes: Bytes,
    /// The time those bytes may take, per frame.
    pub time: Micros,
    /// The encode lane's share (CB5): the 4×4 blocks the frame's GPU
    /// encodes may write, on average — what an [`EncodeBucket`] is
    /// refilled by each frame. `u64::MAX`: no encode lane.
    pub encode_blocks: u64,
}

/// The encode lane's carry from frame to frame (CB5): refilled by the
/// frame's share, capped at one share, and taken by what the frame
/// encodes — into debt when one resource writes more than is left, so that
/// a tile larger than a share still goes, and the frames after it wait
/// until the lane has blocks again. Integers: the same on every platform.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EncodeBucket {
    /// Blocks the lane may still write; negative, the debt.
    pub carry: i64,
}

impl EncodeBucket {
    /// A new frame: `share` blocks more, the carry capped at one share.
    pub fn refill(&mut self, share: u64) {
        let share = i64::try_from(share).unwrap_or(i64::MAX);
        self.carry = self.carry.saturating_add(share).min(share);
    }
}

impl FrameDeadline {
    /// No cap: a film's frame waits for what it needs.
    pub const UNBOUNDED: FrameDeadline = FrameDeadline {
        bytes: Bytes::MAX,
        time: Micros::NEVER,
        encode_blocks: u64::MAX,
    };

    /// This deadline with an encode lane of `blocks` a frame.
    pub const fn with_encode_blocks(mut self, blocks: u64) -> Self {
        self.encode_blocks = blocks;
        self
    }

    /// The deadline of a display at `hz`: the frame period, times the share
    /// of it given to uploads (`upload_share_permille`, thousandths), times
    /// the executor's measured upload throughput (`bytes_per_ms`). The
    /// throughput is the executor's own and may vary; it never enters
    /// `decide`.
    pub fn from_refresh(hz: u32, upload_share_permille: u16, bytes_per_ms: u64) -> Self {
        let period_us = 1_000_000 / u64::from(hz.max(1));
        let time_us = period_us * u64::from(upload_share_permille.min(1_000)) / 1_000;
        let bytes = u128::from(time_us) * u128::from(bytes_per_ms) / 1_000;
        FrameDeadline {
            bytes: Bytes(u64::try_from(bytes).unwrap_or(u64::MAX)),
            time: Micros(time_us),
            encode_blocks: u64::MAX,
        }
    }

    /// The prefix of `queue` (the executor's uploads, in the plan's order)
    /// this frame uploads, `done` bytes having gone already this frame:
    /// while the bytes fit, item by item, a swap group of `groups` whole
    /// when it fits; the first item of a frame always goes, so a resource
    /// larger than the deadline cannot block the queue.
    pub fn take<'p>(
        &self,
        queue: &'p [ResourceId],
        groups: &[SwapGroup],
        done: Bytes,
        cost: impl Fn(ResourceId) -> Bytes,
    ) -> &'p [ResourceId] {
        self.take_with(queue, groups, done, &mut EncodeBucket::default(), |r| {
            (cost(r), 0)
        })
    }

    /// [`Self::take`] with the encode lane (CB5): `cost` gives each item's
    /// bytes and the blocks its GPU encodes write. A unit — an item, or a
    /// swap group whole — goes when its bytes fit and, if it encodes, the
    /// lane has blocks left (`bucket.carry > 0`); its blocks are taken from
    /// the carry, into debt if need be — so a frame encodes at most its
    /// share and one unit more. The first unit to wait stops the
    /// frame: nothing overtakes it. The first item of a frame goes past
    /// the byte cap as before, never past the lane's debt.
    pub fn take_with<'p>(
        &self,
        queue: &'p [ResourceId],
        groups: &[SwapGroup],
        done: Bytes,
        bucket: &mut EncodeBucket,
        cost: impl Fn(ResourceId) -> (Bytes, u64),
    ) -> &'p [ResourceId] {
        let mut used = done;
        let mut n = 0;
        let spend = |bucket: &mut EncodeBucket, blocks: u64| {
            bucket.carry = bucket
                .carry
                .saturating_sub(i64::try_from(blocks).unwrap_or(i64::MAX));
        };
        while n < queue.len() {
            let unit = swap_unit(queue, n, groups);
            let (bytes, blocks) =
                queue[n..n + unit]
                    .iter()
                    .fold((Bytes::ZERO, 0u64), |(a, k), r| {
                        let (b, x) = cost(*r);
                        (a.plus(b), k.saturating_add(x))
                    });
            if blocks > 0 && bucket.carry <= 0 {
                // The lane is spent, or in debt: this unit, and all behind
                // it, wait.
                break;
            }
            if used.plus(bytes) <= self.bytes {
                used = used.plus(bytes);
                spend(bucket, blocks);
                n += unit;
                continue;
            }
            if n == 0 && done == Bytes::ZERO {
                // The first item of a frame always goes, alone if its group
                // is too big.
                spend(bucket, cost(queue[0]).1);
                n = 1;
            }
            break;
        }
        &queue[..n]
    }
}

/// The length of the unit at `n` of `queue` (an executor's uploads, in the
/// plan's order): a whole swap group of `groups` when the queue holds its
/// members from `n` on, contiguously, as the plan lays them out; else the
/// item alone. What every lane of a frame — its bytes, its encodes — takes
/// or defers whole, so that no member of a swap is drawn without the rest.
pub fn swap_unit(queue: &[ResourceId], n: usize, groups: &[SwapGroup]) -> usize {
    groups
        .iter()
        .find(|g| {
            g.members.len() > 1
                && g.members.contains(&queue[n])
                && queue
                    .get(n..n + g.members.len())
                    .is_some_and(|run| run.iter().all(|r| g.members.contains(r)))
        })
        .map_or(1, |g| g.members.len())
}
