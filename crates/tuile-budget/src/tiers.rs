// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the tiles are, level by level: on the disk (a local tile store),
//! in RAM (the budget's decoded copies) and on the GPU (its resident
//! copies) — `tuile_tier_tiles{tier,class,level}` and
//! `tuile_tier_bytes{tier,class,level}`.
//!
//! The RAM and GPU tiers are the budget's own per-level counts
//! ([`LevelStats`](crate::stats::LevelStats), `bytes_by_level`). The disk
//! tier is what the session's loader says its local store holds
//! ([`DiskTiers`]), from the store's own ledger of what it inserted and
//! let go — never a scan of its files. A shell with no local store (a
//! browser) has no disk tier: [`DiskTiers::known`] is false, and nothing
//! of it is exported.

use crate::observe::{labels, BudgetSignals, Series};
use crate::stats::{LAYERS, LEVELS};
use crate::types::{Class, PerClass};

/// Tiles and bytes, by level: `[tiles, bytes]`.
pub type ByLevel = [[u64; 2]; LEVELS];

/// What a session's local store holds, by class and level, and by imagery
/// layer ([`layer_index`](crate::stats::layer_index)) and level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskTiers {
    /// Whether a local store reported at all.
    pub known: bool,
    pub classes: PerClass<ByLevel>,
    pub layers: [ByLevel; LAYERS],
    /// The levels of each class that have held tiles on the disk since
    /// the session started, one bit each: exported from their first, as
    /// the budget's per-level series are.
    pub seen: PerClass<u32>,
}

impl DiskTiers {
    /// Adds `by_level` to `class`, and to imagery layer `layer` if given.
    pub fn add(&mut self, class: Class, layer: Option<usize>, by_level: &ByLevel) {
        for (l, [n, b]) in by_level.iter().enumerate() {
            let c = &mut self.classes[class][l];
            c[0] += n;
            c[1] += b;
            if let Some(i) = layer.filter(|i| *i < LAYERS) {
                let x = &mut self.layers[i][l];
                x[0] += n;
                x[1] += b;
            }
        }
    }

    /// Marks the levels now held as seen, keeping those seen before.
    pub fn mark_seen(&mut self, before: &PerClass<u32>) {
        for c in Class::ALL {
            let mut seen = before[c];
            for (l, [n, _]) in self.classes[c].iter().enumerate() {
                if *n > 0 {
                    seen |= 1 << l;
                }
            }
            self.seen[c] = seen;
        }
    }
}

/// The three tiers.
pub const TIERS: [&str; 3] = ["disk", "ram", "gpu"];

/// The tiles and bytes of `tier` (0 disk, 1 RAM, 2 GPU) of `class` at
/// `level`, as `s` has them; none for the disk when no store reported.
pub fn tier(s: &BudgetSignals, tier: usize, class: Class, level: usize) -> Option<[u64; 2]> {
    match tier {
        0 => s.disk.known.then(|| s.disk.classes[class][level]),
        1 => Some([
            u64::from(s.levels.decoded_tiles[class][level]),
            s.levels.decoded[class][level].0,
        ]),
        _ => {
            let [drawable, transit] = s.bytes_by_level[class][level];
            Some([
                u64::from(s.levels.gpu_tiles[class][level]),
                drawable + transit,
            ])
        }
    }
}

/// `tier_tiles` and `tier_bytes`, by tier, class and level: every level a
/// tier of a class has held since the session started.
pub fn series(s: &BudgetSignals) -> Vec<Series> {
    let mut out = Vec::new();
    for c in Class::ALL {
        let seen = [
            if s.disk.known { s.disk.seen[c] } else { 0 },
            s.levels_seen[c],
            s.levels_seen[c] | s.bytes_levels[c],
        ];
        for (t, name) in TIERS.iter().enumerate() {
            for l in (0..LEVELS).filter(|l| seen[t] & (1 << l) != 0) {
                let Some([n, b]) = tier(s, t, c, l) else {
                    continue;
                };
                let at = vec![
                    ("tier", *name),
                    ("class", labels::class(c)),
                    ("level", labels::LEVEL[l]),
                ];
                out.push(("tier_tiles", at.clone(), n as f64));
                out.push(("tier_bytes", at, b as f64));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Bytes;

    fn value(out: &[Series], name: &str, tier: &str, class: &str, level: &str) -> Option<f64> {
        out.iter()
            .find(|(n, l, _)| {
                *n == name
                    && l.contains(&("tier", tier))
                    && l.contains(&("class", class))
                    && l.contains(&("level", level))
            })
            .map(|(_, _, v)| *v)
    }

    /// The three tiers of a level, from one source each: the disk from the
    /// store's report, RAM and GPU from the budget's own counts.
    #[test]
    fn a_level_has_its_three_tiers() {
        let mut s = BudgetSignals::default();
        s.levels_seen[Class::Drape] = 1 << 12;
        s.levels.decoded_tiles[Class::Drape][12] = 7;
        s.levels.decoded[Class::Drape][12] = Bytes(7_000);
        s.levels.gpu_tiles[Class::Drape][12] = 5;
        s.bytes_by_level[Class::Drape][12] = [4_000, 1_000];
        s.disk.known = true;
        s.disk.add(Class::Drape, Some(0), &{
            let mut b = ByLevel::default();
            b[12] = [30, 90_000];
            b[13] = [4, 12_000];
            b
        });
        s.disk.mark_seen(&PerClass::default());
        let out = series(&s);
        assert_eq!(value(&out, "tier_tiles", "disk", "drape", "12"), Some(30.0));
        assert_eq!(
            value(&out, "tier_bytes", "disk", "drape", "13"),
            Some(12_000.0)
        );
        assert_eq!(value(&out, "tier_tiles", "ram", "drape", "12"), Some(7.0));
        assert_eq!(
            value(&out, "tier_bytes", "ram", "drape", "12"),
            Some(7_000.0)
        );
        assert_eq!(value(&out, "tier_tiles", "gpu", "drape", "12"), Some(5.0));
        assert_eq!(
            value(&out, "tier_bytes", "gpu", "drape", "12"),
            Some(5_000.0)
        );
        assert_eq!(s.disk.layers[0][12], [30, 90_000], "by layer too");
        // A level left on the disk stays exported, at nought.
        let seen = s.disk.seen;
        s.disk.classes = PerClass::default();
        s.disk.mark_seen(&seen);
        assert_eq!(
            value(&series(&s), "tier_tiles", "disk", "drape", "13"),
            Some(0.0)
        );
    }

    /// No store, no disk tier: nothing exported for it.
    #[test]
    fn no_store_no_disk_series() {
        let mut s = BudgetSignals::default();
        s.disk.classes[Class::TerrainMesh][3] = [9, 9];
        s.disk.seen[Class::TerrainMesh] = 1 << 3;
        assert!(series(&s)
            .iter()
            .all(|(_, l, _)| !l.contains(&("tier", "disk"))));
    }
}
