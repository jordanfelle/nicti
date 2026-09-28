# ADR-0025: Continuous catalog backup + integrity checks (Nine Lives)

- **Status:** Accepted
- **Date:** 2026-09-27
- **Ticket:** [#25](https://github.com/jordanfelle/nicti/issues/25) Build: continuous catalog
  backup + integrity checks

## Context

`docs/benchmarks.md`'s Maintenance target: "no optimize catalog; continuous crash-safe backup" —
a direct reaction to Lightroom Classic's slow "back up catalog + optimize" prompt on close, which
this project's own PRD calls out as real pain. ADR-0067 (catalog database engine) already settled
the *mechanism* while deciding SQLite: `VACUUM INTO`, an online snapshot taken with no lock on the
live store, measured at 2.0/2.5s p50/p95 at 2M rows. What #67 didn't settle was #25's actual build:
when a backup should run, how to avoid stalling the UI for that ~2s, how each copy gets checked
before being trusted, and how many to keep.

`crates/nicti-lair::sqlite::SqliteCatalog` (#22, landed) is a `Mutex<Connection>` — every catalog
query goes through that one lock. Naively running `VACUUM INTO` on it would hold that lock for the
whole snapshot, freezing the Library/Develop views for ~2s at 2M rows. Nothing in the crate stored
the catalog's own file path before this ticket, and there was no integrity-check or retention logic
anywhere.

## Decision

**Nine Lives** (`crates/nicti-lair/src/ninelives.rs` — a cat has nine lives, and a verified backup
is a spare one for the catalog):

1. **Snapshot from a second, independent read-only connection**
   (`SqliteCatalog::open_snapshot_reader`), never the shared `Mutex<Connection>`. WAL mode lets a
   reader run alongside the writer, so the ~2s `VACUUM INTO` never blocks a real catalog query.
   Exercised, not just asserted from WAL's documented semantics:
   `tests/ninelives.rs::snapshot_completes_without_waiting_for_a_slower_concurrent_writer` runs a
   deliberately-paced background writer thread and confirms the snapshot returns well before the
   writer's own bounded wall-clock pace finishes, with neither side erroring or deadlocking. That
   test's own doc comment is explicit about its limit: at this small a row count, a snapshot
   completes quickly regardless of which connection performs it, so this specific timing
   comparison doesn't distinguish "correct" from "accidentally reverted to the shared connection"
   the way it would at the 2M-row scale ADR-0067 actually measured (2.0s/2.5s p50/p95) — it's real
   evidence, not a tautology, but not a substitute for a reference-hardware pass at that scale
   either. An in-memory catalog (tests only, `SqliteCatalog::path()` is `None`) has no file to
   reopen, so it falls back to `SqliteCatalog::vacuum_into_locked`, which does briefly hold the
   lock — acceptable there since nothing else contends for an in-memory test catalog's connection.
2. **Write-then-verify-then-rename.** A snapshot lands at `<stem>.<epoch>.sqlite.partial` first.
   `ninelives::verify` runs a full `PRAGMA integrity_check` on the copy (never the live catalog —
   see step 3) plus a `PRAGMA user_version` match against the live catalog. Only a copy that passes
   both gets renamed to its final `<stem>.<epoch>.sqlite` name (`ninelives::rotate`) and fsynced
   first. A copy that fails verification is deleted; every existing good backup is left untouched.
3. **Check the live catalog too, before snapshotting.** `SqliteCatalog::quick_check` runs `PRAGMA
   quick_check` on the live store first. If it reports a problem, the run stops with
   `BackupOutcome::LiveCorrupt` and never touches a single existing backup — a corrupt live catalog
   must never push a corrupt copy over yesterday's good ones. Nothing runs at process startup
   itself, keeping the `<2s` cold-start target at 2M assets intact.
4. **Retention: newest 9, oldest pruned only after a new one verifies.** `BackupPolicy::keep`
   defaults to 9 (the "nine lives" itself). Pruning happens in `rotate`, strictly after the new
   backup is already renamed in — a failed verification never reaches `rotate`, so a bad copy can
   never cause a good one to be pruned. Default location:
   `<catalog dir>/<catalog file name>-backups/` (`BackupPolicy::for_catalog`); a picker UI for a
   different drive is explicitly out of scope here (future work).
5. **Scheduling ("continuous"): `NineLives::due`**, polled by `nicti-pelt`'s app loop roughly every
   30s (`app.rs::poll_backup`, `BACKUP_POLL_INTERVAL`) — cheap, one directory listing. Due whenever:
   no verified backup exists yet; or the newest one is past `interval_secs` (default 15 minutes)
   **and** either this is the first check since the process started (covers a previous session's
   changes that were committed to the catalog but never backed up before it crashed/closed — see
   `NineLives`'s own doc comment for why `SqliteCatalog::change_counter()`, a per-connection
   counter, can't distinguish "genuinely nothing changed ever" from "this connection just opened"
   without that startup flag) or the catalog's own change counter has moved since the last
   completed run. **Nothing runs on exit**, by design — that's the entire point of this ticket
   versus LRC's backup-on-close prompt.
6. **Runs as a `Pounce` job** (`pounce_jobs::BackupJob`, `Lane::Cpu`, `Priority::Background`, new
   `JobKind::Backup`): four chunks (quick_check -> snapshot -> verify -> rotate), one per
   `ninelives` step, so the activity panel shows real progress on a multi-second run instead of one
   opaque `Done`. Cancellation is only ever *between* chunks
   (`nicti_pounce::ChunkedJob` has no on-cancel callback, see its own doc comment) — a `.partial`
   file abandoned by a cancellation between `Snapshot` and `Rotate` isn't deleted synchronously;
   it's swept up by the *next* run's own `QuickCheck` chunk
   (`ninelives::cleanup_stale_partials`), which every `run_backup`/`BackupJob` run already does
   first regardless of cancellation. A small, deliberate scope narrowing from this ticket's
   original design sketch, which assumed an immediate on-cancel delete that the job trait doesn't
   actually support.
7. **Filenames are plain Unix-second epochs**, not a formatted UTC calendar timestamp
   (`<stem>.<unix-seconds>.sqlite`) — sortable and trivially parsed with no new dependency; no
   `chrono`/`time` crate exists anywhere in this workspace today, and adding one for filename
   cosmetics alone wasn't worth it.

## Review findings

An adversarial review (before this PR opened, per this repo's own standing practice) caught two
real bugs, both fixed before merge:

- **`NineLives::record_ran` was called eagerly at job-submission time, regardless of outcome, and
  `BackupJob::step` returned `Err` on a genuine I/O failure** — `nicti_pounce::Pounce` marks a job
  `JobState::Failed` on an `Err` return and never calls back into it, so its `ReportSlot` was left
  permanently unresolved. Combined, a single transient failure (a disk error, a momentarily-locked
  file) would silently and permanently suppress every later scheduled attempt, since the "last
  backup" baseline had already advanced as if the attempt had succeeded. Fixed by (a) adding
  `BackupOutcome::Failed(String)` and having `BackupJob::step` route every `ninelives` error
  through `finish` instead of returning `Err`, so the job always reaches `Done` and its
  `ReportSlot` always resolves, and (b) moving `NineLives::record_ran` to fire only once a
  resolved report is actually `BackupOutcome::Verified` — a failure now leaves the "last backup"
  baseline untouched, so `NineLives::due` retries on the very next poll rather than waiting for an
  unrelated further catalog edit.
- **`unique_partial_path` only checked for a colliding `.partial` name, never the *final*
  `<stem>.<epoch>.sqlite` a same-epoch rerun would eventually `rotate` into** — `fs::rename`
  silently overwrites an existing destination on both Unix and Windows, so two runs landing on the
  same `now_unix` (unreachable through today's only production call site,
  `nicti-pelt`'s 30s-interval poll, but directly reachable by any caller passing its own
  `now_unix`, including tests and a future manual "back up now" button) would destroy an
  already-verified backup with no record that it happened. Fixed by checking both names before
  picking an epoch.

The review also flagged, without treating as blocking: `SqliteCatalog::quick_check` holds the same
shared connection mutex every other catalog query does for the duration of `PRAGMA quick_check` —
consistent with every other read on this crate's `CatalogStore` trait (not a new architectural
concern this ticket introduces), but not separately measured at 2M-row scale either.

**A separate, real Windows-only failure surfaced by CI itself (not the review), after the fixes
above landed**: `cargo test (windows)` failed two unit tests with `Io("Access is denied. (os error
5)")` — a freshly-written or freshly-renamed file transiently refusing even a read-only open,
`ERROR_ACCESS_DENIED`, which GitHub's Windows runners hit reliably enough on a create/rename-then-
immediately-reopen pattern to be a known class of flake (real-time antivirus scanning a new file
before releasing it; Linux has no equivalent lock and never reproduced it). Fixed with a small,
bounded retry (`ninelives::retry_on_transient_access_denied` / its `rusqlite`-flavored twin) around
every genuine production call site that reopens a file it just wrote (`snapshot_into`'s fsync,
`verify`'s open of the `.partial`, `rotate`'s rename) — cheap insurance on a background-job-only
code path, not a real cost. Two of this ticket's own new tests reopen a freshly-rotated backup file
for their own assertions (mimicking a hypothetical future "restore" caller) and needed the same
retry to stop being Windows-flaky themselves.

## What this doesn't do

- No UI for picking a different backup drive/location — `BackupPolicy::for_catalog`'s default
  (alongside the catalog) is the only option right now.
- No restore flow — a verified backup is a real, independently-openable `SqliteCatalog` (proven by
  `tests/ninelives.rs`), but nothing in this ticket wires up "open this backup instead" in the UI.
- No cross-machine/cloud backup destination — local disk only, matching this project's existing
  local-first stance elsewhere (e.g. `feedback_relay_storage_r2_deferred` in another repo's own
  memory, same reasoning: don't build the cloud-storage path before it's asked for).

## Measured

Not reference-hardware-measured this pass (unlike several other ADRs in this repo) — ADR-0067
already measured `VACUUM INTO`'s own timing (2.0/2.5s p50/p95 at 2M rows) as part of its engine
comparison; this ticket's own tests instead exercise the *concurrency* property that measurement
didn't cover (a real writer thread still making progress, and the snapshot itself returning
without waiting for it,
`tests/ninelives.rs::snapshot_completes_without_waiting_for_a_slower_concurrent_writer` — see its
own doc comment for the honest limit of what a small-scale timing test like this can and can't
distinguish), plus correctness of verification, rotation, and scheduling — all fast, deterministic,
no hardware dependency.
