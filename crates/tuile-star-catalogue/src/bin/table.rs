// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The committed Parquet catalogue into the binned table the shaders read
//! (`tuile-atmosphere/src/stars.table`): see the crate's documentation.

use tuile_atmosphere::stars::StarTable;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [from, to] = args.as_slice() else {
        eprintln!("usage: table <bright_stars.parquet> <stars.table>");
        std::process::exit(2);
    };
    let rows = tuile_star_catalogue::read_parquet(from.as_ref()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let stars: Vec<_> = rows.iter().map(|r| r.star()).collect();
    let table = StarTable::of(&stars);
    std::fs::write(to, table.to_bytes()).unwrap_or_else(|e| {
        eprintln!("{to}: {e}");
        std::process::exit(1);
    });
    eprintln!(
        "{} stars, {} x {} texels into {to}",
        stars.len(),
        table.width,
        table.height
    );
}
