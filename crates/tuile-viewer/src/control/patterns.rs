// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The motion patterns of the graphics budget's specification, as short
//! runs.
//!
//! | key | pattern | the motion |
//! |---|---|---|
//! | `p1-cold` | P1 progressive loading | the first view, from an empty store |
//! | `p1-warm` | P1 | the same view again after a relaunch over the store |
//! | `p1-low-memory` | P1 | the first view on a phone left little memory |
//! | `p2-slow` | P2 zoom in | 160 km to 5 km, twice as close a second |
//! | `p2-fast` | P2 | 2 000 km to 2 km, ten times a second |
//! | `p2-deepest` | P2 | 20 km to 150 m, four times a second |
//! | `p3` | P3 zoom out | 10 km out to 200 km and back, ten times a second |
//! | `p4-yaw` | P4 rotation | a turn in place at 180°/s for 2 s |
//! | `p4-head-rate` | P4 | the same at a head's 300°/s |
//! | `p4-orbit` | P4 | a full circle about a target in 4 s |
//! | `p4-tilt` | P4 | from straight down to 15° above the horizon at 60°/s |
//! | `p5-rotate` | P5 orientation | a phone turned four times, 0.75 s each way |
//! | `p5-resize` | P5 | a window made larger and back |
//! | `p5-rescale` | P5 | a window moved to a plain panel and back |
//! | `p6-stereo` | P6 stereo | a headset's two eyes, the head turning at up to 300°/s, at 90 Hz |
//!
//! Each is short on purpose: a run comes up (given [`Pattern::warm`] at
//! most), makes its motion — seconds, not minutes — and is watched settling
//! for [`Pattern::watch`] at most, against the specification's bound: two
//! seconds on a desktop, four on a phone. The eye is moved by steering
//! commands sampled at the display's refresh; a run whose frames fall
//! behind moves it as far, in fewer steps.

use super::{Action, Device, Eyes, Series, Start, View};

/// Where the patterns look: Paris.
pub const PARIS: (f64, f64) = (2.35, 48.85);

/// The specification's convergence bound, seconds: a desktop's, a phone's.
pub const DESKTOP_SECONDS: f64 = 2.0;
pub const PHONE_SECONDS: f64 = 4.0;

/// How long a run is given to come up before its motion, at most.
pub const WARM_SECONDS: f64 = 15.0;

/// How long a run is watched settling after its motion, at most: long
/// enough to say by how much it misses its bound.
pub const WATCH_SECONDS: f64 = 10.0;

/// A headset: Android, and roomier than a phone.
pub const HEADSET: Device = Device::Android {
    memory: 8 << 30,
    available: Some(4 << 30),
    cores: 8,
};

/// A run of one pattern: see the module.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// What a command line names it by.
    pub key: &'static str,
    pub name: &'static str,
    pub view: View,
    pub device: Device,
    /// One eye's frame, device pixels, and the density.
    pub size: (u32, u32),
    pub scale: f64,
    pub eyes: Eyes,
    pub refresh_hz: f64,
    /// Seconds the run is given to come up before the motion; none where
    /// coming up is the pattern.
    pub warm: f64,
    pub motion: Series,
    /// The specification's bound, seconds: what the settling is held to.
    pub settle: f64,
    /// Seconds the settling is watched for, at most.
    pub watch: f64,
    /// Whether the run is a relaunch over the store a first run filled.
    pub relaunch: bool,
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

    /// The same pattern on another device: its frame, density and bound
    /// follow the device.
    #[must_use]
    pub fn on(mut self, device: Device) -> Self {
        let (size, scale, settle) = frame_of(device);
        self.device = device;
        self.size = size;
        self.scale = scale;
        self.settle = settle;
        self
    }
}

/// A device's frame, density and convergence bound: a desktop's window at
/// 1280 × 800 on a Retina panel; a phone held upright, 1170 × 2532 at three
/// pixels a point; a headset's eye, 1280 × 1280.
pub fn frame_of(device: Device) -> ((u32, u32), f64, f64) {
    match device {
        Device::Desktop => ((1280, 800), 2.0, DESKTOP_SECONDS),
        Device::Handheld { .. } => ((1170, 2532), 3.0, PHONE_SECONDS),
        Device::Android { .. } if device == HEADSET => ((1280, 1280), 1.0, PHONE_SECONDS),
        Device::Android { .. } => ((1080, 2400), 2.75, PHONE_SECONDS),
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

fn pattern(key: &'static str, name: &'static str, view: View, motion: Series) -> Pattern {
    let (size, scale, settle) = frame_of(Device::Desktop);
    Pattern {
        key,
        name,
        view,
        device: Device::Desktop,
        size,
        scale,
        eyes: Eyes::Mono,
        refresh_hz: 60.0,
        warm: WARM_SECONDS,
        motion,
        settle,
        watch: WATCH_SECONDS,
        relaunch: false,
    }
}

/// From `from` metres to `to`, straight down at Paris, `rate` times closer
/// (or further) a second.
fn zoom(from: f64, to: f64, rate: f64) -> Series {
    let seconds = (from / to).ln().abs() / rate.ln();
    let (lon, lat) = PARIS;
    Series::path(seconds, 60.0, |t| {
        over(lon, lat, from * (to / from).powf(t), 0.0, 90.0)
    })
}

/// P1: the first view, from nothing — the run's own start is the pattern,
/// launch included.
pub fn cold_start() -> Pattern {
    Pattern {
        warm: 0.0,
        watch: WARM_SECONDS,
        ..pattern("p1-cold", "P1 cold start", View::default(), Series::new())
    }
}

/// P1: the same view after the application was killed and opened again
/// over the store the first run filled.
pub fn warm_start() -> Pattern {
    Pattern {
        key: "p1-warm",
        name: "P1 warm start",
        relaunch: true,
        ..cold_start()
    }
}

/// P1: the first view on a phone the system has left little memory.
pub fn cold_start_at_low_memory() -> Pattern {
    Pattern {
        key: "p1-low-memory",
        name: "P1 cold start at low memory",
        ..cold_start().on(Device::LOW_MEMORY_PHONE)
    }
}

/// P2: straight down at Paris from 160 km to 5 km, twice as close a second.
pub fn zoom_in_slow() -> Pattern {
    let (lon, lat) = PARIS;
    pattern(
        "p2-slow",
        "P2 zoom in, slow",
        over(lon, lat, 1.6e5, 0.0, 90.0),
        zoom(1.6e5, 5.0e3, 2.0),
    )
}

/// P2: from 2 000 km to 2 km, ten times closer a second.
pub fn zoom_in_fast() -> Pattern {
    let (lon, lat) = PARIS;
    pattern(
        "p2-fast",
        "P2 zoom in, fast",
        over(lon, lat, 2.0e6, 0.0, 90.0),
        zoom(2.0e6, 2.0e3, 10.0),
    )
}

/// P2: from 20 km down to 150 m, four times closer a second, where the
/// finest levels are chosen.
pub fn zoom_in_to_the_deepest_level() -> Pattern {
    let (lon, lat) = PARIS;
    pattern(
        "p2-deepest",
        "P2 zoom in, to the deepest level",
        over(lon, lat, 2.0e4, 0.0, 90.0),
        zoom(2.0e4, 150.0, 4.0),
    )
}

/// P3: from 10 km out to 200 km and back in, ten times a second each way.
pub fn zoom_out_and_back() -> Pattern {
    let (lon, lat) = PARIS;
    pattern(
        "p3",
        "P3 zoom out to 200 km and back",
        over(lon, lat, 1.0e4, 0.0, 90.0),
        zoom(1.0e4, 2.0e5, 10.0).then(zoom(2.0e5, 1.0e4, 10.0)),
    )
}

fn yaw(key: &'static str, name: &'static str, degrees_per_second: f64, seconds: f64) -> Pattern {
    let (lon, lat) = PARIS;
    pattern(
        key,
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

/// P4: turning in place at 20 km, looking 45° down, half a turn a second for
/// two seconds.
pub fn yaw_in_place() -> Pattern {
    yaw("p4-yaw", "P4 yaw in place, 180°/s", 180.0, 2.0)
}

/// P4: the same at a head's rate, 300°/s.
pub fn yaw_at_head_rate() -> Pattern {
    yaw("p4-head-rate", "P4 yaw at head rate, 300°/s", 300.0, 1.2)
}

/// P4: a full circle about a point on the ground, 30 km out, looking 30°
/// down at it, in four seconds.
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
    pattern(
        "p4-orbit",
        "P4 orbit about a target",
        at(0.0),
        Series::path(4.0, 60.0, at),
    )
}

/// P4: from straight down to 15° below the horizon at 10 km, 60° a second.
pub fn tilt() -> Pattern {
    let (lon, lat) = PARIS;
    pattern(
        "p4-tilt",
        "P4 tilt toward the horizon",
        over(lon, lat, 1.0e4, 0.0, 90.0),
        Series::path(75.0 / 60.0, 60.0, |t| {
            over(lon, lat, 1.0e4, 0.0, 90.0 - 75.0 * t)
        }),
    )
}

/// P5: a phone at 20 km over Paris turned from portrait to landscape and
/// back, twice, three quarters of a second in each.
pub fn rotate() -> Pattern {
    let (lon, lat) = PARIS;
    let mut motion = Series::new();
    for turn in 0..4 {
        let (width, height) = if turn % 2 == 0 {
            (2532, 1170)
        } else {
            (1170, 2532)
        };
        motion = motion.then(
            Series::new()
                .at(0.0, Action::Resize { width, height })
                .then(Series::still(0.75)),
        );
    }
    pattern(
        "p5-rotate",
        "P5 portrait and landscape",
        over(lon, lat, 2.0e4, 0.0, 60.0),
        motion,
    )
    .on(Device::PHONE)
}

/// P5: a window made larger, and back.
pub fn resize() -> Pattern {
    let (lon, lat) = PARIS;
    let to = |width, height| Series::new().at(0.0, Action::Resize { width, height });
    pattern(
        "p5-resize",
        "P5 resize",
        over(lon, lat, 2.0e4, 0.0, 60.0),
        to(1920, 1200)
            .then(Series::still(1.0))
            .then(to(1280, 800))
            .then(Series::still(1.0)),
    )
}

/// P5: a window moved to a plain panel, and back to a Retina one.
pub fn rescale() -> Pattern {
    let (lon, lat) = PARIS;
    let to = |scale| Series::new().at(0.0, Action::Rescale(scale));
    pattern(
        "p5-rescale",
        "P5 change of density",
        over(lon, lat, 2.0e4, 0.0, 60.0),
        to(1.0)
            .then(Series::still(1.0))
            .then(to(2.0))
            .then(Series::still(1.0)),
    )
}

/// P6: a headset's two eyes, 63 mm apart, at 90 Hz, over Paris at 2 km, the
/// head turning ±30° at up to 300°/s and nodding as it goes, for two
/// seconds.
pub fn stereo_head_motion() -> Pattern {
    let (lon, lat) = PARIS;
    let seconds = 2.0;
    Pattern {
        eyes: Eyes::Stereo { separation: 0.063 },
        refresh_hz: 90.0,
        ..pattern(
            "p6-stereo",
            "P6 stereo, head motion at 90 Hz",
            over(lon, lat, 2.0e3, 0.0, 30.0),
            Series::path(seconds, 90.0, |t| {
                let s = t * seconds;
                // ±30° at ten radians a second: 300°/s at the middle of each
                // swing.
                let heading = (30.0 * (10.0 * s).sin()).rem_euclid(360.0);
                over(lon, lat, 2.0e3, heading, 30.0 + 10.0 * (7.0 * s).sin())
            }),
        )
        .on(HEADSET)
    }
}

/// Every pattern, in the specification's order: what `all` runs.
pub fn all() -> Vec<Pattern> {
    vec![
        cold_start(),
        warm_start(),
        cold_start_at_low_memory(),
        zoom_in_slow(),
        zoom_in_fast(),
        zoom_in_to_the_deepest_level(),
        zoom_out_and_back(),
        yaw_in_place(),
        yaw_at_head_rate(),
        orbit(),
        tilt(),
        rotate(),
        resize(),
        rescale(),
        stereo_head_motion(),
    ]
}

/// The pattern a command line names.
pub fn named(key: &str) -> Option<Pattern> {
    all().into_iter().find(|p| p.key.eq_ignore_ascii_case(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern has its own key, and each is short: its motion a few
    /// seconds, and the whole run — coming up, the motion, the watch —
    /// bounded well under a minute.
    #[test]
    fn every_pattern_is_named_once_and_short() {
        let all = all();
        let mut keys: Vec<&str> = all.iter().map(|p| p.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), all.len());
        for p in &all {
            assert!(
                p.motion.seconds() <= 6.0,
                "{}: {} s",
                p.key,
                p.motion.seconds()
            );
            assert!(p.warm + p.motion.seconds() + p.watch <= 35.0, "{}", p.key);
            assert_eq!(named(p.key).map(|n| n.name), Some(p.name));
        }
        assert!(named("p9").is_none());
    }

    /// A device carries its own frame and bound: a phone's 4 s, a desktop's
    /// 2 s, a headset's two eyes at its own size.
    #[test]
    fn a_pattern_takes_its_frame_and_bound_from_its_device() {
        let phone = zoom_in_fast().on(Device::PHONE);
        assert_eq!((phone.size, phone.settle), ((1170, 2532), PHONE_SECONDS));
        let p6 = stereo_head_motion();
        assert_eq!(
            (p6.size, p6.eyes.count(), p6.refresh_hz),
            ((1280, 1280), 2, 90.0)
        );
        assert_eq!(zoom_in_fast().settle, DESKTOP_SECONDS);
    }
}
