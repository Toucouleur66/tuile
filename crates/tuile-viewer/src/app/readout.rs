// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The menu-bar readout, where there is a menu bar.
//!
//! A desktop shows where the eye is in a status item beside the clock. A phone
//! and a page have no menu bar and the crates behind the item do not build
//! for them, so there the readout is a type with the same three calls that
//! never exists:
//! [`StatusBar::new`] answers `None`, exactly as it does on a desktop that
//! declines the item, and every caller already handles that.

#[cfg(not(any(target_os = "ios", target_os = "android", target_arch = "wasm32")))]
pub(super) use tuile_ui::StatusBar;

/// No menu bar, so no readout.
#[cfg(any(target_os = "ios", target_os = "android", target_arch = "wasm32"))]
pub(super) struct StatusBar;

#[cfg(any(target_os = "ios", target_os = "android", target_arch = "wasm32"))]
impl StatusBar {
    pub(super) fn new() -> Option<Self> {
        None
    }

    pub(super) fn refresh(&self) {}

    pub(super) fn set_camera(&self, _lon: f64, _lat: f64, _alt: f64, _heading: f64, _pitch: f64) {}
}
