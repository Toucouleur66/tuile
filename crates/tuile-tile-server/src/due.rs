// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! When a zone next has something to be done, read from its manifest alone.
//!
//! A store's upkeep — expired epochs dropped, runs of deltas merged, retired
//! archives deleted once their grace is over — used to be found by going
//! round every zone and looking. Each of those is decided by the manifest and
//! the clock, so the instant it falls due is known as soon as the manifest is
//! written: a planner keeps it, and looks at a zone then, and only then.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::manifest::Manifest;
use crate::store::StoreConfig;
use crate::{tiering, Layer};

/// What falls due in a zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// A retired archive whose grace ends: it may be deleted.
    Retired { key: String },
    /// An epoch whose tiles have all outlived the layer's expiry.
    Expires { epoch: String },
    /// A run of archives the tiering would merge.
    Merge { epoch: String },
}

/// When a zone next has something to be done, and what.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Due {
    /// The earliest instant something falls due; `None`: nothing ever will
    /// until the zone is written again.
    pub at: Option<SystemTime>,
    /// Everything that falls due, earliest or not, in the order found.
    pub why: Vec<(SystemTime, Why)>,
}

/// When `manifest`, a zone of `layer`, next has something to be done under
/// `config`, seen at `now`. What is due already is due at `now`.
pub fn due(layer: &Layer, manifest: &Manifest, config: &StoreConfig, now: SystemTime) -> Due {
    let mut why = Vec::new();
    for retired in &manifest.retired {
        let at = UNIX_EPOCH + Duration::from_secs(retired.retired) + config.retire_grace;
        why.push((at, Why::Retired { key: retired.key.clone() }));
    }
    let mut epochs: Vec<&str> = Vec::new();
    for archive in &manifest.archives {
        if !epochs.contains(&archive.epoch.as_str()) {
            epochs.push(&archive.epoch);
        }
    }
    for epoch in epochs {
        if let Some(at) = layer.expires_at(epoch) {
            why.push((at, Why::Expires { epoch: epoch.to_string() }));
        }
        if !layer.is_expired(epoch, now) && tiering::select(&manifest.archives, epoch, config.tiering).is_some() {
            why.push((now, Why::Merge { epoch: epoch.to_string() }));
        }
    }
    let at = why.iter().map(|(at, _)| *at).min().map(|at| at.max(now));
    Due { at, why }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ArchiveRef, Retired};
    use crate::{Compression, Grid, TileType};

    const DAY: Duration = Duration::from_secs(86_400);

    fn layer(expiry: Option<Duration>) -> Layer {
        Layer {
            name: "imagery".into(),
            grid: Grid::WebMercator,
            tile_type: TileType::Jpeg,
            tile_compression: Compression::None,
            zone_level: 10,
            expiry,
            content_type: "image/jpeg".into(),
        }
    }

    fn archive(key: &str, epoch: &str, bytes: u64) -> ArchiveRef {
        ArchiveRef {
            key: key.into(),
            epoch: epoch.into(),
            created: 0,
            tiles: 1,
            bytes,
            oldest_fetch_ms: None,
            newest_fetch_ms: None,
        }
    }

    /// 2026-10-13 12:00 UTC.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_791_892_800)
    }

    #[test]
    fn a_zone_with_one_archive_of_a_durable_layer_has_nothing_ever_due() {
        let m = Manifest { generation: 1, archives: vec![archive("a", "durable", 1000)], retired: vec![] };
        assert_eq!(due(&layer(None), &m, &StoreConfig::default(), now()), Due::default());
    }

    #[test]
    fn a_retired_archive_is_due_when_its_grace_ends() {
        let config = StoreConfig::default();
        let retired_at = now() - Duration::from_secs(60);
        let m = Manifest {
            generation: 3,
            archives: vec![archive("b", "durable", 1000)],
            retired: vec![Retired { key: "a".into(), retired: retired_at.duration_since(UNIX_EPOCH).unwrap().as_secs() }],
        };
        let d = due(&layer(None), &m, &config, now());
        assert_eq!(d.at, Some(retired_at + config.retire_grace));
        assert_eq!(d.why, [(retired_at + config.retire_grace, Why::Retired { key: "a".into() })]);
        // With an epoch that expires months later, the earliest is the grace.
        let expiring = Manifest { archives: vec![archive("b", "202610", 1000)], ..m.clone() };
        let d = due(&layer(Some(152 * DAY)), &expiring, &config, now());
        assert_eq!(d.at, Some(retired_at + config.retire_grace));
        assert_eq!(d.why.len(), 2);
        // Long past its grace: due now, not in the past.
        let later = now() + DAY;
        assert_eq!(due(&layer(None), &m, &config, later).at, Some(later));
    }

    #[test]
    fn an_epoch_is_due_when_it_has_outlived_the_layer_s_expiry_from_its_end() {
        let expiry = 152 * DAY;
        let m = Manifest { generation: 1, archives: vec![archive("a", "202610", 1000)], retired: vec![] };
        let d = due(&layer(Some(expiry)), &m, &StoreConfig::default(), now());
        // October 2026 ends on 1 November 2026 00:00 UTC.
        let end = UNIX_EPOCH + Duration::from_secs(1_793_491_200);
        assert_eq!(d.at, Some(end + expiry));
        assert_eq!(d.why, [(end + expiry, Why::Expires { epoch: "202610".into() })]);
    }

    #[test]
    fn a_run_the_tiering_would_merge_is_due_now_and_the_earliest_wins() {
        let config = StoreConfig::default();
        let fanout = config.tiering.fanout.max(2);
        let archives = (0..fanout).map(|i| archive(&format!("d{i}"), "durable", 1000)).collect();
        let m = Manifest { generation: 9, archives, retired: vec![] };
        let d = due(&layer(None), &m, &config, now());
        assert_eq!(d.at, Some(now()));
        assert_eq!(d.why, [(now(), Why::Merge { epoch: "durable".into() })]);
        // An epoch already expired is dropped, not merged.
        let old = Manifest {
            generation: 9,
            archives: (0..fanout).map(|i| archive(&format!("d{i}"), "202501", 1000)).collect(),
            retired: vec![],
        };
        let d = due(&layer(Some(DAY)), &old, &config, now());
        assert!(d.why.iter().all(|(_, w)| !matches!(w, Why::Merge { .. })), "{:?}", d.why);
        assert_eq!(d.at, Some(now()), "expired long ago: due now");
    }
}
