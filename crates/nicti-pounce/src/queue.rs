//! The two-class priority queue: foreground always dequeues before background (ADR-0054 decision
//! rule #3), background order is delegated to a pluggable key function so
//! `nicti_tapetum::prefetch::priority_order` (ADR-0044's own scheduling contract) plugs in
//! without this module knowing anything about images or cursors.
//!
//! Split from `spikes/crouch::queue`'s single `run_next` into `take_next`/`finish` so a caller
//! (`runtime.rs`'s worker threads) can run a job's `step()` *without* holding this scheduler's own
//! lock -- `spikes/crouch` only ever ran on one thread inline with the timing loop measuring it,
//! so `run_next` holding the lock across `step()` was never a problem there; a real threaded
//! runtime needs other lanes/callers (`submit`, `cancel`, `reprioritize`) to keep working while a
//! chunk is in flight.

use std::collections::VecDeque;

use crate::admission::Admission;
use crate::cancel::{CancelToken, EditingGate};
use crate::job::{ChunkedJob, JobId, JobSpec, Priority};

/// A background-ordering key, e.g. `nicti_tapetum::prefetch::priority_order`'s own
/// nearest-to-cursor comparison.
type ReprioritizeKey = Box<dyn Fn(&JobSpec) -> usize + Send>;

struct Entry {
    id: JobId,
    job: Box<dyn ChunkedJob>,
    cancel: CancelToken,
}

/// A job removed from the queue by [`Scheduler::take_next`], to be stepped by the caller without
/// holding the scheduler's lock, then handed back to [`Scheduler::finish`].
pub struct Taken {
    pub id: JobId,
    pub job: Box<dyn ChunkedJob>,
    pub cancel: CancelToken,
    priority: Priority,
}

/// What happened to a [`Taken`] job's chunk -- decides whether [`Scheduler::finish`] re-enqueues
/// it (`Yielded`) or drops it (`Done`/`Cancelled`). A job that errored (`step()` returned `Err`)
/// is the caller's responsibility to report; from this queue's own perspective that's the same as
/// `Done` -- it's never re-enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Yielded,
    Done,
}

/// Owns every pending/in-flight job and decides, each time a worker asks, which one runs its next
/// chunk. Not itself a running worker -- a lane in `runtime.rs` drives it from a real worker
/// thread, or `sim.rs`'s discrete-event simulation, or a contention benchmark's own timing loop
/// (both of the latter still live in `spikes/crouch`, unpromoted).
pub struct Scheduler {
    foreground: VecDeque<Entry>,
    background: Vec<Entry>,
    editing_gate: EditingGate,
    admission: Admission,
    reprioritize_key: Option<ReprioritizeKey>,
}

impl Scheduler {
    /// `vram_budget_bytes` is the total budget `admission::Admission` enforces against background
    /// jobs (decision rule #5) -- foreground is never refused on VRAM grounds regardless of this
    /// budget, see `admission.rs`'s own doc comment for why. Pass `u64::MAX` for a lane that
    /// doesn't do VRAM admission at all (the CPU lane).
    pub fn new(editing_gate: EditingGate, vram_budget_bytes: u64) -> Self {
        Scheduler {
            foreground: VecDeque::new(),
            background: Vec::new(),
            editing_gate,
            admission: Admission::new(vram_budget_bytes),
            reprioritize_key: None,
        }
    }

    /// Submits a job under a caller-supplied id (unlike `spikes/crouch::queue::Scheduler`, this
    /// scheduler doesn't mint its own ids -- a real runtime has two lanes, each with its own
    /// `Scheduler`, and job ids must stay unique across both for the activity panel's status map
    /// to key on them correctly). Returns a [`CancelToken`] the caller can hold onto.
    pub fn submit(&mut self, id: JobId, job: Box<dyn ChunkedJob>) -> CancelToken {
        let cancel = CancelToken::new();
        let entry = Entry {
            id,
            job,
            cancel: cancel.clone(),
        };
        match entry.job.spec().priority {
            Priority::Foreground => self.foreground.push_back(entry),
            Priority::Background => self.insert_background_sorted(entry),
        }
        cancel
    }

    pub fn cancel(&mut self, id: JobId) {
        for entry in self.foreground.iter().chain(self.background.iter()) {
            if entry.id == id {
                entry.cancel.cancel();
            }
        }
    }

    /// Re-sorts pending background jobs by `key` (lower sorts first) and remembers `key` for any
    /// job [`Scheduler::finish`] re-enqueues afterward -- called on every cursor move, per
    /// ADR-0044's "reprioritized immediately whenever the cursor moves" contract. A background
    /// chunk already in flight (not held in this queue while running -- see [`Scheduler::take_next`])
    /// is unaffected; only the *next* pick reflects the new order.
    pub fn reprioritize_background(&mut self, key: impl Fn(&JobSpec) -> usize + Send + 'static) {
        self.background.sort_by_key(|e| key(&e.job.spec()));
        self.reprioritize_key = Some(Box::new(key));
    }

    fn insert_background_sorted(&mut self, entry: Entry) {
        match &self.reprioritize_key {
            Some(key) => {
                let target = key(&entry.job.spec());
                let pos = self
                    .background
                    .partition_point(|e| key(&e.job.spec()) <= target);
                self.background.insert(pos, entry);
            }
            None => self.background.push(entry),
        }
    }

    /// Removes and returns the next-highest-priority job's entry, or `None` if there's nothing
    /// runnable right now. A cancelled job is dropped (without running a chunk) rather than
    /// returned -- checked at the chunk boundary, per `job.rs`'s own cooperative-cancellation
    /// contract. Background work is also withheld while [`EditingGate::is_editing`] is set
    /// (decision rule #4) -- an already-admitted in-flight chunk isn't affected, since this
    /// method is only ever called between chunks.
    ///
    /// A background job whose declared VRAM exceeds the *total* budget is dropped outright (it
    /// could never become admittable -- `admit`/`release` bracket one chunk's runtime, so no
    /// reservation ever actually outlives a single in-flight chunk for another job to contend
    /// with; found by CodeRabbit review against the original `spikes/crouch` version, which would
    /// otherwise leak such a job in `background` forever). A job whose VRAM exceeds only the
    /// currently-*remaining* budget (i.e. fits the total budget but not alongside whatever this
    /// same call already skipped) is instead skipped for this pick and stays queued, per
    /// `admission::Admission`'s own decision rule #5.
    ///
    /// The caller must eventually pass the returned [`Taken`] to [`Scheduler::finish`] -- this
    /// method already reserved this job's VRAM (if background) and removed it from the queue, so
    /// a `Taken` dropped without calling `finish` would leak that reservation and the job itself.
    pub fn take_next(&mut self) -> Option<Taken> {
        while let Some(entry) = self.foreground.front() {
            if entry.cancel.is_cancelled() {
                self.foreground.pop_front();
                continue;
            }
            break;
        }
        if let Some(entry) = self.foreground.pop_front() {
            return Some(Taken {
                id: entry.id,
                job: entry.job,
                cancel: entry.cancel,
                priority: Priority::Foreground,
            });
        }

        if self.editing_gate.is_editing() {
            return None;
        }

        let budget = self.admission.budget();
        self.background
            .retain(|entry| !entry.cancel.is_cancelled() && entry.job.spec().vram_bytes <= budget);

        let remaining = self.admission.remaining();
        let pick = self
            .background
            .iter()
            .position(|entry| entry.job.spec().vram_bytes <= remaining)?;
        let entry = self.background.remove(pick);
        let vram_bytes = entry.job.spec().vram_bytes;

        self.admission
            .admit(entry.id, Priority::Background, vram_bytes)
            .expect("checked against admission.remaining() above, must not be refused");

        Some(Taken {
            id: entry.id,
            job: entry.job,
            cancel: entry.cancel,
            priority: Priority::Background,
        })
    }

    /// Reports the outcome of running `taken`'s chunk. Releases its VRAM reservation (if
    /// background) regardless of outcome, then either re-enqueues it (`Outcome::Yielded`, unless
    /// it was cancelled while its chunk was running) in its correct sorted background slot --
    /// found by CodeRabbit review against the original `spikes/crouch` version, which appended to
    /// the end of the vec instead, silently degrading nearest-to-cursor-first into round-robin --
    /// or drops it (`Outcome::Done`, or a cancelled `Yielded` job).
    pub fn finish(&mut self, taken: Taken, outcome: Outcome) {
        if taken.priority == Priority::Background {
            self.admission.release(taken.id);
        }
        if outcome == Outcome::Yielded && !taken.cancel.is_cancelled() {
            let entry = Entry {
                id: taken.id,
                job: taken.job,
                cancel: taken.cancel,
            };
            match taken.priority {
                Priority::Foreground => self.foreground.push_back(entry),
                Priority::Background => self.insert_background_sorted(entry),
            }
        }
    }

    pub fn foreground_len(&self) -> usize {
        self.foreground.len()
    }

    pub fn background_len(&self) -> usize {
        self.background.len()
    }

    pub fn is_idle(&self) -> bool {
        self.foreground.is_empty() && self.background.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobKind, JobSpec, Lane, Progress, Step};

    struct CountingJob {
        spec: JobSpec,
        remaining: u32,
        ticks: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        label: &'static str,
    }

    impl ChunkedJob for CountingJob {
        fn spec(&self) -> JobSpec {
            self.spec
        }
        fn label(&self) -> String {
            self.label.to_string()
        }
        fn progress(&self) -> Progress {
            Progress::default()
        }
        fn step(&mut self) -> Result<Step, crate::job::JobError> {
            self.ticks.lock().unwrap().push(self.label);
            self.remaining -= 1;
            if self.remaining == 0 {
                Ok(Step::Done)
            } else {
                Ok(Step::Yield)
            }
        }
    }

    fn spec(priority: Priority, image_index: usize) -> JobSpec {
        spec_with_vram(priority, image_index, 0)
    }

    fn spec_with_vram(priority: Priority, image_index: usize, vram_bytes: u64) -> JobSpec {
        JobSpec {
            priority,
            kind: JobKind::Bake,
            lane: Lane::Gpu,
            vram_bytes,
            image_index: Some(image_index),
        }
    }

    /// Drives one chunk end-to-end through `take_next`/`step`/`finish`, mirroring what a real
    /// worker thread does (minus the lock-drop-relock a real thread needs across `step`).
    fn run_one(scheduler: &mut Scheduler) -> Option<(JobId, Step)> {
        let mut taken = scheduler.take_next()?;
        let step = taken.job.step().unwrap();
        let id = taken.id;
        let outcome = if step == Step::Yield {
            Outcome::Yielded
        } else {
            Outcome::Done
        };
        scheduler.finish(taken, outcome);
        Some((id, step))
    }

    #[test]
    fn foreground_always_dequeues_before_background() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 3,
                ticks: ticks.clone(),
                label: "bg",
            }),
        );
        scheduler.submit(
            JobId(1),
            Box::new(CountingJob {
                spec: spec(Priority::Foreground, 0),
                remaining: 1,
                ticks: ticks.clone(),
                label: "fg",
            }),
        );

        run_one(&mut scheduler); // fg runs first even though bg was submitted first
        assert_eq!(*ticks.lock().unwrap(), vec!["fg"]);
        run_one(&mut scheduler);
        assert_eq!(*ticks.lock().unwrap(), vec!["fg", "bg"]);
    }

    #[test]
    fn cancelled_job_is_dropped_at_next_chunk_boundary() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancel = scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Foreground, 0),
                remaining: 5,
                ticks: ticks.clone(),
                label: "fg",
            }),
        );
        cancel.cancel();
        assert!(scheduler.take_next().is_none());
        assert!(
            ticks.lock().unwrap().is_empty(),
            "a cancelled job must never step"
        );
    }

    #[test]
    fn editing_gate_withholds_background_work() {
        let gate = EditingGate::new();
        let mut scheduler = Scheduler::new(gate.clone(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 1,
                ticks: ticks.clone(),
                label: "bg",
            }),
        );

        gate.set_editing(true);
        assert!(scheduler.take_next().is_none());
        assert!(ticks.lock().unwrap().is_empty());

        gate.set_editing(false);
        assert!(run_one(&mut scheduler).is_some());
        assert_eq!(*ticks.lock().unwrap(), vec!["bg"]);
    }

    #[test]
    fn reprioritize_background_changes_next_pick() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 10),
                remaining: 1,
                ticks: ticks.clone(),
                label: "far",
            }),
        );
        scheduler.submit(
            JobId(1),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 1,
                ticks: ticks.clone(),
                label: "near",
            }),
        );

        // Cursor at 0: "near" (image_index 0) should be picked first.
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(usize::MAX));
        run_one(&mut scheduler);
        assert_eq!(*ticks.lock().unwrap(), vec!["near"]);
    }

    #[test]
    fn yielding_job_is_re_enqueued_not_dropped() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Foreground, 0),
                remaining: 2,
                ticks: ticks.clone(),
                label: "fg",
            }),
        );
        assert_eq!(run_one(&mut scheduler), Some((JobId(0), Step::Yield)));
        assert_eq!(run_one(&mut scheduler), Some((JobId(0), Step::Done)));
        assert_eq!(*ticks.lock().unwrap(), vec!["fg", "fg"]);
    }

    #[test]
    fn background_job_over_total_budget_is_dropped_not_leaked_forever() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec_with_vram(Priority::Background, 0, 1000), // over the 100-byte total budget
                remaining: 1,
                ticks: ticks.clone(),
                label: "too-big",
            }),
        );
        assert!(
            scheduler.take_next().is_none(),
            "a job over the total budget must not run"
        );
        assert!(ticks.lock().unwrap().is_empty());
        assert_eq!(
            scheduler.background_len(),
            0,
            "dropped outright -- it could never become admittable"
        );
    }

    #[test]
    fn background_job_fitting_the_budget_runs_normally() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec_with_vram(Priority::Background, 0, 50),
                remaining: 1,
                ticks: ticks.clone(),
                label: "fits",
            }),
        );
        assert!(run_one(&mut scheduler).is_some());
        assert_eq!(*ticks.lock().unwrap(), vec!["fits"]);
    }

    #[test]
    fn yielded_background_job_keeps_its_sorted_position_not_round_robin() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 10),
                remaining: 3,
                ticks: ticks.clone(),
                label: "far",
            }),
        );
        scheduler.submit(
            JobId(1),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 3,
                ticks: ticks.clone(),
                label: "near",
            }),
        );
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(usize::MAX));

        for _ in 0..3 {
            run_one(&mut scheduler);
        }
        assert_eq!(
            *ticks.lock().unwrap(),
            vec!["near", "near", "near"],
            "the near job must run all its chunks before the far job starts"
        );
        run_one(&mut scheduler);
        assert_eq!(
            ticks.lock().unwrap().last(),
            Some(&"far"),
            "far only starts once near is fully done"
        );
    }

    #[test]
    fn cursor_move_during_an_in_flight_chunk_still_reinserts_sorted() {
        // Regression for the take_next/finish split: reprioritizing *while* a job is out of the
        // queue (taken, mid-step on another thread in a real runtime) must still land it in the
        // right sorted slot when `finish` re-enqueues it, using whatever key was most recently
        // set -- not the stale index-based reinsert `spikes/crouch`'s single-threaded `run_next`
        // used, which only worked because nothing else could touch the queue mid-step there.
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(usize::MAX));

        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 5),
                remaining: 2,
                ticks: ticks.clone(),
                label: "mid",
            }),
        );
        let mut taken = scheduler.take_next().unwrap();
        assert_eq!(taken.id, JobId(0));
        let step = taken.job.step().unwrap();
        assert_eq!(step, Step::Yield);

        // A new, nearer job arrives and the cursor moves while "mid" is still out of the queue.
        scheduler.submit(
            JobId(1),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 1,
                ticks: ticks.clone(),
                label: "near",
            }),
        );
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(usize::MAX));

        scheduler.finish(taken, Outcome::Yielded);

        // "near" (image_index 0) must now be ahead of "mid" (image_index 5, still 1 chunk left).
        run_one(&mut scheduler);
        assert_eq!(ticks.lock().unwrap().last(), Some(&"near"));
    }
}
