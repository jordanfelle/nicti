## Catalog engine

Covers the catalog database engine decision (SQLite) and every evaluated alternative/follow-up: Turso, redb, RocksDB, the facet-count cache, DuckDB, libSQL, and fjall.

- **Catalog database engine**: `docs/adr/0008-catalog-database-engine.md` — **SQLite** (`rusqlite`,
  WAL), with a `(model, rating)` composite index and `GLOB` (not `LIKE`) for every prefix-scan
  predicate — this build's `LIKE`-to-index-range-scan transform never triggered, confirmed via
  `EXPLAIN QUERY PLAN`. Measured in `spikes/den/` against a corrected-cardinality synthetic
  generator (`gen.rs`'s per-event `BENCH_LEAF_KEYWORD`, after the first-pass 11-value keyword
  vocabulary turned out to make every hierarchical query artificially non-selective): clears every
  gate at 2M assets except faceted-filter-with-facet-counts (151ms p95 vs the 100ms budget — a
  known, unattempted mitigation is a trigger-maintained facet table). **DuckDB passed every gate
  with the best margins of any candidate, including the point-update gate the issue predicted it
  would fail** — not chosen for v1 only because #22's row-store schema fits SQLite's maturity
  better, kept explicit as the fallback if the facet-query ceiling becomes a real problem. **LMDB
  had the best raw numbers on every indexable op, but its crash-safety gate could not be
  measured**: `heed`/`liblmdb`'s process-wide open-environment guard makes the in-process
  `mem::forget`-based crash simulation this spike used structurally inapplicable (confirmed, not
  assumed) — a real fork+exec+SIGKILL harness is the only way to test it, out of scope this pass.
  Both embedded-Postgres candidates named in the issue were eliminated at the hard-gate stage
  before either got a spike: `pglite-rs`'s `build.rs` unconditionally emits a Unix-only linker
  flag (no Windows path exists in the crate); `pglite-oxide` doesn't compile against its own
  published dependency graph on any target (`wasmer-wasix` vs `virtual-net`, confirmed on two
  versions). Unblocks #22, #23, #24, #25, #71.
- **Turso Database, evaluated post-ADR-0008**: `docs/adr/0009-turso-database-evaluation.md` —
  **not adopted**. The only pure-Rust catalog candidate (matching ADR-0001's own stated
  preference), with real Windows-CI and MIT-license evidence, but its pre-1.0 status shows up
  where it matters: two query shapes already sit at the 600k-scale budget edge due to a
  confirmed-missing `LIKE`/`GLOB` prefix-scan optimization (a competing "missing index"
  explanation was tested and ruled out), and — the deciding factor — a 2M-row bulk-ingest run was
  stopped after its WAL file passed 19GB and was still climbing linearly (data that should be a
  few hundred MB in any other engine; confirmed not explained by a durability-pragma mismatch,
  tested directly). Crash-safety is left **inconclusive**, not failed: every reopen after an
  abandoned transaction hit `database is locked`, but a hostile re-review and a follow-up
  experiment produced two results that don't fully agree on why — see the ADR's own crash-safety
  row for the full, genuinely unresolved account, plus a new `Workload::prepare_for_forget` hook
  this investigation added. ADR-0008 is unchanged: SQLite stays chosen, DuckDB stays the fallback.
  Revisit post-1.0, not never.
- **`redb`, evaluated post-ADR-0009**: `docs/adr/0010-redb-evaluation.md` — **not adopted**. The
  strongest hard-gate evidence of any pure-Rust candidate so far (real Windows CI, MIT/Apache-2.0
  license, 1.0-plus and zero dependencies, unlike Turso's pre-1.0 status) — but two query shapes
  miss the 2M budget (range query ~1.5x over, filename search ~5x over), root-caused to a real,
  documented cost: redb checksums every page on read (confirmed via its own design docs and its
  own upstream benchmark table, which independently shows the same ~1.5–2x-slower-than-LMDB
  pattern on read-heavy workloads specifically). Crash-safety is **inconclusive**, but for a
  cleanly understood reason this time, not a murky one like Turso's: a direct follow-up experiment
  confirmed the reopen failure is an OS-level advisory lock scoped to the one leaked file
  descriptor (not a process-wide guard like LMDB's) — the same *class* of fd-scoped lock ADR-0009
  found in Turso, which this in-process `mem::forget` technique can never get past regardless of
  engine. ADR-0008 is unchanged: SQLite stays chosen, DuckDB stays the fallback.
- **RocksDB, evaluated post-ADR-0010**: `docs/adr/0015-rocksdb-evaluation.md` — **not adopted**.
  Strong Windows-build/license/maintenance evidence (including a real Windows CI gotcha found and
  mirrored into this repo's own CI: `librocksdb-sys`'s `bindgen`-generated FFI bindings need
  libclang, which conflicts with GitHub's `windows-latest` runner's bundled msys64 install unless
  removed first). Two query shapes miss the 2M budget (range query ~1.8x over, filename search
  ~4.5x over, the worst of any candidate on this shape). The unflushed-memtable hypothesis was
  tested and ruled out (compaction was tried and did not help); the remaining explanation
  (ordinary LSM per-read block-cache cost under this pass's default, untuned configuration) is
  consistent with RocksDB's architecture but not independently isolated — no bloom-filter policy
  is even configured, so that specific mechanism an earlier draft named isn't actually in play.
  Crash-safety is **inconclusive**, same class of
  finding as LMDB/Turso/redb: a leaked `LOCK` file blocks reopening the same forgotten path in this
  in-process technique, confirmed (via a direct probe, same methodology as ADR-0009/0010) to be
  scoped to that specific path, not a process-wide guard like LMDB's. **The concurrent-multi-writer
  comparison this ADR exists to produce — measured for the first time in this series, since every
  prior candidate was only ever benchmarked single-threaded — has a genuine, nuanced answer**:
  RocksDB does not show SQLite's own textbook single-writer-serialization signature (SQLite's
  corrected aggregate throughput settles into a ~93k-98k writes/sec range at 8-16 threads, not
  scaling with thread count, while its own max latency grew monotonically from under 1ms to
  1,963ms under contention — a clean, reproduced confirmation of the exact concern #115 was filed
  to test, after fixing a real benchmark bug a hostile review caught: `synchronous=NORMAL` was only
  ever set on the setup connection, not each worker's own connection, silently running the timed
  writes under SQLite's stricter default `FULL` instead), and RocksDB's peak observed throughput
  (1.05M writes/sec) was roughly 8-12x SQLite's corrected range on a minimal KV-shaped write (not a
  faithful `write_rating`-equivalent op — a hostile review caught this ADR overclaiming operation-
  shape equivalence, corrected there) — but RocksDB's own default (untuned) configuration showed
  large, non-monotonic run-to-run variance under sustained concurrent load (as low as 47.7k
  writes/sec in one 16-thread run, now clearly below SQLite's entire corrected range); the cause
  remains unresolved
  (RocksDB's own documented write-stall backpressure mechanism and this session's own heavily-loaded
  shared host are both plausible, uneliminated explanations) — not a clean win either way. A tuned re-run (larger
  block cache/write buffers, bloom-filter tuning) is the named,
  unattempted follow-up if RocksDB is ever reconsidered. ADR-0008 is unchanged: SQLite stays
  chosen, DuckDB stays the fallback — but this ADR's own numbers are the first real evidence in
  this repo of what SQLite's concurrency tradeoff actually costs, worth remembering if a future
  multi-writer feature (#64) ever forces a re-decision.
- **Facet-count cache for SQLite's faceted-filter gap**: `docs/adr/0011-facet-count-cache.md` —
  **trigger-maintained SQLite facet table**, closing ADR-0008's one measured miss (faceted-filter
  at 2M) without adding a new dependency. Clears the <100ms budget by ~33-89x at 600k/2M (well
  under 1ms-3ms p95); the actual per-write gate ADR-0008 sets (`write_rating` ≤5ms) clears with
  10x+ margin at both scales, though a 100-row rating burst (a proxy for #43's rate-and-advance
  culling pattern) is a real, non-negligible added cost (9-20ms, vs. plain SQLite's <2.1ms) — found
  via a benchmark bug (a trigger `WHEN`-guard no-op on repeated same-value writes) that a hostile
  review caught and this ADR documents in full. Verified correct against a from-scratch
  recomputation both at ingest and under a write burst. A DuckDB-backed read-side cache alternative
  was also built and measured (also clears budget, but adds a second store, a real multi-second
  refresh cost at 2M, and a demonstrated staleness window between refreshes) and is kept as the
  explicit fallback, not adopted. Both candidates share a real, verified scope limitation: their
  `(model, rating, keyword)`-grain facet table only answers a *keyword-narrowed* facet query
  correctly — an unfiltered/no-keyword facet count needs a separate table or `sqlite.rs`'s own
  from-scratch query. Unblocks #22's facet-count implementation.
- **DuckDB as v1 primary catalog store, reconsidered post-ADR-0008**:
  `docs/adr/0012-duckdb-as-primary-catalog-store.md` — **not adopted**. ADR-0008's decision is
  unchanged, now for a demonstrated technical reason instead of a soft one: #22's planned
  append-only history log with burst-compaction (ADR-0002) needs frequent UPDATE+DELETE-heavy
  operations, and a real prototype (`spikes/den/src/schema_fit.rs`) measured DuckDB compacting the
  same history runs SQLite compacts **~80x slower per operation** (4.297ms/op vs 0.054ms/op,
  960,000 raw rows down to 19,963 compacted rows), turning a ~1-second workload into an ~86-second
  one — a well-understood transaction-commit-overhead effect (matches ADR-0008's own single-row
  `write_rating` cost roughly doubled), not a benchmark artifact. DuckDB's JSON support
  (`json_extract`) is confirmed real and usable — that part of the schema-fit question favors
  DuckDB — but doesn't offset the compaction cost. #103's separate SQLite-trigger-vs-DuckDB-sidecar
  facet-cache work is unaffected by this outcome.
- **libSQL, evaluated post-ADR-0012**: `docs/adr/0014-libsql-evaluation.md` — **not adopted for
  v1** (not a permanent rejection — see below). Unlike Turso Database (ADR-0009, a from-scratch
  Rust rewrite) and `redb` (ADR-0010), libSQL is an actual fork of SQLite's own C source, and it
  shows: it's the first KV-shaped-or-pure-Rust-adjacent candidate in this series to **cleanly
  pass** the crash-safety hard gate (0/20 reopen failures, real `PRAGMA integrity_check`), where
  LMDB/Turso/`redb` all left it inconclusive. Measured gates match plain SQLite's own margins
  exactly, including the same known 2M faceted-filter miss (already mitigated by ADR-0011).
  #113's own specific question — does the embedded-replica feature cost anything when unused —
  resolves cleanly at the source level (opening a local file never constructs the `Sync`/
  `Offline`/`Remote` `DbType` variants or spawns any background task), but a real 1.3–4x
  per-operation overhead exists on most ops anyway (async-dispatch cost from the crate's
  `tokio`-wrapped API, the same architectural shape as Turso Database's own overhead, just
  smaller — `write_rating` and folder-subtree-count tie, named explicitly rather than folded into
  a blanket claim an earlier draft made and a hostile review caught), plus a ~5x larger dependency
  graph from the crate's default features
  (`tonic`/`tower`/`hyper`/`h2`, unused by this engine's code path). Not enough reason to prefer
  it over plain SQLite for v1's single-machine catalog — but flagged as the leading candidate to
  revisit specifically when #64 (multi-machine catalog) becomes active, since it's the only
  evaluated engine combining real SQLite's own reliability track record with a built-in
  offline-first sync mechanism. A real, structural finding along the way: `libsql`'s bundled
  SQLite C fork and `rusqlite`'s bundled SQLite cannot link into the same binary (both define the
  same C symbols) — required fixing two pre-existing cfg-gating gaps in `bin/den.rs`/
  `tests/facet_cache.rs` and splitting `.github/workflows/ci.yml`'s `cargo test` job so `den` gets
  its own feature-scoped commands instead of one blanket `--all-features` invocation.
- **fjall, evaluated post-ADR-0014**: `docs/adr/0016-fjall-evaluation.md` — **not adopted**. The
  cleanest Windows-build story of any catalog candidate so far (fjall and its own `lsm-tree`
  dependency have no `build.rs` at all — 100% safe Rust, no native C/C++ core to audit) and a real,
  active maintenance signal (v3.1.10, 25 days old at spike time) — but **fails 3 of 8 measured
  query gates (folder-subtree count, range query, filename search) at both 600k and 2M**, by 2–5x,
  the widest and earliest measured-gate failure of any candidate in this series (SQLite's and
  `redb`'s own misses only showed up at the 2M planning horizon, not already at 600k). Investigated
  directly rather than assumed: ruled out post-ingest LSM-compaction lag as the cause (calling
  `major_compact()` barely moved the number), confirmed fjall's default LZ4 block compression is a
  real but only partial contributor (~2–3x, not enough to close the gap to LMDB/`redb`'s zero-copy
  mmap reads), and attributes the remainder to fjall's own `Guard`/iterator overhead per matched
  item — a real, still-open question a future profiling pass could resolve, not fully closed here.
  Crash-safety is **inconclusive**, the same structural finding as Turso/`redb`: fjall holds an
  OS-level advisory file lock (`std::fs::File::try_lock()`) released only on a clean `Drop`, which
  this spike's `mem::forget`-based crash simulation always skips — 20/20 reopen failures, all the
  identical `FjallError: Locked`, not a torn-write finding. A real, structural discovery along the
  way: fjall's atomic `Database::batch()` stages entirely in process memory until `.commit()`
  (unlike LMDB's/`redb`'s open write transactions, which touch on-disk structures pre-commit), so
  it has no read-your-own-writes within one open batch, and its `crash_mid_ingest` test — leaving a
  genuinely open, uncommitted batch, then `mem::forget`ing it — structurally cannot leave any
  on-disk trace for fjall at all, unlike every prior candidate. Unlike libSQL, fjall has **no
  bundled native C source**, so it links cleanly into the same `den` binary as every other
  candidate (no CI job split needed, unlike ADR-0014's mandatory `--exclude den` fix). One real
  `deny.toml` edit was needed: `varint-rs` (transitive via `lsm-tree`) carries `0BSD`, not
  previously allowlisted.
