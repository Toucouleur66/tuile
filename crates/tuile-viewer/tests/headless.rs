// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The headless viewer is the viewer, and what it reads is what it drew.**
//!
//! The harness every pattern test stands on: a frame drawn and read back, the
//! stepped clock that makes two runs one, a resize that keeps a frame, two
//! eyes read back each from its own camera, the session log, an observer
//! plugged in, a tape flown, the background and the way back, the socket.
//! Real GPU, real application, the fixture globe — no network.

mod support;

use std::io::{BufRead, BufReader, Write};

use tuile_viewer::control::{patterns, Action, Device, Eyes, Observer, Series, Start, State, View};

/// Over Paris, looking down from 50 km.
fn over_paris(altitude: f64) -> View {
    View {
        lon: patterns::PARIS.0,
        lat: patterns::PARIS.1,
        altitude,
        heading: 0.0,
        pitch: 90.0,
    }
}

/// A frame is drawn into the offscreen eye and read back: ground under the
/// eye, none of it the clear colour, and the frame's own picture to look at.
#[test]
fn a_headless_frame_is_drawn_read_back_and_scanned() {
    let Some(mut run) = support::start(Start::default()) else {
        return;
    };
    let settled = run.settle(600);
    assert!(settled.settled, "never settled: {:?}", run.state());
    let state = run.state();
    assert!(state.drew);
    assert_eq!(state.eyes.len(), 1);
    // Straight down from 2 000 km at 256 px: most of the frame is globe.
    assert!(
        state.eyes[0].ground > 256 * 256 / 3,
        "too little ground scanned: {:?}",
        state.eyes[0]
    );
    assert_eq!(
        state.eyes[0].black, 0,
        "ground left black: {:?}",
        state.eyes[0]
    );
    let (pixels, size) = run.picture(0).expect("a picture");
    assert_eq!(size, (256, 256));
    assert_eq!(pixels.len(), 256 * 256 * 4);
    // The middle of the frame is the fixture's imagery, bright.
    let middle = (128 * 256 + 128) * 4;
    assert!(
        pixels[middle..middle + 3].iter().any(|&c| c > 60),
        "the middle is {:?}",
        &pixels[middle..middle + 4]
    );
    assert!(run.picture(1).is_none(), "one eye, one picture");
    assert!(state.metrics.frame_exact > 0 && state.metrics.uploads > 0);
}

/// A script that moves the eye, eases it with the wheel, glides it and
/// resizes the frame, run twice: the same states, frame for frame, and the
/// same session log, message for message. The glide is what a wall clock
/// would give away: where the eye is on each frame of it is the clock's to
/// say.
#[test]
fn the_stepped_clock_makes_two_runs_of_one_script_the_same() {
    let dir = tempfile::tempdir().expect("a directory");
    let script = Series::still(30)
        .then(Series::new().at(
            0,
            Action::Wheel {
                amount: 6.0,
                at: (0.4, 0.6),
            },
        ))
        .then(Series::still(40))
        .then(Series::new().at(0, Action::Go(over_paris(5.0e4))))
        .then(Series::still(20))
        .then(Series::new().at(0, Action::Glide(over_paris(2.0e5), 1.0)))
        .then(Series::still(70))
        .then(Series::new().at(
            0,
            Action::Resize {
                width: 320,
                height: 200,
            },
        ))
        .then(Series::still(20));
    let mut runs = Vec::new();
    for n in 0..2 {
        let log = dir.path().join(format!("run-{n}.mcap"));
        let Some(mut run) = support::start(Start {
            log: Some(log.clone()),
            ..Start::default()
        }) else {
            return;
        };
        let states: Vec<State> = run.play(&script).to_vec();
        let support::Run { run, turn: _turn } = run;
        run.finish().expect("the log closes");
        runs.push((states, tuile_tape::messages(&log).expect("the log reads")));
    }
    let ((a, log_a), (b, log_b)) = (&runs[0], &runs[1]);
    assert_eq!(a.len(), script.len());
    for (x, y) in a.iter().zip(b) {
        assert!(
            x == y,
            "the runs parted at frame {}: {:?}",
            x.frame,
            differences(&x.to_json(), &y.to_json(), "")
        );
    }
    assert!(a.iter().any(|s| s.drew), "the script drew nothing");
    assert_eq!(log_a.len(), log_b.len());
    assert!(log_a == log_b, "the session logs differ");
    // The wheel moved the eye, eased over several frames.
    let altitude = |s: &State| s.view.altitude;
    assert!(
        altitude(&a[60]) < altitude(&a[29]),
        "the wheel did not zoom"
    );
}

/// Where two documents differ, by path.
fn differences(a: &serde_json::Value, b: &serde_json::Value, at: &str) -> Vec<String> {
    match (a, b) {
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => x
            .iter()
            .flat_map(|(k, v)| {
                differences(
                    v,
                    y.get(k).unwrap_or(&serde_json::Value::Null),
                    &format!("{at}/{k}"),
                )
            })
            .collect(),
        _ if a == b => Vec::new(),
        _ => vec![format!("{at}: {a} ≠ {b}")],
    }
}

/// A frame resized — a window dragged, a phone turned — is drawn at its new
/// size on the very next frame, with no ground left black.
#[test]
fn a_resize_keeps_a_frame_and_draws_at_the_new_size() {
    let Some(mut run) = support::start(Start::default()) else {
        return;
    };
    assert!(run.settle(600).settled);
    run.act(&Action::Resize {
        width: 160,
        height: 300,
    });
    let state = run.step().clone();
    assert!(state.drew, "the frame after the resize drew nothing");
    assert_eq!(state.size, (160, 300));
    assert_eq!(run.picture(0).expect("a picture").1, (160, 300));
    assert_eq!(state.black(), 0, "black ground after a resize: {state:?}");
    assert!(state.eyes[0].ground > 0);
}

/// Two eyes far enough apart to see different ground — a hyperstereo rig,
/// kilometres wide — are each drawn and read back from their own camera.
#[test]
fn each_eye_is_drawn_and_read_back_from_its_own_camera() {
    let separation = 2.0e4;
    let Some(mut run) = support::start(Start {
        view: over_paris(5.0e4),
        eyes: Eyes::Stereo { separation },
        ..Start::default()
    }) else {
        return;
    };
    assert!(run.settle(600).settled, "{:?}", run.state());
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
    let state = run.state();
    assert_eq!(state.eyes.len(), 2);
    for eye in &state.eyes {
        assert!(eye.ground > 0 && eye.black == 0, "{eye:?}");
    }
}

/// The session log holds the camera, a state a step and every picture
/// drawn, on one timeline; a mono log's pictures are where a trace keeps
/// them.
#[test]
fn the_session_log_holds_the_camera_the_states_and_the_pictures() {
    let dir = tempfile::tempdir().expect("a directory");
    let log = dir.path().join("session.mcap");
    let Some(mut run) = support::start(Start {
        log: Some(log.clone()),
        log_pictures: true,
        ..Start::default()
    }) else {
        return;
    };
    let steps = 40;
    run.steps(steps);
    let drawn = run.history().iter().filter(|s| s.drew).count();
    let support::Run { run, turn: _turn } = run;
    assert_eq!(run.finish().expect("closes"), Some(log.clone()));
    let messages = tuile_tape::messages(&log).expect("reads");
    let on = |topic: &str| messages.iter().filter(|m| m.topic == topic).count();
    assert_eq!(on("/state"), steps as usize);
    assert_eq!(on("/frame"), drawn);
    assert!(drawn > 0);
    assert!(on("/camera") >= 1, "the camera path is there");
    // A replay reads it as the camera path it is.
    let mut tape = tuile_tape::Tape::replaying(&log).expect("a tape");
    assert_eq!(tape.progress().1, steps as usize);
    assert!(tape.next_frame().is_some());
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
fn an_observer_plugged_in_is_read_every_step_and_logged() {
    let dir = tempfile::tempdir().expect("a directory");
    let log = dir.path().join("observed.mcap");
    let Some(mut run) = support::start(Start {
        log: Some(log.clone()),
        ..Start::default()
    }) else {
        return;
    };
    run.observe_with(Box::new(Counting(0)));
    run.steps(5);
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

/// A recorded camera path is flown one camera a frame, through the path a
/// replay takes — and the run goes on from where it ends rather than ending.
#[test]
fn a_tape_is_flown_frame_for_frame_and_the_run_goes_on() {
    let dir = tempfile::tempdir().expect("a directory");
    let path = dir.path().join("path.mcap");
    let mut tape = tuile_tape::Tape::recording(&path).expect("records");
    let frames: Vec<tuile_tape::Frame> = (0..12)
        .map(|i| {
            let camera = tuile_camera::GlobeCamera::from_geodetic(
                48.85f64.to_radians(),
                (2.35 + 0.01 * f64::from(i)).to_radians(),
                3.0e4,
                0.0,
                std::f64::consts::FRAC_PI_2,
                tuile_camera::DEFAULT_GLOBE_FOVY,
            );
            tuile_tape::Frame {
                position: camera.position.to_array(),
                direction: camera.direction.to_array(),
                up: camera.up.to_array(),
                fovy: camera.fovy,
            }
        })
        .collect();
    for frame in &frames {
        tape.push(*frame);
    }
    tape.finish().expect("closes");
    let Some(mut run) = support::start(Start {
        view: over_paris(3.0e4),
        ..Start::default()
    }) else {
        return;
    };
    assert!(run.settle(600).settled);
    run.fly(tuile_tape::Tape::replaying(&path).expect("replays"));
    for frame in &frames {
        run.step();
        assert_eq!(run.eye_cameras()[0].position, frame.position);
    }
    run.steps(10);
    assert!(run.ended().is_none(), "the run ended with the tape");
    assert!(!run.flying());
    assert_eq!(
        run.eye_cameras()[0].position,
        frames[11].position,
        "the eye left where the tape put it"
    );
}

/// A phone in the background draws nothing and holds its loads; back in
/// front it draws again, covered, at once.
#[test]
fn the_background_draws_nothing_and_the_way_back_draws_at_once() {
    let Some(mut run) = support::start(Start {
        device: Device::PHONE,
        ..Start::default()
    }) else {
        return;
    };
    assert!(run.settle(1200).settled, "{:?}", run.state());
    run.act(&Action::Background);
    let away = run.play(&Series::still(30)).to_vec();
    assert!(away.iter().all(|s| !s.drew), "drew in the background");
    run.act(&Action::Foreground);
    let back = run.step().clone();
    assert!(back.drew, "nothing drawn on the way back");
    assert_eq!(back.black(), 0, "{back:?}");
    assert_eq!(back.metrics.frame_lost, 0, "{back:?}");
}

/// The socket answers on loopback, in its own words, and refuses any other
/// address before it opens.
#[test]
fn the_socket_speaks_the_api_on_loopback_and_nowhere_else() {
    let Some(mut run) = support::start(Start::default()) else {
        return;
    };
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
fn a_run_that_ends_leaves_none_of_its_residency_counted() {
    let m = tuile_core::metrics::metrics();
    let Some(mut run) = support::start(Start::default()) else {
        return;
    };
    // Nothing uploaded yet: what is counted is what earlier runs left.
    let before = (m.meshes_by_level.total(), m.mesh_bytes_by_level.total());
    assert!(run.settle(600).settled);
    assert!(run.state().metrics.mesh_bytes > 0);
    // Stitched: the drawn tiles met their neighbours, and changed size.
    assert!(run.state().metrics.frame_exact > 1);
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
