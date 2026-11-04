//! Benchmark + golden-image harness (#17). See `docs/benchmarks.md` for the methodology this
//! crate implements, and each module's own docs for the specific piece.

// Never-crash policy (#538): production code returns errors instead of panicking. Tests
// are exempt. Needs a documented reason to `#[allow]` on any single site.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod gate;
pub mod golden;
pub mod manifest;
pub mod materialize;
pub mod metrics;
pub mod perf;
pub mod refset;
