// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A process run to its end, or ended for it.
//!
//! A bake or a render is a process of its own on purpose. One that waits on a
//! graphics card for ever, or grows until the machine swaps, cannot be stopped
//! from inside — a task cannot be taken back from the call it is blocked in —
//! and it can from outside: it is killed, and tried again.
//!
//! Two things end a process before it ends itself: memory over a bound, and
//! no processor time at all for a while. The second is measured in the
//! process's own processor time, never in the clock's: a machine that slept
//! through the night has not stalled, and a render was once killed for it.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::process::Command;

/// What a process may take before it is ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Resident bytes. `None`: no bound.
    pub memory_bytes: Option<u64>,
    /// How long without processor time is a stall. Zero: never one.
    pub stall: Duration,
    /// How often the process is looked at.
    pub every: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: None,
            stall: Duration::from_secs(120),
            every: Duration::from_secs(5),
        }
    }
}

/// One look at a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vital {
    /// Processor time since it began, in milliseconds.
    pub cpu_ms: u64,
    pub memory_bytes: u64,
}

/// What a look says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Alive,
    /// No processor time for [`Limits::stall`].
    Stalled,
    /// Over [`Limits::memory_bytes`], at this many bytes.
    OverMemory(u64),
}

/// Processor time under this share of the time gone by is none: a process
/// asleep on a lock still wakes for a timer now and then.
const IDLE_SHARE: f64 = 0.005;

/// A look this many periods late was not late: the machine was asleep, or
/// stopped. The time it was away is not held against the process.
const ASLEEP_AFTER: u32 = 4;

/// The looks at one process, one after the other, and what they add up to.
/// It holds no clock and no process: it is told the time and what was seen.
#[derive(Debug, Clone)]
pub struct Watch {
    limits: Limits,
    /// When it was last looked at, and its processor time then.
    seen: Duration,
    cpu_seen: u64,
    /// When it was last seen to have worked.
    worked: Duration,
    peak: u64,
}

impl Watch {
    /// A watch begun at `now`, a time on the wall's clock.
    pub fn new(limits: Limits, now: Duration) -> Self {
        Self {
            limits,
            seen: now,
            cpu_seen: 0,
            worked: now,
            peak: 0,
        }
    }

    /// The most memory seen.
    pub fn peak_bytes(&self) -> u64 {
        self.peak
    }

    /// What `vital`, seen at `now`, says of the process.
    pub fn see(&mut self, now: Duration, vital: Vital) -> Verdict {
        self.peak = self.peak.max(vital.memory_bytes);
        if let Some(bound) = self.limits.memory_bytes {
            if vital.memory_bytes > bound {
                return Verdict::OverMemory(vital.memory_bytes);
            }
        }
        let late = now.saturating_sub(self.seen);
        let spent = vital.cpu_ms.saturating_sub(self.cpu_seen);
        (self.seen, self.cpu_seen) = (now, vital.cpu_ms);
        // Away, or at work: either way it is not idle as of now.
        let away = late > self.limits.every * ASLEEP_AFTER;
        if away || spent as f64 > late.as_millis() as f64 * IDLE_SHARE {
            self.worked = now;
            return Verdict::Alive;
        }
        if !self.limits.stall.is_zero() && now.saturating_sub(self.worked) >= self.limits.stall {
            return Verdict::Stalled;
        }
        Verdict::Alive
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// By itself, with success.
    Done,
    /// By itself, with this code; `None` when a signal ended it.
    Failed(Option<i32>),
    /// Killed: it had stalled.
    Stalled,
    /// Killed: over its memory, at this many bytes.
    OverMemory(u64),
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ended::Done => write!(f, "done"),
            Ended::Failed(Some(code)) => write!(f, "failed with code {code}"),
            Ended::Failed(None) => write!(f, "ended by a signal"),
            Ended::Stalled => write!(f, "killed, stalled"),
            Ended::OverMemory(bytes) => write!(f, "killed at {} MB of memory", bytes >> 20),
        }
    }
}

/// A process's run, start to end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ran {
    pub ended: Ended,
    pub seconds: f64,
    pub peak_bytes: u64,
}

fn wall() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Runs `command` to its end under `limits`. Everything it prints, on either
/// stream, goes to `log`. It does not outlive this call: ended for it or not,
/// and if the call itself is dropped, it is killed.
pub async fn supervise(mut command: Command, log: &Path, limits: &Limits) -> std::io::Result<Ran> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::File::create(log)?;
    let err = out.try_clone()?;
    command
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true);
    let began = Instant::now();
    let mut child = command.spawn()?;
    let pid = child.id().map(Pid::from_u32);
    let mut system = System::new();
    let mut watch = Watch::new(*limits, wall());
    let every = limits.every.max(Duration::from_millis(10));
    let ended = loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                break if status.success() { Ended::Done } else { Ended::Failed(status.code()) };
            }
            () = tokio::time::sleep(every) => {
                let Some(pid) = pid else { continue };
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().with_cpu().with_memory(),
                );
                let Some(process) = system.process(pid) else { continue };
                let vital = Vital {
                    cpu_ms: process.accumulated_cpu_time(),
                    memory_bytes: process.memory(),
                };
                let verdict = watch.see(wall(), vital);
                let killed = match verdict {
                    Verdict::Alive => continue,
                    Verdict::Stalled => Ended::Stalled,
                    Verdict::OverMemory(bytes) => Ended::OverMemory(bytes),
                };
                child.kill().await?;
                break killed;
            }
        }
    };
    Ok(Ran {
        ended,
        seconds: began.elapsed().as_secs_f64(),
        peak_bytes: watch.peak_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn limits(memory_bytes: Option<u64>) -> Limits {
        Limits {
            memory_bytes,
            stall: 60 * S,
            every: 5 * S,
        }
    }

    fn vital(cpu_ms: u64, memory_bytes: u64) -> Vital {
        Vital {
            cpu_ms,
            memory_bytes,
        }
    }

    #[test]
    fn a_process_that_works_is_never_stalled_however_long_it_runs() {
        let mut watch = Watch::new(limits(None), 1000 * S);
        for step in 1..=1000u32 {
            // Half a processor, looked at every five seconds for 83 minutes.
            let seen = watch.see(1000 * S + 5 * S * step, vital(2500 * u64::from(step), 1));
            assert_eq!(seen, Verdict::Alive, "at look {step}");
        }
    }

    #[test]
    fn no_processor_time_for_the_stall_is_a_stall_and_not_a_look_sooner() {
        let mut watch = Watch::new(limits(None), Duration::ZERO);
        // It worked for a minute and a half, then stopped at 90 s.
        for step in 1..=18u32 {
            assert_eq!(
                watch.see(5 * S * step, vital(5000 * u64::from(step), 1)),
                Verdict::Alive
            );
        }
        let mut stalled_at = None;
        for step in 19..=60u32 {
            if watch.see(5 * S * step, vital(90_000, 1)) == Verdict::Stalled {
                stalled_at = Some(5 * step);
                break;
            }
        }
        // A stall's length (60 s) after it was last seen to work.
        assert_eq!(stalled_at, Some(150));
    }

    #[test]
    fn a_timer_tick_now_and_then_is_not_work() {
        let mut watch = Watch::new(limits(None), Duration::ZERO);
        let mut verdict = Verdict::Alive;
        for step in 1..=12u32 {
            // A millisecond of processor every five seconds.
            verdict = watch.see(5 * S * step, vital(u64::from(step), 1));
        }
        assert_eq!(verdict, Verdict::Stalled);
    }

    #[test]
    fn a_machine_that_slept_has_not_stalled() {
        let mut watch = Watch::new(limits(None), Duration::ZERO);
        assert_eq!(watch.see(5 * S, vital(4000, 1)), Verdict::Alive);
        // Twelve hours later, and not a millisecond more of processor.
        let morning = 12 * 3600 * S;
        assert_eq!(watch.see(morning, vital(4000, 1)), Verdict::Alive);
        // Awake again, it has a stall's length to show it works…
        for step in 1..=11u32 {
            assert_eq!(
                watch.see(morning + 5 * S * step, vital(4000, 1)),
                Verdict::Alive
            );
        }
        // …and is stalled if it does not.
        let mut idle = watch.clone();
        assert_eq!(idle.see(morning + 60 * S, vital(4000, 1)), Verdict::Stalled);
        assert_eq!(watch.see(morning + 60 * S, vital(9000, 1)), Verdict::Alive);
    }

    #[test]
    fn memory_over_the_bound_ends_it_and_the_peak_is_kept() {
        let mut watch = Watch::new(limits(Some(1000)), Duration::ZERO);
        assert_eq!(watch.see(5 * S, vital(5000, 700)), Verdict::Alive);
        assert_eq!(watch.see(10 * S, vital(10_000, 1000)), Verdict::Alive);
        assert_eq!(watch.see(15 * S, vital(15_000, 400)), Verdict::Alive);
        assert_eq!(watch.peak_bytes(), 1000);
        assert_eq!(
            watch.see(20 * S, vital(20_000, 1001)),
            Verdict::OverMemory(1001)
        );
        // With no bound, nothing is too much.
        let mut free = Watch::new(limits(None), Duration::ZERO);
        assert_eq!(free.see(5 * S, vital(5000, u64::MAX)), Verdict::Alive);
    }

    #[test]
    fn a_stall_of_zero_is_never_a_stall() {
        let mut watch = Watch::new(
            Limits {
                memory_bytes: None,
                stall: Duration::ZERO,
                every: 5 * S,
            },
            Duration::ZERO,
        );
        for step in 1..=200u32 {
            assert_eq!(watch.see(5 * S * step, vital(0, 1)), Verdict::Alive);
        }
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    fn quick() -> Limits {
        Limits {
            memory_bytes: None,
            stall: Duration::from_secs(60),
            every: Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn what_a_process_prints_is_its_log_and_how_it_ends_is_said() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("logs/one.log");
        let ran = supervise(sh("echo out; echo err >&2"), &log, &quick())
            .await
            .expect("run");
        assert_eq!(ran.ended, Ended::Done);
        let printed = std::fs::read_to_string(&log).expect("log");
        assert!(
            printed.contains("out") && printed.contains("err"),
            "{printed:?}"
        );

        let ran = supervise(sh("exit 3"), &log, &quick()).await.expect("run");
        assert_eq!(ran.ended, Ended::Failed(Some(3)));
        // The log is this run's: the one before is gone from it.
        assert_eq!(std::fs::read_to_string(&log).expect("log"), "");
    }

    #[tokio::test]
    async fn a_process_that_does_nothing_is_killed_as_stalled() {
        let dir = tempfile::tempdir().expect("dir");
        let limits = Limits {
            memory_bytes: None,
            stall: Duration::from_millis(600),
            every: Duration::from_millis(100),
        };
        let began = Instant::now();
        let ran = supervise(sh("exec sleep 30"), &dir.path().join("l"), &limits)
            .await
            .expect("run");
        assert_eq!(ran.ended, Ended::Stalled);
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "it was waited for: {:?}",
            began.elapsed()
        );
    }

    #[tokio::test]
    async fn a_process_over_its_memory_is_killed() {
        let dir = tempfile::tempdir().expect("dir");
        // Any process at all is over a bound of one byte.
        let limits = Limits {
            memory_bytes: Some(1),
            stall: Duration::from_secs(60),
            every: Duration::from_millis(100),
        };
        let ran = supervise(sh("exec sleep 30"), &dir.path().join("l"), &limits)
            .await
            .expect("run");
        assert!(
            matches!(ran.ended, Ended::OverMemory(bytes) if bytes > 1),
            "{:?}",
            ran.ended
        );
        assert!(ran.peak_bytes > 1);
    }
}
