// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Who decodes what next: the bookkeeping of the browser's decode pool, apart
//! from the workers it books, so it is tested on the host.
//!
//! One job per worker at a time, the rest in arrival order. A worker that has
//! finished takes the next job itself rather than going idle, so a queue
//! drains at the pace the workers set. A job nobody waits for any more — its
//! load left with the camera — is dropped when its turn comes rather than run
//! for nobody. And a job that no worker can ever take, because every one of
//! them is gone, is handed back so its caller runs it itself: the queue never
//! holds a job hostage, which on a globe would be ground that never arrives.

use std::collections::VecDeque;

/// The most decode workers a page starts when it does not say: past four, a
/// browser's pages and its GPU process want the cores more than the decoder.
pub(crate) const DEFAULT_MOST: u32 = 4;

/// How many decode workers to start: one per core the browser reports, less
/// the one the geometry server runs on, at least one, and at most `most` (the
/// page's `decodeWorkers`, [`DEFAULT_MOST`] unless said). `most` of 0 starts
/// none, and decoding stays on the server's thread.
pub(crate) fn pool_size(most: Option<u32>, hardware: Option<f64>) -> usize {
    let most = most.unwrap_or(DEFAULT_MOST) as usize;
    // A browser that hides its core count is treated as a small machine.
    let cores = hardware
        .filter(|n| n.is_finite() && *n >= 1.0)
        .map_or(2, |n| n as usize);
    cores.saturating_sub(1).max(1).min(most)
}

/// What the queue needs to know of a job.
pub(crate) trait Queued {
    /// Whether anyone still waits for it.
    fn wanted(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worker {
    /// Loading its module: can take nothing yet, will soon.
    Starting,
    Idle,
    Busy,
    /// Failed or ended: takes nothing, ever, unless restarted.
    Lost,
}

/// A job, and the worker it goes to.
#[derive(Debug)]
pub(crate) struct Dispatch<J> {
    pub worker: usize,
    pub job: J,
}

/// The workers' states and the jobs waiting for one.
#[derive(Debug)]
pub(crate) struct Scheduler<J> {
    workers: Vec<Worker>,
    queue: VecDeque<J>,
    /// The longest the queue has been: the number to watch.
    peak: usize,
    /// The jobs dropped at their turn because nobody waited for them.
    dropped: u64,
}

impl<J: Queued> Scheduler<J> {
    /// `workers` workers, all starting.
    pub(crate) fn new(workers: usize) -> Self {
        Self {
            workers: vec![Worker::Starting; workers],
            queue: VecDeque::new(),
            peak: 0,
            dropped: 0,
        }
    }

    /// Whether any worker is, or may yet be, able to take a job.
    pub(crate) fn alive(&self) -> bool {
        self.workers.iter().any(|w| *w != Worker::Lost)
    }

    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn peak(&self) -> usize {
        self.peak
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Takes a job: to an idle worker at once, else to the back of the queue.
    /// Handed back when no worker can ever take it.
    pub(crate) fn submit(&mut self, job: J) -> Result<Option<Dispatch<J>>, J> {
        if !self.alive() {
            return Err(job);
        }
        if let Some(worker) = self.workers.iter().position(|w| *w == Worker::Idle) {
            self.workers[worker] = Worker::Busy;
            return Ok(Some(Dispatch { worker, job }));
        }
        self.queue.push_back(job);
        self.peak = self.peak.max(self.queue.len());
        Ok(None)
    }

    /// `worker` has loaded and can take work: the next job, if one waits.
    pub(crate) fn ready(&mut self, worker: usize) -> Option<Dispatch<J>> {
        match self.workers.get(worker) {
            Some(Worker::Starting) => self.next_for(worker),
            _ => None,
        }
    }

    /// `worker` has answered its job: the next one, if one waits.
    pub(crate) fn finished(&mut self, worker: usize) -> Option<Dispatch<J>> {
        match self.workers.get(worker) {
            Some(Worker::Busy) => self.next_for(worker),
            _ => None,
        }
    }

    /// `worker` has failed. When it was the last that could work, every job
    /// still queued is handed back, for its caller to run itself.
    pub(crate) fn lost(&mut self, worker: usize) -> Vec<J> {
        if let Some(state) = self.workers.get_mut(worker) {
            *state = Worker::Lost;
        }
        if self.alive() {
            Vec::new()
        } else {
            self.queue.drain(..).collect()
        }
    }

    /// `worker` is loading again — a replacement for one that failed. Any
    /// job it was running is its caller's to fail.
    pub(crate) fn restarted(&mut self, worker: usize) {
        if let Some(state) = self.workers.get_mut(worker) {
            *state = Worker::Starting;
        }
    }

    /// Whether `worker` is still loading.
    pub(crate) fn starting(&self, worker: usize) -> bool {
        self.workers.get(worker) == Some(&Worker::Starting)
    }

    /// The first job still wanted, for `worker`, which is busy with it — or
    /// idle when there is none. The jobs nobody wants are dropped on the way.
    fn next_for(&mut self, worker: usize) -> Option<Dispatch<J>> {
        while let Some(job) = self.queue.pop_front() {
            if job.wanted() {
                self.workers[worker] = Worker::Busy;
                return Some(Dispatch { worker, job });
            }
            self.dropped += 1;
        }
        self.workers[worker] = Worker::Idle;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Job(u32, bool);

    impl Queued for Job {
        fn wanted(&self) -> bool {
            self.1
        }
    }

    fn job(n: u32) -> Job {
        Job(n, true)
    }

    #[test]
    fn the_pool_is_one_worker_per_core_less_one_and_capped() {
        assert_eq!(pool_size(None, Some(10.0)), DEFAULT_MOST as usize);
        assert_eq!(pool_size(Some(16), Some(10.0)), 9);
        assert_eq!(pool_size(Some(2), Some(10.0)), 2);
        assert_eq!(pool_size(None, Some(2.0)), 1);
        assert_eq!(pool_size(None, Some(1.0)), 1, "at least one");
        assert_eq!(
            pool_size(None, None),
            1,
            "an unknown machine is a small one"
        );
        assert_eq!(pool_size(None, Some(f64::NAN)), 1);
        assert_eq!(pool_size(Some(0), Some(10.0)), 0, "0 starts none");
    }

    /// Nothing runs before a worker has loaded; the first to load takes the
    /// oldest job.
    #[test]
    fn jobs_wait_for_a_worker_to_load_and_go_in_order() {
        let mut s = Scheduler::new(2);
        assert!(s.submit(job(1)).expect("alive").is_none());
        assert!(s.submit(job(2)).expect("alive").is_none());
        let d = s.ready(1).expect("a job");
        assert_eq!((d.worker, d.job), (1, job(1)));
        let d = s.ready(0).expect("a job");
        assert_eq!((d.worker, d.job), (0, job(2)));
        assert!(s.ready(0).is_none(), "a worker loads once");
        assert_eq!(s.peak(), 2);
    }

    /// One job per worker; a finished worker takes the next itself.
    #[test]
    fn a_finished_worker_takes_the_next_job() {
        let mut s = Scheduler::new(1);
        assert!(s.ready(0).is_none());
        let d = s.submit(job(1)).expect("alive").expect("idle worker");
        assert_eq!(d.worker, 0);
        assert!(s.submit(job(2)).expect("alive").is_none(), "busy");
        assert!(s.submit(job(3)).expect("alive").is_none(), "busy");
        assert_eq!(s.finished(0).expect("next").job, job(2));
        assert_eq!(s.finished(0).expect("next").job, job(3));
        assert!(s.finished(0).is_none());
        assert!(s.finished(0).is_none(), "an idle worker finishes nothing");
        assert_eq!(s.submit(job(4)).expect("alive").expect("idle").job, job(4));
    }

    /// A job whose load has gone is skipped, not run for nobody.
    #[test]
    fn a_job_nobody_waits_for_is_dropped_at_its_turn() {
        let mut s = Scheduler::new(1);
        s.ready(0);
        s.submit(job(1)).expect("alive");
        s.submit(Job(2, false)).expect("alive");
        s.submit(Job(3, false)).expect("alive");
        s.submit(job(4)).expect("alive");
        assert_eq!(s.finished(0).expect("next").job, job(4));
        assert_eq!(s.queued(), 0);
        assert_eq!(s.dropped(), 2);
    }

    /// The last worker lost hands every queued job back; after that, a new
    /// job is refused at once rather than queued for ever.
    #[test]
    fn no_job_is_held_once_every_worker_is_gone() {
        let mut s = Scheduler::new(2);
        s.submit(job(1)).expect("alive");
        s.submit(job(2)).expect("alive");
        assert!(s.lost(0).is_empty(), "one worker may still come");
        assert_eq!(s.lost(1), vec![job(1), job(2)]);
        assert!(!s.alive());
        assert_eq!(s.submit(job(3)).expect_err("refused"), job(3));
        // A replacement brings the pool back.
        s.restarted(1);
        assert!(s.alive());
        assert!(s.submit(job(4)).expect("alive").is_none());
        assert_eq!(s.ready(1).expect("a job").job, job(4));
    }

    /// A worker that failed mid-job is replaced, not counted lost: what
    /// waited in the queue waits for the replacement.
    #[test]
    fn a_replaced_worker_keeps_the_queue() {
        let mut s = Scheduler::new(1);
        s.ready(0);
        s.submit(job(1)).expect("alive").expect("idle");
        s.submit(job(2)).expect("alive");
        s.restarted(0);
        assert!(s.starting(0));
        assert!(s.finished(0).is_none(), "a loading worker finished nothing");
        assert_eq!(s.ready(0).expect("a job").job, job(2));
        assert!(!s.starting(0));
    }
}
