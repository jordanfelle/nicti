# ADR-0012: Reconsidering DuckDB as the v1 primary catalog store

- **Status:** Accepted
- **Date:** 2026-09-24
- **Ticket:** [#107](https://github.com/jordanfelle/nicti/issues/107) Requirements: reconsider DuckDB as the v1 primary catalog store (not just a sidecar)

## Context

ADR-0008 chose SQLite for v1 despite DuckDB passing every measured gate at 2M assets with the best
margins of any candidate, including the point-update gate the issue's own framing predicted it
would fail. The one stated reason to keep SQLite anyway — "#22's row-store schema fits SQLite's
maturity better" — was explicit about being a soft, ecosystem-familiarity argument, not a
demonstrated technical mismatch, and ADR-0008's own Consequences section named "revisiting DuckDB
as primary" as a live option it didn't pursue. #107 asks the question directly: is that soft
argument actually right, or does it not survive a real look at DuckDB's data model against #22's
planned schema (ADR-0002)?

**Already settled, not re-litigated here** (per #107's own text): the "100-write rate burst" number
in ADR-0008 (DuckDB 101.6/104.7ms vs SQLite's 0.35/0.42ms) is not a real weak point — it clears
`docs/benchmarks.md`'s actual per-keystroke Culling budget (`keypress -> next image displayed <
50ms`) by ~50x once expressed as a per-write cost (~1ms/write). This ADR does not revisit that
number. Also out of scope: #103's separate, narrower SQLite-trigger-vs-DuckDB-sidecar facet-cache
work, running concurrently in its own branch — not touched or duplicated here.

**What was actually open:** ADR-0002 commits #22 to three concrete schema properties ADR-0008's
`Workload` trait (filter/sort/range/point-update queries on a flat `assets` table) never
exercised at all:

1. A JSON-ish per-stage edit-parameter map (`EditDocument::stages: BTreeMap<String, StageEntry>`,
   `StageEntry.params: serde_json::Value`).
2. An append-only history log that **compacts**: a burst of raw slider ticks lands as one row per
   tick, then most of the burst is deleted and replaced by a single merged row spanning it — a
   genuinely UPDATE/DELETE-heavy pattern, not a pure-insert stream, and one #107 explicitly flagged
   as *possibly* better suited to a columnar engine than to a row-store (which would have
   undercut, not confirmed, ADR-0008's reasoning).
3. Reconstructing "the current effective edit stack for asset X" — the latest row per
   `(asset_id, stage_id)` — cheaply, not via a linear scan of the whole history table.

None of this was answerable from documentation alone (DuckDB's JSON extension exists but its
extraction operations weren't previously exercised anywhere in this repo, and neither engine's
compaction cost under a real burst-and-compact history pattern had ever been measured), so #107's
exit criteria required an actual prototype of this shape against both engines — not an assertion
either way.

## Decision rule (stated before measuring)

Build a rough approximation of the real #22 shape — a JSON `params` column plus a `history` table
with append + burst-compaction + "current effective stack" reconstruction — against both SQLite
and DuckDB (LMDB is out of scope: ADR-0008 already rejected it as primary on the crash-safety gate,
independent of this schema-fit question). Measure, don't assume:

- Does DuckDB have real JSON query operations (path extraction), not just opaque-string storage?
- Insert throughput for the raw-tick stream.
- Compaction cost for the UPDATE+DELETE pattern ADR-0002's burst-compaction actually requires.
- Query cost and SQL-dialect complexity for "current effective edit stack for asset X."

DuckDB becomes the v1 default only if this schema-fit question comes out neutral-or-favorable for
it, given it already leads on every other measured gate from ADR-0008. A decisive negative result
on any of these — not a soft preference — is what would keep SQLite chosen for a real, non-soft
reason.

## Decision

**SQLite stays the v1 catalog store. ADR-0008 is unchanged, now for a demonstrated technical
reason instead of a soft one.** DuckDB's JSON support is real and unremarkable to use (see below),
but its history-log compaction cost is not a close call: DuckDB compacted the same runs SQLite
compacted **~80x slower per operation** (4.297ms/op vs 0.054ms/op), turning a ~1-second operation
into a ~86-second one at the same row count. This is not the "100-write rate burst" number #107
already ruled out as non-gating — that number describes a bulk, batched write path; this one
describes ADR-0002's actual, frequent, per-burst-completion compaction step, run 19,963 times as
19,963 separate single-op transactions, one per genuinely completed, contiguous slider-drag/
history-run (an earlier draft of this ADR miscounted this — see the correction note in Consequences
below). The measured per-op cost lines up cleanly with ADR-0008's own already-measured
single-row `write_rating` transaction-commit overhead on DuckDB (1.7–2.3ms), doubled for
compaction's UPDATE+DELETE pair — a real, well-understood technical mismatch with ADR-0002's
"no optimize catalog" constraint (bounded, continuous maintenance, not a periodic compaction pass
the user has to run), not a benchmark artifact, even though it is a substantially smaller effect
than an earlier draft of this ADR reported.

## Measured results

Backed by `spikes/den/src/schema_fit.rs` (new spike module, see below). Workload: 4,000 synthetic
assets × 6 editing sessions × 40 raw slider ticks per session (one randomly chosen stage per
burst, from `{white_balance, tone, mask_subject, mask_sky, crop}`) = 960,000 raw history rows,
generated deterministically (`generate_bursts`, seed 7). Compaction collapses each *contiguous run
of consecutive-`seq` ticks* sharing `(asset_id, stage_id, control)` down to a single merged row,
exactly per ADR-0002's rule (first tick's `before`, last tick's `after`) — critically, a stage
revisited in a later, non-adjacent burst is its own separate run, not folded into an earlier one —
19,963 runs actually needed compaction (a run of exactly 1 tick has nothing to compact). All
numbers below are `cargo test --release` measurements; an earlier draft of this ADR captured debug
(unoptimized) numbers for SQLite without saying so, which is corrected here too (see Consequences).

| Measurement | SQLite | DuckDB |
|---|---|---|
| Raw insert, 960k rows (Appender/prepared-statement bulk path) | 0.81s (1181.8 rows/ms) | 1.05s (915.7 rows/ms) |
| Compaction, 19,963 ops (one UPDATE + one bulk DELETE + commit per op) | 1.08s total, **0.054ms/op** | 85.79s total, **4.297ms/op** |
| Rows remaining after compaction | 19,963 (from 960,000) | 19,963 (from 960,000) — same logical result, confirming compaction is correct on both engines |
| "Current effective edit stack" query, 200 calls | 3.27ms total, 0.016ms/call | 200.8ms total, 1.004ms/call |
| JSON path extraction (`json_extract(after, '$.v')`) | Works, matches DuckDB's extracted value exactly | Works (`json_extract(...)::DOUBLE`), matches SQLite's extracted value exactly |

**Raw insert (~1.3x slower on DuckDB) is not the finding — it's close and not gated by anything in
`docs/benchmarks.md`.** The finding is compaction: **~80x slower per operation**, and the total
86-second wall time for 19,963 ops at this modest scale (4,000 assets — a fraction of the
2M-asset planning horizon) means a real catalog's worth of compaction, run continuously as
ADR-0002's design calls for, is not a one-off cost DuckDB pays once — it is DuckDB's steady-state
cost for a workload SQLite handles roughly two orders of magnitude faster. The "current effective
edit stack" query is also markedly slower on DuckDB (~61x), though at an absolute ~1ms/call it
would likely still be usable in isolation if compaction weren't the disqualifying factor first.

**Root cause, not just observed:** each `compact_run` call does one `SELECT` to fetch the run's
rows, one `UPDATE`, one bulk `DELETE`, and one transaction commit — the same per-transaction-commit
overhead ADR-0008 already measured on DuckDB's single-row `write_rating` (1.7–2.3ms vs SQLite's
0.012–0.04ms). The measured 4.297ms/op average lines up cleanly with that number roughly doubled
(one UPDATE + one DELETE instead of `write_rating`'s single UPDATE) — a well-understood
transaction-commit-overhead effect, not a mysterious or open one. This spike did not need to reach
for a degrading-storage explanation to account for the measured cost, unlike an earlier draft of
this ADR, which overstated the magnitude and reached for one anyway.

**JSON support is real, not just storage — this part actually favors DuckDB, it just isn't enough
to change the Decision.** `json_extract(json_column, '$.path')` works out of the box on DuckDB
1.10505.0's bundled build (JSON is one of the "core" extensions statically linked into the bundled
binary, not something requiring `INSTALL json; LOAD json;` against network access) and produces
the identical extracted value SQLite's own JSON1 extension does for the same input — proven
directly by `json_extraction_works_on_both_engines`, not asserted from either engine's
documentation. If ADR-0002's edit-parameter JSON blob needed genuine path-based indexing/filtering
inside the catalog (e.g., "find every asset with `crop.aspect_ratio = 16:9`"), DuckDB's JSON
support is at least as capable as SQLite's JSON1 for that specific need. This finding doesn't move
the Decision because the disqualifying cost is in the history table's mutation pattern, not the
edit-document's JSON storage.

**QUALIFY vs. window-function-in-a-subquery: no real dialect-complexity difference.** DuckDB's
`QUALIFY ROW_NUMBER() OVER (...) = 1` clause is marginally more concise than SQLite's equivalent
(a window function computed in a subquery, filtered by an outer `WHERE rn = 1`, since SQLite has no
`QUALIFY`), but both are ordinary, well-understood SQL — "excessive complexity" was not a real
concern on either side.

## Options considered

| Option | Verdict |
|---|---|
| SQLite (status quo, ADR-0008) | **Kept.** Same tradeoffs ADR-0008 already measured, now additionally confirmed as the correct choice for ADR-0002's specific history-log/compaction shape — not merely preferred by "ecosystem maturity." |
| DuckDB as v1 primary | **Rejected**, on new evidence this pass specifically went looking for. Its JSON support is genuinely good and its OLTP-shaped point-update numbers (ADR-0008) remain the best of any candidate in isolation, but ADR-0002's compaction step — an UPDATE+DELETE-heavy, continuously-recurring operation, not a rare one — costs ~80x more per operation than on SQLite, a real per-transaction-commit-overhead effect (see Root cause above), not a benchmark artifact. |

## Prior art

None beyond this repo's own ADR-0008/0009 — this is a targeted follow-up measurement on a schema
shape those ADRs didn't test, not a new engine comparison.

## Consequences

- **ADR-0008's Decision is unchanged**, and its "kept explicit as the fallback" framing for DuckDB
  should be read narrowly from here forward: DuckDB remains a candidate worth revisiting for a
  pure filter/sort/search read path (exactly ADR-0008's own OLAP-sidecar framing), but **not** for
  the catalog's history log — this ADR is new, first-class evidence against that specific use,
  not merely an unresolved caveat.
- **#22's `CatalogStore` implementation should not consider DuckDB for the history table under
  ADR-0002's compaction design**, even if a future need arises to reconsider the primary store for
  other reasons. If ADR-0002's compaction strategy itself changes (e.g., a batched/deferred
  compaction sweep instead of one-op-per-completed-burst), this finding should be re-measured
  against the new access pattern rather than assumed to still apply unchanged.
- **#103's SQLite-trigger-vs-DuckDB-sidecar facet-cache work is unaffected and still needed** —
  this ADR does not recommend switching primaries, so the facet-query ceiling ADR-0008 already
  identified (faceted-filter-with-counts at 2M, ~1.3–1.6x the budget) is not mooted by anything
  here. #103 should proceed independently of this ADR's outcome.
- **DuckDB's JSON support is confirmed usable** should #22 or a later ticket ever want a DuckDB-backed
  read-side sidecar that needs to query into the edit-document JSON (e.g., a facet/search index
  keyed on specific stage parameters) — this was previously unverified, now it's measured.
- **Correction (adversarial review, before this ADR was finalized):** an earlier draft's
  `compact_run` grouped every row ever touching a given `(asset_id, stage_id, control)` key,
  including rows from separate, non-adjacent bursts on the same stage — collapsing multiple
  unrelated editing sessions into one oversized op instead of one op per completed burst, and
  under-counting the true number of compaction ops (14,733 vs the corrected 19,963). The same
  draft's SQLite numbers were also captured without `--release`, understating SQLite's real speed
  by ~3-4x. Both are fixed in `compact_run` (now splits on contiguous `seq` runs) and in how these
  numbers were captured; the qualitative conclusion (SQLite wins, DuckDB unsuitable for this
  workload) is unchanged, but the magnitude is: ~80x slower per op, not ~575x, and ~86 seconds of
  wall time at this scale, not 18 minutes.

## Spike: `spikes/den/src/schema_fit.rs`

Not production code — same caveat as every other `spikes/den` module (see `CLAUDE.md`'s package
map). Adds, gated on both the `sqlite` and `duckdb` features already default-on in this crate (no
new dependency, no `docs/licensing.md` change needed):

- `generate_bursts` / `compact`: a deterministic burst-and-compact history generator and reference
  compaction function, modeling ADR-0002's "coalescing `control` key" rule directly (consecutive
  same-`(stage_id, control)` ticks merge into one row spanning the run).
- `duckdb_fit::Fit` / `sqlite_fit::Fit`: a `history` table (JSON `before`/`after` columns on
  DuckDB, TEXT on SQLite — SQLite's JSON1 operates on TEXT, it has no distinct JSON storage type)
  plus `insert_raw`, `compact_run`, `current_effective_stack`, and a JSON-extraction sanity check
  on each engine.
- `#[cfg(test)]` tests: `json_extraction_works_on_both_engines` (cross-engine JSON-value
  agreement), `burst_insert_and_compact_duckdb` / `_sqlite` (the actual measured numbers above,
  printed via `--nocapture` and captured here). Scale (4,000 assets × 6 bursts × 40 ticks =
  960,000 raw rows) was chosen to be large enough that a real per-op cost problem shows up as a
  meaningful wall-clock number rather than getting lost in fixed per-test overhead, while staying
  well under `spikes/den`'s main 2M-asset benchmark scale, since this measures one query/mutation
  shape in isolation rather than a full catalog.

Two implementation notes worth recording:

- `compact_run` splits a `(asset_id, stage_id, control)` key's rows into maximal contiguous runs
  of consecutive `seq` values before compacting each run separately — a stage revisited in a
  later, non-adjacent burst is correctly treated as its own run, not folded into an earlier one.
  (An earlier draft of this function grouped by key alone with no adjacency check; see the
  Consequences correction note above.)
- This prototype still checks compaction eagerly per `(asset_id, stage_id)` pair after all bursts
  have landed, rather than the real design's likely trigger (a time-window-based coalescing check
  running inline as ticks arrive). This doesn't change the measured per-operation cost (the same
  UPDATE+DELETE+commit shape either way), but a real implementation's exact compaction cadence is
  #22's decision, not reproduced here.
