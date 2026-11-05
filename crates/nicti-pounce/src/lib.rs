//! Pounce (#54/#55, ADR-0054): the production job scheduler. Promotes `spikes/crouch`'s
//! research (`job`/`cancel`/`queue`/`admission`/`throttle`/`telemetry`) into a real threaded
//! runtime (`runtime::Pounce`) real callers can submit jobs to -- Scruff/Patrol import and sync
//! (`nicti-lair::pounce_jobs`) are the first, `crates/nicti-pelt::activity` is the UI consumer.
//!
//! `spikes/crouch` itself is unchanged and stays in place: its own GPU-contention harnesses
//! (`gpu_contend`/`ort_contend`), the tile-granular hero-scenario sim (`sim`), and the
//! `prefetch::priority_order` copy are research tooling this crate doesn't need and doesn't
//! promote. See `.claude/rules/jobs/REFERENCE.md` for the full decision history.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod admission;
pub mod cancel;
pub mod guard;
pub mod hackles;
pub mod job;
pub mod queue;
pub mod runtime;
pub mod telemetry;
pub mod throttle;

/// Locks `m`, recovering the data if another thread panicked while holding it. Pounce's
/// poisoning must not cascade into every other thread. Job `step`/`progress`/drop run under
/// `catch_unwind`; a panicking `ChunkedJob::spec()` under the scheduler lock is not guarded, so
/// a buggy job impl can still leave its lane's queue truncated (its worker dies with it).
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub use job::{ChunkedJob, JobError, JobId, JobKind, JobSpec, Lane, Priority, Progress, Step};
pub use runtime::{JobState, JobStatus, Pounce, Submitter};
