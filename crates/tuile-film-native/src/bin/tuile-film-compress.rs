// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's final stage by hand: one film in, delivered by a profile; or a
//! film described, as a viewer's player would find it.
//!
//! ```text
//! tuile-film-compress <in> <out> <delivery> [--cap-mb <n>] [--fps <n>]
//! tuile-film-compress --probe <film>
//! ```
//!
//! `<delivery>` is `h264`, `hevc-main10`, `mezzanine` or `copy`
//! (`tuile_farm::compress::Delivery::named`). `<in>` and `<out>` are paths,
//! or `store:<key>` in the run store the environment names
//! (`TUILE_STORE_*`, as for `tuile-film-produce`). The encode runs on the
//! NVIDIA card `TUILE_CUDA_DEVICE` (0 unless said).
//!
//! `--probe` says what a player finds: the order of the top boxes (the
//! index first is a film that plays from its first bytes), the codec, its
//! size, bit depth and colour, the frames, the key frames, the length, the
//! bitrate, and whether pictures are reordered.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use tuile_farm::{ObjectRunStore, RunStore};
use tuile_film_native::Error;

/// What a player finds in a film.
fn probe(path: &Path) -> Result<String, Error> {
    let bytes = std::fs::read(path)?;
    let mut order = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let size32 = u32::from_be_bytes(bytes[at..at + 4].try_into()?) as u64;
        let size = match size32 {
            1 => u64::from_be_bytes(bytes[at + 8..at + 16].try_into()?),
            0 => (bytes.len() - at) as u64,
            n => n,
        } as usize;
        order.push(String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned());
        if size < 8 {
            break;
        }
        at += size;
    }
    let track =
        tuile_mp4::film::read_track(&mut std::io::Cursor::new(&bytes))?.ok_or("no video track")?;
    let (codec, depth, colr) = match &track.entry {
        tuile_mp4::film::Entry::Avc1(e) => (
            format!(
                "H.264 profile {} level {}",
                e.avcc.avc_profile_indication, e.avcc.avc_level_indication
            ),
            e.avcc.ext.as_ref().map_or(8, |x| x.bit_depth_luma),
            e.colr.clone(),
        ),
        tuile_mp4::film::Entry::Hvc1(e) => (
            format!(
                "HEVC profile {} level {}{}",
                e.hvcc.general_profile_idc,
                e.hvcc.general_level_idc,
                if e.lhvc.is_some() {
                    ", two layers (MV-HEVC)"
                } else {
                    ""
                }
            ),
            e.hvcc.bit_depth_luma_minus8 + 8,
            e.colr.clone(),
        ),
        tuile_mp4::film::Entry::Av01(e) => (
            "AV1".to_string(),
            if e.av1c.high_bitdepth { 10 } else { 8 },
            e.colr.clone(),
        ),
        _ => ("other".to_string(), 0, None),
    };
    let (w, h) = track.width_height().unwrap_or((0, 0));
    let frames = track.samples.len();
    let keys = track.samples.iter().filter(|s| s.key).count();
    let seconds = track.seconds();
    let reordered = track.samples.iter().any(|s| s.composition != 0);
    let fps = track.samples.first().map_or(0.0, |s| {
        f64::from(track.timescale) / f64::from(s.duration.max(1))
    });
    Ok(format!(
        "{}: {:.1} MB, boxes {}{}\n  {codec}, {w}×{h}, {depth} bits, colour {}\n  {frames} frames at {fps:.2} fps, {seconds:.3} s, {keys} key frames (one every {:.2} s), {:.2} Mb/s{}",
        path.display(),
        bytes.len() as f64 / 1e6,
        order.join(" "),
        if order.iter().position(|b| b == "moov") < order.iter().position(|b| b == "mdat") {
            " (faststart)"
        } else {
            " (index last)"
        },
        match colr {
            Some(tuile_mp4::film::Colr::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range_flag,
            }) => format!(
                "nclx {colour_primaries}/{transfer_characteristics}/{matrix_coefficients} {}",
                if full_range_flag { "full" } else { "limited" }
            ),
            Some(other) => format!("{other:?}"),
            None => "unsaid".into(),
        },
        seconds / keys.max(1) as f64,
        bytes.len() as f64 * 8.0 / seconds.max(1e-3) / 1e6,
        if reordered { ", pictures reordered (B-frames)" } else { "" },
    ))
}

/// A path, or a run store key behind `store:`.
enum Place {
    Path(PathBuf),
    Store(String),
}

impl Place {
    fn of(word: &str) -> Self {
        match word.strip_prefix("store:") {
            Some(key) => Place::Store(key.to_string()),
            None => Place::Path(word.into()),
        }
    }
}

async fn run(args: &[String]) -> Result<(), Error> {
    if let [flag, film] = args {
        if flag == "--probe" {
            println!("{}", probe(Path::new(film))?);
            return Ok(());
        }
    }
    let [input, output, delivery, rest @ ..] = args else {
        return Err("usage: tuile-film-compress <in> <out> <delivery> [--cap-mb <n>] [--fps <n>] | --probe <film>".into());
    };
    let mut cap = None;
    let mut fps = None;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        let value = words
            .next()
            .ok_or_else(|| format!("{word} needs a value"))?;
        match word.as_str() {
            "--cap-mb" => cap = Some((value.parse::<f64>()? * 1e6) as u64),
            "--fps" => fps = Some(value.parse::<u32>()?),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let delivery = tuile_farm::compress::Delivery::named(delivery, cap)
        .ok_or_else(|| format!("no delivery named {delivery}"))?;
    let scratch = tempfile::tempdir()?;
    let mut store = None;
    let open_store = || -> Result<ObjectRunStore, Error> { Ok(ObjectRunStore::from_env()?) };
    let local_in = match Place::of(input) {
        Place::Path(path) => path,
        Place::Store(key) => {
            let at = scratch.path().join("in.mp4");
            let s = open_store()?;
            s.get(&key, &at).await?;
            store = Some(s);
            at
        }
    };
    let fps = match fps {
        Some(fps) => fps,
        None => {
            let track = tuile_mp4::film::read_track(&mut std::fs::File::open(&local_in)?)?
                .ok_or("no video track")?;
            let first = track.samples.first().ok_or("a film of no frame")?;
            (f64::from(track.timescale) / f64::from(first.duration.max(1))).round() as u32
        }
    };
    let (local_out, key) = match Place::of(output) {
        Place::Path(path) => (path, None),
        Place::Store(key) => (scratch.path().join("out.mp4"), Some(key)),
    };
    let device = std::env::var("TUILE_CUDA_DEVICE")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    let done =
        tuile_film_native::compress::compress(&local_in, &local_out, &delivery, fps, device)?;
    println!(
        "{}: {} frames, {:.1} MB, {} pass(es), {:.1} s",
        delivery.key(),
        done.frames,
        done.bytes as f64 / 1e6,
        done.passes,
        done.seconds
    );
    println!("{}", probe(&local_in)?);
    println!("{}", probe(&local_out)?);
    if let Some(key) = key {
        let s = match store {
            Some(s) => s,
            None => open_store()?,
        };
        s.put(&local_out, &key).await?;
        println!("→ store:{key}");
    }
    Ok(())
}

fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tuile-film-compress: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tuile-film-compress: {e}");
            ExitCode::FAILURE
        }
    }
}
