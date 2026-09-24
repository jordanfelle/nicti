//! Spike for #67: compares embedded catalog-database engines against the query patterns and
//! scale in `docs/benchmarks.md`. Not production code — see `CLAUDE.md`'s package map.

pub mod gen;
pub mod stats;
pub mod workload;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "duckdb")]
pub mod duckdb_engine;

// #103's two facet-count-cache candidates for SQLite's faceted-filter gap (ADR-0008). Both build
// on `sqlite.rs` directly (its `connection()`/`naive_faceted_filter()` escape hatches), so both
// require the `sqlite` feature; the DuckDB-backed candidate additionally requires `duckdb`.
#[cfg(feature = "sqlite")]
pub mod facet_cache_trigger;

#[cfg(all(feature = "sqlite", feature = "duckdb"))]
pub mod facet_cache_duckdb;

// No `pglite` module: both embedded-Postgres candidates hard-gate-failed before a backend was
// worth writing — see ADR-0008's Measured results / Options considered. pglite-rs's build.rs
// unconditionally passes a Unix-only linker flag (no Windows path at all); pglite-oxide's own
// published dependency graph (wasmer-wasix 0.702.0-alpha.3 against any current virtual-net
// release) fails to compile on any target, confirmed independently against two virtual-net
// versions while investigating this spike.
#[cfg(feature = "lmdb")]
pub mod lmdb;

// #102's follow-up candidate, added after ADR-0008 merged — the only pure-Rust engine in this
// comparison, matching ADR-0001's own stated preference. Module named `turso_engine`, not
// `turso`, to avoid shadowing the external `turso` crate it wraps.
#[cfg(feature = "turso")]
pub mod turso_engine;

// #106's follow-up candidate, added after ADR-0009 merged — like LMDB, no query planner (hand-
// maintained secondary indexes); unlike LMDB, a pure-Rust dependency and, per ADR-0010's own
// crash-safety finding, no process-wide open-environment guard blocking in-process reopen after a
// forgotten write transaction. Module named `redb_engine`, not `redb`, to avoid shadowing the
// external `redb` crate it wraps.
#[cfg(feature = "redb")]
pub mod redb_engine;

pub use gen::{generate_catalog, Asset, Flag};
pub use workload::{FacetCounts, RangeQuery, Workload};
