# ADR-0011: Facet-count cache for SQLite's faceted-filter gap

- **Status:** Accepted
- **Date:** 2026-09-24
- **Ticket:** [#103](https://github.com/jordanfelle/nicti/issues/103) Build: DuckDB-backed facet-count cache for SQLite's faceted-filter gap (or trigger-maintained alternative)

## Context

ADR-0008 chose SQLite as the v1 catalog store but measured one real miss: faceted-filter-with-live-
facet-counts came in at 151ms p95 at 2M synthetic assets (re-measurements ranged 134.5–160ms across
runs), against `docs/benchmarks.md`'s <100ms budget — root-caused to the outer predicate's own
selectivity (`model = ? AND rating >= ?` matches roughly the whole "picks" population), not a
fixable query-shape bug. ADR-0008 named two follow-up mitigations without attempting either: (a) a
trigger-maintained aggregate facet-count table inside SQLite (no new dependency), or (b) a
DuckDB-backed read-side cache for this one query shape (SQLite stays the source of truth for
everything else). #103 is the ticket for building and measuring both, and its own text states the
decision rule up front: **the trigger approach should win by default unless it measurably can't hit
budget or its write-path cost is unacceptable against ADR-0008's existing ≤5ms/≤16ms-under-load
point-update gates.**

Both candidates were explicitly scoped narrow, per #103: not a second general-purpose store, not
"replace SQLite," and not the "DuckDB as an OLAP sidecar" option ADR-0008 already declined when
nothing measured needed it — a materialized/cached facet-count layer serving *only* the
faceted-filter-with-counts query shape.

**Sandbox note, distinct from ADR-0008's own:** this pass ran on the same Linux/WSL2 sandbox as
ADR-0008/0009, but with **other Claude Code sessions concurrently building/testing unrelated
worktrees on the same shared machine** (confirmed via `ps aux` mid-run: parallel `cargo build`/
`cargo check` processes for `nicti-wt-redb` and `nicti-wt-duckdb-primary`, other in-flight spikes
for this same account). This measurably inflated *absolute* latencies across the board relative to
ADR-0008's original numbers — this pass's own plain-SQLite baseline at 2M (re-run in this session
for a same-environment comparison, see below) shows faceted_filter at 513ms p95, ~3.4x slower than
ADR-0008's 151-160ms on presumably-uncontended hardware, and `bulk_ingest` timings in particular
swing by more than 1.5x between runs of the *same* code path in ways only explainable by shared-CPU
contention (see the Measured results' explicit callout). Every within-scale comparison below (plain
SQLite vs. trigger vs. DuckDB-cache) was still run in the same contended environment close together
in time, so the *relative* comparison between candidates stays meaningful; only `bulk_ingest`'s
absolute numbers are unreliable enough to caveat explicitly rather than trust at face value.

## Decision rule (stated before measuring, per ADR-0008/0009's own methodology)

1. Does `faceted_filter` (the exact benchmarked shape: `model = "NIKON Z 8" AND rating >= 3 AND
   keyword` narrowed to one specific leaf, per `gen.rs`'s `BENCH_LEAF_KEYWORD`) clear the <100ms p95
   budget at 2M for each candidate?
2. Does trigger-maintenance / cache-refresh cost added to the write path stay within ADR-0008's
   existing point-update gates (≤5ms per rating write, ≤16ms under a 100-write burst)?
3. Is the trigger-maintained table's/cache's answer actually correct — checked against a
   from-scratch recomputation, not just fast?
4. **Default:** the trigger-maintained SQLite table wins unless (1) fails for it, or its write cost
   from (2) is unacceptable. The DuckDB cache is only adopted if the trigger approach genuinely
   can't clear budget or its write cost is unacceptable against those same gates — not chosen for
   being additionally fast, since it adds a second store and a real staleness/consistency surface
   that the trigger approach doesn't.

## Decision

**Trigger-maintained SQLite facet table.** It clears the 100ms budget by 58-88x at 2M (1.6-1.7ms
p95, vs. this session's own contended-hardware 513ms plain-SQLite baseline, and vs. ADR-0008's
151-160ms on quieter hardware), and every write-path op it touches (`write_rating`, `rate_burst`,
`bulk_ingest`'s keyword inserts) stays enormously inside ADR-0008's ≤5ms/≤16ms gates — the slowest
touched op, `tag_keyword_10k` (not itself a gated op), came in at 23-28ms for a 10,000-row batch,
roughly 2x plain SQLite's 13-14ms/26-40ms and still two orders of magnitude under any per-op budget
that matters. It adds no new dependency and no second store to keep consistent. Per #103's own
stated default, this is a clean, unambiguous win — the DuckDB cache never got a chance to be
"needed," because the trigger approach never came close to failing either of the two things that
would have forced adopting it.

**The DuckDB-backed cache also works, measured honestly, and is kept explicit as the fallback**
(same posture ADR-0008 gave DuckDB as primary-store fallback) if a real schema in #22 finds some
other reason the trigger approach doesn't fit (e.g., a facet dimension too expensive to maintain
incrementally via triggers). Its own real cost — refreshing the cache — is not hidden: 2.8s after a
2M-row bulk ingest, 3.6s after a subsequent 100-row rating burst + 10k-row keyword tag. That's a
real, non-gating (refresh is an explicit, on-demand/periodic action here, not a hot-path op) but
non-trivial cost, and — the more important point — **the cache is provably stale between
refreshes**: a deliberate demonstration (rate a real asset belonging to the exact benchmarked facet,
without calling `refresh()`) shows the cached answer diverging from a from-scratch recomputation
every time, exactly the "adds a dependency and a consistency surface" cost #103 asked to be
quantified rather than asserted.

## Measured results

Backed by `spikes/den/facet_cache_trigger.rs` (candidate 1), `spikes/den/facet_cache_duckdb.rs`
(candidate 2), and a re-run of the existing plain-SQLite `sqlite.rs` in this same session (for a
same-environment baseline, not ADR-0008's original numbers, which ran on different — less
contended — hardware). p50/p95/max over 5 measured runs, 1 discarded warm-up, per
`docs/benchmarks.md`'s methodology, at 600k and 2M synthetic assets (same generator, same seed,
same `ref-10k-manifest.csv` distributions as ADR-0008/0009).

### Faceted filter (the gated query: `model="NIKON Z 8"`, `rating>=3`, keyword narrowed to
`BENCH_LEAF_KEYWORD`) — the actual gate

| Scale | Plain SQLite (this session) | Trigger-maintained | DuckDB cache (post-refresh) |
|---|---|---|---|
| 600k | 39.95 / 42.09 ms | **0.482 / 0.562 ms** ✅ (71-75x) | 1.887 / 1.991 ms ✅ (21-23x) |
| 2M | 411.8 / 513.2 ms ⛔ (budget miss, worse than ADR-0008's own 151-160ms — contended hardware, see Context) | **1.599 / 1.726 ms** ✅ (58x margin vs. this session's own 100ms-scaled gate; ~88-93x vs. ADR-0008's original 2M baseline) | 1.838 / 2.085 ms (pre-refresh-of-this-check) / 1.731 / 1.940 ms (post) ✅ (~48-58x) |

Both candidates clear the 2M budget by a wide enough margin (>3x) that, per ADR-0008's own
reference-machine rule, this doesn't need a less-contended re-run to be treated as final for the
*decision* itself (the contention only inflates the plain-SQLite baseline's absolute number, which
is exactly the problem being fixed either way).

### Write-path cost (the ops each design's maintenance touches), 2M scale

| Op | Plain SQLite (this session) | Trigger-maintained | Budget (ADR-0008) |
|---|---|---|---|
| `write_rating` (single) | 0.018 / 0.078 ms | 0.019 / 0.038 ms | ≤ 5 ms |
| `rate_burst_100` | 0.826 / 2.099 ms | 0.558 / 0.681 ms | ≤ 16 ms (informative in ADR-0008, used here as the "under load" reference) |
| `tag_keyword_10k` (not budget-gated) | 40.47 / 63.78 ms | 23.09 / 26.10 ms | — |
| `bulk_ingest`, 2M rows (informative; see contention caveat) | 214.1 s | 122.3 s | — |

The DuckDB-cache candidate's write path is, by construction, identical to plain SQLite (writes go
straight to `sqlite.rs`'s schema, untouched by the cache) — its own 2M numbers (`write_rating`
0.013/0.018ms, `rate_burst_100` 0.402/0.763ms, `tag_keyword_10k` 10.27/10.32ms) confirm this rather
than needing a separate row here.

**`bulk_ingest`'s numbers are the least trustworthy in this table, called out explicitly rather than
read at face value:** the trigger candidate's 2M `bulk_ingest` (122.3s) measured *faster* than this
session's own plain-SQLite baseline (214.1s) despite the trigger candidate doing strictly more work
per keyword insert (a real trigger firing, vs. a bare `INSERT`) — the only explanation consistent
with everything else observed is that the two runs landed in different phases of this shared
machine's contention from concurrent unrelated builds (confirmed present via `ps aux`, not
inferred), not that triggers make ingest faster. The 600k numbers, captured closer together in time
with (apparently) less contention variance, show the expected direction instead: trigger `34.4s` vs.
plain-SQLite `26.8s` (~1.29x overhead) — consistent with maintaining an extra table on every keyword
insert. Treat the 2M `bulk_ingest` row as **directionally uninformative, not as "triggers are
free at bulk-ingest scale"** — a re-run on quiet hardware is the honest way to get a trustworthy
number here, out of scope for this pass since `bulk_ingest` is explicitly "informative, not gated"
in ADR-0008's own methodology.

### DuckDB cache: refresh cost (the real, quantified "consistency surface" cost)

| Scale | Refresh after 2M-row bulk ingest | Refresh after a 100-row rating burst + 10k-row keyword tag |
|---|---|---|
| 600k | 1.061 s | 1.053 s |
| 2M | 2.781 s | 3.563 s |

This is a full-rebuild refresh (see Options considered below for why, not incremental) — it scales
with catalog size, not with how much actually changed since the last refresh. A UI wiring this in
would need to decide a refresh cadence (on-demand before showing facets, a debounced background
timer, etc.); 2.8-3.6s at 2M is not a hot-path number by any definition in `docs/benchmarks.md`; the
important number is the next one.

### DuckDB cache: correctness and staleness, verified directly (not asserted)

- `facet_correctness_ok_after_ingest`: **true** at both scales — the cache matches a from-scratch
  SQLite recomputation immediately after its first `refresh()`.
- `facet_correctness_ok_while_stale`: **false** at both scales — after a real asset belonging to the
  exact benchmarked facet is rated (pushing it into the matching set) *without* calling `refresh()`,
  the cache's answer for that exact facet query diverges from the from-scratch recomputation, every
  time this was checked. This is the deliberate, concrete demonstration #103 asked for: staleness is
  not a theoretical risk here, it is a reproduced one.
- `facet_correctness_ok_after_refresh`: **true** again once `refresh()` is called after that same
  write — the design works as specified, it's just genuinely stale in the gap between a write and
  the next refresh.

### Trigger-maintained table: correctness, verified directly

- `facet_correctness_ok_after_ingest` / `facet_correctness_ok_after_writes`: **true** at both 600k
  and 2M — checked against `sqlite.rs::naive_faceted_filter` (a from-scratch recomputation using the
  exact same query shape `sqlite.rs`'s own `faceted_filter` uses) both right after bulk ingest and
  again after the write burst (100 rating changes + a 10,000-row keyword tag) that the benchmark
  above measured the cost of.
- `spikes/den/tests/facet_cache.rs` extends this with a dedicated correctness test at a small (10k)
  fixture: multiple `(model, rating)` combinations, not just the one benchmarked, all checked before
  and after a rating burst / keyword tag / single rating write, plus `PRAGMA integrity_check`.

### A real, shared scope limitation found while writing the correctness tests (not glossed over)

Both candidates' facet tables are keyed at `(model, rating, keyword)` grain — one row per distinct
combination, not one row per asset. `SUM(cnt)` over that grain, filtered only by `model`/`rating`
with **no** `keyword_prefix`, does not equal the true distinct-asset count `sqlite.rs`'s own
per-asset query returns: an asset with two keywords is counted twice, an asset with zero keywords
isn't counted at all. This was caught directly by `tests/facet_cache.rs` (an initial version of
that test asserted an unfiltered query *should* match and failed), not discovered by manual
inspection. Both `facet_cache_trigger.rs` and `facet_cache_duckdb.rs`'s module docs, and the test
file itself, now state this explicitly and assert the mismatch as a known, verified gap rather than
silently working around it: **neither candidate is a drop-in replacement for `sqlite.rs`'s
`faceted_filter` in general — both are correct only for the keyword-narrowed shape #103 actually
scopes this to** (which is also the only shape the real UI facet panel and this benchmark exercise).
If a future caller ever needs a facet count *without* a keyword filter, it should keep calling
`sqlite.rs`'s own from-scratch implementation for that shape specifically, not either cache.

### A second real bug found and fixed while writing this pass's own tests

`sqlite.rs`'s existing `faceted_filter` (and the fixed placeholder-slot pattern this candidate's
first draft copied from it) relies on SQLite tolerating an unreferenced `?N` slot when *some*
clauses are appended but not all — true, and already covered by `tests/cross_engine.rs`'s
`model: None` case. It does **not** tolerate the fully-unfiltered case: with every clause omitted,
the SQL text has zero declared placeholders, but a fixed 3-value `params![...]` literal still
supplies three bound values, and `rusqlite` rejects that outright ("Wrong number of parameters
passed to query. Got 1, needed 0"). This was caught by `tests/facet_cache.rs`'s own
`verify_against_naive(None, None, None)` call (checking the unfiltered case, per the scope-limitation
finding above) — fixed in `sqlite.rs::naive_faceted_filter` and `facet_cache_trigger.rs`'s own
`faceted_filter` by building placeholders dynamically, one `?N` per clause actually appended (the
same pattern `duckdb_engine.rs`/`facet_cache_duckdb.rs` already used, for the opposite reason —
DuckDB's binder never tolerated *any* mismatch at all). `sqlite.rs`'s own original, already-merged
`faceted_filter` was left as-is (out of this ticket's scope, and no existing caller exercises its
fully-unfiltered case), but this is worth a maintainer's attention if a future caller ever does.

## Options considered

| Option | Verdict |
|---|---|
| Trigger-maintained SQLite facet table | **Chosen.** Clears the 2M budget by 58-88x, write-path cost negligible against ADR-0008's gates, no new dependency, no second store. Verified correct against a from-scratch recomputation both at ingest and under a write burst. |
| DuckDB-backed read-side cache | Also clears budget with a comparable margin, but adds a second store, a real (if non-gating) refresh cost (2.8-3.6s at 2M), and a demonstrated staleness window between refreshes. Kept explicit as the fallback if a future facet dimension doesn't fit the trigger approach, not adopted now. |
| Full rebuild refresh (DuckDB candidate) | **Chosen for the DuckDB candidate specifically**, over incremental refresh. SQLite's schema has no change-log/watermark column to identify "changed since last refresh" — adding one would give `assets`/`asset_keywords` the same per-write bookkeeping the trigger candidate already does more directly, at which point the trigger candidate is strictly simpler for the same cost. The aggregation itself runs inside SQLite (an indexed join + `GROUP BY`), so only the small aggregated result (bounded by distinct `(model, rating, keyword)` combinations, not row count) crosses into DuckDB — not a full raw-row export every refresh. |
| Do nothing (leave SQLite's plain scan) | Rejected — this is the exact gap #103 exists to close; ADR-0008 already flagged it as the one measured miss. |

## Consequences

- **#22's `CatalogStore`/facet-count implementation should be modeled on
  `spikes/den/src/facet_cache_trigger.rs`**: the same schema (`facet_counts(model, rating, keyword,
  cnt)`, `PRIMARY KEY (model, rating, keyword)`, an index on `(model, keyword)`) and the same three
  maintenance triggers (`asset_keywords` insert/delete, `assets.rating` update), carried forward
  directly rather than rediscovered.
- **The `(model, rating, keyword)`-grain limitation carries forward as a real constraint, not a
  spike-only footnote**: #22's facet-count query surface must always narrow by a specific keyword
  (or keyword prefix) when reading from this table; an unfiltered/no-keyword facet count needs
  either a separate, simpler `(model, rating) -> COUNT(*)` maintained table (a small addition to the
  same trigger set, not attempted in this pass since nothing in #103's own benchmark needed it) or a
  fallback to `sqlite.rs`'s existing from-scratch query for that specific shape.
- **DuckDB as a dependency is still not needed for v1's catalog store**, consistent with ADR-0008's
  own conclusion — the trigger-maintained table closes the one gap ADR-0008 left open, without
  adding DuckDB (or any new crate) to the shipping dependency graph. `docs/licensing.md` needs no
  update from this ADR: both candidates were built entirely from `rusqlite`/`duckdb`, already
  dependencies of `spikes/den` since ADR-0008.
- **If #22 later finds a facet dimension the trigger approach can't maintain cheaply** (e.g., a much
  higher-cardinality dimension than this benchmark's ~11 broad + per-event-leaf keyword
  vocabulary), `facet_cache_duckdb.rs`'s refresh-based design is the proven fallback — already built,
  measured, and correctness-checked, not a cold start.
- **A future pass measuring `bulk_ingest` at 2M scale should do so on quiet hardware**, per this
  ADR's own contention caveat — the 2M `bulk_ingest` numbers above should not be cited as evidence
  that triggers are free at bulk-ingest scale; the 600k numbers (captured with less observed
  contention) are the more trustworthy signal on that specific question (~1.29x overhead).

## Spike: `spikes/den/`

Adds to the `spikes/den/` crate from ADR-0008/0009 (see that ADR's own Spike section for the
generator/workload/cross-engine-test infrastructure this reuses unchanged):

- `src/facet_cache_trigger.rs` — the trigger-maintained SQLite candidate: same schema/pragmas as
  `sqlite.rs` plus a `facet_counts` table and three maintenance triggers, a `faceted_filter`
  override reading that table, and `verify_against_naive` (the correctness oracle).
- `src/facet_cache_duckdb.rs` — the DuckDB-cache candidate: wraps `sqlite.rs::SqliteEngine`
  unmodified for every op except `faceted_filter`, plus `refresh()` (the full-rebuild) and
  `verify_against_naive`.
- `src/sqlite.rs` gained two small additions used by both candidates: `connection()` (a read-only
  escape hatch to the underlying `rusqlite::Connection`) and `naive_faceted_filter` (the from-scratch
  correctness oracle both candidates check against — one implementation, not duplicated per module).
- `src/bin/den.rs` gained `den facet-bench --variant <trigger|duckdb-cache> --catalog <path>
  --out-dir <dir> --runs <n>`, following `den bench`'s existing CLI conventions but with its own
  measurement flow per candidate (refresh timing and staleness demonstration for the DuckDB
  candidate; correctness checks folded into both).
- `tests/facet_cache.rs` — correctness tests at a 10k fixture (same pattern as
  `tests/cross_engine.rs`): both candidates checked against `naive_faceted_filter` before/after
  writes, the DuckDB candidate's pre-refresh error path and demonstrated staleness, and the shared
  `(model, rating, keyword)`-grain limitation asserted directly for both.
- Two real bugs found and fixed while building this, beyond the shared scope-limitation finding
  above: a borrow-checker error in the DuckDB refresh query (a `rusqlite`-style `Statement` temporary
  outliving its intended scope — trivial, caught by the compiler, not a logic bug) and the
  all-`None`-filters placeholder-count mismatch described above (a real logic bug, caught by a test,
  not the compiler).
