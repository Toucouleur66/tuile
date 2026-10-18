// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the page and its worker say to each other beside the stream.
//!
//! The streaming protocol crosses as bytes (`tuile_web::post`). What is left
//! is what a desktop shares through memory and a page cannot: the start-up,
//! the imagery detail and slot count the loader reads, a change of layer, the
//! loads held while the tab is hidden. Each is a small object, posted as a
//! structured clone.

use serde::{Deserialize, Serialize};

use super::options::{ProfileChoice, SourcesSpec};

/// From the page to its worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ToWorker {
    /// Bring the session up: the sources, which terrain and layers, the
    /// numbers to run with. No credential: the catalog is a proxy that holds
    /// its own.
    #[serde(rename_all = "camelCase")]
    Start {
        sources: SourcesSpec,
        opening: usize,
        profile: ProfileChoice,
        device_memory: Option<f64>,
    },
    /// The ground texel spacing the imagery is draped at, metres: see
    /// `tuile_planetary::ImageryDetail`.
    Detail { metres: f64 },
    /// The sampled textures per stage the device reported: see
    /// `tuile_planetary::LayerBudget`.
    Budget { reported: u32 },
    /// Drape the host's layer of this index.
    Imagery { layer: usize },
    /// Hold the loads not yet started, or let them go.
    Pause { paused: bool },
}

/// From the worker to its page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ToPage {
    /// The geometry server is running; the stream is open.
    Started,
    /// It will not: why, for the person.
    Failed { reason: String },
    /// A change of layer has resolved: the generation now draped, or why not.
    Switched {
        layer: usize,
        outcome: Result<u64, String>,
    },
}
