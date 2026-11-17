// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a browser's Performance API entry says of a tile request, read the
//! way a native transport labels its own (`tuile-native-fetchers`). On every
//! target, so that its tests run on the host.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// A tile's kind from its address's path — as the native transport reads it
/// — or `None` for what is not a tile (a layer's description, this push).
pub(crate) fn kind(url: &str) -> Option<&'static str> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if path.ends_with(".terrain") {
        Some("mesh")
    } else if [".jpeg", ".jpg", ".png", ".webp", ".avif", ".ktx2"]
        .iter()
        .any(|ext| path.ends_with(ext))
    {
        Some("texture")
    } else {
        None
    }
}

/// Where the browser says a response came from: `cache` (no bytes on the
/// wire, a body), `upstream` (bytes on the wire) or `unknown` (a
/// cross-origin response that hides its sizes).
pub(crate) fn source(transfer_size: f64, encoded_body_size: f64) -> &'static str {
    if transfer_size > 0.0 {
        "upstream"
    } else if encoded_body_size > 0.0 {
        "cache"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_sources() {
        assert_eq!(kind("https://t.example/1/2/3.terrain?v=1.2"), Some("mesh"));
        assert_eq!(kind("https://t.example/a/b.JPEG"), Some("texture"));
        assert_eq!(kind("https://t.example/layer.json"), None);
        assert_eq!(kind("https://collector.example/otlp/v1/metrics"), None);
        assert_eq!(source(0.0, 1000.0), "cache");
        assert_eq!(source(1300.0, 1000.0), "upstream");
        assert_eq!(source(0.0, 0.0), "unknown");
    }
}
