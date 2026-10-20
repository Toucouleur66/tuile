// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What an application embedding the viewer may do with each frame before it
//! is traversed and drawn.
//!
//! [`ViewerHost`](crate::embed::ViewerHost) says where a session's *data*
//! comes from, once, before the window exists. This says what happens *every
//! frame*, and it is the other half: a host that owns the camera — a replay, a
//! simulator, a scripted flight — needs to put the eye exactly where its own
//! model says, and needs the globe to stream for that eye rather than for the
//! one a person last dragged.
//!
//! # Driving versus placing
//!
//! The viewer's [`CameraController`](tuile_camera::CameraController) exists to
//! make a person's gestures feel right: it eases toward what the gesture asked
//! for, and it holds the eye above the relief
//! ([`min_altitude`](tuile_camera::CameraController::min_altitude)) so a drag
//! cannot push it inside a ridge. Both are wrong for a host that has already
//! decided where the eye goes:
//!
//! * easing means the drawn pose is not the asked-for pose but somewhere
//!   behind it, by an amount that depends on the frame time;
//! * the floor means a pose the host computed against *its* ground model is
//!   silently raised by the viewer's.
//!
//! So there are two doors, and the difference is the whole contract:
//!
//! * [`HostCamera::drive`] — this frame is traversed and drawn from exactly
//!   this pose. No easing, no clamp, no input. What the host asks for is what
//!   the eye sees, bit for bit.
//! * [`HostCamera::place`] — move the controller's camera there and let it
//!   behave normally from that point: a person can carry on dragging, the
//!   easing and the floor apply again.
//!
//! [`HostCamera::release`] hands a driven camera back to the controller,
//! continuing from the last driven pose rather than jumping back to wherever
//! the controller had been left.
//!
//! # Preload views
//!
//! A host that is about to move the camera somewhere knows it before the
//! viewer does. [`HostFrame::preload`] takes poses that are **traversed but
//! never drawn**: the tiles they need are asked for now, so the move does not
//! arrive on an empty globe. They go through the same eyes/union mechanism a
//! stereo pair uses, which is why they cost traversal and not a second frame.
//!
//! # One contract, two shells
//!
//! The same trait is handed to the native embedding and to the web start, so a
//! host is written once. On the web it is **not** required to be `Send` or
//! `Sync`: a web host holds UI signals that are neither, and the page's event
//! loop is single-threaded anyway.

use tuile_camera::GlobeCamera;

/// A camera pose: ECEF metres, a unit direction and up, and a vertical field
/// of view in radians — all `f64`.
///
/// This is the viewer's own camera type rather than a new one, so a pose
/// handed in is the pose the renderer uses with no conversion to go wrong.
pub type Pose = GlobeCamera;

/// Whether the host dealt with an event, or the viewer should.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumed {
    /// The host handled it. The viewer's own controls never see it.
    Yes,
    /// Not the host's; the viewer handles it as it always has.
    No,
}

impl Consumed {
    /// `Yes` when `consumed`, so a host can answer with a predicate.
    pub fn when(consumed: bool) -> Self {
        if consumed {
            Self::Yes
        } else {
            Self::No
        }
    }

    pub fn is_consumed(self) -> bool {
        self == Self::Yes
    }
}

/// How soon a preload view's tiles are wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Wanted as much as what is on screen: the camera is about to be there.
    Now,
    /// Wanted, but behind the drawn view.
    Later,
}

/// A pose traversed but never drawn, so its tiles are on hand before the
/// camera arrives.
#[derive(Debug, Clone, Copy)]
pub struct PreloadView {
    pub pose: Pose,
    /// Width over height of the view this pose will be drawn at. The drawn
    /// viewport's aspect when the host has no better answer.
    pub aspect: f64,
    pub priority: Priority,
}

impl PreloadView {
    /// A view wanted as soon as possible, at `aspect`.
    pub fn now(pose: Pose, aspect: f64) -> Self {
        Self {
            pose,
            aspect,
            priority: Priority::Now,
        }
    }
}

/// The camera, for the frame being prepared.
///
/// Reading is always allowed. Writing chooses between the two doors described
/// at the module level.
#[derive(Debug)]
pub struct HostCamera {
    /// The pose the last frame was drawn from.
    drawn: Pose,
    /// What the host asked for this frame, and which door it used.
    asked: Option<Asked>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Asked {
    /// Exactly this pose, no clamp and no easing.
    Drive(Pose),
    /// The controller's camera moves here and carries on normally.
    Place(Pose),
    /// The controller takes over from the last driven pose.
    Release,
}

impl HostCamera {
    /// For the frame about to be prepared, drawn last from `drawn`.
    pub(crate) fn new(drawn: Pose) -> Self {
        Self { drawn, asked: None }
    }

    /// The camera the last frame was drawn from.
    ///
    /// Not what this frame will use: a host that has already called
    /// [`Self::drive`] this frame still reads the previous pose here, because
    /// this frame has not been drawn yet.
    pub fn get(&self) -> Pose {
        self.drawn
    }

    /// This frame is traversed and drawn from exactly `pose`.
    ///
    /// No easing, no minimum altitude, and no gesture the viewer may have
    /// received. The next call to [`Self::get`] returns it, because by then it
    /// is what was drawn.
    pub fn drive(&mut self, pose: Pose) {
        self.asked = Some(Asked::Drive(pose));
    }

    /// Moves the controller's camera to `pose` and lets it behave normally
    /// from there: easing, the altitude floor and a person's gestures all
    /// apply again.
    pub fn place(&mut self, pose: Pose) {
        self.asked = Some(Asked::Place(pose));
    }

    /// Gives the camera back to the controller, continuing from the last
    /// driven pose.
    pub fn release(&mut self) {
        self.asked = Some(Asked::Release);
    }

    /// What the host asked for, for the shell to apply.
    pub(crate) fn taken(&self) -> Option<Asked> {
        self.asked
    }
}

/// What a host may read and change about the frame being prepared.
pub struct HostFrame<'a> {
    pub camera: &'a mut HostCamera,
    /// Poses to traverse without drawing. Replaced wholesale each frame the
    /// host touches it: what is not asked for again is no longer wanted.
    pub preload: &'a mut Vec<PreloadView>,
    /// The drawn frame's size in device pixels.
    pub viewport: (u32, u32),
}

impl HostFrame<'_> {
    /// Width over height of the drawn frame, for a preload view that will be
    /// drawn at the same shape.
    pub fn aspect(&self) -> f64 {
        let (w, h) = self.viewport;
        f64::from(w.max(1)) / f64::from(h.max(1))
    }
}

/// An application that owns what the viewer draws, frame by frame.
///
/// On the web this is required to be neither `Send` nor `Sync`: a page's host
/// holds UI signals that are neither, and its event loop is single-threaded.
pub trait FrameHost: 'static {
    /// Before the frame is traversed. `dt` is the seconds since the last one.
    fn before_frame(&mut self, dt: f64, frame: &mut HostFrame<'_>);

    /// Before the viewer's own controls see the event.
    ///
    /// Answering [`Consumed::Yes`] stops it there — the controller never
    /// receives it, so a host that drives the camera is not fighting a drag it
    /// cannot see. The default answers [`Consumed::No`]: a host that does not
    /// care about input does not have to say so.
    fn window_event(&mut self, _event: &HostEvent, _frame: &HostFrame<'_>) -> Consumed {
        Consumed::No
    }
}

/// An input, as the host sees it: what happened, not how the shell received
/// it.
///
/// Deliberately not winit's own type. A host written against this compiles for
/// the page and for a window alike, and neither shell's version leaks into it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostEvent {
    /// A pointer moved to a position in device pixels, from the frame's
    /// top-left.
    PointerMoved { x: f64, y: f64 },
    /// A button went down or came up at that position.
    PointerButton {
        x: f64,
        y: f64,
        button: PointerButton,
        down: bool,
    },
    /// A wheel or a trackpad scrolled. Lines where the device reports lines,
    /// device pixels where it reports pixels — `lines` says which.
    Scrolled { dx: f64, dy: f64, lines: bool },
    /// A key went down or came up.
    Key { key: Key, down: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
    Other(u16),
}

/// A key, named by what it means rather than where it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Space,
    Left,
    Right,
    Up,
    Down,
    Escape,
    /// A printable character, lowercased.
    Char(char),
    /// Anything else, by the shell's own name for it.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn pose(height: f64) -> Pose {
        GlobeCamera::look_at(
            DVec3::new(6_378_137.0 + height, 0.0, 0.0),
            DVec3::new(6_378_137.0, 0.0, 0.0),
            DVec3::Z,
            0.8,
        )
    }

    #[test]
    fn a_host_that_says_nothing_leaves_the_frame_alone() {
        let camera = HostCamera::new(pose(1000.0));
        assert!(
            camera.taken().is_none(),
            "nothing asked for is nothing applied"
        );
        assert_eq!(camera.get().position, pose(1000.0).position);
    }

    #[test]
    fn get_is_the_pose_last_drawn_not_the_one_just_asked_for() {
        // The distinction matters to a host that reads the camera to decide
        // where to put it: it must see where it *was*, or it reads back its
        // own answer and the feedback loop is on itself.
        let mut camera = HostCamera::new(pose(1000.0));
        camera.drive(pose(5000.0));
        assert_eq!(camera.get().position, pose(1000.0).position);
    }

    #[test]
    fn the_last_word_in_a_frame_wins() {
        // A host may change its mind within one `before_frame`; the frame can
        // only be drawn once, so the last call is what happens.
        let mut camera = HostCamera::new(pose(1000.0));
        camera.drive(pose(5000.0));
        camera.release();
        assert!(matches!(camera.taken(), Some(Asked::Release)));
    }

    #[test]
    fn an_unhandled_event_goes_on_to_the_viewer() {
        struct Quiet;
        impl FrameHost for Quiet {
            fn before_frame(&mut self, _dt: f64, _frame: &mut HostFrame<'_>) {}
        }
        let mut host = Quiet;
        let mut camera = HostCamera::new(pose(1000.0));
        let mut preload = Vec::new();
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            viewport: (1920, 1080),
        };
        let seen = host.window_event(
            &HostEvent::Key {
                key: Key::Space,
                down: true,
            },
            &frame,
        );
        assert_eq!(seen, Consumed::No);
    }

    #[test]
    fn the_frame_s_aspect_is_its_pixels_and_never_divides_by_zero() {
        let mut camera = HostCamera::new(pose(1000.0));
        let mut preload = Vec::new();
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            viewport: (1920, 1080),
        };
        assert!((frame.aspect() - 16.0 / 9.0).abs() < 1e-12);

        let mut camera = HostCamera::new(pose(1000.0));
        let mut preload = Vec::new();
        // A window collapsed to nothing still has to answer something finite:
        // a NaN aspect reaches the projection matrix and loses the frame.
        let frame = HostFrame {
            camera: &mut camera,
            preload: &mut preload,
            viewport: (0, 0),
        };
        assert!(frame.aspect().is_finite());
    }

    /// What the application does with each answer, written as the frame step
    /// does it. The frame step itself needs a GPU; this pins the decision it
    /// makes, which is the part that can be got wrong silently.
    #[test]
    fn each_door_reaches_the_controller_it_should() {
        use tuile_camera::CameraController;

        let floor = 100.0;
        let under = {
            let p = pose(10.0);
            GlobeCamera::look_at(p.position, DVec3::ZERO, DVec3::Z, p.fovy)
        };

        // Drive: the pose survives the floor and the frame's ease.
        let mut ctrl = CameraController::new(pose(50_000.0)).with_min_altitude(floor);
        ctrl.drive(under);
        ctrl.advance(1.0 / 60.0);
        assert_eq!(
            ctrl.camera.position, under.position,
            "drive must not be clamped"
        );
        assert!(ctrl.is_driven());

        // Place: the floor applies, and the host is no longer driving.
        let mut ctrl = CameraController::new(pose(50_000.0)).with_min_altitude(floor);
        ctrl.jump_to(under);
        assert!(!ctrl.is_driven(), "placing hands the camera back");
        assert_ne!(
            ctrl.camera.position, under.position,
            "place must be clamped"
        );

        // Release: driving stops, and the eye stays where it was driven.
        let mut ctrl = CameraController::new(pose(50_000.0)).with_min_altitude(floor);
        let high = pose(4_000.0);
        ctrl.drive(high);
        ctrl.release();
        assert!(!ctrl.is_driven());
        ctrl.advance(1.0 / 60.0);
        assert_eq!(
            ctrl.camera.position, high.position,
            "release must not move the eye"
        );
    }

    #[test]
    fn a_preload_view_takes_the_frame_s_shape_by_default() {
        let p = pose(2000.0);
        let view = PreloadView::now(p, 16.0 / 9.0);
        assert_eq!(view.priority, Priority::Now);
        assert_eq!(view.pose.position, p.position);
    }
}
