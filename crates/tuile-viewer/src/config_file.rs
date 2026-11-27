// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The configuration file a tester opens to set the application up: one
//! TOML file, opened once.
//!
//! ```toml
//! [ion]                                 # the host's section: Identity::config_section
//! token = "your-access-token"           # stored in the system's secure storage
//! api = "https://api.cesium.com/"       # optional: the service's API base
//! terrain = 1                           # optional: the terrain's asset id
//!
//! [[ion.imagery]]                       # optional: replaces the host's layers
//! key = "aerial"
//! name = "Aerial"
//! asset = 2
//! attribution = "© Microsoft and its data suppliers"
//! substitute = "aerial-coarse"          # optional: the layer its far tiles are drawn from
//! substitute_gap = 4                    # optional: levels below the finest drawn (default 4)
//! substitute_hysteresis = 1             # optional: levels of hysteresis (default 1)
//!
//! [[ion.imagery]]                       # a layer from a tile server, by URL template
//! key = "aerial-coarse"
//! name = "Aerial, coarse levels"
//! template = "https://tiles.example/aerial/{z}/{x}/{y}.webp"  # XYZ: row 0 north
//! scheme = "webmercator"                # optional: or "geographic" (default webmercator)
//! max_level = 18                        # optional: the deepest level it serves
//! attribution = "© the pictures' owner"
//! token = "your-tile-token"             # optional: the server's credential; secure storage
//! token_header = "Authorization"        # optional: the header it goes in (default
//!                                       # Authorization, as `Bearer <token>`)
//!
//! [start]                               # optional: where the first session opens
//! lon = 6.86
//! lat = 45.83
//! altitude = 6000
//! heading = 120
//! pitch = 25
//!
//! [telemetry]                           # optional: performance numbers sent while running
//! endpoint = "https://collector.example/otlp"   # OTLP/HTTP base; nothing is sent without it
//! token = "your-ingest-token"           # sent as Authorization: Bearer; secure storage
//! interval = 15                         # optional: seconds between two pushes
//! environment = "beta"                  # optional: deployment.environment
//! profiling_endpoint = "https://collector.example/pyroscope"  # optional: CPU profiles
//! profiling = "on"                      # optional: on (default) or off — the menu switches it
//! profiling_hz = 100                    # optional: samples a second
//!
//! [snapshot]                            # optional: the S key's pictures
//! folder = "~/Pictures/Tuile"           # optional: where they go (this is the default)
//! format = "jpeg"                       # optional: or "png", lossless (default jpeg)
//! quality = 95                          # optional: the JPEG's, 1 to 100 (default 95)
//! layers = ["shapes"]                   # optional: drawn over the globe: shapes, hud,
//!                                       # controls (default none: the globe only)
//! ```
//!
//! A template layer answers 404 for a tile it does not have: an absence,
//! drawn from what is held around it, never a hole. A PNG, a JPEG or a WebP
//! tile is told apart by its bytes: the format is not configured.
//!
//! A template layer's `token` goes to secure storage like the section's,
//! under a key of the layer's own ([`layer_token_variable`]); the settings
//! keep its `token_header`, which says a credential is expected.
//!
//! **The token is read once and handed to secure storage.** It never reaches
//! a log line, an error message or the settings file: [`Token`] has no
//! `Display` and a `Debug` that says only that it is there, and every error
//! names the file and the key, never the value — including a TOML syntax
//! error, which is reported by line alone, since the parser's own message
//! quotes the text around the fault.
//!
//! The rest is checked here and kept as [`Settings`], written back as TOML
//! without the token. The start view is held to the command line's bounds
//! (`crate::start`), and a flag given on the command line wins over it.
//!
//! Strict on purpose, like the command line: an unknown key is an error that
//! names it, because a misspelt `terain` that is silently ignored opens a
//! session somewhere other than where the file said.

use toml::{Table, Value};

use crate::embed::ImageryChoice;

/// The longest file read: a configuration is a few hundred bytes, and
/// anything much larger is not one.
pub(crate) const LONGEST_FILE: usize = 64 * 1024;

/// The view numbers a file may say, in the command line's names and units.
const START_KEYS: [&str; 5] = ["lon", "lat", "altitude", "heading", "pitch"];

/// The keys of `[telemetry]`.
const TELEMETRY_KEYS: [&str; 7] = [
    "endpoint",
    "token",
    "interval",
    "environment",
    "profiling_endpoint",
    "profiling",
    "profiling_hz",
];

/// The keys of `[snapshot]`.
const SNAPSHOT_KEYS: [&str; 4] = ["folder", "format", "quality", "layers"];

/// An access token. Never printed: no `Display`, a `Debug` that hides it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Token(String);

impl Token {
    pub(crate) fn new(value: &str) -> Self {
        Self(value.trim().to_owned())
    }

    /// The value, for the one place that needs it: secure storage, or the
    /// request that carries it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<withheld>)")
    }
}

/// What a file says besides its token: kept in the application's settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Settings {
    /// The service's API base, ending in `/`.
    pub api: Option<url::Url>,
    /// The terrain's asset id.
    pub terrain: Option<i64>,
    /// The imagery layers, in order; empty means the host's own.
    pub imagery: Vec<ImageryChoice>,
    /// The layers of [`Settings::imagery`] that come from a URL template,
    /// by their asset number: what the sources answer them by.
    pub templates: Vec<TemplateLayer>,
    /// Start-view numbers by flag name (`lon`, `lat`, …), already checked.
    pub start: Vec<(String, f64)>,
    /// Where the performance numbers go (`[telemetry]`), without its token.
    pub telemetry: Option<TelemetrySettings>,
    /// Where snapshots go and how they are made (`[snapshot]`).
    pub snapshot: Option<crate::photo::SnapshotSettings>,
}

/// An imagery layer from a tile server, named by its URL template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemplateLayer {
    /// The layer's key in [`Settings::imagery`].
    pub key: String,
    /// The number it is asked for by: negative, never a catalog's.
    pub asset: i64,
    /// `https://…/{z}/{x}/{y}…`.
    pub template: String,
    /// On the geographic grid (two tiles at level 0) rather than
    /// web-mercator.
    pub geographic: bool,
    /// The deepest level it serves.
    pub max_level: Option<u32>,
    /// The header its credential goes in, when it takes one: the token
    /// itself is in secure storage, under [`layer_token_variable`], or
    /// handed out by its issuer.
    pub token_header: Option<String>,
    /// Where its token is handed out, when it is not given once: asked for
    /// a fresh one before it runs out and when the server refuses it, and
    /// kept in memory only (`crate::renew`).
    pub issuer: Option<TokenIssuer>,
}

/// A template layer's token issuer: a URL whose answer carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenIssuer {
    /// `https`, or `http` on this machine's loopback.
    pub url: String,
    pub answer: IssuedAs,
}

/// How an issuer's answer carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssuedAs {
    /// The cookie of this name (`Set-Cookie`, with its `Max-Age`).
    Cookie(String),
    /// This field of a JSON object (with `expires_in`, in seconds).
    Field(String),
}

/// A layer's `token_url` and its `token_cookie` or `token_field`, checked.
fn read_issuer(
    at: &dyn Fn(&str) -> String,
    text: &dyn Fn(&str) -> Result<Option<String>, Problem>,
    has_token: bool,
) -> Result<Option<TokenIssuer>, Problem> {
    let url = text("token_url")?;
    let (cookie, field) = (text("token_cookie")?, text("token_field")?);
    let Some(url) = url else {
        if cookie.is_some() {
            return Err((at("token_cookie"), "goes with a `token_url` only".into()));
        }
        if field.is_some() {
            return Err((at("token_field"), "goes with a `token_url` only".into()));
        }
        return Ok(None);
    };
    if has_token {
        return Err((
            at("token_url"),
            "a layer has a `token` or a `token_url`, not both".into(),
        ));
    }
    // Checked as an API base is: https, or http on the loopback; said
    // without the address.
    api_base(&url).map_err(|why| (at("token_url"), why))?;
    let answer = match (cookie, field) {
        (Some(name), None) => IssuedAs::Cookie(name),
        (None, Some(name)) => IssuedAs::Field(name),
        _ => {
            return Err((
                at("token_url"),
                "names its answer by a `token_cookie` or a `token_field`, one of them".into(),
            ))
        }
    };
    Ok(Some(TokenIssuer { url, answer }))
}

impl TemplateLayer {
    /// The grid its tiles are cut on.
    pub(crate) fn tiling(&self) -> tuile_core::raster::TilingScheme {
        crate::open::template_tiling(self.geographic, self.max_level)
    }
}

/// The first number a configuration file's template layers are asked for
/// by, the next ones counting down: negative, as every address's is
/// (`crate::open`), and far from the command line's own (`-1`).
pub(crate) const FIRST_TEMPLATE_ASSET: i64 = -1000;

/// The secure-storage key a template layer's token is kept under.
pub(crate) fn layer_token_variable(key: &str) -> String {
    format!(
        "TUILE_IMAGERY_TOKEN_{}",
        key.to_ascii_uppercase().replace('-', "_")
    )
}

/// `[telemetry]`, checked: see the module. Its token is kept apart, as the
/// section's is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TelemetrySettings {
    /// The OTLP/HTTP base URL, checked by `tuile_telemetry::config::endpoint_url`.
    pub endpoint: Option<String>,
    /// Seconds between two pushes.
    pub interval: Option<u64>,
    /// `deployment.environment`.
    pub environment: Option<String>,
    pub profiling_endpoint: Option<String>,
    pub profiling: Option<bool>,
    pub profiling_hz: Option<u32>,
}

impl Settings {
    /// The asset a person would recognise the file by: the first imagery
    /// layer's name, else the terrain's number.
    pub(crate) fn headline(&self) -> Option<String> {
        if let Some(layer) = self.imagery.first() {
            if layer.asset < 0 {
                return Some(layer.name.clone());
            }
            return Some(format!("{} (asset {})", layer.name, layer.asset));
        }
        self.terrain.map(|asset| format!("terrain asset {asset}"))
    }
}

/// A file, read and checked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConfigFile {
    pub token: Option<Token>,
    /// `[telemetry]`'s token: the collector's, never the section's.
    pub telemetry_token: Option<Token>,
    /// The template layers' tokens, by layer key.
    pub layer_tokens: Vec<(String, Token)>,
    pub settings: Settings,
}

/// What was wrong with a file: which file, which key, and why — never a
/// value from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigError {
    pub file: String,
    /// The key's dotted path (`ion.imagery[1].asset`), or what stands for
    /// one (`line 4`) when the file could not be read as TOML at all.
    pub key: String,
    pub why: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.key.is_empty() {
            write!(f, "{}: {}", self.file, self.why)
        } else {
            write!(f, "{}: {}: {}", self.file, self.key, self.why)
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads `text`, the contents of the file called `file`, whose credential
/// section is `section` (the host's: `ion` for the public viewer).
pub(crate) fn parse(file: &str, text: &str, section: &str) -> Result<ConfigFile, ConfigError> {
    let fail = |key: &str, why: &str| ConfigError {
        file: file.to_owned(),
        key: key.to_owned(),
        why: why.to_owned(),
    };
    if text.len() > LONGEST_FILE {
        return Err(fail(
            "",
            &format!(
                "larger than {} KiB: not a configuration",
                LONGEST_FILE / 1024
            ),
        ));
    }
    let table: Table = text.parse().map_err(|e: toml::de::Error| {
        // The span alone: the message quotes the text around the fault, and
        // that text may be the token's line.
        let line = e
            .span()
            .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
        fail(
            &line.map_or_else(String::new, |n| format!("line {n}")),
            "not valid TOML",
        )
    })?;

    let mut token = None;
    let mut telemetry_token = None;
    let mut layer_tokens = Vec::new();
    let mut settings = Settings::default();
    for (key, value) in &table {
        if key == section {
            let Value::Table(source) = value else {
                return Err(fail(key, "must be a table: [section]"));
            };
            read_section(
                section,
                source,
                &mut token,
                &mut layer_tokens,
                &mut settings,
            )
            .map_err(|(k, why)| fail(&k, &why))?;
        } else if key == "start" {
            let Value::Table(start) = value else {
                return Err(fail(key, "must be a table: [start]"));
            };
            settings.start = read_start(start).map_err(|(k, why)| fail(&k, &why))?;
        } else if key == "telemetry" {
            let Value::Table(telemetry) = value else {
                return Err(fail(key, "must be a table: [telemetry]"));
            };
            let (read, its_token) = read_telemetry(telemetry).map_err(|(k, why)| fail(&k, &why))?;
            settings.telemetry = Some(read);
            telemetry_token = its_token;
        } else if key == "snapshot" {
            let Value::Table(snapshot) = value else {
                return Err(fail(key, "must be a table: [snapshot]"));
            };
            settings.snapshot = Some(read_snapshot(snapshot).map_err(|(k, why)| fail(&k, &why))?);
        } else {
            return Err(fail(
                key,
                &format!(
                    "unknown section; this file knows [{section}], [start], [telemetry] \
                     and [snapshot]"
                ),
            ));
        }
    }
    if token.is_none() && telemetry_token.is_none() && settings == Settings::default() {
        return Err(fail(
            section,
            &format!("missing: the file says nothing; it needs at least [{section}] token"),
        ));
    }
    Ok(ConfigFile {
        token,
        telemetry_token,
        layer_tokens,
        settings,
    })
}

type Problem = (String, String);

/// A token as a file gives it, checked; the problem names `at`, never the
/// value.
fn read_token(at: &str, value: &Value) -> Result<Token, Problem> {
    let Value::String(text) = value else {
        return Err((at.to_owned(), "must be a string in quotes".into()));
    };
    let given = Token::new(text);
    if given.expose().is_empty() {
        return Err((at.to_owned(), "is empty".into()));
    }
    if given
        .expose()
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err((
            at.to_owned(),
            "holds spaces or line breaks: copy the token again".into(),
        ));
    }
    if looks_like_placeholder(given.expose()) {
        return Err((
            at.to_owned(),
            "is still the example's placeholder: put your own token there".into(),
        ));
    }
    Ok(given)
}

fn read_section(
    section: &str,
    source: &Table,
    token: &mut Option<Token>,
    layer_tokens: &mut Vec<(String, Token)>,
    settings: &mut Settings,
) -> Result<(), Problem> {
    let at = |key: &str| format!("{section}.{key}");
    for (key, value) in source {
        match key.as_str() {
            "token" => *token = Some(read_token(&at(key), value)?),
            "api" => {
                let Value::String(text) = value else {
                    return Err((at(key), "must be a URL in quotes".into()));
                };
                settings.api = Some(api_base(text).map_err(|why| (at(key), why))?);
            }
            "terrain" => settings.terrain = Some(asset(value).map_err(|why| (at(key), why))?),
            "imagery" => {
                let Value::Array(layers) = value else {
                    return Err((at(key), "must be a list of [[imagery]] tables".into()));
                };
                let read = read_imagery(section, layers)?;
                settings.imagery = read.layers;
                settings.templates = read.templates;
                *layer_tokens = read.tokens;
            }
            _ => {
                return Err((
                    at(key),
                    "unknown key; the keys are token, api, terrain and imagery".into(),
                ))
            }
        }
    }
    Ok(())
}

/// The `[[imagery]]` list, read.
struct Imagery {
    layers: Vec<ImageryChoice>,
    templates: Vec<TemplateLayer>,
    tokens: Vec<(String, Token)>,
}

fn read_imagery(section: &str, layers: &[Value]) -> Result<Imagery, Problem> {
    let mut out: Vec<ImageryChoice> = Vec::new();
    let mut templates: Vec<TemplateLayer> = Vec::new();
    let mut tokens: Vec<(String, Token)> = Vec::new();
    // (layer, the key of its substitute, gap, hysteresis, where it was said)
    let mut substitutes: Vec<(usize, String, u32, u32, String)> = Vec::new();
    for (i, layer) in layers.iter().enumerate() {
        let at = |key: &str| {
            format!(
                "{section}.imagery[{i}]{}{key}",
                if key.is_empty() { "" } else { "." }
            )
        };
        let Value::Table(layer) = layer else {
            return Err((at(""), "must be a table".into()));
        };
        let text = |key: &str| -> Result<Option<String>, Problem> {
            match layer.get(key) {
                None => Ok(None),
                Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
                Some(_) => Err((at(key), "must be a non-empty string".into())),
            }
        };
        const KEYS: [&str; 15] = [
            "key",
            "name",
            "asset",
            "template",
            "scheme",
            "max_level",
            "token",
            "token_header",
            "token_url",
            "token_cookie",
            "token_field",
            "attribution",
            "substitute",
            "substitute_gap",
            "substitute_hysteresis",
        ];
        for key in layer.keys() {
            if !KEYS.contains(&key.as_str()) {
                return Err((
                    at(key),
                    "unknown key; a layer has key, name, asset or template (with scheme, \
                     max_level, token or token_url with token_cookie or token_field, and \
                     token_header), attribution, substitute, substitute_gap and \
                     substitute_hysteresis"
                        .into(),
                ));
            }
        }
        let number = |key: &str, default: u32| -> Result<u32, Problem> {
            match layer.get(key) {
                None => Ok(default),
                Some(Value::Integer(n)) => u32::try_from(*n)
                    .map_err(|_| (at(key), "must be a small positive number".into())),
                Some(_) => Err((at(key), "must be a number".into())),
            }
        };
        if let Some(of) = text("substitute")? {
            substitutes.push((
                out.len(),
                of,
                number("substitute_gap", tuile_core::budget::FallbackRule::GAP)?,
                number(
                    "substitute_hysteresis",
                    tuile_core::budget::FallbackRule::HYSTERESIS,
                )?,
                at("substitute"),
            ));
        }
        let template = text("template")?;
        if template.is_none() {
            for only in [
                "scheme",
                "max_level",
                "token",
                "token_header",
                "token_url",
                "token_cookie",
                "token_field",
            ] {
                if layer.contains_key(only) {
                    return Err((at(only), "goes with a `template` only".into()));
                }
            }
        }
        let asset = match (layer.get("asset"), &template) {
            (Some(value), None) => asset(value).map_err(|why| (at("asset"), why))?,
            (None, Some(template)) => {
                // Said without the template: the module quotes no value.
                crate::open::check_template("the template", template).map_err(|_| {
                    (
                        at("template"),
                        "must be an absolute URL naming {z}, {x} and {y}".to_owned(),
                    )
                })?;
                FIRST_TEMPLATE_ASSET - i64::try_from(templates.len()).unwrap_or(0)
            }
            (Some(_), Some(_)) => {
                return Err((
                    at("template"),
                    "a layer has an `asset` or a `template`, not both".into(),
                ))
            }
            (None, None) => {
                return Err((
                    at("asset"),
                    "missing: give an `asset` or a `template`".into(),
                ))
            }
        };
        let key = text("key")?.unwrap_or_else(|| format!("asset{asset}"));
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err((
                at("key"),
                "lower-case letters, digits and dashes only".into(),
            ));
        }
        if out.iter().any(|l| l.key == key) {
            return Err((at("key"), "used by an earlier layer".into()));
        }
        let name = text("name")?.unwrap_or_else(|| key.clone());
        let attribution = text("attribution")?.unwrap_or_default();
        let Some(template) = template else {
            out.push(ImageryChoice::new(&key, &name, asset, &attribution));
            continue;
        };
        let geographic = match text("scheme")?.as_deref() {
            None | Some("webmercator") => false,
            Some("geographic") => true,
            Some(_) => return Err((at("scheme"), "is \"webmercator\" or \"geographic\"".into())),
        };
        let max_level = match layer.get("max_level") {
            None => None,
            Some(Value::Integer(n)) if (0..=30).contains(n) => Some(*n as u32),
            Some(_) => return Err((at("max_level"), "must be a level from 0 to 30".into())),
        };
        let token_header = text("token_header")?;
        if let Some(header) = &token_header {
            let fine = header
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !fine {
                return Err((
                    at("token_header"),
                    "must be a header name: letters, digits and dashes".into(),
                ));
            }
        }
        let token = layer
            .get("token")
            .map(|value| read_token(&at("token"), value))
            .transpose()?;
        let issuer = read_issuer(&at, &text, token.is_some())?;
        let token_header = match (token.is_some() || issuer.is_some(), token_header) {
            (_, Some(header)) => Some(header),
            (true, None) => Some(tuile_core::fetch::Credential::AUTHORIZATION.to_owned()),
            (false, None) => None,
        };
        if let Some(token) = token {
            tokens.push((key.clone(), token));
        }
        out.push(ImageryChoice::from_template(
            &key,
            &name,
            asset,
            &attribution,
            &template,
        ));
        templates.push(TemplateLayer {
            key: key.clone(),
            asset,
            template,
            geographic,
            max_level,
            token_header,
            issuer,
        });
    }
    // A substitute is another layer of the list, by its key; the rule's
    // numbers are checked as the session will check them.
    for (i, of, gap, hysteresis, at) in substitutes {
        let Some(sub) = out.iter().find(|l| l.key == of).cloned() else {
            return Err((at, format!("names no layer of this list: {of:?}")));
        };
        if sub.key == out[i].key {
            return Err((at, "a layer cannot stand in for itself".into()));
        }
        let choice = crate::embed::SubstituteChoice {
            asset: sub.asset,
            cache: sub.cache.clone(),
            gap,
            hysteresis,
        };
        let layer = out[i].clone().with_substitute(choice);
        layer
            .fallbacks()
            .validate()
            .map_err(|e| (at.clone(), e.to_string()))?;
        out[i] = layer;
    }
    Ok(Imagery {
        layers: out,
        templates,
        tokens,
    })
}

fn read_telemetry(table: &Table) -> Result<(TelemetrySettings, Option<Token>), Problem> {
    let mut out = TelemetrySettings::default();
    let mut token = None;
    for (key, value) in table {
        let at = format!("telemetry.{key}");
        if !TELEMETRY_KEYS.contains(&key.as_str()) {
            return Err((
                at,
                format!("unknown key; the keys are {}", TELEMETRY_KEYS.join(", ")),
            ));
        }
        let text = || match value {
            Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
            _ => Err((
                at.clone(),
                "must be a non-empty string in quotes".to_owned(),
            )),
        };
        let whole = |low: i64, high: i64| match value {
            Value::Integer(n) if (low..=high).contains(n) => Ok(*n),
            _ => Err((
                at.clone(),
                format!("must be a whole number from {low} to {high}"),
            )),
        };
        match key.as_str() {
            "endpoint" => {
                out.endpoint = Some(
                    tuile_telemetry::config::endpoint_url(&text()?)
                        .map_err(|why| (at.clone(), why))?,
                );
            }
            "profiling_endpoint" => {
                out.profiling_endpoint = Some(
                    tuile_telemetry::config::endpoint_url(&text()?)
                        .map_err(|why| (at.clone(), why))?,
                );
            }
            "token" => {
                let given = Token::new(&text()?);
                if given
                    .expose()
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err((
                        at,
                        "holds spaces or line breaks: copy the token again".into(),
                    ));
                }
                if looks_like_placeholder(given.expose()) {
                    return Err((
                        at,
                        "is still the example's placeholder: put your own token there".into(),
                    ));
                }
                token = Some(given);
            }
            "interval" => out.interval = Some(whole(1, 3600)? as u64),
            "environment" => out.environment = Some(text()?),
            "profiling" => {
                out.profiling = Some(
                    tuile_telemetry::config::switch(&text()?).map_err(|why| (at.clone(), why))?,
                );
            }
            "profiling_hz" => {
                out.profiling_hz = Some(whole(
                    1,
                    i64::from(tuile_telemetry::config::HIGHEST_PROFILING_HZ),
                )? as u32);
            }
            _ => unreachable!("checked above"),
        }
    }
    Ok((out, token))
}

fn read_snapshot(table: &Table) -> Result<crate::photo::SnapshotSettings, Problem> {
    use crate::photo::{Format, Layers};
    let mut out = crate::photo::SnapshotSettings::default();
    let mut png = false;
    let mut quality = None;
    for (key, value) in table {
        let at = format!("snapshot.{key}");
        if !SNAPSHOT_KEYS.contains(&key.as_str()) {
            return Err((
                at,
                format!("unknown key; the keys are {}", SNAPSHOT_KEYS.join(", ")),
            ));
        }
        match (key.as_str(), value) {
            ("folder", Value::String(folder)) if !folder.trim().is_empty() => {
                out.folder = Some(folder.trim().to_owned());
            }
            ("format", Value::String(format)) => match format.trim() {
                "jpeg" | "jpg" => png = false,
                "png" => png = true,
                _ => return Err((at, "must be \"jpeg\" or \"png\"".into())),
            },
            ("quality", Value::Integer(q)) if (1..=100).contains(q) => quality = Some(*q as u8),
            ("layers", Value::Array(names)) => {
                let names: Option<Vec<&str>> = names.iter().map(Value::as_str).collect();
                let names = names
                    .ok_or_else(|| (at.clone(), "must be a list of names in quotes".into()))?;
                out.layers = Some(Layers::from_names(names).map_err(|why| (at.clone(), why))?);
            }
            ("folder", _) => return Err((at, "must be a non-empty path in quotes".into())),
            ("format", _) => return Err((at, "must be \"jpeg\" or \"png\"".into())),
            ("quality", _) => return Err((at, "must be a whole number from 1 to 100".into())),
            _ => {
                return Err((
                    at,
                    format!("must be a list of {}", Layers::NAMES.join(", ")),
                ))
            }
        }
    }
    if png && quality.is_some() {
        return Err((
            "snapshot.quality".into(),
            "is a JPEG's: a PNG is lossless".into(),
        ));
    }
    if table.contains_key("format") || quality.is_some() {
        out.format = Some(if png {
            Format::Png
        } else {
            Format::Jpeg {
                quality: quality.unwrap_or(crate::photo::DEFAULT_QUALITY),
            }
        });
    }
    Ok(out)
}

fn read_start(start: &Table) -> Result<Vec<(String, f64)>, Problem> {
    let mut out = Vec::new();
    for (key, value) in start {
        let at = format!("start.{key}");
        if !START_KEYS.contains(&key.as_str()) {
            return Err((
                at,
                format!("unknown key; the keys are {}", START_KEYS.join(", ")),
            ));
        }
        let number = match value {
            Value::Integer(n) => *n as f64,
            Value::Float(x) => *x,
            _ => return Err((at, "must be a number".into())),
        };
        let checked = crate::start::checked(key, number).map_err(|why| (at.clone(), why))?;
        out.push((key.clone(), checked));
    }
    Ok(out)
}

fn asset(value: &Value) -> Result<i64, String> {
    match value {
        Value::Integer(n) if *n > 0 => Ok(*n),
        Value::Integer(_) => Err("must be a positive asset number".into()),
        _ => Err("must be a number, without quotes".into()),
    }
}

/// An API base: `https`, ending in `/` so the service's paths are read under
/// it. `http` only for this machine's own loopback.
fn api_base(text: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(text.trim()).map_err(|_| "is not a URL".to_owned())?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err("must be an https:// URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err("must not carry a user, a password or a query".into());
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

/// The example file's token, or one like it: refused, so a tester who
/// forgot to paste theirs is told so and not sent to a 401.
fn looks_like_placeholder(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("your") && lower.contains("token") || lower.starts_with("paste")
}

/// The settings, as the TOML the application keeps — no token, ever.
pub(crate) fn settings_text(section: &str, settings: &Settings) -> String {
    let mut root = Table::new();
    let mut source = Table::new();
    if let Some(api) = &settings.api {
        source.insert("api".into(), Value::String(api.to_string()));
    }
    if let Some(terrain) = settings.terrain {
        source.insert("terrain".into(), Value::Integer(terrain));
    }
    if !settings.imagery.is_empty() {
        let layers = settings
            .imagery
            .iter()
            .map(|l| {
                let mut t = Table::new();
                t.insert("key".into(), Value::String(l.key.clone()));
                t.insert("name".into(), Value::String(l.name.clone()));
                match settings.templates.iter().find(|x| x.asset == l.asset) {
                    // Never its token: its header says one is kept.
                    Some(x) => {
                        t.insert("template".into(), Value::String(x.template.clone()));
                        if x.geographic {
                            t.insert("scheme".into(), Value::String("geographic".into()));
                        }
                        if let Some(level) = x.max_level {
                            t.insert("max_level".into(), Value::Integer(i64::from(level)));
                        }
                        if let Some(header) = &x.token_header {
                            t.insert("token_header".into(), Value::String(header.clone()));
                        }
                        if let Some(issuer) = &x.issuer {
                            t.insert("token_url".into(), Value::String(issuer.url.clone()));
                            let (key, name) = match &issuer.answer {
                                IssuedAs::Cookie(name) => ("token_cookie", name),
                                IssuedAs::Field(name) => ("token_field", name),
                            };
                            t.insert(key.into(), Value::String(name.clone()));
                        }
                    }
                    None => {
                        t.insert("asset".into(), Value::Integer(l.asset));
                    }
                }
                if !l.attribution.is_empty() {
                    t.insert("attribution".into(), Value::String(l.attribution.clone()));
                }
                let named = l.substitute.as_ref().and_then(|s| {
                    settings
                        .imagery
                        .iter()
                        .find(|o| o.cache == s.cache)
                        .map(|o| (o.key.clone(), s))
                });
                if let Some((key, s)) = named {
                    t.insert("substitute".into(), Value::String(key));
                    t.insert("substitute_gap".into(), Value::Integer(i64::from(s.gap)));
                    t.insert(
                        "substitute_hysteresis".into(),
                        Value::Integer(i64::from(s.hysteresis)),
                    );
                }
                Value::Table(t)
            })
            .collect();
        source.insert("imagery".into(), Value::Array(layers));
    }
    if !source.is_empty() {
        root.insert(section.into(), Value::Table(source));
    }
    if !settings.start.is_empty() {
        let start = settings
            .start
            .iter()
            .map(|(k, v)| (k.clone(), Value::Float(*v)))
            .collect();
        root.insert("start".into(), Value::Table(start));
    }
    if let Some(t) = &settings.telemetry {
        let mut table = Table::new();
        let mut text = |key: &str, value: &Option<String>| {
            if let Some(v) = value {
                table.insert(key.into(), Value::String(v.clone()));
            }
        };
        text("endpoint", &t.endpoint);
        text("environment", &t.environment);
        text("profiling_endpoint", &t.profiling_endpoint);
        text(
            "profiling",
            &t.profiling
                .map(|on| if on { "on" } else { "off" }.to_owned()),
        );
        if let Some(i) = t.interval {
            table.insert("interval".into(), Value::Integer(i as i64));
        }
        if let Some(hz) = t.profiling_hz {
            table.insert("profiling_hz".into(), Value::Integer(i64::from(hz)));
        }
        root.insert("telemetry".into(), Value::Table(table));
    }
    if let Some(snapshot) = &settings.snapshot {
        let mut table = Table::new();
        if let Some(folder) = &snapshot.folder {
            table.insert("folder".into(), Value::String(folder.clone()));
        }
        match snapshot.format {
            Some(crate::photo::Format::Png) => {
                table.insert("format".into(), Value::String("png".into()));
            }
            Some(crate::photo::Format::Jpeg { quality }) => {
                table.insert("format".into(), Value::String("jpeg".into()));
                table.insert("quality".into(), Value::Integer(i64::from(quality)));
            }
            None => {}
        }
        if let Some(layers) = snapshot.layers {
            table.insert(
                "layers".into(),
                Value::Array(
                    layers
                        .names()
                        .into_iter()
                        .map(|n| Value::String(n.into()))
                        .collect(),
                ),
            );
        }
        root.insert("snapshot".into(), Value::Table(table));
    }
    toml::to_string(&root).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[snapshot]`: read, checked key by key, and kept as it was said.
    #[test]
    fn the_snapshot_section_is_read_and_kept() {
        use crate::photo::{Format, Layers};
        let text = "[snapshot]\nfolder = \"~/Desktop/globe\"\nquality = 90\nlayers = [\"hud\"]\n";
        let file = parse("a.tuile", text, "ion").expect("reads");
        let snapshot = file.settings.snapshot.clone().expect("a snapshot section");
        assert_eq!(snapshot.folder.as_deref(), Some("~/Desktop/globe"));
        assert_eq!(snapshot.format, Some(Format::Jpeg { quality: 90 }));
        assert_eq!(
            snapshot.layers,
            Some(Layers {
                hud: true,
                ..Layers::default()
            })
        );
        let kept = settings_text("ion", &file.settings);
        assert_eq!(
            parse("kept", &kept, "ion").expect("reads back").settings,
            file.settings
        );
        let png = parse("a.tuile", "[snapshot]\nformat = \"png\"\n", "ion").expect("reads");
        assert_eq!(
            png.settings.snapshot.and_then(|s| s.format),
            Some(Format::Png)
        );
        for (bad, key) in [
            ("[snapshot]\nformat = \"gif\"\n", "snapshot.format"),
            ("[snapshot]\nquality = 0\n", "snapshot.quality"),
            (
                "[snapshot]\nformat = \"png\"\nquality = 90\n",
                "snapshot.quality",
            ),
            ("[snapshot]\nlayers = [\"menu\"]\n", "snapshot.layers"),
            ("[snapshot]\nfolders = \"x\"\n", "snapshot.folders"),
        ] {
            assert_eq!(
                parse("a.tuile", bad, "ion").expect_err(bad).key,
                key,
                "{bad}"
            );
        }
    }

    /// A token shaped like a real one and belonging to no one.
    const FAKE: &str = "fake.0123456789abcdef.not-a-real-token";

    fn read(text: &str) -> Result<ConfigFile, ConfigError> {
        parse("config.tuile", text, "ion")
    }

    #[test]
    fn a_whole_file_is_read() {
        let file = read(&format!(
            r#"
            [ion]
            token = "{FAKE}"
            api = "https://api.example.test/v1"
            terrain = 1

            [[ion.imagery]]
            key = "aerial"
            name = "Aerial"
            asset = 2
            attribution = "© someone"

            [[ion.imagery]]
            asset = 3954

            [start]
            lon = 6.86
            lat = 45.83
            altitude = 6000
            "#
        ))
        .expect("a good file");
        assert_eq!(file.token.as_ref().map(Token::expose), Some(FAKE));
        let s = &file.settings;
        assert_eq!(
            s.api.as_ref().map(url::Url::as_str),
            Some("https://api.example.test/v1/")
        );
        assert_eq!(s.terrain, Some(1));
        assert_eq!(s.imagery.len(), 2);
        assert_eq!(s.imagery[0].asset, 2);
        assert_eq!(s.imagery[1].key, "asset3954");
        assert_eq!(s.start.len(), 3);
        assert_eq!(s.headline().as_deref(), Some("Aerial (asset 2)"));
    }

    /// A layer names another as its substitute, by key: its asset and its
    /// namespace, and the rule's numbers; written back as read. A layer
    /// without one has none; a missing layer and a bad gap are refused.
    #[test]
    fn a_layer_names_its_substitute() {
        let text = format!(
            r#"
            [ion]
            token = "{FAKE}"

            [[ion.imagery]]
            key = "aerial"
            asset = 2
            substitute = "aerial-coarse"
            substitute_gap = 5

            [[ion.imagery]]
            key = "aerial-coarse"
            asset = 7
            "#
        );
        let s = read(&text).expect("a file").settings;
        let sub = s.imagery[0].substitute.clone().expect("a substitute");
        assert_eq!((sub.asset, sub.gap, sub.hysteresis), (7, 5, 1));
        assert_eq!(sub.cache, s.imagery[1].cache);
        assert_eq!(
            s.imagery[0].fallbacks().to_query(),
            "layer=0&gap=5&hysteresis=1"
        );
        assert!(s.imagery[1].substitute.is_none());
        assert!(s.imagery[1].fallbacks().is_none());
        let again = read(&format!(
            "[ion]\ntoken = \"{FAKE}\"\n{}",
            settings_text("ion", &s).replace("[ion]\n", "")
        ))
        .expect("written back")
        .settings;
        assert_eq!(again.imagery, s.imagery, "round trip");
        let missing = text.replace("substitute = \"aerial-coarse\"", "substitute = \"none\"");
        assert!(read(&missing).is_err());
        let bad = text.replace("substitute_gap = 5", "substitute_gap = 0");
        assert!(read(&bad).is_err());
    }

    /// A tile token shaped like a real one and belonging to no one.
    const TILES: &str = "tiles.0123456789abcdef.not-a-real-token";

    /// A file whose catalog layer is drawn far from a template layer.
    fn with_a_template_substitute() -> String {
        format!(
            r#"
            [ion]
            token = "{FAKE}"

            [[ion.imagery]]
            key = "aerial"
            name = "Aerial"
            asset = 2
            substitute = "aerial-coarse"
            substitute_gap = 4

            [[ion.imagery]]
            key = "aerial-coarse"
            name = "Aerial, coarse levels"
            template = "https://tiles.example/aerial/{{z}}/{{x}}/{{y}}.webp"
            max_level = 18
            attribution = "© someone"
            token = "{TILES}"
            token_header = "X-Tile-Token"
            "#
        )
    }

    /// A layer named by its URL template: its own number and namespace, its
    /// grid, and its token kept apart from the settings, under the header it
    /// named. A catalog layer takes it as its substitute like any layer.
    #[test]
    fn a_template_layer_is_read_and_stands_in_for_a_catalog_layer() {
        let file = read(&with_a_template_substitute()).expect("a file");
        let s = &file.settings;
        assert_eq!(s.imagery.len(), 2);
        let coarse = &s.imagery[1];
        assert_eq!(coarse.asset, FIRST_TEMPLATE_ASSET);
        assert!(
            coarse.cache.starts_with("imagery-template-"),
            "{}",
            coarse.cache
        );
        assert_eq!(s.templates.len(), 1);
        let t = &s.templates[0];
        assert_eq!(t.key, "aerial-coarse");
        assert_eq!(t.asset, coarse.asset);
        assert_eq!(t.template, "https://tiles.example/aerial/{z}/{x}/{y}.webp");
        assert!(!t.geographic);
        assert_eq!(t.tiling().maximum_level, 18);
        assert_eq!(t.token_header.as_deref(), Some("X-Tile-Token"));
        let sub = s.imagery[0].substitute.clone().expect("a substitute");
        assert_eq!(
            (sub.asset, sub.cache.as_str()),
            (coarse.asset, coarse.cache.as_str())
        );
        assert_eq!((sub.gap, sub.hysteresis), (4, 1));
        assert_eq!(file.layer_tokens.len(), 1);
        assert_eq!(file.layer_tokens[0].0, "aerial-coarse");
        assert_eq!(file.layer_tokens[0].1.expose(), TILES);
        assert_eq!(file.token.as_ref().map(Token::expose), Some(FAKE));
        assert!(!format!("{file:?}").contains(TILES), "the token printed");
        assert_eq!(
            layer_token_variable("aerial-coarse"),
            "TUILE_IMAGERY_TOKEN_AERIAL_COARSE"
        );
    }

    /// The settings keep a template layer whole but its token, and read
    /// back to the same layers; a token without a header goes as a bearer
    /// token.
    #[test]
    fn a_template_layer_is_kept_without_its_token() {
        let read_back = |text: &str| {
            let s = read(text).expect("a file").settings;
            let kept = settings_text("ion", &s);
            assert!(!kept.contains(TILES), "the settings hold the token");
            assert!(!kept.contains(FAKE), "the settings hold the token");
            (s, read(&kept).expect("the settings read back").settings)
        };
        let (s, again) = read_back(&with_a_template_substitute());
        assert_eq!(again, s);
        let bearer = with_a_template_substitute().replace("token_header = \"X-Tile-Token\"\n", "");
        let (s, again) = read_back(&bearer);
        assert_eq!(
            s.templates[0].token_header.as_deref(),
            Some("Authorization")
        );
        assert_eq!(again, s);
        let (s, again) = read_back(
            "[[ion.imagery]]\nkey = \"open\"\n\
             template = \"https://t.example/{z}/{x}/{y}.png\"\nscheme = \"geographic\"\n",
        );
        assert!(s.templates[0].geographic);
        assert!(s.templates[0].token_header.is_none());
        assert_eq!(again, s);
    }

    /// A template layer whose token is handed out by an issuer: no token
    /// kept, the issuer and how its answer carries the token, its header;
    /// written back as read. A token and an issuer together, an issuer
    /// without its answer, and an answer without an issuer are refused.
    #[test]
    fn a_template_layer_names_its_token_issuer() {
        let text = r#"
            [[ion.imagery]]
            key = "far"
            template = "https://tiles.example/{z}/{x}/{y}.webp"
            token_url = "https://tiles.example/"
            token_cookie = "Visit"
            token_header = "X-Tile-Token"
            "#;
        let file = read(text).expect("a file");
        assert!(file.layer_tokens.is_empty());
        let t = &file.settings.templates[0];
        assert_eq!(t.token_header.as_deref(), Some("X-Tile-Token"));
        assert_eq!(
            t.issuer,
            Some(TokenIssuer {
                url: "https://tiles.example/".into(),
                answer: IssuedAs::Cookie("Visit".into()),
            })
        );
        let kept = settings_text("ion", &file.settings);
        assert_eq!(read(&kept).expect("read back").settings, file.settings);
        let json = read(&text.replace("token_cookie = \"Visit\"", "token_field = \"token\""))
            .expect("a file");
        let t = &json.settings.templates[0];
        assert_eq!(
            t.issuer.as_ref().map(|i| &i.answer),
            Some(&IssuedAs::Field("token".into()))
        );
        let wrong = [
            (
                text.replace(
                    "token_cookie",
                    "token = \"abc.def\"\n            token_cookie",
                ),
                "ion.imagery[0].token_url",
            ),
            (
                text.replace("token_cookie = \"Visit\"", ""),
                "ion.imagery[0].token_url",
            ),
            (
                text.replace(
                    "token_cookie = \"Visit\"",
                    "token_cookie = \"Visit\"\n            token_field = \"t\"",
                ),
                "ion.imagery[0].token_url",
            ),
            (
                text.replace("token_url = \"https://tiles.example/\"", ""),
                "ion.imagery[0].token_cookie",
            ),
            (
                text.replace("https://tiles.example/\"", "http://tiles.example/\""),
                "ion.imagery[0].token_url",
            ),
        ];
        for (text, key) in wrong {
            assert_eq!(read(&text).expect_err(key).key, key);
        }
    }

    /// What is wrong with a template layer is said by its key, never its
    /// value.
    #[test]
    fn template_layer_errors_name_the_key() {
        let base = with_a_template_substitute();
        let wrong = [
            (base.replace("{y}", "{row}"), "ion.imagery[1].template"),
            (
                base.replace("max_level = 18", "max_level = 99"),
                "ion.imagery[1].max_level",
            ),
            (
                base.replace(
                    "token_header = \"X-Tile-Token\"",
                    "token_header = \"X Tile: Token\"",
                ),
                "ion.imagery[1].token_header",
            ),
            (
                base.replace(&format!("token = \"{TILES}\""), "token = \"with a space\""),
                "ion.imagery[1].token",
            ),
            (
                base.replace(
                    "asset = 2\n",
                    "asset = 2\n            token_header = \"X\"\n",
                ),
                "ion.imagery[0].token_header",
            ),
            (
                base.replace("max_level = 18", "max_level = 18\n            asset = 9"),
                "ion.imagery[1].template",
            ),
        ];
        for (text, key) in wrong {
            let e = read(&text).expect_err(key);
            assert_eq!(e.key, key, "{e}");
            assert!(!e.to_string().contains(TILES), "{e}");
            assert!(!e.to_string().contains("tiles.example"), "{e}");
        }
    }

    #[test]
    fn a_token_alone_is_a_file() {
        let file = read(&format!("[ion]\ntoken = \"{FAKE}\"\n")).expect("a token alone");
        assert_eq!(file.settings, Settings::default());
    }

    /// Every way a file can be wrong names the file and the key — and no
    /// message ever carries the token, even when the fault is on its line.
    #[test]
    fn errors_name_the_key_and_never_the_value() {
        let cases = [
            (format!("[ion]\ntoken = {FAKE}\n"), "line 2"),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\nterain = 1\n"),
                "ion.terain",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\nterrain = \"1\"\n"),
                "ion.terrain",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\napi = \"http://example.test\"\n"),
                "ion.api",
            ),
            (format!("[ion]\ntoken = \"{FAKE} {FAKE}\"\n"), "ion.token"),
            (
                "[ion]\ntoken = \"YOUR-ION-TOKEN\"\n".to_owned(),
                "ion.token",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\n[[ion.imagery]]\nname = \"x\"\n"),
                "ion.imagery[0].asset",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\n[start]\nlat = 91\n"),
                "start.lat",
            ),
            (format!("[ion]\ntoken = \"{FAKE}\"\n[other]\n"), "other"),
            ("# nothing\n".to_owned(), "ion"),
        ];
        for (text, key) in cases {
            let error = read(&text).expect_err(&text);
            assert_eq!(error.key, key, "{text}");
            assert_eq!(error.file, "config.tuile");
            let said = error.to_string();
            assert!(!said.contains(FAKE), "the token leaked into {said:?}");
            assert!(
                !said.contains("fake."),
                "part of the token leaked into {said:?}"
            );
        }
    }

    #[test]
    fn the_token_is_never_printed() {
        let file = read(&format!("[ion]\ntoken = \"{FAKE}\"\n")).expect("a token");
        assert!(!format!("{file:?}").contains(FAKE));
    }

    #[test]
    fn the_settings_file_holds_no_token_and_reads_back() {
        let file = read(&format!(
            "[ion]\ntoken = \"{FAKE}\"\nterrain = 1\n[[ion.imagery]]\nkey = \"aerial\"\nasset = 2\n[start]\nlon = 3\n"
        ))
        .expect("a good file");
        let kept = settings_text("ion", &file.settings);
        assert!(!kept.contains(FAKE));
        assert!(!kept.contains("token"));
        let again = read(&kept).expect("the settings file is itself a configuration");
        assert_eq!(again.token, None);
        assert_eq!(again.settings, file.settings);
    }

    #[test]
    fn a_huge_file_is_refused_unread() {
        let error = read(&"#".repeat(LONGEST_FILE + 1)).expect_err("too large");
        assert!(error.why.contains("KiB"));
    }

    /// `[telemetry]`: read and checked, its token kept apart and never
    /// written back, its errors naming the key and never the value.
    #[test]
    fn the_telemetry_section_is_read() {
        let file = read(&format!(
            r#"
            [telemetry]
            endpoint = "https://collector.example/otlp/"
            token = "{FAKE}"
            interval = 30
            environment = "beta"
            profiling_endpoint = "https://collector.example/pyroscope"
            profiling = "off"
            profiling_hz = 49
            "#
        ))
        .expect("a telemetry file");
        assert_eq!(
            file.token, None,
            "the section's token is not the collector's"
        );
        assert_eq!(file.telemetry_token, Some(Token::new(FAKE)));
        let t = file.settings.telemetry.clone().expect("telemetry");
        assert_eq!(
            t.endpoint.as_deref(),
            Some("https://collector.example/otlp")
        );
        assert_eq!(t.interval, Some(30));
        assert_eq!(t.environment.as_deref(), Some("beta"));
        assert_eq!(
            t.profiling_endpoint.as_deref(),
            Some("https://collector.example/pyroscope")
        );
        assert_eq!(t.profiling, Some(false));
        assert_eq!(t.profiling_hz, Some(49));
        let kept = settings_text("ion", &file.settings);
        assert!(!kept.contains(FAKE), "the token was written back: {kept}");
        let again = read(&kept).expect("the kept settings read back");
        assert_eq!(again.settings, file.settings);
    }

    #[test]
    fn telemetry_errors_name_the_key() {
        for (text, key) in [
            (
                "[telemetry]\nendpoint = \"https://collector.example/otlp?token=s3cret\"",
                "telemetry.endpoint",
            ),
            (
                "[telemetry]\nendpoint = \"https://me:s3cret@collector.example\"",
                "telemetry.endpoint",
            ),
            ("[telemetry]\ninterval = 0", "telemetry.interval"),
            ("[telemetry]\nprofiling = \"s3cret\"", "telemetry.profiling"),
            (
                "[telemetry]\nprofiling_hz = 100000",
                "telemetry.profiling_hz",
            ),
            ("[telemetry]\nheaders = \"s3cret\"", "telemetry.headers"),
            ("[telemetry]\ntoken = \"your-token\"", "telemetry.token"),
        ] {
            let e = read(text).expect_err(text);
            assert_eq!(e.key, key, "{e}");
            assert!(!e.to_string().contains("s3cret"), "the value leaked: {e}");
        }
    }
}
