// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What reading a tile costs, by where it was found.
//!
//! A read goes down the places a tile can be until one answers: this
//! process's buffer, a projection on local disk, the absence store, then the
//! zone's archives — local copies, or the bucket. Each place asked records
//! once, at the point that knows it was asked:
//!
//! - `tuile_tiles_fetch_seconds{source, kind}`, a histogram: how long it took
//!   to answer, whatever it answered;
//! - `tuile_tiles_fetch_total{source, kind, outcome}`: `hit` (it had the
//!   tile), `miss` (it did not: the next place is asked), `absent` (it knows
//!   the tile does not exist), `error`;
//! - `tuile_tiles_fetch_bytes_total{source, kind}`: the bytes of the tiles it
//!   had.
//!
//! `source` is `buffer`, `projection`, `absences`, `disk` or `bucket`;
//! `kind` is `mesh` or `texture`. A buffer is a map in memory looked up by
//! every read: only what it answers is recorded, or every read would count
//! twice. Below the store, a source's own transport records `cache` and
//! `upstream` under the same names (`tuile-native-fetchers`).
//!
//! The histogram's buckets are the recorder's to set:
//! [`tuile_core::metrics::TILE_FETCH_SECONDS_BUCKETS`] is what they are
//! meant to be.

use std::time::Instant;

/// What a place asked for a tile answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Hit,
    Miss,
    Absent,
    Error,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Absent => "absent",
            Self::Error => "error",
        }
    }
}

/// A place being asked for a tile of `kind`, since it was.
pub(crate) struct Asked {
    started: Instant,
    kind: &'static str,
}

impl Asked {
    pub(crate) fn now(kind: &'static str) -> Self {
        Self {
            started: Instant::now(),
            kind,
        }
    }

    /// `source` answered `outcome`, with `bytes` of tile.
    pub(crate) fn answered(self, source: &'static str, outcome: Outcome, bytes: usize) {
        let kind = self.kind;
        metrics::histogram!("tuile_tiles_fetch_seconds", "source" => source, "kind" => kind)
            .record(self.started.elapsed().as_secs_f64());
        metrics::counter!(
            "tuile_tiles_fetch_total",
            "source" => source,
            "kind" => kind,
            "outcome" => outcome.label()
        )
        .increment(1);
        if bytes > 0 {
            metrics::counter!("tuile_tiles_fetch_bytes_total", "source" => source, "kind" => kind)
                .increment(bytes as u64);
        }
    }
}
