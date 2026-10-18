// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bringing the session up without holding the window's thread.
//!
//! Everything between "the application started" and "the geometry server is
//! running" is network: the host's credentials and service, the terrain's
//! layer description, the imagery's metadata, the tile store opening on disk.
//! Seconds on a good connection, and unbounded on a bad one.
//!
//! A desktop can afford to wait for it before opening the window. A phone
//! cannot: its system ends an application whose main thread does not return
//! from launch promptly, or stops answering for long, and a person looking at
//! a frozen launch screen draws the same conclusion. Nor can a browser, whose
//! page freezes for as long as its script does. So each shell runs the
//! sequence where it runs its network work — the desktop's async runtime, the
//! browser's worker — and the window collects the outcome with
//! [`Launch::poll`], which never waits, on whichever turn of its loop it
//! arrives, drawing the globe's shell and the loading indicator meanwhile.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use web_time::Instant;

use crate::shell::Started;

/// What a start-up ends with: the session, or a sentence for the person.
pub(crate) type Outcome = Result<Started, String>;

/// A launch under way, or over.
pub(crate) struct Launch {
    outcome: Receiver<Outcome>,
}

impl Launch {
    /// A launch whose outcome will be sent on the returned sender, from
    /// wherever the shell brings the session up.
    pub(crate) fn pending() -> (Sender<Outcome>, Self) {
        let (tx, rx) = channel();
        (tx, Self { outcome: rx })
    }

    /// A launch whose outcome is already known: a desktop's, which waited
    /// for it.
    #[cfg(any(not(target_arch = "wasm32"), test))]
    pub(crate) fn finished(outcome: Outcome) -> Self {
        let (tx, launch) = Self::pending();
        let _ = tx.send(outcome);
        launch
    }

    /// The outcome, if it has arrived. Never waits.
    pub(crate) fn poll(&mut self) -> Option<Outcome> {
        match self.outcome.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "the session's start-up stopped without an answer".into(),
            )),
        }
    }

    /// Waits for the outcome. For a desktop, before its window opens — a
    /// browser has no thread it may block.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn wait(self) -> Outcome {
        self.outcome
            .recv()
            .unwrap_or_else(|_| Err("the session's start-up stopped without an answer".into()))
    }
}

/// How long a start-up may take before it is called off and tried again: the
/// host's service, its layer descriptions and the store, end to end. A
/// network that answers nothing must not keep a spinner turning for ever.
pub(crate) const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Starting again after a start-up that failed — where there is no command
/// line to run again and no person who should have to: a phone, a page.
///
/// A failure is retried by itself after a wait that doubles each time, from
/// two seconds to a minute: a network that is down for a moment comes back
/// without anyone doing anything, and one that stays down costs a start-up a
/// minute. Coming back to the front retries at once — which is also how a
/// token typed in the system's settings is taken: the person left the
/// application to type it.
pub(crate) struct Retries {
    /// Starts the session again, the way the shell started it the first time.
    restart: Box<dyn FnMut() -> Launch>,
    failures: u32,
    next: Option<Instant>,
}

/// How long to wait after `failures` failures in a row.
pub(crate) fn backoff(failures: u32) -> std::time::Duration {
    let seconds = 2u64.saturating_pow(failures.max(1)).min(60);
    std::time::Duration::from_secs(seconds)
}

impl Retries {
    pub(crate) fn new(restart: impl FnMut() -> Launch + 'static) -> Self {
        Self {
            restart: Box::new(restart),
            failures: 0,
            next: None,
        }
    }

    /// Notes a failure; returns how long until the next attempt.
    pub(crate) fn failed(&mut self, now: Instant) -> std::time::Duration {
        self.failures += 1;
        let wait = backoff(self.failures);
        self.next = Some(now + wait);
        wait
    }

    /// Notes a start-up that came up.
    pub(crate) fn succeeded(&mut self) {
        self.failures = 0;
        self.next = None;
    }

    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    /// When the next attempt is due, if one is waiting.
    pub(crate) fn next(&self) -> Option<Instant> {
        self.next
    }

    /// The application is back in front: a waiting attempt goes now.
    pub(crate) fn foreground(&mut self, now: Instant) {
        if self.next.is_some() {
            self.next = Some(now);
        }
    }

    /// The next start-up, if one is due.
    pub(crate) fn due(&mut self, now: Instant) -> Option<Launch> {
        let next = self.next?;
        if now < next {
            return None;
        }
        self.next = None;
        Some((self.restart)())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The wait doubles from two seconds to a minute.
    #[test]
    fn retries_back_off_to_a_minute() {
        let waits: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    /// A failure schedules the next attempt; nothing goes before it is due;
    /// coming back to the front makes it due at once; success clears it.
    #[test]
    fn a_failed_launch_is_tried_again() {
        let started = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = std::rc::Rc::clone(&started);
        let mut retries = Retries::new(move || {
            counted.set(counted.get() + 1);
            Launch::finished(Err("there is no key".into()))
        });
        let t0 = Instant::now();
        assert!(retries.due(t0).is_none(), "nothing failed yet");
        assert_eq!(retries.failed(t0), Duration::from_secs(2));
        assert!(retries.due(t0 + Duration::from_secs(1)).is_none());
        retries.foreground(t0 + Duration::from_secs(1));
        let again = retries.due(t0 + Duration::from_secs(1));
        assert!(again.is_some(), "back in front did not retry");
        assert!(retries.due(t0 + Duration::from_secs(9)).is_none(), "twice");
        assert_eq!(started.get(), 1);
        assert_eq!(
            again
                .expect("a launch")
                .poll()
                .and_then(Result::err)
                .as_deref(),
            Some("there is no key")
        );
        assert_eq!(retries.failed(t0), Duration::from_secs(4));
        retries.succeeded();
        assert_eq!(retries.failures(), 0);
        assert!(retries.next().is_none());
    }

    /// A launch whose shell went away without answering says so, rather
    /// than leaving the indicator turning for ever.
    #[test]
    fn a_launch_abandoned_without_an_answer_says_so() {
        let (tx, mut launch) = Launch::pending();
        assert!(launch.poll().is_none());
        drop(tx);
        assert!(launch
            .poll()
            .and_then(Result::err)
            .is_some_and(|why| why.contains("without an answer")));
    }
}
