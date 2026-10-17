// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the device says about itself: its memory, how much of it this process
//! may still take, how hot it runs, and whether it is saving power.
//!
//! Three plain functions for the rest of the viewer. The implementation that
//! asks an operating system is the private `system` module, compiled on iOS
//! with the `application` feature; everywhere else the answers are "nominal"
//! and "unknown", which is what [`crate::profile::Profile::desktop`] expects.
//!
//! All of it is cheap enough to ask once a second from the frame loop: a
//! property read on the process object and one system call.

use crate::profile::Thermal;

/// The device's memory, and what is left to this process. A page is told
/// its device's memory by the browser instead.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn memory() -> crate::profile::DeviceMemory {
    system::memory()
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

    use crate::profile::{DeviceMemory, Thermal};

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

#[cfg(not(all(target_os = "ios", feature = "application")))]
mod system {
    use crate::profile::Thermal;

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
