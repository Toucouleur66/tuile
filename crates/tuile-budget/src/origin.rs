// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Whether each origin answers: what a lost network does to the loads.
//!
//! Every load retries on its own after a failure, after a delay that doubles
//! up to the policy's ceiling ([`FailurePolicy`](super::FailurePolicy)).
//! Alone, that is the wrong shape for a network that drops: every wanted
//! tile fails and retries on its own clock, so a few hundred tiles ask a
//! dead origin several times a second between them, and once the network is
//! back each tile waits out its own delay — up to the ceiling — before it
//! asks again.
//!
//! An origin is the source a resource comes from: a mesh class, or a drape
//! layer ([`Origin::of`]). After `OriginBackoff::down_after` failed
//! network loads of an origin in a row, with no answer between them, it is
//! **down**: its loads are not started, but for one **probe** at a time,
//! spaced by a capped exponential backoff with jitter (the `backon` crate's,
//! seeded per origin so that a replay draws the same delays). The first
//! answer of a down origin is its reachability signal: it is **up** again,
//! and every wanted resource of it that was backing off is retried at once,
//! within the load quotas.
//!
//! A resource whose stored copy the store missed (`store_missed`: its copy
//! expired) is not held back by its origin being down: its load is the one
//! that serves the expired copy when the origin cannot answer. Its answer is
//! not a reachability signal for the same reason.
//!
//! Everything here changes on the step's recorded events and starts only,
//! so a replay decides the same. A timed-out load is not a failure here: a
//! slow origin is not an unreachable one (the load timeout grows instead).

use std::time::Duration;

use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};

use super::config::OriginBackoff;
use super::types::{Class, LayerId, Micros, ResourceId};

/// Where a resource comes from, as far as reachability goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// The meshes of one class.
    Mesh(Class),
    /// The drapes of one layer (a substitute layer is an origin of its own).
    Layer(LayerId),
}

impl Origin {
    /// The origin of `r`, of class `class`.
    pub fn of(r: ResourceId, class: Class) -> Self {
        match r.layer() {
            Some(l) => Origin::Layer(l),
            None => Origin::Mesh(class),
        }
    }

    /// The jitter's seed: fixed per origin, so that a run draws the same
    /// delays on every target and in every replay.
    fn seed(self) -> u64 {
        const BASE: u64 = 0x6f72_6967_696e_7321; // "origins!"
        match self {
            Origin::Mesh(c) => BASE ^ (c as u64),
            Origin::Layer(l) => BASE ^ (1 << 32) ^ u64::from(l.0),
        }
    }
}

/// A down origin.
#[derive(Debug)]
struct Down {
    /// When the next probe may start.
    next_probe: Micros,
    /// The probes' delays.
    delays: ExponentialBackoff,
}

/// One origin's state.
#[derive(Debug, Default)]
struct Health {
    /// Failed network loads in a row.
    failures: u32,
    /// Boxed: an origin that fails once in a while (most) keeps a small
    /// entry; the backoff's state is made only when it goes down.
    down: Option<Box<Down>>,
}

/// Every origin that failed since it last answered: a handful at most (a
/// mesh class, a layer), in a vector that keeps its room once grown, so a
/// failure allocates nothing in the steady state.
#[derive(Debug, Default)]
pub(super) struct Origins {
    by: Vec<(Origin, Health)>,
}

fn micros(d: Duration) -> Micros {
    Micros(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

fn duration(m: Micros) -> Duration {
    Duration::from_micros(m.0)
}

impl Origins {
    fn get(&self, o: Origin) -> Option<&Health> {
        self.by.iter().find(|(k, _)| *k == o).map(|(_, h)| h)
    }

    fn get_mut(&mut self, o: Origin) -> Option<&mut Health> {
        self.by.iter_mut().find(|(k, _)| *k == o).map(|(_, h)| h)
    }

    /// Whether a load of `o` may not start at `now`: `o` is down and its
    /// next probe is not due.
    pub fn blocks(&self, o: Origin, now: Micros) -> bool {
        self.get(o)
            .and_then(|h| h.down.as_ref())
            .is_some_and(|d| now < d.next_probe)
    }

    /// A network load of `o` starts at `now`: if `o` is down, it is the
    /// probe, and the next one waits for the backoff's next delay.
    pub fn started(&mut self, o: Origin, now: Micros, p: &OriginBackoff) {
        if let Some(d) = self.get_mut(o).and_then(|h| h.down.as_mut()) {
            let delay = d.delays.next().map_or(p.probe_max, micros);
            d.next_probe = now.plus(delay);
        }
    }

    /// A network load of `o` failed at `now`. After `down_after` in a row
    /// `o` is down; its first probe may start at once.
    pub fn failed(&mut self, o: Origin, now: Micros, p: &OriginBackoff) {
        if self.get(o).is_none() {
            self.by.push((o, Health::default()));
        }
        let Some(h) = self.get_mut(o) else {
            return;
        };
        h.failures = h.failures.saturating_add(1);
        if h.failures >= p.down_after && h.down.is_none() {
            let delays = ExponentialBuilder::new()
                .with_min_delay(duration(p.probe_first))
                .with_max_delay(duration(p.probe_max))
                .with_factor(2.0)
                .with_jitter()
                .with_jitter_seed(o.seed())
                .without_max_times()
                .build();
            h.down = Some(Box::new(Down {
                next_probe: now,
                delays,
            }));
        }
    }

    /// A network load of `o` was answered: its failures are forgotten.
    /// Returns whether `o` was down — the signal to retry its resources
    /// that back off.
    pub fn answered(&mut self, o: Origin) -> bool {
        let Some(i) = self.by.iter().position(|(k, _)| *k == o) else {
            return false;
        };
        self.by.swap_remove(i).1.down.is_some()
    }

    /// How many origins are down.
    pub fn down(&self) -> usize {
        self.by.iter().filter(|(_, h)| h.down.is_some()).count()
    }

    /// Whether any origin is down.
    pub fn any_down(&self) -> bool {
        self.by.iter().any(|(_, h)| h.down.is_some())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const P: OriginBackoff = OriginBackoff::DEFAULT;
    const O: Origin = Origin::Mesh(Class::TerrainMesh);

    #[test]
    fn an_origin_is_down_after_failures_in_a_row_and_up_on_its_first_answer() {
        let mut os = Origins::default();
        let t = Micros::from_secs(1);
        os.failed(O, t, &P);
        os.failed(O, t, &P);
        assert!(!os.blocks(O, t), "two failures: still up");
        os.failed(O, t, &P);
        assert!(!os.blocks(O, t), "down, and the first probe is due at once");
        os.started(O, t, &P);
        assert!(os.blocks(O, t), "a probe in its delay");
        assert!(!os.blocks(Origin::Layer(LayerId(0)), t), "another origin");
        assert!(os.answered(O), "the answer of a down origin is its signal");
        assert!(!os.blocks(O, t));
        os.failed(O, t, &P);
        assert!(
            !os.answered(O),
            "an origin that was not down signals nothing"
        );
    }

    #[test]
    fn probes_back_off_with_jitter_under_a_ceiling_and_replay_the_same() {
        let delays = || {
            let mut os = Origins::default();
            for _ in 0..P.down_after {
                os.failed(O, Micros::ZERO, &P);
            }
            let mut now = Micros::ZERO;
            let mut out = Vec::new();
            for _ in 0..12 {
                os.started(O, now, &P);
                let next = os.get(O).unwrap().down.as_ref().unwrap().next_probe;
                out.push(next.since(now));
                now = next;
            }
            out
        };
        let a = delays();
        assert_eq!(a, delays(), "seeded: the same delays on a replay");
        assert!(a[0] >= P.probe_first && a[0] < P.probe_first.plus(P.probe_first));
        for d in &a {
            assert!(*d <= Micros(2 * P.probe_max.0), "{d:?}");
        }
        assert!(a[11] >= P.probe_max, "the delays reach their ceiling");
        assert!(a.windows(2).any(|w| w[0] != w[1]), "jittered");
    }
}
