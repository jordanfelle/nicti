//! Pounce (#54/#55, ADR-0054): the production job scheduler. Promotes `spikes/crouch`'s
//! research (`job`/`cancel`/`queue`/`admission`/`throttle`/`telemetry`) into a real threaded
//! runtime (`runtime::Pounce`) real callers can submit jobs to -- Scruff/Patrol import and sync
//! (`nicti-lair::pounce_jobs`) are the first, `crates/nicti-pelt::activity` is the UI consumer.
//!
//! `spikes/crouch` itself is unchanged and stays in place: its own GPU-contention harnesses
//! (`gpu_contend`/`ort_contend`), the tile-granular hero-scenario sim (`sim`), and the
//! `prefetch::priority_order` copy are research tooling this crate doesn't need and doesn't
//! promote. See `.claude/rules/jobs/REFERENCE.md` for the full decision history.

pub mod admission;
pub mod cancel;
pub mod job;
pub mod queue;
pub mod runtime;
pub mod telemetry;
pub mod throttle;

pub use job::{ChunkedJob, JobError, JobId, JobKind, JobSpec, Lane, Priority, Progress, Step};
pub use runtime::{JobState, JobStatus, Pounce};
