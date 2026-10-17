// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The window's side of what a phone adds: fingers on the glass, the
//! application leaving and coming back, the loading indicator, and drawing
//! only when there is something to draw.
//!
//! The decisions are made elsewhere, in modules with no window in them —
//! [`super::touch`], [`super::lifecycle`], [`super::loading`],
//! [`super::demand`] — and tested there. What is here carries them out.

use std::time::Instant;

use tuile_core::protocol::{ClientMessage, GeometryStream};
use winit::event::TouchPhase;
use winit::event_loop::{ActiveEventLoop, ControlFlow};

use super::demand::{Busy, Pace, IDLE_WAKE};
use super::lifecycle::Step;
use super::loading::{Observed, Style};
use super::touch::{Gesture, Phase};
use super::App;
use crate::steer::Command;

/// How far one double tap zooms, in the units the wheel and the zoom buttons
/// use.
const DOUBLE_TAP_ZOOM: f64 = 4.0;
/// What one unit of [`tuile_camera::CameraController::zoom`] scales the
/// distance to the ground by, as a natural logarithm: `exp(-0.15 · delta)`.
const ZOOM_LOG_PER_UNIT: f64 = 0.15;
/// The launch indicator's radius, and the small one's, in control units.
const LAUNCH_RADIUS: f64 = 22.0;
const REFINING_RADIUS: f64 = 9.0;
/// How long one turn of the indicator takes.
const TURN_SECONDS: f64 = 1.0;
/// How often the device's temperature and power saving are read.
const READ_THE_DEVICE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

impl App {
    /// One finger event from the window.
    pub(super) fn on_touch(&mut self, touch: winit::event::Touch) {
        let phase = match touch.phase {
            TouchPhase::Started => Phase::Started,
            TouchPhase::Moved => Phase::Moved,
            TouchPhase::Ended => Phase::Ended,
            TouchPhase::Cancelled => Phase::Cancelled,
        };
        let at = (touch.location.x, touch.location.y);
        for gesture in self.touches.on(Instant::now(), touch.id, phase, at) {
            self.gesture(gesture);
        }
        self.demand.poke();
    }

    /// Carries a gesture out on the camera, the control getting first refusal
    /// on a press, exactly as it does for the mouse.
    pub(super) fn gesture(&mut self, gesture: Gesture) {
        let viewport = self.viewport();
        let ui = self.ui_frame();
        match gesture {
            Gesture::Press(at) => {
                self.pointer.cursor = at;
                self.pointer.dragging = !self.nav.press(ui.to(at), ui.size, &mut self.controller);
            }
            Gesture::Drag { from, to } => {
                if self
                    .nav
                    .drag(ui.to(from), ui.to(to), ui.size, &mut self.controller)
                {
                    // The control owns this gesture.
                } else if self.pointer.dragging {
                    self.controller.drag(from, to, viewport);
                }
                self.pointer.cursor = to;
            }
            Gesture::Release => {
                self.pointer.dragging = false;
                if self.nav.release() == Some(tuile_ui::Part::Ring) {
                    self.apply(Command::NorthUp);
                }
            }
            Gesture::Pinch { centre, ratio } => {
                // Fingers twice as far apart bring the ground twice as close.
                self.controller
                    .zoom(ratio.ln() / ZOOM_LOG_PER_UNIT, centre, viewport);
            }
            // Clockwise fingers turn the map clockwise, as the compass ring
            // does when dragged clockwise.
            Gesture::Twist { radians } => self.controller.rotate_heading(radians, viewport),
            Gesture::Tilt { radians } => self.controller.tilt(radians / ui.scale, viewport),
            Gesture::DoubleTap(at) => self.controller.zoom(DOUBLE_TAP_ZOOM, at, viewport),
            Gesture::Hold(_) => self.apply(Command::Here),
        }
    }

    /// Carries out what a change of phase asks for.
    pub(super) fn carry_out(&mut self, steps: Vec<Step>) {
        for step in steps {
            tracing::info!(?step, phase = ?self.lifecycle.phase(), "lifecycle");
            match step {
                Step::PauseLoads => {
                    if let Some(session) = self.session.as_ref() {
                        session.gate.shut();
                    }
                }
                Step::ResumeLoads => {
                    if let Some(session) = self.session.as_ref() {
                        session.gate.open();
                    }
                }
                Step::Trim(budget_bytes) => {
                    if let Some(session) = self.session.as_mut() {
                        // Best effort: a closed stream means the session is over.
                        let _ = session.stream.send(ClientMessage::Trim { budget_bytes });
                    }
                }
                Step::ReleaseSurface => {
                    // The last moment the application is sure to run: a
                    // phone ends background applications without a word.
                    self.remember_the_view();
                    self.release_the_surface();
                }
                Step::RestoreSurface => self.restore_the_surface(),
                Step::Redraw => {
                    self.demand.poke();
                    if let Some(active) = self.active.as_ref() {
                        active.window.request_redraw();
                    }
                }
            }
        }
    }

    /// Decides this frame's loading indicator, from what the last frame saw.
    ///
    /// `None` on a desktop, whose window is held shut until the globe has a
    /// floor and so never needs one.
    pub(super) fn the_loading_indicator(&mut self) -> Option<tuile_ui::Spinner> {
        if self.profile.hold_window_until_warm {
            return None;
        }
        let now = Instant::now();
        let ready = self.session.is_some();
        // Asked only until the launch is over: it drains the server's errors
        // and reports, which the status line does after that.
        let warm = ready && (self.loading.launched() || !self.still_warming());
        let progress = self.active.as_ref().and_then(|active| {
            let level = self.warmup.pinned_level?;
            let priming = active.pump.priming?;
            let expected = priming.expected().max(1);
            let held = active.pump.prepared_through_level(level) as u32;
            Some((held as f32 / expected as f32).min(1.0))
        });
        let resolution = self.resolution.unwrap_or_default();
        let observed = Observed {
            ready,
            warm,
            selected: resolution.exact + resolution.coarser + resolution.lost,
            lost: resolution.lost,
            coarser: resolution.coarser + resolution.stand_ins,
            loading: self
                .active
                .as_ref()
                .map_or(0, |a| a.pump.stats.requested as usize),
            progress,
            failed: self.failed.is_some(),
        };
        let was_launched = self.loading.launched();
        let indicator = self.loading.step(now, observed);
        if !was_launched && self.loading.launched() {
            let seconds = self
                .stats
                .started
                .map_or(0.0, |t| t.elapsed().as_secs_f64());
            tracing::info!(seconds, "first view covered; the indicator fades");
            crate::journal::note(&format!("first view covered in {seconds:.1} s"));
        }
        if indicator.opacity <= 0.0 {
            return None;
        }
        let ui = self.ui_frame();
        let turn = self
            .stats
            .started
            .map_or(0.0, |t| t.elapsed().as_secs_f64() / TURN_SECONDS);
        Some(match indicator.style {
            Style::Launch => tuile_ui::Spinner {
                centre: ui.centre(),
                radius: LAUNCH_RADIUS * ui.scale,
                turn,
                progress: indicator.progress,
                opacity: indicator.opacity,
            },
            // Top left of the safe area, clear of the compass in the bottom
            // right and of the caption along the bottom.
            Style::Refining => {
                let r = REFINING_RADIUS * ui.scale;
                tuile_ui::Spinner {
                    centre: (ui.origin.0 + 2.0 * r, ui.origin.1 + 2.0 * r),
                    radius: r,
                    turn,
                    progress: None,
                    opacity: indicator.opacity,
                }
            }
        })
    }

    /// Tells the pacing what this frame saw.
    pub(super) fn note_what_needs_frames(&mut self, indicator: Option<tuile_ui::Spinner>) {
        let target = *self.controller.target();
        let camera = self.controller.camera;
        let moving = (camera.position - target.position).length() > 1e-3
            || camera.direction.dot(target.direction) < 1.0 - 1e-12
            || camera.up.dot(target.up) < 1.0 - 1e-12;
        let busy = Busy {
            moving,
            touching: self.touches.any(),
            indicator: indicator.is_some(),
            uploaded: self.uploaded,
            pending_uploads: self.active.as_ref().map_or(0, |a| a.pump.pending_uploads()),
            sliding: self.active.as_ref().map_or(0, |a| a.pump.sliding()),
        };
        self.demand.saw(Instant::now(), busy);
    }

    /// A phone's turn of the loop: a frame when one is due, and otherwise a
    /// sleep until it is — never a spin.
    pub(super) fn pace_the_frames(&mut self, event_loop: &ActiveEventLoop) {
        if !self.lifecycle.may_draw() || self.active.is_none() {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        self.read_the_device(now);
        if let Some(held) = self.touches.tick(now) {
            self.gesture(held);
        }
        self.play_the_script(now);
        let interval = match self.demand.pace(now) {
            Pace::Continuous => self.profile.frame_interval(self.thermal, self.low_power),
            Pace::Idle => IDLE_WAKE,
        };
        let due = self.pacing.last_frame + interval;
        if now >= due {
            if let Some(active) = self.active.as_ref() {
                active.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(due));
        }
    }

    /// Plays what is due of `TUILE_TOUCH_SCRIPT`, through the same path as
    /// the window's touches. See [`super::script`].
    fn play_the_script(&mut self, now: Instant) {
        let covered = self.loading.launched();
        let size = self.viewport();
        let Some(script) = self.script.as_mut() else {
            return;
        };
        for (id, phase, at) in script.due(now, covered, size) {
            for gesture in self.touches.on(now, id, phase, at) {
                self.gesture(gesture);
            }
            self.demand.poke();
        }
    }

    /// Reads how hot the device is and whether it saves power, at most once a
    /// second, and says so when either changes.
    fn read_the_device(&mut self, now: Instant) {
        if self
            .device_read
            .is_some_and(|at| now.duration_since(at) < READ_THE_DEVICE_EVERY)
        {
            return;
        }
        self.device_read = Some(now);
        let (thermal, low_power) = (crate::device::thermal(), crate::device::low_power());
        if (thermal, low_power) != (self.thermal, self.low_power) {
            tracing::info!(
                ?thermal,
                low_power,
                frame_ms = self
                    .profile
                    .frame_interval(thermal, low_power)
                    .as_secs_f64()
                    * 1000.0,
                "device state"
            );
            self.thermal = thermal;
            self.low_power = low_power;
        }
    }
}
