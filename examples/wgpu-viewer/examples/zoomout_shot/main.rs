// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Pictures of an aggressive zoom-out, headless, each tile's levels written
//! on it (`tuile://view?labels=on`): the eye low over the ground (level 13
//! or so under it), still long enough for the background preloads, then out
//! to the whole globe in a few seconds, a picture at each stage
//! (`docs/explorations/budget-progressive-coarse-preload.md`).
//!
//! ```text
//! cargo run --release -p tuile-wgpu-viewer --example zoomout_shot -- \
//!     --lon 6.86 --lat 45.83 --altitude 3000 --out videos/zoomout-shot
//! ```
//!
//! Writes `<out>/0-start.png` (settled low), `<out>/1-…png` to `<out>/6-…png`
//! (the zoom-out, the altitude doubled every `--step` seconds, a picture at
//! every other doubling), `<out>/7-window.png` (the budget window at the
//! end) and prints, for each, the budget's line and the black pixels.

use std::path::PathBuf;
use std::sync::Arc;

use tuile_atmosphere::clock::parse_rfc3339;
use tuile_viewer::control::{Action, Headless, Start, View};

struct Asked {
    view: View,
    at: f64,
    out: PathBuf,
    size: (u32, u32),
    hold: f64,
    step: f64,
}

fn read(args: &[String]) -> Result<Asked, String> {
    let mut flags = std::collections::BTreeMap::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        flags.insert(flag.as_str(), value.as_str());
    }
    let number = |flag: &str, or: f64| -> Result<f64, String> {
        flags.get(flag).map_or(Ok(or), |v| {
            v.parse()
                .map_err(|_| format!("{flag} wants a number, got {v:?}"))
        })
    };
    Ok(Asked {
        view: View {
            lon: number("--lon", 6.86)?,
            lat: number("--lat", 45.83)?,
            altitude: number("--altitude", 3000.0)?,
            heading: number("--heading", 0.0)?,
            pitch: number("--pitch", 0.0)?,
        },
        at: parse_rfc3339(flags.get("--at").copied().unwrap_or("2026-11-20T12:00:00Z"))?,
        out: PathBuf::from(flags.get("--out").copied().unwrap_or("videos/zoomout-shot")),
        size: (
            number("--width", 1280.0)? as u32,
            number("--height", 800.0)? as u32,
        ),
        hold: number("--hold", 30.0)?,
        step: number("--step", 0.4)?,
    })
}

fn line() -> String {
    match tuile_core::metrics::budget() {
        None => "budget: not yet".to_owned(),
        Some(s) => format!(
            "lost {} · bare {} · under need {} mesh, {} imagery · floor ≤{:?}",
            s.lost_tiles, s.bare_items, s.under_need_tiles[0], s.under_need_tiles[1], s.floor_level,
        ),
    }
}

fn save(run: &Headless, asked: &Asked, name: &str) -> Result<(), String> {
    let (pixels, (w, h)) = run.picture(0).ok_or("the eye drew nothing")?;
    let path = asked.out.join(format!("{name}.png"));
    image::save_buffer(&path, pixels, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!(
        "{}: {}, {} black pixels",
        path.display(),
        line(),
        run.state().black()
    );
    Ok(())
}

fn run(asked: &Asked) -> Result<(), String> {
    std::fs::create_dir_all(&asked.out).map_err(|e| e.to_string())?;
    let dir = tempfile::Builder::new()
        .prefix("tuile-zoomout-shot-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let mut run = Headless::start(Start {
        view: asked.view,
        size: asked.size,
        scale: 1.0,
        lit_at: asked.at,
        ..Start::new(
            Arc::new(tuile_wgpu_viewer::Public),
            dir.path().join("tiles"),
        )
    })
    .map_err(|e| e.to_string())?;
    run.act(&Action::Url("tuile://view?labels=on".to_owned()));
    run.settle(120.0);
    run.act(&Action::Go(asked.view));
    run.settle(120.0);
    // Still: the background preloads bring the neighbourhood in.
    run.run_for(asked.hold);
    save(&run, asked, "0-start")?;
    let mut altitude = asked.view.altitude;
    let mut shot = 1;
    for doubling in 1..=12 {
        altitude *= 2.0;
        run.act(&Action::Go(View {
            altitude,
            ..asked.view
        }));
        run.run_for(asked.step);
        if doubling % 2 == 0 {
            save(&run, asked, &format!("{shot}-{:.0}km", altitude / 1000.0))?;
            shot += 1;
        }
    }
    run.act(&Action::Url("tuile://view?labels=off".to_owned()));
    run.act(&Action::Url("tuile://budget?window=on".to_owned()));
    run.run_for(1.0);
    save(&run, asked, &format!("{shot}-window"))?;
    run.finish().map(|_| ())
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error,foyer_storage=off".into()),
        )
        .try_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let asked = read(&args).unwrap_or_else(|why| {
        eprintln!("{why}");
        std::process::exit(2);
    });
    if let Err(why) = run(&asked) {
        eprintln!("{why}");
        std::process::exit(1);
    }
}
