// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`BudgetConfig`]: every number the budget runs on, validated once.

use super::error::ConfigError;
use super::types::{Bytes, Class, Micros, PerClass, Policy};

/// What a failed load does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailurePolicy {
    /// Batch: a film never ships a hole, so the first failure ends the session.
    EndSession,
    /// Interactive (decision D1): retry after `first`, doubling up to `max`.
    /// The fallback stays drawn meanwhile, and the failed load holds no slot.
    RetryBackoff {
        /// The first delay.
        first: Micros,
        /// The ceiling of the doubling.
        max: Micros,
    },
}

/// Time-based LOD relaxation (`sse_scale`), in thousandths per second, so it
/// behaves the same at 30 and at 120 frames per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Relaxation {
    /// Rise of the scale per second while the protected set exceeds
    /// `target + overflow` (thousandths: 1000 = +1.0 per second).
    pub up_per_second: u32,
    /// Decay per second while every class is under its low watermark.
    pub down_per_second: u32,
    /// The ceiling of the scale (thousandths: 8000 = ×8).
    pub max_scale: u32,
}

/// Eviction hysteresis, in percent of the target: a sweep starts above `high`
/// and stops at `low`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Watermarks {
    /// Percent of the target above which a sweep starts.
    pub high_percent: u8,
    /// Percent of the target a sweep trims down to.
    pub low_percent: u8,
}

/// The fractions of the configured targets kept under each [`Pressure`]
/// level, in percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PressureFractions {
    /// Under a memory warning.
    pub warning_percent: u8,
    /// Under a critical warning.
    pub critical_percent: u8,
    /// In the background.
    pub background_percent: u8,
}

/// Every number the budget runs on.
///
/// Built from [`BudgetConfig::interactive`] or [`BudgetConfig::batch`], then
/// adjusted field by field; [`Budget::new`](super::Budget::new) validates it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetConfig {
    /// Interactive or Batch.
    pub policy: Policy,
    /// GPU bytes each class is trimmed down to. Batch: [`Bytes::MAX`].
    pub target: PerClass<Bytes>,
    /// Visible-only headroom above the target, per class. Batch: unused.
    pub overflow: PerClass<Bytes>,
    /// Decoded CPU bytes kept off the GPU (the second tier), all classes.
    pub decoded_target: Bytes,
    /// Network loads in flight per class.
    pub max_in_flight: PerClass<u32>,
    /// Local-store reads in flight, all classes: a separate quota, so local
    /// reads never wait behind network work.
    pub max_local_reads: u32,
    /// The bounded wish list actually scheduled in one step.
    pub request_heap: u32,
    /// GPU upload bytes per frame. Batch: [`Bytes::MAX`].
    pub upload_bytes_per_frame: Bytes,
    /// Decoded bytes in transit to the consumer, not yet acknowledged.
    pub delivery_credit: Bytes,
    /// How long a load may stay unneeded before it is cancelled.
    pub cancel_grace: Micros,
    /// A load unanswered this long is cancelled and handled as a transient
    /// failure, so a stalled host never holds a slot for ever. Batch: the
    /// failure ends the session.
    pub load_timeout: Micros,
    /// LOD relaxation under overflow.
    pub relax: Relaxation,
    /// Eviction hysteresis.
    pub watermarks: Watermarks,
    /// Target fractions under memory pressure.
    pub pressure: PressureFractions,
    /// The coarse floor (terrain and imagery): levels `0..=pinned_level` are
    /// requested first and never evicted.
    pub pinned_level: Option<u32>,
    /// The most imagery levels a drape may sit above its tile's terrain level.
    pub imagery_over_terrain_cap: u32,
    /// What a failed load does.
    pub failure: FailurePolicy,
    /// A load that has waited this long gets the full age bonus.
    pub age_full: Micros,
}

impl BudgetConfig {
    /// A viewer's budget for `gpu` bytes in total, split between classes.
    ///
    /// The split and every other number are first proposals (decision D5);
    /// Step 6 of the plan tunes them per profile from simulated curves.
    pub fn interactive(gpu: Bytes) -> Self {
        let share = |pct: u64| gpu.scaled(pct, 100);
        let target = PerClass([share(35), share(15), share(45), share(3), share(2)]);
        BudgetConfig {
            policy: Policy::Interactive,
            target,
            overflow: PerClass::from_fn(|c| target[c].scaled(1, 4)),
            decoded_target: gpu.scaled(1, 4),
            max_in_flight: PerClass([16, 8, 16, 4, 4]),
            max_local_reads: 8,
            request_heap: 256,
            upload_bytes_per_frame: Bytes::mib(8),
            delivery_credit: Bytes::mib(32),
            cancel_grace: Micros::from_millis(500),
            load_timeout: Micros::from_secs(10),
            relax: Relaxation {
                up_per_second: 1_000,
                down_per_second: 250,
                max_scale: 8_000,
            },
            watermarks: Watermarks {
                high_percent: 100,
                low_percent: 90,
            },
            pressure: PressureFractions {
                warning_percent: 50,
                critical_percent: 25,
                background_percent: 0,
            },
            pinned_level: Some(2),
            imagery_over_terrain_cap: 3,
            failure: FailurePolicy::RetryBackoff {
                first: Micros::from_millis(500),
                max: Micros::from_secs(30),
            },
            age_full: Micros::from_secs(2),
        }
    }

    /// A film's or a bake's budget: unbounded, exact, never cancelling.
    pub fn batch() -> Self {
        BudgetConfig {
            policy: Policy::Batch,
            target: PerClass::splat(Bytes::MAX),
            overflow: PerClass::splat(Bytes::ZERO),
            decoded_target: Bytes::MAX,
            max_in_flight: PerClass([64, 32, 64, 8, 8]),
            max_local_reads: 32,
            request_heap: 4_096,
            upload_bytes_per_frame: Bytes::MAX,
            delivery_credit: Bytes::MAX,
            cancel_grace: Micros::NEVER,
            load_timeout: Micros::from_secs(120),
            relax: Relaxation {
                up_per_second: 0,
                down_per_second: 0,
                max_scale: 1_000,
            },
            watermarks: Watermarks {
                high_percent: 100,
                low_percent: 100,
            },
            pressure: PressureFractions {
                warning_percent: 100,
                critical_percent: 100,
                background_percent: 100,
            },
            pinned_level: Some(2),
            imagery_over_terrain_cap: 3,
            failure: FailurePolicy::EndSession,
            age_full: Micros::from_secs(2),
        }
    }

    /// Checks the numbers are coherent.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let w = self.watermarks;
        if w.low_percent > w.high_percent || w.high_percent > 100 {
            return Err(ConfigError::Watermarks {
                low: w.low_percent,
                high: w.high_percent,
            });
        }
        if self.request_heap == 0 {
            return Err(ConfigError::ZeroRequestHeap);
        }
        for c in Class::ALL {
            if self.max_in_flight[c] == 0 {
                return Err(ConfigError::ZeroQuota(c));
            }
        }
        if self.relax.max_scale < 1_000 {
            return Err(ConfigError::RelaxCeiling(self.relax.max_scale));
        }
        if let FailurePolicy::RetryBackoff { first, max } = self.failure {
            if first > max || first == Micros::ZERO {
                return Err(ConfigError::Backoff);
            }
        }
        if self.policy == Policy::Batch && self.failure != FailurePolicy::EndSession {
            return Err(ConfigError::BatchMustEndOnFailure);
        }
        Ok(())
    }
}
