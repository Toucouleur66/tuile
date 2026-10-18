// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Concurrency limits read from the environment.
//!
//! Each limit has a default in code and an `TUILE_*` override, read once per
//! process on first use: the machine sets them, not the request.

use std::sync::OnceLock;

/// A positive count from `raw`, or `default` when it is absent, unparsable
/// or zero — a limit of nothing in flight would stall the stream for ever.
pub(crate) fn parse_count(raw: Option<&str>, default: usize) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// The count `name` says, or `default`, read once into `cell`.
pub(crate) fn count(cell: &'static OnceLock<usize>, name: &str, default: usize) -> usize {
    *cell.get_or_init(|| parse_count(std::env::var(name).ok().as_deref(), default))
}

#[cfg(test)]
mod tests {
    use super::parse_count;

    #[test]
    fn a_count_falls_back_to_its_default_unless_set_to_a_positive_number() {
        assert_eq!(parse_count(None, 16), 16);
        assert_eq!(parse_count(Some(""), 16), 16);
        assert_eq!(parse_count(Some("lots"), 16), 16);
        assert_eq!(parse_count(Some("-3"), 16), 16);
        assert_eq!(parse_count(Some("0"), 16), 16);
        assert_eq!(parse_count(Some("4"), 16), 4);
        assert_eq!(parse_count(Some(" 64 "), 16), 64);
    }
}
