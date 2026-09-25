# ADR-0015: RocksDB (`rocksdb` crate, LSM-tree embedded KV store), evaluated for the catalog store — not adopted

- **Status:** Rejected (not a rejection of the LSM-tree angle itself — see Consequences, same
  caveat ADR-0009/0010 made for Turso/redb)
- **Date:** 2026-09-24
- **Ticket:** [#115](https://github.com/jordanfelle/nicti/issues/115) Research: RocksDB (via
  rust-rocksdb) as a catalog engine candidate

## Context

ADR-0008 named LMDB's raw-KV/no-query-planner tradeoff shape but rejected it on an unmeasurable
crash-safety hard gate, not on performance. #115 asks a related but distinct question: RocksDB
(`facebook/rocksdb`, via the `rocksdb` crate published by the `rust-rocksdb` GitHub org) is the
LSM-tree engine specifically built for high write-concurrency production workloads (CockroachDB,
TiKV, and others chose it precisely because it doesn't have a single-writer-lock-file model the way
SQLite does) — worth evaluating directly against the recurring concern that SQLite-style
single-writer locking becomes a real pain point "at scale." Unlike every prior catalog-engine
candidate in this series (SQLite, DuckDB, LMDB, Turso, redb), all benchmarked single-threaded only,
this ADR's decision rule requires a genuine concurrent-multi-writer-thread measurement, run
identically against RocksDB and SQLite, as a real side-by-side comparison rather than an assumption.

Confirmed before this pass started (per #115's own filing), and re-confirmed here rather than taken
on faith:

- The **crate name is `rocksdb`** (`rust-rocksdb/rust-rocksdb`, current release **0.25.0**, pushed
  2026-08-16 — 5.5 weeks before this evaluation, comfortably inside the 6-month maintenance gate;
  2,178 GitHub stars, not archived). No competing crate under a `rust-rocksdb`-named slug exists on
  crates.io; this is the one and only actively-maintained Rust RocksDB binding.
- **License, confirmed precisely, not from a summary**: the `rocksdb` crate itself declares
  `license = "Apache-2.0"` in its own `Cargo.toml`, matching its repository's top-level `LICENSE`
  file (Apache License 2.0 full text). Its `librocksdb-sys` build/FFI companion is
  `MIT/Apache-2.0/BSD-3-Clause`. The **vendored native RocksDB C++ core** (a git submodule of
  `facebook/rocksdb`, invisible to `cargo deny`'s Rust-crate-graph view — the same
  native-code-license gap `docs/licensing.md`'s Native libraries section exists to catch) is
  **dual-licensed `Apache-2.0` OR `GPL-2.0-only`**, confirmed by reading both `LICENSE.Apache` and
  `COPYING` directly from `facebook/rocksdb`'s repository root — not inferred from README prose.
  **The Apache-2.0 arm is genuinely selectable**: an `X OR Y` dual license lets the recipient elect
  either arm unilaterally, and the Rust binding crate everything in Nicti actually links against is
  Apache-2.0 *only* (no GPL arm at all in the crate itself), so there is no GPL-2.0 exposure to even
  need to elect out of before reaching the dual-licensed native core underneath it.

## Decision rule (stated before measuring, same standard as #102/#106/#113)

Reuse `spikes/den`'s shared `Workload` trait, same hard/measured gates as every prior candidate,
against the same 600k/2M synthetic scale.

### Hard gates (an engine that fails one is not benchmarked further)

1. Builds and passes smoke tests on `x86_64-pc-windows-msvc` in CI.
2. License allowed under ADR-0003 (confirm the Apache-2.0 arm is genuinely selectable, not just
   present as an unused alternative).
3. Survives a mid-write crash: reopens cleanly, its own integrity check passes.
4. Actively maintained: a release within the last 6 months and a real Rust API.

### Measured gates at 600k/2M scale

Against `docs/benchmarks.md`'s Library targets: faceted filter/search/sort < 100ms p95; cold start
< 2s; single write ≤ 5ms. Same hand-maintained-secondary-index requirement as LMDB/redb (no query
planner) — `rocksdb_engine.rs` follows `lmdb.rs`'s exact indexing shape (one column family per
indexed dimension, byte-encoded composite keys, most-selective index chosen per query, the rest
post-filtered in application code) rather than inventing a different one, so the comparison stays
apples-to-apples.

### The question this evaluation specifically exists to answer

Run a workload with multiple concurrent writer threads hammering the store simultaneously, and run
the **identical** concurrent-writer workload against plain SQLite (`rusqlite`, WAL mode) for a real
side-by-side number — not an assumption that RocksDB is better here just because of its reputation.

## Decision

**Not adopted.** Hard gates 1/2/4 all pass with strong, verifiable evidence (see below). Hard gate 3
(crash-safety) is left **inconclusive** via this harness, for the same class of reason ADR-0008/
0009/0010 already documented for LMDB/Turso/redb — not a defect in RocksDB, a limitation of the
in-process `mem::forget` crash-simulation technique. The decision rests on the **measured gates**:
two query shapes miss the 2M budget outright (range query ~1.8x over, filename search ~4.5x over) —
worse in absolute terms than every prior candidate on these same two shapes — root-caused (tested,
not guessed) to RocksDB's real per-read LSM cost under this pass's untuned default configuration,
not a fixable indexing gap. **The concurrent-writer question this ADR exists to answer has a genuine
answer, but it is not the clean "RocksDB wins" story the issue's own framing worried about assuming
uncritically**: RocksDB does not show SQLite's textbook single-writer-serialization signature
(SQLite's aggregate throughput stays flat regardless of thread count while its own tail latency
grows monotonically with contention — exactly what a single-writer lock predicts), but RocksDB's
own throughput under sustained concurrent writers was **highly variable run-to-run in this pass's
environment** — ranging from over 1,000,000 writes/sec down to below SQLite's own ceiling in one
run — consistent with (not conclusively proven to be caused by) write-stall backpressure under its
default (untuned) configuration; a second, equally plausible explanation this pass cannot rule out
is this session's own shared, heavily loaded host (see the concurrent-writer section's own
confound disclosure) — no `Options::enable_statistics()`/stall-counter instrumentation was captured
to distinguish the two, a real gap a hostile review of this ADR correctly flagged. Either way, this
variance does not resemble SQLite's own lock-file model, which produces a *clean, monotonic* signal
(flat throughput, tail latency growing smoothly with thread count), not the noisy, non-monotonic
pattern RocksDB showed. Combined with the two measured-gate misses and
`rocksdb_engine.rs`'s own hand-maintained-secondary-index engineering cost (the same real cost
ADR-0008/0010 already charged against LMDB/redb), RocksDB is not a better fit for #22 than SQLite
today.

## Measured results

Backed by `spikes/den/` (see below). p50/p95/max over 5 measured runs, 1 discarded warm-up, per
`docs/benchmarks.md`'s methodology, except the concurrent-writer section (its own methodology, see
below). **Reference hardware:** this session's Linux/WSL2 sandbox — see the concurrent-writer
section's own caveat about a specific, real confound in that part of the run.

### Hard gates

| Gate | Result |
|---|---|
| 1. Windows build | ✅ Confirmed via `rust-rocksdb/rust-rocksdb`'s own `.github/workflows/rust.yml`: a real `windows-latest` job in its 3-OS test matrix, running `cargo nextest run --all` there — not just a doc claim. **One concrete, non-trivial Windows-specific build gotcha found and worth carrying into this repo's own CI, not just noted**: `librocksdb-sys` generates its FFI bindings with `bindgen`, which needs libclang on the build machine, and upstream's own Windows job has to `Remove-Item C:\msys64` and `choco install llvm -y` first to avoid a `libclang.dll` conflict with GitHub's `windows-latest` runner's own bundled msys64 install — mirrored exactly in this PR's `.github/workflows/ci.yml` change, not invented fresh. |
| 2. License | ✅ Apache-2.0 (binding crate) with a genuinely selectable Apache-2.0 arm on the dual-licensed native core — see the Context section above for the full chain of evidence. Already on `deny.toml`'s allowlist; `docs/licensing.md` updated in this PR with the precise per-component breakdown. |
| 3. Crash-safety | ⚠️ **Inconclusive — same class of harness limitation as LMDB/Turso/redb, confirmed and precisely scoped, not assumed.** `den crash --engine rocksdb --iterations 20` reported **20/20 reopen failures**: `IO error: lock hold by current process ... LOCK: No locks available`. RocksDB takes an OS-level `LOCK` file on the DB directory for the handle's lifetime, released on `Drop`/close; `mem::forget` skips that `Drop`, so the caller's later reopen of the *same* path fails. **A direct follow-up probe (mirroring ADR-0009/0010's own methodology) confirms this is scoped to the specific leaked path, not a process-wide guard like LMDB's** (ADR-0008's hard-gate-3 finding): opening a **different**, never-before-touched path in the same process, right after forgetting the first one, succeeds cleanly. Like ADR-0010's own equivalent probe for redb, this was a throwaway `src/bin/lock_probe.rs` binary, run once and deleted before this commit rather than kept as permanent test coverage — its result is transcribed here verbatim (`PATH B (fresh, never touched): opened OK` / `PATH A (same as forgotten): FAILED: ... LOCK: No locks available`), the same "confirmed by a since-deleted throwaway probe, not left as an uncited assertion" pattern this ADR series has used since ADR-0010, called out explicitly here rather than left implicit (a hostile review of this ADR correctly flagged that the claim had no artifact in the diff to check it against, same as it would for ADR-0010's own probe). This is the same *category* of finding as redb's own fd-scoped advisory lock (ADR-0010) — a real OS-level `SIGKILL` releases every lock the dying process held, which this in-process technique cannot faithfully simulate, so this should be read as "harness limitation, mechanism now precisely understood," not as a failing or passing result. A real fork+exec+SIGKILL harness remains the only way to actually settle it, per ADR-0008/0009/0010's own repeated, still-unbuilt follow-up. |
| 4. Maintained | ✅ `v0.25.0`, pushed 2026-08-16 (5.5 weeks before this evaluation), 2,178 GitHub stars, not archived. The native RocksDB core it vendors is itself a mature, heavily production-proven engine (CockroachDB, TiKV, and many others) — the strongest real-world production-deployment evidence of any candidate evaluated in this series, though that maturity is about the C++ core, not the Rust binding layer specifically. |

### Measured gates, 600k assets

| Query | Budget (2M) | RocksDB p50/p95 |
|---|---|---|
| Faceted filter + facet counts | < 100ms | 0.280 / 0.533 ms |
| Sort by date, first 500 | < 100ms | 0.267 / 0.274 ms |
| Folder-subtree count | < 100ms | 21.8 / 21.9 ms |
| Keyword-subtree query | < 100ms | 0.013 / 0.014 ms |
| Range query | < 100ms | **115.9 / 121.0 ms — already over budget at 600k** |
| Filename substring search | < 100ms | **148.7 / 164.4 ms — already over budget at 600k** |
| Cold open | < 2s | 8.7 ms |
| Single rating write | ≤ 5ms | 0.004 / 0.038 ms |
| 100-write rate burst (informative) | — | 0.254 / 0.319 ms |
| Bulk ingest, 600k rows (informative) | — | 5.45 s |
| Online backup / checkpoint (informative, not gated) | — | 0.176 / 0.278 ms |

Like redb (ADR-0010) and unlike SQLite/DuckDB/LMDB (ADR-0008), RocksDB already misses two gates at
600k scale, not just at 2M — worth stating plainly rather than only reporting the 2M table below.

### Measured gates, 2M assets (the planning-horizon scale the budget is stated against)

| Query | Budget | RocksDB p50/p95 |
|---|---|---|
| Faceted filter + facet counts | < 100ms | 0.145 / 0.180 ms ✅ |
| Sort by date, first 500 | < 100ms | 0.120 / 0.140 ms ✅ |
| Folder-subtree count | < 100ms | 73.7 / 91.6 ms ✅ (thin) |
| Keyword-subtree query | < 100ms | 0.011 / 0.012 ms ✅ |
| Range query | < 100ms | **181.5 / 183.9 ms ⛔** (~1.8x budget) |
| Filename substring search | < 100ms | **442.0 / 466.1 ms ⛔** (~4.5x budget, the worst of any candidate on this shape) |
| Cold open | < 2s | 8.1 ms ✅ |
| Single rating write | ≤ 5ms | 0.002 / 0.030 ms ✅ |
| 100-write rate burst (informative) | — | 0.116 / 0.119 ms |
| Bulk ingest, 2M rows (informative) | — | 21.3 s |
| Online backup / checkpoint (informative, not gated) | — | 0.215 / 0.285 ms |

**Online backup deserves its own positive note, not just a table entry**: RocksDB's `Checkpoint` API
(used by `rocksdb_engine.rs::backup()`) is a first-class, purpose-built, online, no-exclusive-lock
mechanism (hard-links unchanged SST files, copies only the small live WAL/manifest state) —
directly analogous to SQLite's `VACUUM INTO`/DuckDB's `EXPORT DATABASE`/LMDB's `env.copy_to_path`,
and notably **better** than redb's own admitted fallback to a bare `fs::copy` (ADR-0010) — RocksDB
was the only KV-shaped, no-query-planner candidate in this whole series with a real backup API,
not a gap. This isn't the deciding factor (the measured-gate misses are), but it's a genuine
strength worth recording plainly rather than only reporting misses.

**What was actually tested for the two misses, and what remains a hypothesis, not a measured
root cause.** A direct experiment ruled out the first, most obvious explanation (unflushed/
uncompacted L0 data from the single bulk `WriteBatch` commit): forcing an explicit
`compact_range_cf` across every column family **did not improve** `range_query` (54ms before
forced compaction vs 108ms after, on a separate 600k probe run) — if anything slightly worse, most
likely because compaction cold-starts a fresh set of SST blocks that then have to be paged in
again, not because compaction itself made reads slower in general. This rules out "the benchmark
accidentally measured an unflushed memtable" as the explanation — that part is measured, not
guessed. **What remains is a hypothesis, not independently isolated by this probe**: the ordinary
LSM point/range-read cost (a block-cache lookup per SST level touched, plausibly compounded by
this pass's default, unconfigured 8MB block cache at 2M-row scale) is *consistent with*
RocksDB's own documented architecture and this pass's default (untuned) `Options`, but this probe
did not isolate cache-miss cost from other possible factors, and **no bloom-filter policy is
configured anywhere in `rocksdb_engine.rs`** — without one, standard block-based tables don't use
bloom filters at all, so a bloom-filter-check cost specifically is not something this pass's
default configuration would even incur; an earlier draft of this ADR wrongly asserted it did. This
structural-cost family is still a real, distinct-from-LMDB explanation in kind (LMDB's direct
mmap'd B-tree lookups clear both of these same two query shapes easily, per ADR-0008, and redb's
own different but analogous per-page-checksum cost, ADR-0010, is the same class of finding), but
the *specific* mechanism within it is not established here. **A known, unattempted mitigation**: a
larger, explicitly-sized block cache and/or configuring a bloom-filter policy via
`BlockBasedTableOptions` — this pass measured RocksDB's out-of-the-box default configuration
deliberately (the realistic starting point for a hand-rolled catalog store, matching how LMDB/redb
were also evaluated untuned), and did not attempt a tuned re-run; worth a targeted follow-up if
RocksDB is ever reconsidered.

### The concurrent-writer comparison (`den concurrent-bench`) — #115's own reason for existing

**Methodology**: `n_threads` threads, each with its own DB handle/connection to the *same* store on
disk, each updating its own **disjoint** slice of 100,000 pre-seeded rows — disjoint key ranges, not
contended shared keys, matching the realistic shape of concurrent catalog writers (several
culling/tagging operations touching different photos at once) and isolating the engine's own
write-path serialization from application-level row-lock contention. **The actual per-write
operation is lighter than `write_rating`'s** — RocksDB's side is a bare `db.put()` on the default
column family (no read, no secondary-index maintenance), and SQLite's side is an unindexed
single-column `UPDATE` — see the methodology caveat below for exactly how this narrows what the
throughput numbers mean.
**RocksDB**: a plain `rocksdb::DB` (not `TransactionDB`) wrapped in `Arc`, shared across threads —
`rocksdb::DB` is `Send + Sync` and multi-threaded `put` is RocksDB's normal, documented usage
pattern (a write-group-leader thread batches concurrent writers' individual writes into one WAL
append + memtable insert), exactly the "no single-writer-lock-file" design #115 exists to test, not
a hand-tuned special case. **SQLite**: `rusqlite`, WAL mode, each thread opening its own
`Connection` to the same file with a 5-second `busy_timeout` — the realistic multi-writer topology
for an app (not one connection behind a mutex, which would trivially and artificially serialize
everything regardless of engine).

**A real, honest confound in this environment, disclosed rather than hidden**: these runs shared
this sandbox's host with an unrelated, concurrent evaluation session (#116's fjall candidate,
running in a sibling `nicti-wt-fjall` worktree) doing its own heavy `cargo`/`rustc` compiles —
`uptime` showed a load average of 22–30 on this 32-core host throughout this section's measurements.
This does not invalidate the *qualitative* finding below (SQLite's serialization signature is a
clean, well-understood, textbook pattern that this kind of background load wouldn't manufacture from
nothing), but it is a real reason the **RocksDB** numbers below show unusually high run-to-run
variance, and the absolute throughput figures for RocksDB specifically should be treated as
directional, not a clean production-grade number — a re-run on a quiet, dedicated machine is a
reasonable follow-up before treating any single RocksDB throughput figure below as load-bearing.

**SQLite (WAL mode), 100k rows, 20,000 writes/thread:**

| Threads | Aggregate writes/sec | Latency p50/p95/max |
|---|---|---|
| 1 | 89,197 | 0.0094 / 0.0157 / 0.7 ms |
| 4 | 123,792 | 0.0069 / 0.0098 / 229.7 ms |
| 8 | 97,401 | 0.0069 / 0.0129 / 959.4 ms |
| 16 | 93,771 | 0.0081 / 0.0142 / 1,963.2 ms |

**Corrected numbers, re-measured after an adversarial review found a real bug**: an earlier pass of
this benchmark set `synchronous=NORMAL` only on the setup connection, not on each worker thread's
own connection — `synchronous` is connection-specific, not persisted in the database file, so every
worker connection was silently running under SQLite's stricter default (`FULL`, which fsyncs on
every commit) instead of the `NORMAL` setting this benchmark was meant to measure. Fixed in
`concurrent_bench.rs`, and every number in this table is from the corrected re-run, not the original
one. The qualitative signature survives the correction: aggregate throughput does not scale with
thread count (in fact it settles into a narrower ~93k-98k range at 8-16 threads, with 4 threads as a
mild, unstable peak, not a real trend), and **max latency still grows dramatically with
contention**: 0.7ms at 1 thread → 1,963.2ms at 16 threads, as more writers queue up waiting for the
WAL writer lock — the underlying single-writer-serialization mechanism is unchanged by this fix,
only the absolute cost of each write was previously overstated. **A methodology note worth being
precise about**: `busy_retries` (this benchmark's own counter for `SQLITE_BUSY` errors caught and
retried in application code) reported **0 across every run** — this does *not* mean there was no
contention; it means `rusqlite`'s `busy_timeout` handles the wait internally (SQLite's own
busy-handler sleeps and retries before ever returning control to the calling code), so the queueing
time shows up entirely as elevated `max` latency on the writer that had to wait, not as an
application-visible retry count. Calling this out explicitly rather than letting a `busy_retries: 0`
row look like "no contention happened."

**RocksDB (plain `DB`, default `Options`), 100k rows, 20,000 writes/thread:**

| Threads | Aggregate writes/sec | Latency p50/p95/max |
|---|---|---|
| 1 (run 1) | 605,545 | 0.0015 / 0.0017 / 0.12 ms |
| 1 (run 2) | 361,492 | 0.0022 / 0.0040 / 1.66 ms |
| 4 | 193,856 | 0.0023 / 0.0043 / 10.7 ms |
| 8 (run 1) | 1,046,392 | 0.0066 / 0.0123 / 0.46 ms |
| 8 (run 2) | 64,965 | 0.0022 / 0.0436 / 24.8 ms |
| 16 (run 1) | 47,691 | 0.3268 / 0.4556 / 1.07 ms |
| 16 (run 2) | 142,719 | 0.0023 / 0.1862 / 21.8 ms |

**A methodology caveat that narrows what this table actually measures, found by a hostile review of
this ADR and worth stating plainly rather than leaving implicit**: this benchmark's per-write
operation is **not** the same shape as `write_rating` elsewhere in this crate, despite
`concurrent_bench.rs`'s own module doc comment originally claiming so (corrected there and here).
RocksDB's concurrent write is a single bare `db.put()` on the default column family — no read, no
secondary-index maintenance, no `WriteBatch` — versus the real `write_rating`'s read-modify-write
across two column families. SQLite's concurrent write is a real `UPDATE` against a table with **no
secondary indexes** (versus the real schema's three indexes touched by an actual rating write). Both
sides are lighter than their own single-threaded gate numbers, but RocksDB's simplification removes
proportionally more work (a full read plus a second CF's index maintenance) than SQLite's does (an
indexed vs. unindexed single-column `UPDATE` is a smaller relative gap) — so the throughput numbers
below should be read as measuring RocksDB's and SQLite's raw write-path concurrency behavior on a
minimal KV-shaped write, not a faithful prediction of the real catalog write's absolute throughput
under concurrency. The qualitative finding (no single-writer-lock signature vs. a clean one) is not
undermined by this — both engines were simplified in the same direction (fewer indexes/no read) —
but any specific multiplier below should be treated as an upper bound on RocksDB's real advantage,
not a precise prediction.

**RocksDB does not show SQLite's signature at all — but it also does not show a clean "wins outright"
story.** There is no sign of a single-writer lock forcing aggregate throughput flat: RocksDB's
peak observed throughput (1.05M writes/sec at 8 threads) is roughly 8-12x SQLite's own corrected
range (89k-124k writes/sec across thread counts, best single-thread result 89,197 writes/sec) — but
its lowest observed run (47.7k writes/sec at 16 threads) is now clearly **below** SQLite's entire
corrected range, not merely "the same order of magnitude" as an earlier draft of this ADR said
against the pre-fix SQLite numbers. What RocksDB shows instead is **large, non-monotonic run-to-run
variance** at every thread count — the same configuration (8 threads) produced both the best
(1.05M/s) and one of the worst (65.0k/s) results across this section's runs. **Two competing explanations for that variance, neither ruled out by
what this pass actually measured** (a hostile review of this ADR correctly flagged that the first
explanation below was originally stated as established fact with no instrumentation to back it):
(a) RocksDB's own documented write-stall backpressure mechanism (a defensive throttle that kicks in
when the memtable/L0-file count outpaces background flush/compaction) under this pass's **default,
untuned** `Options` (default `write_buffer_size`, default `max_background_jobs`); or (b) this
session's own shared, heavily loaded host (load average 22-30+ on 32 cores from a concurrent sibling
evaluation's builds, disclosed above) — a competing process stealing CPU/IO cycles unpredictably is
at least as plausible an explanation for "same config, best and worst result in the same session" as
an internal throttle. No `Options::enable_statistics()`/stall-property capture was added to this
pass to distinguish them, and a re-run on a quiet, dedicated host is the concrete way to settle it.
What both explanations agree on: this is not remotely SQLite's lock-file model, which produces a
clean, monotonic signal, not this noisy, non-monotonic one.

**Read plainly: the honest answer to #115's own question is "it depends on tuning and load, not a
guaranteed win"** — RocksDB's write path is architecturally free of SQLite's single-writer
bottleneck and can deliver an order of magnitude more throughput under favorable conditions, but
this pass did not reliably avoid large swings in throughput and tail latency under sustained
concurrent load (whether from its own default-configuration write-stall behavior, host contention,
or both), and at
least once landed at or below SQLite's own ceiling. A tuned configuration (larger write buffers,
more background flush/compaction threads, `TransactionDB` with a real conflict-detection story for
genuinely contended keys) was not attempted here — the same "known, unattempted mitigation" caveat
as the range_query/filename_search misses above — and is the right next step before drawing a firm
conclusion either way.

## Options considered

| Option | Verdict |
|---|---|
| RocksDB (`rocksdb` crate) | **Rejected.** Strong Windows-build/license/maintenance evidence (including a real, concrete Windows CI gotcha — a bindgen/libclang conflict — found and mirrored into this repo's own CI). Two measured query shapes miss the 2M budget, one badly (filename search ~4.5x over, the worst of any candidate on this shape). The unflushed-memtable hypothesis for this was tested and ruled out (compaction was tried and did not help); the remaining LSM-read-cost explanation is consistent with RocksDB's architecture but not independently isolated by this pass — see the ADR's own Root-cause section. The concurrent-writer comparison this ADR exists to produce has a genuine, nuanced answer: RocksDB avoids SQLite's textbook single-writer serialization signature and can deliver up to ~8-12x SQLite's corrected throughput range, but its results under default settings were highly variable, including at least one run below SQLite's entire corrected range; the cause remains unresolved — not the clean, reputation-driven "RocksDB wins" story #115 was filed specifically to test against actual measurement. |
| SQLite (`rusqlite`, WAL) | Unchanged from ADR-0008: still chosen. Its concurrent-writer signature (flat aggregate throughput, monotonically growing tail latency under contention) is now directly, cleanly measured for the first time in this series — confirming the real cost this candidate's own filing worried about, even though it isn't the deciding factor here (RocksDB's own measured-gate misses and tuning-dependent concurrent story are). |
| LMDB (`heed`) | Unchanged from ADR-0008: still the best raw single-threaded numbers on every indexable op, still rejected on the same unmeasurable crash-safety hard gate. |
| `redb` | Unchanged from ADR-0010: still not adopted, same per-page-cost story as this ADR's own findings for RocksDB (a different mechanism, checksums vs. bloom-filter/block-cache misses, same *shape* of finding: real per-read cost, not a fixable indexing gap). |

ADR-0008's decision is unchanged: **SQLite remains chosen, DuckDB remains the proven fallback.**

## Consequences

- **The "SQLite's single-writer model is a scaling risk" concern this ADR exists to test is now
  directly measured, not assumed, for the first time in this series** — and it's real: SQLite's own
  concurrent-writer signature (flat throughput, tail latency growing with contention) is exactly
  what the issue predicted. This doesn't change ADR-0008's decision because Nicti's own catalog
  workload (a single-user desktop app, per ADR-0001) has nothing resembling #115's own
  multi-writer-thread stress shape in its real usage pattern today — but if a future feature (#64's
  multi-machine catalog, or any server-side/multi-process write path) ever needs genuine concurrent
  writers, **this ADR's own numbers are the first real evidence in this repo that SQLite's model
  would need to be revisited then**, and RocksDB (tuned, not default) is the strongest candidate on
  record for that specific future need — worth remembering rather than re-researching from scratch.
- **#22 should not spend further design effort accommodating RocksDB.** Proceed on SQLite per
  ADR-0008.
- **The fork+exec+SIGKILL harness gap, flagged in ADR-0008/0009/0010, is now four-for-four**
  (LMDB, Turso, redb, RocksDB all left this pass's crash-safety gate unresolved, for four different
  precisely-identified underlying mechanisms). Still out of scope for this pass; still looking more
  like standing infrastructure this project's database research should just have than an optional
  nicety, four candidates in.
- **A tuned-RocksDB re-run (larger block cache, bloom-filter tuning, larger write buffers/more
  background flush threads) is the concrete, named follow-up** if RocksDB is ever reconsidered —
  both the measured-gate misses and the concurrent-writer variance point at the same class of
  fix (this pass deliberately measured defaults, matching how LMDB/redb were also evaluated
  untuned), not different ones.
- **`tests/cross_engine.rs` now covers six engines**, unchanged in scope from ADR-0009/0010's own
  caveat about what it doesn't exercise (`crash_mid_ingest`, `rate_burst`, `backup()` still aren't
  asserted there for any engine) — still worth widening in a future pass, still not specific to
  this ADR's own conclusion.
- **The Windows CI bindgen/libclang gotcha found here is worth remembering generically**, not just
  for RocksDB: any future crate whose `-sys` companion uses `bindgen` on Windows should expect the
  same `C:\msys64`/`libclang.dll` conflict on GitHub's `windows-latest` runner and the same
  `choco install llvm` fix, confirmed against upstream's own working CI recipe.

## Spike: `spikes/den/`

Not production code — see `CLAUDE.md`'s package map. This ADR adds:

- `src/rocksdb_engine.rs` — implements the same `Workload` trait as every other candidate, behind
  the `rocksdb` Cargo feature (default-off, matching `turso`/`redb`). Named `rocksdb_engine`, not
  `rocksdb`, to avoid shadowing the external crate. Follows `lmdb.rs`'s exact indexing shape: one
  RocksDB column family per indexed dimension (`assets`, `by_date`, `by_folder`, `by_keyword`,
  `by_model`, `by_rating`), byte-encoded composite keys (`prefix || 0x00 || big-endian id`) —
  RocksDB's default bytewise comparator makes prefix scans correct with no `prefix_extractor`/bloom
  configuration needed, confirmed by `tests/cross_engine.rs::rocksdb_matches_shared_workload`, not
  just asserted. `bulk_ingest` uses one `WriteBatch` + one `db.write()` call (atomic across the
  whole call, directly comparable to every other engine's own single-transaction bulk load).
  `crash_mid_ingest` is structurally different from `redb_engine.rs`'s own (see that function's doc
  comment for the full reasoning): RocksDB has no multi-key atomic transaction outside an explicit
  `WriteBatch`, so it commits each of the first half's assets as its own small per-asset atomic
  batch and returns before touching the second half — this distinction turned out not to matter for
  the actual crash-safety gate result (the reopen step fails first, on the leaked-`LOCK`-file issue
  documented above, before the half-committed-vs-fully-committed question is ever reached).
  `backup()` uses RocksDB's `Checkpoint` API (a real online, no-lock snapshot mechanism, better than
  redb's own bare `fs::copy` fallback). `integrity_check()` is a manual full-table deserialize scan,
  same honest-scope-limit pattern as every other engine's own check in this crate.
- `src/concurrent_bench.rs` — #115's own reason for existing: `run_concurrent_writers_rocksdb`/
  `run_concurrent_writers_sqlite`, gated on both the `rocksdb` and `sqlite` features. Not part of
  the shared `Workload` trait (only these two engines are compared this way) — see the module's own
  doc comment for the full methodology (disjoint per-thread key ranges, one connection/handle per
  thread, `busy_timeout` on the SQLite side so contention shows up as latency rather than an
  immediate error).
- `den concurrent-bench --engine <rocksdb|sqlite> --n-rows N --threads T --writes-per-thread M` — a
  new CLI subcommand in `src/bin/den.rs` driving the above.
- `.github/workflows/ci.yml`'s `build-windows` job: added `rocksdb` to the `cargo test -p den
  --features ...` line, plus the `Remove-Item C:\msys64`/`choco install llvm` steps described above
  (mirrored from `rust-rocksdb/rust-rocksdb`'s own working Windows CI recipe, not invented fresh).

Two things found and fixed while producing the numbers above, worth naming since they'd otherwise
have silently produced a wrong benchmark:

- `concurrent_bench.rs`'s SQLite setup phase originally passed a bare `u64` to
  `rusqlite::Statement::execute`, which doesn't implement `rusqlite::ToSql` (only `duckdb::ToSql`,
  a similarly-named trait from a different crate already in this workspace) — caught by `cargo
  check`, not a runtime surprise; fixed with an explicit `as i64` cast.
- The first version of this module's doc comment (and `rocksdb_engine.rs`'s own) asserted RocksDB
  "has no process-wide open-environment guard the way LMDB does," reasoning from the API shape
  alone before the crash test had actually been run. Running `den crash --engine rocksdb` found
  this was wrong in the specific way this ADR's crash-safety row now documents (a real `LOCK` file,
  just scoped differently than LMDB's) — corrected in both the code comments and this ADR rather
  than left as an untested claim once the real behavior was measured.
