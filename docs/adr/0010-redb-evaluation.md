# ADR-0010: `redb` (pure-Rust embedded KV store), evaluated for the catalog store — not adopted

- **Status:** Rejected (not a rejection of the pure-Rust angle itself — see Consequences, same
  caveat ADR-0009 made for Turso)
- **Date:** 2026-09-24
- **Ticket:** [#106](https://github.com/jordanfelle/nicti/issues/106) Research: redb (pure-Rust
  embedded KV store) as a catalog engine candidate

## Context

ADR-0009 evaluated Turso (a pure-Rust SQL engine) and found it not production-ready pre-1.0. #106
asks a related but distinct question: `redb` (`cberner/redb`) is a pure-Rust embedded **KV store**,
not a SQL engine — architecturally the closest thing in this comparison to LMDB (ADR-0008: no query
planner, ordered byte-keyed B-tree, hand-maintained secondary indexes) but without LMDB's C core or
its own hard-gate-3 blocker. Worth its own hard-gate + measured pass for the same reason Turso
earned one: it's the only other candidate matching ADR-0001's stated preference (memory safety, no
C/C++ core) that hadn't been measured yet.

## Decision rule

Same hard-gate + measured-gate rule as ADR-0008/0009, against the same `spikes/den/` `Workload`
trait, generator, and 600k/2M synthetic scale.

## Decision

**Not adopted.** All three of hard gates 1/2/5 pass with strong, verifiable evidence — arguably the
strongest of any candidate in this comparison (see Measured results). Gate 3 (crash-safety) is left
**inconclusive** via this harness, for a reason now well-understood and distinct from both prior
inconclusive cases (LMDB, Turso) — see below. The decision rests on the **measured gates**: two
query shapes miss the 2M budget outright, one badly (filename search at ~5x the budget), both
root-caused to a real, documented architectural cost (redb's per-page checksum verification) rather
than a fixable indexing gap. Combined with `redb_engine.rs`'s own hand-maintained-secondary-index
engineering cost (the same real cost ADR-0008 charged against LMDB) and no native online-backup API
at all (a gap neither LMDB nor Turso nor the two SQL engines have), `redb` is not a better fit for
#22 than SQLite today.

## Measured results

**Hard gates:**

| Gate | Result |
|---|---|
| 1. Windows build | ✅ Confirmed via `cberner/redb`'s own `.github/workflows/ci.yml`: a real `windows-latest` job in its OS matrix (alongside `ubuntu-latest`, `macos-latest`, `ubuntu-24.04-arm`), running `cargo fmt --all -- --check`, `cargo clippy --all --all-targets -- -Dwarnings`, and `just test_all_no_sandbox` (its own real test suite) on that runner — not just a doc claim, matching the strength of evidence ADR-0009 found for Turso. |
| 2. License | ✅ `MIT OR Apache-2.0`, confirmed from crates.io's version-level API response for `redb` 4.3.0 (the crate's own `Cargo.toml` sets `license.workspace = true`, resolving to this dual license) — already on `deny.toml`'s allowlist, no edit needed. |
| 3. Crash-safety | ⚠️ **Inconclusive — but for a different, better-understood reason than LMDB or Turso.** `den crash --engine redb --iterations 20` reported 20/20 reopen failures: `Error: Database already open. Cannot acquire lock.` Traced this to source (`src/tree_store/page_store/file_backend/range_lock.rs`), not left as a guess: redb's `Database::create`/`open` take an OS-level advisory byte-range lock (`libc::flock`/fcntl-style range locks on Linux/macOS, the Windows equivalent on that platform) on specific byte offsets of the database file itself — a real, standard mechanism, the same *category* as the POSIX `fcntl` lock ADR-0009 found in `turso_core`, and **not** a process-wide static registry the way `heed`/`liblmdb`'s guard is (ADR-0008's hard-gate-3 finding). Confirmed the distinction directly with a follow-up experiment (mirroring ADR-0009's own methodology): opening path A, `mem::forget`ing the handle, then opening a **different**, never-before-touched path B in the same process succeeds cleanly — only reopening path A itself (the one whose fd was leaked) fails. This proves the lock is scoped to the specific leaked file descriptor, not global. Since `mem::forget` (unlike a real `SIGKILL`) never closes that fd, the OS-level lock is held for the rest of this harness process's life — this in-process crash-simulation technique structurally cannot get past the reopen step to actually exercise redb's own transactional recovery, for the same class of reason ADR-0008/0009 already flagged for the fd-scoped locks SQLite/DuckDB/Turso all use. **This should be read as "harness limitation, mechanism now understood," not as a failing or passing result** — a real fork+exec+SIGKILL harness remains the only way to actually settle it, per ADR-0008/0009's own open follow-up. |
| 4. Online backup | ⚠️ `redb` has **no** dedicated online-backup API at all — unlike SQLite's `VACUUM INTO`, DuckDB's `EXPORT DATABASE`, or LMDB's `env.copy_to_path`, there is nothing to call. `redb_engine.rs::backup()` does a plain `std::fs::copy` of the single database file with a read transaction held open across the copy (so redb's own MVCC page allocator can't reclaim the snapshot's pages mid-copy), which is a real but partial safety property — it gives no coordination with a concurrent writer the way a purpose-built backup call would. Not measured under a concurrent writer here, same scope limit as every other engine in this spike (see ADR-0008's own two-honest-scope-limits note), but this is a real, additional gap specific to `redb`, not just an unmeasured edge of an existing API. |
| 5. Maintained | ✅ Actively maintained, more clearly than any prior candidate: `v4.3.0` (well past 1.0, unlike Turso's `0.8.0-pre.12`), last repository push the same day as this evaluation, regular tagged releases (`v4.0.0`→`v4.3.0` across 2026), 4,805 GitHub stars, only 7 open issues, not archived. **Zero dependencies** (confirmed via `cargo tree -i redb`) — the smallest, simplest dependency footprint of any candidate across ADR-0008/0009/0010, and no async runtime of any kind (no `tokio`/etc. anywhere in its graph), so `Workload::prepare_for_forget` stays a correct no-op for this engine — confirmed, not assumed. |

**Measured gates, 600k assets:**

| Query | Budget (2M) | redb p50/p95 |
|---|---|---|
| Faceted filter + facet counts | < 100ms | 0.085 / 0.114 ms |
| Sort by date, first 500 | < 100ms | 0.030 / 0.045 ms |
| Folder-subtree count | < 100ms | 10.3 / 10.9 ms |
| Keyword-subtree query | < 100ms | 0.013 / 0.014 ms |
| Range query | < 100ms | 45.6 / 51.5 ms |
| Filename substring search | < 100ms | **105.8 / 118.2 ms — already over budget at 600k** |
| Cold open | < 2s | 0.39 ms |
| Single rating write | ≤ 5ms | 0.065 / 0.085 ms |
| 100-write rate burst (informative) | — | 0.316 / 0.418 ms |
| Bulk ingest, 600k rows (informative) | — | 8.13 s |
| Online backup (informative, not gated; see gate-4 caveat above) | — | 0.299 / 0.322 s |

Unlike every prior candidate in ADR-0008/0009 ("all three clear every gate at 600k"), `redb` already
misses one gate at 600k scale — filename search — worth stating plainly rather than only reporting
the 2M table below.

**Measured gates, 2M assets (the planning-horizon scale the budget is stated against):**

| Query | Budget | redb p50/p95 |
|---|---|---|
| Faceted filter + facet counts | < 100ms | 0.063 / 0.162 ms ✅ (best-in-class — see note below) |
| Sort by date, first 500 | < 100ms | 0.024 / 0.024 ms ✅ |
| Folder-subtree count | < 100ms | 29.8 / 31.1 ms ✅ |
| Keyword-subtree query | < 100ms | 0.011 / 0.011 ms ✅ |
| Range query | < 100ms | **132.1 / 152.3 ms ⛔** (~1.5x budget) |
| Filename substring search | < 100ms | **303.3 / 507.8 ms ⛔** (~5x budget) |
| Cold open | < 2s | 0.38 ms ✅ |
| Single rating write | ≤ 5ms | 0.111 / 0.482 ms ✅ |
| 100-write rate burst (informative) | — | 0.379 / 0.407 ms |
| Bulk ingest, 2M rows (informative) | — | 37.6 s |
| Online backup (informative, not gated; see gate-4 caveat above) | — | 1.70 / 1.87 s |

**Root cause for the two misses, verified rather than guessed (per this spike's own standing
rule):** both `range_query` and `filename_search` are the two query shapes in this workload that
touch the *most* pages per call — `range_query` walks a `by_rating` index range and then does one
`assets` lookup per candidate row to post-filter iso/date (same honest shape as `lmdb.rs`'s own
version, see that module's doc comment); `filename_search` is an unavoidable full-table scan (a
leading-wildcard substring match, unindexable in any of the five engines evaluated across
ADR-0008/0009/0010). Both are page-read-heavy, not index-shaped. Checked redb's own design
documentation rather than assuming: `docs/design.md` states plainly that "all data is checksumed
when written, using a non-cryptographic Merkle tree with XXH3_128" — every B-tree page carries a
`child page checksum` entry, verified as part of reading it, as the load-bearing mechanism behind
redb's own 1-phase-plus-checksum (1PC+C) durable-commit design. LMDB and the two SQL engines don't
pay this specific per-page-read cost (LMDB doesn't checksum backing pages at all; SQLite/DuckDB pay
their own different overheads and did not show this specific pattern in ADR-0008). This isn't just
inferred from one benchmark run here: `redb`'s own upstream README publishes a benchmark table
(`cberner/redb`, "Benchmarks" section, its own hardware) showing **redb is consistently ~1.5–2x
slower than LMDB specifically on random-read and random-range-read workloads** (e.g. "random range
reads": redb 1174ms vs LMDB 565ms; "random reads": redb 1138ms vs LMDB 637ms) while being *faster*
than LMDB on `len()` and individual writes — a pattern consistent with per-page checksum
verification cost being paid specifically when pages are actually read, not on metadata-only or
write-path operations. This spike's own numbers (redb's `filename_search` at 2M running ~2.5x
slower than LMDB's equivalent full-scan implementation, 508ms vs LMDB's 201ms in ADR-0008) land in
the same direction and rough magnitude as that independent, upstream comparison — two lines of
evidence agreeing, not one guess standing alone. A "missing index" explanation was considered and
rejected: both queries already use the same indexing strategy as `lmdb.rs` (rating-range index +
post-filter; full scan for substring search respectively), so there is no missing index to add —
this is a real, structural per-page-read cost trade redb makes for its checksummed-durability
guarantee, not an oversight in this backend.

**`faceted_filter`'s apparent best-in-class number needs the same "why" scrutiny, not just a
celebratory read:** it beats every other engine (including LMDB's 0.035/0.076ms) because this
query's benchmark parameters (`model="NIKON Z 8"`, `min_rating=3`, `keyword_prefix=BENCH_LEAF_KEYWORD`)
resolve through the `by_keyword` index to a **very small** candidate set (a single named event,
per `gen::BENCH_LEAF_KEYWORD`'s own doc comment) — this is the "genuinely rare hierarchy leaf" case
`bin/den.rs`'s own comment describes, and touches few enough pages that the checksum-verification
cost is negligible regardless of engine. It is not evidence that `redb`'s checksum cost is fictional
or engine-position-dependent; it's evidence that the cost scales with pages actually touched, fully
consistent with the range-query/filename-search findings above, not in tension with them.

## Options considered

| Option | Verdict |
|---|---|
| `redb` (pure-Rust embedded KV store) | **Rejected.** Strongest Windows-build/license/maintenance evidence of any candidate evaluated so far, and a genuinely well-understood (not murky) crash-safety inconclusive result. Two measured query shapes miss the 2M budget — one by ~1.5x, one by ~5x — root-caused to a real, documented per-page-checksum cost verified against redb's own design docs and its own upstream LMDB-comparison benchmarks, not a fixable indexing gap. No native online-backup API, and the same hand-maintained-secondary-index engineering cost ADR-0008 already charged against LMDB. |
| `LMDB` (`heed`) | Unchanged from ADR-0008: best raw numbers of any KV-shaped candidate, rejected on the same unmeasurable crash-safety hard gate — still the pending "revisit if a real fork+exec+SIGKILL harness exists" candidate. |

ADR-0008's decision is unchanged: **SQLite remains chosen, DuckDB remains the proven fallback.**

## Consequences

- **The pure-Rust angle keeps not being the deciding factor, twice now** (Turso in ADR-0009, `redb`
  here) — worth naming as a pattern, not just two unrelated misses: both times, a genuine
  architectural property specific to the pure-Rust candidate (Turso's WAL-checkpoint behavior;
  redb's per-page checksum cost) is what actually decided the outcome, not maturity alone.
  `redb` is by far the more production-ready of the two (1.0-plus, zero dependencies, real Windows
  CI) — this is a much closer call than Turso was, and worth remembering as the strongest
  pure-Rust catalog candidate on record if SQLite's own facet-query ceiling (ADR-0008) ever forces
  a real re-decision. Revisit if redb ever ships an optional checksum-verification-off read mode,
  or if a real fork+exec+SIGKILL harness resolves its crash-safety question favorably.
- **#22 should not spend further design effort accommodating `redb`.** Proceed on SQLite per
  ADR-0008.
- **The fork+exec+SIGKILL harness gap, first flagged in ADR-0008 and reiterated in ADR-0009, is now
  a three-for-three pattern** (LMDB, Turso, redb all left this pass's crash-safety gate
  unresolved, for three different underlying mechanisms). Building it is looking less like an
  optional nicety with each new KV-shaped or pure-Rust candidate evaluated this way, and more like
  standing infrastructure this project's database research should just have. Still out of scope
  for this pass; worth its own ticket if a sixth candidate ever needs evaluating.
- **`tests/cross_engine.rs` now covers five engines**, unchanged in scope from ADR-0009's own
  caveat about what it doesn't exercise (`crash_mid_ingest`, `rate_burst`, `backup()` still aren't
  asserted there for any engine) — still worth widening in a future pass, still not specific to
  this ADR's own conclusion.

## Spike: `spikes/den/src/redb_engine.rs`

Implements the same `Workload` trait as the other four engines, behind the `redb` Cargo feature
(default-off, matching `lmdb`/`turso`). Named `redb_engine`, not `redb`, to avoid shadowing the
external crate. Reuses `lmdb.rs`'s exact design pattern: byte-encoded composite keys
(`prefix || 0x00 || big-endian id`) in one dedicated table per indexed dimension, since `redb` (like
LMDB) has no query planner — every query pattern needs its own hand-maintained secondary index.
`redb`'s `Key` trait implementation for `&[u8]` does ordinary byte-lexicographic comparison, which
is exactly what `composite_key` assumes; `redb_matches_shared_workload` in `tests/cross_engine.rs`
is what actually confirms this (passed on the first attempt against the real API, unlike some
engines in this spike's history), not just this doc comment's assertion.

One real API-shape difference from `lmdb.rs`, in redb's favor: `redb::WriteTransaction` is **not**
lifetime-bound to `Database` (its own docs say so explicitly — a dropped `Database` doesn't
invalidate an in-flight `WriteTransaction`). Unlike `heed`'s `RwTxn<'_>` (which borrows `&Env` and
can't be stashed across a method return without becoming self-referential — the exact reason
`lmdb.rs::crash_mid_ingest` had to fall back to a plain `bulk_ingest` call instead of a genuinely
uncommitted transaction), `redb_engine.rs::crash_mid_ingest` **can** hold a genuinely open,
uncommitted `WriteTransaction` as a struct field (`pending_crash_txn`) across its own return, giving
redb the same faithful "half a batch, never committed, never rolled back" simulation SQLite/DuckDB
already got, not the compromise LMDB had to accept. This didn't end up mattering for the final
crash-safety verdict (the reopen step itself is what fails, before the transaction's own fate is
ever checked), but it's a real, worth-noting difference in what this harness could exercise.

`integrity_check()` here is a manual full-table deserialize scan (identical in spirit to
`lmdb.rs`'s own), not a call to redb's own `Database::check_integrity` — that method takes
`&mut self` and attempts a repair, which doesn't fit this trait's `&self` signature without
wrapping `Database` in interior mutability purely for this one call, not attempted in this pass.
Same honest-scope-limit pattern as DuckDB's shallow check in ADR-0008: the ✅ this backend earns on
the crash-safety hard gate's *check* half is real per-row deserialize validation (stronger than
DuckDB's `SELECT COUNT(*)` probe, weaker than redb's own available page-level repair scan) — called
out explicitly rather than left implicit.

A throwaway `#[cfg(test)]` experiment (`leaking_one_path_does_not_block_a_different_path`) was
written to test the process-wide-vs-path-scoped lock-mechanism theory directly, confirmed it, and
was deleted before this PR — its result is transcribed into the crash-safety row above, not kept as
permanent test coverage (it tested a harness/upstream-library property, not this crate's own
behavior).
