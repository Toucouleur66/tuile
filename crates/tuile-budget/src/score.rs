// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The progressive score made readable
//! (`docs/explorations/budget-progressive-coarse-preload.md` §7): its terms
//! for one resource ([`Terms`]), their aggregates per level, distance band
//! and side ([`ScoreBoard`]), one record per decision ([`ScoreEvent`]) and
//! a periodic picture of the GPU-held set ([`HeldReport`]).
//!
//! One source: the budget fills them while it decides, and the budget
//! window, the metrics and the log records all read these, never a
//! recomputation of their own. Everything is an integer, in the decider's
//! units: `A`, `D` and `P` in [`CERTAIN`] units, `R` in µs, bytes in bytes,
//! the score in µflouz per reference byte ([`crate::value::FlouzPerByte`]'s
//! unit: `P × R × ref_bytes / bytes`, µs at certainty for a resource of the
//! reference size).

use crate::stats::LEVELS;
use crate::types::{Class, ResourceId};
use crate::value::CERTAIN;

/// Distance bands of the per-level breakdowns: `d` (tiles of the
/// resource's own level from the view's footprint) under 1, under 2,
/// under 3, under 4, and 4 or more. A resource the traversal did not place
/// (the present need, a flat-anticipation preload) is in band 0.
pub const DISTANCE_BANDS: usize = 5;

/// Labels of the distance bands, as the metrics and the window print them.
pub const DISTANCE_BAND_LABELS: [&str; DISTANCE_BANDS] = ["0", "1", "2", "3", "4+"];

/// The band of a distance in sixteenths of a tile
/// ([`crate::Quantised::away`]).
pub fn distance_band(away: u16) -> usize {
    if away == crate::priority::Quantised::NO_PLACE {
        return 0;
    }
    usize::from(away / 16).min(DISTANCE_BANDS - 1)
}

/// Which side of the decider a number is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    /// A preload started (the value band's anticipation).
    Preload,
    /// A GPU copy evicted.
    Evict,
    /// A GPU copy held.
    Held,
}

impl Side {
    /// Every side, in index order.
    pub const ALL: [Side; 3] = [Side::Preload, Side::Evict, Side::Held];

    /// Its label (`side` in telemetry).
    pub fn label(self) -> &'static str {
        match self {
            Side::Preload => "preload",
            Side::Evict => "evict",
            Side::Held => "held",
        }
    }
}

/// Sides of a breakdown.
pub const SIDES: usize = 3;

/// The terms of one resource's score.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Terms {
    /// `A(ℓ, n)`, [`CERTAIN`] units.
    pub a: u32,
    /// `D(d)`, [`CERTAIN`] units.
    pub d: u32,
    /// `P = A × D`, [`CERTAIN`] units.
    pub p: u32,
    /// `R(tier)`: the restore cost, µs.
    pub r_us: u64,
    /// `bytes(t)`: the GPU copy's bytes (a load's: the reference size).
    pub bytes: u64,
    /// `P × R × ref_bytes / bytes`, µflouz per reference byte.
    pub score: u64,
}

impl Terms {
    /// The terms of a resource of probability `a × d`, restored in `r_us`,
    /// of `bytes` (floored at `ref_bytes / 16`, as the release key does).
    pub fn new(a: u32, d: u32, p: u32, r_us: u64, bytes: u64, ref_bytes: u64) -> Self {
        let size = bytes.max(ref_bytes / 16).max(1);
        let score = u128::from(p.min(CERTAIN)) * u128::from(r_us) * u128::from(ref_bytes)
            / (u128::from(CERTAIN) * u128::from(size));
        Terms {
            a,
            d,
            p,
            r_us,
            bytes,
            score: u64::try_from(score).unwrap_or(u64::MAX),
        }
    }
}

/// Buckets of the score histograms: bucket `i` counts scores in
/// `[2^(i-1), 2^i)` µflouz per reference byte (bucket 0: under 1; the
/// last: the rest).
pub const SCORE_BUCKETS: usize = 24;

/// The bucket of a score.
pub fn score_bucket(score: u64) -> usize {
    let b = 64 - score.leading_zeros() as usize;
    b.min(SCORE_BUCKETS - 1)
}

/// The count, sum, least and greatest of one term over a cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Agg {
    /// Values summed.
    pub sum: u64,
    /// The least, when the cell counts any.
    pub min: u64,
    /// The greatest.
    pub max: u64,
}

impl Agg {
    fn put(&mut self, v: u64, first: bool) {
        self.sum = self.sum.saturating_add(v);
        self.min = if first { v } else { self.min.min(v) };
        self.max = self.max.max(v);
    }
}

/// One cell of a [`ScoreBoard`]: the resources of one level, distance band
/// and side, and their terms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Resources counted.
    pub count: u32,
    /// The score and each term.
    pub score: Agg,
    pub a: Agg,
    pub d: Agg,
    pub r_us: Agg,
    pub bytes: Agg,
    /// The scores, [`SCORE_BUCKETS`].
    pub histogram: [u32; SCORE_BUCKETS],
}

/// The terms a [`Cell`] aggregates, in [`Cell::term`]'s order, as the
/// metrics label them (`term`): the score in µflouz per reference byte,
/// `A` and `D` in [`CERTAIN`] units, `R` in µs, the bytes in bytes.
pub const TERM_LABELS: [&str; 5] = ["score", "a", "d", "r_us", "bytes"];

impl Cell {
    /// The aggregate of term `i` of [`TERM_LABELS`].
    pub fn term(&self, i: usize) -> &Agg {
        match i {
            0 => &self.score,
            1 => &self.a,
            2 => &self.d,
            3 => &self.r_us,
            _ => &self.bytes,
        }
    }

    /// Counts `t`.
    pub fn put(&mut self, t: &Terms) {
        let first = self.count == 0;
        self.count = self.count.saturating_add(1);
        self.score.put(t.score, first);
        self.a.put(u64::from(t.a), first);
        self.d.put(u64::from(t.d), first);
        self.r_us.put(t.r_us, first);
        self.bytes.put(t.bytes, first);
        self.histogram[score_bucket(t.score)] += 1;
    }

    /// Adds `o` (a cumulative view).
    pub fn add(&mut self, o: &Cell) {
        if o.count == 0 {
            return;
        }
        let first = self.count == 0;
        let both = |a: &mut Agg, b: &Agg| {
            a.sum = a.sum.saturating_add(b.sum);
            a.min = if first { b.min } else { a.min.min(b.min) };
            a.max = a.max.max(b.max);
        };
        both(&mut self.score, &o.score);
        both(&mut self.a, &o.a);
        both(&mut self.d, &o.d);
        both(&mut self.r_us, &o.r_us);
        both(&mut self.bytes, &o.bytes);
        for (h, x) in self.histogram.iter_mut().zip(o.histogram) {
            *h = h.saturating_add(x);
        }
        self.count = self.count.saturating_add(o.count);
    }
}

/// The score's aggregates by level ([`crate::stats::level_index`]),
/// distance band and side: every class together. Boxed in the statistics,
/// as it is large.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScoreBoard {
    cells: Vec<Cell>,
}

impl Default for ScoreBoard {
    fn default() -> Self {
        ScoreBoard {
            cells: vec![Cell::default(); LEVELS * DISTANCE_BANDS * SIDES],
        }
    }
}

impl ScoreBoard {
    fn i(level: usize, band: usize, side: Side) -> usize {
        (level.min(LEVELS - 1) * DISTANCE_BANDS + band.min(DISTANCE_BANDS - 1)) * SIDES
            + side as usize
    }

    /// The cell of `level` (index), `band`, `side`.
    pub fn cell(&self, level: usize, band: usize, side: Side) -> &Cell {
        &self.cells[Self::i(level, band, side)]
    }

    /// Counts `t` in its cell.
    pub fn put(&mut self, level: usize, band: usize, side: Side, t: &Terms) {
        self.cells[Self::i(level, band, side)].put(t);
    }

    /// Adds every cell of `o` of `side` (a cumulative view).
    pub fn add_side(&mut self, o: &ScoreBoard, side: Side) {
        for level in 0..LEVELS {
            for band in 0..DISTANCE_BANDS {
                let i = Self::i(level, band, side);
                let c = o.cells[i];
                self.cells[i].add(&c);
            }
        }
    }

    /// Empties the cells of `side`.
    pub fn clear_side(&mut self, side: Side) {
        for level in 0..LEVELS {
            for band in 0..DISTANCE_BANDS {
                self.cells[Self::i(level, band, side)] = Cell::default();
            }
        }
    }

    /// Whether it counts nothing.
    pub fn is_empty(&self) -> bool {
        self.cells.iter().all(|c| c.count == 0)
    }

    /// Every non-empty cell: level index, band, side, cell.
    pub fn cells(&self) -> impl Iterator<Item = (usize, usize, Side, &Cell)> + '_ {
        (0..LEVELS).flat_map(move |l| {
            (0..DISTANCE_BANDS).flat_map(move |b| {
                Side::ALL.into_iter().filter_map(move |s| {
                    let c = self.cell(l, b, s);
                    (c.count > 0).then_some((l, b, s, c))
                })
            })
        })
    }
}

/// Why a decision was taken, as the log records say it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// A preload started with a free slot.
    Preload,
    /// Room made for a delivery of more value.
    Admission,
    /// A preload displaced by one worth more than it by the margin.
    Swap,
    /// The sweep down to the low watermark.
    Sweep,
    /// The sweep, under a memory signal.
    Pressure,
    /// What an epoch's change left behind.
    Recycled,
}

impl Reason {
    /// Its label (`reason` in a log record).
    pub fn label(self) -> &'static str {
        match self {
            Reason::Preload => "preload",
            Reason::Admission => "admission",
            Reason::Swap => "swap",
            Reason::Sweep => "sweep",
            Reason::Pressure => "pressure",
            Reason::Recycled => "recycled",
        }
    }
}

/// One decision with its score: a preload started or a GPU copy evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScoreEvent {
    /// The resource.
    pub resource: ResourceId,
    /// Its class.
    pub class: Class,
    /// Its tile's level.
    pub level: u32,
    /// Its distance band.
    pub band: u8,
    /// The side.
    pub side: Side,
    /// Why.
    pub reason: Reason,
    /// Its terms.
    pub terms: Terms,
}

/// One GPU-held resource in a [`HeldReport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HeldTile {
    /// The resource.
    pub resource: ResourceId,
    /// Its class.
    pub class: Class,
    /// Its tile's level.
    pub level: u32,
    /// Its distance band.
    pub band: u8,
    /// Its terms.
    pub terms: Terms,
    /// What it was last: drawn or in a swap (`kept`), wanted, or left.
    pub state: HeldState,
}

/// What a held resource was at the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeldState {
    /// The floor: never released.
    Floor,
    /// Drawn or in a swap: never released.
    Kept,
    /// Wanted this step: the present need or a preload.
    Wanted,
    /// No longer wanted: released first, by its key.
    Left,
}

impl HeldState {
    /// Its label.
    pub fn label(self) -> &'static str {
        match self {
            HeldState::Floor => "floor",
            HeldState::Kept => "kept",
            HeldState::Wanted => "wanted",
            HeldState::Left => "left",
        }
    }
}

/// How many held resources a [`HeldReport`] lists at each end of the
/// score order.
pub const HELD_LISTED: usize = 32;

/// The GPU-held set as the release order sees it: its aggregates by level
/// and band (side [`Side::Held`] of `board`), and the [`HELD_LISTED`]
/// lowest and highest scored resources outside the floor (the next to go,
/// and the last).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct HeldReport {
    /// The step it was taken at.
    pub step: u64,
    /// The aggregates.
    pub board: ScoreBoard,
    /// The lowest scores, lowest first.
    pub lowest: Vec<HeldTile>,
    /// The highest scores, highest first.
    pub highest: Vec<HeldTile>,
}

/// The parameters of the score in force, for the window's header and the
/// `tuile_budget_param` gauges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScoreParams {
    /// `a0`, ‰.
    pub a0_permille: u32,
    /// `a↓`, ‰.
    pub descend_permille: u32,
    /// `ρ`, tiles.
    pub rho: u32,
    /// The margin `m`, ‰.
    pub margin_permille: u32,
    /// The floor's last level, if any.
    pub floor_level: Option<u32>,
    /// The share of each class's network slots preloads may hold, ‰.
    pub preload_slots_permille: u32,
    /// The level the view needs at its nearest point, as the need placed
    /// the last preload it listed (`n`); `None` before any.
    pub need_level: Option<u32>,
}

impl ScoreParams {
    /// `name=value` pairs, as the gauges and the header print them.
    pub fn pairs(&self) -> [(&'static str, i64); 7] {
        let opt = |v: Option<u32>| v.map_or(-1, i64::from);
        [
            ("a0_permille", i64::from(self.a0_permille)),
            ("a_desc_permille", i64::from(self.descend_permille)),
            ("rho_tiles", i64::from(self.rho)),
            ("margin_permille", i64::from(self.margin_permille)),
            ("floor_level", opt(self.floor_level)),
            ("preload_slots_permille", i64::from(self.preload_slots_permille)),
            ("need_level", opt(self.need_level)),
        ]
    }

    /// The formula with its numbers, one line.
    pub fn formula(&self) -> String {
        let floor = self
            .floor_level
            .map_or_else(|| "none".to_owned(), |f| format!("≤{f}"));
        format!(
            "score = A·D·R/bytes · A=a0·2^−(n−1−ℓ) a0={:.2} a↓={:.2} · D=ρ²/(ρ+d)² ρ={} · m={}% · floor {} · preload ≤{}% slots",
            f64::from(self.a0_permille) / 1000.0,
            f64::from(self.descend_permille) / 1000.0,
            self.rho,
            self.margin_permille / 10,
            floor,
            self.preload_slots_permille / 10,
        )
    }
}

/// One step's score numbers carried in its statistics: preloads in flight
/// and evictions by level and band, and the parameters in force. The
/// decisions themselves, each with its terms, are
/// [`crate::Decisions::scores`]; the held set is [`crate::Budget::held_report`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScoreStats {
    /// Preloads in flight after the step, by level and band.
    pub preload_in_flight: [[u32; DISTANCE_BANDS]; LEVELS],
    /// This step's GPU evictions by level and band.
    pub evictions: [[u32; DISTANCE_BANDS]; LEVELS],
    /// The parameters in force.
    pub params: ScoreParams,
}

impl Default for ScoreStats {
    fn default() -> Self {
        ScoreStats {
            preload_in_flight: [[0; DISTANCE_BANDS]; LEVELS],
            evictions: [[0; DISTANCE_BANDS]; LEVELS],
            params: ScoreParams::default(),
        }
    }
}

/// Builds a [`ScoreBoard`] of decisions: their sides' cells.
pub fn board_of(events: &[ScoreEvent], into: &mut ScoreBoard) {
    for e in events {
        into.put(
            crate::stats::level_index(e.level),
            usize::from(e.band),
            e.side,
            &e.terms,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_and_buckets() {
        assert_eq!(distance_band(0), 0);
        assert_eq!(distance_band(15), 0);
        assert_eq!(distance_band(16), 1);
        assert_eq!(distance_band(63), 3);
        assert_eq!(distance_band(400), 4);
        assert_eq!(distance_band(crate::priority::Quantised::NO_PLACE), 0);
        assert_eq!(score_bucket(0), 0);
        assert_eq!(score_bucket(1), 1);
        assert_eq!(score_bucket(u64::MAX), SCORE_BUCKETS - 1);
    }

    #[test]
    fn a_cell_sums_its_terms() {
        let mut c = Cell::default();
        c.put(&Terms::new(CERTAIN, CERTAIN, CERTAIN, 150_000, 256 << 10, 256 << 10));
        c.put(&Terms::new(CERTAIN / 2, CERTAIN, CERTAIN / 2, 150_000, 256 << 10, 256 << 10));
        assert_eq!(c.count, 2);
        assert_eq!(c.score.max, 150_000);
        assert_eq!(c.score.min, 75_000);
        assert_eq!(c.score.sum, 225_000);
        let mut d = Cell::default();
        d.add(&c);
        d.add(&c);
        assert_eq!(d.count, 4);
        assert_eq!(d.score.min, 75_000);
    }
}
