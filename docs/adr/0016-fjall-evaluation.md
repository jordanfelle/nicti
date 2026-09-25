# ADR-0016: fjall, evaluated for the catalog store — not adopted

- **Status:** Rejected
- **Date:** 2026-09-24
- **Ticket:** [#116](https://github.com/jordanfelle/nicti/issues/116) Research: fjall (pure-Rust
  LSM embedded KV store) as a catalog engine candidate

## Context

ADR-0008 chose SQLite (`rusqlite`) for v1. #102 (Turso Database, ADR-0009), #106 (`redb`,
ADR-0010), and #113 (libSQL, ADR-0014) have all been evaluated since; #115 covers RocksDB in a
parallel evaluation. #116 asks about **fjall** (`fjall-rs/fjall`), a pure-Rust, LSM-tree-based
embedded key-value store — same tradeoff shape as LMDB (ADR-0008) and `redb` (ADR-0010): a raw KV
store with no query planner, needing hand-built secondary indexes, but matching ADR-0001's stated
preference for pure-Rust dependencies where a real option exists.

Confirmed at spike time, not assumed from the issue's own framing:

- **License**: `MIT OR Apache-2.0`, confirmed directly from crates.io's version-level API response
  for `fjall` v3.1.10 (the current stable release) and cross-checked against the bundled
  `LICENSE-MIT`/`LICENSE-APACHE` files in the downloaded crate source. Its own dependency
  (`lsm-tree`, the LSM-tree engine fjall wraps) carries the identical `MIT OR Apache-2.0` in its
  own `Cargo.toml`, not assumed to match fjall's.
- **Maintenance**: `fjall` 3.1.10 was published 2026-08-30 — 25 days before this evaluation,
  comfortably inside the 6-month bar — confirmed via crates.io's own API, not asserted from the
  issue's "reportedly settling into maintenance" framing. The crate's own release history shows a
  steady cadence through 2026 (3.1.4 in April, 3.1.5 in June, 3.1.6/3.1.7 in July, 3.1.8 in July,
  3.1.9 in August, 3.1.10 in August) — active, not stalled.
- **No native C/C++ core at all**: unlike SQLite/DuckDB/LMDB, `fjall` and `lsm-tree` have no
  `build.rs` in either crate — 100% safe Rust (the crate itself carries
  `#![deny(unsafe_code)]`). This is stronger Windows-build evidence than a benign `build.rs`
  (there is no cross-platform linker-flag surface to get wrong at all, the exact class of bug
  that hard-gate-failed `pglite-rs` in ADR-0008), and stronger than needing to read a bundled C
  fork's own platform branches the way ADR-0014 had to for libSQL.

## Decision rule (stated before measuring, same standard as #102/#106/#113)

Reuse `spikes/den`'s shared `Workload` trait, same hard/measured gates as every prior candidate:

**Hard gates** (an engine that fails one is not benchmarked further):

1. Builds and passes smoke tests on `x86_64-pc-windows-msvc` in CI.
2. License allowed under ADR-0003 (permissive, or LGPL isolated behind a dylib).
3. Survives `kill -9` during a write loop: reopens cleanly, integrity check passes.
4. Actively maintained: a release within the last 6 months and a real Rust API.

**Measured gates** at 600k/2M scale, against `docs/benchmarks.md`'s Library targets: faceted
filter/search/sort < 100ms p95; cold start < 2s; single write ≤ 5ms. Needs hand-maintained
secondary indexes (no query planner), following `spikes/den/src/lmdb.rs`'s existing design as the
template for how much indexing is a fair comparison for a raw-KV candidate — the same standard
`redb_engine.rs` (ADR-0010) already followed, so `fjall_engine.rs` reuses `redb_engine.rs`'s exact
indexing strategy (one keyspace per indexed dimension, byte-encoded composite keys) rather than
inventing a new one, keeping the comparison apples-to-apples.

## Decision

**Not adopted.** fjall's license and maintenance gates pass cleanly, and it has a genuinely clean
pure-Rust, no-native-code build story (no `build.rs`, nothing to cross-compile) — the strongest
Windows-build story of any candidate evaluated so far, though this pass's own confirmation is
local/Linux-side reasoning about the absence of native code, not yet a real `windows-latest` CI
run; that's on this PR's own CI to confirm, not asserted here as already-verified. Crash-safety is
**inconclusive**, not a clean pass — for the same structural reason
ADR-0009/ADR-0010 already documented for Turso Database/`redb`: this spike's in-process
`mem::forget` crash simulation cannot distinguish a real `kill -9` (which releases every OS-level
lock the dead process held) from a leaked file descriptor inside the *same, still-alive* test
process — and fjall holds exactly that kind of OS-level advisory file lock (a `std::fs::File::
try_lock()` on a per-directory lock file, released only by its own `Drop` impl, which `mem::forget`
skips). **What actually decides this evaluation**: fjall **fails three of the eight measured query
gates, at both 600k and 2M scale**, by a wide margin (2–5x over budget even at the smaller scale,
not just at the 2M planning horizon the way SQLite's one known miss or `redb`'s two misses only
showed up at 2M) — no other candidate in this series has missed this many gates this early.
Combined with the real engineering cost visible in `fjall_engine.rs` (six hand-maintained
secondary indexes, no query planner, same category of hand-rolled cost LMDB/`redb` already showed),
fjall is not a better fit for #22 than SQLite (still the incumbent per ADR-0008/0012) or than
`redb`/LMDB among the KV-shaped alternatives already evaluated.

## Measured results

**Reference hardware:** this session's Linux/WSL2 sandbox, 32 cores, NVMe-backed — same box,
same session, as the SQLite/LMDB/`redb` baselines below (run back-to-back, not reused from
ADR-0008/0010, so the comparison is apples-to-apples on identical hardware/load). Per the
reference-machine rule ADR-0008 established: WSL numbers count as final when every measured gate
clears its budget with ≥ 3x margin; a Windows re-run is only required for a gate within 3x of its
budget — moot here since fjall's misses are the *opposite* direction (2–5x *over* budget, not
under), which is itself a hardware-independent, root-caused finding (see below), not a
close-margin case needing a Windows re-check.

### Hard gates

| Gate | Result |
|---|---|
| 1. Windows build | ✅ Strong source-level evidence: neither `fjall` nor its own `lsm-tree` dependency has a `build.rs` at all — no native C/C++ compilation step, no linker-flag surface (the exact bug class that hard-gate-failed `pglite-rs` in ADR-0008). The only `cfg(target_os = ...)` branches in the crate (`src/file.rs`, `src/db_config.rs`) are genuine, dedicated `windows`/`macos` branches, not a "does everything except macOS" pattern with no real Windows path. This PR extends `.github/workflows/ci.yml`'s `build-windows` job (`cargo test -p den --features sqlite,duckdb,lmdb,turso,redb,rocksdb,fjall`) — the authoritative confirmation is that CI run, not this local evidence alone. |
| 2. License | ✅ `MIT OR Apache-2.0`, confirmed independently from crates.io and the bundled `LICENSE-MIT`/`LICENSE-APACHE` files — already on `deny.toml`'s allowlist. One real, mechanical allowlist edit was needed (unlike `redb`/Turso's zero-edit updates): `varint-rs` (a transitive dependency of `lsm-tree`) carries `0BSD`, not previously allowed — added as its own entry (OSI-approved, even more permissive than MIT/Apache-2.0, no attribution requirement). `cargo deny --workspace --all-features check licenses` passes clean after that one addition. |
| 3. Crash-safety | ⚠️ **Inconclusive, not failed** — **20/20 reopen failures**, but every failure is the identical `FjallError: Locked`, from a fresh path on the very first attempt to reopen it (not a cross-iteration path-reuse artifact — `den crash`'s harness already uses a fresh path per iteration, per ADR-0009's own established practice). Root-caused at the source level, not assumed: `fjall`'s `LockedFileGuard` (`src/locked_file.rs`) takes a `std::fs::File::try_lock()` (an OS-level advisory lock) on a per-directory lock file when the store opens, released only by `LockedFileGuardInner`'s own `Drop` impl. `mem::forget`ing the engine (this spike's crash-simulation technique) skips `Drop` entirely, so the lock is never released for the rest of this *same, still-alive* test process — the identical structural limitation ADR-0009 found for Turso Database's `fcntl` lock and ADR-0010 found for `redb`'s OS-level byte-range lock, both scoped to the one leaked file descriptor rather than LMDB's process-wide open-environment table. A real `kill -9` doesn't have this problem (the OS reclaims every lock the dead process held), but this in-process technique cannot distinguish that from a bug — a real fork+exec+SIGKILL harness is the only way to actually resolve it, out of scope for this pass, same conclusion as every prior fd-scoped-lock finding in this series. |
| 4. Maintained | ✅ `fjall` 3.1.10 published 2026-08-30 (25 days before this evaluation); steady 2026 release cadence (3.1.4 → 3.1.10, roughly monthly); GitHub repo (`fjall-rs/fjall`) not archived. Real, working API, not a stub — confirmed by reading the actual crate source (`Database`/`Keyspace`/`OwnedWriteBatch`/`Snapshot`), exercised end-to-end by `fjall_engine.rs` and its passing `cross_engine.rs` correctness test. |

### Measured gates, 600k assets (p50/p95, this session's own back-to-back run)

| Query | Budget | SQLite | LMDB | redb | fjall |
|---|---|---|---|---|---|
| Faceted filter + facet counts | < 100ms | 47.7 / 49.0 ms ✅ | 0.031 / 0.041 ms ✅ | 0.045 / 0.108 ms ✅ | 0.49 / 4.57 ms ✅ |
| Sort by date, first 500 | < 100ms | (see 2M row; not separately re-run at 600k this session) | — | — | 0.197 / 0.208 ms ✅ |
| Folder-subtree count | < 100ms | — | **3.77 / 4.72 ms ✅** | **9.44 / 13.99 ms ✅** | **176.2 / 220.3 ms ⛔** |
| Keyword-subtree query | < 100ms | — | — | — | 0.027 / 0.059 ms ✅ |
| Range query (rating+iso+date) | < 100ms | — | **44.7 / 47.5 ms ✅** | **66.7 / 98.6 ms ✅** | **370.0 / 407.0 ms ⛔** |
| Filename substring search | < 100ms | — | 123.5 / 155.0 ms ⛔ (known, unindexable-by-any-of-these-engines gap) | 192.4 / 223.1 ms ⛔ (same) | **446.1 / 604.3 ms ⛔ (worst of the three)** |
| Cold open | < 2s | — | — | — | 3.8 ms ✅ |
| Single rating write | ≤ 5ms | — | — | — | 0.004 / 0.047 ms ✅ |
| Bulk ingest, 600k rows (informative) | — | — | 6.6 s | 15.2 s | **23.4 s (slowest)** |
| Online backup (informative) | — | — | 572 / 618 ms | 566 / 607 ms | 380 / 570 ms |

### Measured gates, 2M assets (the planning-horizon scale the budget is stated against)

| Query | Budget | SQLite | LMDB | redb | fjall |
|---|---|---|---|---|---|
| Faceted filter + facet counts | < 100ms | **172.1 / 201.0 ms ⛔ (known ADR-0008 gap)** | 0.035 / 0.059 ms ✅ | 0.056 / 0.140 ms ✅ | 0.16 / 0.31 ms ✅ |
| Sort by date, first 500 | < 100ms | 0.036 / 0.040 ms ✅ | 0.0038 / 0.0046 ms ✅ | 0.017 / 0.017 ms ✅ | 0.137 / 0.145 ms ✅ |
| Folder-subtree count | < 100ms | 9.99 / 13.00 ms ✅ | **6.14 / 7.44 ms ✅** | **22.58 / 23.38 ms ✅** | **201.4 / 222.1 ms ⛔** |
| Keyword-subtree query | < 100ms | 0.042 / 0.105 ms ✅ | 0.0018 / 0.0027 ms ✅ | 0.0085 / 0.0093 ms ✅ | 0.019 / 0.020 ms ✅ |
| Range query | < 100ms | 4.79 / 4.82 ms ✅ | **86.5 / 150.7 ms ⛔ (p95 over budget)** | **121.5 / 213.3 ms ⛔** | **322.1 / 339.4 ms ⛔** |
| Filename substring search | < 100ms | 89.2 / 90.5 ms ✅ (thin) | 193.3 / 211.6 ms ⛔ | 319.5 / 403.6 ms ⛔ | **505.9 / 533.4 ms ⛔ (worst of all four)** |
| Cold open | < 2s | 1.43 ms ✅ | 0.16 ms ✅ | 0.27 ms ✅ | 2.79 ms ✅ |
| Single rating write | ≤ 5ms | 0.014 / 0.022 ms ✅ | 0.009 / 0.021 ms ✅ | 0.049 / 0.081 ms ✅ | 0.006 / 0.099 ms ✅ |
| 100-write rate burst (informative) | — | 0.60 / 0.63 ms | 0.071 / 0.074 ms | 0.261 / 0.285 ms | 0.159 / 0.240 ms |
| Tag 10k assets (informative) | — | 9.92 / 11.34 ms | 1.66 / 1.72 ms | 6.60 / 6.84 ms | 3.38 / 3.41 ms |
| Bulk ingest, 2M rows (informative) | — | 135.6 s | 14.8 s | 22.5 s | **31.9 s (slowest of the KV engines)** |
| Online backup (informative) | — | 1732 / 1875 ms | 640 / 789 ms | 1022 / 1105 ms | 285 / 300 ms |

**fjall fails `folder_subtree_count`, `range_query`, and `filename_search` at both scales — not
just at 2M.** This is the deciding, disqualifying finding: every other measured op (faceted
filter, sort, keyword-subtree, write_rating, rate_burst, tag_keyword, cold_open) is fast, often the
fastest or competitive with LMDB/redb — the failures are specific to these three query shapes, not
a blanket "fjall is slow" result.

**Root cause, investigated directly, not assumed**: `folder_subtree_count`/`range_query` both walk
a prefix/range scan over a hand-built secondary-index keyspace, matching a broad, non-selective
range (folder_subtree_count's `"NVMe/2024"` prefix alone matches ~100k of 600k rows — the same
absolute query LMDB/redb answer in single-digit milliseconds). Two hypotheses were tested directly
with a throwaway probe (not part of the spike, deleted before this PR), not left as speculation:

1. **LSM read-amplification against freshly-flushed, uncompacted segments right after
   `bulk_ingest`.** Ruled out: calling `Keyspace::major_compact()` on every keyspace between
   ingest and the read benchmarks changed `folder_subtree_count`'s time by only ~15% (125ms →
   106ms) — nowhere near closing a 20–50x gap to LMDB/redb.
2. **fjall's default LZ4 block compression, paid on every read as a per-block decompression
   cost** (unlike LMDB's/redb's raw mmap'd byte reads, which pay no decompression at all).
   **Confirmed as a real, partial contributor**: re-running the same `by_folder` prefix scan with
   `CompressionPolicy::disabled()` cut the time from ~50–125ms to ~38ms — a genuine ~2–3x
   improvement — but still nowhere near LMDB's 3.8–7.4ms or redb's 9.4–23.4ms range.

**The remaining gap (even after ruling out compaction lag and confirming, but not fully
explaining away, a compression cost) is attributed to fjall's own `Guard`/iterator abstraction
overhead per matched item**, relative to `heed`'s/`redb`'s zero-copy byte-slice cursor reads — not
independently root-caused further than this (out of scope for this pass; fjall's own internals
below the public `Keyspace`/`Snapshot` API weren't profiled). This is reported as the most
plausible remaining explanation, not a confirmed final answer — a genuinely open question a future
pass could resolve with a profiler, which this one didn't run.

**`filename_search`** is the one query no engine in this series can index (a leading wildcard
substring match), so every engine's number here is a full scan — fjall is simply the slowest full
scan of the four (505.9/533.4ms at 2M, ~1.3x worse than redb's own already-failing number, ~2.6x
worse than LMDB's), consistent with the same per-item overhead the other two misses point to.

**Bulk ingest is the slowest of the three KV-shaped candidates at both scales** (23.4s/31.9s vs.
LMDB's 6.6s/14.8s and redb's 15.2s/22.5s) — a real, if non-gated, cost. `fjall_engine.rs`'s
`bulk_ingest` stages the whole call in one atomic `Database::batch()` (matching every other
KV-shaped candidate's "one write transaction per call" shape, for a fair comparison), but unlike
`heed`'s `RwTxn`/`redb`'s `WriteTransaction` — both of which apply writes to the store's own
on-disk/mmap structures as part of the transaction — fjall's `OwnedWriteBatch` stages every item in
a plain in-process `Vec<Item>` and only touches the journal inside `.commit()`. This has a second,
structural consequence worth naming even though it isn't gated: **no read-your-own-writes within
one open batch** (a read against a `Keyspace`/`Snapshot` only ever sees the last *committed* state,
never an item already staged in an open, uncommitted batch) — harmless for this benchmark's
`rate_burst` (no id repeats within one burst), but a real divergence from LMDB's/redb's open-txn
semantics a production implementation would need to route around.

**Online backup is fast (285–300ms at 2M, fastest of the four)**, but this is not a fully
apples-to-apples comparison the way the query numbers are: fjall has no purpose-built backup API
(unlike SQLite's `VACUUM INTO`/LMDB's `env.copy_to_path`), so `fjall_engine.rs::backup()` does a
recursive `std::fs::copy` of the whole store directory while holding a `db.snapshot()` open across
the copy. Per fjall's own documented guarantee ("old data will not be dropped until it is not
referenced by any active snapshot"), this is a *stronger* claim than `redb_engine.rs`'s equivalent
bare-copy backup could honestly make (that module's own doc comment calls its held-open read
transaction "a real, if partial, safety property" with no upstream guarantee behind it) — but it is
still not commit-boundary-coordinated the way a purpose-built backup call is, and this spike never
exercises a concurrent writer during backup for any engine (the same scope limit ADR-0008 already
calls out), so this gap doesn't affect the number measured here either way.

## Options considered

| Option | Verdict |
|---|---|
| fjall | **Rejected.** Cleanest Windows-build story of any candidate (no native code, no `build.rs` at all) and a real, active maintenance signal — but fails 3 of 8 measured gates at both 600k and 2M (folder-subtree count, range query, filename search), by 2–5x, the widest and earliest measured-gate failure of any candidate in this series. Root-caused to fjall's own read-path overhead on broad prefix/range scans (partially, not fully, attributable to default LZ4 block compression; ruled out LSM-compaction-lag directly). Crash-safety inconclusive for the same fd-scoped-OS-lock reason as Turso/`redb`. |
| SQLite (`rusqlite`, status quo) | **Kept**, per ADR-0008/0012/0014. Nothing in this evaluation changes that decision. |
| LMDB (heed) | Still the best raw KV numbers on most indexable ops of any candidate measured so far — but this session's own 2M `range_query` p95 (150.7ms) misses the 100ms budget, a real, unremarked-until-hostile-review ~1.8x regression vs. ADR-0008's own 2M baseline for the same op (84.0ms, "thin" but passing there); not chosen as primary per ADR-0008's crash-safety-methodology reasoning either way, unchanged here. |
| `redb` | Still not adopted per ADR-0010 (2M range-query/filename-search misses) — this session's numbers reconfirm that finding on the same hardware. |

## Consequences

- **#22 should not adopt fjall for v1 or as a fallback.** SQLite stays chosen per ADR-0008/0012;
  DuckDB stays the documented fallback if SQLite's own faceted-filter ceiling becomes a blocking
  problem in practice.
- **fjall is not a strong candidate to revisit later either**, unlike libSQL (ADR-0014's "revisit
  for #64" recommendation): its measured gaps are on ordinary filter/range query shapes any v1 or
  v2 catalog workload needs, not on a feature (embedded-replica sync) that's simply unneeded yet.
  A future pass should only revisit fjall if a major-version release specifically claims to have
  closed this read-path gap, not on a routine maintenance-cadence check alone.
- **The compaction-lag-vs-compression investigation is a real, reusable methodology note for any
  future LSM-tree candidate this project evaluates**: rule out post-ingest compaction lag directly
  (call the engine's own compaction API and re-measure) before attributing a fresh-store read
  slowdown to "LSM engines are just slower," and separately test the specific compression
  configuration in use, since both are real, independent, and only partially overlapping
  explanations here.
- **`tests/cross_engine.rs` now covers seven engines** (added `fjall_matches_shared_workload`),
  unchanged in scope from ADR-0010/0014's own caveat about what it doesn't exercise
  (`crash_mid_ingest`, `rate_burst`, `backup()` still aren't asserted there for any engine).
- **No CI job split was needed for fjall** (unlike libSQL/ADR-0014's mandatory `--exclude den` +
  separate feature-scoped commands): fjall has no bundled native C source of its own, so it links
  cleanly into the same `den` binary as `sqlite`/`duckdb`/`lmdb`/`turso`/`redb` — confirmed
  directly with a local `--all-features` build, not assumed from "pure Rust ⇒ no collision."
  `.github/workflows/ci.yml`'s existing `sqlite,duckdb,lmdb,turso,redb` command simply gained
  `,fjall`, on both the Linux `test` job and the Windows `build-windows` job.

## Spike: `spikes/den/src/fjall_engine.rs`

Implements the same `Workload` trait as every other candidate, behind a new `fjall` Cargo feature
(default-off, matching `turso`/`redb`/`libsql`). Named `fjall_engine`, not `fjall`, to avoid
shadowing the external crate (same convention as `turso_engine`/`redb_engine`/`libsql_engine`).
Follows `redb_engine.rs`'s exact indexing design (one keyspace per indexed dimension, byte-encoded
composite keys `prefix || 0x00 || big-endian id`), the same standard #116's own decision rule
requires — with one ergonomic difference worth noting rather than silently inheriting:
fjall's own top-level API ships a native `prefix()` iterator (not just `range()`), so
`fjall_engine.rs` doesn't need `redb_engine.rs`'s manual "does this key still start with the
prefix" loop the way `redb`'s API required.

Three real, structural findings from writing this backend, not workarounds papered over:

- **fjall's atomic cross-keyspace write primitive (`Database::batch()`) stages entirely in
  process memory until `.commit()`**, unlike `heed`'s `RwTxn`/`redb`'s `WriteTransaction` (both of
  which touch the store's own on-disk structures as part of an open transaction). This means
  `crash_mid_ingest`'s "leave a genuinely open, uncommitted transaction, then `mem::forget` it"
  methodology — the identical contract every prior ADR in this series measures against — cannot
  leave *any* on-disk trace for fjall: nothing reaches the journal pre-commit. This isn't a test
  that was skipped or weakened; it's the honest, structural answer fjall's own API gives to the
  same question every other engine's crash test asks (see this module's own doc comment for the
  full reasoning, and the ADR's crash-safety row above for what the *actual* observed failure mode
  turned out to be — an OS file lock, not a torn write).
- **No read-your-own-writes within one open batch** — see the ADR's Measured Results, bulk-ingest
  discussion above.
- **Background worker threads.** `Database::open` starts a small compaction/flush thread pool
  (`min(cores, 4)` by default), joined cleanly on `Drop`. `den crash`'s `mem::forget` technique
  skips `Drop` entirely, and fjall has no public API to stop this pool independently of a full
  graceful close — unlike Turso/libSQL, where `Workload::prepare_for_forget` can shut down just
  their own async runtime first. Each crash-loop iteration therefore leaks that iteration's worker
  threads for the rest of the test process's life. This has no bearing on the crash-safety
  *result* itself (those threads only ever touch already-committed data via background
  compaction/flush; reopening replays the journal regardless of whether a background flush ran),
  but it's a real, fjall-specific spike-tooling cost worth naming, per this file's own standing
  rule that a clean-looking pass and a methodology gap should never look identical to a later
  reader.

`tests/cross_engine.rs` includes `fjall_matches_shared_workload`, which passed on the first
attempt against the real API.

One real `deny.toml` edit was needed: `varint-rs` v2.2.1 (transitively pulled in via `lsm-tree`)
carries `0BSD`, not previously on the allowlist — added as its own permissive, OSI-approved entry
(see this ADR's hard-gate-2 row). `docs/licensing.md` gets one update-log paragraph for `fjall`
itself, documenting both the license confirmation and this one allowlist addition, per this repo's
own ADR-0003-established process — no Native-libraries table row needed, since fjall has no
bundled native C/C++ core to document there at all (unlike SQLite/DuckDB/LMDB/libSQL, all of which
needed one).
