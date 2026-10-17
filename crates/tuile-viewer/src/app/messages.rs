// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Messages for the person, held until there is a window to show them over.
//!
//! A phone's application object does not exist until the system's loop has
//! started, and asking for it earlier is not an error the platform reports
//! politely: the binding panics on the null it gets back. A session that fails
//! at once — no access token, nothing to wait for on the network — fails
//! before the loop has turned even once, so its message is kept here and
//! shown on the first turn that has a window. That is exactly the crash the
//! first device build had, and the simulator never showed it: there the token
//! came from the environment, the start-up went to the network, and the loop
//! was long running by the time anything could fail.

/// What is waiting to be said: a title and a text each.
#[derive(Debug, Default)]
pub(super) struct Messages {
    pending: Vec<(String, String)>,
}

impl Messages {
    pub(super) fn say(&mut self, title: String, text: String) {
        self.pending.push((title, text));
    }

    /// What may be shown now: everything, once there is a window; nothing
    /// before.
    pub(super) fn release(&mut self, window_up: bool) -> Vec<(String, String)> {
        if window_up {
            std::mem::take(&mut self.pending)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure that arrives before the window waits for it, in order, and
    /// is shown once.
    #[test]
    fn nothing_is_shown_before_there_is_a_window() {
        let mut m = Messages::default();
        m.say("Tuile cannot start yet".into(), "no token".into());
        m.say("second".into(), "text".into());
        assert!(m.release(false).is_empty());
        assert!(m.release(false).is_empty(), "held, not dropped");
        let shown = m.release(true);
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].1, "no token");
        assert!(m.release(true).is_empty(), "shown once");
    }
}
