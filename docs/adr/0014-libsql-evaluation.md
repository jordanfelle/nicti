# ADR-0014: libSQL, evaluated for the catalog store — not adopted for v1

- **Status:** Rejected for v1 (not a rejection of the embedded-replica idea itself — see
  Consequences; revisit specifically when #64 becomes active)
- **Date:** 2026-09-24
- **Ticket:** [#113](https://github.com/jordanfelle/nicti/issues/113) Research: libSQL (real SQLite
  C-source fork with embedded replicas) as a catalog engine candidate

## Context

ADR-0008 chose SQLite (`rusqlite`) for v1. #102 (Turso Database, ADR-0009) and #106 (`redb`,
ADR-0010) were both evaluated and rejected; #107 (ADR-0012) separately re-confirmed SQLite over
DuckDB for the specific append-only/burst-compacted history-log shape ADR-0002 requires. #113 asks
a related but distinct question from either of those: **libSQL** (`tursodatabase/libsql`) is an
actual fork of SQLite's own C source — not a from-scratch rewrite like Turso Database — with a
maintained Rust crate (`libsql` on crates.io) adding **embedded replicas** (built-in offline-first
sync) over plain SQLite. This could plausibly reduce or replace the custom sync layer already
anticipated for v2's #64 (multi-machine catalog), instead of building sync from scratch on
ADR-0002's history log. ADR-0009 itself declined to evaluate libSQL separately at the time
("nothing about it would change ADR-0008's own SQLite numbers"), which #113 explicitly revisits
rather than assumes still holds.

**Real caveat, not disqualifying but worth tracking** (named in #113 itself): the company's active
development attention has visibly shifted to Turso Database (the Rust rewrite) since libSQL's
introduction — its own forward maintenance trajectory is less certain than SQLite mainline's. This
needed to be measured, not assumed either way (see hard gate 4 below).

## Decision rule (stated before measuring, same standard as #102/#106/#107)

Reuse `spikes/den`'s shared `Workload` trait so this candidate is measured identically to every
prior one. Hard gates:

1. Builds and passes smoke tests on `x86_64-pc-windows-msvc` in CI.
2. License allowed under ADR-0003 (already is — MIT).
3. Survives `kill -9` during a write loop: reopens cleanly, integrity check passes.
4. Actively maintained: a release within the last 6 months and a real Rust API.

Measured gates at 600k/2M scale, against `docs/benchmarks.md`'s Library targets (same as
ADR-0008/0009/0010): faceted filter/search/sort < 100ms p95; cold start < 2s; single write ≤5ms.

**Additional question specific to this candidate** (#113's own framing): does the embedded-replica
feature's sync/write path add meaningful overhead over plain SQLite for the common case (no active
replica configured)? If libSQL's baseline performance isn't within noise of vanilla SQLite, that's
a real cost baked into every write even for users who never use the sync feature — measured
directly below, not assumed either way.

## Decision

**Not adopted for v1.** Every hard gate passes, including — uniquely among every KV-shaped or
pure-Rust candidate this project has evaluated (LMDB/ADR-0008, Turso/ADR-0009, `redb`/ADR-0010,
all left crash-safety **inconclusive**) — gate 3 (crash-safety), which libSQL **passes cleanly**:
0/20 reopen failures, real `PRAGMA integrity_check` validation, the same result plain SQLite itself
gets. This is expected, not a coincidence: libSQL is a genuine fork of SQLite's own C locking code,
not a reimplementation with its own novel lock semantics. The measured gates all clear too, at the
same margins as plain SQLite (same known faceted-filter miss at 2M, inherited directly from being
the same SQL engine and schema, not a new libSQL-specific failure). But #113's own specific
question is where the real, negative-for-adoption finding is: **the embedded-replica feature's own
code path is never invoked when unconfigured (confirmed at the source level, not assumed), but a
real, consistent per-operation overhead exists anyway** — roughly 1.5–4x libSQL over plain
`rusqlite` on every measured op except the raw single-write gate (which ties), plus a ~19-23%
slower bulk ingest and a ~5.5x slower (but still trivial, 4ms) cold open. This is attributable to
the crate's `async`-wrapped API (every call blocks on a `tokio::runtime::Runtime`, the same
architectural shape ADR-0009 already measured overhead from in Turso Database, just smaller here),
not to replication code actually running. Combined with a ~5x larger dependency graph pulled in by
the crate's own default features (`tonic`/`tower`/`hyper`/`h2`, entirely unused by this engine's own
code path) and no compelling reason to pay either cost for v1 (a single-machine catalog with no
active sync need yet), libSQL is not a better fit for #22 than plain SQLite today. It **is** the
strongest candidate on record for #64 specifically, once that ticket is real — see Consequences.

## Measured results

**Hard gates:**

| Gate | Result |
|---|---|
| 1. Windows build | ✅ (evidence, pending this PR's own CI run) `spikes/den` builds and links cleanly on this session's Linux sandbox with `libsql` enabled (`cargo build`/`cargo test -p den --no-default-features --features libsql,duckdb,lmdb,turso`), and `libsql-ffi-0.9.30`'s own `build.rs` was read directly (not assumed): unlike pglite-rs's disqualifying unconditional Unix-only linker flag (ADR-0008), libSQL's build script has no platform-exclusive code path outside features this evaluation doesn't enable (`bundled-sqlcipher`'s Windows-specific OpenSSL lib-name branch is the only `is_windows` logic in the file, and it's gated behind a feature not used here) — it compiles its bundled `sqlite3.c` fork via the standard `cc` crate, the same mechanism `libsqlite3-sys` itself uses for plain SQLite. This PR extends `.github/workflows/ci.yml`'s `build-windows` job with a real `cargo test -p den` invocation (see the Spike section for why it's a *separate* command from the other engines, not folded into the existing one) — the authoritative confirmation is that CI run, not this local Linux evidence alone. |
| 2. License | ✅ MIT — confirmed twice, independently: crates.io's version-level API response for `libsql` (every version back through 0.9.x, including the crate's newest 0.10.0-pre.4) reports `"license":"MIT"`, and the GitHub repo (`tursodatabase/libsql`) reports the same via its own `license` API field. Already on `deny.toml`'s allowlist (MIT is the very first entry) — no edit needed, confirmed by `cargo deny --workspace --all-features check licenses` passing clean (see below). |
| 3. Crash-safety | ✅ **0/20 reopen failures** — `den crash --engine libsql --iterations 20`, using the identical `crash_mid_ingest` (open a transaction, insert half a batch, never `COMMIT`, `mem::forget` the handle) + `PRAGMA integrity_check` methodology every prior ADR in this series uses. This is the first KV-shaped-or-pure-Rust-adjacent candidate since ADR-0008's original SQLite/DuckDB pass to actually clear this gate rather than leave it inconclusive — expected, given libSQL forks SQLite's own C-level file-locking code rather than reimplementing it (Turso Database's from-scratch rewrite hit a POSIX `fcntl` lock whose interaction with this harness's `mem::forget` technique was never fully resolved, per ADR-0009; `redb`'s own OS-level byte-range lock hit the same structural limitation, per ADR-0010). |
| 4. Maintained | ✅ Real, recent evidence on both fronts #113 asked to distinguish. **Crate release recency:** `libsql` 0.9.30 (the current stable/`max_stable_version`) published 2026-03-19; a newer 0.10.0-pre.4 pre-release published 2026-06-02 — about 3.7 months before this evaluation, comfortably inside the 6-month bar (Turso's own evaluated version in ADR-0009 was a pre-release too, so a pre-release counting here is consistent with that precedent). **Repo activity:** `tursodatabase/libsql` was pushed to 8 days before this evaluation (2026-09-16), 17,235 stars, 533 forks, not archived — this is a real, currently-active repo, not a frozen one, despite the org's newer attention on Turso Database. **Real API, not a stub:** confirmed by reading the actual crate source (`~/.cargo/registry/src/.../libsql-0.9.30/src/`), not just its docs — `Connection::execute`/`query`/`prepare`, `Rows::next`, `Row::get`/`get_value` are all real, working `async fn`s backed by a genuine SQLite connection, exercised end-to-end by this ADR's own `libsql_engine.rs` and its passing `cross_engine.rs` correctness test. |

**Measured gates, 600k assets** (p50/p95 unless noted; SQLite column is this session's own
`rusqlite` baseline run back-to-back with libSQL on the same box, not reused from ADR-0008, so the
#113-specific SQLite-vs-libSQL comparison is apples-to-apples on identical hardware/session):

| Query | Budget | SQLite p50/p95 | libSQL p50/p95 |
|---|---|---|---|
| Faceted filter + facet counts | < 100ms | 47.2 / 49.9 ms ✅ | 50.6 / 52.9 ms ✅ |
| Sort by date, first 500 | < 100ms | 0.018 / 0.020 ms ✅ | 0.077 / 0.096 ms ✅ |
| Folder-subtree count | < 100ms | 2.02 / 2.23 ms ✅ | 1.99 / 2.03 ms ✅ |
| Keyword-subtree query | < 100ms | 0.039 / 0.083 ms ✅ | 0.066 / 0.118 ms ✅ |
| Range query | < 100ms | 1.11 / 1.21 ms ✅ | 1.89 / 1.93 ms ✅ |
| Filename substring search | < 100ms | 25.5 / 25.5 ms ✅ | 26.1 / 26.9 ms ✅ |
| Cold open | < 2s | 0.74 ms ✅ | 4.06 ms ✅ |
| Single rating write | ≤ 5ms | 0.0122 / 0.0176 ms ✅ | 0.0108 / 0.0165 ms ✅ (ties, within noise) |
| 100-write rate burst (informative) | — | 0.283 / 0.334 ms | 0.353 / 0.376 ms |
| Tag 10k assets (informative) | — | 8.09 / 11.07 ms | 18.67 / 20.08 ms |
| Bulk ingest, 600k rows (informative) | — | 11.00 s | 13.12 s |
| Online backup (informative) | — | 483 / 494 ms | 437 / 615 ms |

**Measured gates, 2M assets** (the planning-horizon scale the budget is stated against):

| Query | Budget | SQLite p50/p95 | libSQL p50/p95 |
|---|---|---|---|
| Faceted filter + facet counts | < 100ms | **144.9 / 162.5 ms ⛔** (known ADR-0008 gap) | **161.2 / 165.6 ms ⛔** (same gap, inherited) |
| Sort by date, first 500 | < 100ms | 0.019 / 0.045 ms ✅ | 0.076 / 0.106 ms ✅ |
| Folder-subtree count | < 100ms | 7.81 / 8.30 ms ✅ | 6.61 / 7.12 ms ✅ |
| Keyword-subtree query | < 100ms | 0.038 / 0.086 ms ✅ | 0.089 / 0.118 ms ✅ |
| Range query | < 100ms | 4.68 / 4.81 ms ✅ | 6.81 / 7.07 ms ✅ |
| Filename substring search | < 100ms | 77.0 / 78.2 ms ✅ (thin) | 79.3 / 79.9 ms ✅ (thin) |
| Cold open | < 2s | 0.76 ms ✅ | 4.15 ms ✅ |
| Single rating write | ≤ 5ms | 0.0126 / 0.0212 ms ✅ | 0.0120 / 0.0162 ms ✅ (ties) |
| 100-write rate burst (informative) | — | 0.395 / 0.506 ms | 0.422 / 0.533 ms |
| Tag 10k assets (informative) | — | 8.17 / 10.02 ms | 16.91 / 18.89 ms |
| Bulk ingest, 2M rows (informative) | — | 47.97 s | 58.96 s |
| Online backup (informative) | — | 1548 / 1686 ms | 1328 / 1382 ms |

The one gate both engines miss (faceted-filter-with-counts at 2M) is not a new libSQL finding —
it's the exact same gap ADR-0008 already identified and ADR-0011 already mitigated (a
trigger-maintained facet table); nothing about libSQL's own schema or query plan differs from
`sqlite.rs`'s here (`libsql_engine.rs` deliberately runs byte-identical SQL text — see the Spike
section), so ADR-0011's mitigation would apply equally to a libSQL-backed store if one were ever
built.

## #113's specific question: is there a baked-in embedded-replica cost even when unused?

**Runtime cost: no — confirmed at the source level, not assumed.** `libsql-0.9.30`'s own
`src/database.rs` models the open database as a `DbType` enum with five variants (`Memory`,
`File`, `Sync`, `Offline`, `Remote`), gated behind the `core`/`replication`/`sync`/`remote` Cargo
features respectively. `libsql_engine.rs` opens every store with `Builder::new_local(path).build()`
— reading `src/database/builder.rs`'s own `impl Builder<Local>` confirms this constructs exactly
one variant, `DbType::File`, and nothing else; the `Sync`/`Offline`/`Remote` variants (and all their
associated code) are never constructed or touched by this engine's own code path. A repo-wide grep
for `tokio::spawn` across `database.rs` and `src/local/` (the plain-local-file code) turns up
nothing — opening a local file starts no background replication/sync task, even though the crate
was built with its full **default** feature set (`core`, `replication`, `remote`, `sync`, `tls` —
deliberately not trimmed down to a minimal build, since that's what a real caller would actually
depend on). This is the direct evidence for the "is the feature's code path actually invoked"
question, and the answer is no.

**But there IS a real, measured overhead — just not from replication.** Every measured op above
except the raw single-write gate (which ties within noise) runs 1.5–4x slower on libSQL than on
plain `rusqlite`, and bulk ingest is ~19–23% slower. The write gate tying is itself informative: a
single bare `UPDATE ... WHERE id = ?` pays almost the same cost on both engines, which is
consistent with the overhead being **per-call async-dispatch cost** (every `Workload` method here
blocks a `tokio::runtime::Runtime` on an `async fn` that itself awaits a `prepare()` + `query()` /
`execute()` call chain — the identical architectural shape `turso_engine.rs` already measured
overhead from in ADR-0009, just smaller in magnitude here since libSQL's actual query execution is
real SQLite C, not a from-scratch Rust query engine) rather than from replication/sync code that,
per the paragraph above, never runs at all. Queries that make more per-call round trips inside one
`Workload` method (`tag_keyword_10k`'s 10,000 individual `INSERT`s, `sort_by_date_page`'s and
`keyword_subtree_query`'s smaller absolute times where a fixed per-call async overhead dominates a
tiny query cost) show the largest relative gap; `cold_open`'s ~5.5x gap is a similar fixed-async-
setup-cost story, still trivially within the 2s budget in absolute terms (4ms).

**Compile-time cost is real and separate from both of the above.** `cargo tree -e normal -p den
--features libsql` resolves 433 dependency-tree lines vs. plain `sqlite`'s 85 — about 5x — and
concretely includes `tonic`/`tower`/`hyper`/`h2` (the gRPC/HTTP stack backing `remote`/
`replication`/`sync`), none of which this engine's own code ever calls. This is a genuine
dependency-graph/binary-size/compile-time/supply-chain-surface cost baked into the crate's default
features, independent of (and not contradicted by) the runtime-overhead finding above — reported
here as its own line item per #113's own instruction not to just assume "should be the same since
it's a fork."

**One structural finding, not directly part of #113's own question but discovered while measuring
it:** `libsql`'s bundled `libsql-ffi` (its own fork of `sqlite3.c`) and `rusqlite`'s bundled
`libsqlite3-sys` (plain SQLite's `sqlite3.c`) cannot be linked into the same binary — both
statically define the full `sqlite3_*` C symbol set, and doing so is a real
`multiple definition of sqlite3_prepare_v3` (etc.) link error, reproduced directly, not assumed.
This has no bearing on a real production build (Nicti would depend on exactly one SQL engine, not
both, in any real binary), but it does mean this spike's own comparison harness — and this repo's
CI — needed a real fix; see the Spike section.

## Options considered

| Option | Verdict |
|---|---|
| libSQL (`tursodatabase/libsql`) | **Rejected for v1, not for good.** Passes every hard gate, including the first clean crash-safety pass of any KV-shaped-or-pure-Rust-adjacent candidate in this series. Measured gates match plain SQLite's own margins exactly (same known 2M faceted-filter gap, already mitigated by ADR-0011). #113's own question resolves cleanly: no runtime cost from the embedded-replica feature when unconfigured (confirmed at the source level), but a real, consistent 1.5–4x per-op async-dispatch overhead and a ~5x larger dependency graph exist regardless — real costs to pay for a feature (embedded-replica sync) v1 doesn't need yet. |
| SQLite (`rusqlite`, status quo) | **Kept**, per ADR-0008/ADR-0012. Nothing in this evaluation changes that decision — libSQL doesn't out-measure plain SQLite on any gate that matters for a single-machine v1 catalog. |

## Consequences

- **#22 should not adopt libSQL for v1.** Proceed on SQLite per ADR-0008/ADR-0012, unchanged.
- **libSQL is the leading candidate to revisit specifically when #64 (multi-machine catalog)
  becomes an active ticket, not a "maybe someday" note.** It is the only SQL-compatible engine
  evaluated across ADR-0008/0009/0010/0012/this ADR that both (a) inherits real SQLite's own
  crash-safety/reliability track record (confirmed here, cleanly, unlike every other alternative
  engine) and (b) ships a built-in offline-first sync mechanism (embedded replicas) that could
  plausibly replace hand-rolling sync on top of ADR-0002's history log. Turso Database (the
  from-scratch rewrite with the same org's newer engineering attention) was already rejected in
  ADR-0009 for reasons unrelated to sync (WAL bloat, missing prefix-scan optimization) and doesn't
  change this ranking. When #64 is scoped, benchmark libSQL's actual embedded-replica sync
  round-trip cost specifically (not measured here — #113's own scope was the baseline/unconfigured
  case only) before committing to it over a custom sync layer.
- **The org-attention caveat #113 named up front is real but not disqualifying today**: the repo
  is genuinely active (pushed 8 days before this evaluation) despite Turso Database being the
  newer focus. Re-check this at the time #64 is actually scoped, since maintenance trajectories can
  shift over a period of months to years, not just check it once now and assume it holds
  indefinitely.
- **The bundled-sqlite3-symbol-collision finding is now a documented, load-bearing fact about this
  spike's own tooling**, not a production concern: `spikes/den`'s CI can no longer run every SQL
  engine through one `--all-features` invocation the way it could before this candidate was added
  (see the Spike section for the concrete CI/feature-gating fix this required) — worth remembering
  if a future SQL-engine candidate also bundles its own bundled sqlite3.c fork.
- **`tests/cross_engine.rs` now covers six engines**, unchanged in scope from ADR-0010's own
  caveat about what it doesn't exercise (`crash_mid_ingest`, `rate_burst`, `backup()` still aren't
  asserted there for any engine) — still worth widening in a future pass, still not specific to
  this ADR's own conclusion.

## Spike: `spikes/den/src/libsql_engine.rs`

Implements the same `Workload` trait as every other candidate, behind a new `libsql` Cargo feature
(default-off, matching `turso`/`redb`). Named `libsql_engine`, not `libsql`, to avoid shadowing the
external crate (same convention as `turso_engine`/`redb_engine`). The crate's own API is `async`
(not `rusqlite`-shaped) — every `Workload` method blocks on a per-engine
`tokio::runtime::Runtime`, the identical pattern `turso_engine.rs` established, since both crates
expose the same shape of async API. Every query runs byte-identical SQL text to `sqlite.rs`
(including its `GLOB`-not-`LIKE` prefix-scan convention), so any measured difference between the
two engines in this ADR is attributable to the engine, not to a different query shape.

**Real, structural finding that required fixing code beyond `libsql_engine.rs` itself**: `libsql`'s
bundled `libsql-ffi` and `rusqlite`'s bundled `libsqlite3-sys` both statically compile a full copy
of real SQLite's C symbols, and linking both into the same binary fails at the final link step
(`multiple definition of sqlite3_prepare_v3`, etc. — reproduced directly). This meant `libsql`
could never be exercised in the same `den` binary as `sqlite`, which surfaced two pre-existing gaps
in this crate that had gone unnoticed until a candidate that couldn't coexist with `sqlite` was
added:

1. `bin/den.rs`'s `Cmd::FacetBench` subcommand (and `tests/facet_cache.rs`'s two tests) imported
   `den::facet_cache_trigger`/`den::facet_cache_duckdb` **unconditionally**, even though those
   modules are themselves gated behind `#[cfg(feature = "sqlite")]`/
   `#[cfg(all(feature = "sqlite", feature = "duckdb"))]` in `lib.rs` — meaning `den`'s bin (and
   therefore any `cargo build`/`cargo test -p den`) silently required `sqlite` (and `duckdb` for
   the cache variant) to compile **at all**, regardless of which engine was actually being
   exercised. Fixed by adding the same `#[cfg(...)]` gates to the CLI enum variant, its match arms,
   and the benchmark functions themselves (`bin/den.rs`), and to both test functions plus the
   shared `fixture()` helper (`tests/facet_cache.rs`) — this is what actually made a `libsql`-only
   build possible at all.
2. `.github/workflows/ci.yml`'s `cargo test --workspace --all-targets --all-features` job (used
   for the whole repo, not just `den`) would activate `den`'s `sqlite` and `libsql` features
   together in one feature resolution — unlike `cargo clippy --all-features` (confirmed to never
   reach the link step, so it stayed fine), `cargo test` genuinely links `den`'s test binaries and
   hit the exact collision above. Fixed by adding `--exclude den` to that job's command and giving
   `den` its own two feature-scoped `cargo test` invocations instead (`sqlite,duckdb,lmdb,turso,
   redb` and, separately, `--no-default-features --features libsql,duckdb,lmdb,turso`) — the same
   pattern the `build-windows` job's own `-p den` line already used, now duplicated for the
   Linux `test` job for the same underlying reason. Confirmed both the exclusion and both
   replacement commands actually pass, not just that they parse.

`tests/cross_engine.rs` includes `libsql_matches_shared_workload`, which passes on the first
attempt against the real API — the single-element-tuple `IntoParams` gap (`libsql` doesn't
implement `IntoParams` for a bare `(T,)` the way it does for `[T; 1]`, unlike `turso`'s more
permissive tuple impls) was caught by the compiler itself before any test ran, not a runtime
surprise; fixed by using single-element arrays (`[pattern]`) instead of one-element tuples for
every single-parameter query in this file.

`backup()` uses `VACUUM INTO` directly, unlike `turso_engine.rs`'s try-then-fall-back-to-file-copy
approach — libSQL forks the real sqlite3.c, so there was no reason to expect (and this ADR's own
measured numbers confirm) `VACUUM INTO` behaves identically to `sqlite.rs`'s own online, no-lock
backup, not Turso Database's still-experimental reimplementation of it.

No new `docs/licensing.md` entry was needed beyond noting libSQL's license here (MIT, already
allowed) — `cargo deny --workspace --all-features check licenses` passes clean with libSQL's full
dependency tree resolved, the same pre-existing `cfg_block` (Turso-only) warning as every prior
ADR in this series, nothing new from `libsql`.
