<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-metrics

Publishes the engine's counters to a Prometheus endpoint.

Every number used to reach a human by being printed into one wide log line
per second, mixed with five unrelated subjects. That line is unreadable, and
worse it is the wrong shape: a cumulative counter like `uploads` or
`evictions` says almost nothing as an absolute value and everything as a
*rate*, which a log forces the reader to compute by subtracting two lines by
eye. So the numbers come here, where anything that speaks Prometheus takes
their derivative, and `stdout` keeps only what is genuinely an **event**:
ground drawn black, a server that stopped, imagery that never arrived.

```text
curl -s localhost:9464/metrics | grep tuile_store
```

# Why a bridge at all

[`tuile_core::metrics`] is plain atomics: no allocation, no locks, nothing
behind a feature, and it compiles to `wasm32-unknown-unknown`, where a
registry and an HTTP listener cannot follow. The core may take no backend
dependency, so recording stays there and publishing happens here — a copy of
a few dozen integers, once a second.

`metrics` + `metrics-exporter-prometheus` own the exposition format and the
HTTP listener. Both were briefly written by hand in this file, which was a
waste and worse than what the crates do.

## Where it sits

Depends on `tuile-core` (its `metrics` counters). Used by the native hosts:
`tuile-viewer` and `tuile-bake`.

## Example

```rust,no_run
fn start(runtime: &tokio::runtime::Handle) {
    // Listens on `TUILE_METRICS_ADDR` (`off` disables it).
    tuile_metrics::serve(runtime);
}
```

A host that already exports metrics of its own calls
`tuile_metrics::publish_every` instead.

## Features

None. Native only.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
