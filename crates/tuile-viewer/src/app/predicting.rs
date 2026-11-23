// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The views ahead: where the camera will be, sent to the geometry server
//! each frame (`ClientMessage::Ahead`) so the ground it is heading for loads
//! before it gets there.
//!
//! The frame's future, in order of what is known (`tuile_camera::predict`):
//! a replay's next frames (certain), a glide's path or a fling's coast
//! (known, until a hand interrupts), or the motion model's extrapolation of
//! a hand on the globe. Nothing moving, nothing is sent ahead — once, the
//! empty list that clears the views given before.
//!
//! The same code on a desktop and in a page: the prediction is the camera
//! crate's, the message the protocol's.

use tuile_camera::predict::{Ahead, Future, PredictConfig, Predictor, Source};
use tuile_camera::GlobeCamera;
use tuile_core::protocol::{AheadView, ClientMessage, GeometryStream, Reach};
use web_time::Instant;

use super::App;

/// The step the coast is run ahead at, seconds: the frames the controller
/// will draw, at the display's usual rate.
const COAST_FRAME_S: f64 = 1.0 / 60.0;

/// The prediction's state in a session.
pub(super) struct Predicting {
    pub(super) predictor: Predictor,
    /// The clock the model's times are read on.
    epoch: Instant,
    /// Whether the last message sent listed views: an empty list is sent
    /// once, when they stop.
    sent_some: bool,
    /// The numbers asked on the route, over the use context's.
    pub(super) ask: crate::steer::PredictAsk,
    /// The digest of the numbers in force, sent with the views ahead.
    digest: u64,
}

impl Default for Predicting {
    fn default() -> Self {
        Predicting::new(PredictConfig::default())
    }
}

impl Predicting {
    pub(super) fn new(config: PredictConfig) -> Self {
        publish(&config);
        Predicting {
            digest: config.digest(),
            predictor: Predictor::new(config),
            epoch: Instant::now(),
            sent_some: false,
            ask: crate::steer::PredictAsk::default(),
        }
    }
}

/// The numbers in force, to the process's metrics (exported with the
/// telemetry).
fn publish(c: &PredictConfig) {
    let m = tuile_core::metrics::metrics();
    let ms = |s: f64| {
        if s.is_finite() {
            (s * 1000.0).round() as u64
        } else {
            u64::MAX
        }
    };
    m.predict_digest.set(c.digest());
    m.predict_horizons.set(c.horizons_s.len() as u64);
    m.predict_horizon_max_ms
        .set(ms(c.horizons_s.iter().copied().fold(0.0, f64::max)));
    m.predict_laplace_ms.set(ms(c.laplace_s));
    m.predict_interrupt_ms.set(ms(c.interrupt_s));
    m.predict_rate_decay_ms.set(ms(c.rate_decay_s));
    m.predict_coast_ms.set(ms(c.coast_s));
}

/// The numbers for the use context labelled `context`, with `ask`'s over
/// them. An ask the grammar refuses was refused when it was parsed; were
/// one to reach here, the context's numbers stand.
pub(super) fn configured(context: &str, ask: &crate::steer::PredictAsk) -> PredictConfig {
    let base = PredictConfig::for_context(context);
    base.with_query(&ask.query()).unwrap_or(base)
}

impl App {
    /// The prediction's numbers again, from the use context (the budget's
    /// specification in force) and the route's ask: in force from the next
    /// frame, the controller's coast with them, said in the log and the
    /// metrics.
    pub(super) fn reconfigure_the_prediction(&mut self) {
        let context = self
            .budget_ask
            .kind
            .unwrap_or(tuile_core::budget::SpecKind::Interactive)
            .label();
        let config = configured(context, &self.predicting.ask);
        tracing::info!(query = %config.to_query(), context, "prediction numbers");
        self.controller.inertia.coast_s = config.coast_s;
        self.predicting.digest = config.digest();
        publish(&config);
        self.predicting.predictor.reconfigure(config);
    }

    /// This frame's views ahead, from its future, sent before its camera.
    pub(super) fn send_the_views_ahead(&mut self) {
        let camera = self.controller.camera;
        let t = Instant::now()
            .saturating_duration_since(self.predicting.epoch)
            .as_secs_f64();
        let horizons = self.predicting.predictor.config().horizons_s.clone();
        let refresh = f64::from(self.refresh_hz.max(1));
        // A replay: its next frames, one a display frame.
        let replayed: Option<Vec<(f64, GlobeCamera)>> = self
            .recording
            .tape
            .as_ref()
            .filter(|tape| !tape.is_recording())
            .map(|tape| {
                horizons
                    .iter()
                    .filter_map(|tau| {
                        let k = (tau * refresh).round().max(1.0) as usize;
                        tape.peek(k).map(|f| {
                            (
                                *tau,
                                GlobeCamera {
                                    position: glam::DVec3::from_array(f.position),
                                    direction: glam::DVec3::from_array(f.direction),
                                    up: glam::DVec3::from_array(f.up),
                                    fovy: f.fovy,
                                },
                            )
                        })
                    })
                    .collect()
            });
        let now = Instant::now();
        let coast: Option<Vec<GlobeCamera>> = (self.controller.is_coasting()
            && !self.controller.is_driven())
        .then(|| self.controller.coast_path(&horizons, COAST_FRAME_S));
        // A host's own path: the views ahead it announced, in its frame's
        // shape and timing.
        let hosted: Vec<(f64, GlobeCamera)> = self
            .preload
            .iter()
            .filter_map(|v| match v.priority {
                crate::frame_host::Priority::Ahead { in_us } => Some((in_us as f64 / 1e6, v.pose)),
                _ => None,
            })
            .collect();
        let futures = Futures {
            hosted,
            replayed,
            flight: self.flight.map(|f| (f, now)),
            coast,
        };
        let ahead = views_ahead(&mut self.predicting.predictor, t, &camera, &futures);
        if self.budget_window.shown() {
            let rows = self.prediction_rows();
            self.budget_window.set_prediction(rows);
        }
        if ahead.is_empty() && !self.predicting.sent_some {
            return;
        }
        self.predicting.sent_some = !ahead.is_empty();
        let (width, height) = self.viewport();
        let viewport = glam::DVec2::new(width, height);
        let scale = self.active.as_ref().map_or(1.0, |a| a.scale);
        let views: Vec<AheadView> = ahead
            .iter()
            .map(|a| AheadView {
                view: a.camera.view_state(viewport).with_scale(scale),
                in_us: (a.in_s * 1e6).round() as u64,
                p_permille: a.p_permille,
            })
            .collect();
        let r = self.predicting.predictor.reach();
        let reach = Reach {
            ground_mps: r.ground_mps,
            zoom_out_per_s: r.zoom_out_per_s,
        };
        if let Some(session) = self.session.as_mut() {
            let _ = session.stream.send(ClientMessage::Ahead {
                views,
                reach,
                model: self.predicting.digest,
            });
        }
    }

    /// The prediction section of the budget window: where the views ahead
    /// come from, the error measured per horizon, and the predictions used
    /// and unused.
    pub(super) fn prediction_rows(&self) -> Vec<Vec<String>> {
        prediction_rows(&self.predicting.predictor)
    }
}

/// What a frame knows of its camera's future, most known first.
#[derive(Default)]
pub(super) struct Futures {
    /// The views ahead a host announced: seconds ahead, camera.
    pub(super) hosted: Vec<(f64, GlobeCamera)>,
    /// A replay's next frames at the horizons.
    pub(super) replayed: Option<Vec<(f64, GlobeCamera)>>,
    /// A glide under way, and now.
    pub(super) flight: Option<(super::flight::Flight, Instant)>,
    /// A fling's coast at the horizons.
    pub(super) coast: Option<Vec<GlobeCamera>>,
}

/// The views ahead of a frame at `t` drawn from `camera`: a host's path, a
/// replay, a glide, a coast — or the motion model's, a hand's.
pub(super) fn views_ahead(
    predictor: &mut Predictor,
    t: f64,
    camera: &GlobeCamera,
    futures: &Futures,
) -> Vec<Ahead> {
    let horizons = predictor.config().horizons_s.clone();
    let exact = |list: &[(f64, GlobeCamera)], tau: f64| {
        list.iter()
            .find(|(t, _)| (t - tau).abs() < 1e-9)
            .map(|(_, c)| *c)
    };
    let nearest = |list: &[(f64, GlobeCamera)], tau: f64| {
        list.iter()
            .filter(|(t, _)| (t - tau).abs() <= (0.25 * tau).max(0.1))
            .min_by(|a, b| (a.0 - tau).abs().total_cmp(&(b.0 - tau).abs()))
            .map(|(_, c)| *c)
    };
    if !futures.hosted.is_empty() {
        let f = |tau: f64| nearest(&futures.hosted, tau);
        predictor.frame(t, camera, Future::Known(&f))
    } else if let Some(list) = &futures.replayed {
        let f = |tau: f64| exact(list, tau);
        predictor.frame(t, camera, Future::Certain(&f))
    } else if let Some((flight, now)) = futures.flight {
        let f = |tau: f64| {
            let (view, _) = flight.at(now + std::time::Duration::from_secs_f64(tau));
            Some(view.camera())
        };
        predictor.frame(t, camera, Future::Known(&f))
    } else if let Some(path) = &futures.coast {
        let list: Vec<(f64, GlobeCamera)> =
            horizons.iter().copied().zip(path.iter().copied()).collect();
        let f = |tau: f64| exact(&list, tau);
        predictor.frame(t, camera, Future::Known(&f))
    } else {
        predictor.frame(t, camera, Future::Interactive)
    }
}

/// The rows of [`App::prediction_rows`].
pub(super) fn prediction_rows(p: &Predictor) -> Vec<Vec<String>> {
    let m = tuile_core::metrics::metrics();
    let (used, unused) = (m.ahead_used.get(), m.ahead_unused.get());
    let mut out = vec![vec![format!(
        "Prediction — {} · ring tiles predicted: {} used, {} unused ({:.0} % used)",
        match p.source() {
            Source::Still => "still, nothing ahead",
            Source::Known => "known path (glide, coast)",
            Source::Certain => "replay",
            Source::Interactive => "motion model",
        },
        used,
        unused,
        100.0 * used as f64 / (used + unused).max(1) as f64,
    )]];
    out.push(vec![format!("numbers: {}", p.config().to_query())]);
    out.push(
        [
            "horizon",
            "n",
            "ground km p50/p90",
            "heading ° p50/p90",
            "pitch ° p50/p90",
            "height ±% p50/p90",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
    );
    let q = |h: &tuile_camera::predict::Histogram, scale: f64| -> String {
        let f = |v: Option<f64>| match v {
            None => "·".to_owned(),
            Some(v) if v.is_infinite() => "more".to_owned(),
            Some(v) => format!("{:.2}", v * scale),
        };
        format!("{}/{}", f(h.quantile(0.5)), f(h.quantile(0.9)))
    };
    for h in &p.errors().horizons {
        out.push(vec![
            format!("{:.2} s", h.horizon_s),
            h.ground_km.count().to_string(),
            q(&h.ground_km, 1.0),
            q(&h.heading_deg, 1.0),
            q(&h.pitch_deg, 1.0),
            q(&h.height_ratio, 100.0),
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::start::StartView;

    fn view(lon: f64, lat: f64, altitude: f64) -> StartView {
        StartView {
            lon,
            lat,
            altitude,
            heading: 0.0,
            pitch: 60.0,
        }
    }

    /// A glide under way announces where it will be at each horizon, likely
    /// but not certain: a hand may end it.
    #[test]
    fn a_glide_sends_its_own_path_ahead() {
        let mut p = Predictor::new(PredictConfig::default());
        let now = Instant::now();
        let flight = super::super::flight::Flight::new(
            view(2.35, 48.85, 5_000.0),
            view(6.86, 45.83, 6_000.0),
            now,
            4.0,
        );
        let here = flight.at(now).0.camera();
        let futures = Futures {
            flight: Some((flight, now)),
            ..Futures::default()
        };
        let ahead = views_ahead(&mut p, 0.0, &here, &futures);
        assert_eq!(ahead.len(), 4);
        assert_eq!(p.source(), Source::Known);
        for a in &ahead {
            let due = flight
                .at(now + std::time::Duration::from_secs_f64(a.in_s))
                .0
                .camera();
            assert_eq!(a.camera.position.to_array(), due.position.to_array());
            assert!(
                a.p_permille < 1000 && a.p_permille > 500,
                "{}",
                a.p_permille
            );
        }
    }

    /// A replay announces its next frames, certain.
    #[test]
    fn a_replay_sends_its_next_frames_ahead() {
        let mut p = Predictor::new(PredictConfig::default());
        let here = view(6.86, 45.83, 6_000.0).camera();
        let frames: Vec<(f64, GlobeCamera)> = [0.25, 0.5, 1.0, 2.0]
            .iter()
            .map(|t| (*t, view(6.86 + t * 0.01, 45.83, 6_000.0).camera()))
            .collect();
        let futures = Futures {
            replayed: Some(frames.clone()),
            ..Futures::default()
        };
        let ahead = views_ahead(&mut p, 0.0, &here, &futures);
        assert_eq!(p.source(), Source::Certain);
        assert_eq!(
            ahead.iter().map(|a| a.p_permille).collect::<Vec<_>>(),
            vec![1000; 4]
        );
        for (a, (_, c)) in ahead.iter().zip(&frames) {
            assert_eq!(a.camera.position.to_array(), c.position.to_array());
        }
    }

    /// A host's own announced views are what is sent, before anything the
    /// viewer would predict.
    #[test]
    fn a_hosts_views_ahead_come_first() {
        let mut p = Predictor::new(PredictConfig::default());
        let here = view(6.86, 45.83, 6_000.0).camera();
        let there = view(7.0, 45.9, 3_000.0).camera();
        let futures = Futures {
            hosted: vec![(1.0, there)],
            replayed: Some(vec![(1.0, here)]),
            ..Futures::default()
        };
        let ahead = views_ahead(&mut p, 0.0, &here, &futures);
        assert_eq!(ahead.len(), 1);
        assert!((ahead[0].in_s - 1.0).abs() < 1e-12);
        assert_eq!(
            ahead[0].camera.position.to_array(),
            there.position.to_array()
        );
    }

    /// Numbers asked on the route change the very next views ahead: their
    /// horizons and their probabilities, over the context's.
    #[test]
    fn numbers_asked_at_runtime_change_the_next_views_ahead() {
        let mut p = Predictor::new(configured(
            "interactive",
            &crate::steer::PredictAsk::default(),
        ));
        let now = Instant::now();
        let flight = super::super::flight::Flight::new(
            view(2.35, 48.85, 5_000.0),
            view(6.86, 45.83, 6_000.0),
            now,
            4.0,
        );
        let here = flight.at(now).0.camera();
        let futures = Futures {
            flight: Some((flight, now)),
            ..Futures::default()
        };
        let sent = |p: &mut Predictor, t: f64| -> Vec<(u64, u16)> {
            views_ahead(p, t, &here, &futures)
                .iter()
                .map(|a| ((a.in_s * 1000.0).round() as u64, a.p_permille))
                .collect()
        };
        assert_eq!(
            sent(&mut p, 0.0),
            vec![(250, 959), (500, 920), (1000, 846), (2000, 717)]
        );
        let Ok(asked) = crate::steer::parse_url("tuile://budget?horizons=100,3000&interrupt_s=2")
        else {
            panic!("refused");
        };
        let [crate::steer::Command::Predict(Some(ask))] = asked.as_slice() else {
            panic!("{asked:?}");
        };
        let config = configured("interactive", ask);
        let digest = config.digest();
        p.reconfigure(config);
        // e^(−0.1/2), e^(−3/2).
        assert_eq!(sent(&mut p, 0.1), vec![(100, 951), (3000, 223)]);
        // The known-path context: a path rarely interrupted.
        let film = configured("film", &crate::steer::PredictAsk::default());
        assert_ne!(film.digest(), digest);
        p.reconfigure(film);
        assert_eq!(sent(&mut p, 0.2)[3], (2000, 936));
    }

    /// Nothing moving and nothing known, nothing ahead.
    #[test]
    fn a_still_camera_sends_nothing_ahead() {
        let mut p = Predictor::new(PredictConfig::default());
        let here = view(6.86, 45.83, 6_000.0).camera();
        for i in 0..30 {
            assert!(
                views_ahead(&mut p, f64::from(i) / 60.0, &here, &Futures::default()).is_empty()
            );
        }
    }

    /// The section has its header, its columns and a row per horizon.
    #[test]
    fn the_prediction_section_has_a_row_per_horizon() {
        let p = Predictor::new(PredictConfig::default());
        let rows = prediction_rows(&p);
        assert!(rows[0][0].starts_with("Prediction"));
        assert_eq!(rows.len(), 3 + 4);
        assert!(rows[1][0].starts_with("numbers: horizons=250,500,1000,2000&"));
        assert_eq!(rows[3][0], "0.25 s");
    }
}
