//! Research spike for #54 (ADR-0054): Pounce, the job scheduler ADR-0044 (Tapetum) hands its own
//! bake-scheduling contract to. Not a production crate -- see CLAUDE.md's package-map note on
//! `spikes/*`.

pub mod admission;
pub mod cancel;
pub mod gpu_contend;
pub mod job;
pub mod ort_contend;
pub mod prefetch;
pub mod queue;
pub mod sim;
pub mod telemetry;
pub mod throttle;
