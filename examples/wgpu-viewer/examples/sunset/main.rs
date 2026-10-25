// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A sunset, headless, on the real terrain and imagery: the viewer's own
//! frame at one place, its clock set to one instant after another from
//! before the sun goes down to after it — each frame a PNG, and a contact
//! sheet of them all.
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example sunset -- \
//!     --lon 0.1418 --lat 42.9365 --altitude 2900 --heading 290 --pitch 3 \
//!     --date 2026-06-21 --from -30m --to 45m --every 2m --out videos/sunset-pic-du-midi
//! ```
//!
//! The instants are the ephemeris's: the sunset is `Sun::next_sunset` over
//! the place on that date (UTC noon on), and `--from`/`--to` are durations
//! from it. The token is read from `TUILE_ION_TOKEN` and only the catalog
//! proxy served in this process holds it (see `../headless/catalog.rs`).

// The runner's proxy; what only the runner reads of it is unused here.
#[allow(dead_code)]
#[path = "../headless/catalog.rs"]
mod catalog;

use std::path::PathBuf;

use tuile_atmosphere::clock::{format_rfc3339, parse_duration, parse_rfc3339};
use tuile_viewer::control::{Action, Headless, Start, View};

struct Asked {
    view: View,
    date: String,
    from: f64,
    to: f64,
    every: f64,
    out: PathBuf,
    size: (u32, u32),
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
    let duration = |flag: &str, or: &str| parse_duration(flags.get(flag).copied().unwrap_or(or));
    Ok(Asked {
        view: View {
            lon: number("--lon", 0.1418)?,
            lat: number("--lat", 42.9365)?,
            altitude: number("--altitude", 2_900.0)?,
            heading: number("--heading", 290.0)?,
            pitch: number("--pitch", 3.0)?,
        },
        date: flags
            .get("--date")
            .copied()
            .unwrap_or("2026-06-21")
            .to_owned(),
        from: duration("--from", "-30m")?,
        to: duration("--to", "45m")?,
        every: duration("--every", "2m")?,
        out: PathBuf::from(flags.get("--out").copied().unwrap_or("videos/sunset")),
        size: (
            number("--width", 960.0)? as u32,
            number("--height", 540.0)? as u32,
        ),
    })
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

fn run(asked: &Asked) -> Result<(), String> {
    let place = tuile_core::geo::Geodetic {
        lon: asked.view.lon.to_radians(),
        lat: asked.view.lat.to_radians(),
        height: 0.0,
    };
    let noon = parse_rfc3339(&format!("{}T12:00:00Z", asked.date))?;
    let sunset = tuile_atmosphere::Sun::next_sunset(noon, place)
        .ok_or("the sun does not set there that day")?;
    println!(
        "sunset {} (the ephemeris, sea-level horizon)",
        format_rfc3339(sunset)
    );
    std::fs::create_dir_all(&asked.out).map_err(|e| e.to_string())?;
    let proxy = catalog::Proxy::serve()?;
    let dir = tempfile::Builder::new()
        .prefix("tuile-sunset-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let mut run = Headless::start(Start {
        view: asked.view,
        size: asked.size,
        scale: 1.0,
        lit_at: sunset + asked.from,
        ..Start::new(
            proxy.host(dir.path().join("http")),
            dir.path().join("tiles"),
        )
    })
    .map_err(|e| e.to_string())?;
    let settled = run.settle(120.0);
    let _ = run.wait_until(60.0, tuile_viewer::control::State::idle);
    println!("settled: {} in {:.1} s", settled.settled, settled.seconds);
    let mut frames = Vec::new();
    let mut t = asked.from;
    let mut n = 0;
    while t <= asked.to + 1e-6 {
        let instant = sunset + t;
        run.act(&Action::Time { instant, rate: 0.0 });
        // A light worked out again takes a frame; its sky-view another.
        run.steps(4);
        let (pixels, (w, h)) = run.picture(0).ok_or("the eye drew nothing")?;
        let path = asked.out.join(format!("{n:03}.png"));
        image::save_buffer(&path, pixels, w, h, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let black = run.state().black();
        println!(
            "{n:03} {} ({:+.0} min from sunset){}",
            format_rfc3339(instant),
            t / 60.0,
            if black > 0 {
                format!(" — {black} black pixels")
            } else {
                String::new()
            }
        );
        frames.push(image::RgbaImage::from_raw(w, h, pixels.to_vec()).ok_or("a frame")?);
        t += asked.every;
        n += 1;
    }
    sheet(&frames, &asked.out.join("sheet.png"))?;
    run.finish()?;
    Ok(())
}

/// Every frame, small, in reading order: one picture to look at.
fn sheet(frames: &[image::RgbaImage], path: &std::path::Path) -> Result<(), String> {
    let Some(first) = frames.first() else {
        return Ok(());
    };
    let (w, h) = (first.width() / 3, first.height() / 3);
    let columns = 6u32;
    let rows = (frames.len() as u32).div_ceil(columns);
    let mut out = image::RgbaImage::new(w * columns, h * rows);
    for (i, frame) in frames.iter().enumerate() {
        let small = image::imageops::resize(frame, w, h, image::imageops::FilterType::Triangle);
        let (x, y) = ((i as u32 % columns) * w, (i as u32 / columns) * h);
        image::imageops::overlay(&mut out, &small, i64::from(x), i64::from(y));
    }
    out.save(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("sheet: {}", path.display());
    Ok(())
}
