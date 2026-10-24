// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The log, in the browser's console: the same `tracing` lines the desktop
//! prints, at the console level their own level asks for.
//!
//! The standard subscriber, with a writer that hands each finished line to
//! `console.error`, `console.warn` or `console.log`. No timestamps: the
//! console has its own, and asking the clock from a formatter is one more
//! thing that is not there in a browser.

use std::io::Write;
use std::sync::Once;

use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

/// Installs the console subscriber and the panic hook, once per page or
/// worker; later calls do nothing.
pub(crate) fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Without it a panic reaches the console as a bare
        // `RuntimeError: unreachable`, with no message and no line.
        console_error_panic_hook::set_once();
        // The engine's own crates say what they do; their dependencies only
        // what went wrong. Tile-by-tile streaming stays quiet: a console is
        // read by people, not grepped.
        let filter = Targets::new()
            .with_default(Level::WARN)
            .with_target("tuile_viewer", Level::INFO)
            .with_target("tuile_web", Level::INFO)
            .with_target("tuile_core", Level::INFO)
            .with_target("tuile_planetary", Level::INFO)
            .with_target("tuile_wgpu", Level::INFO);
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(Console);
        // Another subscriber already installed — by a host that brought its
        // own — is the host's choice to keep.
        let _ = tracing_subscriber::registry()
            .with(layer.with_filter(filter))
            .try_init();
    });
}

/// Makes one [`Line`] per event, at the event's level.
struct Console;

impl<'a> MakeWriter<'a> for Console {
    type Writer = Line;

    fn make_writer(&'a self) -> Self::Writer {
        Line::new(Level::INFO)
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        Line::new(*meta.level())
    }
}

/// One event's text, written to the console when it is complete.
struct Line {
    level: Level,
    text: Vec<u8>,
}

impl Line {
    fn new(level: Level) -> Self {
        Self {
            level,
            text: Vec::new(),
        }
    }
}

impl Write for Line {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.text.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.text);
        let text = wasm_bindgen::JsValue::from_str(text.trim_end());
        match self.level {
            Level::ERROR => web_sys::console::error_1(&text),
            Level::WARN => web_sys::console::warn_1(&text),
            _ => web_sys::console::log_1(&text),
        }
    }
}
