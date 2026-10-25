// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod patience;
// The decode pool's bookkeeping, on every target so its tests run on the
// host.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub mod schedule;
pub mod wire;

#[cfg(target_arch = "wasm32")]
pub mod decode;
#[cfg(target_arch = "wasm32")]
pub mod post;
#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
pub use transport::{WebFetcher, WebIonHttp};
