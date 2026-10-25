// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes `src/today.tables`: a clear day's Hillaire tables, computed, for
//! `LutAtmosphere::today` to read instead of computing them again. Run it
//! when the model or its defaults change; the test
//! `todays_tables_are_what_the_model_computes` says when it is due.
//!
//! ```text
//! cargo run --release -p tuile-atmosphere --example bake_today_tables
//! ```

use tuile_atmosphere::{AtmosphereParams, LutAtmosphere};

fn main() -> std::io::Result<()> {
    let tables = LutAtmosphere::new(AtmosphereParams::default()).table_bytes();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/today.tables");
    std::fs::write(&path, &tables)?;
    println!("{} bytes to {}", tables.len(), path.display());
    Ok(())
}
