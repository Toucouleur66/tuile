// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How hard a session may push the machine it runs on.
//!
//! The viewer is one program on a desktop and on a phone, and nearly every
//! difference between the two is a number: how much decoded ground stays
//! resident, how many fetches are in flight, how many threads decode, how big
//! the tile store is, how many frames a second are worth drawing when nothing
//! moves. Those numbers live here, side by side, so the two machines are
//! compared in one place instead of in `cfg` blocks scattered through the
//! session.
//!
//! [`Profile::desktop`] is exactly what the desktop application has always
//! run with. [`Profile::handheld`] is sized from the device: a phone shares
//! one pool of memory between the CPU and the GPU, and the system ends an
//! application that takes too much of it without asking — so the budgets
//! are a share of what the process may still take, not a constant.

use std::time::Duration;

const MIB: usize = 1024 * 1024;

/// What the system says about the memory this process may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceMemory {
    /// The device's physical memory, bytes.
    pub physical: u64,
    /// How much more this process may allocate before the system ends it,
    /// bytes — when the system says. A simulator and a desktop do not.
    pub available: Option<u64>,
}

impl DeviceMemory {
    /// The bytes a session may plan around: what is left to this process when
    /// that is known, a quarter of the device otherwise.
    fn usable(&self) -> u64 {
        self.available
            .filter(|&a| a > 0)
            .unwrap_or(self.physical / 4)
    }
}

/// How hot the device is running, as the system grades it.
// Only a phone's system reports anything but nominal.
#[cfg_attr(not(all(target_os = "ios", feature = "application")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum Thermal {
    #[default]
    Nominal,
    Fair,
    /// The system is throttling: do less, visibly.
    Serious,
    /// Close to being shut down: as little as the picture allows.
    Critical,
}

/// Per-frame limits on moving decoded tiles to the GPU.
///
/// Uploading is done on the frame's own thread, so every upload is time the
/// frame does not have. A count alone is not a budget — one tile is a few
/// kilobytes of mesh, another megabytes of imagery — so a phone also stops
/// when the frame's share of time is spent. One upload always goes through,
/// or a slow device would never make progress at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UploadBudget {
    pub tiles: usize,
    pub time: Option<Duration>,
}

impl UploadBudget {
    /// Whether another upload fits, after `done` uploads that took `spent`.
    pub(crate) fn allows(&self, done: usize, spent: Duration) -> bool {
        if done == 0 {
            return true;
        }
        done < self.tiles && self.time.is_none_or(|limit| spent < limit)
    }
}

/// The numbers a session runs with. See the module.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Profile {
    /// Decoded content the geometry server keeps resident, bytes.
    pub resident_budget_bytes: usize,
    pub resident_tile_limit: usize,
    /// Tile loads in flight at once — one HTTP/2 stream each.
    pub simultaneous_fetches: usize,
    /// The level held for the whole session when `TUILE_PIN_LEVEL` says
    /// nothing. Never below [`crate::backdrop::BASE_LEVEL`], which draws it.
    pub pinned_level: u32,
    /// Threads of the async runtime: network, store, the server.
    pub runtime_workers: usize,
    /// Its blocking pool: the store's file I/O.
    pub blocking_threads: usize,
    /// Threads decoding and resampling tiles.
    pub decode_threads: usize,
    pub uploads: UploadBudget,
    /// The tile store's memory and disk tiers, bytes.
    pub store_memory_bytes: usize,
    pub store_disk_bytes: usize,
    /// Whether the window stays hidden until the coarse pyramid is on the GPU
    /// (a desktop), or opens at once with a loading indicator over the globe
    /// (a phone, where the system ends an application that shows nothing).
    pub hold_window_until_warm: bool,
    /// Whether frames are drawn only while something changes on screen.
    pub render_on_demand: bool,
    /// Whether the surface and its render targets are let go of while the
    /// application is in the background.
    pub release_surface_in_background: bool,
    /// Whether the counters are served on a local HTTP endpoint.
    pub metrics_endpoint: bool,
}

impl Profile {
    /// The desktop application, as it has always run.
    pub(crate) fn desktop() -> Self {
        let interactive = tuile_core::traversal::Config::interactive_globe(None);
        Self {
            resident_budget_bytes: interactive.resident_budget_bytes,
            resident_tile_limit: interactive.resident_tile_limit,
            simultaneous_fetches: interactive.maximum_simultaneous_fetches,
            pinned_level: 4,
            runtime_workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
            blocking_threads: 512,
            decode_threads: std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .saturating_sub(1)
                .max(1),
            uploads: UploadBudget {
                tiles: tuile_wgpu::UPLOADS_PER_FRAME,
                time: None,
            },
            store_memory_bytes: 256 * MIB,
            store_disk_bytes: 4096 * MIB,
            hold_window_until_warm: true,
            render_on_demand: false,
            release_surface_in_background: false,
            metrics_endpoint: true,
        }
    }

    /// A phone or a tablet with `memory`, and `cores` processors.
    pub(crate) fn handheld(memory: DeviceMemory, cores: usize) -> Self {
        // A fifth of what is left to the process for decoded ground. The GPU
        // copy costs about 1.35× that again on the same memory, and imagery,
        // the render targets and the runtime need the rest: well under the
        // half at which the system starts ending applications.
        let usable = usize::try_from(memory.usable()).unwrap_or(usize::MAX);
        let resident = (usable / 5).clamp(160 * MIB, 768 * MIB);
        Self {
            resident_budget_bytes: resident,
            // The desktop's ratio of tiles to bytes: about 256 KiB a tile.
            resident_tile_limit: (resident / (256 * 1024)).max(1024),
            // One HTTP/2 connection carries them all; more in flight is a
            // longer queue inside the client, not more bandwidth.
            simultaneous_fetches: 16,
            // Level 3 is 85 tiles through the pyramid, a few megabytes: the
            // floor every fallback lands on, and what the launch waits for.
            pinned_level: crate::backdrop::BASE_LEVEL,
            runtime_workers: 2,
            blocking_threads: 4,
            // Leave the main thread and the runtime a core each.
            decode_threads: cores.saturating_sub(2).clamp(1, 3),
            uploads: UploadBudget {
                tiles: 4,
                time: Some(Duration::from_millis(6)),
            },
            store_memory_bytes: 32 * MIB,
            store_disk_bytes: 768 * MIB,
            hold_window_until_warm: false,
            render_on_demand: true,
            release_surface_in_background: true,
            metrics_endpoint: false,
        }
    }

    /// The profile for this build's platform and this device.
    pub(crate) fn current() -> Self {
        if cfg!(target_os = "ios") {
            let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
            Self::handheld(crate::device::memory(), cores)
        } else {
            Self::desktop()
        }
    }

    /// The traversal's configuration for this profile, the rest as the
    /// reference interactive session has it.
    pub(crate) fn traversal(&self, pinned_level: Option<u32>) -> tuile_core::traversal::Config {
        tuile_core::traversal::Config {
            resident_budget_bytes: self.resident_budget_bytes,
            resident_tile_limit: self.resident_tile_limit,
            maximum_simultaneous_fetches: self.simultaneous_fetches,
            ..tuile_core::traversal::Config::interactive_globe(pinned_level)
        }
    }

    /// The shortest interval between two frames, for how hot the device runs
    /// and whether it is saving power. A desktop is never held back.
    pub(crate) fn frame_interval(&self, thermal: Thermal, low_power: bool) -> Duration {
        if !self.render_on_demand {
            return Duration::ZERO;
        }
        let fps = match thermal {
            Thermal::Critical => 20,
            Thermal::Serious => 30,
            _ if low_power => 30,
            _ => 60,
        };
        Duration::from_secs(1) / fps
    }

    /// What a memory warning asks the server to let go of: everything beyond
    /// half the budget, except what the view draws and its ancestors — the
    /// server never releases those, so the ground stays covered.
    pub(crate) fn trim_on_warning(&self) -> usize {
        self.resident_budget_bytes / 2
    }

    /// The same, on the way into the background, where the system ends the
    /// largest applications first.
    pub(crate) fn trim_in_background(&self) -> usize {
        self.resident_budget_bytes / 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The desktop profile is the reference interactive session, number for
    /// number: making the phone fit must not move the desktop.
    #[test]
    fn the_desktop_runs_as_it_always_has() {
        let p = Profile::desktop();
        let reference = tuile_core::traversal::Config::interactive_globe(Some(4));
        let config = p.traversal(Some(4));
        assert_eq!(
            config.resident_budget_bytes,
            reference.resident_budget_bytes
        );
        assert_eq!(config.resident_tile_limit, reference.resident_tile_limit);
        assert_eq!(
            config.maximum_simultaneous_fetches,
            reference.maximum_simultaneous_fetches
        );
        assert!(p.hold_window_until_warm && !p.render_on_demand);
        assert_eq!(p.uploads.tiles, tuile_wgpu::UPLOADS_PER_FRAME);
        assert_eq!(p.frame_interval(Thermal::Critical, true), Duration::ZERO);
    }

    /// A phone's budget is a share of what the process may still take, held
    /// between a floor that can still show a view and a ceiling well short of
    /// where the system ends applications.
    #[test]
    fn a_phone_sizes_its_budget_from_the_memory_it_is_left() {
        let phone = |available| {
            Profile::handheld(
                DeviceMemory {
                    physical: 6 * GIB,
                    available,
                },
                6,
            )
        };
        let roomy = phone(Some(3 * GIB));
        assert_eq!(roomy.resident_budget_bytes, 3 * 1024 * MIB / 5);
        assert!(roomy.resident_budget_bytes < 3 * 1024 * MIB / 2);
        // Less left, less held — down to the floor and no further.
        let tight = phone(Some(GIB / 2));
        assert_eq!(tight.resident_budget_bytes, 160 * MIB);
        // Lots left still caps out.
        assert_eq!(phone(Some(16 * GIB)).resident_budget_bytes, 768 * MIB);
        // A simulator says nothing about what is left: a quarter of the
        // machine stands in for it.
        let unknown = phone(None);
        assert_eq!(unknown.resident_budget_bytes, 6 * 1024 * MIB / 4 / 5);
        for p in [roomy, tight, unknown] {
            assert!(p.resident_tile_limit >= 1024);
            assert!(p.simultaneous_fetches < Profile::desktop().simultaneous_fetches);
            assert!(p.runtime_workers <= 2 && p.decode_threads <= 3);
            assert!(p.decode_threads >= 1);
            assert!(!p.hold_window_until_warm && p.render_on_demand);
            assert!(p.release_surface_in_background && !p.metrics_endpoint);
        }
    }

    /// The pinned level is the one the backdrop draws, or the backdrop could
    /// be evicted from under the globe.
    #[test]
    fn a_phone_pins_the_level_its_backdrop_draws() {
        let p = Profile::handheld(
            DeviceMemory {
                physical: 4 * GIB,
                available: None,
            },
            2,
        );
        assert_eq!(p.pinned_level, crate::backdrop::BASE_LEVEL);
        assert_eq!(p.decode_threads, 1);
    }

    /// A trim never asks for more than the budget, and the background asks
    /// for more to go than a warning does.
    #[test]
    fn trims_shrink_toward_what_the_view_draws() {
        let p = Profile::handheld(
            DeviceMemory {
                physical: 8 * GIB,
                available: Some(4 * GIB),
            },
            6,
        );
        assert!(p.trim_on_warning() < p.resident_budget_bytes);
        assert!(p.trim_in_background() < p.trim_on_warning());
    }

    /// Hotter runs slower; saving power runs slower; one upload always goes
    /// through, the rest stop at the count or the time, whichever comes first.
    #[test]
    fn the_phone_slows_down_when_it_is_hot_and_uploads_within_its_frame() {
        let p = Profile::handheld(
            DeviceMemory {
                physical: 8 * GIB,
                available: None,
            },
            6,
        );
        let at = |t, low| p.frame_interval(t, low);
        assert_eq!(at(Thermal::Nominal, false), Duration::from_secs(1) / 60);
        assert_eq!(at(Thermal::Fair, true), Duration::from_secs(1) / 30);
        assert_eq!(at(Thermal::Serious, false), Duration::from_secs(1) / 30);
        assert_eq!(at(Thermal::Critical, false), Duration::from_secs(1) / 20);

        let b = p.uploads;
        assert!(b.allows(0, Duration::from_secs(1)), "the first always goes");
        assert!(b.allows(1, Duration::from_millis(1)));
        assert!(!b.allows(1, Duration::from_millis(7)), "out of time");
        assert!(!b.allows(4, Duration::ZERO), "out of tiles");
    }
}
