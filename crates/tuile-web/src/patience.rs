// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How long the browser's transport waits for an answer, and how often it asks
//! again.
//!
//! A `fetch` the browser never answers holds the tile load that awaits it,
//! and the load holds one of the server's few concurrent slots: a few dozen
//! such requests and the page stops refining for good. The desktop's client
//! bounds every request (`tuile-native-fetchers`); this is the same bound for
//! the browser, kept apart from the `fetch` calls so that it can be tested on
//! any host.
//!
//! - Every attempt is given [`ATTEMPT_TIMEOUT`], then aborted.
//! - A timeout, a network failure or a transient status
//!   ([`tuile_core::fetch::is_retryable_status`]) is tried again, after a
//!   doubling pause, up to [`ATTEMPTS`] attempts in all.
//! - Anything else is the origin's answer, at once.
//!
//! What then fails, fails the request: an imagery tile is left out of its
//! drape, whose coarser floor shows instead, and a terrain tile is built from
//! its parent. Ground that is soft rather than absent.

use std::time::Duration;

/// How long one attempt may take, headers and body together.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(12);

/// Attempts in all, the first included.
pub const ATTEMPTS: u32 = 3;

/// The pause before the second attempt; it doubles after that.
const FIRST_PAUSE: Duration = Duration::from_millis(400);

/// The longest pause between two attempts.
const LONGEST_PAUSE: Duration = Duration::from_secs(3);

/// How one attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The origin answered with this status.
    Answered(u16),
    /// No answer within [`ATTEMPT_TIMEOUT`]: aborted.
    TimedOut,
    /// The browser could not reach the origin.
    Failed,
}

/// After attempt `attempt` (the first is 1) ended as `ended`: the pause before
/// asking again, or `None` when this was the last word.
pub fn again(attempt: u32, ended: Ended) -> Option<Duration> {
    let transient = match ended {
        Ended::Answered(status) => tuile_core::fetch::is_retryable_status(status),
        Ended::TimedOut | Ended::Failed => true,
    };
    if !transient || attempt >= ATTEMPTS {
        return None;
    }
    let doubled = FIRST_PAUSE.saturating_mul(1 << (attempt - 1).min(16));
    Some(doubled.min(LONGEST_PAUSE))
}

/// The longest a request can take, every attempt and pause counted: what a
/// stalled origin can hold a tile load for, at most.
pub fn longest() -> Duration {
    let mut total = Duration::ZERO;
    for attempt in 1..=ATTEMPTS {
        total += ATTEMPT_TIMEOUT;
        total += again(attempt, Ended::TimedOut).unwrap_or_default();
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_origin_is_asked_three_times_with_growing_pauses_then_given_up() {
        assert_eq!(again(1, Ended::TimedOut), Some(Duration::from_millis(400)));
        assert_eq!(again(2, Ended::TimedOut), Some(Duration::from_millis(800)));
        assert_eq!(again(3, Ended::TimedOut), None);
        assert_eq!(again(1, Ended::Failed), Some(Duration::from_millis(400)));
    }

    #[test]
    fn a_final_answer_is_not_asked_again_and_a_transient_one_is() {
        for status in [200, 304, 401, 403, 404] {
            assert_eq!(again(1, Ended::Answered(status)), None, "{status}");
        }
        for status in [429, 500, 503, 504] {
            assert!(again(1, Ended::Answered(status)).is_some(), "{status}");
        }
    }

    /// A request that never answers holds its tile load for less than a
    /// minute, never for ever.
    #[test]
    fn nothing_waits_for_ever() {
        assert!(longest() < Duration::from_secs(60), "{:?}", longest());
        assert!(longest() >= ATTEMPT_TIMEOUT * ATTEMPTS);
    }
}
