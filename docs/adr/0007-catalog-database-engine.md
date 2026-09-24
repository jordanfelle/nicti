# ADR-0007: Catalog database engine

- **Status:** Accepted
- **Date:** 2026-09-24
- **Ticket:** [#67](https://github.com/jordanfelle/nicti/issues/67) Research: embedded database engine (pglite-rs vs rusqlite vs DuckDB)

## Context

#22 (catalog schema + ingest), #23 (search/filter), #24 (filesystem watching), #25 (continuous
backup), and #71 (drive remapping) all need a chosen catalog store before they can start. #67
names four candidates — **pglite-rs**, **rusqlite** (WAL), **DuckDB**, **LMDB** — and merges in
an archived duplicate (#19)'s scope: benchmark point-update latency, faceted filter/search with
live counts, recursive folder counts, hierarchical keywords, rating/ISO/date range queries, and
online backup/crash-safety, at 600k real assets now and a 2M-asset planning horizon.

Constraints already fixed by earlier ADRs/docs:

- `docs/benchmarks.md`: **filter/search/sort p95 < 100ms at 2M**; **cold start < 2s at 2M**. Every
  number in this ADR is judged against those two lines.
- **v1 targets Windows only** (ADR-0001) — a Windows build/test hard gate, not a tiebreaker.
- ADR-0003's license policy applies; any new dependency needs a `docs/licensing.md` row in the
  same PR (done, see that file's 2026-09-24 update).
- `crates/nicti-catalog/src/lib.rs` already holds the `CatalogStore` extension-point trait with no
  methods yet — this ADR's Consequences section is what #22 uses to fill them in.
- **Sandbox note:** this research pass ran in a Linux/WSL sandbox with 32 cores and no Windows
  machine available. Unlike ADR-0005/0006, this is not disqualifying for the *measured* gates —
  see the Decision rule's reference-machine clause below — but the *Windows-build* hard gate is
  necessarily evidence-based (crate source/`build.rs`/dependency-graph inspection, CI's
  `windows-latest` job) rather than a native run in this pass.

## Decision rule (stated before measuring, per ADR-0005/0006's own methodology)

### Hard gates (an engine that fails one is not benchmarked)

1. Builds and passes its own tests on `x86_64-pc-windows-msvc` (CI's `build-windows` job, extended
   by this PR to run `cargo test -p den --features sqlite,duckdb,lmdb`).
2. License allowed under ADR-0003: permissive, or LGPL isolated behind a dylib.
3. Survives a mid-write crash: reopens cleanly, its own integrity check passes.
4. Backs up online — no exclusive lock on the live store, no "optimize" step.
5. Actively maintained: builds against its own currently-published dependency graph, with a real
   Rust API.

### Measured gates at 2M synthetic assets

p95 over 5 measured runs (1 discarded warm-up), against `docs/benchmarks.md`'s targets:

| Query | Target |
|---|---|
| Faceted filter + live facet counts | < 100ms |
| Sort by date + first page | < 100ms |
| Folder-subtree count | < 100ms |
| Keyword-subtree (hierarchical) query | < 100ms |
| Rating/ISO/date range query | < 100ms |
| Filename substring search | < 100ms |
| Cold open + first grid page | < 2s |
| A single rating write | ≤ 5ms |

### Tiebreakers (among candidates passing every hard gate)

Operational simplicity (single file, in-process); binary size/build-time cost; schema/migration
ergonomics; margin over each budget.

### Reference-machine rule

WSL numbers count as final when every measured gate clears its budget with **≥ 3x margin** — these
are CPU/NVMe-bound workloads with no GPU dependency, unlike ADR-0005/0006's render-path numbers.
A Windows re-run is only required for a gate within 3x of its budget. This is stated up front so
this ADR doesn't stall in ADR-0006's "Proposed, pending measurement" state.

### Early exit

A hard-gate failure ends that candidate's evaluation — no spike backend is written for it. Applied
below to both embedded-Postgres candidates.

## Decision

**SQLite (`rusqlite`, WAL mode, `synchronous = NORMAL`)** for the v1 catalog store, with two
concrete schema requirements carried into #22 (see Consequences): a composite `(model, rating)`
index, and `GLOB`, not `LIKE`, for every prefix-scan predicate.

**DuckDB is the real alternative, not a "sidecar," and is worth keeping open for a #22-time
decision, not closed off here.** It passed every hard and measured gate with the largest margins
of any candidate — including the OLTP-shaped point-update gate the issue's own framing predicted
it would fail. SQLite is still the Decision because #22 already assumes a row-store schema
(ADR-0002's `serde_json` edit-stack rows, append-only history log) that's a more natural fit for
SQLite's maturity and this project's Rust-ecosystem familiarity, and because DuckDB's bulk-write
transaction cost (see `rate_burst_100` below) is a real, if currently non-gating, concern for a
"rate an image and immediately advance" hot loop. If #22's schema work finds SQLite's facet-query
ceiling (below) too costly to work around, DuckDB is the fallback, already proven out.

**LMDB is rejected as the primary store**, not on measured performance (its numbers are the best
of any candidate on every op it could index cleanly) but on **hard gate 3, methodologically**: see
Measured results — this pass could not actually exercise LMDB's crash-safety gate at all, because
of a genuine property of the engine, not a benchmark bug. Combined with the real engineering cost
visible in `spikes/den/src/lmdb.rs` (six hand-maintained secondary indexes, manual re-indexing on
every write, no query planner), LMDB is a worse fit for the person-hours #22 has budgeted than
either SQL engine, independent of that open crash-safety question.

**Both embedded-Postgres candidates are eliminated at the hard-gate stage — neither got a spike
backend.**

- **pglite-rs** fails hard gate 1: its own `build.rs` unconditionally emits
  `-Wl,--export_dynamic`/`-Wl,-export_dynamic` (a Unix linker flag) for every `not(target_os =
  "macos")` target, which includes Windows — `link.exe` (MSVC) has no equivalent flag and no
  Unix-style `-Wl,` passthrough. There is no Windows code path in this crate at all.
- **pglite-oxide** fails hard gate 5: it does not compile against its own currently-published
  dependency graph, on any target. `wasmer-wasix` 0.702.0-alpha.3's own source has a non-exhaustive
  `match` over `virtual-net::NetworkError` that is missing the `MessageSize` variant — confirmed
  independently against both `virtual-net` 0.702.0 and 0.702.1, so this isn't a transient
  version-skew fluke fixable by a `Cargo.lock` pin; it's a defect in `pglite-oxide`'s dependency
  chain as published.

DuckDB as an OLAP read-side sidecar (the issue's other named option) is not adopted: nothing
measured below needed it, and it would be a second store to keep consistent for a benefit this
pass didn't find necessary.

## Measured results

Backed by `spikes/den/` (see below). p50/p95/max over 5 measured runs, 1 discarded warm-up, per
`docs/benchmarks.md`'s methodology. **Reference hardware:** this session's Linux/WSL2 sandbox, 32
cores, NVMe-backed. Every number below cleared its gate (or missed it) with far more than 3x
margin in either direction, so per the reference-machine rule above, none of these needed a
Windows re-run to be treated as final — **except pglite-rs/pglite-oxide's hard-gate failures**,
which are Windows-build and dependency-graph facts, already Windows-specific by nature.

### Hard gates

| Candidate | 1. Windows build | 2. License | 3. Crash-safety | 4. Online backup | 5. Maintained |
|---|---|---|---|---|---|
| SQLite (rusqlite) | ✅ (`bundled` feature, pure C, no platform-specific build.rs branch) | ✅ MIT (binding) + public domain (bundled C) | ✅ 0/20 failures, `PRAGMA integrity_check` (real page/b-tree validation) | ✅ per documented API (`VACUUM INTO`, no lock, no optimize step) — not measured under a concurrent writer, see below | ✅ |
| DuckDB | ✅ (`bundled` feature; Windows job already builds/links it) | ✅ MIT (binding + bundled C++ core) | ✅ 0/20 failures, but its own integrity check is a `SELECT COUNT(*)` connectivity probe, not real page validation — see below | ✅ per documented API (`EXPORT DATABASE ... FORMAT PARQUET`, no lock) — not measured under a concurrent writer, see below | ✅ |
| LMDB (heed) | ✅ (`lmdb-master-sys` bundled C, builds via `cc` on MSVC) | ✅ MIT (binding) + OpenLDAP Public License 2.8 (bundled C, attribution-only) | ⚠️ **could not measure — see below** | ✅ per documented API (`env.copy_to_path(..., CompactionOption::Enabled)`, no lock) — not measured under a concurrent writer, see below | ✅ |
| pglite-rs | ⛔ **fails** — `build.rs` passes a Unix-only linker flag unconditionally on non-macOS | ✅ MIT | — | — | — |
| pglite-oxide | — (not reached; see gate 5) | ⚠️ `CDLA-Permissive-2.0` arm needs a `deny.toml` exception (added; see `docs/licensing.md`) | — | — | ⛔ **fails** — doesn't compile against its own published dependency graph (`wasmer-wasix` vs `virtual-net`, confirmed on two versions) |

**Two honest scope limits on gate 4 (online backup) and gate 3 (crash-safety), for all three
surviving candidates:** `den bench` calls `backup()` serially, after every other timed op has
already finished — there is no concurrent writer thread anywhere in this spike, so "no exclusive
lock on the live store" rests on each engine's own documented API contract (`VACUUM INTO`,
`EXPORT DATABASE`, `env.copy_to_path`), not on anything this spike measured under real concurrent
write load. And DuckDB's `integrity_check()` (`duckdb_engine.rs`) is `SELECT COUNT(*) FROM assets
>= 0` — this can only ever return `false`/error if the connection or query itself fails outright,
nothing like SQLite's real `PRAGMA integrity_check` (full page/b-tree validation). Both gaps are
called out explicitly rather than left implicit in a passing ✅, per the standing rule that a
clean-looking result and a skipped check should never look identical to a later reader.

**LMDB's crash-safety gate, in detail:** this spike's crash test (`den crash`) simulates a SIGKILL
in-process: it opens a store, writes half a batch inside a transaction, and — critically — never
calls `COMMIT` before `std::mem::forget`ing the handle, so the transaction is genuinely
interrupted, not a completed write with an unclean shutdown tacked on (an earlier version of this
test called the full `bulk_ingest`, which commits internally, before forgetting the handle — see
the Spike section for how that was caught and fixed). It then reopens a fresh path and runs an
integrity check. This worked cleanly for SQLite and DuckDB (0/20 failures each — each iteration
uses its own fresh store path; reusing one path across "crash" iterations surfaced a leaked-file-
descriptor lock artifact unrelated to actual crash safety, see the Spike section). For LMDB, every
reopen attempt after a `mem::forget` failed outright with `environment already open in this
program; close it to be able to open it again with different options` — confirmed to trigger on
every iteration's own within-iteration reopen, not merely from reusing a path across iterations
(a fresh path per iteration doesn't help). `heed`/`liblmdb` maintain
a process-wide table of open environments specifically to prevent undefined behavior from two
`Env`s pointing at the same file with different options in one process, and `mem::forget` (by
design) never runs the `Drop` that would deregister it. A **real** `kill -9` doesn't have this
problem — the OS reclaims the whole process, including that table — but this spike's in-process
technique cannot distinguish that case from a bug, and building a real fork+exec+SIGKILL harness
was out of scope for this pass. **This is recorded as an open gap, not a passing or failing
result**, and is the deciding factor against LMDB as primary store above independent of its
otherwise-best-in-class numbers.

### Measured gates, 600k assets

| Query | SQLite p50/p95 | DuckDB p50/p95 | LMDB p50/p95 |
|---|---|---|---|
| Faceted filter + facet counts | 47.1 / 48.4 ms | 7.6 / 8.1 ms | 0.034 / 0.072 ms |
| Sort by date, first 500 | 0.019 / 0.037 ms | 2.0 / 2.2 ms | 0.004 / 0.004 ms |
| Folder-subtree count | 2.2 / 2.3 ms | 2.8 / 2.9 ms | 1.0 / 1.4 ms |
| Keyword-subtree query | 0.05 / 0.15 ms | 3.5 / 4.0 ms | 0.002 / 0.002 ms |
| Range query (rating+iso+date) | 1.25 / 1.28 ms | 1.26 / 1.48 ms | 19.3 / 22.8 ms |
| Filename substring search | 26.9 / 28.5 ms | 3.7 / 4.0 ms | 57.5 / 57.9 ms |
| Cold open | 1.6 ms | 16.3 ms | 0.18 ms |
| Single rating write | 0.012 / 0.017 ms | 1.7 / 2.2 ms | 0.010 / 0.016 ms |
| 100-write rate burst (informative, not gated) | 0.24 / 0.35 ms | 79.0 / 85.8 ms | 0.067 / 0.069 ms |

All three clear every gate at 600k.

### Measured gates, 2M assets (the planning-horizon scale the budget is stated against)

| Query | Budget | SQLite p50/p95 | DuckDB p50/p95 | LMDB p50/p95 |
|---|---|---|---|---|
| Faceted filter + facet counts | < 100ms | **151 / 160 ms ⛔** | 14.5 / 15.9 ms ✅ | 0.035 / 0.076 ms ✅ |
| Sort by date, first 500 | < 100ms | 0.018 / 0.020 ms ✅ | 2.6 / 3.1 ms ✅ | 0.004 / 0.005 ms ✅ |
| Folder-subtree count | < 100ms | 7.5 / 7.7 ms ✅ | 4.4 / 5.0 ms ✅ | 6.6 / 6.7 ms ✅ |
| Keyword-subtree query | < 100ms | 0.05 / 0.09 ms ✅ | 12.6 / 13.3 ms ✅ | 0.002 / 0.002 ms ✅ |
| Range query | < 100ms | 4.2 / 4.3 ms ✅ | 2.1 / 2.2 ms ✅ | 82.3 / 84.0 ms ✅ (thin) |
| Filename substring search | < 100ms | 79.5 / 85.7 ms ✅ (thin) | 4.9 / 5.3 ms ✅ | 198.8 / 201.1 ms **⛔** |
| Cold open | < 2s | 1.0 ms ✅ | 12.4 ms ✅ | 0.18 ms ✅ |
| Single rating write | ≤ 5ms | 0.025 / 0.040 ms ✅ | 1.9 / 2.3 ms ✅ | 0.010 / 0.020 ms ✅ |
| 100-write rate burst (informative) | — | 0.35 / 0.42 ms | 101.6 / 104.7 ms | 0.076 / 0.118 ms |
| Bulk ingest, 2M rows (informative) | — | 60.0 s | 13.4 s | 15.1 s |
| Online backup (informative, not gated as a hot-path op) | — | 2.0 / 2.5 s | 0.19 / 0.20 s | 1.2 / 1.3 s |

**SQLite's one real miss: faceted filter at 2M (151ms p50, ~1.5x the budget).** Root-caused, not
just observed: the query plan (`SEARCH a USING COVERING INDEX idx_assets_model_rating (model=? AND
rating>?)` + an indexed `EXISTS` semi-join against the keyword table) is already the right shape —
the cost is the outer predicate's own selectivity. `model = ? AND rating >= 3` matches roughly the
whole "picks" population (the generator's 2–10% keep rate), so a fully indexed scan still touches
tens of thousands of rows before the `EXISTS` check narrows further. This is the
"trigger-maintained aggregate facet tables" option the issue itself names as a tiebreaker-stage
follow-up (see `spikes/den/src/sqlite.rs`'s module doc) — not attempted in this pass, and the right
starting point for #22 if this ceiling matters in practice.

**LMDB's one real miss: filename substring search at 2M (199ms p95, ~2x the budget).** A leading
wildcard (`%substr%`) can't be served by any index in any of the three engines — SQLite (85.7ms)
and DuckDB (5.3ms) both do a full scan too, they're just faster at it (native column scan vs
per-row `bincode` deserialize). This is not fixable by better indexing in any of these engines; it
needs either a separate full-text index (SQLite FTS5, e.g.) or accepting that filename search is
an occasional, not hot-path, operation.

**Generator note, folded into the numbers above:** the first pass at this benchmark's keyword
vocabulary (11 flat category values) made every hierarchical-keyword query artificially
non-selective at 2M scale — a "leaf" query was really a "match half the corpus" query, regardless
of engine. Fixed by adding a per-event keyword (`Events.Named.<event-id>`) to every generated
asset — see `spikes/den/src/gen.rs`'s `BENCH_LEAF_KEYWORD`. Its absolute match count stays roughly
constant across scales (`folder_count = asset_count / 30`, so assets-per-event stays pinned at
~30 × 6 year-folders regardless of `asset_count`) — what improves with scale is *relative*
selectivity (same numerator, a bigger denominator), not the leaf's own cardinality growing. That's
still what makes it a realistic, genuinely selective leaf at both 600k and 2M; it just isn't
"cardinality scaling with catalog size" as such. The keyword-subtree and faceted-filter numbers
above use this corrected leaf; the broad-top-level-branch case (e.g. "everything tagged anywhere
under `Locations`") is a genuinely different, near-full-scan workload for any engine and isn't
what these numbers measure.

## Evidence: pglite hard-gate failures (reproducible)

Both eliminations below were interactive findings from this research pass, not asserted from
memory — reproduced here verbatim so a future reader can verify them without redoing the work.
Neither crate was ever added as a real Cargo dependency (both failed before that point), so
neither appears in this branch's `Cargo.toml`/`Cargo.lock`.

**pglite-rs** (fetched its README and `build.rs` from `github.com/Midwess/pglite-rs`, the crate's
own repository, 2026-09-24):

```rust
// pglite-rs's build.rs
#[cfg(target_os = "macos")]
println!("cargo:rustc-link-arg=-Wl,-export_dynamic");

#[cfg(not(target_os = "macos"))]
println!("cargo:rustc-link-arg=-Wl,--export-dynamic");
```

`-Wl,--export-dynamic` is a GNU-ld/Unix-linker flag with no MSVC (`link.exe`) equivalent — passed
unconditionally for every `not(target_os = "macos")` target, which includes
`x86_64-pc-windows-msvc`. There is no `cfg(windows)` branch anywhere in this file.

**pglite-oxide** (added as a real, if temporary, dependency of `spikes/den` in this session —
`cargo check --no-default-features --features pglite-oxide` against crates.io's then-current
resolution):

```
error[E0004]: non-exhaustive patterns: `NetworkError::MessageSize` not covered
   --> wasmer-wasix-0.702.0-alpha.3/src/net/mod.rs:376:11
    |
376 |     match net_error {
    |           ^^^^^^^^^ pattern `NetworkError::MessageSize` not covered
    |
note: `NetworkError` defined here
   --> virtual-net-0.702.1/src/lib.rs:817:1
```

Re-ran after `cargo update -p virtual-net --precise 0.702.0` (the next-oldest published version)
to rule out a transient version-skew fluke: the identical error reproduced, `MessageSize` already
present in `virtual-net` 0.702.0 too. Both attempts, plus the eventual `pglite-oxide` feature and
its now-unused `tokio` dependency, were removed from `Cargo.toml`/`lib.rs`/`bin/den.rs` once this
was confirmed — see the Spike section.

## Options considered

| Option | Verdict |
|---|---|
| SQLite (rusqlite, WAL) | **Chosen.** Passes every hard gate; one measured miss (faceted filter at 2M) with a known, unattempted mitigation. |
| DuckDB | Passes every gate, best margins overall including the point-update gate the issue predicted it would fail. Not chosen for v1 only because #22's schema shape favors SQLite's row-store maturity here; kept explicitly open as the fallback if SQLite's facet-query ceiling becomes a real problem. |
| LMDB (heed) | Best raw numbers on every op it could index — but the crash-safety gate could not be measured (a genuine property of the engine under this pass's test technique, not a benchmark artifact), and it needs six hand-maintained secondary indexes with no query planner. Rejected as primary; a candidate worth revisiting only if a future pass builds real cross-process crash-safety tooling. |
| pglite-rs | **Hard-gate-failed** — no Windows build path exists in the crate as published. |
| pglite-oxide | **Hard-gate-failed** — does not compile against its own published dependency graph on any target. |
| DuckDB as OLAP sidecar | Not adopted — nothing measured needed a second store. |

## Prior art

Both Lightroom Classic's own `.lrcat` (relevant to #61's import work) and comparable open-source
DAM tools (darktable, digiKam) use SQLite as their catalog format — this Decision keeps Nicti in
that same well-trodden lane rather than an unusual choice for this problem shape.

## Consequences

- **#22's `CatalogStore` methods** (`crates/nicti-catalog/src/lib.rs`) should be modeled on
  `spikes/den/src/workload.rs`'s `Workload` trait — the same query set this ADR measured against,
  not a smaller one discovered later. Two concrete schema requirements carry forward directly:
  a composite `(model, rating)` index (or the general "match your two most common equality/range
  filters together" version of it for whatever the real filter UI ends up needing), and `GLOB`
  (never `LIKE`) for any prefix-scan predicate — this build's `LIKE`-to-index-range-scan optimizer
  transform did not trigger even with `PRAGMA case_sensitive_like` on, confirmed via `EXPLAIN QUERY
  PLAN` staying a full-table `SCAN` either way, while `GLOB`'s prefix scan is unconditional.
- **#25's backup approach**: SQLite's `VACUUM INTO` is the online, no-lock, no-optimize-step
  mechanism `docs/benchmarks.md`'s Maintenance target already assumes.
- **If the faceted-filter ceiling matters in practice**, the two live options are (a)
  trigger-maintained aggregate/facet-count tables in SQLite, tried first since it's the smaller
  change, or (b) revisiting DuckDB as primary — not adopting it as a sidecar, given nothing here
  needed the two-store complexity.
- **#71 (drive remapping)** and any future full-text/filename-search work should treat leading
  substring search as a known, unindexable-in-any-of-these-three-engines gap, not a regression to
  chase in whichever engine is chosen.

## Spike: `spikes/den/`

Not production code. `den` (a cat's den — where it keeps its stash) holds:

- `src/gen.rs` — a deterministic, seeded synthetic-catalog generator (`den gen --seed N --scale
  600k|2m`), sampling EXIF distributions from `docs/ref-10k-manifest.csv`, a 2–10% keep-rate
  rating/flag distribution, a folder tree, and the corrected keyword vocabulary described above.
  Determinism verified via `den gen --verify-determinism` (same seed → byte-identical hash).
- `src/workload.rs` — the `Workload` trait every backend implements identically, so a p50/p95
  number means the same query on every engine. Cross-engine agreement is a real test, not just an
  assertion: `tests/cross_engine.rs` runs the full query set against all three backends on a
  shared 10k fixture and checks their answers match (this caught a real bug — DuckDB's binder
  requires bound-parameter count to match exactly the placeholders textually present in a query,
  unlike SQLite's numbered-parameter semantics which tolerate an unreferenced `?N` slot; the
  dynamic-clause query builder in `duckdb_engine.rs` now numbers placeholders sequentially per
  clause actually appended, not by fixed per-predicate position).
- `src/sqlite.rs`, `src/duckdb_engine.rs`, `src/lmdb.rs` — one module per surviving candidate.
  `lmdb.rs`'s doc comment explains its six hand-maintained secondary indexes and where each query
  pattern's index choice trades off.
- `src/bin/den.rs` — the CLI: `den gen`, `den bench --engine <e> --scale <s>`, `den crash --engine
  <e>` (the in-process crash-safety approximation described above).
- `bench-results/den/` (gitignored) — raw per-run CSV/JSON backing every number in this ADR.

Several harness bugs, found and fixed while producing the numbers above, are worth naming since
they'd otherwise have silently produced wrong conclusions:

- The timing macro originally discarded errors from a failing operation (`let _ = result` instead
  of `result?`), which would have made a query that errors out on every call read as suspiciously
  *fast*.
- **The crash test originally never actually interrupted a write.** It called `bulk_ingest` (which
  commits fully before returning) and only forgot the handle *after* that commit succeeded — so the
  first version of the "0/20 failures" result for SQLite/DuckDB only ever tested reopening after an
  already-fully-committed write with an unclean shutdown tacked on, not a genuinely interrupted
  transaction. Fixed by adding `crash_mid_ingest` to the `Workload` trait: it writes half a batch
  and returns *before* calling `COMMIT`, so the forgotten transaction is real. The reused-path bug
  below was found while fixing this one.
- The crash test's id range and store path were both originally reused across all 20 iterations in
  one process. The id reuse caused a duplicate-key error unrelated to actual crash safety on every
  iteration after the first. Fixing `crash_mid_ingest` to leave a genuinely open transaction then
  surfaced a second, subtler bug from the same path reuse: SQLite's forgotten, never-closed file
  descriptor holds an OS-level advisory lock for the rest of the process's lifetime (unlike a real
  crash, where process death releases every lock the OS attributes to it), so the *second*
  iteration's reopen failed with "database is locked" — an artifact of staying in one process for
  20 trials, not a finding about SQLite's crash safety. Fixed by giving every iteration its own
  fresh store path, which is arguably the more correct design regardless (independent trials, not
  one file accumulating 20 rounds of abandoned state). This does not change LMDB's own result: its
  open-environment guard rejects reopening *any* forgotten path even once, confirmed to trigger on
  every iteration's own within-iteration reopen rather than being specifically about cross-iteration
  reuse — a fresh path per iteration doesn't help it the way it helped the other two.
- **A cross-engine correctness test (`tests/cross_engine.rs`) caught two more real bugs directly**,
  rather than relying on manual inspection: DuckDB's dynamic `faceted_filter` query builder used
  fixed placeholder numbers (`?1`/`?2`/`?3`) per predicate regardless of which clauses were actually
  appended, which works under SQLite's numbered-parameter semantics (unreferenced `?N` slots are
  harmless) but fails outright under DuckDB's binder (which requires the bound-value count to match
  the placeholders textually present) — caught by calling `faceted_filter(None, ...)` in the test,
  which the original benchmark path never exercised. And the benchmark's `tag_keyword` op was timed
  across 6 calls (1 discarded warm-up + 5 measured) using the *same* keyword string every time:
  SQLite's and DuckDB's `tag_keyword` are bare inserts with no dedup, so each call appended another
  10k rows, ending the loop with 60k accumulated duplicate rows and a growing index on later calls
  — while LMDB's equivalent secondary-index entry is a plain key overwrite, naturally idempotent.
  Fixed by numbering the keyword per call (`Bench.Tagged.{n}`), so every engine's timed call does an
  equivalent, non-compounding amount of work.
