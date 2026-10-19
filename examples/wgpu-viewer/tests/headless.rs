// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **This application, headless, on the real catalog, the real store and the
//! real clock.**
//!
//! Every run here is the application a window runs — its session, its
//! runtime, its loads, its upload pacing, its tile store on disk — asking
//! Cesium World Terrain and the aerial imagery over the network through a
//! catalog proxy served in this process (see `support`), drawing into
//! offscreen eyes at the display's refresh, in wall time, every frame read
//! back and scanned.
//!
//! Two kinds of test:
//!
//! - **the harness**: what the headless viewer itself promises — a frame
//!   drawn and read back, a resize, two eyes, the session log and its
//!   replay, an observer, a tape, the background, the socket, a run that
//!   ends, a warm relaunch;
//! - **the patterns** P1–P6 of the graphics budget's specification, each
//!   frame held to its obligations: no ground left black in any eye, no
//!   selected ground drawn by nothing, the server within the profile's
//!   budget, and the picture settled within the bound (two seconds on a
//!   desktop, four on a phone). A pattern today's viewer does not meet is
//!   kept, red and ignored with its numbers: the budget's cut-over is what
//!   turns it green.
//!
//! The network varies from one run to the next and so do the numbers; each
//! run prints its report, and keeps its session log in
//! `target/tmp/headless/` for a red run to be opened and flown again.
//!
//! **They need the network, an ion token and a GPU**, so they are ignored
//! unless asked for, in release — the numbers of a debug build are not the
//! application's:
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo test --release -p tuile-wgpu-viewer --test headless -- --ignored
//! ```

mod support;

use std::io::{BufRead, BufReader, Write};

use support::{log_for, relaunch, report, sessions, Place, Report};
use tuile_viewer::control::{
    patterns, patterns::Pattern, Action, Device, Eyes, Observer, Pictures, Series, Settled, State,
    View,
};

/// How long a run is given to come up and settle in a harness test.
const COMING_UP: f64 = patterns::COMING_UP_SECONDS;

/// Over Paris, looking down from `altitude`.
fn over_paris(altitude: f64) -> View {
    View {
        lon: patterns::PARIS.0,
        lat: patterns::PARIS.1,
        altitude,
        heading: 0.0,
        pitch: 90.0,
    }
}

fn said(name: &str, what: impl std::fmt::Debug) {
    let (endpoints, metadata) = sessions();
    eprintln!(
        "{name}: {what:?} | catalog sessions so far: {endpoints} endpoints, {metadata} imagery metadata"
    );
}

// --- The harness ------------------------------------------------------------

/// A frame is drawn into the offscreen eye and read back: ground under the
/// eye, none of it the clear colour, and the frame's own picture to look at.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn a_frame_is_drawn_read_back_and_scanned() {
    let place = Place::new();
    let mut run = support::start(place.start());
    let settled = run.settle(COMING_UP);
    said("first frame", settled);
    assert!(settled.settled, "never settled: {:?}", run.state());
    let state = run.state();
    assert!(state.drew);
    assert_eq!(state.eyes.len(), 1);
    assert!(state.eyes[0].ground > 10_000, "{:?}", state.eyes[0]);
    assert_eq!(
        state.eyes[0].black, 0,
        "ground left black: {:?}",
        state.eyes[0]
    );
    let (pixels, size) = run.picture(0).expect("a picture");
    assert_eq!(size, (1280, 800));
    assert_eq!(pixels.len(), 1280 * 800 * 4);
    assert!(run.picture(1).is_none(), "one eye, one picture");
    assert!(state.metrics.frame_exact > 0 && state.metrics.uploads > 0);
}

/// A frame resized — a window dragged, a phone turned — is drawn at its new
/// size on the next frame, with no ground left black.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn a_resize_keeps_a_frame_and_draws_at_the_new_size() {
    let place = Place::new();
    let mut run = support::start(place.start());
    assert!(run.settle(COMING_UP).settled);
    run.act(&Action::Resize {
        width: 800,
        height: 1280,
    });
    let state = run.step().clone();
    assert!(state.drew, "the frame after the resize drew nothing");
    assert_eq!(state.size, (800, 1280));
    assert_eq!(run.picture(0).expect("a picture").1, (800, 1280));
    assert_eq!(state.black(), 0, "black ground after a resize: {state:?}");
    assert!(state.eyes[0].ground > 0);
}

/// Two eyes far enough apart to see different ground — a hyperstereo rig,
/// kilometres wide — are each drawn and read back from their own camera.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn each_eye_is_drawn_and_read_back_from_its_own_camera() {
    let separation = 2.0e4;
    let place = Place::new();
    let mut run = support::start(tuile_viewer::control::Start {
        view: over_paris(5.0e4),
        size: (640, 640),
        scale: 1.0,
        eyes: Eyes::Stereo { separation },
        ..place.start()
    });
    assert!(run.settle(COMING_UP).settled, "{:?}", run.state());
    let cameras = run.eye_cameras();
    assert_eq!(cameras.len(), 2);
    let apart = (0..3)
        .map(|i| (cameras[0].position[i] - cameras[1].position[i]).powi(2))
        .sum::<f64>()
        .sqrt();
    assert!((apart - separation).abs() < 1.0, "{apart}");
    let left = run.picture(0).expect("the left eye").0.to_vec();
    let right = run.picture(1).expect("the right eye").0.to_vec();
    assert_eq!(left.len(), right.len());
    assert_ne!(left, right, "both eyes drew the same picture");
    for eye in &run.state().eyes {
        assert!(eye.ground > 0 && eye.black == 0, "{eye:?}");
    }
}

/// The session log holds the camera, a state a step and the pictures it was
/// told to keep, on one timeline.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn the_session_log_holds_the_camera_the_states_and_the_pictures() {
    let place = Place::new();
    let log = log_for("harness session log");
    let mut run = support::start(tuile_viewer::control::Start {
        log: Some(log.clone()),
        pictures: Pictures::Always,
        size: (640, 400),
        scale: 1.0,
        ..place.start()
    });
    assert!(run.settle(COMING_UP).settled);
    run.run_for(1.0);
    let steps = run.history().len();
    let drawn = run.history().iter().filter(|s| s.drew).count();
    let support::Run { run, turn: _turn } = run;
    assert_eq!(run.finish().expect("closes"), Some(log.clone()));
    let messages = tuile_tape::messages(&log).expect("reads");
    let on = |topic: &str| messages.iter().filter(|m| m.topic == topic).count();
    assert_eq!(on("/state"), steps);
    assert_eq!(on("/frame"), drawn);
    assert!(drawn > 0);
    let tape = tuile_tape::Tape::replaying(&log).expect("a tape");
    assert_eq!(tape.progress().1, steps, "a camera a step");
}

/// What a run logged flies again: the camera path of its session log,
/// replayed in a second run over the same store, puts the eye where the first
/// run had it, frame for frame.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn a_logged_run_replays_to_the_same_camera_path() {
    let place = Place::new();
    let log = log_for("harness replayed");
    let mut run = support::start(tuile_viewer::control::Start {
        view: over_paris(3.0e4),
        log: Some(log.clone()),
        pictures: Pictures::Never,
        ..place.start()
    });
    assert!(run.settle(COMING_UP).settled);
    run.play(
        &Series::new()
            .at(0.0, Action::Glide(over_paris(1.5e5), 1.5))
            .then(Series::still(2.0)),
    );
    let mut run = relaunch(run, None);
    assert!(run.settle(COMING_UP).settled);
    let mut tape = tuile_tape::Tape::replaying(&log).expect("the first run's camera path");
    let path: Vec<tuile_tape::Frame> = std::iter::from_fn(|| tape.next_frame()).collect();
    assert!(path.len() > 60, "{} cameras", path.len());
    run.fly(tuile_tape::Tape::replaying(&log).expect("again"));
    for (n, camera) in path.iter().enumerate() {
        run.step();
        assert_eq!(
            run.eye_cameras()[0].position,
            camera.position,
            "the replay left the path at frame {n}"
        );
    }
    run.step();
    assert!(!run.flying());
    assert!(run.ended().is_none(), "the run ended with the tape");
}

/// Counts the steps it saw, and says how many tiles were drawn exactly.
struct Counting(u64);

impl Observer for Counting {
    fn topic(&self) -> &str {
        "/counting"
    }

    fn observe(&mut self, state: &State) -> serde_json::Value {
        self.0 += 1;
        serde_json::json!({"seen": self.0, "exact": state.metrics.frame_exact})
    }
}

/// A source of readings plugged in is read every step, beside the run's own
/// state, and written on its own topic — the way the budget's observer will
/// be.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn an_observer_plugged_in_is_read_every_step_and_logged() {
    let place = Place::new();
    let log = log_for("harness observer");
    let mut run = support::start(tuile_viewer::control::Start {
        log: Some(log.clone()),
        ..place.start()
    });
    run.observe_with(Box::new(Counting(0)));
    for _ in 0..5 {
        run.step();
    }
    assert_eq!(
        run.state().observed.get("/counting"),
        Some(&serde_json::json!({"seen": 5, "exact": run.state().metrics.frame_exact}))
    );
    let support::Run { run, turn: _turn } = run;
    run.finish().expect("closes");
    let messages = tuile_tape::messages(&log).expect("reads");
    assert_eq!(
        messages.iter().filter(|m| m.topic == "/counting").count(),
        5
    );
}

/// A phone in the background draws nothing and holds its loads; back in
/// front it draws again at once, covered.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn the_background_draws_nothing_and_the_way_back_draws_at_once() {
    let place = Place::new();
    let mut run = support::start(tuile_viewer::control::Start {
        device: Device::PHONE,
        size: (585, 1266),
        scale: 1.5,
        ..place.start()
    });
    assert!(run.settle(COMING_UP).settled, "{:?}", run.state());
    run.act(&Action::Background);
    let away = run.play(&Series::still(1.0)).to_vec();
    assert!(away.iter().all(|s| !s.drew), "drew in the background");
    run.act(&Action::Foreground);
    let back = run
        .wait_until(1.0, |s| s.drew)
        .expect("nothing drawn on the way back");
    let state = run.state();
    said("back in front", (back, &state.metrics.frame_lost));
    assert_eq!(state.black(), 0, "{state:?}");
    assert_eq!(state.metrics.frame_lost, 0, "{state:?}");
}

/// The socket answers on loopback, in its own words, and refuses any other
/// address before it opens.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn the_socket_speaks_the_api_on_loopback_and_nowhere_else() {
    let place = Place::new();
    let mut run = support::start(place.start());
    assert!(run
        .listen("0.0.0.0:0".parse().expect("an address"))
        .is_err());
    let address = run
        .listen("127.0.0.1:0".parse().expect("an address"))
        .expect("listens");
    let client = std::thread::spawn(move || {
        let stream = std::net::TcpStream::connect(address).expect("connects");
        let mut out = stream.try_clone().expect("a writer");
        let mut lines = BufReader::new(stream).lines();
        let mut ask = |request: &str| -> serde_json::Value {
            writeln!(out, "{request}").expect("sends");
            serde_json::from_str(&lines.next().expect("an answer").expect("a line")).expect("JSON")
        };
        let version = ask(r#"{"v":1,"cmd":"version"}"#);
        let stepped = ask(r#"{"v":1,"cmd":"step","frames":3}"#);
        let moved = ask(
            r#"{"v":1,"cmd":"go","lon":2.35,"lat":48.85,"altitude":50000,"heading":0,"pitch":90}"#,
        );
        let after = ask(r#"{"v":1,"cmd":"step","frames":2}"#);
        let metrics = ask(r#"{"v":1,"cmd":"metrics"}"#);
        let wrong = ask(r#"{"v":2,"cmd":"state"}"#);
        let unknown = ask(r#"{"v":1,"cmd":"teleport"}"#);
        let closed = ask(r#"{"v":1,"cmd":"close"}"#);
        (
            version, stepped, moved, after, metrics, wrong, unknown, closed,
        )
    });
    run.serve();
    let (version, stepped, moved, after, metrics, wrong, unknown, closed) =
        client.join().expect("the client");
    assert_eq!(version["ok"]["v"], 1);
    assert_eq!(stepped["ok"]["frame"], 3);
    assert_eq!(moved["ok"], serde_json::Value::Null);
    assert_eq!(after["ok"]["frame"], 5);
    let altitude = after["ok"]["view"]["altitude"].as_f64().expect("altitude");
    assert!((altitude - 5.0e4).abs() < 1.0, "{altitude}");
    assert!(metrics["ok"]["frames"].as_u64().is_some(), "{metrics}");
    assert!(wrong["error"].as_str().is_some(), "{wrong}");
    assert!(unknown["error"]
        .as_str()
        .is_some_and(|e| e.contains("teleport")));
    assert_eq!(closed["ok"], serde_json::Value::Null);
}

/// A run that ends takes back what it counted: the meshes it held, stitched
/// or not, and what its server held and selected, are no longer counted once
/// it has gone — the next run in the process reads its own.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn a_run_that_ends_leaves_none_of_its_residency_counted() {
    let m = tuile_core::metrics::metrics();
    let place = Place::new();
    let run = support::start(place.start());
    // The session is up and has uploaded nothing yet: what is counted is
    // what earlier runs left.
    let before = (m.meshes_by_level.total(), m.mesh_bytes_by_level.total());
    let mut run = run;
    assert!(run.settle(COMING_UP).settled);
    assert!(run.state().metrics.mesh_bytes > 0);
    assert!(run.state().metrics.resident_bytes > 0);
    let support::Run { run, turn } = run;
    drop(run);
    assert_eq!(
        (m.meshes_by_level.total(), m.mesh_bytes_by_level.total()),
        before,
        "by level: {:?}",
        m.mesh_bytes_by_level.occupied()
    );
    assert_eq!(
        (
            m.resident_bytes.get(),
            m.tiles_selected.get(),
            m.priming_total.get()
        ),
        (0, 0, 0),
        "the server that went still reads as holding and selecting"
    );
    drop(turn);
}

// --- The patterns -----------------------------------------------------------

/// Runs `pattern` from a fresh place: up and settled, then the motion, then
/// watched settling for as long as it takes, up to a cap — so a run that
/// misses its bound says by how much. Returns the states from the motion
/// on, how the settling went, and the report — printed either way.
///
/// For a pattern whose coming up is the pattern (P1), the settling is
/// counted from before the run starts: the session's launch is part of it.
fn run(pattern: &Pattern) -> (Vec<State>, Settled, Report) {
    let place = Place::new();
    let launched = std::time::Instant::now();
    let mut run = support::start(tuile_viewer::control::Start {
        log: Some(log_for(pattern.name)),
        ..pattern.start(place.host(), place.store())
    });
    let launch = launched.elapsed().as_secs_f64();
    if pattern.warm > 0.0 {
        let up = run.settle(pattern.warm);
        assert!(
            up.settled,
            "{}: never came up in {} s: {:?}",
            pattern.name,
            pattern.warm,
            run.state()
        );
    }
    let first = run.history().len();
    run.play(&pattern.motion);
    let mut settled = run.settle(patterns::WATCHED_SECONDS);
    if pattern.warm == 0.0 {
        settled.seconds += launch;
    }
    let states = run.history()[first..].to_vec();
    let r = report(&states, run.limits().resident_bytes);
    said(pattern.name, (&settled, &r));
    (states, settled, r)
}

/// What every pattern is held to: see the module. `bound` is the
/// specification's, in seconds.
fn holds(name: &str, bound: f64, settled: Settled, r: &Report) {
    assert!(r.drawn > 0, "{name}: nothing drawn");
    assert_eq!(r.black_frames, 0, "{name}: ground left black: {r:?}");
    assert_eq!(r.lost_frames, 0, "{name}: holes in the selection: {r:?}");
    assert_eq!(r.over_budget_frames, 0, "{name}: over the budget: {r:?}");
    assert!(
        settled.settled && settled.seconds <= bound,
        "{name}: settled in {:.1} s (settled: {}), against {bound} s: {r:?}",
        settled.seconds,
        settled.settled
    );
}

fn check(pattern: Pattern) {
    let (_, settled, r) = run(&pattern);
    holds(pattern.name, pattern.settle, settled, &r);
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p1_a_cold_start_covers_the_ground_and_settles_in_time() {
    check(patterns::cold_start());
}

/// Started again over the store a cold run filled: nothing it holds is asked
/// of the network again, and the picture settles in time.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p1_a_warm_start_is_served_by_the_store_and_settles_in_time() {
    let pattern = patterns::cold_start();
    let place = Place::new();
    let mut cold = support::start(pattern.start(place.host(), place.store()));
    let came_up = cold.settle(patterns::COMING_UP_SECONDS);
    assert!(came_up.settled, "the cold run never came up");
    // What the cold run settled on has been fetched; let what it was still
    // fetching land before it is put away.
    let _ = cold.wait_until(10.0, State::idle);
    let launched = std::time::Instant::now();
    let mut warm = relaunch(cold, Some(log_for("P1 warm start")));
    let launch = launched.elapsed().as_secs_f64();
    let mut settled = warm.settle(patterns::WATCHED_SECONDS);
    settled.seconds += launch;
    let r = report(warm.history(), warm.limits().resident_bytes);
    said("P1 warm start", (&settled, &r));
    assert_eq!(
        r.store_misses, 0,
        "the network was asked for what the store holds: {r:?}"
    );
    assert!(r.store_hits > 0, "the store served nothing: {r:?}");
    holds("P1 warm start", pattern.settle, settled, &r);
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p1_a_cold_start_at_low_memory_covers_the_ground_and_settles_in_time() {
    check(patterns::cold_start_at_low_memory());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p2_a_slow_zoom_in_never_uncovers_the_ground() {
    check(patterns::zoom_in_slow());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p2_a_fast_zoom_in_never_uncovers_the_ground() {
    check(patterns::zoom_in_fast());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p2_a_zoom_to_the_deepest_level_never_uncovers_the_ground() {
    check(patterns::zoom_in_to_the_deepest_level());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p3_a_zoom_out_to_200_km_and_back_never_uncovers_the_ground() {
    check(patterns::zoom_out_and_back());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p4_a_yaw_in_place_never_uncovers_the_ground() {
    check(patterns::yaw_in_place());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p4_a_yaw_at_head_rate_never_uncovers_the_ground() {
    check(patterns::yaw_at_head_rate());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p4_an_orbit_about_a_target_never_uncovers_the_ground() {
    check(patterns::orbit());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p4_a_tilt_toward_the_horizon_never_uncovers_the_ground() {
    check(patterns::tilt());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p5_portrait_and_landscape_never_uncover_the_ground() {
    check(patterns::portrait_and_landscape());
}

#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p5_a_resize_and_a_change_of_density_never_uncover_the_ground() {
    check(patterns::resize_and_rescale());
}

/// Both eyes, every frame: neither is left a hole the other lacks.
#[test]
#[ignore = "needs the network, an ion token (TUILE_ION_TOKEN) and a GPU: see the module"]
fn p6_two_eyes_at_90_hz_never_uncover_the_ground_in_either() {
    let pattern = patterns::stereo_head_motion();
    let (states, settled, r) = run(&pattern);
    assert!(states.iter().all(|s| s.eyes.len() == 2));
    holds(pattern.name, pattern.settle, settled, &r);
}
