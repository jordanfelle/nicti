//! Job model: a chunked, cooperatively-cancellable unit of work. GPU dispatches can't be
//! preempted mid-flight (confirmed by ADR-0044's own bake-scheduling contract, which only asks
//! for "cancellable or reprioritized" at job granularity, not mid-dispatch) -- so "cancellation"
//! here means "stop submitting further chunks", and the chunk boundary is what actually bounds
//! foreground preemption latency. A chunk is whatever unit a real bake stage submits as one GPU
//! command-buffer/`ort::Session::run` call -- e.g. one SCUNet tile (ADR-0040: 44.8ms p50 at
//! 256px on the RTX 5080), not a whole-image bake.

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

/// What kind of work a job represents -- used only for reporting/telemetry, the scheduler itself
/// doesn't branch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    LiveRender,
    Bake,
    Export,
    Import,
}

/// Static metadata a job declares up front, before it runs -- what the scheduler needs to make an
/// admission/ordering decision without running the job.
#[derive(Debug, Clone, Copy)]
pub struct JobSpec {
    pub priority: Priority,
    pub kind: JobKind,
    /// Declared peak VRAM this job needs while a chunk is in flight -- checked by
    /// `admission::Admission` before a background job's next chunk is allowed to start (ADR-0054
    /// decision rule #5). Foreground jobs are never refused on VRAM grounds -- see
    /// `admission.rs`'s own doc comment for why.
    pub vram_bytes: u64,
    /// Which image (or None for a job not tied to a specific image, e.g. export) this job's
    /// background priority is ordered by distance-from-cursor against (`prefetch::priority_order`).
    pub image_index: Option<usize>,
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

/// One chunk of cooperatively-cancellable work. `step` runs exactly one chunk (e.g. one SCUNet
/// tile, one wgpu dispatch+readback) and returns whether more remain. The scheduler checks a
/// job's [`crate::cancel::CancelToken`] *between* chunks, never inside one -- a chunk itself is
/// never interrupted, since neither `wgpu::Queue::submit` nor `ort::Session::run` support
/// mid-call cancellation.
pub trait ChunkedJob: Send {
    fn spec(&self) -> JobSpec;
    fn step(&mut self) -> Step;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_outranks_background() {
        assert!(Priority::Foreground < Priority::Background);
    }
}
