// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the store holds of the tiles, by group and level, kept as it
//! changes: the disk tier of the budget window and of `tuile_tier_*`
//! (`ContentStore::stored`).
//!
//! The disk tier is indexed by hash and keeps no key a caller can list, and
//! a scan of its files is out of the question every half second. So the
//! store keeps its own ledger, entry by entry, from its own events:
//!
//! - a value **put** is counted at its key's group and level, with its
//!   size; put again, its size is the new one's; an absence remembered
//!   (`tuile_core::storage::ABSENT`) is not a tile, and puts out a tile it
//!   replaces;
//! - a value the store **lets go** of — expired and removed, evicted from
//!   the memory tier, its disk block reclaimed — is *suspect*: it may be on
//!   the disk still (a memory eviction writes it there; a reclaimed block's
//!   entry may have a newer copy elsewhere), so it is asked of the store's
//!   index ([`Ledger::settle`]) and leaves the ledger once the index says
//!   it is gone — but only once the store's writes queued when it was let
//!   go of have landed (a *barrier*, [`Ledger::begin_barrier`]): an entry
//!   the memory tier evicts while its write is still in the queue is not in
//!   the index yet, and is not gone;
//! - the ledger is written next to the store when the store closes, and
//!   read back when it opens again, each entry kept only if the index
//!   still holds it.
//!
//! What it cannot see: an entry dropped by a reclaim that stopped half-way
//! on an I/O error (foyer drops those from its index at their next read),
//! and what a process killed before closing put since the last close.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Mutex;

use tuile_core::budget::{ByLevel, LEVELS};
use tuile_core::storage::tile_key_level;

/// The file the ledger is kept in, in the store's directory.
pub(crate) const FILE: &str = "tuile-ledger.tsv";

/// Settles a suspect at most this many times before it is let go of as
/// held: an entry still in the index after that has a copy somewhere.
const PATIENCE: u8 = 8;

#[derive(Default)]
struct Inner {
    /// The tiles counted: key → (hash, bytes).
    keys: HashMap<String, (u64, u64)>,
    /// Their keys, by hash: what a reclaimed block names.
    by_hash: HashMap<u64, String>,
    /// The counts, by group: `[tiles, bytes]` by level.
    groups: HashMap<String, ByLevel>,
    /// Keys let go of by the memory tier, and hashes by the disk, to ask
    /// the index about; with how many times they were asked.
    suspect_keys: Vec<Suspect<String>>,
    suspect_hashes: Vec<Suspect<u64>>,
    /// Barriers begun, and the highest one done: a barrier is the store's
    /// queued writes all landed.
    begun: u64,
    done: u64,
}

/// A key or hash to ask the index about once a barrier begun after it was
/// let go of is done.
struct Suspect<T> {
    what: T,
    /// The barriers begun when it was let go of.
    seen: u64,
    /// How many times the index was asked.
    tries: u8,
}

/// See the module.
#[derive(Default)]
pub(crate) struct Ledger(Mutex<Inner>);

impl Inner {
    fn count(&mut self, key: &str, bytes: i128, tiles: i128) {
        let Some((group, level)) = tile_key_level(key) else {
            return;
        };
        let by = self.groups.entry(group.to_owned()).or_default();
        let [n, b] = &mut by[level];
        *n = u64::try_from(i128::from(*n) + tiles).unwrap_or(0);
        *b = u64::try_from(i128::from(*b) + bytes).unwrap_or(0);
    }

    fn forget(&mut self, key: &str) {
        if let Some((hash, bytes)) = self.keys.remove(key) {
            if self.by_hash.get(&hash).is_some_and(|k| k == key) {
                self.by_hash.remove(&hash);
            }
            self.count(key, -i128::from(bytes), -1);
        }
    }
}

impl Ledger {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `key`, hashing to `hash`, put with `bytes` bytes, or as an absence.
    pub(crate) fn put(&self, key: &str, hash: u64, bytes: u64, absent: bool) {
        let mut i = self.inner();
        if absent {
            i.forget(key);
            return;
        }
        if tile_key_level(key).is_none() {
            return;
        }
        match i.keys.insert(key.to_owned(), (hash, bytes)) {
            Some((_, was)) => i.count(key, i128::from(bytes) - i128::from(was), 0),
            None => i.count(key, i128::from(bytes), 1),
        }
        i.by_hash.insert(hash, key.to_owned());
    }

    /// Takes in `other`'s entries: a ledger loaded once the store opened,
    /// into the one its listeners already hold.
    pub(crate) fn take(&self, other: Ledger) {
        let other = other
            .0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut i = self.inner();
        for (key, (hash, bytes)) in other.keys {
            if !i.keys.contains_key(&key) {
                i.keys.insert(key.clone(), (hash, bytes));
                i.by_hash.insert(hash, key.clone());
                i.count(&key, i128::from(bytes), 1);
            }
        }
    }

    /// `key` let go of by the memory tier: on the disk still, or not.
    pub(crate) fn suspect_key(&self, key: &str) {
        let mut i = self.inner();
        if i.keys.contains_key(key) {
            let seen = i.begun;
            i.suspect_keys.push(Suspect {
                what: key.to_owned(),
                seen,
                tries: 0,
            });
        }
    }

    /// The entry hashing to `hash` reclaimed from the disk.
    pub(crate) fn suspect_hash(&self, hash: u64) {
        let mut i = self.inner();
        if i.by_hash.contains_key(&hash) {
            let seen = i.begun;
            i.suspect_hashes.push(Suspect {
                what: hash,
                seen,
                tries: 0,
            });
        }
    }

    /// Begins a barrier if a suspect waits for one: the caller waits for
    /// the store's queued writes to land, then calls [`Ledger::barrier_done`]
    /// with the number returned.
    pub(crate) fn begin_barrier(&self) -> Option<u64> {
        let mut i = self.inner();
        let begun = i.begun;
        let waiting = i.suspect_keys.iter().any(|s| s.seen >= begun)
            || i.suspect_hashes.iter().any(|s| s.seen >= begun);
        waiting.then(|| {
            i.begun += 1;
            i.begun
        })
    }

    /// The writes queued when barrier `n` began have landed.
    pub(crate) fn barrier_done(&self, n: u64) {
        let mut i = self.inner();
        i.done = i.done.max(n);
    }

    /// Asks `holds` — the store's index — about every suspect a barrier
    /// begun after it was let go of has passed: those it no longer holds
    /// leave the ledger. The others wait.
    pub(crate) fn settle(&self, holds: impl Fn(&str) -> bool) {
        let mut i = self.inner();
        let done = i.done;
        let (ready, mut waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut i.suspect_keys)
            .into_iter()
            .partition(|s| s.seen < done);
        for mut s in ready {
            if !holds(&s.what) {
                i.forget(&s.what);
            } else if s.tries + 1 < PATIENCE {
                s.tries += 1;
                waiting.push(s);
            }
        }
        i.suspect_keys = waiting;
        let (ready, mut waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut i.suspect_hashes)
            .into_iter()
            .partition(|s| s.seen < done);
        for mut s in ready {
            let Some(key) = i.by_hash.get(&s.what).cloned() else {
                continue;
            };
            if !holds(&key) {
                i.forget(&key);
            } else if s.tries + 1 < PATIENCE {
                s.tries += 1;
                waiting.push(s);
            }
        }
        i.suspect_hashes = waiting;
    }

    /// `group`'s tiles and bytes by level, added into `out`.
    pub(crate) fn add_group(&self, group: &str, out: &mut ByLevel) {
        let i = self.inner();
        if let Some(by) = i.groups.get(group) {
            for l in 0..LEVELS {
                out[l][0] += by[l][0];
                out[l][1] += by[l][1];
            }
        }
    }

    /// How many tiles are counted.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner().keys.len()
    }

    /// Writes the ledger into `dir`, by way of a file renamed over the
    /// last: a ledger half-written is never read.
    pub(crate) fn save(&self, dir: &Path) -> std::io::Result<()> {
        let tmp = dir.join(format!("{FILE}.tmp"));
        {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            let i = self.inner();
            let mut keys: Vec<(&String, &(u64, u64))> = i.keys.iter().collect();
            keys.sort();
            for (key, (_, bytes)) in keys {
                writeln!(out, "{key}\t{bytes}")?;
            }
            out.flush()?;
        }
        std::fs::rename(tmp, dir.join(FILE))
    }

    /// The ledger written into `dir`, each entry kept only if `holds` says
    /// the store still has it; `hash` hashes a key as the store does.
    pub(crate) fn load(
        dir: &Path,
        holds: impl Fn(&str) -> bool,
        hash: impl Fn(&str) -> u64,
    ) -> Self {
        let ledger = Self::default();
        let Ok(file) = std::fs::File::open(dir.join(FILE)) else {
            return ledger;
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Some((key, bytes)) = line.split_once('\t') else {
                continue;
            };
            let Ok(bytes) = bytes.parse::<u64>() else {
                continue;
            };
            if holds(key) {
                ledger.put(key, hash(key), bytes, false);
            }
        }
        ledger
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(l: &Ledger, g: &str) -> ByLevel {
        let mut out = ByLevel::default();
        l.add_group(g, &mut out);
        out
    }

    /// The queued writes landed: every suspect so far may be asked.
    fn landed(l: &Ledger) {
        if let Some(n) = l.begin_barrier() {
            l.barrier_done(n);
        }
    }

    /// A put counts at its group and level; put again, its new size; an
    /// absence and a key of another shape are not tiles.
    #[test]
    fn puts_are_counted_by_group_and_level() {
        let l = Ledger::default();
        l.put("img/a/12/1/2", 1, 100, false);
        l.put("img/a/12/1/3", 2, 50, false);
        l.put("img/a/13/2/4", 3, 10, false);
        l.put("mesh/t/12/1/2", 4, 7, false);
        l.put("scheme/a", 5, 9, false);
        l.put("img/a/14/0/0", 6, 1, true);
        assert_eq!(group(&l, "img/a")[12], [2, 150]);
        assert_eq!(group(&l, "img/a")[13], [1, 10]);
        assert_eq!(group(&l, "img/a")[14], [0, 0]);
        assert_eq!(group(&l, "mesh/t")[12], [1, 7]);
        l.put("img/a/12/1/2", 1, 30, false);
        assert_eq!(group(&l, "img/a")[12], [2, 80], "put again: its new size");
        l.put("img/a/12/1/2", 1, 0, true);
        assert_eq!(
            group(&l, "img/a")[12],
            [1, 50],
            "an absence puts the tile out"
        );
    }

    /// A tile let go of leaves the ledger once the index says it is gone —
    /// a memory eviction that wrote it to the disk does not.
    #[test]
    fn a_tile_let_go_of_leaves_once_the_index_says_so() {
        let l = Ledger::default();
        l.put("img/a/12/1/2", 1, 100, false);
        l.put("img/a/12/1/3", 2, 50, false);
        l.suspect_key("img/a/12/1/2");
        landed(&l);
        l.settle(|_| true);
        assert_eq!(group(&l, "img/a")[12], [2, 150], "still held: on the disk");
        l.suspect_hash(2);
        landed(&l);
        l.settle(|k| k != "img/a/12/1/3");
        assert_eq!(group(&l, "img/a")[12], [1, 100], "reclaimed from the disk");
        landed(&l);
        l.settle(|_| false);
        assert_eq!(
            group(&l, "img/a")[12],
            [0, 0],
            "the memory tier's suspect, gone at last"
        );
        assert_eq!(l.len(), 0);
    }

    /// A suspect is not asked about before the writes queued when it was
    /// let go of have landed: the index does not hold what is still queued.
    #[test]
    fn a_suspect_waits_for_the_queued_writes() {
        let l = Ledger::default();
        l.put("img/a/12/1/2", 1, 100, false);
        l.suspect_key("img/a/12/1/2");
        let n = l.begin_barrier().expect("a suspect waits");
        // Let go of after the barrier began: it needs the next one.
        l.put("img/a/12/1/3", 2, 50, false);
        l.suspect_key("img/a/12/1/3");
        l.settle(|_| false);
        assert_eq!(group(&l, "img/a")[12], [2, 150], "no barrier is done");
        l.barrier_done(n);
        l.settle(|_| false);
        assert_eq!(group(&l, "img/a")[12], [1, 50], "only the first has passed");
        landed(&l);
        l.settle(|_| false);
        assert_eq!(group(&l, "img/a")[12], [0, 0]);
    }

    /// Written and read back, the ledger keeps what the index still holds.
    #[test]
    fn the_ledger_is_kept_between_runs() {
        let dir = tempfile::tempdir().expect("dir");
        let l = Ledger::default();
        l.put("img/a/12/1/2", 1, 100, false);
        l.put("img/a/12/1/3", 2, 50, false);
        l.save(dir.path()).expect("saved");
        let back = Ledger::load(dir.path(), |k| k == "img/a/12/1/2", |_| 9);
        assert_eq!(group(&back, "img/a")[12], [1, 100]);
    }
}
