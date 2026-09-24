//! Spike for #67: compares embedded catalog-database engines against the query patterns and
//! scale in `docs/benchmarks.md`. Not production code — see `CLAUDE.md`'s package map.

pub mod gen;
pub mod stats;
pub mod workload;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "duckdb")]
pub mod duckdb_engine;

// No `pglite` module: both embedded-Postgres candidates hard-gate-failed before a backend was
// worth writing — see ADR-0008's Measured results / Options considered. pglite-rs's build.rs
// unconditionally passes a Unix-only linker flag (no Windows path at all); pglite-oxide's own
// published dependency graph (wasmer-wasix 0.702.0-alpha.3 against any current virtual-net
// release) fails to compile on any target, confirmed independently against two virtual-net
// versions while investigating this spike.
#[cfg(feature = "lmdb")]
pub mod lmdb;

pub use gen::{generate_catalog, Asset, Flag};
pub use workload::{FacetCounts, RangeQuery, Workload};
