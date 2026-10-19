// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The geometry loop's own instruments, read back after a real session.
//!
//! Its own test binary, and one test in it, on purpose: the metrics are one
//! instance per process, and every other session in the unit tests would add
//! to them while this one is counting. Here nothing else runs, so the counts
//! can be compared for equality rather than bounded.
//!
//! What it pins down is the loop **as it is**: every arrival is waited on
//! once, and arrivals that are ready together share one pass — a tile and its
//! siblings answered from disk land on the same turn, so `arrivals_per_pass`
//! exceeds one and `traversals` falls below the arrivals. Before arrivals were
//! coalesced each one bought its own pass (1/1, and traversals = arrivals + 1);
//! this test is where that change announced itself.

use std::future::Future;
use std::sync::Arc;
use std::task::Poll;

use glam::{dvec2, dvec3};
use tuile_core::fetch::FsFetcher;
use tuile_core::metrics::metrics;
use tuile_core::{
    in_process, ClientMessage, Config, GeometryStream, ServerMessage, Tileset, ViewState,
};
use url::Url;

/// One triangle, as a glb: the least content that decodes.
fn glb() -> Vec<u8> {
    let positions: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0]];
    let mut bin = Vec::new();
    for c in positions.iter().flatten() {
        bin.extend_from_slice(&c.to_le_bytes());
    }
    let idx_offset = bin.len();
    for i in [0u16, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let json = serde_json::json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 }, "indices": 1 }]}],
        "buffers": [{ "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": idx_offset, "target": 34962 },
            { "buffer": 0, "byteOffset": idx_offset, "byteLength": 6, "target": 34963 }
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
              "min": [0.0, 0.0, 0.0], "max": [1.0, 2.0, 3.0] },
            { "bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR" }
        ]
    });
    let mut json = serde_json::to_vec(&json).expect("glb json");
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

/// Root → two mids → four leaves, all REPLACE: seven loads to converge.
fn tileset(dir: &std::path::Path) -> Tileset {
    let glb = glb();
    for name in ["root", "m0", "m1", "l0", "l1", "l2", "l3"] {
        std::fs::write(dir.join(format!("{name}.glb")), &glb).expect("write glb");
    }
    let leaf = |x: f64, uri: &str| {
        format!(
            r#"{{ "boundingVolume": {{ "sphere": [{x}, 0, 0, 25] }},
                 "geometricError": 0, "content": {{ "uri": "{uri}" }} }}"#
        )
    };
    let mid = |x: f64, uri: &str, a: String, b: String| {
        format!(
            r#"{{ "boundingVolume": {{ "sphere": [{x}, 0, 0, 50] }},
                 "geometricError": 50, "refine": "REPLACE",
                 "content": {{ "uri": "{uri}" }}, "children": [{a}, {b}] }}"#
        )
    };
    let json = format!(
        r#"{{ "asset": {{ "version": "1.1" }}, "geometricError": 400,
             "root": {{ "boundingVolume": {{ "sphere": [0, 0, 0, 100] }},
                        "geometricError": 100, "refine": "REPLACE",
                        "content": {{ "uri": "root.glb" }},
                        "children": [{}, {}] }} }}"#,
        mid(
            -50.0,
            "m0.glb",
            leaf(-75.0, "l0.glb"),
            leaf(-25.0, "l1.glb")
        ),
        mid(50.0, "m1.glb", leaf(25.0, "l2.glb"), leaf(75.0, "l3.glb")),
    );
    let path = dir.join("tileset.json");
    std::fs::write(&path, &json).expect("write tileset");
    let url = Url::from_file_path(&path).expect("url");
    Tileset::from_json_bytes(json.as_bytes(), &url).expect("tileset")
}

fn near_view() -> ViewState {
    ViewState::perspective(
        dvec3(0.0, 0.0, 120.0),
        dvec3(0.0, 0.0, -1.0),
        dvec3(0.0, 1.0, 0.0),
        dvec2(1024.0, 768.0),
        std::f64::consts::FRAC_PI_3,
    )
}

#[test]
fn every_arrival_is_waited_on_once_and_siblings_share_a_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        pinned_level: None,
        ..Config::default()
    };
    let (mut stream, server) = in_process(tileset(dir.path()), Arc::new(FsFetcher), config);
    let mut server = Box::pin(server.run());
    stream
        .send(ClientMessage::ViewerState {
            views: vec![near_view()],
            generation: 0,
        })
        .expect("send");

    // Stepped by hand until quiet: `FsFetcher` answers inline, so every poll
    // makes progress and a step count bounds the run.
    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let (mut contents, mut quiet) = (0usize, 0usize);
    for _ in 0..2_000 {
        let _ = server.as_mut().poll(&mut cx);
        let mut got = false;
        while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
            got = true;
            if let ServerMessage::Content { .. } = msg {
                contents += 1;
            }
        }
        quiet = if got { 0 } else { quiet + 1 };
        if quiet > 8 {
            break;
        }
    }
    assert!(quiet > 8, "the session never settled");

    let m = metrics();
    let arrived = m.loads_completed.get() + m.loads_failed.get();
    assert_eq!(m.loads_failed.get(), 0, "every tile of the fixture exists");
    assert!(contents >= 7, "the session stopped short: {contents} tiles");
    assert_eq!(m.loads_completed.get(), contents as u64);

    // Every arrival handled was waited on exactly once.
    assert_eq!(
        m.arrival_wait_seconds.count(),
        arrived,
        "arrival_wait must count each handled completion once"
    );
    // ... and counted in exactly one pass, which siblings ready together share.
    let per_pass = &m.arrivals_per_pass;
    assert_eq!(per_pass.sum(), arrived, "every arrival is in one pass");
    assert!(
        per_pass.count() < arrived,
        "{} passes for {arrived} arrivals: siblings were not coalesced",
        per_pass.count()
    );
    assert!(
        per_pass.peak() > 1,
        "no pass answered more than one arrival"
    );

    // The passes are those, plus the one the camera asked for.
    assert_eq!(m.traversals.get(), per_pass.count() + 1);
    assert_eq!(m.traversal_seconds.count(), m.traversals.get());
    // Every pass ends with a Select, and each is timed.
    assert_eq!(m.select_seconds.count(), m.traversals.get());
}
