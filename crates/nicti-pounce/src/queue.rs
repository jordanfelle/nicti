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
use crate::job::{ChunkedJob, JobId, JobSpec, Priority, Progress};

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
    /// Jobs [`Scheduler::take_next`] dropped because they were cancelled while still sitting in
    /// this queue (never handed out as a [`Taken`], so [`Scheduler::finish`] never runs for them)
    /// -- a caller must drain this after every `take_next` call and report each one `Cancelled`
    /// itself, since nothing else ever will (found by adversarial review: a job cancelled before a
    /// worker ever reached it used to vanish silently, stuck reporting `Queued` forever with its
    /// cancel-token registry entry never released).
    cancelled_while_queued: Vec<(JobId, Progress)>,
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
            cancelled_while_queued: Vec::new(),
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
    /// A background job's VRAM reservation lasts from the pick that first admits it until it is
    /// done or cancelled (#490), across every `Yield`. A started job always passes the budget
    /// check (its bytes are already counted), so it is runnable whenever it is next in order; a
    /// job not yet started is picked only if it fits the *remaining* budget, otherwise it is
    /// skipped for this pick and stays queued, per `admission::Admission`'s decision rule #5.
    /// Once the budget is full only the holders run, until one finishes and frees its bytes, so
    /// admitted jobs can neither spin nor deadlock on each other. A job whose declared VRAM
    /// exceeds the *total* budget is dropped outright (it could never become admittable; found by
    /// CodeRabbit review against the original `spikes/crouch` version, which would otherwise leak
    /// such a job in `background` forever).
    ///
    /// The caller must eventually pass the returned [`Taken`] to [`Scheduler::finish`] -- this
    /// method already reserved this job's VRAM (if background) and removed it from the queue, so
    /// a `Taken` dropped without calling `finish` would leak that reservation and the job itself.
    pub fn take_next(&mut self) -> Option<Taken> {
        while let Some(entry) = self.foreground.front() {
            if entry.cancel.is_cancelled() {
                let entry = self
                    .foreground
                    .pop_front()
                    .expect("just peeked via front()");
                self.cancelled_while_queued
                    .push((entry.id, entry.job.progress()));
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

        let admission = &mut self.admission;
        let budget = admission.budget();
        let cancelled_while_queued = &mut self.cancelled_while_queued;
        self.background.retain(|entry| {
            if entry.cancel.is_cancelled() {
                // A started job cancelled between its chunks still holds its reservation.
                admission.release(entry.id);
                cancelled_while_queued.push((entry.id, entry.job.progress()));
                return false;
            }
            // An already-admitted job is never dropped here: its bytes are counted, and dropping
            // it would leak the reservation. Otherwise an over-budget (but not cancelled)
            // background job is dropped, unchanged from the original spike behavior -- see this
            // method's own doc comment above. It is *not* reported through
            // `cancelled_while_queued` (a distinct outcome from cancellation), so it never reaches
            // a terminal status -- the gap this field fixes for cancellation. Callers must
            // therefore clamp a declaration to the lane total (`Pounce::gpu_vram_budget`), as the
            // render jobs do (#255), or declare 0.
            admission.is_admitted(entry.id) || entry.job.spec().vram_bytes <= budget
        });

        let remaining = admission.remaining();
        let pick = self.background.iter().position(|entry| {
            admission.is_admitted(entry.id) || entry.job.spec().vram_bytes <= remaining
        })?;
        let entry = self.background.remove(pick);

        // A started job (yielded earlier) already holds its reservation; only a first pick admits.
        // The amount is fixed at that first admission, so a later change to the job's declared
        // `vram_bytes` is ignored.
        if !admission.is_admitted(entry.id) {
            let vram_bytes = entry.job.spec().vram_bytes;
            admission
                .admit(entry.id, Priority::Background, vram_bytes)
                .expect("checked against admission.remaining() above, must not be refused");
        }

        Some(Taken {
            id: entry.id,
            job: entry.job,
            cancel: entry.cancel,
            priority: Priority::Background,
        })
    }

    /// Drains and returns every job cancelled while still queued since the last call to this
    /// method -- see [`Scheduler::cancelled_while_queued`]'s own doc comment. Call this after
    /// every [`Scheduler::take_next`] call (whether or not it returned a job), while still
    /// holding the same lock, so nothing in between can miss a cancellation.
    pub fn take_cancelled_while_queued(&mut self) -> Vec<(JobId, Progress)> {
        std::mem::take(&mut self.cancelled_while_queued)
    }

    /// Reports the outcome of running `taken`'s chunk. A background job's VRAM reservation is kept
    /// while it is re-enqueued (`Outcome::Yielded` and not cancelled) and released once the job
    /// leaves the scheduler (`Outcome::Done`, or cancelled). It is re-enqueued in its correct
    /// sorted background slot --
    /// found by CodeRabbit review against the original `spikes/crouch` version, which appended to
    /// the end of the vec instead, silently degrading nearest-to-cursor-first into round-robin --
    /// or drops it (`Outcome::Done`, or a cancelled `Yielded` job).
    ///
    /// Returns `true` when a `Yielded` job was dropped because it was cancelled. `cancel` doesn't
    /// take this lock, so it can land *after* the caller's own last `is_cancelled` check but before
    /// this one; the caller must then record the job as `Cancelled` itself, since nothing else
    /// ever will (it is no longer queued, so `take_cancelled_while_queued` never sees it).
    #[must_use = "a dropped cancelled job must be recorded as Cancelled by the caller"]
    pub fn finish(&mut self, taken: Taken, outcome: Outcome) -> bool {
        let cancelled = taken.cancel.is_cancelled();
        let requeue = outcome == Outcome::Yielded && !cancelled;
        if taken.priority == Priority::Background && !requeue {
            self.admission.release(taken.id);
        }
        if requeue {
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
        outcome == Outcome::Yielded && cancelled
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
        let _ = scheduler.finish(taken, outcome);
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
    fn background_job_declaring_exactly_the_total_budget_runs_alone() {
        // What the render jobs declare for a frame bigger than the whole lane (#255): clamped to
        // the total, which is admittable (`<=`), so it runs rather than being dropped.
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec_with_vram(Priority::Background, 0, 100),
                remaining: 1,
                ticks: ticks.clone(),
                label: "whole-lane",
            }),
        );
        assert!(run_one(&mut scheduler).is_some());
        assert_eq!(*ticks.lock().unwrap(), vec!["whole-lane"]);
    }

    type Ticks = std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>;

    fn submit_sized(
        scheduler: &mut Scheduler,
        id: u64,
        (image_index, vram_bytes): (usize, u64),
        chunks: u32,
        label: &'static str,
        ticks: &Ticks,
    ) -> CancelToken {
        scheduler.submit(
            JobId(id),
            Box::new(CountingJob {
                spec: spec_with_vram(Priority::Background, image_index, vram_bytes),
                remaining: chunks,
                ticks: ticks.clone(),
                label,
            }),
        )
    }

    fn run_to_idle(scheduler: &mut Scheduler) {
        while run_one(scheduler).is_some() {}
    }

    #[test]
    fn reservation_spans_yields_so_jobs_that_do_not_fit_together_run_one_at_a_time() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = Ticks::default();
        submit_sized(&mut scheduler, 0, (0, 60), 3, "a", &ticks);
        submit_sized(&mut scheduler, 1, (1, 60), 3, "b", &ticks);
        run_to_idle(&mut scheduler);
        assert_eq!(*ticks.lock().unwrap(), vec!["a", "a", "a", "b", "b", "b"]);
        assert_eq!(scheduler.admission.remaining(), 100);
    }

    #[test]
    fn jobs_that_fit_together_still_interleave() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = Ticks::default();
        submit_sized(&mut scheduler, 0, (0, 40), 2, "a", &ticks);
        submit_sized(&mut scheduler, 1, (1, 40), 2, "b", &ticks);
        run_to_idle(&mut scheduler);
        assert_eq!(*ticks.lock().unwrap(), vec!["a", "b", "a", "b"]);
    }

    #[test]
    fn a_started_far_job_is_resumed_ahead_of_a_nearer_one_that_does_not_fit() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(0));
        let ticks = Ticks::default();
        submit_sized(&mut scheduler, 0, (10, 60), 3, "far", &ticks);
        assert_eq!(run_one(&mut scheduler), Some((JobId(0), Step::Yield)));
        submit_sized(&mut scheduler, 1, (0, 60), 1, "near", &ticks);
        run_to_idle(&mut scheduler);
        assert_eq!(*ticks.lock().unwrap(), vec!["far", "far", "far", "near"]);
    }

    #[test]
    fn reservation_is_released_when_a_started_job_is_cancelled_while_queued() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let cancel = submit_sized(&mut scheduler, 0, (0, 60), 3, "a", &Ticks::default());
        assert_eq!(run_one(&mut scheduler), Some((JobId(0), Step::Yield)));
        assert_eq!(scheduler.admission.remaining(), 40);
        cancel.cancel();
        assert!(scheduler.take_next().is_none());
        assert_eq!(scheduler.take_cancelled_while_queued().len(), 1);
        assert_eq!(scheduler.admission.remaining(), 100);
    }

    #[test]
    fn reservation_is_released_when_a_yielded_chunk_is_cancelled_in_flight() {
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let cancel = submit_sized(&mut scheduler, 0, (0, 60), 3, "a", &Ticks::default());
        let mut taken = scheduler.take_next().expect("admitted");
        assert_eq!(scheduler.admission.remaining(), 40);
        assert_eq!(taken.job.step().unwrap(), Step::Yield);
        cancel.cancel();
        assert!(scheduler.finish(taken, Outcome::Yielded));
        assert_eq!(scheduler.admission.remaining(), 100);
    }

    #[test]
    fn a_job_that_grows_its_declared_vram_after_admission_is_not_dropped_or_leaked() {
        // The reserved amount is fixed at first admission; `retain` must not treat a started job
        // whose *current* declaration exceeds the total as undeliverable and drop it unreleased.
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = Ticks::default();
        submit_sized(&mut scheduler, 0, (0, 60), 2, "a", &ticks);
        assert_eq!(run_one(&mut scheduler), Some((JobId(0), Step::Yield)));
        // Re-enqueue a different job object under the same id, declaring over the total.
        let mut taken = scheduler.take_next().expect("resumed");
        taken.job = Box::new(CountingJob {
            spec: spec_with_vram(Priority::Background, 0, 1000),
            remaining: 1,
            ticks: ticks.clone(),
            label: "grown",
        });
        let _ = scheduler.finish(taken, Outcome::Yielded);
        run_to_idle(&mut scheduler);
        assert_eq!(*ticks.lock().unwrap(), vec!["a", "grown"]);
        assert_eq!(scheduler.admission.remaining(), 100);
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

        let _ = scheduler.finish(taken, Outcome::Yielded);

        // "near" (image_index 0) must now be ahead of "mid" (image_index 5, still 1 chunk left).
        run_one(&mut scheduler);
        assert_eq!(ticks.lock().unwrap().last(), Some(&"near"));
    }

    #[test]
    fn finish_reports_a_yielded_job_dropped_because_it_was_cancelled() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let token = scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 3,
                ticks,
                label: "job",
            }),
        );
        let taken = scheduler.take_next().unwrap();
        // The race: a cancel lands after the worker's own check, before `finish` reads the token.
        token.cancel();
        assert!(scheduler.finish(taken, Outcome::Yielded));
        assert!(
            scheduler.is_idle(),
            "a cancelled job must not be re-enqueued"
        );
    }

    #[test]
    fn finish_requeues_and_reports_false_for_an_uncancelled_yield() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(
            JobId(0),
            Box::new(CountingJob {
                spec: spec(Priority::Background, 0),
                remaining: 3,
                ticks,
                label: "job",
            }),
        );
        let taken = scheduler.take_next().unwrap();
        assert!(!scheduler.finish(taken, Outcome::Yielded));
        assert!(!scheduler.is_idle());
    }
}
