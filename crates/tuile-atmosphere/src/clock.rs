// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The scene's clock: what instant a frame is lit for, given the wall's.
//!
//! The scene's time is a start, plus an offset, plus the wall time elapsed
//! times a rate:
//!
//! ```text
//! scene(wall) = start + offset + (wall − wall at start) × rate
//! ```
//!
//! By default the start is now, the offset none and the rate one: the
//! scene is lit for the present, and the sun moves as it does outside. A
//! host says otherwise with values, not switches — an absolute start, an
//! offset (`-3h30m`), a rate (`60` an hour a minute, `0` frozen, `-1`
//! backwards) — and moves it while it runs ([`SceneClock::shift`],
//! [`SceneClock::set_rate`], [`SceneClock::pause`], [`SceneClock::now`]).
//!
//! Every method takes the wall time as a number — Unix seconds — rather
//! than reading a clock: the arithmetic is the whole of this module, and a
//! test states the wall as it states everything else.

use tuile_core::geo::Geodetic;

use crate::sun::Sun;

/// A place for a clock that starts at no sunset.
const NOWHERE: Geodetic = Geodetic {
    lon: 0.0,
    lat: 0.0,
    height: 0.0,
};

/// Seconds in a day.
const DAY: f64 = 86_400.0;

/// Where a clock starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Start {
    /// The wall's present.
    Now,
    /// An instant, Unix seconds UTC.
    At(f64),
    /// The next sunset at the camera's place, from the present.
    Sunset,
    /// The next sunrise at the camera's place.
    Sunrise,
}

impl Start {
    /// The event a start is counted from, for a clock started at one.
    pub fn event(&self) -> Option<Event> {
        match self {
            Start::Sunset => Some(Event::Sunset),
            Start::Sunrise => Some(Event::Sunrise),
            Start::Now | Start::At(_) => None,
        }
    }
}

/// A moment of the day a clock can be started from, wherever it is: what
/// `time=sunset` names, rather than an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Sunset,
    Sunrise,
}

impl Event {
    /// The first one at `at` after `after` (Unix seconds); `None` where the
    /// sun does not cross the horizon that day.
    pub fn next(self, after: f64, at: Geodetic) -> Option<f64> {
        match self {
            Event::Sunset => Sun::next_sunset(after, at),
            Event::Sunrise => Sun::next_sunrise(after, at),
        }
    }

    /// The one at `at` nearest `instant` — the first after half a day before
    /// it, so within about half a day of it either way.
    pub fn nearest(self, instant: f64, at: Geodetic) -> Option<f64> {
        self.next(instant - DAY / 2.0, at)
    }

    /// The word a start is written with: `sunset`, `sunrise`.
    pub fn name(self) -> &'static str {
        match self {
            Event::Sunset => "sunset",
            Event::Sunrise => "sunrise",
        }
    }
}

/// A clock as a host states it, before it runs: its start, its offset and
/// its rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockSettings {
    pub start: Start,
    /// Seconds added to the start.
    pub offset: f64,
    /// Scene seconds a wall second.
    pub rate: f64,
}

impl Default for ClockSettings {
    /// The present, in real time.
    fn default() -> Self {
        Self {
            start: Start::Now,
            offset: 0.0,
            rate: 1.0,
        }
    }
}

impl ClockSettings {
    /// From the three texts a command line or a page gives: `time` (`now`,
    /// `sunset`, `sunrise` or an RFC 3339 instant), `offset` (a signed
    /// duration: `-3h30m`, `+45m`, `90s`) and `rate` (a number). `None`: the
    /// default's.
    pub fn parse(
        time: Option<&str>,
        offset: Option<&str>,
        rate: Option<&str>,
    ) -> Result<Self, String> {
        let d = Self::default();
        let start = match time.map(str::trim) {
            None => d.start,
            Some(t) => parse_start(t)?,
        };
        let offset = match offset {
            None => d.offset,
            Some(o) => parse_duration(o)?,
        };
        let rate = match rate.map(str::trim) {
            None => d.rate,
            Some(r) => r
                .parse::<f64>()
                .ok()
                .filter(|r| r.is_finite() && r.abs() <= 1.0e6)
                .ok_or_else(|| {
                    format!("the time rate is a number of scene seconds a second, got {r:?}")
                })?,
        };
        Ok(Self {
            start,
            offset,
            rate,
        })
    }

    /// The clock these settings start at `wall` (Unix seconds), for a
    /// camera at `at` — which a sunset or a sunrise needs.
    pub fn start(&self, wall: f64, at: Geodetic) -> SceneClock {
        let start = match self.start {
            Start::Now => wall,
            Start::At(t) => t,
            Start::Sunset => Event::Sunset.next(wall, at).unwrap_or(wall),
            Start::Sunrise => Event::Sunrise.next(wall, at).unwrap_or(wall),
        };
        SceneClock {
            wall,
            scene: start + self.offset,
            rate: self.rate,
            paused: None,
        }
    }

    /// The settings that start a clock at `instant`, running at `rate`, as a
    /// link writes them: counted from the `event` nearest `instant` at `at`
    /// when there is one — so that the link brings back "half an hour
    /// before sunset" on whatever day it is opened, as `time=sunset` does —
    /// and as the instant itself otherwise, or where the sun does not set.
    /// To the second: a link is text, and a second is no light anyone sees.
    pub fn of_instant(instant: f64, rate: f64, event: Option<Event>, at: Geodetic) -> Self {
        let instant = instant.round();
        let from = event.and_then(|e| Some((e, e.nearest(instant, at)?)));
        let (start, offset) = match from {
            Some((event, at_event)) => (
                match event {
                    Event::Sunset => Start::Sunset,
                    Event::Sunrise => Start::Sunrise,
                },
                (instant - at_event).round(),
            ),
            None => (Start::At(instant), 0.0),
        };
        Self {
            start,
            offset,
            rate,
        }
    }

    /// The three texts [`Self::parse`] reads these back from: the start, the
    /// offset when there is one, and the rate when it is not real time.
    pub fn texts(&self) -> (String, Option<String>, Option<String>) {
        let start = match self.start {
            Start::Now => "now".to_owned(),
            Start::At(t) => format_rfc3339(t),
            Start::Sunset => Event::Sunset.name().to_owned(),
            Start::Sunrise => Event::Sunrise.name().to_owned(),
        };
        let offset = (self.offset != 0.0).then(|| format_duration(self.offset));
        let rate = (self.rate != 1.0).then(|| self.rate.to_string());
        (start, offset, rate)
    }
}

/// A running clock: the scene's instant at a wall instant, and its rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneClock {
    /// The wall instant the clock was last set at, and the scene's then.
    wall: f64,
    scene: f64,
    rate: f64,
    /// The rate it ran at before it was paused.
    paused: Option<f64>,
}

impl SceneClock {
    /// The present, in real time, from `wall`.
    pub fn real_time(wall: f64) -> Self {
        ClockSettings::default().start(wall, NOWHERE)
    }

    /// The scene's instant at `wall`, Unix seconds UTC.
    pub fn at(&self, wall: f64) -> f64 {
        self.scene + (wall - self.wall) * self.rate
    }

    /// Scene seconds a wall second: 0 while paused.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// Set again at `wall`, the scene's instant there kept: what every
    /// change of rate does first, so that the scene never jumps.
    fn anchored(&mut self, wall: f64) {
        self.scene = self.at(wall);
        self.wall = wall;
    }

    /// The scene's instant moved by `seconds` at `wall`, its rate kept.
    pub fn shift(&mut self, seconds: f64, wall: f64) {
        self.anchored(wall);
        self.scene += seconds;
    }

    /// The scene's instant set to `instant` at `wall`.
    pub fn set(&mut self, instant: f64, wall: f64) {
        self.anchored(wall);
        self.scene = instant;
    }

    /// Running at `rate` from `wall`: no jump. Unpauses.
    pub fn set_rate(&mut self, rate: f64, wall: f64) {
        self.anchored(wall);
        self.rate = rate;
        self.paused = None;
    }

    /// Paused at `wall`, or running again at the rate it had.
    pub fn pause(&mut self, wall: f64) {
        self.anchored(wall);
        match self.paused.take() {
            Some(rate) => self.rate = rate,
            None => {
                self.paused = Some(self.rate);
                self.rate = 0.0;
            }
        }
    }

    /// Back to the present, in real time.
    pub fn now(&mut self, wall: f64) {
        *self = Self::real_time(wall);
    }
}

/// The local apparent solar time at longitude `lon` (radians) at `instant`,
/// hours in [0, 24): noon when the sun crosses the meridian.
pub fn solar_time_hours(instant: f64, lon: f64) -> f64 {
    let sun = Sun::at_unix_seconds(instant);
    let hours = 12.0 + (lon - sun.subsolar_lon).to_degrees() / 15.0;
    hours.rem_euclid(24.0)
}

/// `now`, `sunset`, `sunrise` or an RFC 3339 instant.
pub fn parse_start(text: &str) -> Result<Start, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "now" => Ok(Start::Now),
        "sunset" => Ok(Start::Sunset),
        "sunrise" => Ok(Start::Sunrise),
        _ => parse_rfc3339(text).map(Start::At),
    }
}

/// An RFC 3339 instant — `2026-06-21T19:42:00Z`, `2026-06-21T21:42+02:00`,
/// seconds optional, a fraction of them allowed — as Unix seconds.
pub fn parse_rfc3339(text: &str) -> Result<f64, String> {
    let bad = || format!("not an RFC 3339 instant (2026-06-21T19:42:00Z): {text:?}");
    let t = text.trim();
    let (date, rest) = t.split_once(['T', 't', ' ']).ok_or_else(bad)?;
    let mut d = date.split('-');
    let num = |s: Option<&str>| s.and_then(|s| s.parse::<i64>().ok()).ok_or_else(bad);
    let (y, mo, da) = (num(d.next())?, num(d.next())?, num(d.next())?);
    if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return Err(bad());
    }
    // The zone: Z, or ±hh:mm at the end.
    let (clock, zone) = if let Some(c) = rest.strip_suffix(['Z', 'z']) {
        (c, 0.0)
    } else {
        let at = rest.rfind(['+', '-']).ok_or_else(bad)?;
        let (c, z) = rest.split_at(at);
        let sign = if z.starts_with('-') { -1.0 } else { 1.0 };
        let (zh, zm) = z[1..].split_once(':').unwrap_or((&z[1..], "0"));
        let zh: f64 = zh.parse().map_err(|_| bad())?;
        let zm: f64 = zm.parse().map_err(|_| bad())?;
        (c, sign * (zh * 3600.0 + zm * 60.0))
    };
    let mut c = clock.split(':');
    let h: f64 = c.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let mi: f64 = c.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let s: f64 = c.next().map_or(Ok(0.0), |s| s.parse()).map_err(|_| bad())?;
    if c.next().is_some()
        || !(0.0..24.0).contains(&h)
        || !(0.0..60.0).contains(&mi)
        || !(0.0..61.0).contains(&s)
    {
        return Err(bad());
    }
    Ok(days_from_civil(y, mo, da) as f64 * DAY + h * 3600.0 + mi * 60.0 + s - zone)
}

/// A signed duration — `-3h30m`, `+45m`, `90s`, `1d`, `2.5h` — in seconds.
/// A bare number is seconds.
pub fn parse_duration(text: &str) -> Result<f64, String> {
    let bad = || format!("not a duration (-3h30m, +45m, 90s): {text:?}");
    let t = text.trim();
    let (sign, body) = match t.strip_prefix('-') {
        Some(b) => (-1.0, b),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if body.is_empty() {
        return Err(bad());
    }
    if let Ok(seconds) = body.parse::<f64>() {
        return seconds
            .is_finite()
            .then_some(sign * seconds)
            .ok_or_else(bad);
    }
    let mut total = 0.0;
    let mut number = String::new();
    for ch in body.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        let unit = match ch {
            'd' => DAY,
            'h' => 3600.0,
            'm' => 60.0,
            's' => 1.0,
            _ => return Err(bad()),
        };
        let n: f64 = number.parse().map_err(|_| bad())?;
        total += n * unit;
        number.clear();
    }
    if !number.is_empty() {
        return Err(bad());
    }
    Ok(sign * total)
}

/// A duration as [`parse_duration`] reads it, to the second: `-12m`,
/// `1h30m`, `-45s`, `0s`. Hours are the largest unit, so a day and a half is
/// `36h`.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.round();
    if total == 0.0 {
        return "0s".to_owned();
    }
    let sign = if total < 0.0 { "-" } else { "" };
    let total = total.abs() as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    let mut text = sign.to_owned();
    for (n, unit) in [(h, 'h'), (m, 'm'), (s, 's')] {
        if n != 0 {
            text.push_str(&format!("{n}{unit}"));
        }
    }
    text
}

/// An instant as RFC 3339, to the second, UTC.
pub fn format_rfc3339(instant: f64) -> String {
    let seconds = instant.floor() as i64;
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALL: f64 = 1_790_000_000.0;

    #[test]
    fn instants_read_and_written_as_rfc3339() {
        let t = parse_rfc3339("2026-06-21T19:42:00Z").unwrap();
        assert_eq!(format_rfc3339(t), "2026-06-21T19:42:00Z");
        assert_eq!(parse_rfc3339("2026-06-21T21:42+02:00").unwrap(), t);
        assert_eq!(parse_rfc3339("2026-06-21T14:42:00-05:00").unwrap(), t);
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z").unwrap(), 0.0);
        assert_eq!(
            parse_rfc3339("2000-03-01T00:00:00Z").unwrap(),
            951_868_800.0
        );
        for bad in [
            "2026-13-01T00:00Z",
            "yesterday",
            "2026-06-21",
            "2026-06-21T25:00Z",
        ] {
            assert!(parse_rfc3339(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_are_signed_sums_of_units() {
        assert_eq!(parse_duration("-3h30m").unwrap(), -12_600.0);
        assert_eq!(parse_duration("+45m").unwrap(), 2_700.0);
        assert_eq!(parse_duration("90s").unwrap(), 90.0);
        assert_eq!(parse_duration("1d2h").unwrap(), 93_600.0);
        assert_eq!(parse_duration("2.5h").unwrap(), 9_000.0);
        assert_eq!(parse_duration("-30").unwrap(), -30.0);
        for bad in ["", "-", "3x", "h", "3h30"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_scene_runs_from_its_start_plus_offset_at_its_rate() {
        let s =
            ClockSettings::parse(Some("2026-06-21T19:00:00Z"), Some("-30m"), Some("60")).unwrap();
        let clock = s.start(WALL, NOWHERE);
        let start = parse_rfc3339("2026-06-21T18:30:00Z").unwrap();
        assert_eq!(clock.at(WALL), start);
        // A minute of the wall is an hour of the scene.
        assert_eq!(clock.at(WALL + 60.0), start + 3600.0);
        // Now, by default, in real time.
        let now = ClockSettings::default().start(WALL, NOWHERE);
        assert_eq!(now.at(WALL + 5.0), WALL + 5.0);
        // An offset from now.
        let earlier = ClockSettings::parse(None, Some("-3h30m"), None)
            .unwrap()
            .start(WALL, NOWHERE);
        assert_eq!(earlier.at(WALL), WALL - 12_600.0);
    }

    #[test]
    fn a_clock_pauses_runs_backwards_shifts_and_comes_back_to_now() {
        let mut clock = SceneClock::real_time(WALL);
        clock.pause(WALL + 10.0);
        assert!(clock.is_paused());
        assert_eq!(
            clock.at(WALL + 100.0),
            WALL + 10.0,
            "frozen where it was paused"
        );
        clock.pause(WALL + 100.0);
        assert_eq!(
            clock.at(WALL + 101.0),
            WALL + 11.0,
            "running again, at its rate"
        );
        clock.set_rate(-60.0, WALL + 101.0);
        assert_eq!(
            clock.at(WALL + 102.0),
            WALL + 11.0 - 60.0,
            "a minute back a second"
        );
        clock.shift(900.0, WALL + 102.0);
        assert_eq!(
            clock.at(WALL + 102.0),
            WALL - 49.0 + 900.0,
            "a quarter of an hour on"
        );
        assert_eq!(clock.rate(), -60.0, "the rate kept");
        clock.now(WALL + 200.0);
        assert_eq!(clock.at(WALL + 201.0), WALL + 201.0);
        assert_eq!(clock.rate(), 1.0);
    }

    #[test]
    fn a_clock_can_start_at_the_next_sunset() {
        let at = Geodetic {
            lon: 0.15f64.to_radians(),
            lat: 43.0667f64.to_radians(),
            height: 0.0,
        };
        let noon = parse_rfc3339("2026-06-21T12:00:00Z").unwrap();
        let clock = ClockSettings::parse(Some("sunset"), Some("-30m"), Some("30"))
            .unwrap()
            .start(noon, at);
        let sunset = Sun::next_sunset(noon, at).unwrap();
        assert_eq!(clock.at(noon), sunset - 1800.0);
        assert!((clock.at(noon) - parse_rfc3339("2026-06-21T19:12:30Z").unwrap()).abs() < 60.0);
    }

    #[test]
    fn durations_are_written_as_they_are_read() {
        for (seconds, text) in [
            (-720.0, "-12m"),
            (5_400.0, "1h30m"),
            (-45.0, "-45s"),
            (0.0, "0s"),
            (129_600.0, "36h"),
            (3_661.4, "1h1m1s"),
        ] {
            assert_eq!(format_duration(seconds), text);
            assert_eq!(parse_duration(text).unwrap(), seconds.round(), "{text}");
        }
    }

    /// A clock written out for a link is counted from the sunset nearest
    /// its instant when it was started from one, and read back, it starts
    /// at that instant again — on that day, and at the same distance from
    /// the sunset on any other.
    #[test]
    fn a_clock_written_from_a_sunset_reads_back_as_the_same_moment() {
        let pic = Geodetic {
            lon: 0.1418f64.to_radians(),
            lat: 42.9365f64.to_radians(),
            height: 2877.0,
        };
        let noon = parse_rfc3339("2026-06-21T12:00:00Z").unwrap();
        let sunset = Sun::next_sunset(noon, pic).unwrap();
        // Twelve minutes before it, and four after: the nearest either way.
        for shift in [-720.0, 240.0] {
            let offset = shift;
            let instant = (sunset + shift).round();
            let s = ClockSettings::of_instant(instant, 30.0, Some(Event::Sunset), pic);
            let (time, written, rate) = s.texts();
            assert_eq!(time, "sunset");
            assert_eq!(rate.as_deref(), Some("30"));
            let back =
                ClockSettings::parse(Some(&time), written.as_deref(), rate.as_deref()).unwrap();
            assert_eq!(back, s);
            // Opened at noon that day, it is that instant again — within the
            // second the sunset is rounded to, and the one the ephemeris finds
            // it to.
            assert!((back.start(noon, pic).at(noon) - instant).abs() <= 2.0);
            // The offset is the distance from the sunset, to the second the
            // ephemeris finds it to.
            let written = parse_duration(written.as_deref().expect("an offset")).unwrap();
            assert!((written - offset).abs() <= 2.0, "{written} for {offset}");
        }
        // Started from no event, or where there is none: the instant itself.
        let at = ClockSettings::of_instant(noon + 0.4, 1.0, None, pic);
        assert_eq!(at.texts(), ("2026-06-21T12:00:00Z".to_owned(), None, None));
        let pole = Geodetic {
            lon: 0.0,
            lat: 89f64.to_radians(),
            height: 0.0,
        };
        let polar = ClockSettings::of_instant(noon, 0.0, Some(Event::Sunset), pole);
        assert_eq!(polar.start, Start::At(noon));
        assert_eq!(polar.texts().2.as_deref(), Some("0"));
    }

    #[test]
    fn solar_noon_is_when_the_sun_crosses_the_meridian() {
        // At Greenwich on an equinox, apparent noon is within the equation
        // of time of 12:00 UTC: some 7 minutes in March.
        let t = parse_rfc3339("2026-03-20T12:00:00Z").unwrap();
        let h = solar_time_hours(t, 0.0);
        assert!((h - 12.0).abs() < 0.2, "{h}");
    }
}
