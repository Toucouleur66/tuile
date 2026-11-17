// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The recorder the push reads: every series the facade is handed, kept as
//! its latest value — cumulative counters, gauges, and histograms with fixed
//! buckets — until a push copies them out.
//!
//! Recording is an atomic operation on a handle the facade caches; only
//! registering a series takes the lock, and the engine's series are
//! registered once per publish period (`tuile_metrics::publish`), not per
//! frame.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use metrics::atomics::AtomicU64;
use metrics::{
    Counter, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};

/// The buckets of a histogram nobody named: a millisecond to half a minute,
/// doubling — the tile reads' own.
const DEFAULT_BUCKETS: &[f64] = &tuile_core::metrics::TILE_FETCH_SECONDS_BUCKETS;

/// A histogram with fixed bounds: a count per bucket, the count and the sum.
#[derive(Debug)]
pub(crate) struct Buckets {
    bounds: &'static [f64],
    /// One more than the bounds: the last is everything above the highest.
    counts: Vec<AtomicU64>,
    count: AtomicU64,
    /// An `f64`, as its bits.
    sum: AtomicU64,
}

impl Buckets {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0f64.to_bits()),
        }
    }
}

impl HistogramFn for Buckets {
    fn record(&self, value: f64) {
        if !value.is_finite() {
            return;
        }
        // The first bound at or above the value: Prometheus's `le`.
        let i = self.bounds.partition_point(|b| *b < value);
        self.counts[i].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // A compare-and-swap loop: the same on the standard library's atomic
        // and on the portable one a 32-bit target gets.
        let mut bits = self.sum.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(bits) + value).to_bits();
            match self
                .sum
                .compare_exchange_weak(bits, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(now) => bits = now,
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Handle {
    Counter(Arc<AtomicU64>),
    Gauge(Arc<AtomicU64>),
    Histogram(Arc<Buckets>),
}

/// A series' value, copied out.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Monotonic since the process started.
    Counter(u64),
    Gauge(f64),
    Histogram {
        bounds: &'static [f64],
        /// Per bucket, not cumulative; one more than the bounds.
        counts: Vec<u64>,
        count: u64,
        sum: f64,
    },
}

/// One series, copied out: its name, its labels, its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: Value,
}

/// The recorder. Cheap to clone: every clone is the same registry.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    series: Arc<Mutex<HashMap<Key, Handle>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every series and its value now, sorted by name then labels.
    pub fn snapshot(&self) -> Vec<Point> {
        let series: Vec<(Key, Handle)> = match self.series.lock() {
            Ok(s) => s.iter().map(|(k, h)| (k.clone(), h.clone())).collect(),
            Err(_) => return Vec::new(),
        };
        let mut points: Vec<Point> = series
            .into_iter()
            .map(|(key, handle)| Point {
                name: key.name().to_owned(),
                labels: key
                    .labels()
                    .map(|l| (l.key().to_owned(), l.value().to_owned()))
                    .collect(),
                value: match handle {
                    Handle::Counter(c) => Value::Counter(c.load(Ordering::Relaxed)),
                    Handle::Gauge(g) => Value::Gauge(f64::from_bits(g.load(Ordering::Relaxed))),
                    Handle::Histogram(h) => Value::Histogram {
                        bounds: h.bounds,
                        counts: h.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect(),
                        count: h.count.load(Ordering::Relaxed),
                        sum: f64::from_bits(h.sum.load(Ordering::Relaxed)),
                    },
                },
            })
            .collect();
        points.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.labels.cmp(&b.labels)));
        points
    }

    fn handle(&self, key: &Key, make: impl FnOnce() -> Handle) -> Option<Handle> {
        let mut series = self.series.lock().ok()?;
        Some(series.entry(key.clone()).or_insert_with(make).clone())
    }
}

impl Recorder for Registry {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        match self.handle(key, || Handle::Counter(Arc::new(AtomicU64::new(0)))) {
            Some(Handle::Counter(c)) => Counter::from_arc(c),
            _ => Counter::noop(),
        }
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        match self.handle(key, || {
            Handle::Gauge(Arc::new(AtomicU64::new(0f64.to_bits())))
        }) {
            Some(Handle::Gauge(g)) => Gauge::from_arc(g),
            _ => Gauge::noop(),
        }
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let bounds = tuile_metrics::buckets(key.name()).unwrap_or(DEFAULT_BUCKETS);
        match self.handle(key, || Handle::Histogram(Arc::new(Buckets::new(bounds)))) {
            Some(Handle::Histogram(h)) => Histogram::from_arc(h),
            _ => Histogram::noop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_gauges_and_histograms_are_kept() {
        let registry = Registry::new();
        metrics::with_local_recorder(&registry, || {
            metrics::counter!("c", "kind" => "mesh").increment(2);
            metrics::counter!("c", "kind" => "mesh").increment(3);
            metrics::counter!("abs").absolute(7);
            metrics::gauge!("g").set(1.5);
            for v in [0.0001, 0.003, 0.003, 100.0] {
                metrics::histogram!("tuile_tiles_fetch_seconds").record(v);
            }
        });
        let points = registry.snapshot();
        let names: Vec<&str> = points.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["abs", "c", "g", "tuile_tiles_fetch_seconds"]);
        assert_eq!(points[0].value, Value::Counter(7));
        assert_eq!(points[1].value, Value::Counter(5));
        assert_eq!(points[1].labels, [("kind".to_owned(), "mesh".to_owned())]);
        assert_eq!(points[2].value, Value::Gauge(1.5));
        let Value::Histogram {
            bounds,
            counts,
            count,
            sum,
        } = &points[3].value
        else {
            panic!("not a histogram: {:?}", points[3]);
        };
        assert_eq!(
            *bounds,
            &tuile_core::metrics::TILE_FETCH_SECONDS_BUCKETS[..]
        );
        assert_eq!(counts.len(), bounds.len() + 1);
        assert_eq!(*count, 4);
        assert!((sum - 100.0061).abs() < 1e-9);
        // 0.0001 ≤ 0.0005 (first bucket), 0.003 ≤ 0.004 (fourth), 100 above all.
        assert_eq!(counts[0], 1);
        assert_eq!(counts[3], 2);
        assert_eq!(counts[bounds.len()], 1);
    }

    /// A frame's histogram gets the frame's buckets, the one `tuile-metrics`
    /// gives the Prometheus endpoint.
    #[test]
    fn named_histograms_get_their_buckets() {
        let registry = Registry::new();
        metrics::with_local_recorder(&registry, || {
            metrics::histogram!("tuile_frame_time_seconds").record(0.016);
        });
        let Value::Histogram { bounds, .. } = &registry.snapshot()[0].value else {
            panic!("not a histogram");
        };
        assert_eq!(*bounds, &tuile_metrics::FRAME_SECONDS_BUCKETS[..]);
    }
}
