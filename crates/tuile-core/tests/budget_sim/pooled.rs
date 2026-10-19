// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! D-A3's experiment, evidence only: the one arbiter with fixed per-class
//! targets (the interim 40/60 split of the ground's pool) against the same
//! arbiter with a pooled ledger — terrain and imagery sharing one pool,
//! each between a floor and a ceiling of it, so that the split follows the
//! view — over the motion patterns P1–P6 and the profiles.
//!
//! Every run goes through `Sim`, so every invariant is checked at every
//! step whatever the variant; the measures are the architecture
//! experiment's where they apply.
//!
//! Run: `cargo test --release -p tuile-core --features budget-experiments
//! --test budget_sim -- --ignored pooled --nocapture`.

use tuile_core::budget::imagery::{needed_level, LevelClamp};
use tuile_core::budget::{Bytes, Class, Micros, PerClass, Pool, Pressure, ResourceId, Role};

use super::motion::fast_dive;
use super::patterns::{look_around, quiet};
use super::scenarios::{approach, home, zoom};
use super::server::{Profile, Sim};
use super::world::{Camera, Lens};

/// A variant of the ground's pool.
#[derive(Clone, Copy, Debug)]
enum Variant {
    /// Fixed per-class targets: terrain 33 %, imagery 49 % of the device.
    Fixed,
    /// Pooled: terrain between `t.0` and `t.1` percent of the pool, imagery
    /// between `d.0` and `d.1`.
    Pooled { t: (u8, u8), d: (u8, u8) },
}

impl Variant {
    fn name(self) -> String {
        match self {
            Variant::Fixed => "fixed-40/60".into(),
            Variant::Pooled { t, d } => format!("pooled-T{}-{}/D{}-{}", t.0, t.1, d.0, d.1),
        }
    }

    fn apply(self, mut p: Profile) -> Profile {
        if let Variant::Pooled { t, d } = self {
            let mut floor = PerClass::splat(0u8);
            let mut ceiling = PerClass::splat(0u8);
            floor[Class::TerrainMesh] = t.0;
            ceiling[Class::TerrainMesh] = t.1;
            floor[Class::Drape] = d.0;
            ceiling[Class::Drape] = d.1;
            p.budget.pool = Some(Pool {
                floor_percent: floor,
                ceiling_percent: ceiling,
            });
        }
        p
    }
}

/// What one run measured.
#[derive(Clone, Debug, Default)]
struct Measure {
    /// Exact item-steps whose drape is coarser than the unrelaxed need by
    /// more than a level, and the exact item-steps.
    blurry: u64,
    exact: u64,
    /// From the stop to convergence; `None` if the run ended unconverged.
    converged_s: Option<f64>,
    /// Steps the pool (terrain + imagery) exceeds the fixed pool target,
    /// and the worst excess, MiB.
    over_steps: u64,
    over_mib: f64,
    flapping: usize,
    decide_mean_us: f64,
    decide_p99_us: f64,
    net_mib: f64,
    thrash: u64,
    /// The relaxation's peak, thousandths.
    scale_peak: u32,
}

fn measure(
    sim: &mut Sim,
    stop: Micros,
    steps: &[(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)],
) -> Measure {
    let cfg = sim.profile.budget.clone();
    let pool_target = cfg.target[Class::TerrainMesh].plus(cfg.target[Class::Drape]);
    let floor = cfg.pinned_level.unwrap_or(0);
    let cap = cfg.imagery_cap();
    let mut m = Measure::default();
    for (need, d) in steps {
        for (t, n) in &need.drawn {
            let Some(item) = d.draw.get(ResourceId::Mesh(*t)) else {
                continue;
            };
            if item.role != Role::Exact {
                continue;
            }
            let Some(img) = &n.imagery else { continue };
            m.exact += 1;
            let want = needed_level(
                None,
                img.footprint,
                LevelClamp {
                    floor,
                    terrain_level: n.level,
                    over_terrain_cap: cap,
                    source_max: img.layers[0].max_level,
                },
            );
            let worn = item.drapes.first().map_or(0, |x| x.level);
            if worn + 1 < want {
                m.blurry += 1;
            }
        }
        let used = d.stats.gpu[Class::TerrainMesh].plus(d.stats.gpu[Class::Drape]);
        if used > pool_target {
            m.over_steps += 1;
            m.over_mib = m
                .over_mib
                .max(used.minus(pool_target).0 as f64 / (1 << 20) as f64);
        }
        m.scale_peak = m.scale_peak.max(d.sse_scale_milli);
    }
    // Converged from some step to the end.
    let last_unconverged = sim.log.iter().rposition(|l| !l.converged);
    m.converged_s = match last_unconverged {
        None => Some(0.0),
        Some(i) if i + 1 < sim.log.len() => Some(sim.log[i + 1].now.since(stop).0 as f64 / 1e6),
        Some(_) => None,
    }
    .map(|s| s.max(0.0));
    m.flapping = sim.worst_flapping();
    let mut us: Vec<f64> = sim.decide_us.iter().map(|(_, u)| *u).collect();
    us.sort_by(f64::total_cmp);
    m.decide_mean_us = us.iter().sum::<f64>() / us.len().max(1) as f64;
    m.decide_p99_us = us.get(us.len() * 99 / 100).copied().unwrap_or(0.0);
    m.net_mib = sim.net.network_bytes as f64 / (1 << 20) as f64;
    m.thrash = sim.log.iter().map(|l| u64::from(l.stats.thrash)).sum();
    m
}

/// A row of the table: the scenario's name, its profile, its script.
type Cell = (&'static str, fn() -> Profile, Script);

/// A scenario: a name, a profile and a script that runs a simulation and
/// returns the step the camera stopped at.
type Script =
    fn(&mut Sim, &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>) -> Micros;

fn steps(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
    secs: f64,
    script: &dyn Fn(f64) -> Camera,
) {
    let end = sim.now.plus(Micros((secs * 1e6) as u64));
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        out.push(sim.step(script(t)));
    }
}

fn p1_still(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 5.0, &approach);
    let stop = sim.now;
    steps(sim, out, 8.0, &|_| home());
    stop
}

fn p2_dive(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 6.0, &fast_dive);
    let stop = sim.now;
    steps(sim, out, 6.0, &|_| fast_dive(6.0));
    stop
}

fn p3_zoom(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 17.0, &zoom);
    let stop = sim.now;
    steps(sim, out, 5.0, &zoom);
    stop
}

fn p4_yaw(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 5.0, &approach);
    let from = sim.now.0 as f64 / 1e6;
    steps(sim, out, 1.2, &|t| Camera {
        heading: (300f64.to_radians() * (t - from)).min(std::f64::consts::TAU),
        ..home()
    });
    let stop = sim.now;
    steps(sim, out, 6.0, &|_| home());
    stop
}

fn p5_rotation(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 6.0, &|_| home());
    sim.set_lens(Lens::landscape());
    steps(sim, out, 4.0, &|_| home());
    sim.set_lens(Lens::portrait());
    let stop = sim.now;
    steps(sim, out, 6.0, &|_| home());
    stop
}

fn p6_look(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 6.0, &look_around);
    let stop = sim.now;
    steps(sim, out, 6.0, &|_| home());
    stop
}

fn warning(
    sim: &mut Sim,
    out: &mut Vec<(tuile_core::budget::NeedSet, tuile_core::budget::Decisions)>,
) -> Micros {
    steps(sim, out, 8.0, &zoom);
    sim.pressure(Pressure::Warning);
    steps(sim, out, 9.0, &|t| zoom(t));
    let stop = sim.now;
    steps(sim, out, 5.0, &zoom);
    stop
}

/// D-A3: fixed per-class targets against a pooled ledger with per-class
/// floors and ceilings, over P1–P6 and the profiles; prints the table.
#[test]
#[ignore = "D-A3 experiment: cargo test --release -p tuile-core --features budget-experiments --test budget_sim -- --ignored pooled --nocapture"]
fn architecture_pooled_ledger_against_fixed_split() {
    let variants = [
        Variant::Fixed,
        Variant::Pooled {
            t: (25, 60),
            d: (40, 75),
        },
        Variant::Pooled {
            t: (20, 70),
            d: (30, 80),
        },
    ];
    let cells: Vec<Cell> = vec![
        ("P1 still", Profile::desktop, p1_still),
        ("P1 still", Profile::handheld, p1_still),
        ("P1 still", Profile::handheld_low, p1_still),
        ("P2 fast dive", Profile::desktop, p2_dive),
        ("P2 fast dive", Profile::handheld, p2_dive),
        ("P2 fast dive", Profile::handheld_low, p2_dive),
        ("P3 zoom out and back", Profile::desktop, p3_zoom),
        ("P3 zoom out and back", Profile::handheld, p3_zoom),
        ("P4 head-rate yaw", Profile::desktop, p4_yaw),
        ("P4 head-rate yaw", || Profile::headset(90), p4_yaw),
        ("P5 rotation", Profile::handheld, p5_rotation),
        ("P5 rotation", Profile::handheld_low, p5_rotation),
        ("P6 look around", || Profile::headset(90), p6_look),
        ("memory warning", Profile::handheld, warning),
    ];
    eprintln!(
        "| scenario | profile | variant | blurry % | converged s (max) | over-target steps (max MiB) | flapping | scale peak | decide mean / p99 µs | net MiB | thrash |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|---|---|---|");
    for (scenario, profile, script) in &cells {
        for v in variants {
            let mut agg = Measure::default();
            let mut conv: Vec<Option<f64>> = Vec::new();
            let seeds = [301u64, 302];
            let mut name = "";
            for seed in seeds {
                let p = v.apply(quiet(profile()));
                name = p.name;
                let mut sim = Sim::new(p, seed);
                let mut out = Vec::new();
                let stop = script(&mut sim, &mut out);
                let m = measure(&mut sim, stop, &out);
                agg.blurry += m.blurry;
                agg.exact += m.exact;
                agg.over_steps += m.over_steps;
                agg.over_mib = agg.over_mib.max(m.over_mib);
                agg.flapping = agg.flapping.max(m.flapping);
                agg.scale_peak = agg.scale_peak.max(m.scale_peak);
                agg.decide_mean_us += m.decide_mean_us / seeds.len() as f64;
                agg.decide_p99_us += m.decide_p99_us / seeds.len() as f64;
                agg.net_mib += m.net_mib / seeds.len() as f64;
                agg.thrash += m.thrash;
                conv.push(m.converged_s);
            }
            let converged = if conv.iter().all(Option::is_some) {
                format!(
                    "{:.2}",
                    conv.iter().flatten().fold(0.0f64, |a, b| a.max(*b))
                )
            } else {
                format!(
                    "never ({}/{})",
                    conv.iter().filter(|c| c.is_none()).count(),
                    conv.len()
                )
            };
            eprintln!(
                "| {scenario} | {name} | {} | {:.1} | {converged} | {} ({:.1}) | {} | {} | {:.0} / {:.0} | {:.1} | {} |",
                v.name(),
                100.0 * agg.blurry as f64 / agg.exact.max(1) as f64,
                agg.over_steps,
                agg.over_mib,
                agg.flapping,
                agg.scale_peak,
                agg.decide_mean_us,
                agg.decide_p99_us,
                agg.net_mib,
                agg.thrash
            );
        }
    }
    let _ = Bytes::ZERO;
}
