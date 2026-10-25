// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How much of the machine the globe may take.
//!
//! A game is the larger tenant of its device: the globe gets a share it is
//! told, and the rest is the host's. The share is a number of bytes the
//! graphics budget sizes every class from (`BudgetConfig::interactive`): what
//! the GPU holds of meshes and imagery, the overflow it may run into while a
//! view changes, and the hard limit it never crosses.

use tuile_core::budget::{BudgetConfig, Bytes, Micros};

const MIB: u64 = 1024 * 1024;

/// The globe's share of the device, and how sharp it asks the ground to be.
#[derive(Debug, Clone, PartialEq)]
pub struct TuileBudget {
    /// The bytes the graphics budget is sized from: every class's target,
    /// overflow and hard limit are shares of it.
    pub gpu_bytes: u64,
    /// The levels `0..=pinned_level` are loaded first and never evicted:
    /// what every fallback lands on, and the coarse backdrop drawn behind
    /// everything. At least the backdrop's level.
    pub pinned_level: u32,
    /// The traversal's screen-space error, logical pixels: smaller is
    /// sharper and costs more tiles.
    pub sse_threshold: f64,
    /// How many imagery levels a drape may sit above its tile's terrain.
    pub imagery_over_terrain_cap: u32,
    /// How long one load may take before it is cancelled and retried.
    pub load_timeout_s: f64,
    /// Threads decoding and resampling tiles.
    pub decode_threads: usize,
    /// Threads of the runtime the loads run on (network, the server).
    pub runtime_workers: usize,
}

/// The level drawn behind everything, complete, every frame: `2·4^level`
/// tiles, 32 at level 2. Pinned by every budget, so no eviction can take it.
pub const BACKDROP_LEVEL: u32 = 2;

impl TuileBudget {
    /// A desktop's: a gibibyte, every core but two decoding.
    pub fn desktop() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            gpu_bytes: 1024 * MIB,
            pinned_level: BACKDROP_LEVEL,
            sse_threshold: 1.0,
            imagery_over_terrain_cap: 2,
            load_timeout_s: 4.0,
            decode_threads: cores.saturating_sub(2).clamp(1, 8),
            runtime_workers: 2,
        }
    }

    /// A phone's, beside a game that keeps most of the memory: 320 MiB, the
    /// middle of the 300–350 MB a mid-range phone of 2022–23 leaves the
    /// globe out of about a gigabyte.
    pub fn handheld() -> Self {
        Self {
            gpu_bytes: 320 * MIB,
            pinned_level: BACKDROP_LEVEL,
            // ⅔ of a logical pixel: the 2 device pixels a desktop draws at,
            // at a phone's density of 3.
            sse_threshold: 2.0 / 3.0,
            imagery_over_terrain_cap: 2,
            load_timeout_s: 4.0,
            decode_threads: 2,
            runtime_workers: 2,
        }
    }

    /// The same, with `bytes` for the globe.
    #[must_use]
    pub fn with_gpu_bytes(mut self, bytes: u64) -> Self {
        self.gpu_bytes = bytes;
        self
    }

    /// The graphics budget's configuration.
    pub fn config(&self) -> BudgetConfig {
        let mut b = BudgetConfig::interactive(Bytes(self.gpu_bytes));
        b.pinned_level = Some(self.pinned_level.max(BACKDROP_LEVEL));
        b.imagery_over_terrain_cap = self.imagery_over_terrain_cap;
        b.load_timeout = Micros((self.load_timeout_s.max(0.1) * 1e6) as u64);
        b
    }

    /// The traversal's configuration.
    pub fn traversal(&self) -> tuile_core::traversal::Config {
        tuile_core::traversal::Config {
            maximum_screen_space_error: self.sse_threshold,
            ..tuile_core::traversal::Config::interactive_globe()
        }
    }
}

impl Default for TuileBudget {
    /// A phone's on a phone and in a browser, a desktop's elsewhere.
    fn default() -> Self {
        if cfg!(any(
            target_os = "ios",
            target_os = "android",
            target_arch = "wasm32"
        )) {
            Self::handheld()
        } else {
            Self::desktop()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone's share is the one a game leaves it, and the backdrop is
    /// pinned whatever a host asks for.
    #[test]
    fn a_phone_leaves_the_game_its_memory_and_the_backdrop_is_pinned() {
        let phone = TuileBudget::handheld();
        assert!((300 * MIB..=350 * MIB).contains(&phone.gpu_bytes));
        let config = TuileBudget {
            pinned_level: 0,
            ..phone
        }
        .config();
        assert_eq!(config.pinned_level, Some(BACKDROP_LEVEL));
        let config = TuileBudget::desktop().with_gpu_bytes(64 * MIB).config();
        let mut expected = BudgetConfig::interactive(Bytes(64 * MIB));
        expected.pinned_level = Some(BACKDROP_LEVEL);
        expected.imagery_over_terrain_cap = 2;
        expected.load_timeout = Micros(4_000_000);
        assert_eq!(config, expected);
    }
}
