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
better evidence, in fact, than any other candidate in ADR-0008 had going in. But hard gate 3
(crash-safety) could not be cleared, and the measured gates and a real ingest-time discovery both
point the same direction: this candidate is not production-ready for Nicti's catalog yet, which is
exactly the risk #102 flagged before any code was written.

## Measured results

**Hard gates:**

| Gate | Result |
|---|---|
| 1. Windows build | ✅ Confirmed via `tursodatabase/turso`'s own `.github/workflows/rust.yml`: a real 3-OS matrix including a Windows runner, running both `cargo build --all-features` and `cargo nextest run --workspace --all-features` there — not just a doc claim. Materially stronger evidence than the pglite-rs/pglite-oxide eliminations in ADR-0008. |
| 2. License | ✅ MIT, confirmed from crates.io's version-level API response for the crate itself (not a secondary source). |
| 3. Crash-safety | ⛔ **Fails, and in a different, arguably worse way than LMDB's ADR-0008 finding.** Using the exact same `crash_mid_ingest` technique that worked cleanly for SQLite/DuckDB (leave a transaction open, never `COMMIT`, `mem::forget` the handle), every reopen attempt on the same path failed with `database is locked` — 20/20 failures at default iteration count, and still 1/1 at a single iteration in isolation, ruling out any cross-iteration state as the cause. A dedicated probe (`spikes/den/examples/lock_probe.rs`, since deleted — throwaway, not shipped) confirmed the lock never clears: 10 retries at 500ms intervals, still locked after 5 full seconds in the same process. Something (most plausibly a background async task the crate's own runtime spawns, given `mem::forget` cannot signal a graceful stop the way real process death would) holds the lock alive for the rest of the process's lifetime once a connection is dropped uncleanly — a related but distinct failure mode from LMDB's deliberate process-wide open-environment guard. |
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
(faceted filter) and ~1.11x (keyword) of the budget; the extrapolation to 2M is not favorable.

**The disqualifying finding, found while attempting the 2M bulk-ingest run:** partway through a
single 2M-row insert transaction — data that represents perhaps 300–500MB in any of the other three
engines — the on-disk WAL file (`den-turso.db-wal`) had already grown past **19GB and was still
climbing near-linearly** (15.4GB → 18.9GB in one ~30-second sampling window). The run was stopped
deliberately rather than let it consume further disk for a result that was already unambiguous;
this is not a query-speed problem, it is severe, apparently unbounded WAL growth during a large
transaction — consistent with real, currently-open upstream issues surfaced during #102's own
research (a checkpoint-can-truncate-the-log issue, a durability-ordering issue, a shared-WAL-frame
monotonicity panic filed only 9 days before this evaluation).

## Options considered

| Option | Verdict |
|---|---|
| Turso Database, pure-Rust rewrite (`turso` crate) | **Rejected.** Passes the license and Windows-build hard gates with real evidence, but fails crash-safety (a lock that never clears in-process), sits at the edge of budget on two query shapes at 600k due to a confirmed-missing optimizer feature, and — the deciding factor — showed severe, apparently unbounded WAL growth mid-ingest at 2M scale that made completing the run not worth the cost. |
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
  `mem::forget`-based crash simulation, for two different underlying reasons. A real
  fork+exec+SIGKILL harness is looking more clearly like required infrastructure for this project's
  future database-adjacent research, not an optional nicety — worth its own ticket if a fifth
  candidate ever needs evaluating this way.

## Spike

`spikes/den/src/turso_engine.rs` — implements the same `Workload` trait as the other three
engines, behind the `turso` Cargo feature (default-off, matching the others). The crate's own API
is `async` (not `rusqlite`-shaped), so every method blocks on a per-engine
`tokio::runtime::Runtime` rather than exposing an async trait. `tests/cross_engine.rs` includes
`turso_matches_shared_workload`, which passes — the correctness of what was implemented was never
in question, only its production-readiness.

One real bug in this spike's own code was found and fixed before any number here was trusted:
`bulk_ingest`'s first version captured the result of the row-insert loop but then attempted
`COMMIT` unconditionally, so a real failure partway through ingest surfaced as a confusing,
unrelated `Transaction error: cannot commit - no transaction is active` instead of the actual
underlying error (which is how the WAL-bloat finding above was actually uncovered, once fixed).
