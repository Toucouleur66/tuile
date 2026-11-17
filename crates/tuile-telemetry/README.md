<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-telemetry

Sends a viewer's performance numbers to a collector of the host's choosing,
while it runs: the engine's metrics over OTLP/HTTP from every platform, and
sampled CPU profiles to Pyroscope from a desktop.

**Nothing is sent unless an endpoint is configured** — there is no default
destination, in code, in a build or in a document. Both pushes can be
switched off and on again while the program runs ([`set_metrics`],
[`set_profiling`]; the viewer's menu, a steering URL).

## What is sent

- **Metrics**, every interval (15 s by default), cumulative from the
  process's start: every series [`tuile_metrics`] publishes, under the same
  names — the loads, the store, the frames, the uploads, the graphics
  budget's signals (`tuile_budget_*`: bytes per class against targets and
  limits, evictions, loads in flight, fallbacks, lost tiles, decide time) —
  plus the histograms recorded through the `metrics` facade: tile reads by
  place (`tuile_tiles_fetch_seconds{source, kind}`), frame time and interval
  (`tuile_frame_time_seconds`, `tuile_frame_interval_seconds`) and uploads
  per frame (`tuile_frame_uploads`). In a browser, each tile request's
  duration and bytes from the Performance API, `source="cache"` when the
  browser's cache answered.
- **CPU profiles** (macOS and Linux, the `profiling` feature): pprof-rs
  samples at the configured rate, per thread, pushed to Pyroscope every ten
  seconds, tagged with the version, the OS, the device class and the GPU.
  Not on a phone, not in a browser.

## What is never sent

No location, no camera position or path, no tile identifier or address, no
source URL, no user data. Two allow-lists are applied where the data leaves:
the resource attributes ([`resource::ALLOWED`]: service name and version,
commit, OS, architecture, device class, GPU adapter and backend, a random
identifier drawn at each launch) and the metric labels
([`resource::ALLOWED_LABELS`], the engine's closed sets). A series with any
other label is withheld whole; an attribute off the list is dropped.

## Configuration

One shape everywhere ([`Config`]); a native process reads the standard
variables:

| variable | meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | the collector's base URL (`/v1/metrics` is appended); unset: no metrics |
| `OTEL_EXPORTER_OTLP_HEADERS` | `Authorization=Bearer%20<token>`: the credential, as a header |
| `OTEL_RESOURCE_ATTRIBUTES` | extra attributes, allow-listed (`deployment.environment=…`) |
| `OTEL_SERVICE_NAME` | the service name, else the host's |
| `OTEL_METRIC_EXPORT_INTERVAL` | milliseconds between pushes (15000) |
| `OTEL_SDK_DISABLED=true` | nothing at all |
| `TUILE_PROFILING_ENDPOINT` | Pyroscope's base URL; unset: no profiles |
| `TUILE_PROFILING_HEADERS` | its headers; the OTLP headers when unset |
| `TUILE_PROFILING` | `on` (default with an endpoint) or `off`: offered in the menu, not started |
| `TUILE_PROFILING_HZ` | samples a second (100) |

A credential is a header, never part of a URL: an endpoint with a query
string or a user name is refused. Header values are never printed.

## Example

```rust,no_run
# #[cfg(feature = "native")]
fn start(runtime: &tokio::runtime::Handle) {
    let config = tuile_telemetry::Config::from_vars(|k| std::env::var(k).ok())
        .unwrap_or_default();
    let identity = tuile_telemetry::Identity {
        service_name: "my-viewer",
        version: env!("CARGO_PKG_VERSION"),
        commit: None,
        device_class: "desktop",
        realm: "native",
    };
    let registry = tuile_telemetry::start_native(runtime, &config, identity);
    // The Prometheus endpoint and the push, from one recorder.
    tuile_metrics::serve_with(runtime, true, registry.map(|r| Box::new(r) as _));
    tuile_telemetry::ready();
}
```

## Where it sits

Depends on `tuile-core` and `tuile-metrics`. Used by `tuile-viewer` (desktop,
phone, page) and `tuile-bevy`. The encoding is OTLP/JSON, one encoder for
every platform so that a page and a desktop send the same shapes.

## Features

- `native`: the push from a native process, on a tokio runtime.
- `profiling`: CPU profiles to Pyroscope (macOS and Linux).

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
