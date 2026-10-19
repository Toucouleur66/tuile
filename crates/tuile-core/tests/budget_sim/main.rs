// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget's deterministic simulation (plan Step 3): a synthetic world
//! with sea, plain and mountains, a seeded network with failures and stalls,
//! a GPU that refuses the impossible, scripted cameras, and every invariant
//! I1–I15 that applies checked at every step.
//!
//! Scenarios are named after the behaviour they pin down. The one-hour soak
//! is `#[ignore]`d and runs nightly in release; a five-minute soak runs on
//! every PR.

#![allow(clippy::unwrap_used, clippy::panic)]

mod architecture;
mod detmath;
mod gpu;
mod net;
mod patterns;
mod rng;
mod scenarios;
mod server;
mod soak;
mod world;
