// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The scenarios of spec §8.

use tuile_budget::{Bytes, Class, Decisions, Micros, NeedSet, Pressure, Thermal, BACKOFF_KEEP_MAX};

use super::detmath;
use super::net::NetProfile;
use super::server::{Profile, Sim};
use super::world::{Camera, SIDE};

const RED: &str = "the budget core regressed";

/// Over the coastal plain, 2 km up, looking east and 30° down.
pub fn home() -> Camera {
    Camera {
        x: 0.6 * SIDE,
        y: 0.5 * SIDE,
        h: 2_000.0,
        heading: 0.0,
        pitch: 30f64.to_radians(),
    }
}

/// A five-second approach to `home`, then still.
pub fn approach(t: f64) -> Camera {
    let k = (t / 5.0).min(1.0);
    Camera {
        x: home().x - 20_000.0 * (1.0 - k),
        ..home()
    }
}

pub fn settle(sim: &mut Sim, secs: f64) -> Micros {
    sim.run(5.0, &approach);
    let stop = sim.now;
    sim.run(secs, &|_| home());
    stop
}

/// A flight that grows the tracked set, then shrinks it: the approach, up
/// to 60 km and a turn over the plain (thousands of tiles tracked), back
/// down to the still view, then still.
pub fn grow_then_shrink(t: f64) -> Camera {
    let h0 = home();
    let lg = |a: f64, b: f64, k: f64| a * detmath::exp2(detmath::log2(b / a) * k);
    if t < 5.0 {
        approach(t)
    } else if t < 10.0 {
        Camera {
            h: lg(2_000.0, 60_000.0, (t - 5.0) / 5.0),
            ..h0
        }
    } else if t < 25.0 {
        Camera {
            h: 60_000.0,
            heading: (t - 10.0) / 15.0 * std::f64::consts::TAU,
            ..h0
        }
    } else if t < 30.0 {
        Camera {
            h: lg(60_000.0, 2_000.0, (t - 25.0) / 5.0),
            ..h0
        }
    } else {
        h0
    }
}

/// Flies `sim` through [`grow_then_shrink`] for a minute, the app sent to
/// the background at 40 s (only the floor and what is drawn kept) and back
/// at 42 s; `each` sees the simulation after every step, with the step's
/// need and decisions. Returns the tracked peak.
pub fn fly_grow_then_shrink(
    sim: &mut Sim,
    mut each: impl FnMut(&Sim, &NeedSet, &Decisions),
) -> usize {
    let mut peak = 0;
    while sim.now < Micros::from_secs(60) {
        let t = sim.now.0 as f64 / 1e6;
        if (40.0..42.0).contains(&t) {
            sim.pressure(Pressure::Background);
        } else if (42.0..42.1).contains(&t) {
            sim.pressure(Pressure::Normal);
        }
        let (need, d) = sim.step(grow_then_shrink(t));
        peak = peak.max(sim.budget.tracked());
        each(sim, &need, &d);
    }
    peak
}

/// The arena gives its room back, in the budget, checked: on the flight
/// that grows the tracked set to thousands and falls back to the still
/// view, the room the budget has for tracked resources follows — it
/// stays far over the set (sparse) for a while, then is cut to twice the
/// set — and every decision and the whole state are the recorded ones
/// (the reference, recorded with this scenario). The checked simulation
/// (release with debug assertions) runs the arena's and the kept sets'
/// renumbering checks here, every step.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn the_arena_gives_its_room_back_on_the_flight_that_shrinks_the_tracked_set() {
    let mut sim = Sim::new(Profile::desktop(), 7);
    // (t, tracked, room) after every step.
    let mut trace: Vec<(Micros, usize, usize)> = Vec::new();
    let peak = fly_grow_then_shrink(&mut sim, |s, _, _| {
        trace.push((s.now, s.budget.tracked(), s.budget.tracked_room()));
    });
    let room_max = trace.iter().map(|x| x.2).max().unwrap_or(0);
    let sparse = trace
        .iter()
        .filter(|(_, tracked, room)| *room > 256 && *room > 4 * tracked)
        .count();
    // A compaction: the room falls from one step to the next, to twice the
    // tracked set (256 at least), which the budget then grows from.
    let compactions: Vec<&(Micros, usize, usize)> = trace
        .windows(2)
        .filter(|w| w[1].2 < w[0].2)
        .map(|w| &w[1])
        .collect();
    let (_, tracked, room) = *trace.last().expect("steps");
    eprintln!(
        "grow then shrink: tracked peak {peak}, room at most {room_max}, sparse on {sparse} \
         steps, compacted {} time(s) ({}), ending at {tracked} tracked in a room of {room}",
        compactions.len(),
        compactions
            .iter()
            .map(|(t, n, r)| format!("t = {:.2} s: {n} tracked, room {r}", t.0 as f64 / 1e6))
            .collect::<Vec<_>>()
            .join("; ")
    );
    assert!(peak >= 3 * tracked, "{RED}: the flight shrinks the set");
    // Hysteresis: the eighth prune in a row (one every eighth step) that
    // finds the set sparse compacts, so at least seven whole prune
    // intervals pass sparse first.
    assert!(
        sparse >= 7 * 8,
        "{RED}: the set under a quarter of the room on {sparse} steps"
    );
    assert!(
        !compactions.is_empty(),
        "{RED}: the room was never given back"
    );
    for (_, n, r) in &compactions {
        assert_eq!(*r, (2 * n).max(256), "{RED}: the room after a compaction");
    }
    assert!(room < room_max / 2, "{RED}: room {room} of {room_max} kept");
}

pub fn converges(mut profile: Profile) {
    // I3 holds at the p95 latency: no host stalls here (they are the lossy
    // scenario's), failures still retried.
    for s in [&mut profile.net.meshes, &mut profile.net.imagery] {
        s.stall_permille = 0;
    }
    let within = profile.converge_within;
    let mut sim = Sim::new(profile, 1);
    let stop = settle(&mut sim, 6.0);
    // From the moment the camera stops and the floor is drawable.
    let stop = stop.max(sim.floor_at.expect(RED));
    let took = sim.converged_after(stop).expect(RED);
    assert!(
        took <= within,
        "{}: converged after {took:?}, bound {within:?}",
        sim.profile.name
    );
}

/// I3: on a desktop, a still camera converges within two simulated seconds.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_camera_converges_within_two_seconds_on_desktop() {
    converges(Profile::desktop());
}

/// I3: on a phone, within four and a half. Ignored while it measured
/// 6.4 s; it converged in 3.15 s under the pooled ledger (measured
/// 2026-10-21), 4.15 s once priced by view depth (2026-11-24), when the
/// bound went from 4 s to 4.5 s (`Profile::handheld`).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_camera_converges_within_four_and_a_half_seconds_on_a_phone() {
    converges(Profile::handheld());
}

/// I3, audit: at the handheld low clamp (160 MiB), within the phone's
/// bound too (4.27 s measured by view depth, 2026-11-24).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_camera_converges_at_the_handheld_low_clamp() {
    converges(Profile::handheld_low());
}

/// I3, I4, I5, S2 (M2.1): ten still minutes cancel nothing, change no
/// tile's level twice in ten seconds, and hold the decision digest.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn ten_still_minutes_cancel_nothing_flap_nothing_and_hold_the_digest() {
    let mut sim = Sim::new(Profile::desktop(), 2);
    let stop = settle(&mut sim, 600.0);
    let after = sim.converged_after(stop).expect(RED);
    let from = stop.plus(after);
    let still: Vec<_> = sim.log.iter().filter(|l| l.now >= from).collect();
    let cancelled: u32 = still.iter().map(|l| l.stats.cancelled).sum();
    assert_eq!(cancelled, 0, "I5: a still camera cancelled loads");
    assert!(
        sim.worst_flapping() <= 1,
        "I4: a tile changed level {} times in 10 s",
        sim.worst_flapping()
    );
    // Once the budget has nothing left to do (preload and ring included),
    // its decisions do not move.
    let idle = stop.plus(sim.settled_after(stop).expect("settled"));
    let rest: Vec<_> = sim.log.iter().filter(|l| l.now >= idle).collect();
    let first = rest.first().map(|l| l.digest);
    assert!(
        rest.iter().all(|l| Some(l.digest) == first),
        "I3: the digest moved at rest"
    );
    assert!(rest.len() > 30_000, "at rest for most of the ten minutes");
}

pub fn turn(t: f64) -> Camera {
    let k = ((t - 6.0) / 1.0).clamp(0.0, 1.0);
    Camera {
        heading: std::f64::consts::PI * k,
        ..home()
    }
}

/// S3 (M3.2, M3.3): a 180° turn in one second converges within the bound.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_half_turn_in_one_second_converges() {
    let mut p = Profile::desktop();
    p.net.meshes.stall_permille = 0;
    p.net.imagery.stall_permille = 0;
    let mut sim = Sim::new(p, 3);
    sim.run(5.0, &approach);
    sim.run(1.0, &|_| home());
    sim.run(1.0, &turn);
    let stop = sim.now;
    sim.run(6.0, &|_| Camera {
        heading: std::f64::consts::PI,
        ..home()
    });
    let took = sim.converged_after(stop).expect(RED);
    assert!(took <= Micros::from_secs(2), "converged after {took:?}");
}

pub fn zoom(t: f64) -> Camera {
    // 0–5 s approach; 5–10 s climb to 200 km; 10–12 s hold; 12–17 s back.
    let h = if t < 5.0 {
        2_000.0
    } else if t < 10.0 {
        2_000.0 * detmath::exp2(detmath::log2(100.0) * (t - 5.0) / 5.0)
    } else if t < 12.0 {
        200_000.0
    } else if t < 17.0 {
        200_000.0 * detmath::exp2(detmath::log2(0.01) * (t - 12.0) / 5.0)
    } else {
        2_000.0
    };
    Camera {
        h,
        ..approach(t.min(5.0))
    }
}

/// I1, I9: a zoom out to 200 km and back never shows a hole (checked every
/// step, in both swap directions).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_zoom_out_to_200_km_and_back_never_shows_a_hole() {
    let mut sim = Sim::new(Profile::desktop(), 4);
    sim.run(20.0, &zoom);
    let lost: u32 = sim.log.iter().map(|l| l.stats.lost).sum();
    assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    let first_in = sim.log.iter().position(|l| l.stats.exact > 0).unwrap_or(0);
    let after: u32 = sim.log[first_in..].iter().map(|l| l.stats.lost).sum();
    assert_eq!(after, 0, "{lost} lost in all");
}

/// Colour consistency before detail (`T11 · I12→11` on the viewer's labels):
/// after a zoom-out, then still, every exactly drawn tile wears its own
/// imagery level — none a level finer, kept because it was resident, none
/// coarser — within three seconds of rest, far under the memory limits, and
/// keeps it. The imagery being another acquisition per level, a tile worn
/// over its need is a patch of another colour among its neighbours. No step
/// of the whole run draws a tile bare (the simulation fails on any, I1).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn after_a_zoom_out_then_still_every_tile_wears_its_own_imagery_level() {
    let mut sim = Sim::new(Profile::desktop(), 11);
    // Approach and settle at 2 km, then out to 16 km in four seconds.
    let out = |t: f64| {
        if t < 5.0 {
            approach(t)
        } else if t < 10.0 {
            home()
        } else {
            let k = ((t - 10.0) / 4.0).min(1.0);
            Camera {
                h: 2_000.0 * detmath::exp2(3.0 * k),
                ..home()
            }
        }
    };
    sim.run(14.0, &out);
    let stop = sim.now;
    sim.run(8.0, &|_| out(14.0));
    let over_at_stop = sim
        .log
        .iter()
        .find(|l| l.now >= stop)
        .map(|l| l.stats.deficit.imagery_over_tiles)
        .unwrap_or(0);
    let settled = stop.plus(Micros::from_secs(3));
    for l in sim.log.iter().filter(|l| l.now >= settled) {
        let d = &l.stats.deficit;
        assert_eq!(
            (d.imagery_over_tiles, d.imagery_under_tiles),
            (0, 0),
            "{RED}: at {:?}, tiles worn over / under their imagery need \
             ({over_at_stop} over at rest)",
            l.now
        );
        assert!(l.stats.gpu[Class::Drape] < l.stats.target[Class::Drape]);
    }
}

/// S3 (M3.1), I10: back down after a zoom-out, the drapes are sharp again.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn after_a_zoom_out_and_back_the_drapes_are_sharp_again() {
    let mut sim = Sim::new(Profile::desktop(), 5);
    sim.run(17.0, &zoom);
    let stop = sim.now;
    sim.run(6.0, &zoom);
    let took = sim.converged_after(stop).expect(RED);
    assert!(took <= Micros::from_secs(2), "{took:?}");
    let last = sim.log.last().expect("steps");
    assert_eq!(last.stats.underdraped, 0);
}

/// C21, I10: over the plain, imagery is deeper than terrain; everywhere it
/// matches its need.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn over_the_plain_imagery_is_deeper_than_terrain() {
    let mut sim = Sim::new(Profile::desktop(), 6);
    let pan = |t: f64| Camera {
        x: 0.5 * SIDE + 2_000.0 * t,
        heading: std::f64::consts::FRAC_PI_2,
        ..home()
    };
    sim.run(10.0, &pan);
    let stop = sim.now;
    sim.run(6.0, &|_| pan(10.0));
    sim.converged_after(stop).expect(RED);
    let last = sim.log.last().expect("steps");
    let deeper: u32 = last.stats.imagery_over_terrain[9..].iter().sum();
    assert!(deeper > 0, "no tile wears imagery deeper than its terrain");
    assert_eq!(last.stats.underdraped, 0);
}

pub fn back_and_forth(t: f64) -> Camera {
    let phase = (t / 8.0).floor() as u64 % 2;
    let k = (t % 8.0 / 4.0).min(1.0);
    let k = if phase == 0 { k } else { 1.0 - k };
    Camera {
        y: home().y + 40_000.0 * k,
        ..home()
    }
}

/// I14, B anti-patterns: going back and forth reuses what is held: the
/// returns cost a fraction of the first pass, and little is thrashed.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn back_and_forth_reuses_what_is_held() {
    let mut sim = Sim::new(Profile::desktop(), 7);
    sim.run(16.0, &back_and_forth);
    let first = sim.net.network_bytes;
    sim.run(32.0, &|t| back_and_forth(t + 16.0));
    let returns = sim.net.network_bytes - first;
    assert!(first > 0, "{RED}");
    assert!(
        returns * 4 < first,
        "two returns refetched {returns} bytes, first pass {first}"
    );
    let thrash: u32 = sim.log.iter().map(|l| l.stats.thrash).sum();
    assert!(thrash < 50, "{thrash} evict-then-request");
}

/// I8, I1: a memory warning mid-flight brings bytes under the new target
/// within a second, without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_memory_warning_mid_flight_trims_without_a_hole() {
    let mut sim = Sim::new(Profile::handheld(), 8);
    sim.run(8.0, &zoom);
    sim.pressure(Pressure::Warning);
    let at = sim.now;
    sim.run(4.0, &|t| zoom(t + 8.0));
    let after = sim
        .log
        .iter()
        .filter(|l| l.now >= at.plus(Micros::from_secs(1)))
        .collect::<Vec<_>>();
    assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    for l in after {
        for c in Class::ALL {
            let limit = l.stats.target[c].plus(l.stats.overflow[c]);
            assert!(
                l.stats.gpu[c] <= limit.max(l.stats.protected[c]),
                "{c:?} over after the warning"
            );
        }
    }
    assert!(sim
        .log
        .iter()
        .any(|l| l.stats.target[Class::Drape] < Profile::handheld().budget.target[Class::Drape]));
}

/// I8: thermal Serious halves uploads, without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn thermal_serious_halves_uploads_without_a_hole() {
    let mut sim = Sim::new(Profile::handheld(), 9);
    sim.thermal(Thermal::Serious);
    let cap = Profile::handheld()
        .budget
        .upload_bytes_per_frame
        .scaled(1, 2);
    let mut over = 0;
    for _ in 0..600 {
        let (_, d) = sim.step(home());
        if d.upload_plan.items.len() > 1 && d.upload_plan.bytes > cap {
            over += 1;
        }
    }
    assert!(sim.log.iter().any(|l| l.stats.exact > 0), "{RED}");
    assert_eq!(over, 0);
}

/// I8, audit: backgrounding drops to the floor and the drawn set;
/// foregrounding refills without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn backgrounding_and_foregrounding_refill_without_a_hole() {
    let mut sim = Sim::new(Profile::handheld(), 10);
    // Converged first: the refill is what is measured.
    settle(&mut sim, 30.0);
    sim.pressure(Pressure::Background);
    sim.run(2.0, &|_| home());
    let bg = sim.log.last().expect("steps").stats.gpu;
    let total = bg.iter().fold(Bytes::ZERO, |a, (_, b)| a.plus(b));
    sim.pressure(Pressure::Normal);
    let fg = sim.now;
    sim.run(6.0, &|_| home());
    assert!(total > Bytes::ZERO, "{RED}");
    sim.converged_after(fg).expect("refilled");
}

/// D1, M2.7: a lossy network with stalled hosts never blocks the view.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_lossy_stalling_network_never_blocks_the_view() {
    let mut p = Profile::desktop();
    for s in [&mut p.net.meshes, &mut p.net.imagery] {
        s.fail_permille = 50;
        s.stall_permille = 20;
    }
    let mut sim = Sim::new(p, 11);
    let stop = settle(&mut sim, 30.0);
    let took = sim.converged_after(stop).expect(RED);
    assert!(took <= Micros::from_secs(20), "{took:?}");
}

/// The tracked resources holding nothing — no copy, nothing in transit, no
/// load — whose want went: the slots the prune keeps for their failure
/// history alone.
fn kept_for_their_failures(sim: &Sim) -> usize {
    sim.budget
        .snapshot()
        .resources
        .iter()
        .filter(|r| {
            r.gpu.is_none()
                && r.cpu.is_none()
                && r.transit.is_none()
                && r.load.is_none()
                && r.unneeded_since.is_some()
        })
        .count()
}

/// The camera of the outage scenario: the approach, then a jump to a new
/// place every two seconds, 94 km away, at the home view's height (a search
/// box's "go to": each one reveals ground the last place did not share, and
/// the loads of the place left have failed three or four times by then);
/// from 35 s the camera stays at the fifteenth place.
fn jumps(t: f64) -> Camera {
    if t < 5.0 {
        approach(t)
    } else {
        let k = ((t - 5.0) / 2.0).floor().min(14.0);
        Camera {
            x: (0.15 + 0.045 * k) * SIDE,
            ..home()
        }
    }
}

/// I7, D1: a network outage. Every network load fails at once for half a
/// minute (a refused connection) while the view jumps from place to place
/// (nothing arriving, so each place wants its coarse tiles and fails them
/// over and over), then the network is back and the camera stays at the
/// last place, whose tiles then load through their backoffs. Every wanted resource fails and backs off,
/// and the view leaves them behind: the budget keeps their failure history
/// for a while (so a server that is down is not hammered the moment a tile
/// comes back into view), but not without bound — the slots kept for
/// resources the view left are capped by count, and so by the bytes of
/// their metadata (`BACKOFF_KEEP_MAX`), the ones left longest let go first;
/// the wanted ones are the view's own. Once the network is back the view
/// converges, the backoffs of the wanted resources (30 s at most) included.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_network_outage_leaves_the_tracked_state_bounded_and_recovers() {
    let mut sim = Sim::new(Profile::desktop(), 21);
    let (from, to) = (Micros::from_secs(5), Micros::from_secs(35));
    sim.net.outage = Some((from, to));
    let end = Micros::from_secs(80);
    let mut report = Micros::from_secs(5);
    let (mut tracked_max, mut kept_max, mut failed) = (0usize, 0usize, 0u64);
    let mut back = None;
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        if sim.now >= to && back.is_none() {
            back = Some(sim.now);
        }
        let (need, d) = sim.step(jumps(t));
        failed += u64::from(d.stats.failed);
        tracked_max = tracked_max.max(sim.budget.tracked());
        // The prune runs every eighth step: right after one, the cap holds
        // and every tracked resource is accounted for — wanted (a tile's
        // mesh and its drape at most), holding a copy, loading or in
        // transit, or kept for its failures within the cap — through the
        // outage and the recovery alike.
        if sim.budget.steps.is_multiple_of(8) {
            let kept = kept_for_their_failures(&sim);
            kept_max = kept_max.max(kept);
            assert!(
                kept <= BACKOFF_KEEP_MAX,
                "{RED}: {kept} slots kept for their failures alone at t = {t:.1} s, \
                 the cap {BACKOFF_KEEP_MAX}"
            );
            let wanted = 2 * super::patterns::listed(&need).len();
            let held = sim.gpu.drawable.len() + sim.gpu.pending.len();
            let loading = d
                .stats
                .in_flight
                .iter()
                .map(|(_, n)| n as usize)
                .sum::<usize>()
                + d.stats.local_reads as usize;
            let bound = wanted + held + loading + BACKOFF_KEEP_MAX;
            assert!(
                sim.budget.tracked() <= bound,
                "{RED}: I7: {} tracked at t = {t:.1} s, bound {bound} ({wanted} wanted, {held} \
                 held, {loading} loading, {BACKOFF_KEEP_MAX} kept at most)",
                sim.budget.tracked()
            );
        }
        if sim.now >= report {
            eprintln!(
                "outage t = {t:.0} s: tracked {}, drawable {}, kept for their failures {}, \
                 failed so far {failed}",
                sim.budget.tracked(),
                sim.gpu.drawable.len(),
                kept_for_their_failures(&sim)
            );
            report = report.plus(Micros::from_secs(5));
        }
    }
    let back = back.expect("the network came back");
    let took = sim.converged_after(back).expect(RED);
    eprintln!(
        "outage: tracked at most {tracked_max}, kept for their failures at most {kept_max} \
         (cap {BACKOFF_KEEP_MAX}, {} B each), {failed} loads failed; converged {took:?} after \
         the network came back, {} tracked at the end",
        tuile_budget::BACKOFF_KEPT_BYTES,
        sim.budget.tracked()
    );
    assert!(
        took <= Micros::from_secs(35),
        "{RED}: converged {took:?} after the network came back"
    );
}

/// A lost network, then its return (`origin`). Every network load fails at
/// once for a minute while the view jumps from place to place, then the
/// network is back and the camera stays at the last place. While it is
/// out, each origin (here the terrain meshes and the one imagery layer) is
/// asked by one probe at a time, at most two a second each, however many
/// tiles want it: never a storm of retries. Once it is back, the first
/// probe that answers sends every tile that backs off to the network again:
/// the view converges within the time the same view takes to load from
/// nothing, plus two probe ceilings (the longest a probe waits, jitter
/// included) and a second — not after each tile's own backoff (30 s at
/// most).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_lost_network_is_probed_gently_and_its_return_loads_the_view_promptly() {
    let mut profile = Profile::desktop();
    profile.net = profile.net.clean();
    let mut sim = Sim::new(profile, 22);
    let (from, to) = (Micros::from_secs(5), Micros::from_secs(65));
    sim.net.outage = Some((from, to));
    let end = Micros::from_secs(95);
    let mut starts: Vec<(Micros, u64)> = Vec::new();
    let mut back = None;
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        if sim.now >= to && back.is_none() {
            back = Some(sim.now);
        }
        sim.step(jumps(t));
        starts.push((sim.now, sim.net.network_starts));
    }
    // The most network starts in any second of the outage, past its first
    // (the loads in flight when it began fail then).
    let settled = from.plus(Micros::from_secs(1));
    let mut worst = (0u64, Micros::ZERO);
    for (i, &(t, n)) in starts.iter().enumerate() {
        let until = t.plus(Micros::from_secs(1));
        if t < settled || until > to {
            continue;
        }
        let last = starts[i..]
            .iter()
            .take_while(|(u, _)| *u < until)
            .last()
            .map_or(n, |(_, m)| *m);
        if last - n > worst.0 {
            worst = (last - n, t);
        }
    }
    let back = back.expect("the network came back");
    let took = sim.converged_after(back);
    eprintln!(
        "lost network: at most {} network starts in one second of the outage (at {:?}); \
         converged {took:?} after it came back",
        worst.0, worst.1
    );
    assert!(
        worst.0 <= 4,
        "{RED}: {} network starts in the second from {:?}, with the network out: \
         two origins, two probes a second at most each",
        worst.0,
        worst.1
    );
    // The same view, loaded from nothing on a network that never went.
    let mut fresh = Sim::new(sim.profile.clone(), 22);
    let last = jumps(end.0 as f64 / 1e6);
    fresh.run(30.0, &|_| last);
    let load = fresh.converged_after(Micros::ZERO).expect(RED);
    let probe = tuile_budget::BudgetConfig::interactive(Bytes::ZERO)
        .origin
        .probe_max;
    let bound = load.plus(Micros(2 * probe.0)).plus(Micros::from_secs(1));
    let took = took.expect(RED);
    eprintln!("lost network: the view loads in {load:?} from nothing; bound {bound:?}");
    assert!(
        took <= bound,
        "{RED}: converged {took:?} after the network came back, bound {bound:?} \
         ({load:?} to load the view from nothing)"
    );
}

/// I14, I15: a relaunch fills the view from the store, without network for
/// what the store holds.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_relaunch_fills_the_view_from_the_store_without_network() {
    let mut first = Sim::new(Profile::handheld(), 12);
    // The first session converges, so its store holds the view.
    settle(&mut first, 30.0);
    let store = first.net.store.clone();
    assert!(!store.held.is_empty(), "{RED}");
    let mut again = Sim::new(Profile::handheld(), 13);
    again.net.store = store;
    let start = again.now;
    again.run(6.0, &|_| home());
    // Nothing the store holds goes to the network (I14, checked every
    // step); what the first session never loaded (ring, other levels) may.
    let local: u32 = again.log.iter().map(|l| l.stats.started_local).sum();
    assert!(
        u64::from(local) > again.net.network_starts,
        "{local} local reads, {} network starts",
        again.net.network_starts
    );
    let took = again.converged_after(start).expect("converged");
    assert!(took <= Profile::handheld().converge_within, "{took:?}");
}

/// §4b: a lying store index costs one local miss per lie, then the network.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_lying_store_index_costs_one_miss_per_lie() {
    let mut first = Sim::new(Profile::desktop().clone(), 14);
    settle(&mut first, 6.0);
    let mut store = first.net.store.clone();
    let lies: Vec<_> = store.held.iter().copied().step_by(3).collect();
    for r in &lies {
        store.held.remove(r);
        store.lies.insert(*r);
    }
    store.generation += 1;
    let mut again = Sim::new(Profile::desktop(), 15);
    again.net.store = store;
    again.run(6.0, &|_| home());
    let misses: u32 = again.log.iter().map(|l| l.stats.store_misses).sum();
    assert!(misses > 0, "{RED}");
    assert!(misses as usize <= lies.len());
}

pub fn film_cam(frame: u64) -> Camera {
    let t = frame as f64 / 30.0;
    Camera {
        x: home().x + 300.0 * t,
        heading: 0.3 * t,
        ..home()
    }
}

/// I13: a 300-frame film under Batch is exact every frame.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_300_frame_film_is_exact_every_frame() {
    let mut sim = Sim::new(Profile::film(), 16);
    for f in 0..300 {
        let (drawn, _) = sim.film_frame(film_cam(f), 2_000);
        assert!(!drawn.is_empty());
        assert!(drawn.iter().all(|(_, drape)| drape.is_some()));
    }
}

fn film(
    seed: u64,
    net: NetProfile,
    frames: std::ops::Range<u64>,
) -> Vec<Vec<(tuile_budget::ResourceId, Option<tuile_budget::ResourceId>)>> {
    let mut p = Profile::film();
    p.net = net;
    let mut sim = Sim::new(p, seed);
    frames
        .map(|f| sim.film_frame(film_cam(f), 2_000).0)
        .collect()
}

/// I13: a film frame is a function of its inputs, not of the network.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_film_does_not_depend_on_the_network() {
    let fast = film(17, NetProfile::desktop().clean(), 0..60);
    let slow = film(18, NetProfile::handheld().clean(), 0..60);
    assert_eq!(fast, slow);
}

/// I13: nor on how frames are split across machines.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_film_does_not_depend_on_how_frames_are_split() {
    let whole = film(19, NetProfile::desktop().clean(), 0..120);
    let tail = film(20, NetProfile::desktop().clean(), 60..120);
    assert_eq!(whole[60..], tail[..]);
}

/// Golden decision digests per scenario: a change needs a reviewed update
/// (`TUILE_BLESS_BUDGET=1`). The simulation's inputs use no `libm`
/// (`detmath`), so the digests hold on every platform, the Linux CI included.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn golden_decision_digests_hold() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/budget_sim/goldens.txt");
    let mut got = Vec::new();
    for (name, seed, script) in [
        ("still", 21u64, &approach as &dyn Fn(f64) -> Camera),
        ("turn", 22, &turn),
        ("zoom", 23, &zoom),
        ("back-and-forth", 24, &back_and_forth),
    ] {
        let mut sim = Sim::new(Profile::desktop(), seed);
        sim.run(20.0, script);
        got.push(format!("{name} {:016x}", sim.run_digest()));
    }
    let got = got.join("\n") + "\n";
    if std::env::var_os("TUILE_BLESS_BUDGET").is_some() {
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).expect("goldens recorded");
    assert_eq!(got, want);
}

/// I1, I2, I9, review of point 9: at the hard ceiling of a small phone, a
/// zoom-out never deadlocks: the parents of the drawn cut stay resident (the
/// protected chain), so coarsening never waits on bytes it cannot get; no
/// class crosses its ceiling and nothing is lost (checked every step); the
/// budget settles, with nothing churning, at the best view it can hold.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_zoom_out_at_the_hard_ceiling_never_deadlocks() {
    let mut p = Profile::handheld_low();
    p.budget.hard_limit[Class::TerrainMesh] = Bytes::mib(40);
    p.budget.target[Class::TerrainMesh] = Bytes::mib(28);
    p.budget.pinned_level = Some(2);
    let mut sim = Sim::new(p, 25);
    let out = |t: f64| {
        let k = ((t - 8.0) / 3.0).clamp(0.0, 1.0);
        Camera {
            h: 2_000.0 * detmath::exp2(detmath::log2(25.0) * k),
            ..approach(t.min(5.0))
        }
    };
    let mut parents_resident = true;
    let mut strained = false;
    while sim.now < Micros::from_secs(11) {
        let t = sim.now.0 as f64 / 1e6;
        let (need, d) = sim.step(out(t));
        strained |= d.sse_scale_milli > 1_000;
        for (tile, n) in &need.drawn {
            let _ = tile;
            if let Some(p) = n.parent {
                parents_resident &= sim.budget.residency().drawable(p) || p.terrain_coord().0 == 0;
            }
        }
    }
    let stop = sim.now;
    sim.run(20.0, &out);
    assert!(strained, "{RED}: the ceiling was never reached");
    assert!(parents_resident, "a drawn tile's parent was not resident");
    let took = sim.settled_after(stop).expect("settled");
    assert!(took <= Micros::from_secs(10), "{took:?}");
}

/// The city's centre, metres.
fn city_centre() -> (f64, f64) {
    use super::world::{CITY_SIDE, CITY_X0, CITY_Y0};
    (CITY_X0 + CITY_SIDE / 2.0, CITY_Y0 + CITY_SIDE / 2.0)
}

/// A visit to the city: from the west at 2 km, round it at 4 km for twenty
/// seconds looking at its centre, held still, then away up to 40 km and a
/// hundred kilometres west, held still there.
pub fn city_visit(t: f64) -> Camera {
    let (cx, cy) = city_centre();
    let orbit = |a: f64, h: f64| {
        let (s, c) = detmath::sin_cos(a);
        Camera {
            x: cx - 4_000.0 * c,
            y: cy - 4_000.0 * s,
            h,
            heading: a,
            pitch: 25f64.to_radians(),
        }
    };
    if t < 5.0 {
        let k = t / 5.0;
        Camera {
            x: cx - 20_000.0 + 16_000.0 * k,
            y: cy,
            h: 2_000.0,
            heading: 0.0,
            pitch: 30f64.to_radians(),
        }
    } else if t < 25.0 {
        let k = (t - 5.0) / 20.0;
        orbit(k * std::f64::consts::TAU, 2_000.0 - 500.0 * k)
    } else if t < 35.0 {
        orbit(std::f64::consts::TAU, 1_500.0)
    } else if t < 40.0 {
        let k = (t - 35.0) / 5.0;
        Camera {
            x: cx - 4_000.0 - 100_000.0 * k,
            y: cy,
            h: 1_500.0 + 38_500.0 * k,
            heading: 0.0,
            pitch: 30f64.to_radians(),
        }
    } else {
        Camera {
            x: cx - 104_000.0,
            y: cy,
            h: 40_000.0,
            heading: 0.0,
            pitch: 30f64.to_radians(),
        }
    }
}

/// What a run of [`city_visit`] measured.
struct CityRun {
    /// Peak bytes on the GPU per class, and of all classes together.
    peak: tuile_budget::PerClass<Bytes>,
    peak_total: Bytes,
    /// The city's tiles drawn as themselves and the ground meshes drawn
    /// inside the city's square, at the end of the still view over it.
    city_drawn: usize,
    ground_under: usize,
    /// When the still view over the city first converged, seconds after
    /// it began.
    converged_after: Option<f64>,
    /// The city's bytes when the view had been away for ten seconds.
    city_away: Bytes,
}

fn visit_the_city(city: bool) -> CityRun {
    use super::world::{is_city, square, CITY_SIDE, CITY_X0, CITY_Y0};
    let mut sim = Sim::new(Profile::desktop(), 21);
    sim.city = city;
    let mut run = CityRun {
        peak: tuile_budget::PerClass::splat(Bytes::ZERO),
        peak_total: Bytes::ZERO,
        city_drawn: 0,
        ground_under: 0,
        converged_after: None,
        city_away: Bytes::ZERO,
    };
    let inside = |t: tuile_budget::TileId| {
        let [x0, x1, y0, y1] = square(t);
        x0 >= CITY_X0 && x1 <= CITY_X0 + CITY_SIDE && y0 >= CITY_Y0 && y1 <= CITY_Y0 + CITY_SIDE
    };
    while sim.now < Micros::from_secs(50) {
        let t = sim.now.0 as f64 / 1e6;
        let (_, d) = sim.step(city_visit(t));
        let mut total = Bytes::ZERO;
        for c in Class::ALL {
            run.peak[c] = run.peak[c].max(d.stats.gpu[c]);
            total = total.plus(d.stats.gpu[c]);
        }
        run.peak_total = run.peak_total.max(total);
        if (25.0..35.0).contains(&t) && d.stats.converged && run.converged_after.is_none() {
            run.converged_after = Some(t - 25.0);
        }
        if (34.9..35.0).contains(&t) {
            let drawn = d
                .draw
                .items
                .iter()
                .filter(|i| i.role == tuile_budget::Role::Exact);
            run.city_drawn = drawn.clone().filter(|i| is_city(i.mesh.tile())).count();
            run.ground_under = drawn
                .filter(|i| !is_city(i.mesh.tile()) && inside(i.mesh.tile()))
                .count();
        }
        run.city_away = d.stats.gpu[Class::Tiles3dMesh];
    }
    run
}

/// **The globe and a city's 3D Tiles share one budget**: one need, one
/// ledger, the city a source of its own (a tag, `Tiles3dMesh`, its own
/// threshold). Every invariant holds at every step (`Sim::check`: no ground
/// lost once the floor is in, no class over its limit, the pool within its
/// own); the still view over the city converges, the city drawn; the ground
/// under the drawn city is refined coarser than without it (the cover rule:
/// it stays as the fallback, never black); the scene's GPU bytes stay within
/// the device's; and the city takes what the view needs of the scene's pool,
/// beyond the 15 % of the device 3D Tiles once kept apart (0.210 GiB, its
/// target there, against 0.382). Recorded in the reference.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn the_globe_and_a_city_share_one_budget() {
    let with = visit_the_city(true);
    let without = visit_the_city(false);
    let gib = |b: Bytes| b.0 as f64 / f64::from(1u32 << 30);
    eprintln!(
        "city: peak terrain {:.3} / city {:.3} / drapes {:.3} GiB, all {:.3} GiB of 2; \
         {} city tiles and {} ground meshes under it drawn (without the city: {}); \
         converged {:?} s into the still view (without: {:?}); the city's bytes away {:.3} GiB",
        gib(with.peak[Class::TerrainMesh]),
        gib(with.peak[Class::Tiles3dMesh]),
        gib(with.peak[Class::Drape]),
        gib(with.peak_total),
        with.city_drawn,
        with.ground_under,
        without.ground_under,
        with.converged_after,
        without.converged_after,
        gib(with.city_away),
    );
    assert!(with.city_drawn > 0, "{RED}: the city never drawn");
    assert!(
        with.converged_after.is_some(),
        "{RED}: the still view over the city never converged"
    );
    assert!(
        with.ground_under < without.ground_under,
        "{RED}: the ground under the city refined as finely as without it ({} against {})",
        with.ground_under,
        without.ground_under
    );
    assert!(
        with.peak_total <= Bytes::gib(2),
        "{RED}: the scene over the device's budget"
    );
    // No class has a share of its own: the city takes what the view needs
    // of the pool, beyond the 15 % of the device 3D Tiles once kept apart.
    assert!(
        with.peak[Class::Tiles3dMesh] > Bytes::gib(2).scaled(15, 100),
        "{RED}: the city held to a fixed share ({:.3} GiB)",
        gib(with.peak[Class::Tiles3dMesh])
    );
}

/// An imagery origin slower than the load timeout still delivers. Every
/// drape here takes 5 s, the desktop's timeout is 4 s: a load cancelled on
/// its timeout and restarted with the same timeout never lands, and the
/// ground is never dressed — what a throttled or distant origin did to the
/// viewer (16 drape loads in flight for ever, the imagery under its need).
#[test]
#[cfg_attr(debug_assertions, ignore = "slow: an origin slower than the load timeout (runs in release in budget-sim), ~22 s in debug")]
fn an_origin_slower_than_the_load_timeout_still_delivers() {
    let mut profile = Profile::desktop();
    profile.net.imagery.base_ms = 5_000.0;
    profile.net.imagery.jitter_ms = 0.0;
    profile.net.imagery.fail_permille = 0;
    profile.net.imagery.stall_permille = 0;
    profile.net.meshes.stall_permille = 0;
    assert!(profile.budget.load_timeout < Micros::from_secs(5));
    let mut sim = Sim::unchecked(profile, 3);
    let cam = Camera {
        x: 0.6 * SIDE,
        y: 0.5 * SIDE,
        h: 2_000.0,
        heading: 0.0,
        pitch: 20f64.to_radians(),
    };
    let mut dressed = 0usize;
    while sim.now < Micros::from_secs(60) {
        let (_, d) = sim.step(cam);
        dressed = d.draw.items.iter().filter(|i| !i.drapes.is_empty()).count();
    }
    assert!(
        dressed > 0,
        "{RED}: after 60 s of a still view, no drawn tile wears any drape"
    );
}

/// What the screen's centre showed over a run (Laurent, 2026-11-24: the
/// coarse patches that land in the middle of the screen): per step, the
/// wanted tiles within 10° of the view's axis drawn by a coarser stand-in
/// (`fallback`), drawn as themselves in a drape two levels or more under
/// their need (`coarse`), or kept finer on a zoom-out (`held`); and, over
/// the whole screen, the parents whose wanted children were drawn part as
/// themselves, part by a stand-in (`mixed`: two levels inside one parent).
#[derive(Clone, Copy, Debug, Default)]
pub struct CentreTally {
    pub steps: u32,
    pub fallback_steps: u32,
    pub fallback_tiles: u32,
    pub coarse_steps: u32,
    pub coarse_tiles: u32,
    pub held_steps: u32,
    pub mixed_steps: u32,
    pub mixed_parents: u32,
    pub lost: u32,
    /// Wanted tiles counted in the centre, all steps.
    pub centre_tiles: u32,
    /// Wanted tiles anywhere drawn by a coarser stand-in, all steps.
    pub screen_fallback_tiles: u32,
    /// Steps where any wanted tile anywhere was held finer.
    pub screen_held_steps: u32,
}

impl CentreTally {
    /// Counts one step's `d` on its `need`.
    pub fn count(&mut self, need: &NeedSet, d: &Decisions) {
        use std::collections::BTreeMap;
        use tuile_budget::{LayerId, ResourceId, Role, TileId};
        // Within 10° of the axis: centrality (1 + cos off) / 2.
        let centre = (1.0 + detmath::sin_cos(10f64.to_radians()).1) / 2.0;
        let mut by: BTreeMap<TileId, Role> = BTreeMap::new();
        for i in &d.draw.items {
            for s in &i.serves {
                let r = by.entry(*s).or_insert(i.role);
                *r = (*r).min(i.role);
            }
        }
        let (mut fb, mut coarse, mut held) = (0u32, 0u32, 0u32);
        let mut kids: BTreeMap<TileId, (u32, u32)> = BTreeMap::new();
        let (mut centre_n, mut screen_fb, mut screen_held) = (0u32, 0u32, 0u32);
        for (t, n) in &need.drawn {
            let role = by.get(t).copied();
            screen_fb += u32::from(role == Some(Role::Fallback));
            screen_held += u32::from(role == Some(Role::Held));
            if let Some(p) = n.parent {
                let e = kids.entry(p).or_default();
                if role == Some(Role::Exact) {
                    e.0 += 1;
                } else {
                    e.1 += 1;
                }
            }
            if f64::from(n.centrality) < centre {
                continue;
            }
            centre_n += 1;
            match role {
                Some(Role::Exact) => {
                    let worn = d
                        .draw
                        .get(ResourceId::Mesh(*t))
                        .and_then(|i| i.drape(LayerId(0)).copied());
                    if worn.is_some_and(|w| w.level + 2 <= w.need) {
                        coarse += 1;
                    }
                }
                Some(Role::Held) => held += 1,
                Some(Role::Fallback) => fb += 1,
                None => {}
            }
        }
        let mixed = sat(kids.values().filter(|(e, o)| *e > 0 && *o > 0).count());
        self.steps += 1;
        self.fallback_tiles += fb;
        self.fallback_steps += u32::from(fb > 0);
        self.coarse_tiles += coarse;
        self.coarse_steps += u32::from(coarse > 0);
        self.held_steps += u32::from(held > 0);
        self.mixed_parents += mixed;
        self.mixed_steps += u32::from(mixed > 0);
        self.lost += d.stats.lost;
        self.centre_tiles += centre_n;
        self.screen_fallback_tiles += screen_fb;
        self.screen_held_steps += u32::from(screen_held > 0);
    }
}

impl CentreTally {
    /// One line: every count, named.
    pub fn line(&self) -> String {
        format!(
            "steps {} | fallback {} steps, {} tiles | coarse {} steps, {} tiles | \
             held {} steps | mixed {} steps, {} parents | lost {} | \
             centre tiles {} | screen: fallback {} tiles, held {} steps",
            self.steps,
            self.fallback_steps,
            self.fallback_tiles,
            self.coarse_steps,
            self.coarse_tiles,
            self.held_steps,
            self.mixed_steps,
            self.mixed_parents,
            self.lost,
            self.centre_tiles,
            self.screen_fallback_tiles,
            self.screen_held_steps
        )
    }
}

fn sat(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Laurent's flight of 2026-11-24 (10:24–10:26 UTC, the viewer's camera
/// telemetry), in the simulated world: looking straight down, a settle at
/// 5 km, a pan of ten kilometres down to 3.9 km, a zoom out to 55 km in
/// four seconds, a hold, a zoom back in, a hold.
pub fn nadir_flight(t: f64) -> Camera {
    let lg = |a: f64, b: f64, k: f64| a * detmath::exp2(detmath::log2(b / a) * k.clamp(0.0, 1.0));
    let base = Camera {
        x: 0.6 * SIDE,
        y: 0.5 * SIDE,
        h: 5_000.0,
        heading: 5.13,
        pitch: 85f64.to_radians(),
    };
    if t < 8.0 {
        base
    } else if t < 11.0 {
        let k = (t - 8.0) / 3.0;
        Camera {
            x: base.x + 7_000.0 * k,
            y: base.y - 7_000.0 * k,
            h: lg(5_000.0, 3_900.0, k),
            ..base
        }
    } else {
        let at = Camera {
            x: base.x + 7_000.0,
            y: base.y - 7_000.0,
            ..base
        };
        let h = if t < 15.0 {
            lg(3_900.0, 55_000.0, (t - 11.0) / 4.0)
        } else if t < 20.0 {
            55_000.0
        } else if t < 24.0 {
            lg(55_000.0, 3_900.0, (t - 20.0) / 4.0)
        } else {
            3_900.0
        };
        Camera { h, ..at }
    }
}

/// The centre of the screen through [`nadir_flight`]: what it shows by
/// phase (the before/after of the replacement rules), and never a hole.
/// The tallies are printed (`--nocapture`); the holes are asserted.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn the_screen_centre_through_a_nadir_flight() {
    // The imagery an order of magnitude slower than the meshes, as a tile
    // server under load serves it: meshes refine before their drapes.
    let mut slow = Profile::desktop();
    slow.net.imagery.base_ms = 600.0;
    slow.net.imagery.jitter_ms = 400.0;
    for (name, profile) in [
        ("desktop", Profile::desktop()),
        ("handheld", Profile::handheld()),
        ("desktop-slow-imagery", slow),
    ] {
        let mut sim = Sim::new(profile, 17);
        let phases: [(&str, f64); 5] = [
            ("settle", 8.0),
            ("pan", 11.0),
            ("zoom-out", 15.0),
            ("hold-out", 20.0),
            ("zoom-in", 28.0),
        ];
        let mut tallies = [CentreTally::default(); 5];
        let mut first_in = None;
        while sim.now < Micros::from_secs(28) {
            let t = sim.now.0 as f64 / 1e6;
            let (need, d) = sim.step(nadir_flight(t));
            if first_in.is_none() && d.stats.exact > 0 {
                first_in = Some(t);
            }
            if first_in.is_none() {
                continue;
            }
            let phase = phases.iter().position(|p| t < p.1).unwrap_or(4);
            tallies[phase].count(&need, &d);
        }
        for (i, (p, _)) in phases.iter().enumerate() {
            eprintln!("centre {name} {p}: {}", tallies[i].line());
        }
        let lost: u32 = tallies.iter().map(|x| x.lost).sum();
        assert_eq!(lost, 0, "{RED}: {name}: ground lost");
    }
}
