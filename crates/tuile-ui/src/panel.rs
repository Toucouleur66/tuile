// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A panel of text over the view: a table of rows and columns on a dark,
//! translucent background, laid out and rasterised here into **one** coverage
//! mask, so a backend draws it as one quad whatever the number of cells.
//!
//! A row of one cell is a heading and spans the panel; in the others the
//! first column is aligned left and the rest right, each as wide as its
//! widest cell — numbers line up in a proportional font that way.

use crate::text::{Font, Label, Mask};
use crate::{Mesh, Vertex};

/// The background, linear: a dark grey the picture shows through a little.
pub const BACKGROUND: [f32; 4] = [0.008, 0.008, 0.008, 0.82];
/// The text's colour, linear.
pub const TEXT: [f32; 4] = [0.80, 0.80, 0.80, 1.0];

/// A panel's proportions, in points (device pixels at scale 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Panel {
    /// The em of the text.
    pub text_size: f64,
    /// The room around the table.
    pub padding: f64,
    /// The room between two columns.
    pub gap: f64,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            text_size: 11.0,
            padding: 8.0,
            gap: 10.0,
        }
    }
}

/// A panel as drawn on one frame: its background, and its text as one
/// label.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelLayout {
    /// Its background, in device pixels.
    pub background: Mesh,
    /// Its text. `None` for a panel with nothing in it.
    pub label: Option<Label>,
    /// Its size in device pixels.
    pub size: (u32, u32),
}

impl Panel {
    /// The table `rows` with its top-left corner at `at`, in device pixels,
    /// at the display density `scale`, clipped to `frame`.
    pub fn layout(
        &self,
        font: &Font,
        rows: &[Vec<String>],
        at: (f64, f64),
        frame: (u32, u32),
        scale: f64,
    ) -> PanelLayout {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let size = (self.text_size * scale) as f32;
        let (pad, gap) = (self.padding * scale, self.gap * scale);
        let line = f64::from(font.line_height(size));
        // Each column as wide as its widest cell, headings left out.
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0.0f64; columns];
        for row in rows.iter().filter(|r| r.len() > 1) {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = w.max(f64::from(font.width(cell, size)));
            }
        }
        let table: f64 = widths.iter().sum::<f64>() + gap * (columns.saturating_sub(1) as f64);
        let headings = rows
            .iter()
            .filter(|r| r.len() == 1)
            .map(|r| f64::from(font.width(&r[0], size)))
            .fold(0.0, f64::max);
        let inner = (
            table.max(headings).ceil() as u32 + 2,
            (line * rows.len() as f64) as u32,
        );
        let room = |extent: u32, from: f64| (f64::from(extent) - from - 2.0 * pad).max(0.0) as u32;
        let width = inner.0.min(room(frame.0, at.0));
        let height = inner.1.min(room(frame.1, at.1));
        let mut mask = Mask {
            width,
            height,
            coverage: vec![0; (width * height) as usize],
        };
        for (i, row) in rows.iter().enumerate() {
            let y = (line * i as f64) as i64;
            if row.len() == 1 {
                blit(&mut mask, &font.rasterize(&row[0], size), 0, y);
                continue;
            }
            let mut x = 0.0;
            for (j, cell) in row.iter().enumerate() {
                let glyphs = font.rasterize(cell, size);
                let left = if j == 0 {
                    x
                } else {
                    x + widths[j] - f64::from(glyphs.width)
                };
                blit(&mut mask, &glyphs, left.round() as i64, y);
                x += widths[j] + gap;
            }
        }
        let outer = (f64::from(width) + 2.0 * pad, f64::from(height) + 2.0 * pad);
        let mut background = Mesh::new();
        let (x0, y0, x1, y1) = (at.0, at.1, at.0 + outer.0, at.1 + outer.1);
        for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x0, y1), (x1, y0), (x1, y1)] {
            background.push(Vertex::new(x, y, BACKGROUND));
        }
        let label = (width > 0 && height > 0).then(|| Label {
            mask,
            position: [(at.0 + pad).round() as f32, (at.1 + pad).round() as f32],
            color: TEXT,
        });
        PanelLayout {
            background,
            label,
            size: (outer.0.ceil() as u32, outer.1.ceil() as u32),
        }
    }
}

/// `from` into `into` with its top-left corner at `(x, y)`, clipped, each
/// pixel the more inked of the two.
fn blit(into: &mut Mask, from: &Mask, x: i64, y: i64) {
    for row in 0..i64::from(from.height) {
        let ty = y + row;
        if ty < 0 || ty >= i64::from(into.height) {
            continue;
        }
        for col in 0..i64::from(from.width) {
            let tx = x + col;
            if tx < 0 || tx >= i64::from(into.width) {
                continue;
            }
            let c = from.coverage[(row * i64::from(from.width) + col) as usize];
            let t = &mut into.coverage[(ty * i64::from(into.width) + tx) as usize];
            *t = (*t).max(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(cells: &[&[&str]]) -> Vec<Vec<String>> {
        cells
            .iter()
            .map(|r| r.iter().map(|c| (*c).to_owned()).collect())
            .collect()
    }

    /// The table is one mask, as tall as its rows, inked; its background
    /// covers it with the padding around.
    #[test]
    fn a_table_is_one_mask_over_its_background() {
        let font = Font::bundled();
        let table = rows(&[
            &["Budget"],
            &["level", "gpu", "bytes"],
            &["12", "3", "1.5 MiB"],
        ]);
        let p = Panel::default().layout(font, &table, (10.0, 20.0), (1280, 800), 1.0);
        let label = p.label.expect("a label");
        assert_eq!(label.mask.height, font.line_height(11.0) * 3);
        assert!(label.mask.coverage.iter().any(|c| *c > 0), "inked");
        assert_eq!(label.position, [18.0, 28.0]);
        assert_eq!(p.background.len(), 6);
        assert_eq!(p.size.1, label.mask.height + 16);
    }

    /// Numbers are aligned right: a short number ends where a long one
    /// does.
    #[test]
    fn the_numbers_line_up_on_the_right() {
        let font = Font::bundled();
        let table = rows(&[&["a", "1"], &["b", "10000"]]);
        let p = Panel::default().layout(font, &table, (0.0, 0.0), (1280, 800), 1.0);
        let m = p.label.expect("a label").mask;
        let line = font.line_height(11.0) as usize;
        let right = |band: usize| {
            (0..m.width as usize)
                .rev()
                .find(|&x| {
                    (band * line..(band + 1) * line)
                        .any(|y| m.coverage[y * m.width as usize + x] > 0)
                })
                .expect("inked")
        };
        assert!(
            right(0).abs_diff(right(1)) <= 2,
            "{} {}",
            right(0),
            right(1)
        );
    }

    /// A frame too small keeps the panel inside it.
    #[test]
    fn a_small_frame_clips_the_panel() {
        let font = Font::bundled();
        let table = rows(&[&["a very long heading that will not fit at all"]]);
        let p = Panel::default().layout(font, &table, (0.0, 0.0), (100, 100), 1.0);
        assert!(p.label.expect("a label").mask.width <= 84);
    }
}
