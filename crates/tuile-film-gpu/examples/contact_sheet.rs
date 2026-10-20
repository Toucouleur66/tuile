// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A contact sheet: pictures laid out in a grid, row after row, each scaled
//! to one cell, a thin dark line between them — to compare stills of one
//! view under several settings at a glance.
//!
//! ```text
//! cargo run --release -p tuile-film-gpu --example contact_sheet -- \
//!     sheet.png <columns> <cell width> a.png b.png …
//! ```
//!
//! A path given as `-` leaves its cell empty.

use image::{imageops, Rgb, RgbImage};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, columns, width, pictures @ ..] = args.as_slice() else {
        return Err("usage: contact_sheet <out.png> <columns> <cell width> <pictures…>".into());
    };
    let columns: u32 = columns.parse()?;
    let cell_w: u32 = width.parse()?;
    let first = pictures.iter().find(|p| *p != "-").ok_or("no picture")?;
    let probe = image::open(first)?;
    let cell_h = cell_w * probe.height() / probe.width();
    let rows = (pictures.len() as u32).div_ceil(columns);
    let gap = 4;
    let mut sheet = RgbImage::from_pixel(
        columns * (cell_w + gap) + gap,
        rows * (cell_h + gap) + gap,
        Rgb([24, 24, 24]),
    );
    for (i, path) in pictures.iter().enumerate() {
        if path == "-" {
            continue;
        }
        let picture = image::open(path)?.to_rgb8();
        let cell = imageops::resize(&picture, cell_w, cell_h, imageops::FilterType::Triangle);
        let (col, row) = (i as u32 % columns, i as u32 / columns);
        imageops::replace(
            &mut sheet,
            &cell,
            i64::from(gap + col * (cell_w + gap)),
            i64::from(gap + row * (cell_h + gap)),
        );
    }
    sheet.save(out)?;
    println!("{out}: {columns}×{rows} cells of {cell_w}×{cell_h}");
    Ok(())
}
