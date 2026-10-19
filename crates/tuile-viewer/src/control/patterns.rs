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
//! Each is a [`Pattern`]: the device and the view a run starts from, how
//! long it is given to come up, the motion as a [`Series`] in seconds, and
//! how long the picture then has to settle — the specification's
//! convergence bound, two seconds on a desktop and four on a phone. The eye
//! is moved by steering commands, sampled at the display's refresh, exactly
//! as fast as the pattern says; a run whose frames fall behind moves it as
//! far in fewer steps.

use super::{Action, Device, Eyes, Series, Start, View};

/// Where the patterns look: Paris.
pub const PARIS: (f64, f64) = (2.35, 48.85);

/// The specification's convergence bound, seconds: a desktop's, a phone's.
pub const DESKTOP_SECONDS: f64 = 2.0;
pub const PHONE_SECONDS: f64 = 4.0;

/// How long a run is given to come up and settle before its motion: far
/// beyond any bound, since what is measured is the motion.
pub const COMING_UP_SECONDS: f64 = 60.0;

/// A headset: Android, and roomier than a phone.
pub const HEADSET: Device = Device::Android {
    memory: 8 << 30,
    available: Some(4 << 30),
    cores: 8,
};

/// A run of one pattern: see the module.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub name: &'static str,
    pub view: View,
    pub device: Device,
    /// One eye's frame, device pixels, and the density.
    pub size: (u32, u32),
    pub scale: f64,
    pub eyes: Eyes,
    pub refresh_hz: f64,
    /// Seconds the run is given to come up and settle before the motion;
    /// none where coming up is the pattern.
    pub warm: f64,
    pub motion: Series,
    /// Seconds the picture has to settle after it.
    pub settle: f64,
}

impl Pattern {
    /// The run's start, over `host`, its tile store in `store`.
    pub fn start(
        &self,
        host: std::sync::Arc<dyn crate::embed::ViewerHost>,
        store: impl Into<std::path::PathBuf>,
    ) -> Start {
        Start {
            view: self.view,
            size: self.size,
            scale: self.scale,
            eyes: self.eyes,
            device: self.device,
            refresh_hz: self.refresh_hz,
            ..Start::new(host, store)
        }
    }
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

/// A desktop's window, 1280 × 800 on a Retina panel, at 60 Hz.
fn desktop(name: &'static str, view: View, motion: Series) -> Pattern {
    Pattern {
        name,
        view,
        device: Device::Desktop,
        size: (1280, 800),
        scale: 2.0,
        eyes: Eyes::Mono,
        refresh_hz: 60.0,
        warm: COMING_UP_SECONDS,
        motion,
        settle: DESKTOP_SECONDS,
    }
}

/// A phone held upright, 1170 × 2532 at three pixels a point, at 60 Hz.
fn phone(name: &'static str, device: Device, view: View, motion: Series) -> Pattern {
    Pattern {
        name,
        view,
        device,
        size: (1170, 2532),
        scale: 3.0,
        eyes: Eyes::Mono,
        refresh_hz: 60.0,
        warm: COMING_UP_SECONDS,
        motion,
        settle: PHONE_SECONDS,
    }
}

/// From `from` metres to `to`, straight down at Paris, `rate` times closer
/// (or further) a second.
fn zoom(from: f64, to: f64, rate: f64, hz: f64) -> Series {
    let seconds = (from / to).ln().abs() / rate.ln();
    let (lon, lat) = PARIS;
    Series::path(seconds, hz, |t| {
        over(lon, lat, from * (to / from).powf(t), 0.0, 90.0)
    })
}

/// P1: the first view, from nothing — the run's own start is the pattern.
pub fn cold_start() -> Pattern {
    Pattern {
        warm: 0.0,
        ..desktop("P1 cold start", View::default(), Series::new())
    }
}

/// P1: the same, on a phone the system has left little memory.
pub fn cold_start_at_low_memory() -> Pattern {
    Pattern {
        warm: 0.0,
        ..phone(
            "P1 cold start at low memory",
            Device::LOW_MEMORY_PHONE,
            View::default(),
            Series::new(),
        )
    }
}

/// P2: straight down at Paris from 2 000 km to 5 km, twice as close a second.
pub fn zoom_in_slow() -> Pattern {
    desktop(
        "P2 zoom in, slow",
        View::default(),
        zoom(2.0e6, 5.0e3, 2.0, 60.0),
    )
}

/// P2: from 2 000 km to 1 km, ten times closer a second.
pub fn zoom_in_fast() -> Pattern {
    desktop(
        "P2 zoom in, fast",
        View::default(),
        zoom(2.0e6, 1.0e3, 10.0, 60.0),
    )
}

/// P2: down to 150 m, four times closer a second, where the finest levels
/// are chosen.
pub fn zoom_in_to_the_deepest_level() -> Pattern {
    desktop(
        "P2 zoom in, to the deepest level",
        View::default(),
        zoom(2.0e6, 150.0, 4.0, 60.0),
    )
}

/// P3: from 2 km out to 200 km and back in, ten times a second each way.
pub fn zoom_out_and_back() -> Pattern {
    let (lon, lat) = PARIS;
    desktop(
        "P3 zoom out to 200 km and back",
        over(lon, lat, 2.0e3, 0.0, 90.0),
        zoom(2.0e3, 2.0e5, 10.0, 60.0).then(zoom(2.0e5, 2.0e3, 10.0, 60.0)),
    )
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
    desktop(
        name,
        over(lon, lat, 2.0e4, 0.0, 45.0),
        Series::path(seconds, 60.0, |t| {
            over(
                lon,
                lat,
                2.0e4,
                (degrees_per_second * seconds * t) % 360.0,
                45.0,
            )
        }),
    )
}

/// P4: a full circle about a point on the ground, 30 km out, looking 30°
/// down at it, in six seconds.
pub fn orbit() -> Pattern {
    let (lon, lat) = PARIS;
    let (range, pitch) = (3.0e4, 30f64.to_radians());
    let at = move |t: f64| {
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
    desktop(
        "P4 orbit about a target",
        at(0.0),
        Series::path(6.0, 60.0, at),
    )
}

/// P4: from straight down to 15° below the horizon at 10 km, 60° a second.
pub fn tilt() -> Pattern {
    let (lon, lat) = PARIS;
    desktop(
        "P4 tilt toward the horizon",
        over(lon, lat, 1.0e4, 0.0, 90.0),
        Series::path(75.0 / 60.0, 60.0, |t| {
            over(lon, lat, 1.0e4, 0.0, 90.0 - 75.0 * t)
        }),
    )
}

/// P5: a phone at 20 km over Paris turned from portrait to landscape and
/// back, three times, a second in each.
pub fn portrait_and_landscape() -> Pattern {
    let (lon, lat) = PARIS;
    let mut motion = Series::new();
    for turn in 0..6 {
        let (width, height) = if turn % 2 == 0 {
            (2532, 1170)
        } else {
            (1170, 2532)
        };
        motion = motion.then(
            Series::new()
                .at(0.0, Action::Resize { width, height })
                .then(Series::still(1.0)),
        );
    }
    phone(
        "P5 portrait and landscape",
        Device::PHONE,
        over(lon, lat, 2.0e4, 0.0, 60.0),
        motion,
    )
}

/// P5: a window made larger and smaller again, and moved between a Retina
/// panel and a plain one.
pub fn resize_and_rescale() -> Pattern {
    let (lon, lat) = PARIS;
    let resize = |width, height| Series::new().at(0.0, Action::Resize { width, height });
    let rescale = |scale| Series::new().at(0.0, Action::Rescale(scale));
    desktop(
        "P5 resize and density",
        over(lon, lat, 2.0e4, 0.0, 60.0),
        resize(1920, 1200)
            .then(Series::still(1.0))
            .then(rescale(1.0))
            .then(Series::still(1.0))
            .then(resize(1280, 800))
            .then(Series::still(1.0))
            .then(rescale(2.0))
            .then(Series::still(1.0)),
    )
}

/// P6: a headset's two eyes, 63 mm apart, at 90 Hz, over Paris at 2 km, the
/// head turning ±30° at up to 300°/s and nodding as it goes, for three
/// seconds.
pub fn stereo_head_motion() -> Pattern {
    let (lon, lat) = PARIS;
    let seconds = 3.0;
    Pattern {
        name: "P6 stereo, head motion at 90 Hz",
        view: over(lon, lat, 2.0e3, 0.0, 30.0),
        device: HEADSET,
        size: (1280, 1280),
        scale: 1.0,
        eyes: Eyes::Stereo { separation: 0.063 },
        refresh_hz: 90.0,
        warm: COMING_UP_SECONDS,
        motion: Series::path(seconds, 90.0, |t| {
            let s = t * seconds;
            // ±30° at ten radians a second: 300°/s at the middle of each
            // swing.
            let heading = (30.0 * (10.0 * s).sin()).rem_euclid(360.0);
            over(lon, lat, 2.0e3, heading, 30.0 + 10.0 * (7.0 * s).sin())
        }),
        settle: PHONE_SECONDS,
    }
}

/// Every pattern, in the specification's order.
pub fn all() -> Vec<Pattern> {
    vec![
        cold_start(),
        cold_start_at_low_memory(),
        zoom_in_slow(),
        zoom_in_fast(),
        zoom_in_to_the_deepest_level(),
        zoom_out_and_back(),
        yaw_in_place(),
        yaw_at_head_rate(),
        orbit(),
        tilt(),
        portrait_and_landscape(),
        resize_and_rescale(),
        stereo_head_motion(),
    ]
}
