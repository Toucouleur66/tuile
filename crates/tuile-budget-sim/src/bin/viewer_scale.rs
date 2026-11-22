// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What one `decide` costs at the viewer's scale (tens of thousands of
//! tracked resources), not the bench's thousand: the desktop on a large
//! device, warmed by a long tour so that its tracked set grows as the
//! viewer's does, then the aggressive sequence played again and again with
//! every `decide` timed.
//!
//! ```text
//! cargo run --release -p tuile-budget-sim --bin tuile-budget-viewer-scale -- \
//!     [--gib 8] [--warm 300] [--loops 3] [--top 0] [--digest]
//! ```
//!
//! Prints the tracked resources, then `decide` mean / p50 / p99 / max (µs,
//! the thread's CPU time where the platform has a thread clock — a loaded
//! machine inflates the wall time alone; `--wall` for the wall time),
//! the work units and the heap blocks per step, in motion. `--top n` lists
//! the `n` slowest steps with what they did; `--digest` folds every step's
//! decision digest into one (the same inputs give the same number: an
//! optimisation that keeps it is bit-identical on this run). Run it under
//! `sample` (macOS) or `perf` for a per-function profile.

use tuile_budget::{BudgetConfig, Bytes};
use tuile_budget_sim::catalogue::{desktop_on, named};
use tuile_budget_sim::{Counting, Sim};

/// Every heap block `decide` asks for is counted (armed around the call
/// alone, by the simulation).
#[global_allocator]
static HEAP: Counting = Counting;

fn arg(name: &str, or: f64) -> f64 {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(or)
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

fn pct<T: Copy + Default>(v: &[T], p: usize) -> T {
    v.get(v.len() * p / 100).copied().unwrap_or_default()
}

fn main() {
    let gib = arg("--gib", 8.0) as u64;
    let warm = arg("--warm", 300.0);
    let loops = arg("--loops", 3.0) as usize;
    let top = arg("--top", 0.0) as usize;
    let mut cfg = BudgetConfig::interactive(Bytes::gib(gib));
    cfg.decoded_target = Bytes::gib(gib / 2);
    let mut sim = Sim::unchecked(desktop_on(cfg), 7);
    let tour = tuile_budget_sim::catalogue::tour;
    sim.run(warm, &tour);
    let s = named("aggressive-sequence").expect("in the catalogue");
    let start = sim.now;
    let first = sim.decide_us.len();
    // `--pause-at t [--pause-loop k]`: loop `k` (the first by default) runs
    // to `t` seconds into the sequence,
    // waits five seconds (to attach a profiler to the steps that follow),
    // then runs 50 ms more and stops.
    let pause = arg("--pause-at", -1.0);
    for k in 0..loops {
        let t0 = sim.now.0 as f64 / 1e6;
        let path = s.path;
        let seconds = s.seconds;
        if pause >= 0.0 && k == arg("--pause-loop", 0.0) as usize {
            sim.run(pause, &move |t| path((t - t0).clamp(0.0, seconds)));
            eprintln!("paused, pid {}", std::process::id());
            std::thread::sleep(std::time::Duration::from_secs_f64(arg("--pause-for", 5.0)));
            sim.run(0.05, &move |t| path((t - t0).clamp(0.0, seconds)));
            let n = sim.decide_cpu_us.len();
            for (now, u) in &sim.decide_cpu_us[n.saturating_sub(6)..] {
                eprintln!("  t {:.3} s: {u:.0} µs", now.0 as f64 / 1e6);
            }
            for l in &sim.log[sim.log.len().saturating_sub(6)..] {
                eprintln!(
                    "  t {:.3} s: digest {:016x}",
                    l.now.0 as f64 / 1e6,
                    l.digest
                );
            }
            return;
        }
        sim.run(seconds + 4.0, &move |t| path((t - t0).clamp(0.0, seconds)));
        eprintln!(
            "loop {k}: tracked {}",
            sim.log.last().map_or(0, |l| l.stats.tracked)
        );
    }
    let logs: Vec<_> = sim.log.iter().filter(|l| l.now >= start).collect();
    // The thread's CPU time where the platform has a thread clock, the
    // wall time otherwise (`--wall` forces it).
    let cpu = !flag("--wall") && sim.decide_cpu_us.iter().any(|(_, u)| *u > 0.0);
    let timed = if cpu {
        &sim.decide_cpu_us[first..]
    } else {
        &sim.decide_us[first..]
    };
    let allocs = &sim.decide_allocs[first..];
    let mut us: Vec<f64> = timed.iter().map(|(_, u)| *u).collect();
    us.sort_by(f64::total_cmp);
    let mean = us.iter().sum::<f64>() / us.len().max(1) as f64;
    let mut w: Vec<u32> = logs.iter().map(|l| l.stats.work).collect();
    w.sort_unstable();
    let mut a: Vec<u64> = allocs.iter().map(|x| x.heap.allocs).collect();
    a.sort_unstable();
    let amean = a.iter().sum::<u64>() as f64 / a.len().max(1) as f64;
    let mut tr: Vec<u32> = logs.iter().map(|l| l.stats.tracked).collect();
    tr.sort_unstable();
    println!(
        "viewer scale ({gib} GiB, warm {warm} s): tracked {} (p50 {}, max {}), steps {}; \
         decide ({}) mean {mean:.0} µs, p50 {:.0}, p99 {:.0}, max {:.0}; \
         work p50 {}, p99 {}, max {}; allocs mean {amean:.1}, p50 {}, p99 {}, max {}",
        sim.log.last().map_or(0, |l| l.stats.tracked),
        pct(&tr, 50),
        tr.last().copied().unwrap_or(0),
        us.len(),
        if cpu { "thread CPU" } else { "wall" },
        pct(&us, 50),
        pct(&us, 99),
        us.last().copied().unwrap_or(0.0),
        pct(&w, 50),
        pct(&w, 99),
        w.last().copied().unwrap_or(0),
        pct(&a, 50),
        pct(&a, 99),
        a.last().copied().unwrap_or(0),
    );
    if flag("--digest") {
        // FNV-1a over every step's digest, warm-up included.
        let d = sim.log.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, l| {
            (h ^ l.digest).wrapping_mul(0x0100_0000_01b3)
        });
        println!("digest {d:016x} over {} steps", sim.log.len());
    }
    if flag("--digests-near") {
        let at = arg("--near", 329.4);
        for l in sim
            .log
            .iter()
            .filter(|l| (l.now.0 as f64 / 1e6 - at).abs() < 0.05)
        {
            println!(
                "  t {:.3} s: digest {:016x}",
                l.now.0 as f64 / 1e6,
                l.digest
            );
        }
    }
    if top > 0 {
        let mut order: Vec<usize> = (0..timed.len()).collect();
        order.sort_by(|&i, &j| timed[j].1.total_cmp(&timed[i].1));
        for &i in order.iter().take(top) {
            let (now, u) = timed[i];
            let st = logs.iter().find(|l| l.now == now).map(|l| &l.stats);
            if let Some(st) = st {
                println!(
                    "  t {:.3} s: {u:.0} µs, work {}, allocs {} ({} KiB, peak {} KiB), tracked {}, levels_moved {}, \
                     heap {}, starts {}, evicted {}, released {}, delivered {}, lost {}, \
                     fallback {}, sse {}",
                    now.0 as f64 / 1e6,
                    st.work,
                    allocs[i].heap.allocs,
                    allocs[i].heap.bytes / 1024,
                    allocs[i].heap.peak / 1024,
                    st.tracked,
                    st.levels_moved,
                    st.heap_len,
                    st.started_network + st.started_local,
                    st.evicted,
                    st.released,
                    st.delivered,
                    st.lost,
                    st.fallback,
                    st.sse_scale_milli,
                );
            }
        }
    }
}
