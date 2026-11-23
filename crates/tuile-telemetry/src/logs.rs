// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The graphics budget's decisions as OTLP log records
//! (`docs/explorations/budget-progressive-coarse-preload.md` §7): one record
//! per preload started and per GPU copy evicted, with its score's terms as
//! attributes, and a picture of the held set — the copies the release order
//! takes next and those it keeps longest — so that Grafana joins the
//! metrics (by level and distance band) and the logs (by tile).
//!
//! Rate-limited at the source (`tuile_core::metrics::SCORE_RECORDS_KEPT` a
//! push), the drops counted in a record of their own. A tile's address is
//! an attribute of a log record here — it reveals the place viewed, and is
//! sent, by Laurent's decision (2026-11-23), only when a collector endpoint
//! is configured — never a metric label: the metrics' closed label sets
//! stay as they are (`crate::resource::ALLOWED_LABELS`).

use serde_json::{json, Value as Json};
use tuile_core::budget::score::{HeldTile, ScoreEvent, Terms, DISTANCE_BANDS, DISTANCE_BAND_LABELS};
use tuile_core::budget::{observe::labels, ResourceId};
use tuile_core::metrics::ScoreRecords;

use crate::resource::Resource;

/// The tile of `r` as `level/x/y`, and the resource's kind.
fn tile(r: ResourceId) -> (String, String) {
    let at = |t: tuile_core::budget::TileId| {
        let (l, x, y) = t.payload().terrain_coord();
        format!("{l}/{x}/{y}")
    };
    match r {
        ResourceId::Mesh(t) => (at(t), "mesh".to_owned()),
        ResourceId::Drape { tile, layer, level } => {
            (at(tile), format!("drape/{}/{level}", layer.0))
        }
    }
}

fn s(k: &str, v: &str) -> Json {
    json!({ "key": k, "value": { "stringValue": v } })
}

fn i(k: &str, v: u64) -> Json {
    json!({ "key": k, "value": { "intValue": v.to_string() } })
}

fn terms(out: &mut Vec<Json>, t: &Terms) {
    out.push(i("a", u64::from(t.a)));
    out.push(i("d", u64::from(t.d)));
    out.push(i("p", u64::from(t.p)));
    out.push(i("r_us", t.r_us));
    out.push(i("bytes", t.bytes));
    out.push(i("score", t.score));
}

fn band(b: u8) -> &'static str {
    DISTANCE_BAND_LABELS[usize::from(b).min(DISTANCE_BANDS - 1)]
}

fn record(now: &str, event: &str, attributes: Vec<Json>) -> Json {
    json!({
        "timeUnixNano": now,
        "observedTimeUnixNano": now,
        "severityNumber": 9,
        "severityText": "INFO",
        "body": { "stringValue": event },
        "attributes": attributes,
    })
}

/// The attributes of one decision.
pub fn decision_attributes(e: &ScoreEvent) -> Vec<Json> {
    let (t, kind) = tile(e.resource);
    let mut a = vec![
        s("event", "tuile.budget.decision"),
        s("tile", &t),
        s("resource", &kind),
        s("class", labels::class(e.class)),
        i("level", u64::from(e.level)),
        s("distance", band(e.band)),
        s("side", e.side.label()),
        s("reason", e.reason.label()),
    ];
    terms(&mut a, &e.terms);
    a
}

/// The attributes of one held copy of the picture, at `end` (`lowest` or
/// `highest`) and `rank` from it.
pub fn held_attributes(h: &HeldTile, end: &str, rank: usize, step: u64) -> Vec<Json> {
    let (t, kind) = tile(h.resource);
    let mut a = vec![
        s("event", "tuile.budget.held"),
        s("tile", &t),
        s("resource", &kind),
        s("class", labels::class(h.class)),
        i("level", u64::from(h.level)),
        s("distance", band(h.band)),
        s("side", "held"),
        s("state", h.state.label()),
        s("end", end),
        i("rank", rank as u64),
        i("step", step),
    ];
    terms(&mut a, &h.terms);
    a
}

/// `records` as one `ExportLogsServiceRequest` in OTLP/JSON, `None` when
/// there is nothing to say.
pub fn encode(resource: &Resource, records: &ScoreRecords, now_ns: u64) -> Option<String> {
    let now = now_ns.to_string();
    let mut logs: Vec<Json> = records
        .decisions
        .iter()
        .map(|e| record(&now, "tuile.budget.decision", decision_attributes(e)))
        .collect();
    if records.dropped > 0 {
        logs.push(record(
            &now,
            "tuile.budget.decisions_dropped",
            vec![i("dropped", records.dropped)],
        ));
    }
    if let Some(h) = &records.held {
        for (end, list) in [("lowest", &h.lowest), ("highest", &h.highest)] {
            for (rank, t) in list.iter().enumerate() {
                logs.push(record(
                    &now,
                    "tuile.budget.held",
                    held_attributes(t, end, rank, h.step),
                ));
            }
        }
    }
    if logs.is_empty() {
        return None;
    }
    let attributes: Vec<Json> = resource.iter().map(|(k, v)| s(k, v)).collect();
    Some(
        json!({
            "resourceLogs": [{
                "resource": { "attributes": attributes },
                "scopeLogs": [{
                    "scope": { "name": crate::otlp::SCOPE, "version": env!("CARGO_PKG_VERSION") },
                    "logRecords": logs,
                }],
            }],
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::budget::score::{Reason, Side};
    use tuile_core::budget::{Class, TileId};

    /// The record carries the decision's own numbers: the same the window
    /// prints and the metrics sum.
    #[test]
    fn a_decision_record_carries_its_terms() {
        let e = ScoreEvent {
            resource: ResourceId::Mesh(TileId::from_terrain(12, 2049, 1400)),
            class: Class::TerrainMesh,
            level: 12,
            band: 2,
            side: Side::Evict,
            reason: Reason::Sweep,
            terms: Terms::new(25_600, 16_000, 12_800, 150_000, 100 << 10, 256 << 10),
        };
        let records = ScoreRecords {
            decisions: vec![e],
            dropped: 3,
            held: None,
        };
        let body = encode(&Resource::default(), &records, 7).unwrap();
        let v: Json = serde_json::from_str(&body).unwrap();
        let logs = &v["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
        assert_eq!(logs.as_array().unwrap().len(), 2);
        let attrs = logs[0]["attributes"].as_array().unwrap();
        let get = |k: &str| {
            attrs
                .iter()
                .find(|a| a["key"] == k)
                .map(|a| a["value"].clone())
                .unwrap()
        };
        assert_eq!(get("tile")["stringValue"], "12/2049/1400");
        assert_eq!(get("distance")["stringValue"], "2");
        assert_eq!(get("side")["stringValue"], "evict");
        assert_eq!(get("score")["intValue"], e.terms.score.to_string());
        assert_eq!(get("a")["intValue"], "25600");
        assert!(encode(&Resource::default(), &ScoreRecords::default(), 7).is_none());
    }
}
