// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer's performance numbers, sent where its configuration says —
//! and nowhere when it says nothing, which is the default.
//!
//! One configuration on every platform ([`tuile_telemetry::Config`]):
//!
//! - a desktop reads the standard variables (`OTEL_EXPORTER_OTLP_ENDPOINT`,
//!   `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_RESOURCE_ATTRIBUTES`,
//!   `TUILE_PROFILING_ENDPOINT`, …), and falls back on the configuration
//!   file's `[telemetry]` section (`crate::config_file`), whose token is kept
//!   in secure storage;
//! - a phone has no environment: its `[telemetry]` comes in the
//!   configuration file it opened, as the rest of its settings do;
//! - a page passes `telemetry: { endpoint, headers, … }` in its options
//!   (`crate::web::options`).
//!
//! Both switches are the menu's (*Telemetry*), the steering URL's
//! (`tuile://telemetry?metrics=off`) and the scripts', and act at once.
//! What is sent, and what never is, is `tuile_telemetry`'s README.

/// The service the numbers are filed under unless the configuration names
/// another.
pub(crate) const SERVICE: &str = "tuile-viewer";

/// The commit this build was made from, when the build said
/// (`TUILE_GIT_COMMIT=$(git rev-parse HEAD) cargo build …`).
pub(crate) const COMMIT: Option<&str> = option_env!("TUILE_GIT_COMMIT");

/// The kind of machine, as dashboards group it: the profile's own split.
pub(crate) fn device_class() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        "browser"
    } else if cfg!(any(target_os = "ios", target_os = "android")) {
        "handheld"
    } else {
        "desktop"
    }
}

/// Who is sending, from `realm`: `native`, a page's `page` or `worker`.
pub(crate) fn identity(realm: &'static str) -> tuile_telemetry::Identity<'static> {
    tuile_telemetry::Identity {
        service_name: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        commit: COMMIT,
        device_class: device_class(),
        realm,
    }
}

/// Hands the camera's pose to the next push, when metrics are sent at all:
/// `camera` the one between the eyes, `above_ground` the height over the
/// ground the viewer measured for its readout. The pose is the camera's
/// alone — the device's location (`crate::location`) never reaches here.
pub(crate) fn note_the_camera(camera: &tuile_camera::GlobeCamera, above_ground: f64) {
    if tuile_telemetry::offered().metrics && tuile_telemetry::metrics_on() {
        tuile_telemetry::set_camera(pose(camera, above_ground));
    }
}

/// The pose the push sends for `camera`.
pub(crate) fn pose(
    camera: &tuile_camera::GlobeCamera,
    above_ground: f64,
) -> tuile_telemetry::CameraPose {
    tuile_telemetry::CameraPose::of(camera, above_ground)
}

/// The configuration a file's `[telemetry]` and its token state.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn from_file(
    settings: &crate::config_file::TelemetrySettings,
    token: Option<&crate::config_file::Token>,
) -> tuile_telemetry::Config {
    let headers = token
        .map(|t| tuile_telemetry::Headers::bearer(t.expose()))
        .unwrap_or_default();
    let mut config = tuile_telemetry::Config {
        endpoint: settings.endpoint.clone(),
        headers: headers.clone(),
        ..tuile_telemetry::Config::default()
    };
    if let Some(seconds) = settings.interval {
        config.interval = std::time::Duration::from_secs(seconds);
    }
    if let Some(environment) = &settings.environment {
        config
            .attributes
            .push(("deployment.environment".to_owned(), environment.clone()));
    }
    config.profiling.endpoint = settings.profiling_endpoint.clone();
    // One token for both: a collector's gate usually checks the same.
    config.profiling.headers = headers;
    if let Some(on) = settings.profiling {
        config.profiling.on = on;
    }
    if let Some(hz) = settings.profiling_hz {
        config.profiling.hz = hz;
    }
    config
}

/// What this process is configured to send: the variables, else the
/// configuration file. An unreadable variable is said, naming it, and sends
/// nothing rather than something unintended.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn configured() -> tuile_telemetry::Config {
    let file = crate::import::running()
        .telemetry
        .map(|settings| from_file(&settings, crate::import::telemetry_token().as_ref()))
        .unwrap_or_default();
    match tuile_telemetry::Config::from_vars(|k| std::env::var(k).ok()) {
        Ok(vars) => vars.or(file),
        Err(why) => {
            tracing::error!("telemetry: {why}; nothing is sent");
            tuile_telemetry::Config::default()
        }
    }
}

/// Starts the metrics — the local endpoint when `endpoint` (a desktop's
/// profile), the push when configured — and the profiles when configured.
/// The profiles wait for the GPU ([`crate::app`]'s setup says it).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn start(handle: &tokio::runtime::Handle, endpoint: bool) {
    let config = configured();
    let registry = tuile_telemetry::start_native(handle, &config, identity("native"));
    tuile_metrics::serve_with(
        handle,
        endpoint,
        registry.map(|r| Box::new(r) as Box<dyn metrics::Recorder + Send + Sync>),
    );
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::config_file::{TelemetrySettings, Token};

    /// What a push of the viewer's carries of where things are: the camera's
    /// place, wherever it was flown — and nothing of the device's own
    /// location, even with a fix in hand.
    #[test]
    fn the_device_s_location_never_reaches_an_export() {
        // The device's fix, with digits nothing else here has.
        let fix = crate::location::Fix {
            lon: 2.294_481_7,
            lat: 48.858_370_3,
            accuracy: 12.0,
        };
        // The camera, flown elsewhere.
        let camera = tuile_camera::GlobeCamera::from_geodetic(
            45.832_6f64.to_radians(),
            6.865_2f64.to_radians(),
            9000.0,
            0.5,
            0.6,
            1.0,
        );
        let points = pose(&camera, 4200.0).points();
        let out =
            tuile_telemetry::otlp::encode(&tuile_telemetry::Resource::default(), &points, 0, 1);
        assert_eq!(out.series, 8);
        assert!(out.body.contains("tuile_camera_latitude_degrees"));
        for digits in ["2.294481", "48.85837"] {
            assert!(!out.body.contains(digits), "{digits} in {}", out.body);
        }
        assert!(out.body.contains("45.8326"), "{}", out.body);
        assert!(!out.body.contains(&fix.lat.to_string()[..7]));
    }

    #[test]
    fn a_file_s_section_is_a_configuration() {
        let settings = TelemetrySettings {
            endpoint: Some("https://collector.example/otlp".into()),
            interval: Some(30),
            environment: Some("beta".into()),
            profiling_endpoint: Some("https://collector.example/pyroscope".into()),
            profiling: Some(false),
            profiling_hz: Some(49),
        };
        let token = Token::new("fake.0123456789abcdef.not-a-real-token");
        let config = from_file(&settings, Some(&token));
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://collector.example/otlp")
        );
        assert_eq!(
            config.headers.expose(),
            [(
                "Authorization".to_owned(),
                "Bearer fake.0123456789abcdef.not-a-real-token".to_owned()
            )]
        );
        assert_eq!(config.interval, std::time::Duration::from_secs(30));
        assert_eq!(
            config.attributes,
            [("deployment.environment".to_owned(), "beta".to_owned())]
        );
        assert!(!config.profiling.on);
        assert_eq!(config.profiling.hz, 49);
        assert_eq!(config.profiling.headers, config.headers);
        // No section, nothing sent.
        assert!(!from_file(&TelemetrySettings::default(), None).sends_anything());
    }
}
