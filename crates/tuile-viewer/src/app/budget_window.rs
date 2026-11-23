// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget window: the graphics budget, per level and per imagery layer,
//! in a window of its own — an operating-system window on a desktop, a
//! browser window opened by the page, a texture of its own in a headless
//! run (`budget_screen`) — and in a panel over the view on a phone, which
//! has no second window. The same panel everywhere, laid out here and drawn
//! by the overlay as the credits bar is.
//!
//! It reads the budget's published signals (`tuile_core::metrics::budget`,
//! a [`BudgetSignals`]) and nothing else: the numbers the telemetry exports,
//! never a second computation of them. Two views:
//!
//! - **instant**: the last step — what is held, drawn, selected, under its
//!   need, in flight and queued, by level and class, and the layers' drapes;
//! - **cumulative**: the counters summed since the session started, or since
//!   the window was told to sum from now (`window=reset`) — loads started,
//!   completed and wasted, evictions and releases by level, the layers'
//!   requests by tier, completions and failures, bytes uploaded.
//!
//! The header has the totals against the targets and limits, the pressure,
//! the LOD scale, the specification in force, the time and work a decision
//! takes, and the ground the budget could not cover (lost and bare tiles).

use tuile_core::budget::{BudgetSignals, Class, LAYERS, LEVELS};
use web_time::Instant;

use crate::steer::WindowAsk;

/// How often the panel is laid out again while shown: the numbers change
/// every step, a person reads a few times a second.
const EVERY_MS: u128 = 250;

/// The classes the table has columns for, with their short names.
const CLASSES: [(Class, &str); 3] = [
    (Class::TerrainMesh, "T"),
    (Class::Drape, "D"),
    (Class::Tiles3dMesh, "3D"),
];

/// The window's state: shown, which view, from when the cumulative view
/// sums, and the panel as last laid out.
#[derive(Default)]
pub(super) struct BudgetWindow {
    shown: bool,
    cumulative: bool,
    /// The signals the cumulative view subtracts, and when they were taken;
    /// none: since the session started.
    from: Option<(BudgetSignals, Instant)>,
    /// A reset asked for, taken on the next signals.
    reset: bool,
    /// Drawn in a window of its own rather than over the view: see
    /// [`Self::own_window`].
    own_window: bool,
    layout: Option<tuile_ui::PanelLayout>,
    laid: Option<(Instant, (u32, u32), u64)>,
}

impl BudgetWindow {
    pub(super) fn ask(&mut self, ask: WindowAsk) {
        match ask {
            WindowAsk::Show(on) => self.shown = on,
            WindowAsk::Cumulative(on) => {
                self.shown = true;
                self.cumulative = on;
            }
            WindowAsk::Reset => {
                self.shown = true;
                self.cumulative = true;
                self.reset = true;
            }
        }
        // Laid out again at once.
        self.laid = None;
        if !self.shown {
            self.layout = None;
        }
    }

    pub(super) fn shown(&self) -> bool {
        self.shown
    }

    /// Drawn in a window of its own — whatever the shell's is — instead of
    /// over the view. Set once, by the shell, before anything is shown.
    pub(super) fn set_own_window(&mut self, own: bool) {
        self.own_window = own;
        self.laid = None;
    }

    /// Whether the panel wants a window of its own now: shown, on a shell
    /// that gives it one.
    pub(super) fn wants_own_window(&self) -> bool {
        self.shown && self.own_window
    }

    /// Its own window was closed by the person, or could not be opened:
    /// the window is hidden, as `window=off` hides it, and nothing else
    /// changes — the view, its session and the budget go on as they were.
    pub(super) fn closed(&mut self) {
        self.ask(WindowAsk::Show(false));
    }

    /// Laid out again on the next call, whenever that is.
    pub(super) fn again(&mut self) {
        self.laid = None;
    }

    /// The panel as laid out for its own window, if it has one and is
    /// shown.
    pub(super) fn own_layout(&self) -> Option<&tuile_ui::PanelLayout> {
        self.layout
            .as_ref()
            .filter(|_| self.shown && self.own_window)
    }

    pub(super) fn cumulative(&self) -> bool {
        self.cumulative
    }

    /// The panel as drawn over the view, if shown there: never when it has
    /// a window of its own.
    pub(super) fn layout(&self) -> Option<&tuile_ui::PanelLayout> {
        self.layout
            .as_ref()
            .filter(|_| self.shown && !self.own_window)
    }

    /// The panel's own size, device pixels, for `signals` at `scale`: what
    /// a window of its own is opened at.
    pub(super) fn natural_size(
        &self,
        signals: Option<&BudgetSignals>,
        scale: f64,
        layers: &[String],
    ) -> (u32, u32) {
        let rows = self.rows_for(signals, Instant::now(), layers);
        tuile_ui::Panel::default()
            .layout(
                tuile_ui::Font::bundled(),
                &rows,
                (0.0, 0.0),
                (16_384, 16_384),
                scale,
            )
            .size
    }

    /// Lays the panel out from `signals`, at most every [`EVERY_MS`], in a
    /// frame of `frame` device pixels whose safe area starts at `at`. Says
    /// whether it was laid out again.
    pub(super) fn lay_out(
        &mut self,
        signals: Option<&BudgetSignals>,
        frame: (u32, u32),
        at: (f64, f64),
        scale: f64,
        layers: &[String],
    ) -> bool {
        if !self.shown {
            return false;
        }
        let now = Instant::now();
        let step = signals.map_or(0, |s| s.decide_micros.count);
        if let Some((when, size, _)) = self.laid {
            if size == frame && now.duration_since(when).as_millis() < EVERY_MS {
                return false;
            }
        }
        if std::mem::take(&mut self.reset) {
            self.from = signals.map(|s| (s.clone(), now));
        }
        let rows = self.rows_for(signals, now, layers);
        self.layout = Some(tuile_ui::Panel::default().layout(
            tuile_ui::Font::bundled(),
            &rows,
            at,
            frame,
            scale,
        ));
        self.laid = Some((now, frame, step));
        true
    }

    /// The rows for `signals`, in the view asked for, at `now`.
    fn rows_for(
        &self,
        signals: Option<&BudgetSignals>,
        now: Instant,
        layers: &[String],
    ) -> Vec<Vec<String>> {
        match signals {
            None => vec![vec!["Budget: no signals yet".to_owned()]],
            Some(s) => rows(
                s,
                self.cumulative
                    .then(|| {
                        self.from
                            .as_ref()
                            .map(|(f, when)| (f, now.duration_since(*when).as_secs_f64()))
                    })
                    .flatten(),
                self.cumulative,
                layers,
            ),
        }
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

/// A count, or a dot for nought: a table of zeros reads as a wall.
fn n(v: u64) -> String {
    if v == 0 {
        "·".to_owned()
    } else {
        v.to_string()
    }
}

fn pair(a: u64, b: u64) -> String {
    if a == 0 && b == 0 {
        "·".to_owned()
    } else {
        format!("{a}/{b}")
    }
}

/// The panel's rows for `s`: the instant view, or the cumulative one —
/// summed since `from` (the signals then, and how many seconds ago) or
/// since the session started. `layers` names the imagery layers by their
/// index, on the device only.
pub(super) fn rows(
    s: &BudgetSignals,
    from: Option<(&BudgetSignals, f64)>,
    cumulative: bool,
    layers: &[String],
) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let line = |t: String| vec![t];
    let since = |cur: u64, old: fn(&BudgetSignals) -> u64| -> u64 {
        cur.saturating_sub(from.map_or(0, |(f, _)| old(f)))
    };
    out.push(line(match (cumulative, from) {
        (false, _) => "Budget window — instant (the last step)".to_owned(),
        (true, None) => "Budget window — cumulative, since the session started".to_owned(),
        (true, Some((_, secs))) => format!("Budget window — cumulative, the last {secs:.0} s"),
    }));
    // The decision: its time and work.
    let d = &s.decide_micros;
    let (sum, count) = match from {
        Some((f, _)) => (
            d.sum.saturating_sub(f.decide_micros.sum),
            d.count.saturating_sub(f.decide_micros.count),
        ),
        None => (d.sum, d.count),
    };
    let work = since(s.decide_work_total, |f| f.decide_work_total);
    out.push(line(format!(
        "spec {}{} · SSE scale {:.2} · pressure {} · thermal {} · decide mean {:.2} ms, max {:.2} ms · work {} this step, {:.0} mean · {} steps",
        s.spec.label(),
        if s.spec_overridden { " (override)" } else { " (context)" },
        f64::from(s.sse_scale_milli) / 1000.0,
        ["normal", "warning", "critical", "background"]
            .get(usize::from(s.pressure_level))
            .unwrap_or(&"?"),
        s.thermal_level,
        sum as f64 / count.max(1) as f64 / 1000.0,
        s.decide_peak_micros as f64 / 1000.0,
        s.decide_work,
        work as f64 / count.max(1) as f64,
        count,
    )));
    // The totals against their targets and limits.
    let mut totals = Vec::new();
    for (c, name) in CLASSES {
        let [drawable, transit] = s.bytes[c];
        let hard = s.hard_limit_bytes[c];
        totals.push(format!(
            "{name} {} MiB (+{} transit) of {} target{}, {} protected",
            mib(drawable),
            mib(transit),
            mib(s.target_bytes[c]),
            if hard == u64::MAX {
                String::new()
            } else {
                format!(", {} hard", mib(hard))
            },
            mib(s.protected_bytes[c]),
        ));
    }
    out.push(line(totals.join(" · ")));
    out.push(line(format!(
        "decoded {} MiB · in flight {} · local reads {} · ground holes: {} lost, {} bare · floor {} · under need {} mesh, {} imagery",
        mib(s.decoded_bytes),
        s.in_flight.iter().map(|(_, v)| u64::from(v)).sum::<u64>(),
        s.local_reads,
        s.lost_tiles,
        s.bare_items,
        match (s.floor_in, s.floor_level) {
            (_, None) => "none".to_owned(),
            (true, Some(l)) => format!("in, levels ≤ {l}"),
            (false, Some(l)) => format!("not in yet, levels ≤ {l}"),
        },
        s.under_need_tiles[0],
        s.under_need_tiles[1],
    )));
    if cumulative {
        let up: u64 = s.upload_bytes_total.iter().map(|(_, v)| v).sum();
        let up0: u64 = from.map_or(0, |(f, _)| {
            f.upload_bytes_total.iter().map(|(_, v)| v).sum()
        });
        out.push(line(format!(
            "uploaded {} MiB · completions {} · wasted starts {} · cancels grace {}, timeout {}, preempted {}",
            mib(up.saturating_sub(up0)),
            since(s.completions_total, |f| f.completions_total),
            since(
                s.wasted_starts_total.iter().map(|(_, v)| v).sum(),
                |f| f.wasted_starts_total.iter().map(|(_, v)| v).sum()
            ),
            since(s.cancels_total[0], |f| f.cancels_total[0]),
            since(s.cancels_total[1], |f| f.cancels_total[1]),
            since(s.cancels_total[2], |f| f.cancels_total[2]),
        )));
    }
    // The classes with something to show; terrain and drapes always.
    let classes: Vec<(Class, &str)> = CLASSES
        .into_iter()
        .filter(|(c, _)| {
            *c != Class::Tiles3dMesh || s.levels_seen[*c] != 0 || s.bytes_levels[*c] != 0
        })
        .collect();
    let mut head = vec!["level".to_owned()];
    for (_, name) in &classes {
        let cols: &[&str] = if cumulative {
            &["started", "done", "wasted", "evicted", "released"]
        } else {
            &[
                "gpu",
                "MiB",
                "dec",
                "drawn/sel",
                "under",
                "fly n/p",
                "queue n/p",
            ]
        };
        head.extend(cols.iter().map(|c| format!("{name} {c}")));
    }
    out.push(Vec::new());
    out.push(head);
    let seen = classes
        .iter()
        .fold(0u32, |a, (c, _)| a | s.levels_seen[*c] | s.bytes_levels[*c]);
    let v = &s.levels;
    for l in (0..LEVELS).filter(|l| seen & (1 << l) != 0) {
        let floor = s.floor_level.is_some_and(|f| l as u32 <= f);
        let mut row = vec![format!("{l}{}", if floor { " F" } else { "" })];
        for (c, _) in &classes {
            let c = *c;
            if cumulative {
                let e = s.level_events_total[c][l];
                let e0 = from.map_or([0; 4], |(f, _)| f.level_events_total[c][l]);
                let ev = s.evictions_by_level_total[c][l]
                    .saturating_sub(from.map_or(0, |(f, _)| f.evictions_by_level_total[c][l]));
                row.extend([
                    n(e[0].saturating_sub(e0[0])),
                    n(e[1].saturating_sub(e0[1])),
                    n(e[2].saturating_sub(e0[2])),
                    n(ev),
                    n(e[3].saturating_sub(e0[3])),
                ]);
            } else {
                let [drawable, transit] = s.bytes_by_level[c][l];
                let bytes = drawable + transit;
                row.extend([
                    n(u64::from(v.gpu_tiles[c][l])),
                    if bytes == 0 {
                        "·".to_owned()
                    } else {
                        mib(bytes)
                    },
                    n(u64::from(v.decoded_tiles[c][l])),
                    pair(u64::from(v.drawn[c][l]), u64::from(v.selected[c][l])),
                    n(u64::from(v.under_need[c][l])),
                    pair(
                        u64::from(v.in_flight[c][l][0]),
                        u64::from(v.in_flight[c][l][1]),
                    ),
                    pair(u64::from(v.queued[c][l][0]), u64::from(v.queued[c][l][1])),
                ]);
            }
        }
        out.push(row);
    }
    // The imagery layers: by index, named here from the device's own list.
    out.push(Vec::new());
    let mut head = vec![
        "layer".to_owned(),
        "gpu".to_owned(),
        "MiB".to_owned(),
        "drawn".to_owned(),
        "in flight".to_owned(),
    ];
    head.extend(
        ["store", "network", "RAM hits", "done", "failed"]
            .iter()
            .map(|c| format!("{c}{}", if cumulative { "" } else { " (step)" })),
    );
    out.push(head);
    let y = &s.layers;
    for i in (0..LAYERS).filter(|i| s.layers_seen & (1 << i) != 0) {
        let name = match (i, layers.get(i)) {
            (0, Some(n)) => format!("0 base: {n}"),
            (0, None) => "0 base".to_owned(),
            (_, Some(n)) => format!("{i} overlay: {n}"),
            (_, None) => format!("{i} overlay"),
        };
        let mut row = vec![
            name,
            n(u64::from(y.gpu_tiles[i])),
            mib(y.gpu_bytes[i].0),
            n(u64::from(y.drawn[i])),
            n(u64::from(y.in_flight[i])),
        ];
        if cumulative {
            let t = s.layer_loads_total[i];
            let t0 = from.map_or([0; 5], |(f, _)| f.layer_loads_total[i]);
            row.extend((0..5).map(|k| n(t[k].saturating_sub(t0[k]))));
        } else {
            row.extend(
                [
                    y.requests[i][0],
                    y.requests[i][1],
                    y.ram_hits[i],
                    y.completions[i],
                    y.failures[i],
                ]
                .map(|v| n(u64::from(v))),
            );
        }
        out.push(row);
    }
    // An empty row is a blank line.
    out.into_iter()
        .map(|r| if r.is_empty() { vec![String::new()] } else { r })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> BudgetSignals {
        let mut s = BudgetSignals::default();
        s.levels_seen[Class::TerrainMesh] = 1 << 12;
        s.levels_seen[Class::Drape] = 1 << 13;
        s.levels.gpu_tiles[Class::TerrainMesh][12] = 7;
        s.levels.drawn[Class::TerrainMesh][12] = 6;
        s.levels.selected[Class::TerrainMesh][12] = 9;
        s.levels.in_flight[Class::Drape][13] = [2, 1];
        s.bytes_by_level[Class::TerrainMesh][12] = [3 * 1024 * 1024, 0];
        s.level_events_total[Class::TerrainMesh][12] = [40, 35, 5, 2];
        s.layers_seen = 1;
        s.layers.gpu_tiles[0] = 11;
        s.layer_loads_total[0] = [3, 8, 4, 10, 1];
        s.decide_micros.sum = 3_000;
        s.decide_micros.count = 10;
        s.floor_level = Some(2);
        s
    }

    fn row<'a>(rows: &'a [Vec<String>], first: &str) -> &'a [String] {
        rows.iter()
            .find(|r| r.first().is_some_and(|c| c == first))
            .unwrap_or_else(|| panic!("no row {first}: {rows:?}"))
    }

    /// The instant view reads the step's numbers off the signals, at their
    /// level and class; the layers by index, named from the device's list.
    #[test]
    fn the_instant_view_is_the_signals_of_the_step() {
        let s = signals();
        let rows = rows(&s, None, false, &["Aerial".to_owned()]);
        let t12 = row(&rows, "12");
        assert_eq!(&t12[1..5], ["7", "3.0", "·", "6/9"]);
        let d13 = row(&rows, "13");
        // Terrain's seven columns, then the drape's: fly n/p is the sixth.
        assert_eq!(d13[1 + 7 + 5], "2/1");
        let layer = row(&rows, "0 base: Aerial");
        assert_eq!(layer[1], "11");
        assert!(rows[1][0].contains("decide mean 0.30 ms"), "{:?}", rows[1]);
    }

    /// The cumulative view is the counters since its start, or since it
    /// was reset: the signals then subtracted.
    #[test]
    fn the_cumulative_view_sums_from_its_start() {
        let s = signals();
        let rows_all = rows(&s, None, true, &[]);
        assert_eq!(&row(&rows_all, "12")[1..6], ["40", "35", "5", "·", "2"]);
        assert_eq!(&row(&rows_all, "0 base")[5..10], ["3", "8", "4", "10", "1"]);
        let mut then = s.clone();
        then.level_events_total[Class::TerrainMesh][12] = [30, 30, 5, 0];
        then.layer_loads_total[0] = [3, 2, 4, 4, 1];
        let rows_reset = rows(&s, Some((&then, 12.0)), true, &[]);
        assert!(rows_reset[0][0].contains("the last 12 s"));
        assert_eq!(&row(&rows_reset, "12")[1..6], ["10", "5", "·", "·", "2"]);
        assert_eq!(
            &row(&rows_reset, "0 base")[5..10],
            ["·", "6", "·", "6", "·"]
        );
    }

    /// The floor's levels are marked.
    #[test]
    fn the_floor_levels_are_marked() {
        let mut s = signals();
        s.levels_seen[Class::TerrainMesh] |= 1 << 2;
        let rows = rows(&s, None, false, &[]);
        assert!(rows.iter().any(|r| r[0] == "2 F"), "{rows:?}");
    }

    /// Shown by its toggle or by asking for a view; a reset shows the
    /// cumulative view.
    #[test]
    fn the_window_follows_what_it_is_asked() {
        let mut w = BudgetWindow::default();
        assert!(!w.shown());
        w.ask(WindowAsk::Show(true));
        assert!(w.shown() && !w.cumulative());
        w.ask(WindowAsk::Reset);
        assert!(w.shown() && w.cumulative());
        w.ask(WindowAsk::Show(false));
        assert!(!w.shown() && w.layout().is_none());
    }

    /// On a shell that gives it a window of its own, the panel is laid out
    /// for that window and never drawn over the view; closing its window
    /// hides it, as `window=off` does, and asking again shows it again.
    #[test]
    fn a_panel_with_its_own_window_is_never_over_the_view() {
        let s = signals();
        let mut w = BudgetWindow::default();
        w.set_own_window(true);
        assert!(!w.wants_own_window(), "hidden: no window");
        w.ask(WindowAsk::Show(true));
        assert!(w.wants_own_window());
        assert!(w.lay_out(Some(&s), (900, 400), (0.0, 0.0), 1.0, &[]));
        assert!(w.own_layout().is_some(), "laid out for its window");
        assert!(w.layout().is_none(), "nothing over the view");
        w.closed();
        assert!(!w.shown() && !w.wants_own_window() && w.own_layout().is_none());
        w.ask(WindowAsk::Cumulative(true));
        assert!(w.wants_own_window() && w.cumulative());
    }

    /// Over the view (a phone), the panel is drawn there, and never asks
    /// for a window.
    #[test]
    fn a_panel_over_the_view_never_wants_a_window() {
        let s = signals();
        let mut w = BudgetWindow::default();
        w.ask(WindowAsk::Show(true));
        assert!(!w.wants_own_window());
        w.lay_out(Some(&s), (900, 400), (8.0, 8.0), 1.0, &[]);
        assert!(w.layout().is_some() && w.own_layout().is_none());
    }

    /// A window of its own opens at the panel's size: as wide as the
    /// table, whatever the frame.
    #[test]
    fn its_own_window_opens_at_the_panels_size() {
        let s = signals();
        let w = BudgetWindow::default();
        let (width, height) = w.natural_size(Some(&s), 1.0, &[]);
        let (w2, h2) = w.natural_size(Some(&s), 2.0, &[]);
        assert!(width > 300 && height > 100, "{width}x{height}");
        assert!(w2 > width * 3 / 2 && h2 > height * 3 / 2, "denser, larger");
    }
}
