//! Job model: a chunked, cooperatively-cancellable unit of work. GPU dispatches can't be
//! preempted mid-flight (ADR-0054's own bake-scheduling contract only asks for "cancellable or
//! reprioritized" at job granularity, not mid-dispatch) -- so "cancellation" here means "stop
//! submitting further chunks", and the chunk boundary is what actually bounds foreground
//! preemption latency. A chunk is whatever unit a real job does as one step -- one file ingested
//! (Scruff/Patrol, this crate's first real clients), one SCUNet tile, one wgpu dispatch+readback.
//!
//! Promoted from `spikes/crouch::job` (ADR-0054) with three production additions: a `label` for
//! the activity panel, a `progress` readout, and a fallible `step` (a failing job is reported
//! through the panel, not a panic that would take the worker thread down with it).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job#{}", self.0)
    }
}

/// Priority 0 (foreground UI) always preempts priority 1 (background batch) -- ADR-0054's own
/// decision rule #3. There are exactly two classes; a job doesn't get to declare a third.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Foreground,
    Background,
}

/// Which lane a job runs on. GPU is exactly one serial worker (one shared `wgpu::Device`/one
/// `ort::Session`, per ADR-0016/0019/0050) -- ADR-0054's own one-chunk-in-flight rule. CPU is a
/// user-throttled pool of worker threads for CPU/disk-bound work (Scruff/Patrol's own scan), a
/// separate lane so CPU-bound background work never waits behind the GPU worker (#206's own
/// finding: decode is CPU-only and doesn't need to serialize behind GPU dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    Gpu,
    Cpu,
}

/// What kind of work a job represents -- used only for reporting/telemetry, the scheduler itself
/// doesn't branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    LiveRender,
    Bake,
    Export,
    Import,
    Sync,
    Backup,
    /// A RAW decode (#31) -- CPU-only, distinct from `Bake` (a GPU-lane Tapetum stage bake) per
    /// #206's own finding that decode shouldn't serialize behind GPU dispatch.
    Decode,
}

/// Static metadata a job declares up front, before it runs -- what the scheduler needs to make an
/// admission/ordering decision without running the job.
#[derive(Debug, Clone, Copy)]
pub struct JobSpec {
    pub priority: Priority,
    pub kind: JobKind,
    pub lane: Lane,
    /// Declared peak VRAM this job needs while a chunk is in flight -- checked by
    /// `admission::Admission` before a background job's next chunk is allowed to start (ADR-0054
    /// decision rule #5). Foreground jobs are never refused on VRAM grounds -- see
    /// `admission.rs`'s own doc comment for why. Always `0` for a `Lane::Cpu` job -- VRAM
    /// admission only applies to the GPU lane.
    pub vram_bytes: u64,
    /// Which image (or None for a job not tied to a specific image, e.g. export) this job's
    /// background priority is ordered by distance-from-cursor against (`prefetch::priority_order`
    /// in `nicti-tapetum`, not reproduced in this crate).
    pub image_index: Option<usize>,
}

/// How far along a running job is. `total` is `None` when the job can't know its own total up
/// front (Scruff's directory walk is lazy) -- a caller must render that as an indeterminate
/// spinner, never fabricate a fake total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
}

/// What happened after running one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// More chunks remain -- eligible to be re-enqueued (and re-ordered) before its next chunk
    /// runs.
    Yield,
    /// This job is finished; drop it.
    Done,
}

/// A job's own report of what went wrong. Kept as a plain message rather than a typed error per
/// job kind -- the scheduler and activity panel only ever display it, never branch on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct JobError(pub String);

impl JobError {
    pub fn new(msg: impl Into<String>) -> Self {
        JobError(msg.into())
    }
}

/// One chunk of cooperatively-cancellable work. `step` runs exactly one chunk (e.g. one file
/// ingested, one SCUNet tile, one wgpu dispatch+readback) and returns whether more remain, or an
/// error if this chunk failed -- a job that errors is dropped (reported `Failed`, not retried),
/// matching Scruff's own "one bad file doesn't abort the run" policy being the *job's* own
/// responsibility (a job that wants that behavior swallows a per-file error internally and keeps
/// going, the way `IngestJob` does; only a job that can't make any further progress at all should
/// return `Err`). The scheduler checks a job's [`crate::cancel::CancelToken`] *between* chunks,
/// never inside one -- a chunk itself is never interrupted, since neither `wgpu::Queue::submit`
/// nor `ort::Session::run` support mid-call cancellation.
pub trait ChunkedJob: Send {
    fn spec(&self) -> JobSpec;
    /// A short, human-readable label for the activity panel (e.g. "Import: D:\\Photos\\2026").
    /// Read once at submit time -- a job's label doesn't change over its own lifetime.
    fn label(&self) -> String;
    /// Current progress. Called after every `step()` -- must be cheap.
    fn progress(&self) -> Progress;
    fn step(&mut self) -> Result<Step, JobError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_outranks_background() {
        assert!(Priority::Foreground < Priority::Background);
    }
}
