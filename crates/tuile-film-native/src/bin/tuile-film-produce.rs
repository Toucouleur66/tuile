// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A whole film from a tape of cameras, in one command.
//!
//! ```text
//! tuile-film-produce flight.mcap
//! tuile-film-produce flight.mcap --resolution 3840x2160 --fps 30 --sse 3 --imagery 3954
//! ```
//!
//! The tape's frames are cut in packs and each pack in parts; each pack is
//! baked, each part rendered to a segment, several packs at once
//! (`--at-once`), and the segments are joined. All of it in this process —
//! the bake and the render are the engine's own, called — and all of it kept
//! in the run store: the packs under `packs/<scene>…/`, the parts and the
//! film under `films/<scene>…/<look>/`, where the scene names everything a
//! pack depends on and the look everything a picture does beyond its pack.
//!
//! So what was made is found: a pack already baked with these parameters is
//! not baked again, a part already rendered with them is not rendered
//! again, and the same command stopped and asked again goes on from where
//! it was. `--bake-again` and `--render-again` say otherwise. Changing a
//! parameter of the picture renders another film from the same packs;
//! changing one of the bake bakes other packs.
//!
//! The stores are the environment's:
//!
//! ```text
//! TUILE_STORE_ENDPOINT, TUILE_STORE_BUCKET,
//! TUILE_STORE_ACCESS_KEY_ID, TUILE_STORE_SECRET_ACCESS_KEY   the run store, a bucket
//! TUILE_STORE_DIR                                            …or a directory, instead
//! TUILE_TILES_REMOTE (+ _HEADER, _SECRET) or TUILE_TILES_BUCKET   the tile store
//! TUILE_ION_TOKEN                                            the bake's sources
//! ```
//!
//! Nothing is left on this machine: steps write to a scratch directory, and
//! what the render reads of the stores is cached under `--cache`, both gone
//! when the film is made unless `--cache` names a directory to keep.
//!
//! Exit codes: 0 the film is in the store (its key is printed, `FILM <key>`),
//! 1 it is not (the parts missing are named), 2 usage, 3 stopped — by an
//! object named `STOP` under the film's prefix, or by a step that did not
//! return within `--step-minutes`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tuile_bake::tape::TapeBake;
use tuile_farm::produce::{produce, Host, Journal, Order, ProduceError, Stretch};
use tuile_farm::{ObjectRunStore, RunStore};
use tuile_film::Haze;
use tuile_film_native::engine::{Engine, Rendering};
use tuile_film_native::setup::{rgb, tile_store};
use tuile_film_native::{Error, Tone};

#[derive(Parser)]
#[command(
    name = "tuile-film-produce",
    about = "Bake, render in parts and join a whole film, kept in the run store"
)]
struct Args {
    /// The tape of cameras: one frame a camera.
    tape: PathBuf,
    /// First and last frame, `first:last`. Default: the whole tape.
    #[arg(long, value_parser = stretch)]
    frames: Option<Stretch>,

    // ── what is baked ──────────────────────────────────────────────────
    /// The picture's size, `<width>x<height>`.
    #[arg(long, default_value = "1920x1080", value_parser = resolution)]
    resolution: (u32, u32),
    /// The selection's largest screen-space error, in pixels. Default: the
    /// engine's.
    #[arg(long)]
    sse: Option<f64>,
    /// The imagery layer, as its source's asset; negative for none.
    /// Default: the engine's aerial imagery.
    #[arg(long, allow_negative_numbers = true)]
    imagery: Option<i64>,
    /// The terrain layer, as its source's asset. Default: the engine's.
    #[arg(long)]
    terrain: Option<i64>,
    /// What a pack carries of its tiles: `references` into the tile store,
    /// the tiles themselves (`embedded`), or `both`.
    #[arg(long, default_value = "references", value_parser = content)]
    content: tuile_pack::Content,

    // ── how it is rendered ─────────────────────────────────────────────
    /// Frames a second.
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// The picture's size against the size baked for.
    #[arg(long, default_value_t = 1.0)]
    scale: f32,
    /// Samples a pixel, a side.
    #[arg(long, default_value_t = 2)]
    supersample: u32,
    /// The encoder: `h264`, `av1`, `nvenc-h264`, `nvenc-av1`. Default: the
    /// best this build has.
    #[arg(long)]
    codec: Option<String>,
    /// Megabits a second.
    #[arg(long, default_value_t = 12.0)]
    mbps: f64,
    /// The sun, `<azimuth>,<elevation>` in degrees, where the film begins.
    #[arg(long, value_parser = pair, allow_hyphen_values = true)]
    sun: Option<(f64, f64)>,
    /// Stops of exposure.
    #[arg(long, allow_negative_numbers = true)]
    exposure: Option<f32>,
    #[arg(long)]
    contrast: Option<f32>,
    #[arg(long)]
    saturation: Option<f32>,
    /// How much of the sun a shadow takes, 0 to 1.
    #[arg(long)]
    shadow: Option<f32>,
    /// The air's extinction at the ground, per metre: asks for the air.
    #[arg(long)]
    haze_density: Option<f32>,
    /// The height over which the air thins by a factor of e, in metres.
    #[arg(long)]
    haze_height: Option<f32>,
    /// The air's colour at the horizon, `<r>,<g>,<b>` linear.
    #[arg(long)]
    haze_horizon: Option<String>,
    /// The sky's colour straight up, `<r>,<g>,<b>` linear.
    #[arg(long)]
    haze_zenith: Option<String>,
    /// The sun's light, `<r>,<g>,<b>` linear.
    #[arg(long)]
    sun_light: Option<String>,
    /// The sky's light, `<r>,<g>,<b>` linear.
    #[arg(long)]
    sky_light: Option<String>,
    /// The imagery as stored, without the film's tone correction.
    #[arg(long)]
    no_tone: bool,

    // ── how it is produced ─────────────────────────────────────────────
    /// Frames to a pack.
    #[arg(long, default_value_t = 450)]
    pack_frames: u32,
    /// Frames to a part: never more than a pack's. Each part pays its own
    /// start — its tiles read, its first frame — so few long parts are
    /// rendered sooner than many short ones.
    #[arg(long, default_value_t = 450)]
    part_frames: u32,
    /// Packs worked on at once, each on a thread: so many bakes or renders
    /// run together. A 4K render holds 4 to 6.5 GB.
    #[arg(long, default_value_t = 1)]
    at_once: usize,
    /// Times a bake or a render is tried.
    #[arg(long, default_value_t = 3)]
    tries: u32,
    /// Minutes a bake or a render may take before the production stops.
    #[arg(long)]
    step_minutes: Option<u64>,
    /// Bake the packs the parts to render need, even those already baked.
    #[arg(long)]
    bake_again: bool,
    /// Render every part, even those already rendered.
    #[arg(long)]
    render_again: bool,
    /// Where what the render reads of the stores is kept while it runs.
    /// Default: a scratch directory, gone at the end.
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Also bring the film here once it is in the store.
    #[arg(long)]
    download: Option<PathBuf>,
}

fn stretch(text: &str) -> Result<Stretch, String> {
    let (first, last) = text.split_once(':').ok_or("expected <first>:<last>")?;
    let number = |n: &str| n.parse::<u32>().map_err(|e| format!("{n}: {e}"));
    let frames = Stretch {
        first: number(first)?,
        last: number(last)?,
    };
    if frames.first == 0 || frames.first > frames.last {
        return Err(format!(
            "{}:{} is not a range of frames",
            frames.first, frames.last
        ));
    }
    Ok(frames)
}

fn resolution(text: &str) -> Result<(u32, u32), String> {
    let (width, height) = text.split_once('x').ok_or("expected <width>x<height>")?;
    let side = |n: &str| match n.parse::<u32>() {
        Ok(side) if side > 0 => Ok(side),
        _ => Err(format!("{n} is not a number of pixels")),
    };
    Ok((side(width)?, side(height)?))
}

fn pair(text: &str) -> Result<(f64, f64), String> {
    let (a, b) = text
        .split_once(',')
        .ok_or("expected <azimuth>,<elevation>")?;
    let degrees = |n: &str| n.trim().parse::<f64>().map_err(|e| format!("{n}: {e}"));
    Ok((degrees(a)?, degrees(b)?))
}

fn content(text: &str) -> Result<tuile_pack::Content, String> {
    match text {
        "references" => Ok(tuile_pack::Content::References),
        "embedded" => Ok(tuile_pack::Content::Embedded),
        "both" => Ok(tuile_pack::Content::Both),
        other => Err(format!(
            "a pack carries references, embedded or both, not {other}"
        )),
    }
}

/// How the film is rendered, as the command line says.
fn rendering(args: &Args) -> Result<Rendering, Error> {
    let mut order = tuile_film_native::Order {
        scale: args.scale,
        supersample: args.supersample.max(1),
        fps: args.fps.max(1),
        ..Default::default()
    };
    if args.no_tone {
        order.tone = Tone::Off;
    }
    let look = &mut order.look;
    if let Some(stops) = args.exposure {
        look.exposure_ev = stops;
    }
    if let Some(power) = args.contrast {
        look.contrast = power;
    }
    if let Some(factor) = args.saturation {
        look.saturation = factor;
    }
    if let Some(strength) = args.shadow {
        look.shadow = strength;
    }
    if let Some(colour) = &args.sun_light {
        look.sun = rgb("--sun-light", colour)?;
    }
    if let Some(colour) = &args.sky_light {
        look.world = rgb("--sky-light", colour)?;
    }
    // Naming anything of the air asks for the air.
    if args.haze_density.is_some()
        || args.haze_height.is_some()
        || args.haze_horizon.is_some()
        || args.haze_zenith.is_some()
    {
        let haze = look.haze.get_or_insert_with(Haze::default);
        if let Some(density) = args.haze_density {
            haze.density = density;
        }
        if let Some(height) = args.haze_height {
            haze.scale_height = height;
        }
        if let Some(colour) = &args.haze_horizon {
            haze.horizon = rgb("--haze-horizon", colour)?;
        }
        if let Some(colour) = &args.haze_zenith {
            haze.zenith = rgb("--haze-zenith", colour)?;
        }
    }
    Ok(Rendering {
        order,
        codec: args.codec.clone(),
        bitrate: (args.mbps * 1e6) as u32,
        sun: args.sun,
    })
}

/// What it came to, as an exit code.
enum Ended {
    Film,
    NoFilm,
    Usage,
    Stopped,
}

async fn run(args: Args) -> Result<Ended, Error> {
    let poses = TapeBake::poses_of(&args.tape)?;
    let whole = Stretch {
        first: 1,
        last: poses.len() as u32,
    };
    let frames = args.frames.unwrap_or(whole);
    if frames.last > whole.last {
        eprintln!(
            "tuile-film-produce: frames {}:{} of a tape that holds {}",
            frames.first, frames.last, whole.last
        );
        return Ok(Ended::Usage);
    }
    let rendering = rendering(&args)?;
    let bake = TapeBake {
        token: std::env::var("TUILE_ION_TOKEN")
            .map_err(|_| "TUILE_ION_TOKEN is not set; the bake needs it")?,
        cache_dir: std::env::var("TUILE_CACHE_DIR").ok().map(Into::into),
        poses,
        viewport: (f64::from(args.resolution.0), f64::from(args.resolution.1)),
        sse: args.sse,
        terrain: args.terrain,
        imagery: args.imagery,
        content: args.content,
    };

    // The run store: a bucket, unless the environment names a directory.
    let store = Arc::new(ObjectRunStore::from_env()?);
    // What the render reads is kept here while the film is made.
    let scratch = tempfile::tempdir()?;
    let cache = args
        .cache
        .clone()
        .unwrap_or_else(|| scratch.path().to_path_buf());
    let tiles = tile_store(&cache).await?;
    let engine = Engine::open(
        bake,
        rendering,
        frames.first,
        store.clone(),
        tiles.objects.clone(),
        &cache,
    )
    .await?;
    let host: Arc<dyn Host> = Arc::new(engine);

    let order = Order {
        pack_frames: args.pack_frames,
        part_frames: args.part_frames,
        at_once: args.at_once,
        tries: args.tries,
        step_timeout: args
            .step_minutes
            .map(|minutes| Duration::from_secs(minutes * 60)),
        fps: Some(f64::from(args.fps.max(1))),
        bake_again: args.bake_again,
        render_again: args.render_again,
        ..Order::of(frames)
    };
    let journal = Journal::default();
    let made = produce(&order, store.as_ref(), host, &journal).await;
    if let Some(blocks) = &tiles.block_store {
        // What it has not written by now is downloaded again next time.
        let _ = blocks.close().await;
    }
    match made {
        Ok(film) => {
            if let Some(dest) = &args.download {
                store.get(&film.key, dest).await?;
                println!("FILM-HERE {}", dest.display());
            }
            println!("FILM {} {} frames", film.key, film.frames);
            Ok(Ended::Film)
        }
        Err(ProduceError::Order(why)) => {
            eprintln!("tuile-film-produce: {why}");
            Ok(Ended::Usage)
        }
        Err(e @ (ProduceError::Stopped(_) | ProduceError::Stalled { .. })) => {
            println!("FILM-STOPPED {e}");
            Ok(Ended::Stopped)
        }
        Err(e) => {
            println!("FILM-FAILED {e}");
            Ok(Ended::NoFilm)
        }
    }
}

fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tuile-film-produce: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let ended = runtime.block_on(run(args));
    // A step that did not return holds its thread for ever: the runtime is
    // not waited for.
    runtime.shutdown_background();
    match ended {
        Ok(Ended::Film) => ExitCode::SUCCESS,
        Ok(Ended::NoFilm) => ExitCode::FAILURE,
        Ok(Ended::Usage) => ExitCode::from(2),
        Ok(Ended::Stopped) => ExitCode::from(3),
        Err(e) => {
            eprintln!("tuile-film-produce: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parsed(line: &str) -> Result<Args, clap::Error> {
        Args::try_parse_from(line.split_whitespace())
    }

    #[test]
    fn the_command_line_is_one_clap_accepts() {
        Args::command().debug_assert();
    }

    #[test]
    fn a_tape_alone_is_a_film_with_every_default() {
        let args = parsed("tuile-film-produce flight.mcap").expect("parse");
        assert_eq!(args.frames, None);
        assert_eq!(args.resolution, (1920, 1080));
        assert_eq!(
            (
                args.fps,
                args.pack_frames,
                args.part_frames,
                args.at_once,
                args.tries
            ),
            (30, 450, 450, 1, 3)
        );
        assert_eq!(args.content, tuile_pack::Content::References);
        assert!(!args.bake_again && !args.render_again);
        assert!(args.imagery.is_none() && args.sse.is_none() && args.cache.is_none());
        let rendering = rendering(&args).expect("rendering");
        assert_eq!(rendering.bitrate, 12_000_000);
        assert!(rendering.order.look.haze.is_none());
    }

    #[test]
    fn what_is_said_of_the_film_is_what_it_is_made_with() {
        let args = parsed(
            "tuile-film-produce flight.mcap --frames 12800:14599 --resolution 3840x2160 --fps 25 \
             --sse 3 --imagery 3954 --terrain 1 --content both --sun 255,10 --exposure -0.5 \
             --shadow 1 --haze-horizon 1.0,0.42,0.16 --sun-light 3.2,1.7,0.8 --mbps 45 \
             --codec h264 --no-tone --at-once 2 --render-again --step-minutes 30",
        )
        .expect("parse");
        assert_eq!(
            args.frames,
            Some(Stretch {
                first: 12800,
                last: 14599
            })
        );
        assert_eq!(args.resolution, (3840, 2160));
        assert_eq!(
            (args.sse, args.imagery, args.terrain),
            (Some(3.0), Some(3954), Some(1))
        );
        assert_eq!(args.content, tuile_pack::Content::Both);
        assert!(args.render_again && !args.bake_again);
        assert_eq!((args.at_once, args.step_minutes), (2, Some(30)));
        let rendering = rendering(&args).expect("rendering");
        assert_eq!(rendering.order.fps, 25);
        assert_eq!(rendering.sun, Some((255.0, 10.0)));
        assert_eq!(rendering.bitrate, 45_000_000);
        assert_eq!(rendering.codec.as_deref(), Some("h264"));
        assert!(matches!(rendering.order.tone, Tone::Off));
        let look = rendering.order.look;
        assert_eq!((look.exposure_ev, look.shadow), (-0.5, 1.0));
        assert_eq!(look.sun.to_array(), [3.2, 1.7, 0.8]);
        // Naming one end of the air asks for the air, the rest of it as it is.
        let haze = look.haze.expect("haze");
        assert_eq!(haze.horizon.to_array(), [1.0, 0.42, 0.16]);
        assert_eq!(haze.density, Haze::default().density);
        // No imagery at all is a negative asset.
        assert_eq!(
            parsed("tuile-film-produce t.mcap --imagery -1")
                .expect("parse")
                .imagery,
            Some(-1)
        );
    }

    #[test]
    fn what_cannot_be_a_film_is_refused_at_the_command_line() {
        for line in [
            "tuile-film-produce",
            "tuile-film-produce t.mcap --frames 9:3",
            "tuile-film-produce t.mcap --frames 0:3",
            "tuile-film-produce t.mcap --resolution 1920",
            "tuile-film-produce t.mcap --resolution 0x1080",
            "tuile-film-produce t.mcap --content pictures",
            "tuile-film-produce t.mcap --sun 255",
        ] {
            assert!(parsed(line).is_err(), "{line}");
        }
        let args = parsed("tuile-film-produce t.mcap --sky-light 1,2").expect("parse");
        assert!(rendering(&args).is_err());
    }
}
