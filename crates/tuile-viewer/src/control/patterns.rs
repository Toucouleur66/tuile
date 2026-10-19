// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The motion patterns of the graphics budget's specification, as runs.
//!
//! | | pattern | the runs |
//! |---|---|---|
//! | P1 | progressive loading | a cold start, a warm one from the store, a cold one at low memory |
//! | P2 | zoom in | slow (×2/s), fast (×10/s), down to the deepest level |
//! | P3 | zoom out | to 200 km and back |
//! | P4 | rotation | yaw in place at 180°/s, an orbit about a target, a tilt, a head's 300°/s |
//! | P5 | orientation | portrait and landscape, a resize, a change of density |
//! | P6 | stereo | two eyes 63 mm apart, a head moving at 90 Hz |
//!
//! Each is a [`Pattern`]: where the run starts, how long it is given to come
//! up, the motion as a [`Series`], and how long the picture then has to
//! settle — the specification's convergence bound, two seconds on a desktop
//! and four on a phone, at the run's refresh. The eye is moved by steering
//! commands, one a frame, exactly as fast as the pattern says.

use super::{Action, Device, Eyes, Series, Start, View};

/// Where the patterns look: Paris.
pub const PARIS: (f64, f64) = (2.35, 48.85);

/// The specification's convergence bound, seconds: a desktop's, a phone's.
pub const DESKTOP_SECONDS: f64 = 2.0;
pub const PHONE_SECONDS: f64 = 4.0;

/// A headset: Android, and roomier than a phone.
pub const HEADSET: Device = Device::Android {
    memory: 8 << 30,
    available: Some(4 << 30),
    cores: 8,
};

/// A run of one pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub name: &'static str,
    pub start: Start,
    /// Steps the run is given before the motion, to come up and settle.
    pub warm: u64,
    pub motion: Series,
    /// Steps the picture is given to settle after it.
    pub settle: u64,
}

fn frames(seconds: f64, hz: f64) -> u64 {
    (seconds * hz).ceil() as u64
}

fn bound(start: &Start) -> u64 {
    let seconds = match start.device {
        Device::Desktop => DESKTOP_SECONDS,
        _ => PHONE_SECONDS,
    };
    frames(seconds, start.refresh_hz)
}

fn over(lon: f64, lat: f64, altitude: f64, heading: f64, pitch: f64) -> View {
    View {
        lon,
        lat,
        altitude,
        heading,
        pitch,
    }
}

fn pattern(name: &'static str, start: Start, motion: Series) -> Pattern {
    let settle = bound(&start);
    Pattern {
        name,
        // Coming up is P1's to measure; the others start from a settled
        // picture, given generously the time to reach it.
        warm: 10 * settle,
        motion,
        settle,
        start,
    }
}

/// Each frame's view along a path: `at(t)` for `t` from 0 to 1, over
/// `seconds` at `hz`.
fn path(seconds: f64, hz: f64, at: impl Fn(f64) -> View) -> Series {
    let n = frames(seconds, hz).max(1);
    Series::of((1..=n).map(|i| Action::Go(at(i as f64 / n as f64))))
}

/// From `from` metres to `to`, straight down at the target, at `rate` times
/// closer (or further) a second.
fn zoom(from: f64, to: f64, rate: f64, hz: f64) -> Series {
    let seconds = (from / to).ln().abs() / rate.ln();
    let (lon, lat) = PARIS;
    path(seconds, hz, |t| {
        over(lon, lat, from * (to / from).powf(t), 0.0, 90.0)
    })
}

/// P1: the first view, from nothing — the run's own start is the pattern.
pub fn cold_start() -> Pattern {
    let start = Start::default();
    Pattern {
        name: "P1 cold start",
        warm: 0,
        motion: Series::new(),
        settle: bound(&start),
        start,
    }
}

/// P1: the same, on a phone the system has left little memory.
pub fn cold_start_at_low_memory() -> Pattern {
    let start = Start {
        device: Device::LOW_MEMORY_PHONE,
        size: (144, 312),
        scale: 3.0,
        ..Start::default()
    };
    Pattern {
        name: "P1 cold start at low memory",
        warm: 0,
        motion: Series::new(),
        settle: bound(&start),
        start,
    }
}

/// P2: straight down at Paris from 2 000 km to 5 km, twice as close a second.
pub fn zoom_in_slow() -> Pattern {
    let start = Start::default();
    let motion = zoom(2.0e6, 5.0e3, 2.0, start.refresh_hz);
    pattern("P2 zoom in, slow", start, motion)
}

/// P2: from 2 000 km to 1 km, ten times closer a second.
pub fn zoom_in_fast() -> Pattern {
    let start = Start::default();
    let motion = zoom(2.0e6, 1.0e3, 10.0, start.refresh_hz);
    pattern("P2 zoom in, fast", start, motion)
}

/// P2: down to 150 m, where the deepest level the fixture serves is chosen.
pub fn zoom_in_to_the_deepest_level() -> Pattern {
    let start = Start::default();
    let motion = zoom(2.0e6, 150.0, 4.0, start.refresh_hz);
    pattern("P2 zoom in, to the deepest level", start, motion)
}

/// P3: from 2 km out to 200 km and back in, ten times a second each way.
pub fn zoom_out_and_back() -> Pattern {
    let (lon, lat) = PARIS;
    let start = Start {
        view: over(lon, lat, 2.0e3, 0.0, 90.0),
        ..Start::default()
    };
    let hz = start.refresh_hz;
    let motion = zoom(2.0e3, 2.0e5, 10.0, hz).then(zoom(2.0e5, 2.0e3, 10.0, hz));
    pattern("P3 zoom out to 200 km and back", start, motion)
}

/// P4: turning in place at 20 km, looking 45° down, half a turn a second for
/// two seconds.
pub fn yaw_in_place() -> Pattern {
    yaw("P4 yaw in place, 180°/s", 180.0, 2.0)
}

/// P4: the same at a head's rate, 300°/s.
pub fn yaw_at_head_rate() -> Pattern {
    yaw("P4 yaw at head rate, 300°/s", 300.0, 1.2)
}

fn yaw(name: &'static str, degrees_per_second: f64, seconds: f64) -> Pattern {
    let (lon, lat) = PARIS;
    let start = Start {
        view: over(lon, lat, 2.0e4, 0.0, 45.0),
        ..Start::default()
    };
    let motion = path(seconds, start.refresh_hz, |t| {
        over(
            lon,
            lat,
            2.0e4,
            (degrees_per_second * seconds * t) % 360.0,
            45.0,
        )
    });
    pattern(name, start, motion)
}

/// P4: a full circle about a point on the ground, 30 km out, looking 30°
/// down at it, in six seconds.
pub fn orbit() -> Pattern {
    let (lon, lat) = PARIS;
    let (range, pitch) = (3.0e4, 30f64.to_radians());
    let at = |t: f64| {
        let heading = 360.0 * t;
        // The eye stands back from the target opposite to where it looks,
        // and up: on the local tangent plane, near enough at this range.
        let back = -range * pitch.cos();
        let (north, east) = (
            back * heading.to_radians().cos(),
            back * heading.to_radians().sin(),
        );
        let metres_per_degree = 111_320.0;
        over(
            lon + east / (metres_per_degree * lat.to_radians().cos()),
            lat + north / metres_per_degree,
            range * pitch.sin(),
            heading % 360.0,
            30.0,
        )
    };
    let start = Start {
        view: at(0.0),
        ..Start::default()
    };
    let motion = path(6.0, start.refresh_hz, at);
    pattern("P4 orbit about a target", start, motion)
}

/// P4: from straight down to 15° below the horizon at 10 km, 60° a second.
pub fn tilt() -> Pattern {
    let (lon, lat) = PARIS;
    let start = Start {
        view: over(lon, lat, 1.0e4, 0.0, 90.0),
        ..Start::default()
    };
    let motion = path(75.0 / 60.0, start.refresh_hz, |t| {
        over(lon, lat, 1.0e4, 0.0, 90.0 - 75.0 * t)
    });
    pattern("P4 tilt toward the horizon", start, motion)
}

/// P5: a phone at 20 km over Paris turned from portrait to landscape and
/// back, three times, a second in each.
pub fn portrait_and_landscape() -> Pattern {
    let (lon, lat) = PARIS;
    let start = Start {
        view: over(lon, lat, 2.0e4, 0.0, 60.0),
        device: Device::PHONE,
        size: (144, 312),
        scale: 3.0,
        ..Start::default()
    };
    let second = start.refresh_hz as usize;
    let mut motion = Series::new();
    for turn in 0..6 {
        let (width, height) = if turn % 2 == 0 {
            (312, 144)
        } else {
            (144, 312)
        };
        motion = motion.then(
            Series::new()
                .at(0, Action::Resize { width, height })
                .then(Series::still(second - 1)),
        );
    }
    pattern("P5 portrait and landscape", start, motion)
}

/// P5: a window made larger and smaller again, and a change of density.
pub fn resize_and_rescale() -> Pattern {
    let (lon, lat) = PARIS;
    let start = Start {
        view: over(lon, lat, 2.0e4, 0.0, 60.0),
        ..Start::default()
    };
    let second = start.refresh_hz as usize;
    let motion = Series::new()
        .at(
            0,
            Action::Resize {
                width: 384,
                height: 320,
            },
        )
        .then(Series::still(second))
        .then(Series::new().at(0, Action::Rescale(2.0)))
        .then(Series::still(second))
        .then(Series::new().at(
            0,
            Action::Resize {
                width: 256,
                height: 256,
            },
        ))
        .then(Series::still(second))
        .then(Series::new().at(0, Action::Rescale(1.0)))
        .then(Series::still(second));
    pattern("P5 resize and density", start, motion)
}

/// P6: a headset's two eyes, 63 mm apart, at 90 Hz, over Paris at 2 km, the
/// head turning ±30° at up to 300°/s and nodding as it goes, for three
/// seconds.
pub fn stereo_head_motion() -> Pattern {
    let (lon, lat) = PARIS;
    let start = Start {
        view: over(lon, lat, 2.0e3, 0.0, 30.0),
        eyes: Eyes::Stereo { separation: 0.063 },
        device: HEADSET,
        refresh_hz: 90.0,
        ..Start::default()
    };
    let seconds = 3.0;
    let motion = path(seconds, start.refresh_hz, |t| {
        let s = t * seconds;
        // ±30° at ten radians a second: 300°/s at the middle of each swing.
        let heading = (30.0 * (10.0 * s).sin()).rem_euclid(360.0);
        over(lon, lat, 2.0e3, heading, 30.0 + 10.0 * (7.0 * s).sin())
    });
    pattern("P6 stereo, head motion at 90 Hz", start, motion)
}
