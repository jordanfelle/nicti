# ADR-0009: Turso Database, evaluated for the catalog store — not adopted

- **Status:** Rejected (not a rejection of the pure-Rust idea itself — see Consequences)
- **Date:** 2026-09-24
- **Ticket:** [#102](https://github.com/jordanfelle/nicti/issues/102) Research: Turso Database (pure-Rust SQLite rewrite) as a catalog engine candidate

## Context

ADR-0008 chose SQLite as the v1 catalog store and named its one real gap (faceted-filter-with-counts
missed the 2M budget by ~1.3–1.6x). That decision prompted #102: Turso Database
(`tursodatabase/turso`, formerly "Limbo") is a from-scratch, pure-Rust rewrite of a
SQLite-compatible engine — the only catalog candidate across ADR-0008/#102 that would actually
match ADR-0001's own stated preference (memory safety, no C/C++ core). Worth its own hard-gate and
measured pass, not a footnote, per #102's own framing — but #102 also named real caveats to weigh
rather than assume away: pre-1.0 (`v0.8.0-pre.12`), "some features explicitly marked experimental,"
and a distinct, more mature sibling project (`libSQL`) not to be confused with it.

## Decision rule

Same hard-gate + measured-gate rule as ADR-0008, against the same `spikes/den/` `Workload` trait,
generator, and 600k/2M synthetic scale.

## Decision

**Not adopted.** Hard gate 1 (Windows build) and gate 2 (license) both pass with real evidence —
better evidence, in fact, than any other candidate in ADR-0008 had going in. Hard gate 3
(crash-safety) is left **inconclusive**, not failed — see below for why this pass's in-process
technique couldn't settle it either way. The decision doesn't rest on that gate: a confirmed-real,
already-open-upstream WAL-bloat problem during bulk ingest, two query shapes already at the
600k-scale budget edge from a confirmed-missing optimizer feature, and markedly slower bulk-insert
throughput are independently sufficient. This candidate is not production-ready for Nicti's catalog
yet, which is exactly the risk #102 flagged before any code was written.

## Measured results

**Hard gates:**

| Gate | Result |
|---|---|
| 1. Windows build | ✅ Confirmed via `tursodatabase/turso`'s own `.github/workflows/rust.yml`: a real 3-OS matrix including a Windows runner, running both `cargo build --all-features` and `cargo nextest run --workspace --all-features` there — not just a doc claim. Materially stronger evidence than the pglite-rs/pglite-oxide eliminations in ADR-0008. |
| 2. License | ✅ MIT, confirmed from crates.io's version-level API response for the crate itself (not a secondary source). |
| 3. Crash-safety | ⚠️ **Inconclusive — this harness's in-process technique could not produce a trustworthy answer, on either side.** Using the exact same `crash_mid_ingest` technique that worked cleanly for SQLite/DuckDB (leave a transaction open, never `COMMIT`, `mem::forget` the handle), every reopen attempt failed with `database is locked` — 20/20 at default iteration count, still 1/1 in isolation. The first hypothesis (a leaked `tokio::runtime::Runtime`'s worker threads, never joined because `mem::forget` skips `Runtime::drop`) turned out to be incomplete: a hostile re-review read Turso's actual source (`turso_core`'s `io/unix.rs`) and found the real lock is a POSIX `fcntl(F_SETLK)` advisory lock tied to the `Database`/`Connection`'s own file descriptor, released only by closing that fd, process exit, or explicit unlock — none of which involve the tokio runtime at all. That reading predicts that shutting down *just* the runtime while still forgetting `_db`/`conn` should change nothing. It didn't hold up cleanly under direct experiment either way: a **minimal reproduction** (a bare `CREATE TABLE`, a few dozen rows, no indexes) *did* reopen successfully after shutting down only the runtime and still forgetting the database/connection handles — but the same fix, applied to the real `TursoEngine` under its actual multi-table, multi-index schema and 500-row `crash_mid_ingest` workload, still failed identically. Neither the pure "leaked runtime thread" theory nor the pure "fd-level fcntl lock, runtime is irrelevant" theory fully explains both results. What's confirmed is that this in-process `mem::forget` technique does not compose cleanly with Turso's own resource lifecycle for reasons not fully isolated in this pass — the same open question ADR-0008 already flagged as needing a real fork+exec+SIGKILL harness to resolve properly, now doubly true here. **This gate should be read as "not yet answered," not as "Turso's crash-safety is broken"** — a real `SIGKILL` unconditionally closes every fd and releases every lock a process held, which this in-process leak does not faithfully reproduce either way. |
| 5. Maintained | ⚠️ Real, fast-turnaround development (multiple serious WAL/MVCC corruption-class bugs found and fixed within days to weeks per #102's own research), but with durability-relevant issues open in the same category as this ADR's own finding below, including one only 9 days old at research time. |

**Measured gates, 600k assets** (2M was not completed — see the ingest-time finding below, which
made completing it not worth the disk/time cost once the direction was already unambiguous):

| Query | Budget | Turso p50/p95 |
|---|---|---|
| Faceted filter + facet counts | < 100ms | **94.6 / 100.4 ms** — already at the edge at 600k |
| Keyword-subtree query | < 100ms | **89.3 / 90.9 ms** — already at the edge at 600k |
| Folder-subtree count | < 100ms | 26.2 / 26.6 ms |
| Filename substring search | < 100ms | 29.3 / 33.6 ms |
| Range query | < 100ms | 3.2 / 3.2 ms |
| Sort by date, first 500 | < 100ms | 0.08 / 0.09 ms |
| Single rating write | ≤ 5ms | 0.03 / 0.39 ms |
| Cold open | < 2s | 5.3 ms |
| Bulk ingest, 600k rows (informative) | — | 34.9 s — notably slower than SQLite (19.7s) or DuckDB (2.9s) at the same scale, consistent with no bulk-loader/appender API: every row is one awaited `execute()` call. |
| Online backup (informative) | — | 10.7 / 12.6 s — `VACUUM INTO` (still gated behind an `--experimental-vacuum` flag upstream, per #102's own research) failed in practice here, falling back to a plain file copy; see `turso_engine.rs`'s `backup()`. |

Root cause for the two near-budget queries: confirmed via `tursodatabase/turso#8990` (an open
optimizer PR whose own description states this directly) that **no `LIKE`/`GLOB` prefix-scan-to-
index-range-scan optimization exists yet** — unlike `sqlite.rs`, where switching the operator from
`LIKE` to `GLOB` fixed a real index-usage bug, there is no equivalent fix available here. Every
prefix-scan query is a full scan regardless of operator. At 600k this is already within ~1.06x
(faceted filter) and ~1.11x (keyword) of the budget; the extrapolation to 2M is not favorable. A
hostile re-review raised — and this turned out to rule out — a competing explanation: that
`turso_engine.rs`'s schema might simply be missing an index `sqlite.rs` has. It doesn't; the two
schemas carry equivalent composite indexes (`(model, rating)`, `(rating, iso, capture_date)`, a
keyword index), so the missing-optimizer explanation stands.

**The disqualifying finding, found while attempting the 2M bulk-ingest run:** partway through a
single 2M-row insert transaction — data that represents perhaps 300–500MB in any of the other three
engines — the on-disk WAL file (`den-turso.db-wal`) had already grown past **19GB and was still
climbing near-linearly** (15.4GB → 18.9GB in one ~30-second sampling window). The run was stopped
deliberately rather than let it consume further disk for a result that was already unambiguous.
**Tested and ruled out as the explanation:** `turso_engine.rs`'s first version set no `synchronous`/
`journal_mode` pragmas at all, unlike `sqlite.rs`'s explicit `journal_mode=WAL`/`synchronous=NORMAL`
— Turso's own default `synchronous` is `FULL` (confirmed via a throwaway probe), a stricter,
fsync-heavier setting than what SQLite was benchmarked with. Matching it explicitly
(`PRAGMA synchronous = NORMAL`, now in `open()`) and re-running at 600k scale did **not**
meaningfully change the bloat: the WAL still passed 7GB within seconds and kept climbing at a
comparable rate. This is not a query-speed problem and not a durability-setting mismatch — it is
severe, apparently unbounded WAL growth during a large transaction, consistent with real,
currently-open upstream issues surfaced during #102's own research (a checkpoint-can-truncate-the-
log issue, a durability-ordering issue, a shared-WAL-frame monotonicity panic filed only 9 days
before this evaluation).

## Options considered

| Option | Verdict |
|---|---|
| Turso Database, pure-Rust rewrite (`turso` crate) | **Rejected.** Passes the license and Windows-build hard gates with real evidence. Crash-safety is inconclusive (this harness's in-process technique couldn't settle it — see Measured results), not a confirmed failure. Sits at the edge of budget on two query shapes at 600k due to a confirmed-missing optimizer feature (a competing "missing index" explanation was tested and ruled out). The deciding factor: severe WAL growth mid-ingest at 2M scale, confirmed not explained by a durability-pragma mismatch (tested directly), that made completing the run not worth the cost. |
| `libSQL` (`tursodatabase/libsql`, the older C fork) | Not evaluated separately — it's the same SQLite engine family ADR-0008 already measured via `rusqlite`, with extras (embedded replicas, vector search) Nicti doesn't need yet. Nothing about it would change ADR-0008's own SQLite numbers. |

ADR-0008's decision is unchanged: **SQLite remains chosen, DuckDB remains the proven fallback.**

## Consequences

- **The pure-Rust angle was worth checking and wasn't a dead end in principle** — the candidate
  that fits Nicti's own architecture preference best just isn't mature enough *yet*. Revisit once
  Turso Database reaches 1.0 and its WAL/checkpoint behavior under sustained large-transaction load
  has real evidence of being fixed, not just claimed. This is a "not now," not a "never."
- **#22 should not spend any further design effort accommodating Turso.** Proceed on SQLite per
  ADR-0008.
- **This evaluation doubles as a second, independent data point for ADR-0008's own crash-safety
  methodology**: two different engines (LMDB, Turso) both defeated the in-process
  `mem::forget`-based crash simulation. LMDB's cause is fully understood (a deliberate,
  process-wide open-environment guard). Turso's is not — a hostile re-review and a follow-up
  experiment produced two data points that don't fully agree with each other (see Measured
  results), and pinning down the actual mechanism would mean reading deeper into `turso_core`'s own
  async task/connection lifecycle than this pass did. A real fork+exec+SIGKILL harness is looking
  more clearly like required infrastructure for this project's future database-adjacent research,
  not an optional nicety — worth its own ticket if a fifth candidate ever needs evaluating this way,
  and the only way to actually resolve Turso's own crash-safety question with confidence.
- **`tests/cross_engine.rs`'s coverage is narrower than "correctness confirmed" implies**, for all
  four engines, not just Turso: it exercises `bulk_ingest`/`range_query`/`keyword_subtree_query`/
  `faceted_filter`/`sort_by_date_page`/`folder_subtree_count`/`filename_search`/`tag_keyword`/
  `integrity_check`, but never `crash_mid_ingest`, `rate_burst`, or `backup()`. Worth widening in a
  future pass, not specific to this ADR's own conclusion.

## Spike

`spikes/den/src/turso_engine.rs` — implements the same `Workload` trait as the other three
engines, behind the `turso` Cargo feature (default-off, matching the others). The crate's own API
is `async` (not `rusqlite`-shaped), so every method blocks on a per-engine
`tokio::runtime::Runtime` rather than exposing an async trait. `tests/cross_engine.rs` includes
`turso_matches_shared_workload`, which passes — the correctness of what this test actually
exercises was never in question, only production-readiness (and see the coverage caveat above for
what it doesn't exercise).

Two real bugs in this spike's own code were found and fixed before any number here was trusted:
- `bulk_ingest`'s first version captured the result of the row-insert loop but then attempted
  `COMMIT` unconditionally, so a real failure partway through ingest surfaced as a confusing,
  unrelated `Transaction error: cannot commit - no transaction is active` instead of the actual
  underlying error (which is how the WAL-bloat finding above was actually uncovered, once fixed).
- `open()` set no `synchronous`/`journal_mode` pragmas at all, unlike `sqlite.rs`'s explicit
  settings — Turso's own default `synchronous` (`FULL`) is stricter than what SQLite was
  benchmarked with. Fixed by setting `PRAGMA synchronous = NORMAL` explicitly to match; confirmed
  by direct re-measurement that this was not, in fact, what explained the WAL-bloat finding.

`Workload::prepare_for_forget` (a new, default-no-op trait method) exists specifically because of
the crash-safety investigation above: it gives an async engine a hook to shut down its own runtime
cleanly, right before the caller `mem::forget`s the rest of the engine, so a leaked *harness*
thread pool is never conflated with the engine's own on-disk state. It's a real, worthwhile
addition regardless of how Turso's own specific case turned out — see its doc comment in
`workload.rs` for the full account, including the parts that didn't resolve cleanly.
