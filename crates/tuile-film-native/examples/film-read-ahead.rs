// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a moving film of references costs the render, frame by frame: the
//! film of `tests/support/moving.rs`, written on a disk at the size the
//! environment says, rendered on the machine's adapter to an AV1 film, an
//! observer taking each frame's timings.
//!
//! ```text
//! READ_AHEAD_FRAMES=60 READ_AHEAD_ACROSS=6 READ_AHEAD_DOWN=4 READ_AHEAD_CELLS=4 \
//!   cargo run --release -p tuile-film-native --example film-read-ahead -- film.mp4
//! ```
//!
//! Says where each frame's milliseconds went — what the frame waited on
//! that came from the store (reads, meshes built, imagery decoded) apart
//! from what is the GPU's — the bytes asked of the store's cache and
//! fetched behind it, and the process's peak resident memory.

#[path = "../tests/support/moving.rs"]
mod moving;

use std::time::Instant;

use tuile_film_native::{render, Av1Film, FrameOut, Observer, Order, Timings, Tone};

fn knob(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Each frame's timings and the tiles it entered.
#[derive(Default)]
struct Frames(Vec<(u32, usize, Timings)>);

impl Observer for Frames {
    fn frame(&mut self, frame: &FrameOut<'_>) {
        self.0.push((frame.frame, frame.entered, frame.timings));
    }
}

/// The peak resident memory, where the system says it (Linux).
fn peak_mib() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024.0)
}

/// What came from the store: read, built, decoded on the render's thread.
fn store_side(t: &Timings) -> f64 {
    t.read_terrain + t.read_imagery + t.read_pack + t.mesh + t.decode
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "film-read-ahead.mp4".into());
    let shape = moving::Shape {
        frames: knob("READ_AHEAD_FRAMES", 60),
        across: knob("READ_AHEAD_ACROSS", 6),
        down: knob("READ_AHEAD_DOWN", 4),
        cells: knob("READ_AHEAD_CELLS", 4),
        grid: knob("READ_AHEAD_GRID", 33),
        viewport: (
            knob("READ_AHEAD_WIDTH", 640),
            knob("READ_AHEAD_HEIGHT", 360),
        ),
        composed_side: knob("READ_AHEAD_SIDE", 512),
        height: f64::from(knob("READ_AHEAD_HEIGHT_M", 9000)),
    };
    let dir = tempfile::tempdir()?;
    let t = Instant::now();
    let written = moving::write(dir.path(), shape).await;
    println!(
        "store: {} terrain tiles, {} imagery tiles ({:.1} MB of JPEG), written in {:.1} s; {shape:?}",
        written.terrain_tiles,
        written.imagery_tiles,
        written.imagery_bytes as f64 / 1e6,
        t.elapsed().as_secs_f64()
    );
    let order = Order {
        supersample: 1,
        tone: Tone::Off,
        ..Order::default()
    };
    let mut frames = Frames::default();
    let mut sink = Av1Film::at(&out, 0);
    let began = Instant::now();
    let done = render(
        &written.sources,
        &written.film,
        &order,
        &mut (),
        &mut sink,
        &mut frames,
    )
    .await?;
    let seconds = began.elapsed().as_secs_f64();
    println!(
        "rendered {} frames {}×{} in {seconds:.2} s ({:.1} ms a frame), {:.2} s before the first",
        done.frames,
        done.width,
        done.height,
        seconds * 1000.0 / f64::from(done.frames.max(1)),
        done.setup_seconds
    );

    let (opening, rest) = frames.0.split_first().ok_or("no frame")?;
    let mut after = Timings::default();
    for (_, _, t) in rest {
        after.add(t);
    }
    let n = rest.len().max(1) as f64;
    let entered: usize = rest.iter().map(|(_, e, _)| e).sum();
    println!(
        "\nstep              opening frame   per frame after ({} frames, {:.1} tiles entered each)",
        rest.len(),
        entered as f64 / n
    );
    for ((name, first), (_, total)) in opening.2.steps().iter().zip(after.steps()) {
        println!("{name:<18}{first:>10.1} ms {:>14.2} ms", total / n);
    }
    println!(
        "{:<18}{:>10.1} ms {:>14.2} ms",
        "all steps",
        opening.2.total(),
        after.total() / n
    );
    println!(
        "{:<18}{:>10.1} ms {:>14.2} ms   (reads, meshes and decodes on the render's thread)",
        "store side",
        store_side(&opening.2),
        store_side(&after) / n
    );
    let worst = rest
        .iter()
        .map(|(f, _, t)| (store_side(t), *f))
        .fold((0.0f64, 0u32), |a, b| if b.0 > a.0 { b } else { a });
    println!(
        "worst frame after the first: {:.1} ms of store side (frame {})",
        worst.0, worst.1
    );
    let (asked, fetched) = written.sources.archives.reads();
    println!(
        "archives: {} reads ({:.1} MB) asked of the cache, {} ({:.1} MB) fetched behind it",
        asked.reads,
        asked.bytes as f64 / 1e6,
        fetched.reads,
        fetched.bytes as f64 / 1e6
    );
    if let Some(peak) = peak_mib() {
        println!("peak resident memory: {peak:.0} MiB");
    }
    println!("film: {out}");
    Ok(())
}
