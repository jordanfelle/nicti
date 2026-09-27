//! The two-class priority queue: foreground always dequeues before background (ADR-0054 decision
//! rule #3), background order is delegated to a pluggable key function so
//! `prefetch::priority_order` (ADR-0044's own scheduling contract) plugs in without this module
//! knowing anything about images or cursors.

use std::collections::VecDeque;

use crate::admission::Admission;
use crate::cancel::{CancelToken, EditingGate};
use crate::job::{ChunkedJob, JobId, JobSpec, Priority, Step};

struct Entry {
    id: JobId,
    job: Box<dyn ChunkedJob>,
    cancel: CancelToken,
}

/// Owns every pending/in-flight job and decides, each time the worker asks, which one runs its
/// next chunk. Not itself a running worker -- `Scheduler::run_next` is called from whatever drives
/// the one serial GPU/`ort` thread (a real implementation, or `sim.rs`'s discrete-event
/// simulation, or a contention benchmark's own timing loop).
pub struct Scheduler {
    foreground: VecDeque<Entry>,
    background: Vec<Entry>,
    editing_gate: EditingGate,
    admission: Admission,
    next_id: u64,
}

impl Scheduler {
    /// `vram_budget_bytes` is the total budget `admission::Admission` enforces against background
    /// jobs (decision rule #5) -- foreground is never refused on VRAM grounds regardless of this
    /// budget, see `admission.rs`'s own doc comment for why.
    pub fn new(editing_gate: EditingGate, vram_budget_bytes: u64) -> Self {
        Scheduler {
            foreground: VecDeque::new(),
            background: Vec::new(),
            editing_gate,
            admission: Admission::new(vram_budget_bytes),
            next_id: 0,
        }
    }

    /// Submits a job and returns its id (for cancellation) plus a [`CancelToken`] the caller can
    /// hold onto.
    pub fn submit(&mut self, job: Box<dyn ChunkedJob>) -> (JobId, CancelToken) {
        let id = JobId(self.next_id);
        self.next_id += 1;
        let cancel = CancelToken::new();
        let entry = Entry {
            id,
            job,
            cancel: cancel.clone(),
        };
        match entry.job.spec().priority {
            Priority::Foreground => self.foreground.push_back(entry),
            Priority::Background => self.background.push(entry),
        }
        (id, cancel)
    }

    pub fn cancel(&mut self, id: JobId) {
        for entry in self.foreground.iter().chain(self.background.iter()) {
            if entry.id == id {
                entry.cancel.cancel();
            }
        }
    }

    /// Re-sorts pending background jobs by `key` (lower sorts first) -- called on every cursor
    /// move, per ADR-0044's "reprioritized immediately whenever the cursor moves" contract. A
    /// background chunk already in flight (not held in this queue while running -- see
    /// `run_next`) is unaffected; only the *next* pick reflects the new order.
    pub fn reprioritize_background(&mut self, key: impl Fn(&JobSpec) -> usize) {
        self.background.sort_by_key(|e| key(&e.job.spec()));
    }

    /// Runs exactly one chunk of the next-highest-priority job and returns its id and the
    /// [`Step`] it reported, or `None` if there's nothing runnable right now. A cancelled job is
    /// dropped (without running a chunk) rather than stepped -- checked at the chunk boundary,
    /// per `job.rs`'s own cooperative-cancellation contract. Background work is also withheld
    /// while [`EditingGate::is_editing`] is set (decision rule #4) -- an already-admitted
    /// in-flight chunk isn't affected, since this method is only ever called between chunks.
    ///
    /// A background job whose declared VRAM exceeds the *total* budget is dropped outright (it
    /// could never become admittable -- `admit`/`release` bracket one `step()` call on this same
    /// synchronous thread, so no reservation ever actually outlives a single `run_next` call for
    /// another job to contend with; found by CodeRabbit review, which correctly noted the
    /// original code would otherwise leak such a job in `background` forever). A job whose VRAM
    /// exceeds only the currently-*remaining* budget (i.e. fits the total budget but not
    /// alongside whatever this same call already skipped) is instead skipped for this pick and
    /// stays queued, per `admission::Admission`'s own decision rule #5.
    pub fn run_next(&mut self) -> Option<(JobId, Step)> {
        while let Some(entry) = self.foreground.front() {
            if entry.cancel.is_cancelled() {
                self.foreground.pop_front();
                continue;
            }
            break;
        }
        if let Some(mut entry) = self.foreground.pop_front() {
            let step = entry.job.step();
            let id = entry.id;
            if step == Step::Yield {
                self.foreground.push_back(entry);
            }
            return Some((id, step));
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
        let mut entry = self.background.remove(pick);
        let id = entry.id;
        let vram_bytes = entry.job.spec().vram_bytes;

        self.admission
            .admit(id, Priority::Background, vram_bytes)
            .expect("checked against admission.remaining() above, must not be refused");
        let step = entry.job.step();
        self.admission.release(id);

        if step == Step::Yield {
            // Keep the priority order `reprioritize_background` established: this job keeps its
            // sorted slot until the next cursor move re-sorts the queue, rather than cycling to
            // the back of the vec -- found by CodeRabbit review, which correctly noted that
            // appending here degrades nearest-to-cursor-first (ADR-0044's own scheduling
            // contract) into round-robin for every job that takes more than one chunk to finish.
            self.background.insert(pick, entry);
        }
        Some((id, step))
    }

    pub fn foreground_len(&self) -> usize {
        self.foreground.len()
    }

    pub fn background_len(&self) -> usize {
        self.background.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobKind, JobSpec};

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
        fn step(&mut self) -> Step {
            self.ticks.lock().unwrap().push(self.label);
            self.remaining -= 1;
            if self.remaining == 0 {
                Step::Done
            } else {
                Step::Yield
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
            vram_bytes,
            image_index: Some(image_index),
        }
    }

    #[test]
    fn foreground_always_dequeues_before_background() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Background, 0),
            remaining: 3,
            ticks: ticks.clone(),
            label: "bg",
        }));
        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Foreground, 0),
            remaining: 1,
            ticks: ticks.clone(),
            label: "fg",
        }));

        scheduler.run_next(); // fg runs first even though bg was submitted first
        assert_eq!(*ticks.lock().unwrap(), vec!["fg"]);
        scheduler.run_next();
        assert_eq!(*ticks.lock().unwrap(), vec!["fg", "bg"]);
    }

    #[test]
    fn cancelled_job_is_dropped_at_next_chunk_boundary() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (id, _cancel) = scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Foreground, 0),
            remaining: 5,
            ticks: ticks.clone(),
            label: "fg",
        }));
        scheduler.cancel(id);
        assert_eq!(scheduler.run_next(), None);
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
        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Background, 0),
            remaining: 1,
            ticks: ticks.clone(),
            label: "bg",
        }));

        gate.set_editing(true);
        assert_eq!(scheduler.run_next(), None);
        assert!(ticks.lock().unwrap().is_empty());

        gate.set_editing(false);
        assert!(scheduler.run_next().is_some());
        assert_eq!(*ticks.lock().unwrap(), vec!["bg"]);
    }

    #[test]
    fn reprioritize_background_changes_next_pick() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Background, 10),
            remaining: 1,
            ticks: ticks.clone(),
            label: "far",
        }));
        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Background, 0),
            remaining: 1,
            ticks: ticks.clone(),
            label: "near",
        }));

        // Cursor at 0: "near" (image_index 0) should be picked first.
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(usize::MAX));
        scheduler.run_next();
        assert_eq!(*ticks.lock().unwrap(), vec!["near"]);
    }

    #[test]
    fn yielding_job_is_re_enqueued_not_dropped() {
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Foreground, 0),
            remaining: 2,
            ticks: ticks.clone(),
            label: "fg",
        }));
        assert_eq!(scheduler.run_next(), Some((JobId(0), Step::Yield)));
        assert_eq!(scheduler.run_next(), Some((JobId(0), Step::Done)));
        assert_eq!(*ticks.lock().unwrap(), vec!["fg", "fg"]);
    }

    #[test]
    fn background_job_over_total_budget_is_dropped_not_leaked_forever() {
        // Regression test for a real CodeRabbit finding: `admit`/`release` bracket one `step()`
        // call on this same synchronous thread, so `remaining()` is always the *full* budget at
        // the point `run_next` checks it -- a job whose own `vram_bytes` exceeds the *total*
        // budget can never pass that check, no matter what else does or doesn't run. The
        // original code kept such a job in `background` forever (background_len() never reached
        // 0); this must drop it instead.
        let mut scheduler = Scheduler::new(EditingGate::new(), 100);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        scheduler.submit(Box::new(CountingJob {
            spec: spec_with_vram(Priority::Background, 0, 1000), // over the 100-byte total budget
            remaining: 1,
            ticks: ticks.clone(),
            label: "too-big",
        }));
        assert_eq!(
            scheduler.run_next(),
            None,
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
        scheduler.submit(Box::new(CountingJob {
            spec: spec_with_vram(Priority::Background, 0, 50),
            remaining: 1,
            ticks: ticks.clone(),
            label: "fits",
        }));
        assert!(scheduler.run_next().is_some());
        assert_eq!(*ticks.lock().unwrap(), vec!["fits"]);
    }

    #[test]
    fn yielded_background_job_keeps_its_sorted_position_not_round_robin() {
        // Regression test for a real CodeRabbit finding (Major): pushing a yielded background job
        // to the *end* of the vec, instead of back into its sorted slot, silently degrades
        // nearest-to-cursor-first (ADR-0044's own scheduling contract, enforced here by
        // `reprioritize_background`) into round-robin for any job that takes more than one chunk
        // to finish. With two multi-chunk jobs, the near one must finish entirely before the far
        // one starts, not alternate with it chunk-for-chunk.
        let mut scheduler = Scheduler::new(EditingGate::new(), u64::MAX);
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Background, 10),
            remaining: 3,
            ticks: ticks.clone(),
            label: "far",
        }));
        scheduler.submit(Box::new(CountingJob {
            spec: spec(Priority::Background, 0),
            remaining: 3,
            ticks: ticks.clone(),
            label: "near",
        }));
        scheduler.reprioritize_background(|spec| spec.image_index.unwrap_or(usize::MAX));

        for _ in 0..3 {
            scheduler.run_next();
        }
        assert_eq!(
            *ticks.lock().unwrap(),
            vec!["near", "near", "near"],
            "the near job must run all its chunks before the far job starts"
        );
        scheduler.run_next();
        assert_eq!(
            ticks.lock().unwrap().last(),
            Some(&"far"),
            "far only starts once near is fully done"
        );
    }
}
