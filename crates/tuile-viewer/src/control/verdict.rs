// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a run came to, held to the specification's obligations.
//!
//! Pure: a run's states in — read live, or back from its session log — and a
//! [`Verdict`] out, with the numbers behind each obligation. Every frame
//! drawn is held to three of them, and the run to a fourth:
//!
//! - **no ground left black**, in any eye — the scan of the picture itself;
//! - **no selected ground drawn by nothing** (`frame_lost`): what the
//!   backstop shell then hides is a hole all the same;
//! - **the server within the profile's budget** (`resident_bytes`), the one
//!   class limit today's viewer has;
//! - **convergence within the bound**: the picture settled — every selected
//!   tile drawn by itself, the drawn set holding still — within the
//!   specification's seconds.
//!
//! A warm relaunch is held to a fifth: **nothing the store holds is asked of
//! the network again** (`store_misses`).

use std::path::Path;

use super::{EyeReading, Settled, State, View};

/// The obligations a run is held to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Obligations {
    /// Seconds the picture has to settle in.
    pub bound: f64,
    /// Decoded bytes the server may hold: the profile's budget.
    pub resident_budget: u64,
    /// Whether the run is a warm relaunch, held to fetch nothing.
    pub warm: bool,
}

/// One obligation, kept or not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Check {
    pub kept: bool,
}

/// What a stretch of a run came to: the numbers behind the obligations.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub seconds: f64,
    pub frames: usize,
    pub drawn: usize,
    /// The longest a frame took, seconds, and the median.
    pub slowest_frame: f64,
    pub median_frame: f64,
    /// Frames with ground left the clear colour, in any eye; the worst count
    /// of such pixels; the second it first happened at.
    pub black_frames: usize,
    pub worst_black: u64,
    pub first_black: Option<f64>,
    /// Frames with selected ground drawn by nothing; the most tiles; the
    /// second it first happened at.
    pub lost_frames: usize,
    pub worst_lost: u64,
    pub first_lost: Option<f64>,
    /// Frames drawn with coarser ancestors standing in; the most tiles.
    pub coarser_frames: usize,
    pub worst_coarser: u64,
    /// The most decoded bytes the server held, and the frames it held more
    /// than the budget.
    pub worst_resident_bytes: u64,
    pub over_budget_frames: usize,
    /// The most GPU bytes held, meshes and imagery.
    pub worst_gpu_bytes: u64,
    /// The most imagery bytes held on the GPU, in the formats it was held
    /// in: what holding it block-compressed divides by four.
    pub worst_imagery_bytes: u64,
    /// The imagery textures held at that moment: two runs that held
    /// different tiles compare by the bytes per texture.
    pub worst_imagery_textures: u64,
    /// Lookups the store could not answer, which went to the network; and
    /// those it could.
    pub store_misses: u64,
    pub store_hits: u64,
}

impl Report {
    /// Reads `states`, with `budget` the profile's resident bytes.
    pub fn of(states: &[State], budget: u64) -> Self {
        let mut r = Self {
            frames: states.len(),
            ..Self::default()
        };
        let (Some(first), Some(last)) = (states.first(), states.last()) else {
            return r;
        };
        r.seconds = last.seconds - first.seconds + first.took;
        r.store_misses = last
            .metrics
            .store_misses
            .saturating_sub(first.metrics.store_misses);
        r.store_hits = last
            .metrics
            .store_hits
            .saturating_sub(first.metrics.store_hits);
        let mut took: Vec<f64> = states.iter().filter(|s| s.drew).map(|s| s.took).collect();
        took.sort_by(f64::total_cmp);
        r.slowest_frame = took.last().copied().unwrap_or(0.0);
        r.median_frame = took.get(took.len() / 2).copied().unwrap_or(0.0);
        for s in states {
            let m = &s.metrics;
            r.worst_resident_bytes = r.worst_resident_bytes.max(m.resident_bytes);
            r.worst_gpu_bytes = r.worst_gpu_bytes.max(m.mesh_bytes + m.texture_bytes);
            if m.texture_bytes > r.worst_imagery_bytes {
                r.worst_imagery_bytes = m.texture_bytes;
                r.worst_imagery_textures = m.textures;
            }
            if m.resident_bytes > budget {
                r.over_budget_frames += 1;
            }
            if !s.drew {
                continue;
            }
            r.drawn += 1;
            let black = s.black();
            if black > 0 {
                r.black_frames += 1;
                r.worst_black = r.worst_black.max(black);
                r.first_black.get_or_insert(s.seconds);
            }
            if m.frame_lost > 0 {
                r.lost_frames += 1;
                r.worst_lost = r.worst_lost.max(m.frame_lost);
                r.first_lost.get_or_insert(s.seconds);
            }
            if m.frame_coarser > 0 {
                r.coarser_frames += 1;
                r.worst_coarser = r.worst_coarser.max(m.frame_coarser);
            }
        }
        r
    }
}

/// A run held to its obligations.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub obligations: Obligations,
    pub report: Report,
    /// How the picture settled, and in how long.
    pub settled: Settled,
    pub black: Check,
    pub lost: Check,
    pub budget: Check,
    pub convergence: Check,
    /// Held only on a warm relaunch.
    pub warm_fetches: Option<Check>,
}

impl Verdict {
    /// Holds `states` and the way they `settled` to `obligations`.
    pub fn of(states: &[State], settled: Settled, obligations: Obligations) -> Self {
        let report = Report::of(states, obligations.resident_budget);
        let kept = |kept| Check { kept };
        Self {
            black: kept(report.black_frames == 0),
            lost: kept(report.lost_frames == 0),
            budget: kept(report.over_budget_frames == 0),
            convergence: kept(settled.settled && settled.seconds <= obligations.bound),
            warm_fetches: obligations.warm.then(|| kept(report.store_misses == 0)),
            obligations,
            report,
            settled,
        }
    }

    /// Whether every obligation was kept, and something was drawn.
    pub fn kept(&self) -> bool {
        self.report.drawn > 0
            && self.black.kept
            && self.lost.kept
            && self.budget.kept
            && self.convergence.kept
            && self.warm_fetches.is_none_or(|c| c.kept)
    }

    /// One line an obligation: what it asks, kept or violated, and the
    /// numbers.
    pub fn lines(&self) -> Vec<String> {
        let r = &self.report;
        let mark = |c: Check| if c.kept { "ok      " } else { "VIOLATED" };
        let at = |s: Option<f64>| s.map_or(String::new(), |s| format!(", first at {s:.1} s"));
        let mut lines = vec![
            format!(
                "{} no black ground: {} frames of {} drawn, worst {} px{}",
                mark(self.black),
                r.black_frames,
                r.drawn,
                r.worst_black,
                at(r.first_black)
            ),
            format!(
                "{} no hole: {} frames with selected ground drawn by nothing, worst {} tiles{}; \
                 {} frames coarser, worst {} tiles",
                mark(self.lost),
                r.lost_frames,
                r.worst_lost,
                at(r.first_lost),
                r.coarser_frames,
                r.worst_coarser
            ),
            format!(
                "{} convergence: {} in {:.1} s against {:.1} s",
                mark(self.convergence),
                if self.settled.settled {
                    "settled"
                } else {
                    "NOT settled"
                },
                self.settled.seconds,
                self.obligations.bound
            ),
            format!(
                "{} budget: resident {:.0} MB at most against {:.0} MB, over on {} frames \
                 (imagery on the GPU {:.1} MB at most, {} textures, {} bytes each)",
                mark(self.budget),
                r.worst_resident_bytes as f64 / 1e6,
                self.obligations.resident_budget as f64 / 1e6,
                r.over_budget_frames,
                r.worst_imagery_bytes as f64 / 1e6,
                r.worst_imagery_textures,
                r.worst_imagery_bytes
                    .checked_div(r.worst_imagery_textures)
                    .unwrap_or(0)
            ),
        ];
        if let Some(check) = self.warm_fetches {
            lines.push(format!(
                "{} warm relaunch: {} lookups went to the network, {} served by the store",
                mark(check),
                r.store_misses,
                r.store_hits
            ));
        }
        lines.push(format!(
            "         frames: {} drawn of {} in {:.1} s, median {:.0} ms, slowest {:.0} ms",
            r.drawn,
            r.frames,
            r.seconds,
            r.median_frame * 1000.0,
            r.slowest_frame * 1000.0
        ));
        lines
    }

    /// As a summary file writes it.
    pub fn to_json(&self) -> serde_json::Value {
        let r = &self.report;
        let check = |c: Check| c.kept;
        serde_json::json!({
            "kept": self.kept(),
            "black": {
                "kept": check(self.black),
                "frames": r.black_frames,
                "worst_pixels": r.worst_black,
                "first_seconds": r.first_black,
            },
            "lost": {
                "kept": check(self.lost),
                "frames": r.lost_frames,
                "worst_tiles": r.worst_lost,
                "first_seconds": r.first_lost,
                "coarser_frames": r.coarser_frames,
                "worst_coarser_tiles": r.worst_coarser,
            },
            "convergence": {
                "kept": check(self.convergence),
                "settled": self.settled.settled,
                "seconds": self.settled.seconds,
                "bound_seconds": self.obligations.bound,
            },
            "budget": {
                "kept": check(self.budget),
                "worst_resident_bytes": r.worst_resident_bytes,
                "budget_bytes": self.obligations.resident_budget,
                "frames_over": r.over_budget_frames,
                "worst_gpu_bytes": r.worst_gpu_bytes,
                "worst_imagery_bytes": r.worst_imagery_bytes,
                "worst_imagery_textures": r.worst_imagery_textures,
            },
            "warm_fetches": self.warm_fetches.map(|c| serde_json::json!({
                "kept": c.kept,
                "store_misses": r.store_misses,
                "store_hits": r.store_hits,
            })),
            "frames": {
                "drawn": r.drawn,
                "steps": r.frames,
                "seconds": r.seconds,
                "median_ms": r.median_frame * 1000.0,
                "slowest_ms": r.slowest_frame * 1000.0,
            },
        })
    }
}

impl State {
    /// A state as [`State::to_json`] wrote it: what a session log holds.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let f = |key: &str| v.get(key).and_then(serde_json::Value::as_f64);
        let view = v.get("view")?;
        let g = |key: &str| view.get(key).and_then(serde_json::Value::as_f64);
        let size = v.get("size")?.as_array()?;
        Some(Self {
            frame: v.get("frame")?.as_u64()?,
            seconds: f("seconds")?,
            took: f("took").unwrap_or(0.0),
            drew: v.get("drew")?.as_bool()?,
            view: View {
                lon: g("lon")?,
                lat: g("lat")?,
                altitude: g("altitude")?,
                heading: g("heading")?,
                pitch: g("pitch")?,
            },
            size: (
                u32::try_from(size.first()?.as_u64()?).ok()?,
                u32::try_from(size.get(1)?.as_u64()?).ok()?,
            ),
            scale: f("scale").unwrap_or(1.0),
            eyes: v
                .get("eyes")?
                .as_array()?
                .iter()
                .map(|e| EyeReading {
                    ground: e
                        .get("ground")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    black: e
                        .get("black")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                })
                .collect(),
            metrics: serde_json::from_value(v.get("metrics")?.clone()).ok()?,
            observed: v
                .get("observed")
                .and_then(|o| serde_json::from_value(o.clone()).ok())
                .unwrap_or_default(),
        })
    }
}

/// Every step's state of the session log at `path`, in order.
pub fn states_of(path: &Path) -> Result<Vec<State>, String> {
    let messages = tuile_tape::messages(path).map_err(|e| e.to_string())?;
    messages
        .iter()
        .filter(|m| m.topic == "/state")
        .map(|m| {
            serde_json::from_slice::<serde_json::Value>(&m.data)
                .ok()
                .and_then(|v| State::from_json(&v))
                .ok_or_else(|| format!("a state in {} does not read", path.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn state(frame: u64, drew: bool, black: u64, lost: u64, resident: u64) -> State {
        State {
            frame,
            seconds: frame as f64 / 60.0,
            took: 0.01,
            drew,
            view: View::default(),
            size: (64, 64),
            scale: 1.0,
            eyes: vec![EyeReading { ground: 100, black }],
            metrics: tuile_core::metrics::Snapshot {
                frame_lost: lost,
                frame_exact: 4,
                resident_bytes: resident,
                ..Default::default()
            },
            observed: BTreeMap::new(),
        }
    }

    fn settled(seconds: f64) -> Settled {
        Settled {
            frames: 10,
            seconds,
            settled: true,
        }
    }

    const HELD: Obligations = Obligations {
        bound: 2.0,
        resident_budget: 1000,
        warm: false,
    };

    /// A clean run keeps everything; each fault breaks exactly its own
    /// obligation, with its numbers.
    #[test]
    fn each_fault_breaks_its_own_obligation_and_says_by_how_much() {
        let clean: Vec<State> = (0..10).map(|n| state(n, true, 0, 0, 500)).collect();
        assert!(Verdict::of(&clean, settled(1.0), HELD).kept());

        let mut black = clean.clone();
        black[3].eyes[0].black = 42;
        let v = Verdict::of(&black, settled(1.0), HELD);
        assert!(!v.kept() && !v.black.kept && v.lost.kept && v.budget.kept);
        assert_eq!((v.report.black_frames, v.report.worst_black), (1, 42));

        let mut lost = clean.clone();
        lost[5].metrics.frame_lost = 3;
        let v = Verdict::of(&lost, settled(1.0), HELD);
        assert!(!v.lost.kept && v.black.kept);
        assert_eq!(v.report.worst_lost, 3);

        let mut over = clean.clone();
        over[7].metrics.resident_bytes = 2000;
        let v = Verdict::of(&over, settled(1.0), HELD);
        assert!(!v.budget.kept);
        assert_eq!(v.report.over_budget_frames, 1);

        let v = Verdict::of(&clean, settled(2.5), HELD);
        assert!(!v.convergence.kept && v.black.kept);
        let unsettled = Settled {
            settled: false,
            ..settled(1.0)
        };
        assert!(!Verdict::of(&clean, unsettled, HELD).convergence.kept);
    }

    /// A frame that was not drawn is held to nothing a picture is; a run
    /// that drew nothing kept nothing.
    #[test]
    fn frames_not_drawn_are_not_pictures_and_a_run_must_draw() {
        let hidden: Vec<State> = (0..5).map(|n| state(n, false, 9, 9, 0)).collect();
        let v = Verdict::of(&hidden, settled(1.0), HELD);
        assert!(v.black.kept && v.lost.kept);
        assert!(!v.kept(), "nothing was drawn");
    }

    /// A warm relaunch is held to fetch nothing the store holds.
    #[test]
    fn a_warm_relaunch_that_fetches_is_a_violation() {
        let mut states: Vec<State> = (0..3).map(|n| state(n, true, 0, 0, 0)).collect();
        states[2].metrics.store_misses = 12;
        let warm = Obligations { warm: true, ..HELD };
        let v = Verdict::of(&states, settled(1.0), warm);
        assert_eq!(v.warm_fetches, Some(Check { kept: false }));
        assert_eq!(v.report.store_misses, 12);
        assert!(Verdict::of(&states, settled(1.0), HELD)
            .warm_fetches
            .is_none());
    }

    /// A run's verdict can be had again from its session log alone.
    #[test]
    fn a_session_log_is_judged_as_the_run_was() {
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("run.mcap");
        let mut states: Vec<State> = (0..6).map(|n| state(n, true, 0, 0, 500)).collect();
        states[4].metrics.frame_lost = 2;
        let mut tape = tuile_tape::Tape::recording(&path).expect("records");
        for s in &states {
            tape.push(tuile_tape::Frame {
                position: [7.0e6, 0.0, 0.0],
                direction: [-1.0, 0.0, 0.0],
                up: [0.0, 0.0, 1.0],
                fovy: 0.8,
            });
            tape.push_json("/state", "test", "{}", &s.to_json().to_string());
        }
        tape.finish().expect("closes");
        let read = states_of(&path).expect("states");
        assert_eq!(read, states);
        let v = Verdict::of(&read, settled(1.0), HELD);
        assert!(!v.lost.kept);
        assert_eq!(v.report.worst_lost, 2);
    }
}
