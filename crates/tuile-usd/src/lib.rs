// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod stage;

pub use stage::{render_origin, write_manifest, ManifestConfig};
