// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod camera;
pub mod config;
pub mod logs;
pub mod otlp;
pub mod registry;
pub mod resource;
mod timing;
#[cfg(test)]
#[allow(dead_code)]
mod vcs;

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
mod native;
#[cfg(all(feature = "profiling", any(target_os = "macos", target_os = "linux")))]
mod profiling;
#[cfg(target_arch = "wasm32")]
mod web;

pub use camera::{set_camera, CameraPose};
pub use config::{Config, Headers, Profiling};
pub use registry::Registry;
pub use resource::Resource;

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
pub use native::start_native;
#[cfg(target_arch = "wasm32")]
pub use web::{on_switch, start_web};

/// The build of tuile this crate was compiled from, as its build script
/// found it: from git in a checkout of tuile, else from `TUILE_GIT_COMMIT`
/// and `TUILE_GIT_DESCRIBE` (a CI or release build, a tarball), else
/// absent. A host passes them on in its [`Identity`].
pub mod build {
    /// The commit, twelve hex digits (`git rev-parse --short=12 HEAD`).
    pub const REVISION: Option<&str> = option_env!("TUILE_BUILD_REVISION");
    /// The nearest tag and the way from it
    /// (`git describe --tags --always --dirty`: `v0.17.1-12-gabc1234`).
    pub const REF_NAME: Option<&str> = option_env!("TUILE_BUILD_REF_NAME");
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Who is sending: what the host knows of itself, before the configuration
/// adds to it.
#[derive(Debug, Clone, Copy)]
pub struct Identity<'a> {
    /// `service.name` unless the configuration names one: the program
    /// (`tuile-viewer`, a game's name).
    pub service_name: &'a str,
    /// `service.version`: the build's version.
    pub version: &'a str,
    /// `vcs.ref.head.revision`: the commit it was built from, when known.
    pub commit: Option<&'a str>,
    /// `vcs.ref.head.name`: the tag it was built from and the way from it
    /// (a `git describe`: `v0.17.1-12-gabc1234`), when known.
    pub ref_name: Option<&'a str>,
    /// `device.class`: the profile the session runs under — `desktop`,
    /// `handheld`, `browser`.
    pub device_class: &'a str,
    /// `tuile.realm`: `native`, or a page's `page` and `worker`.
    pub realm: &'a str,
}

/// What the configuration makes possible, for a menu to offer: each switch
/// is only shown when there is somewhere to send to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offered {
    pub metrics: bool,
    pub profiling: bool,
}

static METRICS_ON: AtomicBool = AtomicBool::new(true);
static PROFILING_ON: AtomicBool = AtomicBool::new(false);
static METRICS_OFFERED: AtomicBool = AtomicBool::new(false);
static PROFILING_OFFERED: AtomicBool = AtomicBool::new(false);

/// What the process says about itself on everything it sends.
fn resource_slot() -> &'static Mutex<Resource> {
    static RESOURCE: OnceLock<Mutex<Resource>> = OnceLock::new();
    RESOURCE.get_or_init(|| Mutex::new(Resource::default()))
}

/// The resource as it stands.
pub fn resource() -> Resource {
    resource_slot()
        .lock()
        .map(|r| r.clone())
        .unwrap_or_default()
}

/// Sets one resource attribute, from now on — if it is on the allow-list
/// ([`resource::ALLOWED`]).
pub fn set_attribute(key: &str, value: &str) {
    if let Ok(mut r) = resource_slot().lock() {
        r.set(key, value);
    }
}

/// The GPU the session draws with, once it is chosen: `gpu.adapter` and
/// `gpu.backend` on the metrics, and on the profiles taken from now on.
pub fn describe_gpu(adapter: &str, backend: &str) {
    set_attribute("gpu.adapter", adapter);
    set_attribute("gpu.backend", &backend.to_ascii_lowercase());
}

/// The resource is complete: profiles configured to start wait for this,
/// so that they carry the GPU. A host that never learns its GPU calls it
/// after [`start_native`].
pub fn ready() {
    #[cfg(all(feature = "profiling", any(target_os = "macos", target_os = "linux")))]
    profiling::ready();
}

/// The resource a configuration and an identity describe: the identity's
/// attributes, the platform's, a fresh session identifier, then the
/// configuration's — each held to the allow-list.
pub fn describe(config: &Config, identity: &Identity<'_>) -> Resource {
    let mut r = Resource::default();
    r.set("telemetry.sdk.name", "tuile-telemetry");
    r.set("telemetry.sdk.language", "rust");
    r.set("telemetry.sdk.version", env!("CARGO_PKG_VERSION"));
    r.set(
        "service.name",
        config
            .service_name
            .as_deref()
            .unwrap_or(identity.service_name),
    );
    r.set("service.version", identity.version);
    r.set("service.instance.id", &resource::session_id());
    if let Some(commit) = identity.commit {
        r.set("vcs.ref.head.revision", commit);
    }
    if let Some(name) = identity.ref_name {
        r.set("vcs.ref.head.name", name);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        r.set("os.type", resource::os_type());
        r.set("host.arch", std::env::consts::ARCH);
    }
    r.set("device.class", identity.device_class);
    r.set("tuile.realm", identity.realm);
    for (k, v) in &config.attributes {
        // The session's identifier is drawn here, never configured: a fixed
        // one would merge every launch's counters into one series.
        if k != "service.instance.id" {
            r.set(k, v);
        }
    }
    r
}

/// Adopts `resource` as the process's, and records what `config` offers.
fn adopt(config: &Config, resource: Resource) {
    if let Ok(mut r) = resource_slot().lock() {
        *r = resource;
    }
    METRICS_OFFERED.store(config.endpoint.is_some(), Ordering::Relaxed);
    let profiles = config.profiling.endpoint.is_some() && profiling_supported();
    PROFILING_OFFERED.store(profiles, Ordering::Relaxed);
    PROFILING_ON.store(profiles && config.profiling.on, Ordering::Relaxed);
}

/// What can be switched on: see [`Offered`].
pub fn offered() -> Offered {
    Offered {
        metrics: METRICS_OFFERED.load(Ordering::Relaxed),
        profiling: PROFILING_OFFERED.load(Ordering::Relaxed),
    }
}

/// Whether CPU profiles can be taken on this platform in this build: macOS
/// and Linux, with the `profiling` feature. Not a phone, not a browser.
pub fn profiling_supported() -> bool {
    cfg!(all(
        feature = "profiling",
        any(target_os = "macos", target_os = "linux")
    ))
}

/// Whether metrics are sent — when an endpoint is configured. On until
/// switched off.
pub fn metrics_on() -> bool {
    METRICS_ON.load(Ordering::Relaxed)
}

/// Switches the metrics' push on or off, from the next interval, without
/// a restart. Nothing is queued while off: the counters keep counting, and
/// the first push after is cumulative as every push is.
pub fn set_metrics(on: bool) {
    let was = METRICS_ON.swap(on, Ordering::Relaxed);
    if was != on {
        tracing::info!("telemetry: metrics {}", if on { "on" } else { "off" });
    }
    #[cfg(target_arch = "wasm32")]
    web::switched(on);
}

/// Whether CPU profiles are being taken.
pub fn profiling_on() -> bool {
    PROFILING_ON.load(Ordering::Relaxed)
}

/// Starts or stops the CPU profiles, without a restart; nothing where they
/// are not offered. Stopping sends what was sampled since the last push.
pub fn set_profiling(on: bool) {
    let on = on && offered().profiling;
    let was = PROFILING_ON.swap(on, Ordering::Relaxed);
    if was != on {
        tracing::info!("telemetry: CPU profiles {}", if on { "on" } else { "off" });
        #[cfg(all(feature = "profiling", any(target_os = "macos", target_os = "linux")))]
        profiling::apply();
    }
}

/// What one push sends: the registry's series, then the camera's pose.
pub(crate) fn points(registry: &Registry) -> Vec<registry::Point> {
    let mut points = registry.snapshot();
    points.extend(camera::points());
    points
}

/// Now, Unix nanoseconds: the system's clock natively, the page's in a
/// browser (where the standard library's clock panics).
pub(crate) fn unix_nanos() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default()
    }
    #[cfg(target_arch = "wasm32")]
    {
        (js_sys::Date::now() * 1e6) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_holds_the_identity_and_the_allowed_configuration() {
        let config = Config {
            attributes: vec![
                ("deployment.environment".into(), "test".into()),
                ("user.email".into(), "someone@example.com".into()),
                ("service.instance.id".into(), "fixed".into()),
            ],
            ..Config::default()
        };
        let identity = Identity {
            service_name: "tuile-viewer",
            version: "0.17.1",
            commit: Some("abc123def456"),
            ref_name: Some("v0.17.1-12-gabc123def456"),
            device_class: "desktop",
            realm: "native",
        };
        let r = describe(&config, &identity);
        assert_eq!(r.get("service.name"), Some("tuile-viewer"));
        assert_eq!(r.get("service.version"), Some("0.17.1"));
        assert_eq!(r.get("vcs.ref.head.revision"), Some("abc123def456"));
        assert_eq!(r.get("vcs.ref.head.name"), Some("v0.17.1-12-gabc123def456"));
        assert_eq!(r.get("device.class"), Some("desktop"));
        assert_eq!(r.get("deployment.environment"), Some("test"));
        assert_ne!(r.get("service.instance.id"), Some("fixed"));
        assert_eq!(r.get("service.instance.id").map(str::len), Some(32));
        assert!(r.get("user.email").is_none());
        assert!(r.iter().all(|(k, _)| resource::allowed(k)));
        let named = describe(
            &Config {
                service_name: Some("other".into()),
                ..Config::default()
            },
            &identity,
        );
        assert_eq!(named.get("service.name"), Some("other"));
    }
}
