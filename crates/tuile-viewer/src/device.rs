// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the device says about itself: its memory, how much of it this process
//! may still take, how short of memory the system is, how hot it runs, and
//! whether it is saving power.
//!
//! Plain functions for the rest of the viewer — and the inputs a budget sized
//! from the device reads, on every handset alike. The implementation that
//! asks an operating system is the private `system` module, compiled on iOS
//! with the `application` feature and on Android; everywhere else the answers
//! are "nominal" and "unknown", which is what
//! [`crate::profile::Profile::desktop`] expects.
//!
//! All of it is cheap enough to ask once a second from the frame loop: a
//! property read on the process object and one system call.

use crate::profile::{Pressure, Thermal};

/// The device's memory, and what is left to this process. A page is told
/// its device's memory by the browser instead.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn memory() -> crate::profile::DeviceMemory {
    system::memory()
}

/// How many times the system has said it is short of memory, and how short
/// it said it was the last time. A system that says so through a warning
/// event instead (iOS, winit's `memory_warning`) always answers `(0,
/// Nominal)`.
pub(crate) fn pressure() -> (u64, Pressure) {
    system::pressure()
}

/// How hot the device is running.
pub(crate) fn thermal() -> Thermal {
    system::thermal()
}

/// Whether the person has asked the device to save power.
pub(crate) fn low_power() -> bool {
    system::low_power()
}

#[cfg(all(target_os = "ios", feature = "application"))]
mod system {
    #![allow(unsafe_code)]

    use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};

    use crate::profile::{DeviceMemory, Pressure, Thermal};

    pub(super) fn pressure() -> (u64, Pressure) {
        (0, Pressure::Nominal)
    }

    extern "C" {
        /// `<os/proc.h>`: the bytes this process may still allocate before
        /// it reaches its memory limit. Zero when the process has no limit —
        /// on a simulator, for one. In libSystem, which every program links.
        fn os_proc_available_memory() -> usize;
    }

    pub(super) fn memory() -> DeviceMemory {
        let physical = NSProcessInfo::processInfo().physicalMemory();
        // SAFETY: a system call with no arguments and no preconditions.
        let available = unsafe { os_proc_available_memory() } as u64;
        DeviceMemory {
            physical,
            available: (available > 0).then_some(available),
        }
    }

    pub(super) fn thermal() -> Thermal {
        let state = NSProcessInfo::processInfo().thermalState();
        if state == NSProcessInfoThermalState::Critical {
            Thermal::Critical
        } else if state == NSProcessInfoThermalState::Serious {
            Thermal::Serious
        } else if state == NSProcessInfoThermalState::Fair {
            Thermal::Fair
        } else {
            Thermal::Nominal
        }
    }

    pub(super) fn low_power() -> bool {
        NSProcessInfo::processInfo().isLowPowerModeEnabled()
    }
}

/// Android: the memory from the kernel, the rest from the activity.
#[cfg(target_os = "android")]
mod system {
    use crate::native::android;
    use crate::native::android_signals as signals;
    use crate::profile::{DeviceMemory, Pressure, Thermal};

    pub(super) fn memory() -> DeviceMemory {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(signals::memory_from_meminfo)
            .unwrap_or(DeviceMemory {
                // A low-end handset, when even the kernel will not say.
                physical: 2 << 30,
                available: None,
            })
    }

    pub(super) fn pressure() -> (u64, Pressure) {
        signals::unpack_trim(android::trim_signal())
    }

    pub(super) fn thermal() -> Thermal {
        signals::thermal_of_status(android::thermal_status())
    }

    pub(super) fn low_power() -> bool {
        android::power_save()
    }
}

#[cfg(not(any(all(target_os = "ios", feature = "application"), target_os = "android")))]
mod system {
    use crate::profile::{Pressure, Thermal};

    pub(super) fn pressure() -> (u64, Pressure) {
        (0, Pressure::Nominal)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn memory() -> crate::profile::DeviceMemory {
        crate::profile::DeviceMemory {
            // Unused off a phone, where the profile is not sized from memory.
            physical: 8 << 30,
            available: None,
        }
    }

    pub(super) fn thermal() -> Thermal {
        Thermal::Nominal
    }

    pub(super) fn low_power() -> bool {
        false
    }
}
