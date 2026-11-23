// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget window, drawn: the numbers of the budget's tables
//! (`BudgetSignals`, the signals the budget publishes and the telemetry
//! exports) as gauges, bars and colours, read at a glance — and every
//! number still written beside its shape.
//!
//! Nothing here computes a number the signals do not carry: each bar is a
//! value of the window's tables, each colour a rule over those values
//! ([`gauge`], [`level_severity`], [`attention`]). The rules say what
//! deserves a look:
//!
//! - **alert** (vermillion, a triangle): ground missing, a class over its
//!   target or hard limit, tiles under their need with nothing in flight
//!   or queued for them — a stall — a layer's loads failing;
//! - **warn** (orange, a diamond): a class near its target, tiles under
//!   their need while their loads run, selected tiles drawn by stand-ins,
//!   memory pressure or heat;
//! - **waste** (purple, a square): many copies held at a level nothing
//!   selects and nothing draws;
//! - **ok** (bluish green, a dot).
//!
//! The palette is Okabe and Ito's, told apart by readers with any of the
//! common colour-vision deficiencies, and every colour comes with a shape
//! and a word: colour is never the only cue.
//!
//! The drawing is a [`Drawing`]: triangles and text labels in device
//! pixels, drawn by any backend's overlay — the desktop's own window, a
//! page's browser window and a headless run's texture alike.

use std::collections::{HashMap, VecDeque};

use tuile_core::budget::{BudgetSignals, Class, LAYERS, LEVELS};

use crate::text::{Font, Label, Mask};
use crate::{Mesh, Vertex};

/// A panel drawn: triangles, then text over them, in device pixels; and
/// its size.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drawing {
    pub mesh: Mesh,
    pub labels: Vec<Label>,
    pub size: (u32, u32),
}

impl From<crate::PanelLayout> for Drawing {
    fn from(p: crate::PanelLayout) -> Self {
        Self {
            mesh: p.background,
            labels: p.label.into_iter().collect(),
            size: p.size,
        }
    }
}

/// How much a value deserves a look, least first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    Waste,
    Warn,
    Alert,
}

/// An sRGB colour, as the overlay takes it: linear.
fn srgb(hex: u32, alpha: f32) -> [f32; 4] {
    let c = |shift: u32| {
        let v = f32::from(((hex >> shift) & 0xff) as u8) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    [c(16), c(8), c(0), alpha]
}

impl Severity {
    /// Its colour: Okabe and Ito's bluish green, reddish purple, orange
    /// and vermillion.
    pub fn colour(self) -> [f32; 4] {
        match self {
            Self::Ok => srgb(0x009E73, 1.0),
            Self::Waste => srgb(0xCC79A7, 1.0),
            Self::Warn => srgb(0xE69F00, 1.0),
            Self::Alert => srgb(0xD55E00, 1.0),
        }
    }

    /// Its word, for the attention list.
    pub fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Waste => "waste",
            Self::Warn => "watch",
            Self::Alert => "ALERT",
        }
    }
}

/// How near its target a class may come before it is watched: nine
/// tenths.
pub const NEAR_TARGET: f64 = 0.9;

/// A class's GPU bytes against its target and hard limit, as the budget
/// counts them: its unprotected bytes over the target, or all of them over
/// the hard limit, is an alert (the budget's invariant I2); its unprotected
/// bytes at [`NEAR_TARGET`] of the target or more, a warning.
pub fn gauge(gpu: u64, protected: u64, target: u64, hard: u64) -> Severity {
    let unprotected = gpu.saturating_sub(protected);
    if unprotected > target || gpu > hard {
        Severity::Alert
    } else if target > 0 && unprotected as f64 >= NEAR_TARGET * target as f64 {
        Severity::Warn
    } else {
        Severity::Ok
    }
}

/// Copies held at a level nothing selects, from which a level is waste.
pub const WASTE_TILES: u32 = 64;

/// One level of one class, as the instant table has it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LevelCell {
    pub gpu: u32,
    pub bytes: u64,
    pub decoded: u32,
    pub drawn: u32,
    pub selected: u32,
    pub under: u32,
    /// Need, prefetch.
    pub in_flight: [u32; 2],
    pub queued: [u32; 2],
}

impl LevelCell {
    pub fn of(s: &BudgetSignals, c: Class, l: usize) -> Self {
        let v = &s.levels;
        let [drawable, transit] = s.bytes_by_level[c][l];
        Self {
            gpu: v.gpu_tiles[c][l],
            bytes: drawable + transit,
            decoded: v.decoded_tiles[c][l],
            drawn: v.drawn[c][l],
            selected: v.selected[c][l],
            under: v.under_need[c][l],
            in_flight: v.in_flight[c][l],
            queued: v.queued[c][l],
        }
    }

    fn loading(&self) -> u32 {
        self.in_flight[0] + self.in_flight[1] + self.queued[0] + self.queued[1]
    }
}

/// One level of one class, judged: tiles under their need with nothing in
/// flight or queued at the level is a stall (alert); under their need while
/// loads run, or selected tiles drawn by stand-ins, a warning; many copies
/// at a level nothing selects or draws — the floor's levels aside — waste.
pub fn level_severity(cell: &LevelCell, floor: bool) -> Severity {
    if cell.under > 0 && cell.loading() == 0 {
        Severity::Alert
    } else if cell.under > 0 || cell.drawn < cell.selected {
        Severity::Warn
    } else if !floor && cell.selected == 0 && cell.gpu.saturating_sub(cell.drawn) >= WASTE_TILES {
        Severity::Waste
    } else {
        Severity::Ok
    }
}

/// Something that deserves a look, in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    pub text: String,
    /// How large it is, to order issues of one severity.
    weight: u64,
}

/// The classes, with the names a person reads.
const CLASSES: [(Class, &str, &str); 3] = [
    (Class::TerrainMesh, "T", "terrain"),
    (Class::Drape, "D", "imagery"),
    (Class::Tiles3dMesh, "3D", "3D Tiles"),
];

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn floor_at(s: &BudgetSignals, l: usize) -> bool {
    s.floor_level.is_some_and(|f| l as u32 <= f)
}

/// What deserves a look in `s`, the worst first. A healthy budget has
/// nothing here.
pub fn attention(s: &BudgetSignals) -> Vec<Issue> {
    let mut out = Vec::new();
    let mut add = |severity, weight: u64, text: String| {
        out.push(Issue {
            severity,
            text,
            weight,
        });
    };
    if s.lost_tiles > 0 || s.bare_items > 0 {
        add(
            Severity::Alert,
            u64::from(s.lost_tiles + s.bare_items),
            format!(
                "ground: {} tiles lost, {} bare — ground missing",
                s.lost_tiles, s.bare_items
            ),
        );
    }
    for (c, _, name) in CLASSES {
        let cells: Vec<(usize, LevelCell)> =
            (0..LEVELS).map(|l| (l, LevelCell::of(s, c, l))).collect();
        let under: u64 = cells.iter().map(|(_, x)| u64::from(x.under)).sum();
        let loading: u64 = cells.iter().map(|(_, x)| u64::from(x.loading())).sum();
        if under > 0 && loading == 0 {
            add(
                Severity::Alert,
                under,
                format!("{name}: {under} tiles under need, nothing in flight or queued — stalled"),
            );
        } else if under > 0 {
            add(
                Severity::Warn,
                under,
                format!("{name}: {under} tiles under need, {loading} loads in flight or queued"),
            );
            // The levels that wait with nothing of their own loading.
            for (l, x) in &cells {
                if x.under > 0 && x.loading() == 0 {
                    add(
                        Severity::Alert,
                        u64::from(x.under),
                        format!(
                            "level {l} {name}: {} under need, nothing loading at this level",
                            x.under
                        ),
                    );
                }
            }
        }
        let stand_ins: u64 = cells
            .iter()
            .map(|(_, x)| u64::from(x.selected.saturating_sub(x.drawn)))
            .sum();
        if stand_ins > 0 {
            add(
                Severity::Warn,
                stand_ins,
                format!("{name}: {stand_ins} selected tiles drawn by stand-ins"),
            );
        }
        for (l, x) in &cells {
            if level_severity(x, floor_at(s, *l)) == Severity::Waste {
                add(
                    Severity::Waste,
                    u64::from(x.gpu - x.drawn),
                    format!(
                        "level {l} {name}: {} resident, {} drawn, none selected",
                        x.gpu, x.drawn
                    ),
                );
            }
        }
        let [drawable, transit] = s.bytes[c];
        let gpu = drawable + transit;
        let (target, hard) = (s.target_bytes[c], s.hard_limit_bytes[c]);
        match gauge(gpu, s.protected_bytes[c], target, hard) {
            Severity::Alert if gpu > hard => add(
                Severity::Alert,
                gpu,
                format!(
                    "{name}: {} MiB over its {} MiB hard limit",
                    mib(gpu),
                    mib(hard)
                ),
            ),
            Severity::Alert => add(
                Severity::Alert,
                gpu,
                format!(
                    "{name}: {} MiB unprotected, over its {} MiB target",
                    mib(gpu.saturating_sub(s.protected_bytes[c])),
                    mib(target)
                ),
            ),
            Severity::Warn => add(
                Severity::Warn,
                gpu,
                format!(
                    "{name}: {} MiB, near its {} MiB target",
                    mib(gpu),
                    mib(target)
                ),
            ),
            _ => {}
        }
    }
    let state = |n: u8| ["normal", "warning", "critical", "background"][usize::from(n.min(3))];
    match s.pressure_level {
        0 => {}
        1 => add(Severity::Warn, 1, "memory pressure: warning".to_owned()),
        n => add(Severity::Alert, 2, format!("memory pressure: {}", state(n))),
    }
    match s.thermal_level {
        0 => {}
        1 => add(Severity::Warn, 1, "thermal state 1: fair".to_owned()),
        n => add(Severity::Alert, 2, format!("thermal state {n}: hot")),
    }
    for i in (0..LAYERS).filter(|i| s.layers_seen & (1 << i) != 0) {
        let failed = s.layers.failures[i];
        if failed > 0 {
            add(
                Severity::Alert,
                u64::from(failed),
                format!("layer {i}: {failed} loads failed this step"),
            );
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.weight.cmp(&a.weight)));
    out
}

/// The decision's time over the last publications: the mean of the steps
/// they cover and the worst of them. The budget publishes every half
/// second, its peak since the last publication and its sums since the
/// start; the time a step takes is read here from the sums' differences
/// and the peaks of the same publications, so the mean and the maximum
/// always cover the same steps — the maximum is never under the mean.
#[derive(Debug, Clone, Default)]
pub struct DecideHistory {
    /// The last publication's sums: steps, µs.
    last: Option<(u64, u64)>,
    /// Per publication since: steps, µs, peak µs.
    samples: VecDeque<(u64, u64, u64)>,
}

/// The publications [`DecideHistory`] keeps: a minute.
pub const DECIDE_SAMPLES: usize = 120;

/// The decision's time over a window: mean and maximum, ms, the steps and
/// publications it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecideSummary {
    pub mean_ms: f64,
    pub max_ms: f64,
    pub steps: u64,
    pub publications: usize,
}

impl DecideHistory {
    /// Takes `s` if it is a publication not yet seen.
    pub fn note(&mut self, s: &BudgetSignals) {
        let (count, sum) = (s.decide_micros.count, s.decide_micros.sum);
        match self.last {
            Some((c, _)) if c == count => return,
            // A new session: start again.
            Some((c, _)) if count < c => self.samples.clear(),
            Some((c, m)) => {
                self.samples
                    .push_back((count - c, sum.saturating_sub(m), s.decide_peak_micros));
                while self.samples.len() > DECIDE_SAMPLES {
                    self.samples.pop_front();
                }
            }
            // The first: only where the sums start from — its peak covers
            // the last half second, its sums the whole session.
            None => {}
        }
        self.last = Some((count, sum));
    }

    /// The summary, once two publications have been seen.
    pub fn summary(&self) -> Option<DecideSummary> {
        let steps: u64 = self.samples.iter().map(|s| s.0).sum();
        if steps == 0 {
            return None;
        }
        let sum: u64 = self.samples.iter().map(|s| s.1).sum();
        let peak = self.samples.iter().map(|s| s.2).max().unwrap_or(0);
        Some(DecideSummary {
            mean_ms: sum as f64 / steps as f64 / 1000.0,
            max_ms: peak as f64 / 1000.0,
            steps,
            publications: self.samples.len(),
        })
    }
}

/// The texts rasterised, kept from one drawing to the next.
#[derive(Debug, Default)]
pub struct TextCache {
    masks: HashMap<(String, u32), Mask>,
}

/// What the board shows: this step, or the counters summed — since the
/// start, or since `from` (the signals then, and how many seconds ago).
pub struct BoardView<'a> {
    pub cumulative: bool,
    pub from: Option<(&'a BudgetSignals, f64)>,
    /// The imagery layers' names, by index.
    pub layers: &'a [String],
    pub decide: Option<DecideSummary>,
}

const TEXT: u32 = 0xDDDDDD;
const DIM: u32 = 0x8C8C8C;
const TRACK: u32 = 0x2A2A2A;
/// Drawn: Okabe and Ito's blue.
const DRAWN: u32 = 0x0072B2;
/// Held, not drawn.
const HELD: u32 = 0x5C5C5C;
/// Loads: need in sky blue, prefetch the same paler; queued in yellow.
const LOADS: u32 = 0x56B4E9;
const QUEUED: u32 = 0xF0E442;

/// The text's em, points.
const EM: f64 = 11.0;
const ROW: f64 = 16.0;
const MARGIN: f64 = 12.0;

struct Painter<'a> {
    font: &'static Font,
    scale: f64,
    mesh: Mesh,
    labels: Vec<Label>,
    cache: &'a mut TextCache,
    extent: (f64, f64),
}

impl Painter<'_> {
    fn grow(&mut self, x: f64, y: f64) {
        self.extent.0 = self.extent.0.max(x);
        self.extent.1 = self.extent.1.max(y);
    }

    fn tri(&mut self, p: [(f64, f64); 3], colour: [f32; 4]) {
        for (x, y) in p {
            self.grow(x, y);
            self.mesh
                .push(Vertex::new(x * self.scale, y * self.scale, colour));
        }
    }

    fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, colour: [f32; 4]) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (x1, y1) = (x + w, y + h);
        self.tri([(x, y), (x1, y), (x, y1)], colour);
        self.tri([(x, y1), (x1, y), (x1, y1)], colour);
    }

    fn width(&self, text: &str, size: f64) -> f64 {
        f64::from(self.font.width(text, (size * self.scale) as f32)) / self.scale
    }

    /// `text` with its top-left corner at `(x, y)`; returns its width.
    fn text(&mut self, x: f64, y: f64, text: &str, size: f64, colour: [f32; 4]) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        let px = (size * self.scale) as f32;
        if self.cache.masks.len() > 4_096 {
            self.cache.masks.clear();
        }
        let font = self.font;
        let mask = self
            .cache
            .masks
            .entry((text.to_owned(), (px * 16.0) as u32))
            .or_insert_with(|| font.rasterize(text, px))
            .clone();
        let w = f64::from(mask.width) / self.scale;
        self.grow(x + w, y + f64::from(mask.height) / self.scale);
        self.labels.push(Label {
            mask,
            position: [
                (x * self.scale).round() as f32,
                (y * self.scale).round() as f32,
            ],
            color: colour,
        });
        w
    }

    /// `text` ending at `right`.
    fn text_right(&mut self, right: f64, y: f64, text: &str, size: f64, colour: [f32; 4]) {
        let w = self.width(text, size);
        self.text(right - w, y, text, size, colour);
    }

    /// The severity's shape, `size` points, centred at `(x, y)`: a dot, a
    /// square, a diamond, a triangle.
    fn marker(&mut self, sev: Severity, x: f64, y: f64, size: f64) {
        let r = size / 2.0;
        let c = sev.colour();
        match sev {
            Severity::Ok => {
                let n = 8;
                for i in 0..n {
                    let a0 = std::f64::consts::TAU * f64::from(i) / f64::from(n);
                    let a1 = std::f64::consts::TAU * f64::from(i + 1) / f64::from(n);
                    let k = r * 0.8;
                    self.tri(
                        [
                            (x, y),
                            (x + k * a0.cos(), y + k * a0.sin()),
                            (x + k * a1.cos(), y + k * a1.sin()),
                        ],
                        c,
                    );
                }
            }
            Severity::Waste => self.rect(x - r * 0.8, y - r * 0.8, r * 1.6, r * 1.6, c),
            Severity::Warn => {
                self.tri([(x, y - r), (x + r, y), (x, y + r)], c);
                self.tri([(x, y - r), (x, y + r), (x - r, y)], c);
            }
            Severity::Alert => {
                self.tri([(x, y - r), (x + r, y + r * 0.8), (x - r, y + r * 0.8)], c)
            }
        }
    }

    /// A bar of `w × h` at `(x, y)`: its track, then `parts` stacked from
    /// the left, each its share of `max`.
    fn bar(&mut self, x: f64, y: f64, w: f64, h: f64, parts: &[(f64, [f32; 4])], max: f64) {
        self.rect(x, y, w, h, srgb(TRACK, 1.0));
        if max <= 0.0 {
            return;
        }
        let mut at = x;
        for (v, c) in parts {
            let len = (v / max * w).clamp(0.0, x + w - at);
            self.rect(at, y, len, h, *c);
            at += len;
        }
    }

    /// A badge: the severity's colour behind `text`, its marker before it.
    fn badge(&mut self, x: f64, y: f64, sev: Severity, text: &str) -> f64 {
        let w = self.width(text, EM) + 22.0;
        let mut bg = sev.colour();
        bg[3] = 0.28;
        self.rect(x, y - 1.0, w, ROW - 2.0, bg);
        self.marker(sev, x + 8.0, y + ROW / 2.0 - 2.0, 9.0);
        self.text(x + 16.0, y, text, EM, srgb(TEXT, 1.0));
        w
    }
}

/// A count, or a dot for nought.
fn n(v: u64) -> String {
    if v == 0 {
        "·".to_owned()
    } else {
        v.to_string()
    }
}

fn pair(a: u32, b: u32) -> String {
    if a == 0 && b == 0 {
        "·".to_owned()
    } else {
        format!("{a}/{b}")
    }
}

/// The instant chart's number columns, per class: header, width.
const INSTANT_COLUMNS: [(&str, f64); 7] = [
    ("gpu", 36.0),
    ("MiB", 44.0),
    ("dec", 36.0),
    ("drawn/sel", 66.0),
    ("under", 42.0),
    ("fly n/p", 50.0),
    ("queue n/p", 58.0),
];
const CUMULATIVE_COLUMNS: [(&str, f64); 5] = [
    ("started", 54.0),
    ("done", 54.0),
    ("wasted", 54.0),
    ("evicted", 54.0),
    ("released", 60.0),
];
const LEVEL_COLUMN: f64 = 40.0;
const RESIDENCY_BAR: f64 = 110.0;
const LOADS_BAR: f64 = 50.0;

/// Draws the board for `s` (none yet: a line saying so) at `scale` device
/// pixels per point, its top-left corner at the origin.
pub fn draw(
    s: Option<&BudgetSignals>,
    view: &BoardView<'_>,
    cache: &mut TextCache,
    scale: f64,
) -> Drawing {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut p = Painter {
        font: Font::bundled(),
        scale,
        mesh: Mesh::new(),
        labels: Vec::new(),
        cache,
        extent: (0.0, 0.0),
    };
    let mut y = MARGIN;
    match s {
        None => {
            p.text(MARGIN, y, "Budget: no signals yet", EM, srgb(TEXT, 1.0));
        }
        Some(s) => {
            y = header(&mut p, s, view, y);
            y = issues(&mut p, s, y);
            y = gauges(&mut p, s, view, y);
            y = levels(&mut p, s, view, y);
            layers(&mut p, s, view, y);
        }
    }
    let (w, h) = (p.extent.0 + MARGIN, p.extent.1 + MARGIN);
    // The background, under everything: the triangles drawn so far go
    // after it.
    let mut mesh = Mesh::new();
    let bg = crate::PANEL_BACKGROUND;
    let opaque = [bg[0], bg[1], bg[2], 1.0];
    for (x, yy) in [(0.0, 0.0), (w, 0.0), (0.0, h), (0.0, h), (w, 0.0), (w, h)] {
        mesh.push(Vertex::new(x * scale, yy * scale, opaque));
    }
    mesh.extend(p.mesh);
    Drawing {
        mesh,
        labels: p.labels,
        size: ((w * scale).ceil() as u32, (h * scale).ceil() as u32),
    }
}

/// The title, the specification, pressure and heat, the decision's time.
fn header(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let title = match (view.cumulative, view.from) {
        (false, _) => "Budget — instant (the last step)".to_owned(),
        (true, None) => "Budget — cumulative, since the session started".to_owned(),
        (true, Some((_, secs))) => format!("Budget — cumulative, the last {secs:.0} s"),
    };
    p.text(MARGIN, y, &title, 13.0, text);
    let y = y + 20.0;
    let mut x = MARGIN;
    x += p.text(
        x,
        y,
        &format!(
            "spec {}{} · SSE scale {:.2}",
            s.spec.label(),
            if s.spec_overridden {
                " (override)"
            } else {
                " (context)"
            },
            f64::from(s.sse_scale_milli) / 1000.0
        ),
        EM,
        text,
    ) + 12.0;
    let pressure = ["normal", "warning", "critical", "background"]
        .get(usize::from(s.pressure_level))
        .unwrap_or(&"?");
    let sev = |l: u8| match l {
        0 => Severity::Ok,
        1 => Severity::Warn,
        _ => Severity::Alert,
    };
    x += p.badge(x, y, sev(s.pressure_level), &format!("pressure {pressure}")) + 8.0;
    x += p.badge(
        x,
        y,
        sev(s.thermal_level),
        &format!("thermal {}", s.thermal_level),
    ) + 12.0;
    let decide = match view.decide {
        Some(d) => format!(
            "decide mean {:.2} ms, max {:.2} ms over the last {} steps ({:.0} s)",
            d.mean_ms,
            d.max_ms,
            d.steps,
            d.publications as f64 * 0.5
        ),
        None => "decide: waiting for a second publication".to_owned(),
    };
    x += p.text(x, y, &decide, EM, text) + 12.0;
    p.text(
        x,
        y,
        &format!(
            "work {} this step, {:.0} mean · {} steps",
            s.decide_work,
            s.decide_work_total as f64 / s.decide_micros.count.max(1) as f64,
            s.decide_micros.count
        ),
        EM,
        dim,
    );
    y + ROW + 4.0
}

/// The attention list: the worst issues, one line each, or one line
/// saying there are none.
fn issues(p: &mut Painter<'_>, s: &BudgetSignals, y: f64) -> f64 {
    let list = attention(s);
    let text = srgb(TEXT, 1.0);
    let mut y = y;
    if list.is_empty() {
        p.marker(Severity::Ok, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
        p.text(MARGIN + 14.0, y, "nothing needs attention", EM, text);
        return y + ROW + 6.0;
    }
    const SHOWN: usize = 6;
    for issue in list.iter().take(SHOWN) {
        p.marker(issue.severity, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
        let w = p.text(
            MARGIN + 14.0,
            y,
            issue.severity.word(),
            EM,
            issue.severity.colour(),
        );
        p.text(MARGIN + 20.0 + w.max(36.0), y, &issue.text, EM, text);
        y += ROW;
    }
    if list.len() > SHOWN {
        p.text(
            MARGIN + 14.0,
            y,
            &format!("and {} more", list.len() - SHOWN),
            EM,
            srgb(DIM, 1.0),
        );
        y += ROW;
    }
    y + 6.0
}

/// The classes' GPU bytes against their targets and limits, and the
/// header's other numbers.
fn gauges(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    const BAR: f64 = 260.0;
    for (c, short, name) in CLASSES {
        let [drawable, transit] = s.bytes[c];
        if c == Class::Tiles3dMesh && drawable + transit == 0 && s.levels_seen[c] == 0 {
            continue;
        }
        let (target, hard, protected) = (
            s.target_bytes[c],
            s.hard_limit_bytes[c],
            s.protected_bytes[c],
        );
        let sev = gauge(drawable + transit, protected, target, hard);
        let finite_hard = (hard != u64::MAX).then_some(hard);
        let max = (finite_hard.unwrap_or(0).max(target).max(drawable + transit) as f64) * 1.05;
        let x0 = MARGIN + 74.0;
        p.marker(sev, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
        p.text(MARGIN + 14.0, y, &format!("{short} {name}"), EM, text);
        let mut prot = srgb(DRAWN, 1.0);
        prot[3] = 0.55;
        let mut fill = sev.colour();
        fill[3] = 0.9;
        let shown = drawable.saturating_sub(protected.min(drawable));
        p.bar(
            x0,
            y + 3.0,
            BAR,
            9.0,
            &[
                (protected.min(drawable) as f64, prot),
                (shown as f64, fill),
                (transit as f64, srgb(LOADS, 0.8)),
            ],
            max,
        );
        // The target and the hard limit, as ticks.
        // The target white, the hard limit grey: neither in a severity's
        // colour, which is the bar's alone.
        for (at, colour) in [
            (Some(target), srgb(TEXT, 1.0)),
            (finite_hard, srgb(DIM, 1.0)),
        ] {
            if let Some(v) = at.filter(|_| max > 0.0) {
                let tx = x0 + v as f64 / max * BAR;
                p.rect(tx - 1.0, y + 1.0, 2.0, 13.0, colour);
            }
        }
        p.text(
            x0 + BAR + 10.0,
            y,
            &format!(
                "{} MiB (+{} transit) of {} target{}, {} protected",
                mib(drawable),
                mib(transit),
                mib(target),
                finite_hard.map_or(String::new(), |h| format!(", {} hard", mib(h))),
                mib(protected),
            ),
            EM,
            text,
        );
        y += ROW;
    }
    // The other numbers of the header, as the table has them; ground holes
    // and floor as badges.
    let mut x = MARGIN;
    let holes = if s.lost_tiles > 0 || s.bare_items > 0 {
        Severity::Alert
    } else {
        Severity::Ok
    };
    x += p.badge(
        x,
        y,
        holes,
        &format!("ground holes: {} lost, {} bare", s.lost_tiles, s.bare_items),
    ) + 8.0;
    let floor = match (s.floor_in, s.floor_level) {
        (_, None) => (Severity::Ok, "floor none".to_owned()),
        (true, Some(l)) => (Severity::Ok, format!("floor in, levels ≤ {l}")),
        (false, Some(l)) => (Severity::Warn, format!("floor not in yet, levels ≤ {l}")),
    };
    x += p.badge(x, y, floor.0, &floor.1) + 12.0;
    p.text(
        x,
        y,
        &format!(
            "decoded {} MiB · in flight {} · local reads {} · under need {} mesh, {} imagery",
            mib(s.decoded_bytes),
            s.in_flight.iter().map(|(_, v)| u64::from(v)).sum::<u64>(),
            s.local_reads,
            s.under_need_tiles[0],
            s.under_need_tiles[1],
        ),
        EM,
        text,
    );
    y += ROW;
    if view.cumulative {
        let from = view.from.map(|(f, _)| f);
        let since =
            |cur: u64, old: fn(&BudgetSignals) -> u64| cur.saturating_sub(from.map_or(0, old));
        let up: u64 = s.upload_bytes_total.iter().map(|(_, v)| v).sum();
        let up0: u64 = from.map_or(0, |f| f.upload_bytes_total.iter().map(|(_, v)| v).sum());
        p.text(
            MARGIN,
            y,
            &format!(
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
            ),
            EM,
            dim,
        );
        y += ROW;
    }
    y + 8.0
}

/// The chart by level: for each class, side by side, the copies held
/// (drawn, held undrawn) as a bar, the table's numbers, and the loads (in
/// flight and queued, need and prefetch) as a bar; each level's cell
/// tinted and marked by [`level_severity`]. Cumulative: the counters summed.
fn levels(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let classes: Vec<(Class, &str)> = CLASSES
        .iter()
        .filter(|(c, _, _)| {
            *c != Class::Tiles3dMesh || s.levels_seen[*c] != 0 || s.bytes_levels[*c] != 0
        })
        .map(|(c, short, _)| (*c, *short))
        .collect();
    let seen = classes
        .iter()
        .fold(0u32, |a, (c, _)| a | s.levels_seen[*c] | s.bytes_levels[*c]);
    let numbers: f64 = if view.cumulative {
        CUMULATIVE_COLUMNS.iter().map(|c| c.1).sum()
    } else {
        INSTANT_COLUMNS.iter().map(|c| c.1).sum()
    };
    let block = 14.0
        + RESIDENCY_BAR
        + 6.0
        + numbers
        + if view.cumulative {
            0.0
        } else {
            8.0 + LOADS_BAR
        }
        + 16.0;
    // The scales: the most copies, the most loads, of a class at a level.
    let mut most = HashMap::new();
    for (c, _) in &classes {
        let (mut copies, mut loads) = (1u64, 1u64);
        for l in (0..LEVELS).filter(|l| seen & (1 << l) != 0) {
            let x = LevelCell::of(s, *c, l);
            copies = copies.max(u64::from(x.gpu.max(x.selected)));
            loads = loads.max(u64::from(x.loading()));
            if view.cumulative {
                let e = s.level_events_total[*c][l];
                copies = copies.max(e[0]).max(e[1]);
            }
        }
        most.insert(*c as usize, (copies as f64, loads as f64));
    }
    // The header.
    let mut y = y;
    p.text(MARGIN, y, "level", EM, dim);
    for (i, (_, short)) in classes.iter().enumerate() {
        let x0 = MARGIN + LEVEL_COLUMN + i as f64 * block;
        p.text(
            x0 + 14.0,
            y,
            &if view.cumulative {
                format!("{short} started / done")
            } else {
                format!("{short} drawn | held")
            },
            EM,
            dim,
        );
        let mut x = x0 + 14.0 + RESIDENCY_BAR + 6.0;
        let columns: &[(&str, f64)] = if view.cumulative {
            &CUMULATIVE_COLUMNS
        } else {
            &INSTANT_COLUMNS
        };
        for (name, w) in columns {
            x += w;
            p.text_right(x, y, name, EM - 1.0, dim);
        }
        if !view.cumulative {
            p.text(x + 8.0, y, "loads", EM - 1.0, dim);
        }
    }
    y += ROW;
    for l in (0..LEVELS).filter(|l| seen & (1 << l) != 0) {
        let floor = floor_at(s, l);
        p.text_right(
            MARGIN + LEVEL_COLUMN - 10.0,
            y,
            &format!("{l}{}", if floor { " F" } else { "" }),
            EM,
            if floor { dim } else { text },
        );
        for (i, (c, _)) in classes.iter().enumerate() {
            let x0 = MARGIN + LEVEL_COLUMN + i as f64 * block;
            let (copies, loads) = most[&(*c as usize)];
            if view.cumulative {
                level_cumulative(p, s, view, *c, l, x0, y, copies);
            } else {
                level_instant(p, s, *c, l, x0, y, copies, loads);
            }
        }
        y += ROW;
    }
    y + 8.0
}

#[allow(clippy::too_many_arguments)]
fn level_instant(
    p: &mut Painter<'_>,
    s: &BudgetSignals,
    c: Class,
    l: usize,
    x0: f64,
    y: f64,
    copies: f64,
    loads: f64,
) {
    let text = srgb(TEXT, 1.0);
    let x = LevelCell::of(s, c, l);
    let sev = level_severity(&x, floor_at(s, l));
    if sev != Severity::Ok {
        let mut tint = sev.colour();
        tint[3] = 0.16;
        p.rect(
            x0,
            y - 1.0,
            14.0 + RESIDENCY_BAR + 6.0 + 332.0 + 8.0 + LOADS_BAR,
            ROW - 1.0,
            tint,
        );
        p.marker(sev, x0 + 6.0, y + ROW / 2.0 - 2.0, 8.0);
    }
    let held = if sev == Severity::Waste {
        Severity::Waste.colour()
    } else {
        srgb(HELD, 1.0)
    };
    p.bar(
        x0 + 14.0,
        y + 3.0,
        RESIDENCY_BAR,
        9.0,
        &[
            (f64::from(x.drawn.min(x.gpu)), srgb(DRAWN, 1.0)),
            (f64::from(x.gpu.saturating_sub(x.drawn)), held),
        ],
        copies,
    );
    // The selected tiles not drawn: a tick where the selection ends.
    if x.selected > x.drawn {
        let tx = x0 + 14.0 + (f64::from(x.selected) / copies * RESIDENCY_BAR).min(RESIDENCY_BAR);
        p.rect(tx - 1.0, y + 1.0, 2.0, 13.0, Severity::Warn.colour());
    }
    let under_colour = match sev {
        Severity::Alert | Severity::Warn if x.under > 0 => sev.colour(),
        _ => text,
    };
    let values = [
        (n(u64::from(x.gpu)), text),
        (
            if x.bytes == 0 {
                "·".to_owned()
            } else {
                mib(x.bytes)
            },
            text,
        ),
        (n(u64::from(x.decoded)), text),
        (
            pair(x.drawn, x.selected),
            if x.drawn < x.selected {
                Severity::Warn.colour()
            } else {
                text
            },
        ),
        (n(u64::from(x.under)), under_colour),
        (pair(x.in_flight[0], x.in_flight[1]), text),
        (pair(x.queued[0], x.queued[1]), text),
    ];
    let mut cx = x0 + 14.0 + RESIDENCY_BAR + 6.0;
    for ((value, colour), (_, w)) in values.iter().zip(INSTANT_COLUMNS) {
        cx += w;
        p.text_right(cx, y, value, EM, *colour);
    }
    let mut pale = srgb(LOADS, 1.0);
    pale[3] = 0.5;
    let mut pale_q = srgb(QUEUED, 1.0);
    pale_q[3] = 0.5;
    p.bar(
        cx + 8.0,
        y + 3.0,
        LOADS_BAR,
        9.0,
        &[
            (f64::from(x.in_flight[0]), srgb(LOADS, 1.0)),
            (f64::from(x.in_flight[1]), pale),
            (f64::from(x.queued[0]), srgb(QUEUED, 1.0)),
            (f64::from(x.queued[1]), pale_q),
        ],
        loads,
    );
}

#[allow(clippy::too_many_arguments)]
fn level_cumulative(
    p: &mut Painter<'_>,
    s: &BudgetSignals,
    view: &BoardView<'_>,
    c: Class,
    l: usize,
    x0: f64,
    y: f64,
    copies: f64,
) {
    let text = srgb(TEXT, 1.0);
    let from = view.from.map(|(f, _)| f);
    let e = s.level_events_total[c][l];
    let e0 = from.map_or([0; 4], |f| f.level_events_total[c][l]);
    let ev = s.evictions_by_level_total[c][l]
        .saturating_sub(from.map_or(0, |f| f.evictions_by_level_total[c][l]));
    let d = |k: usize| e[k].saturating_sub(e0[k]);
    let mut started = srgb(LOADS, 1.0);
    started[3] = 0.5;
    p.bar(
        x0 + 14.0,
        y + 3.0,
        RESIDENCY_BAR,
        4.0,
        &[(d(0) as f64, started)],
        copies,
    );
    p.bar(
        x0 + 14.0,
        y + 8.0,
        RESIDENCY_BAR,
        4.0,
        &[(d(1) as f64, srgb(DRAWN, 1.0))],
        copies,
    );
    let mut cx = x0 + 14.0 + RESIDENCY_BAR + 6.0;
    for (v, (_, w)) in [d(0), d(1), d(2), ev, d(3)].iter().zip(CUMULATIVE_COLUMNS) {
        cx += w;
        p.text_right(cx, y, &n(*v), EM, text);
    }
}

/// The imagery layers: copies held and drawn as a bar, the table's numbers,
/// failures marked.
fn layers(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let suffix = if view.cumulative { "" } else { " (step)" };
    let columns = [
        ("gpu".to_owned(), 44.0),
        ("MiB".to_owned(), 52.0),
        ("drawn".to_owned(), 50.0),
        ("in flight".to_owned(), 62.0),
        (format!("store{suffix}"), 84.0),
        (format!("network{suffix}"), 100.0),
        (format!("RAM hits{suffix}"), 104.0),
        (format!("done{suffix}"), 84.0),
        (format!("failed{suffix}"), 86.0),
    ];
    const NAME: f64 = 170.0;
    let mut y = y;
    p.text(MARGIN, y, "layer", EM, dim);
    p.text(MARGIN + NAME, y, "drawn | held", EM, dim);
    let mut x = MARGIN + NAME + RESIDENCY_BAR + 6.0;
    for (name, w) in &columns {
        x += w;
        p.text_right(x, y, name, EM - 1.0, dim);
    }
    y += ROW;
    let l = &s.layers;
    let most = (0..LAYERS)
        .map(|i| f64::from(l.gpu_tiles[i]))
        .fold(1.0, f64::max);
    for i in (0..LAYERS).filter(|i| s.layers_seen & (1 << i) != 0) {
        let name = match (i, view.layers.get(i)) {
            (0, Some(n)) => format!("0 base: {n}"),
            (0, None) => "0 base".to_owned(),
            (_, Some(n)) => format!("{i} overlay: {n}"),
            (_, None) => format!("{i} overlay"),
        };
        let counts: [u64; 5] = if view.cumulative {
            let t = s.layer_loads_total[i];
            let t0 = view.from.map_or([0; 5], |(f, _)| f.layer_loads_total[i]);
            std::array::from_fn(|k| t[k].saturating_sub(t0[k]))
        } else {
            [
                l.requests[i][0],
                l.requests[i][1],
                l.ram_hits[i],
                l.completions[i],
                l.failures[i],
            ]
            .map(u64::from)
        };
        let failing = counts[4] > 0 && (!view.cumulative || l.failures[i] > 0);
        if failing {
            p.marker(Severity::Alert, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 8.0);
        }
        p.text(MARGIN + 14.0, y, &name, EM, text);
        p.bar(
            MARGIN + NAME,
            y + 3.0,
            RESIDENCY_BAR,
            9.0,
            &[
                (f64::from(l.drawn[i].min(l.gpu_tiles[i])), srgb(DRAWN, 1.0)),
                (
                    f64::from(l.gpu_tiles[i].saturating_sub(l.drawn[i])),
                    srgb(HELD, 1.0),
                ),
            ],
            most,
        );
        let mut values: Vec<(String, [f32; 4])> = vec![
            (n(u64::from(l.gpu_tiles[i])), text),
            (mib(l.gpu_bytes[i].0), text),
            (n(u64::from(l.drawn[i])), text),
            (n(u64::from(l.in_flight[i])), text),
        ];
        values.extend(counts.iter().enumerate().map(|(k, v)| {
            (
                n(*v),
                if k == 4 && *v > 0 {
                    Severity::Alert.colour()
                } else {
                    text
                },
            )
        }));
        let mut x = MARGIN + NAME + RESIDENCY_BAR + 6.0;
        for ((value, colour), (_, w)) in values.iter().zip(&columns) {
            x += w;
            p.text_right(x, y, value, EM, *colour);
        }
        y += ROW;
    }
    y
}

/// Budget signals for pictures and tests: a healthy step, and a stalled
/// one.
#[doc(hidden)]
pub mod fixtures {
    use tuile_core::budget::{BudgetSignals, Class};

    const MIB: u64 = 1024 * 1024;

    fn base() -> BudgetSignals {
        let mut s = BudgetSignals::default();
        s.floor_level = Some(2);
        s.floor_in = true;
        s.sse_scale_milli = 1000;
        s.decide_micros.count = 1300;
        s.decide_micros.sum = 1300 * 900;
        s.decide_work = 90_000;
        s.decide_work_total = 1300 * 110_000;
        for (c, target, hard) in [
            (Class::TerrainMesh, 1410, 2015),
            (Class::Drape, 1763, 2519),
            (Class::Tiles3dMesh, 823, 1176),
        ] {
            s.target_bytes[c] = target * MIB;
            s.hard_limit_bytes[c] = hard * MIB;
        }
        s.layers_seen = 1;
        s
    }

    /// One level: `gpu` copies, `drawn` drawn of `selected`, at `mib`.
    fn level(
        s: &mut BudgetSignals,
        c: Class,
        l: usize,
        gpu: u32,
        drawn: u32,
        selected: u32,
        mib: u64,
    ) {
        s.levels_seen[c] |= 1 << l;
        s.bytes_levels[c] |= 1 << l;
        s.levels.gpu_tiles[c][l] = gpu;
        s.levels.decoded_tiles[c][l] = gpu;
        s.levels.drawn[c][l] = drawn;
        s.levels.selected[c][l] = selected;
        s.bytes_by_level[c][l] = [mib * MIB / 10, 0];
        s.bytes[c][0] += mib * MIB / 10;
    }

    /// A healthy step: every selected tile drawn at its level, nothing
    /// under its need, the classes well under their targets.
    pub fn healthy() -> BudgetSignals {
        let mut s = base();
        for l in 0..=17 {
            let k = (l as u32 + 1) * 12;
            level(
                &mut s,
                Class::TerrainMesh,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k),
            );
            level(
                &mut s,
                Class::Drape,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k) * 3,
            );
        }
        s.protected_bytes[Class::TerrainMesh] = 60 * MIB;
        s.levels.in_flight[Class::Drape][17] = [3, 2];
        s.levels.queued[Class::Drape][17] = [0, 6];
        s.layers.gpu_tiles[0] = 2400;
        s.layers.gpu_bytes[0].0 = 680 * MIB;
        s.layers.drawn[0] = 1800;
        s.layers.in_flight[0] = 5;
        s.layers.requests[0] = [4, 1];
        s.layers.completions[0] = 5;
        s.layer_loads_total[0] = [900, 300, 120, 1180, 0];
        s.decoded_bytes = 1022 * MIB;
        s.in_flight[Class::Drape] = 5;
        s
    }

    /// The stall Laurent's captures show: imagery under its need at levels
    /// 15 to 17 with nothing in flight or queued, and 1522 drapes held at
    /// level 21 that nothing selects or draws.
    pub fn stalled() -> BudgetSignals {
        let mut s = base();
        for l in 0..=17 {
            let k = (l as u32 + 1) * 12;
            level(
                &mut s,
                Class::TerrainMesh,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k),
            );
            level(
                &mut s,
                Class::Drape,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k) * 3,
            );
        }
        for (l, under) in [(15, 80), (16, 70), (17, 59)] {
            s.levels.under_need[Class::Drape][l] = under;
        }
        s.under_need_tiles = [0, 209];
        level(&mut s, Class::Drape, 21, 1522, 0, 0, 1522 * 2);
        s.layers.gpu_tiles[0] = 3900;
        s.layers.gpu_bytes[0].0 = 1500 * MIB;
        s.layers.drawn[0] = 1800;
        s.layer_loads_total[0] = [900, 300, 120, 1180, 0];
        s.decoded_bytes = 2040 * MIB;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// The thresholds: under nine tenths of the target is fine, from it a
    /// warning, over the target (unprotected bytes) or the hard limit an
    /// alert. Protected bytes do not count against the target.
    #[test]
    fn a_class_is_coloured_by_its_target_and_limit() {
        let (t, h) = (1000 * MIB, 1500 * MIB);
        assert_eq!(gauge(800 * MIB, 0, t, h), Severity::Ok);
        assert_eq!(gauge(899 * MIB, 0, t, h), Severity::Ok);
        assert_eq!(gauge(900 * MIB, 0, t, h), Severity::Warn);
        assert_eq!(gauge(1000 * MIB, 0, t, h), Severity::Warn);
        assert_eq!(gauge(1001 * MIB, 0, t, h), Severity::Alert);
        assert_eq!(
            gauge(1200 * MIB, 400 * MIB, t, h),
            Severity::Ok,
            "protected"
        );
        assert_eq!(gauge(1501 * MIB, 1000 * MIB, t, h), Severity::Alert, "hard");
        assert_eq!(gauge(10, 0, t, u64::MAX), Severity::Ok, "no hard limit");
    }

    /// A level: a stall is under its need with nothing loading; under its
    /// need while loading, or stand-ins, a warning; many copies nothing
    /// selects, waste — but never the floor's.
    #[test]
    fn a_level_is_coloured_by_its_loads_and_its_need() {
        let base = LevelCell {
            gpu: 100,
            drawn: 40,
            selected: 40,
            ..LevelCell::default()
        };
        assert_eq!(level_severity(&base, false), Severity::Ok);
        let stall = LevelCell { under: 5, ..base };
        assert_eq!(level_severity(&stall, false), Severity::Alert);
        let loading = LevelCell {
            under: 5,
            queued: [0, 1],
            ..base
        };
        assert_eq!(level_severity(&loading, false), Severity::Warn);
        let stand_ins = LevelCell { drawn: 30, ..base };
        assert_eq!(level_severity(&stand_ins, false), Severity::Warn);
        let waste = LevelCell {
            gpu: 1522,
            drawn: 0,
            selected: 0,
            ..LevelCell::default()
        };
        assert_eq!(level_severity(&waste, false), Severity::Waste);
        assert_eq!(level_severity(&waste, true), Severity::Ok, "the floor");
        let few = LevelCell {
            gpu: WASTE_TILES - 1,
            ..waste
        };
        assert_eq!(level_severity(&few, false), Severity::Ok);
    }

    /// A healthy step has nothing to look at.
    #[test]
    fn a_healthy_budget_needs_no_attention() {
        assert_eq!(attention(&fixtures::healthy()), Vec::new());
    }

    /// The stall: imagery under its need with nothing in flight or queued
    /// is the first, red, item; the drapes held at level 21 that nothing
    /// draws follow as waste.
    #[test]
    fn a_stall_is_the_first_red_item() {
        let list = attention(&fixtures::stalled());
        let first = list.first().expect("an issue");
        assert_eq!(first.severity, Severity::Alert);
        assert_eq!(
            first.text,
            "imagery: 209 tiles under need, nothing in flight or queued — stalled"
        );
        assert!(
            list.iter().any(|i| i.severity == Severity::Waste
                && i.text == "level 21 imagery: 1522 resident, 0 drawn, none selected"),
            "{list:?}"
        );
        // One loading level elsewhere: no longer a stall of the class, but
        // the levels with nothing of their own loading still are.
        let mut s = fixtures::stalled();
        s.levels.in_flight[Class::Drape][14] = [2, 0];
        let list = attention(&s);
        assert!(list.iter().any(|i| i.severity == Severity::Warn
            && i.text.starts_with("imagery: 209 tiles under need, 2 loads")));
        assert!(list.iter().any(|i| i.severity == Severity::Alert
            && i.text == "level 15 imagery: 80 under need, nothing loading at this level"));
    }

    /// Ground missing, a class over its limit, pressure and failures are
    /// alerts.
    #[test]
    fn missing_ground_overruns_and_failures_are_alerts() {
        let mut s = fixtures::healthy();
        s.lost_tiles = 3;
        s.bytes[Class::Drape] = [2600 * MIB, 0];
        s.pressure_level = 2;
        s.layers.failures[0] = 4;
        let list = attention(&s);
        for text in [
            "ground: 3 tiles lost, 0 bare — ground missing",
            "imagery: 2600.0 MiB over its 2519.0 MiB hard limit",
            "memory pressure: critical",
            "layer 0: 4 loads failed this step",
        ] {
            assert!(
                list.iter()
                    .any(|i| i.severity == Severity::Alert && i.text == text),
                "{text}: {list:?}"
            );
        }
    }

    /// The decision's time: mean and maximum over the same publications,
    /// the first only setting where the sums start — so a session whose
    /// early steps were slow does not show a mean over its maximum.
    #[test]
    fn the_decide_time_covers_the_same_steps_for_its_mean_and_its_max() {
        let mut h = DecideHistory::default();
        let mut s = BudgetSignals::default();
        // A slow start: 1000 steps of 4 ms, the peak of the last half
        // second 2 ms — the window #278 showed, max under mean.
        s.decide_micros.count = 1000;
        s.decide_micros.sum = 4_000_000;
        s.decide_peak_micros = 2_000;
        h.note(&s);
        assert_eq!(h.summary(), None, "one publication: nothing to say");
        s.decide_micros.count = 1030;
        s.decide_micros.sum = 4_000_000 + 30 * 700;
        s.decide_peak_micros = 1_500;
        h.note(&s);
        h.note(&s);
        let d = h.summary().expect("two publications");
        assert_eq!(d.steps, 30);
        assert!((d.mean_ms - 0.7).abs() < 1e-9, "{d:?}");
        assert!((d.max_ms - 1.5).abs() < 1e-9);
        assert!(d.max_ms >= d.mean_ms);
    }

    /// The board draws every number of the tables: the healthy step's
    /// level 12 terrain (156 copies, 117 of 117 drawn) is written on it.
    #[test]
    fn the_board_writes_the_tables_numbers() {
        let s = fixtures::healthy();
        let mut cache = TextCache::default();
        let view = BoardView {
            cumulative: false,
            from: None,
            layers: &["Aerial".to_owned()],
            decide: None,
        };
        let d = draw(Some(&s), &view, &mut cache, 1.0);
        let texts: Vec<&str> = cache.masks.keys().map(|(t, _)| t.as_str()).collect();
        for t in [
            "156",
            "117/117",
            "0 base: Aerial",
            "nothing needs attention",
            "3/2",
        ] {
            assert!(texts.contains(&t), "{t} not drawn: {texts:?}");
        }
        assert!(d.size.0 > 900 && d.size.1 > 300, "{:?}", d.size);
        // And twice as large at twice the density.
        let d2 = draw(Some(&s), &view, &mut cache, 2.0);
        assert!(d2.size.0 >= d.size.0 * 2 - 2);
    }
}
