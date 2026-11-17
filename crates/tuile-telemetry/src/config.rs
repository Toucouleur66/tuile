// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the numbers go, as whom, and how often: one shape on every platform.
//!
//! A native process reads it from the standard OpenTelemetry variables
//! ([`Config::from_vars`]); a viewer's configuration file, a page's options
//! and a plugin's settings fill the same fields by hand. Nothing in here has
//! a default destination: **no endpoint, nothing sent**.
//!
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT   the collector's base URL; /v1/metrics is appended.
//!                               Unset or empty: no metrics are sent.
//! OTEL_EXPORTER_OTLP_HEADERS    k=v,k2=v2, values URL-encoded (Authorization=Bearer%20…)
//! OTEL_RESOURCE_ATTRIBUTES      k=v,… added to the resource — only keys of the
//!                               allow-list (crate::resource::ALLOWED) are kept
//! OTEL_SERVICE_NAME             service.name (else the host's)
//! OTEL_METRIC_EXPORT_INTERVAL   milliseconds between two pushes (15000)
//! OTEL_SDK_DISABLED=true        nothing at all, whatever else is set
//! TUILE_PROFILING_ENDPOINT      Pyroscope's base URL. Unset: no profiles.
//! TUILE_PROFILING_HEADERS       as OTEL_EXPORTER_OTLP_HEADERS; the OTLP headers
//!                               when unset (one token for both, as a collector
//!                               gate usually has)
//! TUILE_PROFILING               on (the default with an endpoint) or off: off
//!                               leaves the switch in the menu, unticked
//! TUILE_PROFILING_HZ            samples a second (100)
//! ```
//!
//! **A credential is a header, never part of a URL**: an endpoint with a
//! query string or a user name is refused, since a URL ends up in logs and
//! a header does not. Header values have no `Debug`: a [`Config`] printed
//! names the headers it carries, not what they say.

use std::time::Duration;

/// Between two metric pushes unless configured: the interval a scrape of
/// the same series would have.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);

/// The shortest interval accepted: below it the push is a load of its own.
pub const SHORTEST_INTERVAL: Duration = Duration::from_secs(1);

/// CPU samples a second unless configured: the rate a farm's profiles are
/// commonly read at.
pub const DEFAULT_PROFILING_HZ: u32 = 100;

/// The highest sampling rate accepted.
pub const HIGHEST_PROFILING_HZ: u32 = 1000;

/// Request headers whose values are secret: their names are shown, never
/// their values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// Headers from `(name, value)` pairs.
    pub fn new(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }

    /// `Authorization: Bearer <token>`, the usual shape of a collector's
    /// credential.
    pub fn bearer(token: &str) -> Self {
        Self(vec![(
            "Authorization".to_owned(),
            format!("Bearer {}", token.trim()),
        )])
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The pairs, for the one place that needs them: the request.
    pub fn expose(&self) -> &[(String, String)] {
        &self.0
    }
}

impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| format!("{name}: <withheld>")))
            .finish()
    }
}

/// The CPU profiles' part of the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiling {
    /// Pyroscope's base URL, without a trailing `/`. `None`: no profiles.
    pub endpoint: Option<String>,
    pub headers: Headers,
    /// Whether profiles are taken from the start. The menu turns them on or
    /// off afterwards either way, as long as there is an endpoint.
    pub on: bool,
    /// Samples a second.
    pub hz: u32,
}

impl Default for Profiling {
    fn default() -> Self {
        Self {
            endpoint: None,
            headers: Headers::default(),
            on: true,
            hz: DEFAULT_PROFILING_HZ,
        }
    }
}

/// Where and how a process sends its numbers. See the module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The collector's base URL, without the signal's path and without a
    /// trailing `/`. `None`: no metrics are sent.
    pub endpoint: Option<String>,
    pub headers: Headers,
    /// Resource attributes the configuration adds, before the allow-list:
    /// what is not on it is dropped when the resource is built.
    pub attributes: Vec<(String, String)>,
    /// `service.name`, when the configuration names one; the host's otherwise.
    pub service_name: Option<String>,
    /// Between two pushes.
    pub interval: Duration,
    pub profiling: Profiling,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoint: None,
            headers: Headers::default(),
            attributes: Vec::new(),
            service_name: None,
            interval: DEFAULT_INTERVAL,
            profiling: Profiling::default(),
        }
    }
}

impl Config {
    /// Whether any number leaves the process.
    pub fn sends_anything(&self) -> bool {
        self.endpoint.is_some() || self.profiling.endpoint.is_some()
    }

    /// The configuration the variables (read through `var`) state. `Ok` with
    /// no endpoint when they name none; `Err` names the variable at fault,
    /// never a header's value.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let set = |name: &str| {
            var(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        if set("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            return Ok(Self::default());
        }
        let mut config = Self::default();
        if let Some(endpoint) = set("OTEL_EXPORTER_OTLP_ENDPOINT") {
            config.endpoint = Some(
                endpoint_url(&endpoint)
                    .map_err(|why| format!("OTEL_EXPORTER_OTLP_ENDPOINT {why}"))?,
            );
        }
        if let Some(list) = set("OTEL_EXPORTER_OTLP_HEADERS") {
            config.headers =
                Headers(pairs(&list).map_err(|why| format!("OTEL_EXPORTER_OTLP_HEADERS: {why}"))?);
        }
        if let Some(list) = set("OTEL_RESOURCE_ATTRIBUTES") {
            config.attributes =
                pairs(&list).map_err(|why| format!("OTEL_RESOURCE_ATTRIBUTES: {why}"))?;
        }
        config.service_name = set("OTEL_SERVICE_NAME");
        if let Some(ms) = set("OTEL_METRIC_EXPORT_INTERVAL") {
            config.interval =
                interval_ms(&ms).map_err(|why| format!("OTEL_METRIC_EXPORT_INTERVAL {why}"))?;
        }
        if let Some(endpoint) = set("TUILE_PROFILING_ENDPOINT") {
            config.profiling.endpoint = Some(
                endpoint_url(&endpoint).map_err(|why| format!("TUILE_PROFILING_ENDPOINT {why}"))?,
            );
        }
        config.profiling.headers = match set("TUILE_PROFILING_HEADERS") {
            Some(list) => {
                Headers(pairs(&list).map_err(|why| format!("TUILE_PROFILING_HEADERS: {why}"))?)
            }
            None => config.headers.clone(),
        };
        if let Some(on) = set("TUILE_PROFILING") {
            config.profiling.on = switch(&on).map_err(|why| format!("TUILE_PROFILING {why}"))?;
        }
        if let Some(hz) = set("TUILE_PROFILING_HZ") {
            config.profiling.hz =
                profiling_hz(&hz).map_err(|why| format!("TUILE_PROFILING_HZ {why}"))?;
        }
        Ok(config)
    }

    /// `self`, with what it leaves unsaid taken from `fallback`: the
    /// variables over a configuration file, a file over nothing. The
    /// endpoint decides — an endpoint here brings its own headers, never
    /// the fallback's, so a token is never sent where it was not meant for.
    pub fn or(self, fallback: Self) -> Self {
        let (endpoint, headers) = match self.endpoint {
            Some(e) => (Some(e), self.headers),
            None => (fallback.endpoint, fallback.headers),
        };
        let profiling = match self.profiling.endpoint {
            Some(_) => self.profiling,
            None => fallback.profiling,
        };
        let mut attributes = fallback.attributes;
        attributes.extend(self.attributes);
        Self {
            endpoint,
            headers,
            attributes,
            service_name: self.service_name.or(fallback.service_name),
            interval: if self.interval == DEFAULT_INTERVAL {
                fallback.interval
            } else {
                self.interval
            },
            profiling,
        }
    }
}

/// `text` as an endpoint: `http://` or `https://`, a host, no query, no
/// fragment, no user name or password — a credential is a header — and no
/// trailing `/`.
pub fn endpoint_url(text: &str) -> Result<String, String> {
    let text = text.trim();
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .ok_or("must start with http:// or https://")?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err("names no host".into());
    }
    if host.contains('@') {
        return Err("holds a user name or a password: a credential goes in a header".into());
    }
    if text.contains('?') || text.contains('#') {
        return Err("has a query or a fragment: a credential goes in a header".into());
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("holds spaces or control characters".into());
    }
    Ok(text.trim_end_matches('/').to_owned())
}

/// `k=v,k2=v2`, each part trimmed and percent-decoded, as the OTLP exporter
/// specification writes headers and resource attributes. A part that is
/// not `key=value` is an error naming its position, not its text.
pub fn pairs(list: &str) -> Result<Vec<(String, String)>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .enumerate()
        .map(|(i, part)| match part.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => Ok((decode(k.trim()), decode(v.trim()))),
            _ => Err(format!("part {} is not key=value", i + 1)),
        })
        .collect()
}

/// Percent-decoding, by hand: two hex digits after a `%`, the rest as it is.
/// A malformed escape is kept literally, as the specification's decoders do.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `on` or `off` (also `true`/`false`, `1`/`0`).
pub fn switch(text: &str) -> Result<bool, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => Err("is on or off".into()),
    }
}

/// Milliseconds between two pushes, at least [`SHORTEST_INTERVAL`].
pub fn interval_ms(text: &str) -> Result<Duration, String> {
    let ms: u64 = text
        .trim()
        .parse()
        .map_err(|_| "is a number of milliseconds".to_owned())?;
    let interval = Duration::from_millis(ms);
    if interval < SHORTEST_INTERVAL {
        return Err(format!("is at least {} ms", SHORTEST_INTERVAL.as_millis()));
    }
    Ok(interval)
}

/// A sampling rate, 1 to [`HIGHEST_PROFILING_HZ`].
pub fn profiling_hz(text: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(hz) if (1..=HIGHEST_PROFILING_HZ).contains(&hz) => Ok(hz),
        _ => Err(format!(
            "is a whole number from 1 to {HIGHEST_PROFILING_HZ}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(list: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = list
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    /// No endpoint, nothing sent: the default, and what no variable gives.
    #[test]
    fn nothing_is_configured_by_default() {
        let config = Config::from_vars(vars(&[])).expect("no variables");
        assert_eq!(config, Config::default());
        assert!(!config.sends_anything());
        // Headers and attributes without an endpoint still send nothing.
        let config = Config::from_vars(vars(&[
            ("OTEL_EXPORTER_OTLP_HEADERS", "Authorization=Bearer%20abc"),
            ("OTEL_RESOURCE_ATTRIBUTES", "deployment.environment=test"),
        ]))
        .expect("headers alone");
        assert!(!config.sends_anything());
    }

    #[test]
    fn the_standard_variables_are_read() {
        let config = Config::from_vars(vars(&[
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                " https://collector.example/otlp/ ",
            ),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "Authorization=Bearer%20s3cret, x-scope = a",
            ),
            ("OTEL_RESOURCE_ATTRIBUTES", "deployment.environment=test"),
            ("OTEL_SERVICE_NAME", "my-viewer"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "5000"),
            (
                "TUILE_PROFILING_ENDPOINT",
                "https://collector.example/pyroscope",
            ),
            ("TUILE_PROFILING", "off"),
            ("TUILE_PROFILING_HZ", "49"),
        ]))
        .expect("all of them");
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://collector.example/otlp")
        );
        assert_eq!(
            config.headers.expose(),
            [
                ("Authorization".to_owned(), "Bearer s3cret".to_owned()),
                ("x-scope".to_owned(), "a".to_owned())
            ]
        );
        assert_eq!(
            config.attributes,
            [("deployment.environment".into(), "test".into())]
        );
        assert_eq!(config.service_name.as_deref(), Some("my-viewer"));
        assert_eq!(config.interval, Duration::from_secs(5));
        let p = &config.profiling;
        assert_eq!(
            p.endpoint.as_deref(),
            Some("https://collector.example/pyroscope")
        );
        assert!(!p.on);
        assert_eq!(p.hz, 49);
        // The profiles take the OTLP headers when none of their own are set.
        assert_eq!(p.headers, config.headers);
    }

    #[test]
    fn the_sdk_switch_turns_everything_off() {
        let config = Config::from_vars(vars(&[
            ("OTEL_SDK_DISABLED", "true"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.example"),
            ("TUILE_PROFILING_ENDPOINT", "https://collector.example/p"),
        ]))
        .expect("disabled");
        assert!(!config.sends_anything());
    }

    /// A credential in a URL ends up in a log: refused, wherever it is put.
    #[test]
    fn a_credential_in_a_url_is_refused() {
        for bad in [
            "https://user:s3cret@collector.example",
            "https://collector.example/otlp?token=s3cret",
            "https://collector.example/otlp#s3cret",
            "collector.example",
            "ftp://collector.example",
            "https://",
        ] {
            let e =
                Config::from_vars(vars(&[("OTEL_EXPORTER_OTLP_ENDPOINT", bad)])).expect_err(bad);
            assert!(e.starts_with("OTEL_EXPORTER_OTLP_ENDPOINT"), "{e}");
            assert!(!e.contains("s3cret"), "the value leaked: {e}");
        }
    }

    /// An error names the variable, never what a header says.
    #[test]
    fn errors_name_the_variable_not_the_value() {
        let e = Config::from_vars(vars(&[("OTEL_EXPORTER_OTLP_HEADERS", "Bearer s3cret")]))
            .expect_err("not key=value");
        assert!(e.contains("OTEL_EXPORTER_OTLP_HEADERS"), "{e}");
        assert!(!e.contains("s3cret"), "{e}");
        for (var, bad) in [
            ("OTEL_METRIC_EXPORT_INTERVAL", "10"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "soon"),
            ("TUILE_PROFILING", "maybe"),
            ("TUILE_PROFILING_HZ", "0"),
            ("TUILE_PROFILING_HZ", "5000"),
        ] {
            let e = Config::from_vars(vars(&[(var, bad)])).expect_err(var);
            assert!(e.starts_with(var), "{e}");
        }
    }

    /// A printed configuration names its headers and hides their values.
    #[test]
    fn headers_print_without_their_values() {
        let config = Config {
            endpoint: Some("https://collector.example".into()),
            headers: Headers::bearer("s3cret"),
            ..Config::default()
        };
        let shown = format!("{config:?}");
        assert!(shown.contains("Authorization: <withheld>"), "{shown}");
        assert!(!shown.contains("s3cret"), "{shown}");
    }

    /// The variables over the file — and a file's token never follows
    /// another endpoint.
    #[test]
    fn variables_win_and_tokens_stay_with_their_endpoint() {
        let file = Config {
            endpoint: Some("https://file.example".into()),
            headers: Headers::bearer("file-token"),
            interval: Duration::from_secs(30),
            ..Config::default()
        };
        let env = Config {
            endpoint: Some("https://env.example".into()),
            ..Config::default()
        };
        let merged = env.or(file.clone());
        assert_eq!(merged.endpoint.as_deref(), Some("https://env.example"));
        assert!(
            merged.headers.is_empty(),
            "the file's token followed another endpoint"
        );
        assert_eq!(merged.interval, Duration::from_secs(30));
        let merged = Config::default().or(file.clone());
        assert_eq!(merged, file);
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(decode("Bearer%20a%2Cb"), "Bearer a,b");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("a%2"), "a%2");
    }
}
