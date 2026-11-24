// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The screen-space error by view depth (Laurent, 2026-11-24): the
//! simulation's traversal prices a tile at its depth along the view axis,
//! as the real one does, so the levels change in bands parallel to the
//! screen rather than in rings round the eye; and the report of what that
//! costs on the four device profiles.

use tuile_budget::Class;

use super::motion::fast_dive;
use super::patterns::look_around;
use super::scenarios::home;
use super::server::{Profile, Sim};
use super::world::{seen, sse_max, tile, Camera, Lens, View, CULL_MARGIN, SIDE};

const DEG: f64 = std::f64::consts::PI / 180.0;

/// **Two tiles at one view depth get the same refinement**, however far
/// apart across the screen: dead ahead and 20° to the side, 40 km in
/// front of an eye 20 km up looking 30° down, the same level's tiles of
/// one column are as deep, and their errors are the same to the bit. Red
/// with the distance to the eye.
#[test]
fn two_tiles_at_one_view_depth_are_refined_alike() {
    let cam = Camera {
        x: 0.6 * SIDE,
        y: 0.5 * SIDE,
        h: 20_000.0,
        heading: 0.0,
        pitch: 30.0 * DEG,
    };
    let views = [View {
        cam,
        lens: Lens::desktop(),
    }];
    let side = SIDE / 1024.0;
    let x = ((cam.x + 40_000.0) / side) as u64;
    let y = (cam.y / side) as u64;
    let ahead = tile(10, x, y);
    let aside = tile(10, x, y + (15_000.0 / side) as u64);
    assert!(seen(&views, ahead, CULL_MARGIN) && seen(&views, aside, CULL_MARGIN));
    let (a, b) = (sse_max(&views, ahead), sse_max(&views, aside));
    assert_eq!(a.to_bits(), b.to_bits(), "ahead {a} px, aside {b} px");
}

/// One run's numbers.
struct Row {
    tiles_mean: f64,
    tiles_max: usize,
    listed_mean: f64,
    gpu_peak: u64,
    network: u64,
    work_mean: f64,
    work_max: u32,
    wasted: u64,
    thrash: u64,
    flapping: usize,
    steps: usize,
}

fn measure(profile: Profile, seed: u64, script: &dyn Fn(&mut Sim) -> Vec<(usize, usize)>) -> Row {
    let mut sim = Sim::new(profile, seed);
    let needs = script(&mut sim);
    let steps = sim.log.len().max(1);
    let gpu = |l: &super::server::StepLog| -> u64 { l.stats.gpu.iter().map(|(_, b)| b.0).sum() };
    Row {
        tiles_mean: needs.iter().map(|n| n.0 as f64).sum::<f64>() / needs.len().max(1) as f64,
        tiles_max: needs.iter().map(|n| n.0).max().unwrap_or(0),
        listed_mean: needs.iter().map(|n| n.1 as f64).sum::<f64>() / needs.len().max(1) as f64,
        gpu_peak: sim.log.iter().map(gpu).max().unwrap_or(0),
        network: sim.net.network_bytes,
        work_mean: sim.log.iter().map(|l| f64::from(l.stats.work)).sum::<f64>() / steps as f64,
        work_max: sim.log.iter().map(|l| l.stats.work).max().unwrap_or(0),
        wasted: sim
            .log
            .iter()
            .map(|l| {
                Class::ALL
                    .iter()
                    .map(|c| u64::from(l.stats.wasted_starts[*c]))
                    .sum::<u64>()
            })
            .sum(),
        thrash: sim.log.iter().map(|l| u64::from(l.stats.thrash)).sum(),
        flapping: sim.worst_flapping(),
        steps,
    }
}

/// Steps `secs` seconds of `script`, counting each step's drawn tiles and
/// listed tiles.
fn run(sim: &mut Sim, secs: f64, script: &dyn Fn(f64) -> Camera) -> Vec<(usize, usize)> {
    let end = sim.now.0 + (secs * 1e6) as u64;
    let mut out = Vec::new();
    while sim.now.0 < end {
        let t = sim.now.0 as f64 / 1e6;
        let (need, _) = sim.step(script(t));
        out.push((need.drawn.len(), need.entries().count()));
    }
    out
}

/// The before/after table of the view-depth pricing: per device profile
/// and camera script, the tiles the view draws (mean, peak), the tiles the
/// need lists, the GPU bytes at their peak, the network bytes, the budget's
/// work per step (mean, peak), the wasted starts, the thrash and the worst
/// flapping. Holes are not a column: `Sim::check` fails the run on the
/// first (I1, I17), so a row printed is a run without one.
///
/// ```text
/// cargo test --release -p tuile-budget --test budget_sim depth_report -- --ignored --nocapture
/// ```
#[test]
#[ignore = "a report: --ignored --nocapture, in release"]
fn depth_report() {
    type Script = Box<dyn Fn(&mut Sim) -> Vec<(usize, usize)>>;
    let scripts: Vec<(&str, Script)> = vec![
        ("still", Box::new(|s: &mut Sim| run(s, 6.0, &|_| home()))),
        ("dive", Box::new(|s: &mut Sim| run(s, 6.0, &fast_dive))),
        (
            "yaw",
            Box::new(|s: &mut Sim| {
                let mut n = run(s, 2.0, &|_| home());
                let from = s.now.0 as f64 / 1e6;
                n.extend(run(s, 3.0, &move |t| Camera {
                    heading: (300.0 * DEG * (t - from)).min(360.0 * DEG),
                    ..home()
                }));
                n
            }),
        ),
        ("look", Box::new(|s: &mut Sim| run(s, 6.0, &look_around))),
    ];
    eprintln!(
        "| profile | script | steps | tiles mean | tiles peak | listed mean | GPU peak MiB | network MiB | work mean | work peak | wasted | thrash | flapping |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for (i, profile) in [
        Profile::desktop(),
        Profile::handheld(),
        Profile::web(),
        Profile::headset(90),
    ]
    .into_iter()
    .enumerate()
    {
        for (j, (name, script)) in scripts.iter().enumerate() {
            let p = profile.clone();
            let label = p.name;
            let r = measure(p, 900 + 10 * i as u64 + j as u64, script.as_ref());
            eprintln!(
                "| {label} | {name} | {} | {:.1} | {} | {:.1} | {:.1} | {:.1} | {:.0} | {} | {} | {} | {} |",
                r.steps,
                r.tiles_mean,
                r.tiles_max,
                r.listed_mean,
                r.gpu_peak as f64 / 1_048_576.0,
                r.network as f64 / 1_048_576.0,
                r.work_mean,
                r.work_max,
                r.wasted,
                r.thrash,
                r.flapping,
            );
        }
    }
}
