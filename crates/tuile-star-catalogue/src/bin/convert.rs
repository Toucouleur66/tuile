// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The CDS's V/50 `catalog.gz` into the committed Parquet catalogue: see
//! the crate's documentation.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [from, to] = args.as_slice() else {
        eprintln!("usage: convert <catalog.gz> <bright_stars.parquet>");
        std::process::exit(2);
    };
    let rows = tuile_star_catalogue::read_cds_gz(from.as_ref()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    tuile_star_catalogue::write_parquet(&rows, to.as_ref()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    eprintln!("{} stars into {to}", rows.len());
}
