// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The session log, and the sources it writes down.
//!
//! One MCAP file a run, on a tape's timeline (`tuile-tape`): the camera on
//! `/camera`, each step's state on `/state`, every [`Observer`] on its own
//! topic and, as [`Pictures`] says, the eyes' pictures. What it is for is a
//! run that went wrong: the frames that broke an obligation, the numbers
//! around them, and the camera path to fly again (`TUILE_REPLAY`, or
//! `Headless::fly`) until the fault is found.

use std::path::{Path, PathBuf};

use super::State;
use crate::app::App;

/// A source of readings, one per step: a topic in the session log beside
/// `/state`, and a key in every [`State::observed`].
///
/// What the run reads of itself — the engine's published numbers, the
/// pictures — is in the state already; an observer is for what something
/// else knows. The budget's observer is one: when the budget drives the
/// frame, it is added here and nothing about the harness changes.
pub trait Observer {
    /// Where its readings go: an MCAP topic, `/budget` say.
    fn topic(&self) -> &str;

    /// One step's reading, given what the run read of the step.
    fn observe(&mut self, state: &State) -> serde_json::Value;
}

/// Which of the eyes' pictures a session log keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pictures {
    /// None: the camera and the numbers only.
    Never,
    /// The frames that broke what every frame is held to
    /// ([`State::wrong`]): the ones a red run is opened to look at.
    WhenWrong,
    /// Every frame drawn.
    Always,
}

/// The schema `/state` is declared with.
const STATE_SCHEMA: &str = "tuile.HeadlessState";
const STATE_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "description": "one step of a headless run: see tuile_viewer::control::State",
  "properties": {
    "frame":    {"type": "integer"},
    "seconds":  {"type": "number"},
    "took":     {"type": "number"},
    "drew":     {"type": "boolean"},
    "view":     {"type": "object"},
    "size":     {"type": "array", "items": {"type": "integer"}},
    "scale":    {"type": "number"},
    "eyes":     {"type": "array", "items": {"type": "object"}},
    "metrics":  {"type": "object", "description": "tuile_core::metrics::Snapshot since the run began"},
    "observed": {"type": "object"}
  },
  "required": ["frame", "seconds", "drew", "view", "metrics"]
}"#;

/// An observer's topic is declared with this: its documents are its own.
const OBSERVED_SCHEMA: &str = "tuile.Observed";
const OBSERVED_JSON_SCHEMA: &str = r#"{"type": "object"}"#;

/// A run being written down.
pub(super) struct SessionLog {
    tape: tuile_tape::Tape,
    path: PathBuf,
    pictures: Pictures,
    eyes: usize,
}

impl SessionLog {
    pub(super) fn open(path: &Path, pictures: Pictures, eyes: usize) -> Result<Self, String> {
        let tape = tuile_tape::Tape::recording(path).map_err(|e| e.to_string())?;
        Ok(Self {
            tape,
            path: path.to_path_buf(),
            pictures,
            eyes,
        })
    }

    /// One step: its camera, its state, what each observer read, and the
    /// pictures it keeps.
    pub(super) fn write(&mut self, state: &State, app: &App) {
        self.tape
            .push(crate::app::offscreen::frame_of(&app.camera()));
        self.tape.push_json(
            "/state",
            STATE_SCHEMA,
            STATE_JSON_SCHEMA,
            &state.to_json().to_string(),
        );
        for (topic, reading) in &state.observed {
            self.tape.push_json(
                topic,
                OBSERVED_SCHEMA,
                OBSERVED_JSON_SCHEMA,
                &reading.to_string(),
            );
        }
        let keep = match self.pictures {
            Pictures::Never => false,
            Pictures::WhenWrong => state.wrong(),
            Pictures::Always => state.drew,
        };
        if !keep {
            return;
        }
        for eye in 0..self.eyes {
            let Some((pixels, (width, height))) = app.eye_pixels(eye) else {
                continue;
            };
            if self.eyes == 1 {
                // Where a trace keeps its frames: `tuile-scan-black` reads
                // this log as it reads one.
                self.tape.push_image(pixels, width, height);
            } else {
                self.tape
                    .push_image_on(&format!("/eye/{eye}/frame"), pixels, width, height);
            }
        }
    }

    /// Closes the file; says where it is.
    pub(super) fn finish(self) -> Result<PathBuf, String> {
        self.tape.finish().map_err(|e| e.to_string())?;
        Ok(self.path)
    }
}
