---
paths:
  - "spikes/den/**"
  - "crates/nicti-catalog/**"
---

# Catalog Engine — Quick Reference

Full reasoning/history: `.claude/docs/catalog-engine/README.md`.

- **Chosen: SQLite** (`rusqlite`, WAL) — `docs/adr/0008`. `(model, rating)` composite index,
  `GLOB` not `LIKE` for prefix scans. Clears every gate at 2M assets except
  faceted-filter-with-facet-counts (closed by the facet-count cache below). DuckDB kept as the
  explicit fallback if the facet-query ceiling becomes a real problem.
- **Turso** (`docs/adr/0009`), **redb** (`docs/adr/0010`), **RocksDB** (`docs/adr/0015`) —
  **not adopted**: each misses 1-3 query-gate budgets at 2M by 1.5–5x, and crash-safety is left
  inconclusive for all three (the in-process `mem::forget` crash simulation can't get past their
  OS-level file locks — a structural test-harness limit, not a finding about the engines). RocksDB
  additionally got the series' only concurrent-multi-writer measurement: beats SQLite's
  single-writer-serialization ceiling on peak throughput but shows large untuned run-to-run
  variance — SQLite still chosen, kept as reference data for a future multi-writer decision (#64).
- **Facet-count cache** (`docs/adr/0011`) — **adopted**: trigger-maintained SQLite facet table,
  closes ADR-0008's one measured miss without a new dependency. Clears budget 33-89x at 600k/2M.
  Only answers keyword-narrowed facet queries; an unfiltered facet count needs a separate query.
- **DuckDB as primary store** (`docs/adr/0012`) — **not adopted**: DuckDB compacts #22's planned
  append-only history log ~80x slower per op than SQLite (a real transaction-commit-overhead
  effect, not a benchmark artifact). DuckDB's JSON support is real but doesn't offset this.
- **libSQL** (`docs/adr/0014`) — **not adopted for v1, not a permanent rejection**: only KV/pure-
  Rust-adjacent candidate to cleanly pass the crash-safety gate. Real 1.3–4x per-op overhead
  (async-dispatch), ~5x larger dependency graph. Flagged as the leading candidate to revisit when
  #64 (multi-machine catalog) becomes active — built-in offline-first sync. **Cannot link into the
  same binary as `rusqlite`** (both bundle SQLite C symbols) — CI splits `den`'s test job.
- **fjall** (`docs/adr/0016`) — **not adopted**: cleanest Windows-build story (100% safe Rust, no
  `build.rs`) but fails 3/8 query gates at 600k already (the earliest/widest failure in the
  series), attributed to `Guard`/iterator overhead. Crash-safety inconclusive (same OS-lock class
  as Turso/redb). Links cleanly alongside every other candidate, unlike libSQL.
