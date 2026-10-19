// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The control API over a socket: for scripts, agents, and the device tests
//! that drive a build from outside.
//!
//! TCP on a **loopback address only** — anything else is refused before a
//! socket is opened — one JSON object a line each way. A request names its
//! version and its command; the answer carries the version and either `ok`
//! or `error`:
//!
//! ```text
//! → {"v":1,"cmd":"step","frames":60}
//! ← {"v":1,"ok":{"frame":60,"drew":true,"view":{…},"metrics":{…},…}}
//! → {"v":1,"cmd":"go","lon":2.35,"lat":48.85,"altitude":5000,"heading":0,"pitch":60}
//! ← {"v":1,"ok":null}
//! ```
//!
//! # Version 1
//!
//! | `cmd` | arguments | answers |
//! |---|---|---|
//! | `version` | | `{"v":1,"commands":[…]}` |
//! | `state` | | the last step's state (see [`super::State`]) |
//! | `metrics` | | the engine's numbers now, absolute: `tuile_core::metrics::Snapshot` |
//! | `limits` | | the profile's limits |
//! | `step` | `frames` (1): refreshes of the run's display | the state after them |
//! | `settle` | `seconds` (10): the bound | `{"frames":n,"seconds":s,"settled":bool}` |
//! | `go` | `lon` `lat` `altitude` `heading` `pitch`, `seconds` to glide | `null` |
//! | `url` | `url`: a steering URL | `null` |
//! | `touch` | `id`, `finger` (`down` `moved` `up` `cancelled`), `x` `y` as fractions | `null` |
//! | `wheel` | `amount`, `x` `y` as fractions | `null` |
//! | `resize` | `width` `height`, device pixels | `null` |
//! | `rescale` | `scale` | `null` |
//! | `memory_warning` | | `null` |
//! | `pressure` | `level`: `warning` or `critical` | `null` |
//! | `background`, `foreground` | | `null` |
//! | `picture` | `eye` (0), `path`: where to write the PNG | `{"path","width","height","ground","black"}` |
//! | `close` | | `null`, and [`super::Headless::serve`] returns |
//!
//! Moves and inputs take effect on the next step, as they would in a window
//! on the next frame. A run is in real time: between two requests it waits,
//! as a window does between two frames nobody asked for.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use serde_json::{json, Value};

use super::{Action, Finger, Headless, View};
use crate::control::series::Pressure;

/// The protocol's version.
pub const VERSION: u64 = 1;

/// Every command of [`VERSION`].
pub const COMMANDS: [&str; 18] = [
    "version",
    "state",
    "metrics",
    "limits",
    "step",
    "settle",
    "go",
    "url",
    "touch",
    "wheel",
    "resize",
    "rescale",
    "memory_warning",
    "pressure",
    "background",
    "foreground",
    "picture",
    "close",
];

/// A request, and where its answer goes.
pub(super) type Pending = (Value, Sender<Value>);

/// The socket, listening: requests arrive here from its connections.
pub(super) struct Listener {
    requests: Receiver<Pending>,
    pub(super) address: SocketAddr,
    /// Set when the listener goes, so its thread stops accepting.
    closed: Arc<AtomicBool>,
}

impl Drop for Listener {
    /// Stops the thread that accepts connections: it is woken by one last
    /// connection of our own, finds the socket closed, and returns.
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
    }
}

impl Listener {
    /// Every request waiting, without waiting for one.
    pub(super) fn pending(&self) -> Vec<Pending> {
        self.requests.try_iter().collect()
    }

    /// The next request, waiting for it; `None` once nobody can send one.
    pub(super) fn next(&self) -> Option<Pending> {
        self.requests.recv().ok()
    }
}

/// Opens the socket on `address`, which must be a loopback one.
pub(super) fn listen(address: SocketAddr) -> std::io::Result<Listener> {
    if !address.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the control socket listens on loopback only, not {address}"),
        ));
    }
    let socket = TcpListener::bind(address)?;
    let address = socket.local_addr()?;
    let (tx, requests) = channel();
    let closed = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&closed);
    std::thread::Builder::new()
        .name("tuile-control".into())
        .spawn(move || {
            for connection in socket.incoming().flatten() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let tx = tx.clone();
                let _ = std::thread::Builder::new()
                    .name("tuile-control-connection".into())
                    .spawn(move || converse(connection, &tx));
            }
        })?;
    tracing::info!(%address, "control socket");
    Ok(Listener {
        requests,
        address,
        closed,
    })
}

/// One connection: a line in, a line out, until it closes.
fn converse(connection: TcpStream, requests: &Sender<Pending>) {
    let Ok(mut out) = connection.try_clone() else {
        return;
    };
    for line in BufReader::new(connection).lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(request) => {
                let (reply, answered) = channel();
                if requests.send((request, reply)).is_err() {
                    return;
                }
                answered
                    .recv()
                    .unwrap_or_else(|_| failure("the viewer has gone"))
            }
            Err(e) => failure(&format!("not JSON: {e}")),
        };
        if writeln!(out, "{answer}").is_err() {
            return;
        }
    }
}

fn failure(why: &str) -> Value {
    json!({"v": VERSION, "error": why})
}

fn number(request: &Value, key: &str) -> Result<f64, String> {
    request
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{key} is a number, and is required"))
}

fn count(request: &Value, key: &str, or: u64) -> Result<u64, String> {
    match request.get(key) {
        None => Ok(or),
        Some(v) => v.as_u64().ok_or_else(|| format!("{key} is a count")),
    }
}

/// One request, read and checked: what [`answer`] carries out.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Version,
    State,
    Metrics,
    Limits,
    Step { frames: u64 },
    Settle { seconds: f64 },
    Act(Action),
    Picture { eye: usize, path: String },
    Close,
}

/// Reads a request: its version, its command and the command's arguments,
/// or says what is wrong with it. Nothing is done.
pub fn read(request: &Value) -> Result<Request, String> {
    match request.get("v").and_then(Value::as_u64) {
        Some(VERSION) => {}
        other => {
            return Err(format!(
                "this is version {VERSION} of the protocol; the request said {other:?}"
            ))
        }
    }
    let cmd = request
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or("a request names its cmd")?;
    let view = || -> Result<View, String> {
        Ok(View {
            lon: number(request, "lon")?,
            lat: number(request, "lat")?,
            altitude: number(request, "altitude")?,
            heading: number(request, "heading")?,
            pitch: number(request, "pitch")?,
        })
    };
    Ok(match cmd {
        "version" => Request::Version,
        "state" => Request::State,
        "metrics" => Request::Metrics,
        "limits" => Request::Limits,
        "step" => Request::Step {
            frames: count(request, "frames", 1)?.max(1),
        },
        "settle" => Request::Settle {
            seconds: match request.get("seconds") {
                None => 10.0,
                Some(v) => v.as_f64().ok_or("seconds is a number")?,
            },
        },
        "go" => Request::Act(match request.get("seconds").and_then(Value::as_f64) {
            Some(seconds) => Action::Glide(view()?, seconds),
            None => Action::Go(view()?),
        }),
        "url" => Request::Act(Action::Url(
            request
                .get("url")
                .and_then(Value::as_str)
                .ok_or("url is a steering URL")?
                .to_owned(),
        )),
        "touch" => {
            let finger = match request.get("finger").and_then(Value::as_str) {
                Some("down") => Finger::Down,
                Some("moved") => Finger::Moved,
                Some("up") => Finger::Up,
                Some("cancelled") => Finger::Cancelled,
                _ => return Err("finger is down, moved, up or cancelled".into()),
            };
            Request::Act(Action::Touch {
                id: count(request, "id", 0)?,
                finger,
                at: (number(request, "x")?, number(request, "y")?),
            })
        }
        "wheel" => Request::Act(Action::Wheel {
            amount: number(request, "amount")?,
            at: (number(request, "x")?, number(request, "y")?),
        }),
        "resize" => {
            let side = |key| -> Result<u32, String> {
                u32::try_from(count(request, key, 0)?)
                    .ok()
                    .filter(|v| *v > 0)
                    .ok_or_else(|| format!("{key} is device pixels, above zero"))
            };
            Request::Act(Action::Resize {
                width: side("width")?,
                height: side("height")?,
            })
        }
        "rescale" => Request::Act(Action::Rescale(number(request, "scale")?)),
        "memory_warning" => Request::Act(Action::MemoryWarning),
        "pressure" => Request::Act(Action::Pressure(
            match request.get("level").and_then(Value::as_str) {
                Some("warning") => Pressure::Warning,
                Some("critical") => Pressure::Critical,
                _ => return Err("level is warning or critical".into()),
            },
        )),
        "background" => Request::Act(Action::Background),
        "foreground" => Request::Act(Action::Foreground),
        "picture" => Request::Picture {
            eye: usize::try_from(count(request, "eye", 0)?).map_err(|e| e.to_string())?,
            path: request
                .get("path")
                .and_then(Value::as_str)
                .ok_or("path is where the PNG goes")?
                .to_owned(),
        },
        "close" => Request::Close,
        other => {
            return Err(format!(
                "{other:?} is not a command; the commands are {}",
                COMMANDS.join(", ")
            ))
        }
    })
}

/// The answer to one request.
pub(super) fn answer(run: &mut Headless, request: &Value) -> Value {
    match read(request).and_then(|request| carry(run, request)) {
        Ok(ok) => json!({"v": VERSION, "ok": ok}),
        Err(why) => failure(&why),
    }
}

fn carry(run: &mut Headless, request: Request) -> Result<Value, String> {
    match request {
        Request::Version => Ok(json!({"v": VERSION, "commands": COMMANDS})),
        Request::State => Ok(run.state().to_json()),
        Request::Metrics => serde_json::to_value(tuile_core::metrics::metrics().snapshot())
            .map_err(|e| e.to_string()),
        Request::Limits => {
            let l = run.limits();
            Ok(json!({
                "resident_bytes": l.resident_bytes,
                "resident_tiles": l.resident_tiles,
                "uploads_per_frame": l.uploads_per_frame,
            }))
        }
        Request::Step { frames } => Ok(run.steps(frames).to_json()),
        Request::Settle { seconds } => {
            let settled = run.settle(seconds);
            Ok(json!({
                "frames": settled.frames,
                "seconds": settled.seconds,
                "settled": settled.settled,
            }))
        }
        Request::Act(action) => {
            run.act(&action);
            Ok(Value::Null)
        }
        Request::Picture { eye, path } => picture(run, eye, &path),
        Request::Close => Ok(Value::Null),
    }
}

/// Writes eye `eye`'s last picture to the PNG at `path`.
fn picture(run: &mut Headless, eye: usize, path: &str) -> Result<Value, String> {
    let (pixels, (width, height)) = run
        .picture(eye)
        .ok_or_else(|| format!("eye {eye} has drawn nothing"))?;
    image::save_buffer(path, pixels, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| e.to_string())?;
    let reading = run.state().eyes.get(eye).copied().unwrap_or_default();
    Ok(json!({
        "path": path,
        "width": width,
        "height": height,
        "ground": reading.ground,
        "black": reading.black,
    }))
}

impl Headless {
    /// Serves the control API on `address`, a loopback one; port 0 picks a
    /// free one. Returns where it listens. Requests are answered between
    /// steps — on every [`Headless::step`], or by [`Headless::serve`].
    pub fn listen(&mut self, address: SocketAddr) -> std::io::Result<SocketAddr> {
        let listener = listen(address)?;
        let address = listener.address;
        self.socket = Some(listener);
        Ok(address)
    }

    /// Answers requests, waiting for each, until one says `close` or the
    /// socket goes: the run driven entirely from outside.
    pub fn serve(&mut self) {
        loop {
            let Some((request, reply)) = self.socket.as_ref().and_then(Listener::next) else {
                return;
            };
            let close = request.get("cmd").and_then(Value::as_str) == Some("close");
            let _ = reply.send(answer(self, &request));
            if close {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Request, String> {
        read(&serde_json::from_str(text).expect("JSON"))
    }

    /// Each command reads into what it asks, with its defaults.
    #[test]
    fn a_request_reads_into_its_command() {
        assert_eq!(parse(r#"{"v":1,"cmd":"version"}"#), Ok(Request::Version));
        assert_eq!(
            parse(r#"{"v":1,"cmd":"step"}"#),
            Ok(Request::Step { frames: 1 })
        );
        assert_eq!(
            parse(r#"{"v":1,"cmd":"settle","seconds":2.5}"#),
            Ok(Request::Settle { seconds: 2.5 })
        );
        assert_eq!(
            parse(
                r#"{"v":1,"cmd":"go","lon":2,"lat":48,"altitude":5000,"heading":0,"pitch":60,"seconds":1.5}"#
            ),
            Ok(Request::Act(Action::Glide(
                View {
                    lon: 2.0,
                    lat: 48.0,
                    altitude: 5000.0,
                    heading: 0.0,
                    pitch: 60.0
                },
                1.5
            )))
        );
        assert_eq!(
            parse(r#"{"v":1,"cmd":"touch","id":3,"finger":"moved","x":0.5,"y":0.25}"#),
            Ok(Request::Act(Action::Touch {
                id: 3,
                finger: Finger::Moved,
                at: (0.5, 0.25)
            }))
        );
        assert_eq!(
            parse(r#"{"v":1,"cmd":"pressure","level":"critical"}"#),
            Ok(Request::Act(Action::Pressure(Pressure::Critical)))
        );
        assert_eq!(
            parse(r#"{"v":1,"cmd":"picture","path":"/tmp/eye.png"}"#),
            Ok(Request::Picture {
                eye: 0,
                path: "/tmp/eye.png".into()
            })
        );
    }

    /// What is not version 1, not a command, or not complete is refused in
    /// words that say why — and nothing is done.
    #[test]
    fn a_bad_request_is_refused_with_its_reason() {
        assert!(parse(r#"{"v":2,"cmd":"state"}"#).is_err_and(|e| e.contains("version 1")));
        assert!(parse(r#"{"cmd":"state"}"#).is_err());
        assert!(parse(r#"{"v":1,"cmd":"teleport"}"#).is_err_and(|e| e.contains("teleport")));
        assert!(parse(r#"{"v":1,"cmd":"go","lon":2}"#).is_err_and(|e| e.contains("lat")));
        assert!(parse(r#"{"v":1,"cmd":"resize","width":0,"height":10}"#)
            .is_err_and(|e| e.contains("width")));
        assert!(parse(r#"{"v":1,"cmd":"touch","finger":"pinch","x":0,"y":0}"#).is_err());
        assert!(parse(r#"{"v":1,"cmd":"pressure","level":"mild"}"#).is_err());
    }

    /// The socket opens on loopback only, and refuses anything else before
    /// it opens.
    #[test]
    fn the_socket_listens_on_loopback_only() {
        assert!(listen("0.0.0.0:0".parse().expect("an address")).is_err());
        let listener = listen("127.0.0.1:0".parse().expect("an address")).expect("listens");
        assert!(listener.address.ip().is_loopback());
    }
}
