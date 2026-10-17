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

    /// Offers each waiting message to `show`, in order, once there is a
    /// window; a message `show` could not present stays for the next turn,
    /// and so does everything after it.
    pub(super) fn offer(&mut self, window_up: bool, mut show: impl FnMut(&str, &str) -> bool) {
        if !window_up {
            return;
        }
        while let Some((title, text)) = self.pending.first() {
            if !show(title, text) {
                return;
            }
            self.pending.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure that arrives before the window waits for it; one the
    /// platform could not present yet waits again; each is shown once, in
    /// order.
    #[test]
    fn a_message_waits_for_a_window_that_can_show_it() {
        let mut m = Messages::default();
        m.say("Tuile cannot start yet".into(), "no token".into());
        m.say("second".into(), "text".into());
        let mut shown = Vec::new();
        m.offer(false, |_, t| {
            shown.push(t.to_owned());
            true
        });
        assert!(shown.is_empty(), "shown before there was a window");
        // A window, not yet on screen: the presentation does not take.
        m.offer(true, |_, _| false);
        m.offer(true, |_, t| {
            shown.push(t.to_owned());
            true
        });
        assert_eq!(shown, ["no token", "text"]);
        m.offer(true, |_, t| {
            shown.push(t.to_owned());
            true
        });
        assert_eq!(shown.len(), 2, "shown once");
    }
}
