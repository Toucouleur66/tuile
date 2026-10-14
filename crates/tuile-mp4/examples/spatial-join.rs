// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Spatial films joined into one, as a film's segments are.
//!
//! ```text
//! cargo run -p tuile-mp4 --example spatial-join -- <out.mp4> <segment.mp4>…
//! ```

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    let Some((out, segments)) = args.split_first() else {
        return Err("usage: spatial-join <out.mp4> <segment.mp4>…".into());
    };
    let frames = tuile_mp4::spatial::join(segments, out)?;
    println!("{frames} frames → {}", out.display());
    Ok(())
}
