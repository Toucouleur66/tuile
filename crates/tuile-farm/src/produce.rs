// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A whole film on one machine: baked in packs, rendered in parts, joined.
//!
//! ```text
//! frames  1 ───────────────────────────────────────────────── 18000
//! packs   [   1–150   ][  151–300  ][  301–450  ] …   baked, each once
//! parts   [1–50][51–100][101–150][151–200] …          rendered, each to a segment
//! film    ═══════════════════════════════════════     the segments, joined
//! ```
//!
//! Each pack is a lane: its bake, then its parts one after the other. Several
//! lanes run at once ([`Order::at_once`]). A bake and a render are processes
//! of their own, run under [`crate::watch`]: one that stalls or outgrows its
//! memory is killed and tried again, and a lane that fails does not stop the
//! others.
//!
//! # What counts as done
//!
//! A part is done when its **receipt** says so: written after the segment was
//! opened and found to hold exactly the part's frames, naming its size. Not
//! the segment's presence — a render killed mid-way leaves a file behind —
//! and not the process's exit code alone. A pack is done when it is at its
//! place, where it is moved only once its bake has ended well.
//!
//! So a production can be stopped at any moment and asked for again: what is
//! done is kept, what is not is done. And the film is joined only when every
//! part is there, and must hold every frame: a film with a hole in it is not
//! written, the holes are named.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use futures_util::StreamExt;
use tokio::process::Command;

use crate::concat;
use crate::watch::{supervise, Ended, Limits, Ran};

/// Frames `first..=last`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Stretch {
    pub first: u32,
    pub last: u32,
}

impl Stretch {
    pub fn frames(&self) -> u64 {
        u64::from(self.last - self.first) + 1
    }
}

impl std::fmt::Display for Stretch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

/// What to produce, and where.
#[derive(Debug, Clone)]
pub struct Order {
    pub frames: Stretch,
    /// Frames to a pack, and to a part. A part never straddles two packs.
    pub pack_frames: u32,
    pub part_frames: u32,
    /// Packs worked on at once: so many bakes or renders run together.
    pub at_once: usize,
    /// How many times a bake or a render is tried.
    pub tries: u32,
    /// Where the packs are kept, as `<first>-<last>.tuilepack`.
    pub packs: PathBuf,
    /// Segments, receipts, logs and the journal.
    pub work: PathBuf,
    /// The film.
    pub out: PathBuf,
    /// The film's cadence, checked against what was joined. `None`: not checked.
    pub fps: Option<f64>,
}

/// One pack and the parts rendered from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub pack: Stretch,
    pub parts: Vec<Stretch>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProduceError {
    #[error("nothing to produce: {0}")]
    Order(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} of {} parts are missing, the film is not written: frames {}", .missing.len(), .parts, named(.missing))]
    Missing { missing: Vec<Stretch>, parts: usize },
    #[error("stopped: {0} is there")]
    Stopped(PathBuf),
    #[error("the film: {0}")]
    Film(String),
}

fn named(stretches: &[Stretch]) -> String {
    stretches
        .iter()
        .map(Stretch::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The lanes of an order: its packs in order, each with its parts in order.
pub fn plan(order: &Order) -> Result<Vec<Lane>, ProduceError> {
    let Stretch { first, last } = order.frames;
    if first > last {
        return Err(ProduceError::Order(format!("frames {first}-{last}")));
    }
    if order.pack_frames == 0 || order.part_frames == 0 {
        return Err(ProduceError::Order("a pack or a part of no frame".into()));
    }
    let cut = |within: Stretch, by: u32| -> Vec<Stretch> {
        let mut out = Vec::new();
        let mut at = within.first;
        loop {
            let end = at.saturating_add(by - 1).min(within.last);
            out.push(Stretch {
                first: at,
                last: end,
            });
            if end == within.last {
                return out;
            }
            at = end + 1;
        }
    };
    Ok(cut(order.frames, order.pack_frames)
        .into_iter()
        .map(|pack| Lane {
            pack,
            parts: cut(pack, order.part_frames),
        })
        .collect())
}

/// A step of the production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Bake,
    Render,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Bake => "bake",
            Step::Render => "render",
        }
    }
}

/// What knows how a pack is baked and a part rendered: the commands, and
/// nothing of how they are run.
pub trait Host: Send + Sync {
    /// Bakes `frames` into the pack at `out`.
    fn bake(&self, frames: Stretch, out: &Path) -> Command;
    /// Renders `frames` to the segment at `out`. Their pack is at its place.
    fn render(&self, frames: Stretch, out: &Path) -> Command;
    /// After a failed try whose output is in `log`: something to run before
    /// the next, or `None`. If it fails, the step is not tried again.
    fn mend(&self, _step: Step, _frames: Stretch, _log: &Path) -> Option<Command> {
        None
    }
}

/// What is said as the production goes: on standard output, and kept in
/// `journal.log` of the work directory.
pub struct Journal {
    file: Mutex<std::fs::File>,
}

impl Journal {
    pub fn open(work: &Path) -> Result<Self, ProduceError> {
        let path = work.join("journal.log");
        let io = |source| ProduceError::Io {
            path: path.clone(),
            source,
        };
        std::fs::create_dir_all(work).map_err(io)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn say(&self, line: &str) {
        use std::io::Write;
        let line = format!(
            "{} {line}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        println!("{line}");
        if let Ok(mut file) = self.file.lock() {
            // A journal that cannot be written does not stop a film.
            let _ = writeln!(file, "{line}");
        }
    }
}

/// A part's receipt: beside its segment, written once the segment was found
/// whole.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Receipt {
    pub frames: Stretch,
    /// The segment's size: a segment of another size is not the one vouched for.
    pub bytes: u64,
    pub tries: u32,
    pub seconds: f64,
    pub peak_mb: u64,
}

struct Places<'a> {
    order: &'a Order,
}

impl Places<'_> {
    fn pack(&self, frames: Stretch) -> PathBuf {
        self.order.packs.join(format!("{frames}.tuilepack"))
    }
    fn baking(&self, frames: Stretch) -> PathBuf {
        self.order
            .work
            .join("baking")
            .join(format!("{frames}.tuilepack"))
    }
    fn segment(&self, frames: Stretch) -> PathBuf {
        self.order
            .work
            .join("segments")
            .join(format!("{frames}.mp4"))
    }
    fn receipt(&self, frames: Stretch) -> PathBuf {
        self.order
            .work
            .join("segments")
            .join(format!("{frames}.json"))
    }
    fn log(&self, step: Step, frames: Stretch, name: &str) -> PathBuf {
        self.order
            .work
            .join("logs")
            .join(format!("{}-{frames}.{name}.log", step.name()))
    }
    fn stop(&self) -> PathBuf {
        self.order.work.join("STOP")
    }
}

/// Whether the part is done: its receipt, and the segment it vouches for.
fn done(places: &Places<'_>, frames: Stretch) -> bool {
    let Ok(text) = std::fs::read(places.receipt(frames)) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<Receipt>(&text) else {
        return false;
    };
    let size = std::fs::metadata(places.segment(frames))
        .map(|m| m.len())
        .ok();
    receipt.frames == frames && size == Some(receipt.bytes)
}

/// A production under way: what was asked, of whom, within what.
#[derive(Clone, Copy)]
struct Run<'a> {
    order: &'a Order,
    host: &'a dyn Host,
    limits: &'a Limits,
    journal: &'a Journal,
}

/// Runs a step until it ends well and `whole` finds its output so, at most
/// [`Order::tries`] times. `None` when it never did.
async fn attempt(
    run: &Run<'_>,
    step: Step,
    frames: Stretch,
    command: impl Fn() -> Command,
    whole: impl Fn() -> Result<(), String>,
) -> Option<(u32, Ran)> {
    let Run {
        order,
        host,
        limits,
        journal,
    } = *run;
    let places = Places { order };
    for tried in 1..=order.tries.max(1) {
        let log = places.log(step, frames, &tried.to_string());
        let said = |ran: &Ran, how: &str| {
            journal.say(&format!(
                "{} {frames} try {tried}: {how} — {:.0} s, peak {} MB — {}",
                step.name(),
                ran.seconds,
                ran.peak_bytes >> 20,
                log.display()
            ));
        };
        let ran = match supervise(command(), &log, limits).await {
            Ok(ran) => ran,
            Err(e) => {
                journal.say(&format!(
                    "{} {frames} try {tried}: not run: {e}",
                    step.name()
                ));
                return None;
            }
        };
        match ran.ended {
            Ended::Done => match whole() {
                Ok(()) => {
                    said(&ran, "done");
                    return Some((tried, ran));
                }
                Err(why) => said(
                    &ran,
                    &format!("ended well, and its output is not whole: {why}"),
                ),
            },
            other => said(&ran, &other.to_string()),
        }
        if tried == order.tries.max(1) {
            break;
        }
        if let Some(mend) = host.mend(step, frames, &log) {
            let mend_log = places.log(step, frames, &format!("{tried}.mend"));
            match supervise(mend, &mend_log, limits).await {
                Ok(Ran {
                    ended: Ended::Done,
                    seconds,
                    ..
                }) => {
                    journal.say(&format!(
                        "{} {frames}: mended in {seconds:.0} s",
                        step.name()
                    ));
                }
                Ok(ran) => {
                    journal.say(&format!(
                        "{} {frames}: not mended ({}), not tried again — {}",
                        step.name(),
                        ran.ended,
                        mend_log.display()
                    ));
                    return None;
                }
                Err(e) => {
                    journal.say(&format!(
                        "{} {frames}: the mending was not run: {e}",
                        step.name()
                    ));
                    return None;
                }
            }
        }
    }
    None
}

/// One lane: the pack if a part needs it, then the parts not yet done. Gives
/// back the parts still missing.
async fn work(run: &Run<'_>, lane: &Lane) -> Result<Vec<Stretch>, ProduceError> {
    let Run { order, host, .. } = *run;
    let places = Places { order };
    let wanted: Vec<Stretch> = lane
        .parts
        .iter()
        .copied()
        .filter(|p| !done(&places, *p))
        .collect();
    if wanted.is_empty() {
        return Ok(wanted);
    }
    if places.stop().exists() {
        return Err(ProduceError::Stopped(places.stop()));
    }
    let pack = places.pack(lane.pack);
    if !pack.exists() {
        let baking = places.baking(lane.pack);
        for dir in [baking.parent(), pack.parent()].into_iter().flatten() {
            std::fs::create_dir_all(dir).map_err(|source| ProduceError::Io {
                path: dir.into(),
                source,
            })?;
        }
        let baked = attempt(
            run,
            Step::Bake,
            lane.pack,
            || host.bake(lane.pack, &baking),
            || match std::fs::metadata(&baking) {
                Ok(m) if m.len() > 0 => Ok(()),
                Ok(_) => Err("an empty pack".into()),
                Err(e) => Err(format!("no pack: {e}")),
            },
        )
        .await;
        if baked.is_none() {
            return Ok(wanted);
        }
        // At its place only now: a pack found there is a pack baked whole.
        std::fs::rename(&baking, &pack).map_err(|source| ProduceError::Io {
            path: pack.clone(),
            source,
        })?;
    }
    let mut missing = Vec::new();
    for part in wanted {
        if places.stop().exists() {
            return Err(ProduceError::Stopped(places.stop()));
        }
        let segment = places.segment(part);
        if let Some(dir) = segment.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ProduceError::Io {
                path: dir.into(),
                source,
            })?;
        }
        // A receipt that vouches for nothing, or a segment with no receipt:
        // neither is kept beside the new try.
        let _ = std::fs::remove_file(places.receipt(part));
        let rendered = attempt(
            run,
            Step::Render,
            part,
            || {
                let _ = std::fs::remove_file(&segment);
                host.render(part, &segment)
            },
            || match concat::count_frames(&segment) {
                Ok(frames) if frames == part.frames() => Ok(()),
                Ok(frames) => Err(format!("{frames} frames of {}", part.frames())),
                Err(e) => Err(e.to_string()),
            },
        )
        .await;
        let Some((tries, ran)) = rendered else {
            missing.push(part);
            continue;
        };
        let receipt = Receipt {
            frames: part,
            bytes: std::fs::metadata(&segment)
                .map_err(|source| ProduceError::Io {
                    path: segment.clone(),
                    source,
                })?
                .len(),
            tries,
            seconds: ran.seconds,
            peak_mb: ran.peak_bytes >> 20,
        };
        let path = places.receipt(part);
        let json =
            serde_json::to_vec_pretty(&receipt).map_err(|e| ProduceError::Film(e.to_string()))?;
        std::fs::write(&path, json).map_err(|source| ProduceError::Io { path, source })?;
    }
    Ok(missing)
}

/// The film an order made.
#[derive(Debug, Clone, PartialEq)]
pub struct Film {
    pub frames: u64,
    pub parts: usize,
    pub seconds: f64,
}

/// Produces the film of `order`: bakes what is not baked, renders what is not
/// rendered, and joins the parts once all are there.
///
/// Stopped by a file named `STOP` in the work directory: nothing new is
/// begun, what is running ends, and the same order asked again goes on from
/// there.
pub async fn produce(
    order: &Order,
    host: &dyn Host,
    limits: &Limits,
    journal: &Journal,
) -> Result<Film, ProduceError> {
    let lanes = plan(order)?;
    let places = Places { order };
    let parts: Vec<Stretch> = lanes.iter().flat_map(|l| l.parts.iter().copied()).collect();
    let kept = parts.iter().filter(|p| done(&places, **p)).count();
    journal.say(&format!(
        "frames {}: {} packs, {} parts ({kept} already done), {} at once, {} tries each",
        order.frames,
        lanes.len(),
        parts.len(),
        order.at_once.max(1),
        order.tries.max(1)
    ));

    let mut missing = Vec::new();
    let mut stopped = None;
    let run = Run {
        order,
        host,
        limits,
        journal,
    };
    let mut running = futures_util::stream::iter(&lanes)
        .map(|lane| work(&run, lane))
        .buffer_unordered(order.at_once.max(1));
    while let Some(ended) = running.next().await {
        match ended {
            Ok(some) => missing.extend(some),
            // The lanes already running end by themselves; none is begun.
            Err(ProduceError::Stopped(by)) => stopped = Some(by),
            Err(e) => return Err(e),
        }
    }
    drop(running);
    if let Some(by) = stopped {
        journal.say(&format!("stopped: {} is there", by.display()));
        return Err(ProduceError::Stopped(by));
    }
    if !missing.is_empty() {
        missing.sort();
        let e = ProduceError::Missing {
            missing,
            parts: parts.len(),
        };
        journal.say(&e.to_string());
        return Err(e);
    }

    // Joined beside its place, and moved there whole.
    let segments: Vec<PathBuf> = parts.iter().map(|p| places.segment(*p)).collect();
    let joining = order.work.join("film.joining.mp4");
    let film = |e: String| ProduceError::Film(e);
    let frames = concat::concat(&segments, &joining).map_err(|e| film(e.to_string()))?;
    if frames != order.frames.frames() {
        return Err(film(format!(
            "{frames} frames joined of {}",
            order.frames.frames()
        )));
    }
    let probe = concat::probe(&joining).map_err(|e| film(e.to_string()))?;
    if probe.frames != frames {
        return Err(film(format!(
            "{} frames in the film of {frames} joined",
            probe.frames
        )));
    }
    if let Some(fps) = order.fps {
        concat::check_cadence(probe, fps).map_err(film)?;
    }
    if let Some(dir) = order.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|source| ProduceError::Io {
            path: dir.into(),
            source,
        })?;
    }
    std::fs::rename(&joining, &order.out).map_err(|source| ProduceError::Io {
        path: order.out.clone(),
        source,
    })?;
    journal.say(&format!(
        "film: {} — {frames} frames, {:.1} s, {} parts",
        order.out.display(),
        probe.seconds,
        parts.len()
    ));
    Ok(Film {
        frames,
        parts: parts.len(),
        seconds: probe.seconds,
    })
}

/// A host whose commands are written out: a program and its arguments, with
/// `{first}`, `{last}` and `{out}` standing for the frames and the output.
/// A command that names no frame is given `--frames <first>:<last>`, and one
/// that names no output `--out <file>`, after its own arguments.
/// Split on spaces before anything is put in, so a path put in may hold
/// spaces and a template may not quote.
#[derive(Debug, Clone, Default)]
pub struct Templates {
    pub bake: String,
    pub render: String,
    /// Run after a failed render whose log names tiles the store lacks
    /// (`not in the tile store: <layer> <level>/<x>/<y>`, one a line):
    /// `{list}` stands for a file of those names, `<layer> <level>/<x>/<y>`
    /// a line. What it does about them is the host's own business.
    pub on_absent: Option<String>,
}

/// What a render prints for each tile it needs and the store does not hold.
pub const ABSENT: &str = "not in the tile store: ";

impl Templates {
    /// `template`'s command, `{name}` replaced by its value in every word.
    pub fn command(template: &str, values: &[(&str, &str)]) -> Result<Command, String> {
        let mut words = template.split_whitespace().map(|word| {
            values.iter().fold(word.to_string(), |word, (name, value)| {
                word.replace(&format!("{{{name}}}"), value)
            })
        });
        let program = words.next().ok_or("an empty command")?;
        let mut command = Command::new(program);
        command.args(words);
        Ok(command)
    }

    fn filled(template: &str, frames: Stretch, out: &Path) -> Command {
        let (first, last) = (frames.first.to_string(), frames.last.to_string());
        let out = out.to_string_lossy();
        let values = [
            ("first", first.as_str()),
            ("last", last.as_str()),
            ("out", &*out),
        ];
        // An empty template is refused when the order is read; here it
        // becomes a command that cannot be run, and is said so.
        let mut command = Self::command(template, &values).unwrap_or_else(|_| Command::new(""));
        // What the template leaves unsaid goes the way the engine's own
        // programs take it.
        if !template.contains("{first}") && !template.contains("{last}") {
            command.arg("--frames").arg(format!("{first}:{last}"));
        }
        if !template.contains("{out}") {
            command.arg("--out").arg(&*out);
        }
        command
    }
}

impl Host for Templates {
    fn bake(&self, frames: Stretch, out: &Path) -> Command {
        Self::filled(&self.bake, frames, out)
    }

    fn render(&self, frames: Stretch, out: &Path) -> Command {
        Self::filled(&self.render, frames, out)
    }

    fn mend(&self, step: Step, _frames: Stretch, log: &Path) -> Option<Command> {
        let template = self.on_absent.as_ref().filter(|_| step == Step::Render)?;
        let said = std::fs::read_to_string(log).ok()?;
        let absent: Vec<&str> = said
            .lines()
            .filter_map(|line| line.strip_prefix(ABSENT))
            .collect();
        if absent.is_empty() {
            return None;
        }
        let list = log.with_extension("absent");
        std::fs::write(&list, absent.join("\n") + "\n").ok()?;
        Self::command(template, &[("list", &*list.to_string_lossy())]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x5e];

    fn order(dir: &Path, frames: (u32, u32)) -> Order {
        Order {
            frames: Stretch {
                first: frames.0,
                last: frames.1,
            },
            pack_frames: 30,
            part_frames: 10,
            at_once: 1,
            tries: 3,
            packs: dir.join("packs"),
            work: dir.join("work"),
            out: dir.join("out/film.mp4"),
            fps: None,
        }
    }

    fn limits() -> Limits {
        Limits {
            memory_bytes: None,
            stall: Duration::from_secs(60),
            every: Duration::from_millis(50),
        }
    }

    /// A host made of shell lines. `$first`, `$last`, `$out` and `$n` (the
    /// part's frames) are the step's; `$here` is the test's directory, where
    /// `seg-<n>.mp4` is a segment of n frames.
    struct Shell {
        here: PathBuf,
        bake: String,
        render: String,
        mend: Option<String>,
    }

    impl Shell {
        fn new(here: &Path) -> Self {
            for frames in [5u32, 7, 10] {
                concat::write_test_segment(&here.join(format!("seg-{frames}.mp4")), frames, SPS, 1)
                    .expect("segment");
            }
            Self {
                here: here.to_path_buf(),
                bake: "echo baked >> $here/bakes; echo pack > $out".into(),
                render: "echo $first-$last >> $here/renders; cp $here/seg-$n.mp4 $out".into(),
                mend: None,
            }
        }

        fn line(&self, script: &str, frames: Stretch, out: &Path) -> Command {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(script)
                .env("here", &self.here)
                .env("first", frames.first.to_string())
                .env("last", frames.last.to_string())
                .env("n", frames.frames().to_string())
                .env("out", out);
            command
        }
    }

    impl Host for Shell {
        fn bake(&self, frames: Stretch, out: &Path) -> Command {
            self.line(&self.bake, frames, out)
        }
        fn render(&self, frames: Stretch, out: &Path) -> Command {
            self.line(&self.render, frames, out)
        }
        fn mend(&self, _: Step, frames: Stretch, log: &Path) -> Option<Command> {
            let mut command = self.line(self.mend.as_ref()?, frames, Path::new(""));
            command.env("log", log);
            Some(command)
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn parts_cover_the_frames_once_and_never_straddle_two_packs() {
        let dir = Path::new("/nowhere");
        let lanes = plan(&Order {
            pack_frames: 150,
            part_frames: 50,
            ..order(dir, (1, 18000))
        })
        .expect("plan");
        assert_eq!(lanes.len(), 120);
        assert_eq!(
            lanes[0].pack,
            Stretch {
                first: 1,
                last: 150
            }
        );
        assert_eq!(
            lanes[0].parts,
            [(1, 50), (51, 100), (101, 150)].map(|(first, last)| Stretch { first, last })
        );
        assert_eq!(
            lanes[119].pack,
            Stretch {
                first: 17851,
                last: 18000
            }
        );

        // Sizes that divide nothing: every frame still once, in order, and
        // each part within its pack.
        let lanes = plan(&Order {
            pack_frames: 7,
            part_frames: 3,
            ..order(dir, (5, 30))
        })
        .expect("plan");
        let mut next = 5;
        for lane in &lanes {
            assert!(lane.pack.frames() <= 7);
            assert_eq!(lane.parts.first().map(|p| p.first), Some(lane.pack.first));
            assert_eq!(lane.parts.last().map(|p| p.last), Some(lane.pack.last));
            for part in &lane.parts {
                assert_eq!(part.first, next);
                assert!(part.frames() <= 3 && part.last <= lane.pack.last);
                next = part.last + 1;
            }
        }
        assert_eq!(next, 31);
        // A part larger than a pack is the pack.
        let lanes = plan(&Order {
            pack_frames: 4,
            part_frames: 9,
            ..order(dir, (1, 8))
        })
        .expect("plan");
        assert_eq!(lanes[1].parts, [Stretch { first: 5, last: 8 }]);
        // The last frame there is does not overflow.
        let top = u32::MAX;
        let lanes = plan(&order(dir, (top - 3, top))).expect("plan");
        assert_eq!(
            lanes[0].parts,
            [Stretch {
                first: top - 3,
                last: top
            }]
        );

        assert!(plan(&order(dir, (9, 3))).is_err());
        assert!(plan(&Order {
            part_frames: 0,
            ..order(dir, (1, 9))
        })
        .is_err());
    }

    #[tokio::test]
    async fn a_film_is_baked_rendered_and_joined_and_asked_again_does_nothing_over() {
        let dir = tempfile::tempdir().expect("dir");
        let host = Shell::new(dir.path());
        // 55 frames: two packs (30, 25), parts of 10, 10, 10 | 10, 10, 5.
        let order = order(dir.path(), (101, 155));
        let journal = Journal::open(&order.work).expect("journal");

        let film = produce(&order, &host, &limits(), &journal)
            .await
            .expect("film");
        assert_eq!((film.frames, film.parts), (55, 6));
        assert_eq!(concat::count_frames(&order.out).expect("film"), 55);
        assert_eq!(lines(&dir.path().join("bakes")).len(), 2);
        assert_eq!(
            lines(&dir.path().join("renders")),
            ["101-110", "111-120", "121-130", "131-140", "141-150", "151-155"]
        );
        assert!(order.packs.join("101-130.tuilepack").exists());
        assert!(order.packs.join("131-155.tuilepack").exists());

        // Asked again with a host that can do nothing: nothing is asked of it.
        let idle = Shell {
            bake: "exit 1".into(),
            render: "exit 1".into(),
            ..Shell::new(dir.path())
        };
        std::fs::remove_file(&order.out).expect("remove");
        let film = produce(&order, &idle, &limits(), &journal)
            .await
            .expect("film");
        assert_eq!(film.frames, 55);
        assert_eq!(lines(&dir.path().join("renders")).len(), 6);

        // One segment lost: that part alone is rendered again, from the
        // pack that is there.
        std::fs::remove_file(order.work.join("segments/121-130.mp4")).expect("remove");
        produce(
            &order,
            &Shell {
                bake: "exit 1".into(),
                ..Shell::new(dir.path())
            },
            &limits(),
            &journal,
        )
        .await
        .expect("film");
        assert_eq!(
            lines(&dir.path().join("renders"))
                .last()
                .map(String::as_str),
            Some("121-130")
        );
        assert_eq!(lines(&dir.path().join("renders")).len(), 7);
        assert!(std::fs::read_to_string(order.work.join("journal.log"))
            .expect("journal")
            .contains("film: "));
    }

    #[tokio::test]
    async fn a_part_that_fails_is_tried_again_and_one_that_never_ends_well_is_named() {
        let dir = tempfile::tempdir().expect("dir");
        let mut host = Shell::new(dir.path());
        // Part 11-20 fails its first try; part 21-30 always does.
        host.render = "echo $first-$last >> $here/renders; \
                       [ $first = 21 ] && exit 9; \
                       [ $first = 11 ] && [ ! -f $here/once ] && { touch $here/once; exit 9; }; \
                       cp $here/seg-$n.mp4 $out"
            .into();
        let order = order(dir.path(), (1, 40));
        let journal = Journal::open(&order.work).expect("journal");

        let failed = produce(&order, &host, &limits(), &journal)
            .await
            .expect_err("a hole");
        assert!(
            matches!(&failed, ProduceError::Missing { missing, parts }
                if *missing == [Stretch { first: 21, last: 30 }] && *parts == 4),
            "{failed}"
        );
        // No film with a hole in it; the other parts are done all the same,
        // the second lane's too.
        assert!(!order.out.exists());
        let renders = lines(&dir.path().join("renders"));
        assert_eq!(renders.iter().filter(|r| *r == "11-20").count(), 2);
        assert_eq!(renders.iter().filter(|r| *r == "21-30").count(), 3);
        assert_eq!(renders.iter().filter(|r| *r == "31-40").count(), 1);

        // The fault gone, the same order renders the hole alone and joins.
        let mended = Shell::new(dir.path());
        let film = produce(&order, &mended, &limits(), &journal)
            .await
            .expect("film");
        assert_eq!(film.frames, 40);
        assert_eq!(lines(&dir.path().join("renders")).len(), renders.len() + 1);
    }

    #[tokio::test]
    async fn a_segment_short_of_its_frames_is_not_a_part_done() {
        let dir = tempfile::tempdir().expect("dir");
        let mut host = Shell::new(dir.path());
        // Ends well, with seven frames where ten were asked.
        host.render = "cp $here/seg-7.mp4 $out".into();
        let order = Order {
            tries: 2,
            ..order(dir.path(), (1, 10))
        };
        let journal = Journal::open(&order.work).expect("journal");
        let failed = produce(&order, &host, &limits(), &journal)
            .await
            .expect_err("short");
        assert!(
            matches!(failed, ProduceError::Missing { ref missing, .. } if missing.len() == 1),
            "{failed}"
        );
        assert!(!order.work.join("segments/1-10.json").exists());
        let said = std::fs::read_to_string(order.work.join("journal.log")).expect("journal");
        assert!(said.contains("7 frames of 10"), "{said}");
    }

    #[tokio::test]
    async fn a_bake_that_fails_leaves_no_pack_at_its_place_and_its_parts_missing() {
        let dir = tempfile::tempdir().expect("dir");
        let mut host = Shell::new(dir.path());
        host.bake = "echo half > $out; exit 4".into();
        let order = Order {
            tries: 2,
            ..order(dir.path(), (1, 30))
        };
        let journal = Journal::open(&order.work).expect("journal");
        let failed = produce(&order, &host, &limits(), &journal)
            .await
            .expect_err("no pack");
        assert!(
            matches!(failed, ProduceError::Missing { ref missing, .. } if missing.len() == 3),
            "{failed}"
        );
        assert!(!order.packs.join("1-30.tuilepack").exists());
        assert!(lines(&dir.path().join("renders")).is_empty());
    }

    #[tokio::test]
    async fn a_failed_try_is_mended_before_the_next_and_not_tried_again_if_it_cannot_be() {
        let dir = tempfile::tempdir().expect("dir");
        let mut host = Shell::new(dir.path());
        // Fails until it has been mended.
        host.render =
            "[ -f $here/mended ] || { echo broken; exit 9; }; cp $here/seg-$n.mp4 $out".into();
        host.mend = Some("grep -q broken $log && touch $here/mended".into());
        let order = order(dir.path(), (1, 10));
        let journal = Journal::open(&order.work).expect("journal");
        assert_eq!(
            produce(&order, &host, &limits(), &journal)
                .await
                .expect("film")
                .frames,
            10
        );

        // A mending that fails: one try, and no more.
        let other = tempfile::tempdir().expect("dir");
        let mut host = Shell::new(other.path());
        host.render = "echo try >> $here/renders; exit 9".into();
        host.mend = Some("exit 1".into());
        let order = super::tests::order(other.path(), (1, 10));
        let journal = Journal::open(&order.work).expect("journal");
        produce(&order, &host, &limits(), &journal)
            .await
            .expect_err("not mended");
        assert_eq!(lines(&other.path().join("renders")).len(), 1);
    }

    #[tokio::test]
    async fn lanes_at_once_give_the_same_film() {
        let dir = tempfile::tempdir().expect("dir");
        let host = Shell::new(dir.path());
        let order = Order {
            at_once: 3,
            ..order(dir.path(), (1, 95))
        };
        let journal = Journal::open(&order.work).expect("journal");
        let film = produce(&order, &host, &limits(), &journal)
            .await
            .expect("film");
        assert_eq!((film.frames, film.parts), (95, 10));

        let alone = tempfile::tempdir().expect("dir");
        let one = Order {
            at_once: 1,
            ..super::tests::order(alone.path(), (1, 95))
        };
        let journal = Journal::open(&one.work).expect("journal");
        produce(&one, &Shell::new(alone.path()), &limits(), &journal)
            .await
            .expect("film");
        assert_eq!(
            std::fs::read(&order.out).expect("film"),
            std::fs::read(&one.out).expect("film")
        );
    }

    #[tokio::test]
    async fn a_stop_file_begins_nothing_and_the_order_goes_on_once_it_is_gone() {
        let dir = tempfile::tempdir().expect("dir");
        let host = Shell::new(dir.path());
        let order = order(dir.path(), (1, 30));
        let journal = Journal::open(&order.work).expect("journal");
        std::fs::write(order.work.join("STOP"), "").expect("stop");
        let stopped = produce(&order, &host, &limits(), &journal)
            .await
            .expect_err("stopped");
        assert!(matches!(stopped, ProduceError::Stopped(_)), "{stopped}");
        assert!(lines(&dir.path().join("bakes")).is_empty());
        std::fs::remove_file(order.work.join("STOP")).expect("remove");
        assert_eq!(
            produce(&order, &host, &limits(), &journal)
                .await
                .expect("film")
                .frames,
            30
        );
    }

    #[test]
    fn a_template_is_split_before_its_values_are_put_in() {
        let command = Templates::command(
            "render film --frames {first}:{last} --out {out}",
            &[("first", "1"), ("last", "50"), ("out", "/a dir/part.mp4")],
        )
        .expect("command");
        let std = command.as_std();
        assert_eq!(std.get_program(), "render");
        let args: Vec<_> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["film", "--frames", "1:50", "--out", "/a dir/part.mp4"]
        );
        assert!(Templates::command("  ", &[]).is_err());
    }

    #[test]
    fn a_template_that_does_not_say_where_the_frames_and_the_output_go_is_given_them() {
        let args = |command: Command| -> Vec<String> {
            command
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let host = Templates {
            bake: "bake --tape flight.mcap".into(),
            render: "render film --range={first}..{last} --codec h264".into(),
            on_absent: None,
        };
        let frames = Stretch {
            first: 51,
            last: 100,
        };
        // Neither said: both are added, as the engine's own programs take them.
        assert_eq!(
            args(host.bake(frames, Path::new("/w/51-100.tuilepack"))),
            [
                "--tape",
                "flight.mcap",
                "--frames",
                "51:100",
                "--out",
                "/w/51-100.tuilepack"
            ]
        );
        // The frames said in the template's own way: left as they are said,
        // and only the output is added.
        assert_eq!(
            args(host.render(frames, Path::new("/w/a part.mp4"))),
            [
                "film",
                "--range=51..100",
                "--codec",
                "h264",
                "--out",
                "/w/a part.mp4"
            ]
        );
        // Everything said: nothing is added.
        let host = Templates {
            render: "render -o {out} {first} {last}".into(),
            ..host
        };
        assert_eq!(
            args(host.render(frames, Path::new("p.mp4"))),
            ["-o", "p.mp4", "51", "100"]
        );
    }

    #[test]
    fn the_tiles_a_render_found_absent_are_handed_to_the_hosts_own_command() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("render-1-50.1.log");
        std::fs::write(
            &log,
            "ahead of frames\nnot in the tile store: imagery 17/65451/48330\n\
             not in the tile store: terrain 12/4000/3000\nError: 2 tiles\n",
        )
        .expect("log");
        let host = Templates {
            on_absent: Some("fetch-again --from {list}".into()),
            ..Default::default()
        };
        let frames = Stretch { first: 1, last: 50 };
        let command = host.mend(Step::Render, frames, &log).expect("a mending");
        let list = dir.path().join("render-1-50.1.absent");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["--from", &*list.to_string_lossy()]);
        assert_eq!(
            std::fs::read_to_string(&list).expect("list"),
            "imagery 17/65451/48330\nterrain 12/4000/3000\n"
        );
        // Nothing absent, a bake, or no command: nothing to run.
        std::fs::write(&log, "Error: out of memory\n").expect("log");
        assert!(host.mend(Step::Render, frames, &log).is_none());
        assert!(host.mend(Step::Bake, frames, &log).is_none());
        assert!(Templates::default()
            .mend(Step::Render, frames, &log)
            .is_none());
    }
}
