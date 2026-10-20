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
//! - Every attempt is given its asker's [`attempt_timeout`], then aborted.
//!   The timer runs from the `fetch` call, so it counts the time the request
//!   waits for one of the browser's few connections per host: a tile load,
//!   whose drape asks for a mosaic of imagery tiles at once, is given as long
//!   as the budget gives the load ([`LOAD_ATTEMPT_TIMEOUT`]) — at 12 s, a
//!   view near the ground queued hundreds of requests behind six connections
//!   per host and timed most of them out, the drapes failing whole.
//! - A **tile load** is asked once ([`Asker::Load`]). Its retries are the
//!   budget's: a failed or timed-out load is retried with a backoff
//!   (`FailurePolicy::RetryBackoff`) while it is still wanted, and its time
//!   is bounded by the budget's `load_timeout`. Retrying it here as well held
//!   a load slot through pauses the budget never saw, for a tile the camera
//!   may already have left.
//! - A **session request** — resolving the sources before there is a budget
//!   ([`Asker::Session`]) — is tried again on a timeout, a network failure
//!   or a transient status ([`tuile_core::fetch::is_retryable_status`]),
//!   after a doubling pause, up to [`ATTEMPTS`] attempts in all.
//! - Anything else is the origin's answer, at once.

use std::time::Duration;

/// How long one attempt of a session request may take, headers and body
/// together.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(12);

/// How long one attempt of a tile load may take: the browser profile's load
/// timeout, so the transport never gives up on a load the budget still
/// waits for.
pub const LOAD_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one attempt of `asker` may take.
pub fn attempt_timeout(asker: Asker) -> Duration {
    match asker {
        Asker::Load => LOAD_ATTEMPT_TIMEOUT,
        Asker::Session => ATTEMPT_TIMEOUT,
    }
}

/// Attempts in all for a session request, the first included.
pub const ATTEMPTS: u32 = 3;

/// Who asks: what decides whether the transport asks again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asker {
    /// A tile load, whose retries are the budget's: asked once.
    Load,
    /// A request outside any budget: asked up to [`ATTEMPTS`] times.
    Session,
}

/// The pause before the second attempt; it doubles after that.
const FIRST_PAUSE: Duration = Duration::from_millis(400);

/// The longest pause between two attempts.
const LONGEST_PAUSE: Duration = Duration::from_secs(3);

/// How one attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The origin answered with this status.
    Answered(u16),
    /// No answer within the attempt's timeout: aborted.
    TimedOut,
    /// The browser could not reach the origin.
    Failed,
}

/// After `asker`'s attempt `attempt` (the first is 1) ended as `ended`: the
/// pause before asking again, or `None` when this was the last word.
pub fn again(asker: Asker, attempt: u32, ended: Ended) -> Option<Duration> {
    if asker == Asker::Load {
        return None;
    }
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

/// The longest `asker`'s request can take, every attempt and pause counted:
/// what a stalled origin can hold it for, at most.
pub fn longest(asker: Asker) -> Duration {
    let mut total = Duration::ZERO;
    let mut attempt = 1;
    loop {
        total += attempt_timeout(asker);
        match again(asker, attempt, Ended::TimedOut) {
            Some(pause) => total += pause,
            None => return total,
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Asker::{Load, Session};

    #[test]
    fn a_silent_origin_is_asked_three_times_with_growing_pauses_then_given_up() {
        assert_eq!(
            again(Session, 1, Ended::TimedOut),
            Some(Duration::from_millis(400))
        );
        assert_eq!(
            again(Session, 2, Ended::TimedOut),
            Some(Duration::from_millis(800))
        );
        assert_eq!(again(Session, 3, Ended::TimedOut), None);
        assert_eq!(
            again(Session, 1, Ended::Failed),
            Some(Duration::from_millis(400))
        );
    }

    #[test]
    fn a_final_answer_is_not_asked_again_and_a_transient_one_is() {
        for status in [200, 304, 401, 403, 404] {
            assert_eq!(again(Session, 1, Ended::Answered(status)), None, "{status}");
        }
        for status in [429, 500, 503, 504] {
            assert!(
                again(Session, 1, Ended::Answered(status)).is_some(),
                "{status}"
            );
        }
    }

    /// **A tile load is asked once**: its retries are the budget's, which
    /// knows whether the tile is still wanted.
    #[test]
    fn a_tile_load_is_asked_once() {
        for ended in [
            Ended::TimedOut,
            Ended::Failed,
            Ended::Answered(503),
            Ended::Answered(200),
        ] {
            assert_eq!(again(Load, 1, ended), None, "{ended:?}");
        }
        assert_eq!(longest(Load), LOAD_ATTEMPT_TIMEOUT);
    }

    /// A request that never answers holds what waits on it for less than a
    /// minute, never for ever.
    #[test]
    fn nothing_waits_for_ever() {
        let longest = longest(Session);
        assert!(longest < Duration::from_secs(60), "{longest:?}");
        assert!(longest >= ATTEMPT_TIMEOUT * ATTEMPTS);
    }
}
