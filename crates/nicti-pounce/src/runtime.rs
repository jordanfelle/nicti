//! `Pounce`: the real threaded runtime `spikes/crouch`'s own research didn't build (its own doc
//! comment: "does not include a real bake pipeline to schedule ... or a wired-in Scruff/telemetry
//! consumer" -- that's this module). Two lanes, per #206's option 2 (CPU decode work isn't
//! serialized behind the GPU worker):
//!
//! - [`Lane::Gpu`]: exactly one worker thread, one `Scheduler`, real VRAM admission -- ADR-0054's
//!   own one-chunk-in-flight rule, since neither `wgpu::Queue::submit` nor `ort::Session::run`
//!   support interrupting a call in flight.
//! - [`Lane::Cpu`]: a pool of worker threads (one per hardware thread, an upper bound) gated by a
//!   shared [`Throttle`] whose limit is live-adjustable (`set_cpu_limit`) -- Scruff/Patrol's
//!   import/sync scan (`nicti-lair::pounce_jobs`) is the first real client.
//!
//! Every job id is minted here (not by `Scheduler`, which now takes a caller-supplied id) so ids
//! stay unique across both lanes -- the status map below is keyed on `JobId` regardless of which
//! lane a job ran on.
//!
//! No async runtime anywhere in this design (see `cancel.rs`'s own doc comment for why) -- worker
//! loops use a `Condvar` with a bounded `wait_timeout` as a deliberate belt-and-suspenders wakeup:
//! every state change that could make a worker runnable (`submit`, `cancel`, `reprioritize`,
//! `set_cpu_limit`, `editing_gate().set_editing(false)`, `shutdown`) already calls `notify_all` on
//! the affected lane(s), but a bounded wait means a missed or reordered notify (this module's own
//! bug, not a caller's) costs one extra wakeup delay, never a permanent hang -- the same reasoning
//! this repo's own `docs/adr/0054-job-scheduler-pounce.md` used for treating an unthrottled GPU
//! queue backlog as a real correctness bug rather than a benchmark curiosity: a scheduler that can
//! silently wedge is worse than one that occasionally wakes up to find nothing to do.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::cancel::{CancelToken, EditingGate};
use crate::job::{ChunkedJob, JobId, JobKind, JobSpec, Lane, Progress, Step};
use crate::queue::{Outcome, Scheduler};
use crate::throttle::Throttle;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How many finished jobs the activity panel can still show after they complete.
const FINISHED_RING_CAPACITY: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct JobStatus {
    pub id: JobId,
    pub label: String,
    pub kind: JobKind,
    pub lane: Lane,
    pub state: JobState,
    pub progress: Progress,
}

struct LaneState {
    scheduler: Mutex<Scheduler>,
    condvar: Condvar,
}

impl LaneState {
    fn new(editing_gate: EditingGate, vram_budget_bytes: u64) -> Self {
        LaneState {
            scheduler: Mutex::new(Scheduler::new(editing_gate, vram_budget_bytes)),
            condvar: Condvar::new(),
        }
    }

    fn notify(&self) {
        self.condvar.notify_all();
    }
}

struct Inner {
    gpu: LaneState,
    cpu: LaneState,
    cpu_throttle: Throttle,
    editing_gate: EditingGate,
    next_id: AtomicU64,
    /// A `CancelToken` clone kept independently of either lane's own queue, keyed by `JobId` --
    /// `queue::Scheduler::cancel` only finds an entry *currently sitting in its queue*, so it can
    /// never reach a job a worker thread has already taken out via `take_next` to step (which is
    /// most of a job's lifetime once it starts running: `take_next`/`step`/`finish` deliberately
    /// don't hold the lane's lock across `step`, so a fast-yielding job spends very little time
    /// actually sitting in the queue for `Scheduler::cancel` to catch). Cloning a `CancelToken`
    /// shares the same underlying `AtomicBool` (`cancel.rs`'s own doc comment), so cancelling
    /// through this registry reaches a job regardless of whether it's queued or checked out.
    cancel_tokens: Mutex<HashMap<JobId, CancelToken>>,
    statuses: Mutex<HashMap<JobId, JobStatus>>,
    finished_order: Mutex<std::collections::VecDeque<JobId>>,
    shutdown: AtomicBool,
    on_change: Box<dyn Fn() + Send + Sync>,
}

impl Inner {
    fn submit(&self, job: Box<dyn ChunkedJob>) -> JobId {
        let id = JobId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let spec = job.spec();
        let status = JobStatus {
            id,
            label: job.label(),
            kind: spec.kind,
            lane: spec.lane,
            state: JobState::Queued,
            progress: job.progress(),
        };
        self.set_status(status);

        let lane = self.lane(spec.lane);
        {
            // Register the cancel token *before* releasing the lane lock -- once it's released,
            // a worker can immediately take this job, run it to `Done` (a real one-chunk job
            // can finish before this thread ever reaches the `insert` below), and call
            // `finish_status`, which removes a token that isn't in the registry yet -- a no-op.
            // `submit` then inserts it anyway, and nothing ever removes it again: a permanent
            // leak for every fast job that hits this window (found by CodeRabbit's review).
            let mut scheduler = lane.scheduler.lock().unwrap();
            let cancel_token = scheduler.submit(id, job);
            self.cancel_tokens.lock().unwrap().insert(id, cancel_token);
        }
        lane.notify();
        (self.on_change)();
        id
    }

    fn cancel(&self, id: JobId) {
        if let Some(token) = self.cancel_tokens.lock().unwrap().get(&id) {
            token.cancel();
        }
        self.notify_all_lanes();
    }

    fn set_status(&self, status: JobStatus) {
        let mut statuses = self.statuses.lock().unwrap();
        statuses.insert(status.id, status);
    }

    /// Updates only `state`/`progress` on an already-registered job, leaving its `label`/`kind`/
    /// `lane` untouched -- used for every in-place status change after the initial `submit`, so a
    /// caller never has to reconstruct (and risk getting wrong) the parts of a `JobStatus` that
    /// don't change over a job's lifetime.
    fn update_state(&self, id: JobId, state: JobState, progress: Progress) {
        let mut statuses = self.statuses.lock().unwrap();
        if let Some(status) = statuses.get_mut(&id) {
            status.state = state;
            status.progress = progress;
        }
    }

    fn finish_status(&self, id: JobId, state: JobState, progress: Progress) {
        self.update_state(id, state, progress);
        self.cancel_tokens.lock().unwrap().remove(&id);
        let mut order = self.finished_order.lock().unwrap();
        order.push_back(id);
        if order.len() > FINISHED_RING_CAPACITY {
            if let Some(evicted) = order.pop_front() {
                self.statuses.lock().unwrap().remove(&evicted);
            }
        }
    }

    /// Reports every job [`crate::queue::Scheduler::take_cancelled_while_queued`] handed back as
    /// `Cancelled` and releases its cancel-token registry entry -- see that method's own doc
    /// comment for why nothing else ever does this for a job cancelled before a worker reached
    /// it. A no-op for an empty list (the common case), so callers can call this unconditionally.
    fn report_cancelled(&self, cancelled: Vec<(JobId, Progress)>) {
        if cancelled.is_empty() {
            return;
        }
        for (id, progress) in cancelled {
            self.finish_status(id, JobState::Cancelled, progress);
        }
        (self.on_change)();
    }

    fn lane(&self, lane: Lane) -> &LaneState {
        match lane {
            Lane::Gpu => &self.gpu,
            Lane::Cpu => &self.cpu,
        }
    }

    fn notify_all_lanes(&self) {
        self.gpu.notify();
        self.cpu.notify();
    }
}

/// The runtime handle -- cheap to clone (an `Arc` underneath), `Send + Sync`. Every clone shares
/// the same two lanes and worker threads; there is exactly one real set of workers per
/// `Pounce::new` call, started eagerly.
#[derive(Clone)]
pub struct Pounce {
    inner: Arc<Inner>,
    gpu_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    cpu_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Pounce {
    /// `vram_budget_bytes` is the GPU lane's total VRAM admission budget (decision rule #5).
    /// `cpu_threads` bounds how many worker threads the CPU lane ever spawns -- the *live*
    /// concurrency cap is `cpu_limit` (adjustable afterward via [`Pounce::set_cpu_limit`]), which
    /// must not exceed `cpu_threads` or the extra permits could never be used. `on_change` is
    /// called (from a worker thread) after every status-affecting event -- wire it to
    /// `egui::Context::request_repaint` so a UI redraws when Pounce's own state changes without
    /// this crate depending on egui.
    pub fn new(
        vram_budget_bytes: u64,
        cpu_threads: usize,
        cpu_limit: usize,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let cpu_threads = cpu_threads.max(1);
        let cpu_limit = cpu_limit.clamp(1, cpu_threads);
        let editing_gate = EditingGate::new();
        let inner = Arc::new(Inner {
            gpu: LaneState::new(editing_gate.clone(), vram_budget_bytes),
            cpu: LaneState::new(editing_gate.clone(), u64::MAX),
            cpu_throttle: Throttle::new(cpu_limit),
            editing_gate,
            next_id: AtomicU64::new(0),
            cancel_tokens: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            finished_order: Mutex::new(std::collections::VecDeque::new()),
            shutdown: AtomicBool::new(false),
            on_change: Box::new(on_change),
        });

        let gpu_inner = inner.clone();
        let gpu_handle = std::thread::Builder::new()
            .name("pounce-gpu".into())
            .spawn(move || worker_loop(gpu_inner, Lane::Gpu))
            .expect("spawning the Pounce GPU worker thread");

        let mut cpu_handles = Vec::with_capacity(cpu_threads);
        for i in 0..cpu_threads {
            let cpu_inner = inner.clone();
            let handle = std::thread::Builder::new()
                .name(format!("pounce-cpu-{i}"))
                .spawn(move || worker_loop(cpu_inner, Lane::Cpu))
                .expect("spawning a Pounce CPU worker thread");
            cpu_handles.push(handle);
        }

        Pounce {
            inner,
            gpu_handle: Arc::new(Mutex::new(Some(gpu_handle))),
            cpu_handles: Arc::new(Mutex::new(cpu_handles)),
        }
    }

    pub fn submit(&self, job: Box<dyn ChunkedJob>) -> JobId {
        self.inner.submit(job)
    }

    /// Cancels a job regardless of whether it's currently queued or being stepped by a worker
    /// thread right now -- see [`Inner::cancel_tokens`]'s own doc comment for why this can't just
    /// delegate to `queue::Scheduler::cancel`. A cancelled job stops at its *next* chunk boundary,
    /// never mid-chunk (this crate's cooperative-cancellation contract throughout).
    pub fn cancel(&self, id: JobId) {
        self.inner.cancel(id);
    }

    /// A handle a *job* can hold to submit follow-up jobs (#57's export stages chain
    /// decode -> render -> encode this way). Deliberately not a `Pounce` clone: `Drop for Pounce`
    /// joins the worker threads when it sees the last live handle, so a queued job holding a
    /// `Pounce` clone could become the last holder on a worker thread and join itself. This holds
    /// only a `Weak<Inner>`, so it never keeps the runtime alive and never joins anything.
    pub fn submitter(&self) -> Submitter {
        Submitter {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Re-orders both lanes' pending background jobs by `key` -- called on every cursor move
    /// (ADR-0044's own contract). Only the GPU lane has image-tied bake jobs today, but the key is
    /// applied to both lanes for forward-compat with a future CPU-lane job that also wants
    /// nearest-to-cursor ordering.
    pub fn reprioritize(&self, key: impl Fn(&JobSpec) -> usize + Send + Clone + 'static) {
        self.inner
            .gpu
            .scheduler
            .lock()
            .unwrap()
            .reprioritize_background(key.clone());
        self.inner
            .cpu
            .scheduler
            .lock()
            .unwrap()
            .reprioritize_background(key);
        self.inner.notify_all_lanes();
    }

    pub fn editing_gate(&self) -> EditingGate {
        self.inner.editing_gate.clone()
    }

    /// Sets whether a slider is being dragged (decision rule #4). Wakes both lanes immediately
    /// when cleared, so background work resumes without waiting out a worker's poll interval.
    pub fn set_editing(&self, editing: bool) {
        self.inner.editing_gate.set_editing(editing);
        if !editing {
            self.inner.notify_all_lanes();
        }
    }

    pub fn cpu_limit(&self) -> usize {
        self.inner.cpu_throttle.limit()
    }

    /// Adjusts the CPU lane's live concurrency cap, clamped to the worker-thread count fixed at
    /// construction (raising this above that count would promise concurrency the lane has no
    /// threads to provide).
    pub fn set_cpu_limit(&self, limit: usize) {
        let capped = limit.clamp(1, self.cpu_handles.lock().unwrap().len().max(1));
        self.inner.cpu_throttle.set_limit(capped);
        self.inner.cpu.notify();
    }

    /// A snapshot of every job Pounce knows about right now -- active jobs plus up to the last
    /// [`FINISHED_RING_CAPACITY`] finished ones, for the activity panel to render. Ordered by id
    /// (submission order).
    pub fn snapshot(&self) -> Vec<JobStatus> {
        let statuses = self.inner.statuses.lock().unwrap();
        let mut all: Vec<JobStatus> = statuses.values().cloned().collect();
        all.sort_by_key(|s| s.id);
        all
    }

    /// Signals every worker thread to stop after its current chunk (if any) and joins them all.
    /// Blocks until every thread has exited. Idempotent -- a second call is a no-op (the threads
    /// are already joined and gone).
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.notify_all_lanes();
        if let Some(handle) = self.gpu_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        let mut cpu_handles = self.cpu_handles.lock().unwrap();
        for handle in cpu_handles.drain(..) {
            let _ = handle.join();
        }
    }
}

/// A weak, job-safe handle for submitting follow-up jobs -- see [`Pounce::submitter`].
#[derive(Clone)]
pub struct Submitter {
    inner: std::sync::Weak<Inner>,
}

impl Submitter {
    /// Submits `job`, or returns `None` if the runtime has shut down or been dropped (the job is
    /// dropped un-run, so its `Drop` still gets to settle any result slot it owns).
    pub fn submit(&self, job: Box<dyn ChunkedJob>) -> Option<JobId> {
        let inner = self.inner.upgrade()?;
        if inner.shutdown.load(Ordering::SeqCst) {
            return None;
        }
        Some(inner.submit(job))
    }

    /// Cancels a job by id; a no-op once the runtime is gone.
    pub fn cancel(&self, id: JobId) {
        if let Some(inner) = self.inner.upgrade() {
            inner.cancel(id);
        }
    }
}

impl Drop for Pounce {
    fn drop(&mut self) {
        // Only the last live *handle* actually owns the worker threads worth joining -- every
        // clone shares the same `Inner`/handles, and joining from an arbitrary clone's drop would
        // block that clone's own thread on workers a *different* still-live clone might still
        // want running. Checked against `gpu_handle`'s own strong count, not `inner`'s: each
        // worker thread holds its own long-lived `Arc<Inner>` clone for as long as it runs (see
        // `Pounce::new`'s `gpu_inner`/`cpu_inner`), so `inner`'s count never drops to 1 while any
        // worker is still alive -- checking it would make this branch dead code, never firing the
        // first time it's needed (found by adversarial review). `gpu_handle` is never handed to a
        // worker thread, only cloned alongside the rest of `Pounce` on `derive(Clone)`, so its
        // strong count tracks exactly how many `Pounce` handles are still live.
        if Arc::strong_count(&self.gpu_handle) == 1 {
            self.shutdown();
        }
    }
}

/// Wraps a CPU-lane [`crate::throttle::Permit`] so releasing it also wakes any worker parked on
/// the lane's own condvar waiting for capacity -- `Throttle::Permit::drop` only notifies
/// `Throttle`'s *own* internal condvar, which nothing in this module ever waits on (every CPU
/// worker uses non-blocking `try_acquire`, never the blocking `acquire` that condvar is for), so
/// without this wrapper a freed permit went unnoticed until the next `POLL_INTERVAL` backstop
/// wakeup rather than immediately (found by adversarial review).
struct WakingPermit<'a> {
    _permit: crate::throttle::Permit<'a>,
    lane_state: &'a LaneState,
    /// Set by an iteration that found no job: it never really used the capacity, so releasing it
    /// must not wake the other workers. Otherwise every idle worker's release wakes every other
    /// idle worker, which re-acquires, finds nothing, releases and wakes again -- a livelock that
    /// pins most of the pool at 100% with an empty queue (#407).
    quiet: bool,
}

impl Drop for WakingPermit<'_> {
    fn drop(&mut self) {
        if !self.quiet {
            self.lane_state.notify();
        }
    }
}

fn worker_loop(inner: Arc<Inner>, lane: Lane) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }

        let lane_state = inner.lane(lane);

        let mut permit = if lane == Lane::Cpu {
            match inner.cpu_throttle.try_acquire() {
                Some(permit) => Some(WakingPermit {
                    _permit: permit,
                    lane_state,
                    quiet: false,
                }),
                None => {
                    let guard = lane_state.scheduler.lock().unwrap();
                    let _ = lane_state.condvar.wait_timeout(guard, POLL_INTERVAL);
                    continue;
                }
            }
        } else {
            None
        };

        let taken = {
            let mut scheduler = lane_state.scheduler.lock().unwrap();
            let taken = scheduler.take_next();
            // Checked on every call, whether or not a job was also taken -- a job cancelled while
            // still queued is dropped by `take_next` itself (never becomes a `Taken`), so nothing
            // else would ever report it `Cancelled` or release its cancel-token registry entry
            // otherwise (found by adversarial review).
            let cancelled_while_queued = scheduler.take_cancelled_while_queued();
            match taken {
                Some(taken) => {
                    inner.report_cancelled(cancelled_while_queued);
                    taken
                }
                None => {
                    inner.report_cancelled(cancelled_while_queued);
                    if let Some(p) = permit.as_mut() {
                        p.quiet = true;
                    }
                    let _ = lane_state.condvar.wait_timeout(scheduler, POLL_INTERVAL);
                    continue;
                }
            }
        };

        inner.update_state(taken.id, JobState::Running, taken.job.progress());
        (inner.on_change)();

        let mut taken = taken;
        let step_result = taken.job.step();
        let progress = taken.job.progress();
        let cancelled = taken.cancel.is_cancelled();

        let (outcome, terminal) = match (&step_result, cancelled) {
            (_, true) => (Outcome::Yielded, Some(JobState::Cancelled)),
            (Ok(Step::Yield), false) => (Outcome::Yielded, None),
            (Ok(Step::Done), false) => (Outcome::Done, Some(JobState::Done)),
            (Err(e), false) => (Outcome::Done, Some(JobState::Failed(e.0.clone()))),
        };

        let id = taken.id;
        let terminal = {
            let mut scheduler = lane_state.scheduler.lock().unwrap();
            // Re-check cancellation now, under the same lock `finish` uses to decide whether to
            // re-enqueue the job -- a cancellation landing in the window between this chunk's
            // own check above and this point would otherwise leave `terminal` at `None` (so
            // `Queued` gets written below) even though `finish` itself, reading the same live
            // `CancelToken` a moment later, correctly drops the job without re-enqueuing it: the
            // job would end up permanently stuck reporting `Queued`, with its `cancel_tokens`
            // entry never removed either (found by CodeRabbit's review).
            let terminal = if terminal.is_none() && taken.cancel.is_cancelled() {
                Some(JobState::Cancelled)
            } else {
                terminal
            };
            // A non-terminal (`Yielded`, not cancelled) status must be written *before*
            // `finish` re-enqueues the job, and while this same lock is still held -- once
            // `finish` returns, another CPU-lane worker (the GPU lane only ever has one) can
            // immediately take this same job back out, run it to a real terminal state, and
            // call `finish_status` itself. Writing `Queued` after releasing the lock could then
            // land after that worker's own terminal write and silently clobber it back to
            // `Queued` forever (found by CodeRabbit's review) -- `Running`, by contrast, has no
            // such race, since only the worker that took a job can ever write that job's own
            // `Running` status.
            if terminal.is_none() {
                inner.update_state(id, JobState::Queued, progress);
            }
            // `finish` re-reads the token, so a cancel landing after the re-check above is only
            // visible here: it drops the job, and without this the status would stay `Queued`
            // forever (the flake in `cancelling_mid_job_stops_it_at_the_next_boundary`).
            let dropped_cancelled = scheduler.finish(taken, outcome);
            terminal.or(dropped_cancelled.then_some(JobState::Cancelled))
        };

        if let Some(state) = terminal {
            inner.finish_status(id, state, progress);
        }
        (inner.on_change)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// Total CPU ticks (utime + stime) this process's `pounce-cpu-*` threads have burned.
    #[cfg(target_os = "linux")]
    fn pounce_cpu_ticks() -> u64 {
        let mut total = 0;
        for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
            let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
            if !comm.starts_with("pounce-cpu-") {
                continue;
            }
            let stat = std::fs::read_to_string(task.path().join("stat")).unwrap_or_default();
            // Fields after the parenthesised comm; utime/stime are fields 14/15 overall.
            if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) {
                let f: Vec<&str> = rest.split_whitespace().collect();
                total += f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                    + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            }
        }
        total
    }

    /// Regression for #407: idle CPU workers each woke every other idle worker on every permit
    /// release, a livelock that pinned most of the pool with an empty queue.
    #[cfg(target_os = "linux")]
    #[test]
    fn idle_cpu_pool_does_not_burn_cpu() {
        let _pounce = Pounce::new(1 << 30, 16, 8, || {});
        std::thread::sleep(Duration::from_millis(200));
        let before = pounce_cpu_ticks();
        std::thread::sleep(Duration::from_secs(1));
        let burned = pounce_cpu_ticks() - before;
        // 100 ticks/s per core; a healthy idle pool is a handful of ticks, the livelock was
        // hundreds.
        assert!(burned < 20, "idle pool burned {burned} ticks in 1s");
    }

    struct StepJob {
        spec: JobSpec,
        label: String,
        remaining: u32,
        on_step: Option<Box<dyn Fn() + Send>>,
    }

    impl ChunkedJob for StepJob {
        fn spec(&self) -> JobSpec {
            self.spec
        }
        fn label(&self) -> String {
            self.label.clone()
        }
        fn progress(&self) -> Progress {
            Progress {
                done: 0,
                total: None,
            }
        }
        fn step(&mut self) -> Result<Step, crate::job::JobError> {
            if let Some(f) = &self.on_step {
                f();
            }
            self.remaining -= 1;
            if self.remaining == 0 {
                Ok(Step::Done)
            } else {
                Ok(Step::Yield)
            }
        }
    }

    struct FailingJob {
        spec: JobSpec,
    }

    impl ChunkedJob for FailingJob {
        fn spec(&self) -> JobSpec {
            self.spec
        }
        fn label(&self) -> String {
            "failing".to_string()
        }
        fn progress(&self) -> Progress {
            Progress::default()
        }
        fn step(&mut self) -> Result<Step, crate::job::JobError> {
            Err(crate::job::JobError::new("boom"))
        }
    }

    fn cpu_spec() -> JobSpec {
        JobSpec {
            priority: crate::job::Priority::Background,
            kind: JobKind::Import,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn gpu_spec(priority: crate::job::Priority) -> JobSpec {
        JobSpec {
            priority,
            kind: JobKind::Bake,
            lane: Lane::Gpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn a_submitted_job_runs_to_done() {
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});
        let id = pounce.submit(Box::new(StepJob {
            spec: cpu_spec(),
            label: "one-shot".into(),
            remaining: 1,
            on_step: None,
        }));

        assert!(wait_until(
            || {
                pounce
                    .snapshot()
                    .into_iter()
                    .any(|s| s.id == id && s.state == JobState::Done)
            },
            Duration::from_secs(2)
        ));
        pounce.shutdown();
    }

    #[test]
    fn cpu_lane_never_exceeds_its_limit() {
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let pounce = Pounce::new(u64::MAX, 4, 2, || {});

        for _ in 0..6 {
            let concurrent = concurrent.clone();
            let max_seen = max_seen.clone();
            pounce.submit(Box::new(StepJob {
                spec: cpu_spec(),
                label: "job".into(),
                remaining: 3,
                on_step: Some(Box::new(move || {
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })),
            }));
        }

        assert!(wait_until(
            || pounce.snapshot().iter().all(|s| {
                matches!(
                    s.state,
                    JobState::Done | JobState::Failed(_) | JobState::Cancelled
                )
            }),
            Duration::from_secs(5)
        ));
        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "CPU lane ran more than its limit of 2 concurrently: {}",
            max_seen.load(Ordering::SeqCst)
        );
        pounce.shutdown();
    }

    #[test]
    fn gpu_lane_never_runs_more_than_one_chunk_at_once() {
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});

        for _ in 0..4 {
            let concurrent = concurrent.clone();
            let max_seen = max_seen.clone();
            pounce.submit(Box::new(StepJob {
                spec: gpu_spec(crate::job::Priority::Background),
                label: "bake".into(),
                remaining: 2,
                on_step: Some(Box::new(move || {
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                })),
            }));
        }

        assert!(wait_until(
            || pounce
                .snapshot()
                .iter()
                .all(|s| matches!(s.state, JobState::Done)),
            Duration::from_secs(5)
        ));
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "GPU lane must never run more than one chunk in flight"
        );
        pounce.shutdown();
    }

    #[test]
    fn editing_gate_set_then_released_wakes_workers_without_hanging() {
        let pounce = Pounce::new(u64::MAX, 1, 1, || {});
        pounce.set_editing(true);
        let id = pounce.submit(Box::new(StepJob {
            spec: gpu_spec(crate::job::Priority::Background),
            label: "gated".into(),
            remaining: 1,
            on_step: None,
        }));

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            pounce
                .snapshot()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap()
                .state,
            JobState::Queued,
            "background work must not start while the editing gate is set"
        );

        pounce.set_editing(false);
        assert!(wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Done),
            Duration::from_secs(2)
        ));
        pounce.shutdown();
    }

    #[test]
    fn cancelling_mid_job_stops_it_at_the_next_boundary() {
        let pounce = Pounce::new(u64::MAX, 1, 1, || {});
        let id = pounce.submit(Box::new(StepJob {
            spec: cpu_spec(),
            label: "cancel-me".into(),
            remaining: 1000,
            on_step: Some(Box::new(|| {
                std::thread::sleep(Duration::from_millis(5));
            })),
        }));
        std::thread::sleep(Duration::from_millis(20));
        pounce.cancel(id);

        assert!(wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Cancelled),
            Duration::from_secs(2)
        ));
        pounce.shutdown();
    }

    #[test]
    fn a_failing_step_is_reported_without_killing_the_worker() {
        let pounce = Pounce::new(u64::MAX, 1, 1, || {});
        let failing_id = pounce.submit(Box::new(FailingJob { spec: cpu_spec() }));
        assert!(wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| { s.id == failing_id && matches!(s.state, JobState::Failed(_)) }),
            Duration::from_secs(2)
        ));

        // The worker must still be alive to run a second job after the first one failed.
        let ok_id = pounce.submit(Box::new(StepJob {
            spec: cpu_spec(),
            label: "after-failure".into(),
            remaining: 1,
            on_step: None,
        }));
        assert!(wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == ok_id && s.state == JobState::Done),
            Duration::from_secs(2)
        ));
        pounce.shutdown();
    }

    #[test]
    fn shutdown_joins_cleanly_even_with_pending_work() {
        let pounce = Pounce::new(u64::MAX, 2, 1, || {});
        for _ in 0..5 {
            pounce.submit(Box::new(StepJob {
                spec: cpu_spec(),
                label: "queued".into(),
                remaining: 1000,
                on_step: None,
            }));
        }
        // Shut down almost immediately -- must not hang even though most of these jobs never
        // finish.
        pounce.shutdown();
    }

    #[test]
    fn set_cpu_limit_is_clamped_to_the_spawned_thread_count() {
        let pounce = Pounce::new(u64::MAX, 2, 1, || {});
        pounce.set_cpu_limit(100);
        assert_eq!(pounce.cpu_limit(), 2);
        pounce.shutdown();
    }

    #[test]
    fn cancelling_a_job_still_sitting_in_the_queue_is_reported_cancelled() {
        // Regression test for a real bug an adversarial review caught: `Scheduler::take_next`
        // silently drops a cancelled queued entry without ever producing a `Taken` for it, so
        // nothing used to report it `Cancelled` (it stuck at `Queued` forever) or release its
        // cancel-token registry entry. Saturate the single CPU worker with an unrelated job so
        // the second submission never gets a chance to be taken before it's cancelled.
        let pounce = Pounce::new(u64::MAX, 1, 1, || {});
        // Both foreground priority (still the CPU lane, one single worker thread): "busy" is
        // unconditionally in front of the queue and takes >=5ms per step, so "never-runs" is
        // still sitting behind it in `Scheduler::foreground` (never yet popped by `take_next`) at
        // the moment `cancel` runs immediately below -- there's no artificial delay between
        // submitting it and cancelling it.
        let _busy = pounce.submit(Box::new(StepJob {
            spec: JobSpec {
                priority: crate::job::Priority::Foreground,
                ..cpu_spec()
            },
            label: "busy".into(),
            remaining: 1000,
            on_step: Some(Box::new(|| {
                std::thread::sleep(Duration::from_millis(5));
            })),
        }));
        let queued_id = pounce.submit(Box::new(StepJob {
            spec: JobSpec {
                priority: crate::job::Priority::Foreground,
                ..cpu_spec()
            },
            label: "never-runs".into(),
            remaining: 1,
            on_step: None,
        }));
        pounce.cancel(queued_id);

        assert!(
            wait_until(
                || pounce
                    .snapshot()
                    .into_iter()
                    .any(|s| s.id == queued_id && s.state == JobState::Cancelled),
                Duration::from_secs(2)
            ),
            "a job cancelled while still queued must be reported Cancelled, not stuck at Queued forever"
        );
        pounce.shutdown();
    }

    #[test]
    fn freeing_a_cpu_permit_wakes_a_waiting_worker_promptly() {
        // Regression test for a real bug an adversarial review caught: `Throttle::Permit::drop`
        // only notified `Throttle`'s own internal condvar, which no worker in this module ever
        // waits on (every CPU worker uses non-blocking `try_acquire`) -- so a freed permit went
        // unnoticed until the next `POLL_INTERVAL` (200ms) backstop wakeup instead of promptly.
        // With `cpu_limit=1` and a first job holding the only permit for `HOLD` while a second,
        // fast job waits behind it, the second must finish well before one full poll interval
        // after the first releases its permit.
        const HOLD: Duration = Duration::from_millis(100);
        let pounce = Pounce::new(u64::MAX, 2, 1, || {});
        pounce.submit(Box::new(StepJob {
            spec: cpu_spec(),
            label: "holds-the-permit".into(),
            remaining: 1,
            on_step: Some(Box::new(move || std::thread::sleep(HOLD))),
        }));
        let fast_id = pounce.submit(Box::new(StepJob {
            spec: cpu_spec(),
            label: "fast".into(),
            remaining: 1,
            on_step: None,
        }));

        let start = Instant::now();
        assert!(wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == fast_id && s.state == JobState::Done),
            Duration::from_secs(2)
        ));
        let elapsed = start.elapsed();
        assert!(
            elapsed < HOLD + Duration::from_millis(POLL_INTERVAL.as_millis() as u64 / 2),
            "fast job took {elapsed:?} after the permit-holder released -- looks like it waited \
             out a full POLL_INTERVAL backstop instead of being woken promptly"
        );
        pounce.shutdown();
    }

    #[test]
    fn a_yielded_jobs_queued_status_never_clobbers_a_later_workers_terminal_write() {
        // Regression test for a real race a CodeRabbit review caught: writing a yielded job's
        // `Queued` status *after* `Scheduler::finish` re-enqueues it (releasing the lane lock in
        // between) left a window where a second CPU-lane worker could take that same job back
        // out, run it to a real terminal state, and write that terminal status -- all before the
        // first worker's now-stale `Queued` write executed, clobbering the real outcome back to
        // `Queued` forever. Many short multi-chunk jobs across several CPU workers, run
        // repeatedly, gives every job many chances to hit that window if the race still exists.
        for _ in 0..20 {
            let pounce = Pounce::new(u64::MAX, 4, 4, || {});
            let ids: Vec<JobId> = (0..20)
                .map(|_| {
                    pounce.submit(Box::new(StepJob {
                        spec: cpu_spec(),
                        label: "quick".into(),
                        remaining: 3,
                        on_step: None,
                    }))
                })
                .collect();

            assert!(wait_until(
                || {
                    let snapshot = pounce.snapshot();
                    ids.iter().all(|id| {
                        snapshot
                            .iter()
                            .any(|s| s.id == *id && s.state == JobState::Done)
                    })
                },
                Duration::from_secs(2)
            ));
            pounce.shutdown();
        }
    }

    #[test]
    fn fast_single_chunk_jobs_never_leave_a_stuck_status_or_registry_leak() {
        // Regression test for two more real races a CodeRabbit review caught, both about the
        // window right after a job is handed off between two different locks/threads:
        // 1. `submit` used to register a job's `CancelToken` in `cancel_tokens` *after* releasing
        //    the lane lock -- a worker could take a fast (single-chunk) job, run it to `Done`,
        //    and call `finish_status` (which removes the registry entry) all before `submit`'s
        //    own `insert` ever ran, leaking that entry forever once `insert` finally executed.
        // 2. A cancellation landing between this chunk's own `is_cancelled()` check and the
        //    `scheduler.finish` call a moment later left `terminal` stuck at `None` (so `Queued`
        //    was written) even though `finish` itself, re-reading the same live token, correctly
        //    dropped the job without re-enqueuing it.
        // Neither is deterministically reproducible without hooks into those exact windows, so
        // this stress-submits many single-chunk jobs (the shape most likely to hit window 1) and
        // cancels half of them immediately after submit (most likely to hit window 2) across
        // several runs, asserting every job reaches a real terminal state -- never stuck at
        // `Queued` forever.
        for _ in 0..20 {
            let pounce = Pounce::new(u64::MAX, 4, 4, || {});
            let ids: Vec<JobId> = (0..30)
                .map(|i| {
                    let id = pounce.submit(Box::new(StepJob {
                        spec: cpu_spec(),
                        label: "fast".into(),
                        remaining: 1,
                        on_step: None,
                    }));
                    if i % 2 == 0 {
                        pounce.cancel(id);
                    }
                    id
                })
                .collect();

            assert!(wait_until(
                || {
                    let snapshot = pounce.snapshot();
                    ids.iter().all(|id| {
                        snapshot.iter().any(|s| {
                            s.id == *id
                                && matches!(
                                    s.state,
                                    JobState::Done | JobState::Cancelled | JobState::Failed(_)
                                )
                        })
                    })
                },
                Duration::from_secs(2)
            ));
            pounce.shutdown();
        }
    }

    /// A job that, on its one step, submits a follow-up through its `Submitter`.
    struct ChainJob {
        submitter: Submitter,
        next: Option<Box<dyn ChunkedJob>>,
        submitted: Arc<Mutex<Option<Option<JobId>>>>,
    }

    impl ChunkedJob for ChainJob {
        fn spec(&self) -> JobSpec {
            cpu_spec()
        }
        fn label(&self) -> String {
            "chain".into()
        }
        fn progress(&self) -> Progress {
            Progress::default()
        }
        fn step(&mut self) -> Result<Step, crate::job::JobError> {
            let next = self.next.take().expect("stepped once");
            *self.submitted.lock().unwrap() = Some(self.submitter.submit(next));
            Ok(Step::Done)
        }
    }

    #[test]
    fn a_job_can_chain_a_follow_up_through_its_submitter() {
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});
        let ran = Arc::new(AtomicUsize::new(0));
        let ran2 = ran.clone();
        let submitted = Arc::new(Mutex::new(None));
        pounce.submit(Box::new(ChainJob {
            submitter: pounce.submitter(),
            next: Some(Box::new(StepJob {
                spec: cpu_spec(),
                label: "follow-up".into(),
                remaining: 1,
                on_step: Some(Box::new(move || {
                    ran2.fetch_add(1, Ordering::SeqCst);
                })),
            })),
            submitted: submitted.clone(),
        }));
        assert!(wait_until(
            || ran.load(Ordering::SeqCst) == 1,
            Duration::from_secs(2)
        ));
        assert!(matches!(*submitted.lock().unwrap(), Some(Some(_))));
        pounce.shutdown();
    }

    #[test]
    fn a_submitter_returns_none_after_shutdown_and_after_the_runtime_is_dropped() {
        let pounce = Pounce::new(u64::MAX, 1, 1, || {});
        let submitter = pounce.submitter();
        let job = || {
            Box::new(StepJob {
                spec: cpu_spec(),
                label: "late".into(),
                remaining: 1,
                on_step: None,
            })
        };
        assert!(submitter.submit(job()).is_some());
        pounce.shutdown();
        assert!(submitter.submit(job()).is_none());
        drop(pounce);
        assert!(submitter.submit(job()).is_none());
        submitter.cancel(JobId(0)); // no-op, must not panic
    }

    #[test]
    fn dropping_the_last_pounce_while_a_queued_job_holds_a_submitter_does_not_deadlock() {
        // A queued job holds a Submitter (not a Pounce): when the last Pounce handle drops,
        // shutdown joins the workers and returns; the job -- and its Submitter -- are dropped
        // afterwards without ever joining anything from a worker thread.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let pounce = Pounce::new(u64::MAX, 1, 1, || {});
            // Occupy the only CPU worker so the chain job stays queued.
            pounce.submit(Box::new(StepJob {
                spec: cpu_spec(),
                label: "blocker".into(),
                remaining: 1,
                on_step: Some(Box::new(|| std::thread::sleep(Duration::from_millis(100)))),
            }));
            pounce.submit(Box::new(ChainJob {
                submitter: pounce.submitter(),
                next: None,
                submitted: Arc::new(Mutex::new(None)),
            }));
            drop(pounce);
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "dropping the last Pounce handle hung"
        );
        handle.join().unwrap();
    }
}
