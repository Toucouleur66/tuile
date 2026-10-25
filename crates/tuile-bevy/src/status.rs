// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the globe tells the host, and the light the host gives it.

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use glam::DVec3;
use tuile_wgpu::{ContentPump, Resolution};

/// How the session stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SessionState {
    /// The sources are being resolved; the backdrop is drawn meanwhile.
    #[default]
    Starting,
    Running,
    /// It could not start, and why, in a sentence for a person.
    Failed(String),
}

/// The globe as it was last drawn: read it from the main world, a frame or
/// two behind the render world that fills it.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct TuileStatus {
    pub session: SessionState,
    /// Tiles drawn as themselves.
    pub exact: usize,
    /// Wanted tiles drawn by a coarser ancestor, or by finer tiles held on a
    /// zoom-out, while their own stream in.
    pub coarser: usize,
    /// Wanted tiles drawn by nothing — ground left to the backdrop. Zero is
    /// the rule; anything else is a fault of this side.
    pub lost: usize,
    /// Drawn tiles missing a drape they need.
    pub bare: usize,
    /// Whether the last draw set was complete in the batch sense: every
    /// wanted tile exact (a bulk render's gate; an interactive view need not
    /// reach it).
    pub frame_ready: bool,
    /// Whether the session has nothing more to load for the current view.
    pub converged: bool,
    /// Bytes held on the GPU, every class together.
    pub gpu_bytes: u64,
    /// Tiles still sliding onto a neighbour's edge after what is drawn
    /// beside them changed.
    pub sliding: usize,
    /// Frames the globe was drawn in.
    pub frames: u64,
}

impl TuileStatus {
    pub(crate) fn record(&mut self, resolution: &Resolution, pump: PumpFacts) {
        self.exact = resolution.exact;
        self.coarser = resolution.coarser;
        self.lost = resolution.lost;
        self.bare = resolution.bare;
        self.frame_ready = pump.frame_ready;
        self.converged = pump.converged;
        self.gpu_bytes = pump.gpu_bytes;
        self.sliding = pump.sliding;
        self.frames += 1;
    }

    /// Settled: running, converged, every wanted tile drawn as itself, and
    /// every edge where its neighbours have it.
    pub fn is_settled(&self) -> bool {
        self.session == SessionState::Running
            && self.converged
            && self.sliding == 0
            && self.lost == 0
            && self.coarser == 0
    }
}

/// What the pump said of its last step, read before a draw borrows it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PumpFacts {
    frame_ready: bool,
    converged: bool,
    gpu_bytes: u64,
    sliding: usize,
}

impl PumpFacts {
    pub(crate) fn of(pump: &ContentPump) -> Self {
        Self {
            frame_ready: pump.frame_ready,
            converged: pump.converged,
            gpu_bytes: pump.held_bytes().0.iter().sum(),
            sliding: pump.sliding(),
        }
    }
}

/// The status as the render world writes it.
#[derive(Resource, Clone, Default)]
pub(crate) struct SharedStatus(pub Arc<Mutex<TuileStatus>>);

pub(crate) fn publish_status(shared: Res<SharedStatus>, mut status: ResMut<TuileStatus>) {
    let Ok(now) = shared.0.lock() else {
        return;
    };
    if *status != *now {
        *status = now.clone();
    }
}

/// The light on the ground.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct TuileLight {
    /// The direction sunlight travels, ECEF, or `None` for a sun straight
    /// above the camera wherever it is.
    pub sun: Option<DVec3>,
    /// The share of light that reaches a slope facing away from the sun.
    pub ambient: f32,
}

impl Default for TuileLight {
    fn default() -> Self {
        Self {
            sun: None,
            ambient: 0.5,
        }
    }
}

impl TuileLight {
    /// Which way the light travels, seen from `eye`.
    pub(crate) fn travel(&self, eye: DVec3) -> DVec3 {
        self.sun
            .unwrap_or_else(|| -eye)
            .try_normalize()
            .unwrap_or(DVec3::NEG_Z)
    }
}
