// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod corners;
mod field;
mod film;
mod grade;
mod levels;
mod linear;
mod matrix;
mod measure;
mod stats;
mod tiles;
mod transfer;

pub use corners::{CornerField, CornerReport, CornerTrace, FieldBounds};
pub use field::{apply, GainField, LATTICE, STOPS_PER_UNIT};
pub use film::{Bounds, FilmGrade, LookTarget, Pictured};
pub use grade::{region_key, Grade, GradeReport, LevelGrades, Seen, REGION_LEVEL};
pub use levels::{apply_multipliers, tone_of, LevelGains, LevelParams, Observation, PairReport};
pub use linear::{Fit, Line, Paired, PAIRS};
pub use matrix::{through, Affine, Look, MatrixBounds, MatrixField, MatrixReport, SAME};
pub use measure::{Blended, Curves, Limits, Local, Measure, KNOTS};
pub use stats::{linear_of, BlockStats, BLOCKS};
pub use tiles::{
    Apart, Edge, Junction, Observed, TileAt, TileBounds, TileGains, TileReport, TileSeen,
    TileTrace, Tones, BINS, GRID,
};
pub use transfer::{transfer, Params, Transfer};
