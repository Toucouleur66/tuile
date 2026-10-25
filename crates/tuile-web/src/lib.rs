// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod patience;
pub mod wire;

#[cfg(target_arch = "wasm32")]
pub mod post;
#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
pub use transport::{WebFetcher, WebIonHttp};
