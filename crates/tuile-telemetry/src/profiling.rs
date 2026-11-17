// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! CPU profiles to Pyroscope: pprof-rs, through Pyroscope's own Rust agent,
//! started and stopped at run time.
//!
//! Sampled at the configured rate (a `SIGPROF` timer on the process's CPU
//! time, each sample a walk of the running thread's stack, the thread's
//! name kept) and pushed every ten seconds as the service's name, tagged
//! with the version, the OS, the device class, the realm and the GPU. Not
//! the session's identifier: a profile is read across sessions.
//!
//! Starting waits for [`ready`] — the host has said which GPU it draws with
//! — so the very first profile carries it. Starting and stopping happen on
//! a thread of their own: stopping sends what was sampled since the last
//! push, which may take the agent's whole timeout, and no frame waits on it.

use std::sync::Mutex;

use crate::config::Profiling;

type Agent = pyroscope::PyroscopeAgent<pyroscope::pyroscope::PyroscopeAgentRunning>;

struct State {
    config: Profiling,
    ready: bool,
    agent: Option<Agent>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Records the configuration; nothing starts before [`ready`].
pub(crate) fn configure(config: Profiling) {
    if let Ok(mut state) = STATE.lock() {
        *state = Some(State {
            config,
            ready: false,
            agent: None,
        });
    }
}

/// The resource is complete: start if the switch says so.
pub(crate) fn ready() {
    if let Ok(mut state) = STATE.lock() {
        if let Some(state) = state.as_mut() {
            state.ready = true;
        }
    }
    apply();
}

/// Brings the agent in line with the switch, on a thread of its own.
pub(crate) fn apply() {
    let spawned = std::thread::Builder::new()
        .name("tuile-profiling".into())
        .spawn(|| {
            let Ok(mut guard) = STATE.lock() else {
                return;
            };
            let Some(state) = guard.as_mut() else {
                return;
            };
            let wanted = crate::profiling_on() && state.ready;
            match (wanted, state.agent.take()) {
                (true, None) => state.agent = start(&state.config),
                (false, Some(agent)) => stop(agent),
                (_, agent) => state.agent = agent,
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("telemetry: the profiler's thread did not start ({e})");
    }
}

/// A Pyroscope tag value: letters, digits, `.`, `-` and `_`, the rest
/// replaced — an adapter's name has spaces and parentheses.
fn tag_value(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The tags every profile carries, from the resource.
fn tags() -> Vec<(String, String)> {
    let resource = crate::resource();
    [
        ("service.version", "version"),
        ("os.type", "os"),
        ("host.arch", "arch"),
        ("device.class", "device_class"),
        ("tuile.realm", "realm"),
        ("gpu.adapter", "gpu_adapter"),
        ("gpu.backend", "gpu_backend"),
        ("deployment.environment", "environment"),
    ]
    .into_iter()
    .filter_map(|(key, tag)| Some((tag.to_owned(), tag_value(resource.get(key)?))))
    .collect()
}

fn start(config: &Profiling) -> Option<Agent> {
    use pyroscope::backend::{pprof_backend, BackendConfig, PprofConfig};
    let url = config.endpoint.as_deref()?;
    let resource = crate::resource();
    let application = resource.get("service.name").unwrap_or("tuile");
    let version = resource.get("service.version").unwrap_or("unknown");
    let backend = pprof_backend(
        PprofConfig {
            sample_rate: config.hz,
        },
        BackendConfig {
            report_thread_id: false,
            report_thread_name: true,
            report_pid: false,
        },
    );
    let tags = tags();
    let headers = config.headers.expose().iter().cloned().collect();
    let agent = pyroscope::pyroscope::PyroscopeAgentBuilder::new(
        url,
        application,
        config.hz,
        "pyroscope-rs",
        version,
        backend,
    )
    .tags(tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
    .http_headers(headers)
    .build()
    .and_then(|agent| agent.start());
    match agent {
        Ok(agent) => {
            tracing::info!(
                "telemetry: CPU sampled at {} Hz, profiles to {url}{}",
                config.hz,
                if config.headers.is_empty() {
                    ""
                } else {
                    ", with credentials"
                }
            );
            Some(agent)
        }
        Err(e) => {
            tracing::warn!("telemetry: the profiler did not start ({e}); no CPU profiles");
            None
        }
    }
}

fn stop(agent: Agent) {
    match agent.stop() {
        Ok(ready) => ready.shutdown(),
        Err(e) => tracing::warn!("telemetry: the profiler did not stop cleanly ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_values_are_cleaned() {
        assert_eq!(tag_value("Apple M2 Pro"), "Apple_M2_Pro");
        assert_eq!(tag_value("NVIDIA L4 (Vulkan)"), "NVIDIA_L4__Vulkan_");
        assert_eq!(tag_value("0.17.1"), "0.17.1");
    }
}
