// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The browser side of tuile: a `fetch`-backed transport, so the same engine
//! that runs on a laptop runs in a Web Worker.
//!
//! # Why a crate rather than an example
//!
//! `tuile-core` takes no backend, and the browser is a backend like any other —
//! `web-sys` is as much a transport dependency as `reqwest` is. This crate is
//! the browser's counterpart to `tuile-native-fetchers`: it implements the same
//! two seams ([`TileFetcher`](tuile_core::fetch::TileFetcher) and
//! [`IonHttp`](tuile_cesium_ion::IonHttp)) against `window.fetch`, and adds
//! nothing else to the engine's vocabulary.
//!
//! # Where it runs
//!
//! In a **Worker**, not on the page's thread. That is the entire point: the
//! engine's traversal, its decodes and its resamples are CPU work, and on the
//! main thread they are work the browser cannot paint through. `fetch` is
//! available in a worker through `WorkerGlobalScope`, which is why
//! `global_fetch` looks for either scope rather than assuming a `Window`.
//!
//! # Single-threaded, and honest about it
//!
//! A `Response` is a JS object and JS objects are not `Send`. The engine's
//! traits are already declared `?Send` under `target_arch = "wasm32"`, so the
//! futures here never claim otherwise, and the server is driven by
//! `spawn_local` rather than a work-stealing runtime. Nothing here would be
//! made faster by pretending: a worker has one thread, and parallelism comes
//! from running several workers, not from a scheduler inside one.
//!
//! # What is portable
//!
//! Everything that touches the browser is gated on `wasm32`. The forwarding
//! rules ([`forward`]) are not: they are pure Rust, so they are tested on the
//! host and checked by every native build of the workspace.

pub mod forward;

#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
pub use transport::{WebFetcher, WebIonHttp};
#[cfg(target_arch = "wasm32")]
pub mod worker;
