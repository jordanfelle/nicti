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

// #107's schema-fit prototype: JSON-column + append-only/burst-compacted history table, per
// ADR-0002's planned #22 shape — a different question from `workload.rs`'s OLTP filter/sort/range
// gates. Needs both engines to compare, so it's gated on both features rather than either alone.
#[cfg(all(feature = "sqlite", feature = "duckdb"))]
pub mod schema_fit;

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

// #113's follow-up candidate, added after ADR-0010 (`redb`) merged — the actual SQLite C-source
// fork (not a from-scratch rewrite like Turso Database), so the query set stays byte-identical to
// `sqlite.rs`'s own SQL text on purpose. Module named `libsql_engine`, not `libsql`, to avoid
// shadowing the external `libsql` crate it wraps (same convention as `turso_engine`/`redb_engine`).
#[cfg(feature = "libsql")]
pub mod libsql_engine;

// #115's candidate, added after ADR-0010 merged — like LMDB/redb, no query planner (hand-
// maintained secondary indexes, same shape as `lmdb.rs`). Its own crash-safety gate is
// inconclusive via this harness for the same reason as redb's (a leaked-fd-scoped OS lock, not a
// process-wide guard like LMDB's) — see ADR-0015. Its own genuine multi-threaded-writer story is
// worth measuring directly (no prior candidate's crash test exercised concurrent writers either).
// Module named `rocksdb_engine`, not `rocksdb`, to avoid shadowing the external `rocksdb` crate.
#[cfg(feature = "rocksdb")]
pub mod rocksdb_engine;

// #115's own reason for existing: a real concurrent-multi-writer-thread comparison between
// RocksDB and SQLite, not an assumption. Needs both engines to compare against each other.
#[cfg(all(feature = "rocksdb", feature = "sqlite"))]
pub mod concurrent_bench;

pub use gen::{generate_catalog, Asset, Flag};
pub use workload::{FacetCounts, RangeQuery, Workload};
