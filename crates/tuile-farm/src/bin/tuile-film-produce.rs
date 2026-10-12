// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A whole film on this machine, from one command.
//!
//! ```text
//! tuile-film-produce --frames 1:18000 --out film.mp4 --packs packs/loop --work work/loop \
//!   --bake   "tuile-bake --tape flight.mcap --frames {first}:{last} --out {out}" \
//!   --render "tuile-film-render loop --frames {first}:{last} --out {out} --codec h264"
//! ```
//!
//! The frames are cut in packs (`--pack-frames`) and each pack in parts
//! (`--part-frames`). Each pack is baked by the `--bake` command, each part
//! rendered to an H.264 segment by the `--render` command, several packs at
//! once (`--at-once`), and the segments are joined into `--out` once every
//! one is there. In both commands `{first}`, `{last}` and `{out}` stand for
//! the frames and the file to write; a command is split on spaces, with no
//! quoting. What the two commands need of the world — a store, a token — they
//! find in the environment, which is theirs as it is this program's.
//!
//! Each bake and each render is watched: killed over `--memory-mb`, or after
//! `--stall-seconds` without processor time, and tried `--tries` times. A
//! render that names tiles the store lacks is followed, before its next try,
//! by the `--on-absent` command if there is one, `{list}` standing for a file
//! of those tiles.
//!
//! Everything done is kept: asked again, the same command goes on from where
//! it was. An empty file named `STOP` in the work directory stops it between
//! two steps. The journal is `journal.log` there, and on standard output.
//!
//! Exit codes: 0 the film is written, 1 it is not (the journal says which
//! parts are missing), 2 usage, 3 stopped.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tuile_farm::produce::{produce, Journal, Order, ProduceError, Stretch, Templates};
use tuile_farm::watch::Limits;

#[derive(Parser)]
#[command(
    name = "tuile-film-produce",
    about = "Bake, render in parts and join a whole film on this machine"
)]
struct Args {
    /// First and last frame, `first:last`.
    #[arg(long, value_parser = stretch)]
    frames: Stretch,
    /// The film to write.
    #[arg(long)]
    out: PathBuf,
    /// Where the packs are kept, as `<first>-<last>.tuilepack`.
    #[arg(long)]
    packs: PathBuf,
    /// Segments, receipts, logs and the journal.
    #[arg(long)]
    work: PathBuf,
    /// The command that bakes a pack.
    #[arg(long)]
    bake: String,
    /// The command that renders a part to a segment.
    #[arg(long)]
    render: String,
    /// The command run after a render found tiles absent from the store.
    #[arg(long)]
    on_absent: Option<String>,
    #[arg(long, default_value_t = 150)]
    pack_frames: u32,
    #[arg(long, default_value_t = 50)]
    part_frames: u32,
    /// Packs worked on at once.
    #[arg(long, default_value_t = 1)]
    at_once: usize,
    #[arg(long, default_value_t = 3)]
    tries: u32,
    /// A bake or a render over this much resident memory is killed. 0: no bound.
    #[arg(long, default_value_t = 0)]
    memory_mb: u64,
    /// A bake or a render with no processor time for this long is killed. 0: never.
    #[arg(long, default_value_t = 120)]
    stall_seconds: u64,
    /// The film's cadence, checked against what was joined.
    #[arg(long)]
    fps: Option<f64>,
    /// Let the machine sleep while the film is produced.
    #[arg(long)]
    may_sleep: bool,
}

fn stretch(text: &str) -> Result<Stretch, String> {
    let (first, last) = text.split_once(':').ok_or("expected first:last")?;
    let number = |n: &str| n.parse::<u32>().map_err(|e| format!("{n}: {e}"));
    let frames = Stretch {
        first: number(first)?,
        last: number(last)?,
    };
    if frames.first > frames.last {
        return Err(format!("{} is after {}", frames.first, frames.last));
    }
    Ok(frames)
}

/// Keeps the machine awake for as long as this process lives, where the
/// system has a way to ask: a film of hours is otherwise a film of the hours
/// the machine did not sleep. Not being able to ask stops nothing.
fn stay_awake() -> Option<std::process::Child> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    std::process::Command::new("caffeinate")
        .args(["-i", "-m", "-s", "-w", &std::process::id().to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

fn main() -> ExitCode {
    let args = Args::parse();
    for (name, command) in [("--bake", &args.bake), ("--render", &args.render)] {
        if command.trim().is_empty() {
            eprintln!("tuile-film-produce: {name} is empty");
            return ExitCode::from(2);
        }
    }
    let order = Order {
        frames: args.frames,
        pack_frames: args.pack_frames,
        part_frames: args.part_frames,
        at_once: args.at_once,
        tries: args.tries,
        packs: args.packs,
        work: args.work,
        out: args.out,
        fps: args.fps,
    };
    let limits = Limits {
        memory_bytes: (args.memory_mb > 0).then(|| args.memory_mb << 20),
        stall: Duration::from_secs(args.stall_seconds),
        ..Limits::default()
    };
    let host = Templates {
        bake: args.bake,
        render: args.render,
        on_absent: args.on_absent,
    };
    let journal = match Journal::open(&order.work) {
        Ok(journal) => journal,
        Err(e) => {
            eprintln!("tuile-film-produce: {e}");
            return ExitCode::FAILURE;
        }
    };
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
    let awake = (!args.may_sleep).then(stay_awake).flatten();
    let made = runtime.block_on(produce(&order, &host, &limits, &journal));
    if let Some(mut awake) = awake {
        // It would end with this process; ended here so none is left behind
        // by a caller that waits on its children.
        let _ = awake.kill();
        let _ = awake.wait();
    }
    match made {
        Ok(film) => {
            println!("FILM {} {} frames", order.out.display(), film.frames);
            ExitCode::SUCCESS
        }
        Err(ProduceError::Stopped(_)) => ExitCode::from(3),
        Err(ProduceError::Order(why)) => {
            eprintln!("tuile-film-produce: {why}");
            ExitCode::from(2)
        }
        Err(e) => {
            println!("FILM-FAILED {e}");
            ExitCode::FAILURE
        }
    }
}
