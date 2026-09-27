---
paths:
  - "spikes/den/**"
  - "crates/nicti-catalog/**"
---

# Catalog Engine — Quick Reference

Full reasoning/history: `docs/decisions/catalog-engine.md`.

- **Chosen: SQLite** (`rusqlite`, WAL) — `docs/adr/0067`. `(model, rating)` composite index,
  `GLOB` not `LIKE` for prefix scans. Clears every gate at 2M assets except
  faceted-filter-with-facet-counts (closed by the facet-count cache below). DuckDB kept as the
  explicit fallback if the facet-query ceiling becomes a real problem.
- **Turso** (`docs/adr/0102`) — **not adopted**: two query shapes already sit at the 600k-scale
  budget edge, but the deciding factor was a 2M-row bulk-ingest run whose WAL file passed 19GB and
  was still climbing linearly (not explained by a durability-pragma mismatch). Crash-safety left
  inconclusive (same OS-lock structural limit as below). Revisit post-1.0, not never.
- **redb** (`docs/adr/0106`), **RocksDB** (`docs/adr/0115`) — **not adopted**: each misses 1-3
  query-gate budgets at 2M by 1.5–5x, and crash-safety is left inconclusive for both (the
  in-process `mem::forget` crash simulation can't get past their OS-level file locks — a
  structural test-harness limit, not a finding about the engines). RocksDB additionally got the
  series' only concurrent-multi-writer measurement: beats SQLite's single-writer-serialization
  ceiling on peak throughput but shows large untuned run-to-run variance — SQLite still chosen,
  kept as reference data for a future multi-writer decision (#64).
- **Facet-count cache** (`docs/adr/0103`) — **adopted**: trigger-maintained SQLite facet table,
  closes ADR-0067's one measured miss without a new dependency. Clears budget 33-89x at 600k/2M.
  Only answers keyword-narrowed facet queries; an unfiltered facet count needs a separate query.
- **DuckDB as primary store** (`docs/adr/0107`) — **not adopted**: DuckDB compacts #22's planned
  append-only history log ~80x slower per op than SQLite (a real transaction-commit-overhead
  effect, not a benchmark artifact). DuckDB's JSON support is real but doesn't offset this.
- **libSQL** (`docs/adr/0113`) — **not adopted for v1, not a permanent rejection**: only KV/pure-
  Rust-adjacent candidate to cleanly pass the crash-safety gate. Real 1.3–4x per-op overhead
  (async-dispatch), ~5x larger dependency graph. Flagged as the leading candidate to revisit when
  #64 (multi-machine catalog) becomes active — built-in offline-first sync. **Cannot link into the
  same binary as `rusqlite`** (both bundle SQLite C symbols) — CI splits `den`'s test job.
- **fjall** (`docs/adr/0116`) — **not adopted**: cleanest Windows-build story (100% safe Rust, no
  `build.rs`) but fails 3/8 query gates at 600k already (the earliest/widest failure in the
  series), attributed to `Guard`/iterator overhead. Crash-safety inconclusive (same OS-lock class
  as Turso/redb). Links cleanly alongside every other candidate, unlike libSQL.

## Package contents

- **`spikes/den`** (#67/ADR-0067's catalog-database-engine comparison plus #102/ADR-0102's Turso
  follow-up, #106/ADR-0106's `redb` follow-up, #103/ADR-0103's facet-count-cache follow-up,
  #107/ADR-0107's schema-fit reconsideration, #113/ADR-0113's `libSQL` follow-up, #115/ADR-0115's
  RocksDB follow-up, and #116/ADR-0116's `fjall` follow-up) — one module per candidate:
  `sqlite.rs`/`duckdb_engine.rs`/`lmdb.rs`/`turso_engine.rs`/`redb_engine.rs`/`libsql_engine.rs`/
  `rocksdb_engine.rs`/`fjall_engine.rs`/`facet_cache_trigger.rs`/`facet_cache_duckdb.rs`, behind
  matching Cargo features (`turso`, `redb`, `libsql`, `rocksdb`, `fjall` all default-off,
  evaluated-not-adopted, kept for reference — `libsql` cannot be enabled in the same binary as
  `sqlite`, both bundle their own SQLite C symbols and collide at link time, see ADR-0113's Spike
  section; `rocksdb`/`fjall` have no such collision, see ADR-0115's/ADR-0116's Consequences; the
  two facet-cache modules require `sqlite`, `facet_cache_duckdb` additionally requires `duckdb`),
  plus `schema_fit.rs` (ADR-0021's JSON-column + append-only/burst-compacted history-table shape,
  gated on both `sqlite` and `duckdb`) and `concurrent_bench.rs` (#115's genuinely concurrent
  multi-writer-thread comparison between RocksDB and SQLite, gated on both `rocksdb` and `sqlite`,
  not part of the shared `Workload` trait since only these two engines are compared this way). `gen.rs`'s
  synthetic-catalog generator is reusable for future Library-scale benchmarks (see
  `docs/benchmarks.md`). Not production code — don't build on top of a spike crate; **#22 has now
  landed** (`crates/nicti-catalog`: `schema.rs`/`sqlite.rs`/`scruff.rs`), so this spike is slated
  for deletion — see #123 for the follow-up cleanup (CI jobs, the Renovate rule, `deny.toml`
  exceptions), not done as part of #22 itself.
