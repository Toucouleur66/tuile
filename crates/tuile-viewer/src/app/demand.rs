// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Drawing only what changed, and turning the loop only as often as needed.
//!
//! Two questions, one per turn of the loop.
//!
//! **How soon is the next turn?** At the display's rate while the view moves,
//! a finger is down, the indicator turns, tiles load, arrive or slide into
//! place; once all of that has been quiet for a moment, a slow wake-up. The
//! wake-up is not idle time thrown away: each one drains the stream, so a
//! tile that lands on a still camera is drawn within a quarter of a second,
//! and its upload makes the next turns busy again. Every device, the same
//! way: a desktop, a phone, a page.
//!
//! **Does this turn draw?** Only when the picture it would draw is not the
//! one on screen ([`Repaint`]). A picture is a function of its inputs — the
//! eyes, the scene's instant, the frame's size, what the pump would draw,
//! the overlays, the air and the view's settings — and two equal sets of
//! inputs draw the same picture. When nothing moved, nothing is encoded,
//! submitted or presented: the surface keeps the last image it was given,
//! and the GPU has nothing to do. A moving view still draws every frame:
//! continuous functions of the inputs, sampled each time one of them moves.
//!
//! Pure: what the turn saw in, a decision out, the clock passed in.

use std::time::Duration;
use web_time::Instant;

/// Quiet for this long before the rate drops: an ease coming to rest, a
/// finger lifting between two gestures.
const GRACE: Duration = Duration::from_millis(500);
/// How often a quiet session wakes to look for news.
pub(super) const IDLE_WAKE: Duration = Duration::from_millis(250);

/// What a frame saw that needs further frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Busy {
    /// The eased camera has not reached where the gestures put it.
    pub moving: bool,
    /// A finger is on the glass.
    pub touching: bool,
    /// The loading indicator is on screen, and turning.
    pub indicator: bool,
    /// Tiles reached the GPU this frame.
    pub uploaded: usize,
    /// Tiles decoded and waiting for a frame's upload budget.
    pub pending_uploads: usize,
    /// Tiles still sliding to a new edge where they meet a neighbour.
    pub sliding: usize,
    /// Loads under way: what lands is shown as it lands, not on the next
    /// wake-up.
    pub loading: bool,
    /// A recording that keeps a picture of every frame.
    pub recording: bool,
}

impl Busy {
    fn any(&self) -> bool {
        self.moving
            || self.touching
            || self.indicator
            || self.uploaded > 0
            || self.pending_uploads > 0
            || self.sliding > 0
            || self.loading
            || self.recording
    }
}

/// How often to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pace {
    /// As often as the device allows.
    Continuous,
    /// Once every [`IDLE_WAKE`].
    Idle,
}

/// A start-up taking longer than this is waiting on a slow or absent network.
const PATIENT_AFTER: Duration = Duration::from_secs(3);
/// The frame interval while it waits: the indicator still turns, at a rate
/// that costs little.
const WHILE_WAITING: Duration = Duration::from_nanos(1_000_000_000 / 15);

/// The frame interval for `interval`, given how long the session has been
/// waiting to start (`None` once it is up). A start-up that hangs on a dead
/// network must not draw sixty frames a second of a spinner.
pub(super) fn while_waiting(interval: Duration, waited: Option<Duration>) -> Duration {
    match waited {
        Some(waited) if waited >= PATIENT_AFTER => interval.max(WHILE_WAITING),
        _ => interval,
    }
}

#[derive(Debug, Default)]
pub(super) struct Demand {
    /// Since when nothing has needed a frame; `None` while something does.
    quiet_since: Option<Instant>,
}

impl Demand {
    /// Notes what a frame saw.
    pub(super) fn saw(&mut self, now: Instant, busy: Busy) {
        if busy.any() {
            self.quiet_since = None;
        } else {
            self.quiet_since.get_or_insert(now);
        }
    }

    /// Something happened between frames — a touch, a command, the
    /// application coming back — and wants to be seen at once.
    pub(super) fn poke(&mut self) {
        self.quiet_since = None;
    }

    pub(super) fn pace(&self, now: Instant) -> Pace {
        match self.quiet_since {
            Some(since) if now.duration_since(since) >= GRACE => Pace::Idle,
            _ => Pace::Continuous,
        }
    }
}

/// The time to the next turn, from the last frame's start: `full` — the
/// device's frame interval — while something is busy, and never less than
/// one refresh when the last turn presented nothing, since then no present
/// waits for the display and the loop would spin; [`IDLE_WAKE`] once quiet.
pub(super) fn turn_interval(
    pace: Pace,
    full: Duration,
    presented: bool,
    refresh: Duration,
) -> Duration {
    match pace {
        Pace::Continuous if presented => full,
        Pace::Continuous => full.max(refresh),
        Pace::Idle => IDLE_WAKE,
    }
}

/// How far the sky turns, radians per second of the scene's clock: the
/// Earth's rotation, which carries the sun, the Moon and the stars across the
/// picture together.
const SKY_RADIANS_PER_SECOND: f64 = 7.292_115e-5;

/// A turn of the sky smaller than this since the picture on screen was drawn
/// is not drawn: a tenth of a milliradian. At 2400 × 1436 pixels over sixty
/// degrees a star moves by a seventh of a pixel, and a slope's shading by
/// three hundredths of one of 255 levels. At the real clock's rate that is
/// one picture every 1.4 s; at a timelapse's, every frame.
const SKY_STILL_RADIANS: f64 = 1.0e-4;

/// An eye moved by less than this since the picture on screen was drawn has
/// not moved: a tenth of a millimetre — a seventh of a pixel a metre away.
/// The ease never quite arrives, and its last micrometres are not worth a
/// frame each.
const EYE_STILL_METRES: f64 = 1.0e-4;

/// Nor has a view turned by less than this, as one minus the cosine of the
/// angle: about 1.4 microradians, a five-hundredth of a pixel.
const TURN_STILL: f64 = 1.0e-12;

/// Everything a picture is drawn from beside the eyes and the instant, each
/// compared exactly: a change in any of them is another picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Settings {
    /// The frame, in device pixels, and the picture inside it (a host's
    /// letterbox).
    pub size: (u32, u32),
    pub picture: (u32, u32, u32, u32),
    /// The eye's height over the ground: where the near plane is, and how
    /// thick the air below.
    pub clearance: f64,
    /// [`tuile_wgpu::ContentPump::revision`]: what is drawn, as uploaded,
    /// stitched and dressed.
    pub content: u64,
    /// What the 2-D overlays are made of: the controls, the credits, the
    /// labels, the budget panel, the indicator (`App::upload_the_control`).
    pub overlays: u64,
    /// The host's shapes in the world, as last set or rebased.
    pub shapes: u64,
    /// The air and its light, as the host and the checkbox leave them.
    pub atmosphere: tuile_wgpu::Atmosphere,
    pub air: tuile_atmosphere::AerialAir,
    pub air_shown: bool,
    /// The diagnostic view, the labels forced and the wireframe.
    pub view: (usize, bool, bool),
}

/// What one picture is drawn from.
#[derive(Debug, Clone)]
pub(super) struct Picture {
    /// Each eye's camera.
    pub eyes: Vec<tuile_camera::GlobeCamera>,
    /// The scene's instant, Unix seconds: where the sun, the Moon and the
    /// stars stand.
    pub instant: f64,
    pub settings: Settings,
}

impl Picture {
    /// Whether this picture would look like `shown`: the same settings, eyes
    /// within a seventh of a pixel, and a sky turned by less than one.
    fn looks_like(&self, shown: &Picture) -> bool {
        self.settings == shown.settings
            && self.eyes.len() == shown.eyes.len()
            && self
                .eyes
                .iter()
                .zip(&shown.eyes)
                .all(|(a, b)| same_place(a, b))
            && ((self.instant - shown.instant).abs() * SKY_RADIANS_PER_SECOND) < SKY_STILL_RADIANS
    }
}

fn same_place(a: &tuile_camera::GlobeCamera, b: &tuile_camera::GlobeCamera) -> bool {
    a.fovy == b.fovy
        && a.position.distance(b.position) < EYE_STILL_METRES
        && 1.0
            - a.direction
                .normalize_or_zero()
                .dot(b.direction.normalize_or_zero())
            < TURN_STILL
        && 1.0 - a.up.normalize_or_zero().dot(b.up.normalize_or_zero()) < TURN_STILL
}

/// What is on screen, and whether the next frame must be drawn whatever it
/// shows.
#[derive(Debug, Default)]
pub(super) struct Repaint {
    /// The picture on screen; `None` before the first, and after anything
    /// that may have taken it away.
    shown: Option<Picture>,
}

impl Repaint {
    /// Whether `picture` must be drawn: it is not the one on screen.
    pub(super) fn due(&self, picture: &Picture) -> bool {
        self.shown
            .as_ref()
            .is_none_or(|shown| !picture.looks_like(shown))
    }

    /// `picture` reached the screen.
    pub(super) fn shown(&mut self, picture: Picture) {
        self.shown = Some(picture);
    }

    /// What is on screen can no longer be trusted to be there — a surface
    /// made again, resized, uncovered: the next frame is drawn.
    pub(super) fn forget(&mut self) {
        self.shown = None;
    }
}

/// How often the server is told the camera when it has not moved: the
/// budget's clocks — a load's timeout, a retry's wait, its tiers measured
/// once a second — are read on its steps, and a still camera asks for no
/// other step once nothing is in flight. Its timeouts are seconds long.
pub(super) const VIEW_HEARTBEAT: Duration = Duration::from_secs(1);

/// The views last sent to the server, and when.
#[derive(Debug, Default)]
pub(super) struct ViewSends {
    sent: Vec<tuile_core::traversal::ViewState>,
    at: Option<Instant>,
}

impl ViewSends {
    /// Whether `views` go to the server this turn: when they moved, while
    /// anything loads — its steps are then as many as before — and once a
    /// [`VIEW_HEARTBEAT`] otherwise.
    pub(super) fn due(
        &self,
        views: &[tuile_core::traversal::ViewState],
        loading: bool,
        now: Instant,
    ) -> bool {
        loading
            || self
                .at
                .is_none_or(|at| now.duration_since(at) >= VIEW_HEARTBEAT)
            || views.len() != self.sent.len()
            || views.iter().zip(&self.sent).any(|(a, b)| !a.same_as(b))
    }

    /// `views` were sent at `now`.
    pub(super) fn sent(&mut self, views: &[tuile_core::traversal::ViewState], now: Instant) {
        self.sent.clear();
        self.sent.extend_from_slice(views);
        self.at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A launch gets full rate for its first seconds, then fifteen frames a
    /// second for as long as it hangs; a running session is never held back.
    #[test]
    fn a_long_wait_to_start_is_drawn_slowly() {
        let full = Duration::from_secs(1) / 60;
        assert_eq!(while_waiting(full, None), full);
        assert_eq!(while_waiting(full, Some(Duration::from_secs(1))), full);
        assert_eq!(
            while_waiting(full, Some(Duration::from_secs(4))),
            WHILE_WAITING
        );
        // Already slower (idle, or hot): not made faster.
        let idle = IDLE_WAKE;
        assert_eq!(while_waiting(idle, Some(Duration::from_secs(9))), idle);
    }

    #[test]
    fn a_quiet_session_slows_down_after_a_grace_and_wakes_on_news() {
        let t0 = Instant::now();
        let mut d = Demand::default();
        assert_eq!(d.pace(t0), Pace::Continuous, "a new session draws");

        d.saw(t0, Busy::default());
        assert_eq!(d.pace(t0 + Duration::from_millis(100)), Pace::Continuous);
        let t1 = t0 + GRACE;
        assert_eq!(d.pace(t1), Pace::Idle);
        // Staying quiet does not restart the grace.
        d.saw(t1, Busy::default());
        assert_eq!(d.pace(t1), Pace::Idle);

        // Each reason alone is enough to draw at full rate again.
        for busy in [
            Busy {
                moving: true,
                ..Busy::default()
            },
            Busy {
                touching: true,
                ..Busy::default()
            },
            Busy {
                indicator: true,
                ..Busy::default()
            },
            Busy {
                uploaded: 1,
                ..Busy::default()
            },
            Busy {
                pending_uploads: 3,
                ..Busy::default()
            },
            Busy {
                sliding: 2,
                ..Busy::default()
            },
            Busy {
                loading: true,
                ..Busy::default()
            },
            Busy {
                recording: true,
                ..Busy::default()
            },
        ] {
            let mut d = Demand::default();
            d.saw(t0, Busy::default());
            assert_eq!(d.pace(t1), Pace::Idle);
            d.saw(t1, busy);
            assert_eq!(d.pace(t1), Pace::Continuous, "{busy:?}");
        }

        // A poke between frames does the same.
        d.poke();
        assert_eq!(d.pace(t1), Pace::Continuous);
    }

    fn camera() -> tuile_camera::GlobeCamera {
        let earth = 6_378_137.0;
        tuile_camera::GlobeCamera::look_at(
            glam::DVec3::new(earth + 2_000.0, 0.0, 0.0),
            glam::DVec3::new(earth, 1_000.0, 0.0),
            glam::DVec3::X,
            0.8,
        )
    }

    fn picture() -> Picture {
        let atmosphere = tuile_wgpu::Atmosphere::new(
            tuile_wgpu::AtmosphereConfig::tuned(
                &tuile_atmosphere::AirTuning::default(),
                tuile_atmosphere::ClockSettings::default(),
            ),
            1.7e9,
            camera().position,
        );
        Picture {
            eyes: vec![camera()],
            instant: 1.7e9,
            settings: Settings {
                size: (2400, 1436),
                picture: (0, 0, 2400, 1436),
                clearance: 2_000.0,
                content: 7,
                overlays: 3,
                shapes: 0,
                atmosphere,
                air: tuile_atmosphere::AirTuning::default().viewer,
                air_shown: true,
                view: (0, false, false),
            },
        }
    }

    /// A still view is drawn once and then not again: the same picture,
    /// turn after turn, with the clock running at its real rate, asks for
    /// no frame.
    #[test]
    fn a_still_view_is_drawn_once() {
        let mut repaint = Repaint::default();
        let mut now = picture();
        assert!(repaint.due(&now), "the first picture is drawn");
        repaint.shown(now.clone());
        for frame in 1..60 {
            // A second of the real clock, sixty turns of it.
            now.instant = 1.7e9 + f64::from(frame) / 60.0;
            assert!(!repaint.due(&now), "turn {frame} drew the same picture");
        }
        // The ease's last micrometres are not a picture.
        now.eyes[0].position.y += 2.0e-5;
        assert!(!repaint.due(&now));
    }

    /// Each input alone is another picture: the camera, the frame, what is
    /// drawn, the overlays, the air, the view — and the sky once it has
    /// turned by a tenth of a milliradian.
    #[test]
    fn every_input_that_moves_draws_a_frame() {
        let mut repaint = Repaint::default();
        let shown = picture();
        repaint.shown(shown.clone());
        type Change = (&'static str, Box<dyn Fn(&mut Picture)>);
        let changes: Vec<Change> = vec![
            (
                "eye moved a millimetre",
                Box::new(|p| p.eyes[0].position.y += 1.0e-3),
            ),
            (
                "view turned",
                Box::new(|p| {
                    p.eyes[0].direction =
                        (p.eyes[0].direction + glam::DVec3::new(0.0, 0.0, 1.0e-5)).normalize()
                }),
            ),
            ("zoomed", Box::new(|p| p.eyes[0].fovy *= 0.99)),
            ("a second eye", Box::new(|p| p.eyes.push(camera()))),
            ("resized", Box::new(|p| p.settings.size.0 += 1)),
            ("letterboxed", Box::new(|p| p.settings.picture.1 += 1)),
            (
                "the ground rose",
                Box::new(|p| p.settings.clearance -= 0.01),
            ),
            ("a tile landed", Box::new(|p| p.settings.content += 1)),
            ("a control lit", Box::new(|p| p.settings.overlays += 1)),
            ("a host's shape", Box::new(|p| p.settings.shapes += 1)),
            (
                "the air switched off",
                Box::new(|p| p.settings.air_shown = false),
            ),
            (
                "the atmosphere off",
                Box::new(|p| p.settings.atmosphere.enabled = !p.settings.atmosphere.enabled),
            ),
            ("a diagnostic view", Box::new(|p| p.settings.view.0 = 1)),
            ("the labels", Box::new(|p| p.settings.view.1 = true)),
            ("the wireframe", Box::new(|p| p.settings.view.2 = true)),
            // 1e-4 rad of the Earth's turn: 1.37 s of the real clock.
            ("the sky turned", Box::new(|p| p.instant += 1.4)),
            ("the clock was set back", Box::new(|p| p.instant -= 3_600.0)),
        ];
        for (what, change) in changes {
            let mut now = shown.clone();
            change(&mut now);
            assert!(repaint.due(&now), "{what}: no frame drawn");
        }
    }

    /// What is on screen may be gone — a surface made again, a resize, a
    /// window uncovered: the next picture is drawn whatever it is.
    #[test]
    fn a_forgotten_picture_is_drawn_again() {
        let mut repaint = Repaint::default();
        repaint.shown(picture());
        assert!(!repaint.due(&picture()));
        repaint.forget();
        assert!(repaint.due(&picture()));
    }

    /// Slow motion accumulates: a drift too small to draw on one turn is
    /// drawn once it adds up to something, because it is measured from the
    /// picture on screen, not from the turn before.
    #[test]
    fn a_slow_drift_is_drawn_once_it_adds_up() {
        let mut repaint = Repaint::default();
        let shown = picture();
        repaint.shown(shown.clone());
        let mut now = shown;
        let mut drawn_at = None;
        for turn in 1..=20 {
            now.eyes[0].position.y += 3.0e-5;
            if repaint.due(&now) {
                drawn_at = Some(turn);
                break;
            }
        }
        assert_eq!(
            drawn_at,
            Some(4),
            "30 µm a step adds up to a tenth of a millimetre at the fourth"
        );
    }

    /// The loop never spins: a busy turn that presented nothing waits one
    /// refresh, since no present waited for the display.
    #[test]
    fn a_busy_turn_without_a_present_waits_a_refresh() {
        let refresh = Duration::from_secs(1) / 60;
        assert_eq!(
            turn_interval(Pace::Continuous, Duration::ZERO, true, refresh),
            Duration::ZERO,
            "a present paces the loop itself"
        );
        assert_eq!(
            turn_interval(Pace::Continuous, Duration::ZERO, false, refresh),
            refresh
        );
        // A device held back by its heat keeps its own, longer interval.
        let hot = Duration::from_secs(1) / 30;
        assert_eq!(turn_interval(Pace::Continuous, hot, false, refresh), hot);
        assert_eq!(
            turn_interval(Pace::Idle, Duration::ZERO, true, refresh),
            IDLE_WAKE
        );
    }

    /// A still camera is not sent to the server on every turn — once a
    /// heartbeat while nothing loads; every turn while something does, or
    /// when it moved.
    #[test]
    fn a_still_camera_is_sent_once_a_heartbeat() {
        let t0 = Instant::now();
        let view = |y: f64| {
            let mut c = camera();
            c.position.y += y;
            c.view_state(glam::DVec2::new(2400.0, 1436.0))
                .with_scale(2.0)
        };
        let mut sends = ViewSends::default();
        let still = [view(0.0)];
        assert!(sends.due(&still, false, t0), "the first view is sent");
        sends.sent(&still, t0);
        let soon = t0 + Duration::from_millis(16);
        assert!(!sends.due(&still, false, soon), "a still view sent again");
        assert!(sends.due(&still, true, soon), "loads under way: every turn");
        assert!(sends.due(&[view(1.0e-9)], false, soon), "a moved view");
        assert!(
            sends.due(&[view(0.0), view(0.0)], false, soon),
            "a second eye"
        );
        assert!(
            sends.due(&still, false, t0 + VIEW_HEARTBEAT),
            "the heartbeat"
        );
    }
}
