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
//! | `step` | `frames` (1) | the state after them |
//! | `settle` | `bound` | `{"frames":n,"settled":bool}` |
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
//! on the next frame.

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

/// The answer to one request.
pub(super) fn answer(run: &mut Headless, request: &Value) -> Value {
    match request.get("v").and_then(Value::as_u64) {
        Some(VERSION) => {}
        other => {
            return failure(&format!(
                "this is version {VERSION} of the protocol; the request said {other:?}"
            ))
        }
    }
    match carry(run, request) {
        Ok(ok) => json!({"v": VERSION, "ok": ok}),
        Err(why) => failure(&why),
    }
}

fn carry(run: &mut Headless, request: &Value) -> Result<Value, String> {
    let cmd = request
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or("a request names its cmd")?;
    let act = |run: &mut Headless, action: Action| {
        run.act(&action);
        Ok(Value::Null)
    };
    match cmd {
        "version" => Ok(json!({"v": VERSION, "commands": COMMANDS})),
        "state" => Ok(run.state().to_json()),
        "metrics" => serde_json::to_value(tuile_core::metrics::metrics().snapshot())
            .map_err(|e| e.to_string()),
        "limits" => {
            let l = run.limits();
            Ok(json!({
                "resident_bytes": l.resident_bytes,
                "resident_tiles": l.resident_tiles,
                "uploads_per_frame": l.uploads_per_frame,
            }))
        }
        "step" => {
            let frames = count(request, "frames", 1)?.max(1);
            Ok(run.steps(frames).to_json())
        }
        "settle" => {
            let settled = run.settle(count(request, "bound", 600)?);
            Ok(json!({"frames": settled.frames, "settled": settled.settled}))
        }
        "go" => {
            let view = View {
                lon: number(request, "lon")?,
                lat: number(request, "lat")?,
                altitude: number(request, "altitude")?,
                heading: number(request, "heading")?,
                pitch: number(request, "pitch")?,
            };
            match request.get("seconds").and_then(Value::as_f64) {
                Some(seconds) => act(run, Action::Glide(view, seconds)),
                None => act(run, Action::Go(view)),
            }
        }
        "url" => {
            let url = request
                .get("url")
                .and_then(Value::as_str)
                .ok_or("url is a steering URL")?;
            act(run, Action::Url(url.to_owned()))
        }
        "touch" => {
            let finger = match request.get("finger").and_then(Value::as_str) {
                Some("down") => Finger::Down,
                Some("moved") => Finger::Moved,
                Some("up") => Finger::Up,
                Some("cancelled") => Finger::Cancelled,
                _ => return Err("finger is down, moved, up or cancelled".into()),
            };
            let id = count(request, "id", 0)?;
            let at = (number(request, "x")?, number(request, "y")?);
            act(run, Action::Touch { id, finger, at })
        }
        "wheel" => {
            let amount = number(request, "amount")?;
            let at = (number(request, "x")?, number(request, "y")?);
            act(run, Action::Wheel { amount, at })
        }
        "resize" => {
            let width = count(request, "width", 0)? as u32;
            let height = count(request, "height", 0)? as u32;
            if width == 0 || height == 0 {
                return Err("width and height are device pixels, above zero".into());
            }
            act(run, Action::Resize { width, height })
        }
        "rescale" => act(run, Action::Rescale(number(request, "scale")?)),
        "memory_warning" => act(run, Action::MemoryWarning),
        "pressure" => match request.get("level").and_then(Value::as_str) {
            Some("warning") => act(run, Action::Pressure(Pressure::Warning)),
            Some("critical") => act(run, Action::Pressure(Pressure::Critical)),
            _ => Err("level is warning or critical".into()),
        },
        "background" => act(run, Action::Background),
        "foreground" => act(run, Action::Foreground),
        "picture" => picture(run, request),
        "close" => Ok(Value::Null),
        other => Err(format!(
            "{other:?} is not a command; the commands are {}",
            COMMANDS.join(", ")
        )),
    }
}

/// Writes eye `eye`'s last picture to the PNG `path` names.
fn picture(run: &mut Headless, request: &Value) -> Result<Value, String> {
    let eye = count(request, "eye", 0)? as usize;
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .ok_or("path is where the PNG goes")?;
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
