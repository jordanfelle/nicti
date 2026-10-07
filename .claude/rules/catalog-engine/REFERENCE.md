---
paths:
  - "crates/nicti-lair/**"
---

# Catalog Engine — Quick Reference

Full reasoning/history: `docs/decisions/catalog-engine.md`.

- **Chosen: SQLite** (`rusqlite`, WAL) — `docs/adr/0067`. `(model, rating)` composite index,
  `GLOB` not `LIKE` for prefix scans. Clears every gate at 2M assets except
  faceted-filter-with-facet-counts (closed by the facet-count cache below). DuckDB kept as the
  explicit fallback if the facet-query ceiling becomes a real problem.
- **Turso** (`docs/adr/0102`) — **not adopted**: two query shapes already sit at the 600k-scale
  budget edge, but the deciding factor was a 2M-row bulk-ingest run whose WAL file passed 19GB and
  was still climbing linearly (not explained by a durability-pragma mismatch). Crash-safety left
  inconclusive (same OS-lock structural limit as below). Revisit post-1.0, not never.
- **redb** (`docs/adr/0106`), **RocksDB** (`docs/adr/0115`) — **not adopted**: each misses 1-3
  query-gate budgets at 2M by 1.5–5x, and crash-safety is left inconclusive for both (the
  in-process `mem::forget` crash simulation can't get past their OS-level file locks — a
  structural test-harness limit, not a finding about the engines). RocksDB additionally got the
  series' only concurrent-multi-writer measurement: beats SQLite's single-writer-serialization
  ceiling on peak throughput but shows large untuned run-to-run variance — SQLite still chosen,
  kept as reference data for a future multi-writer decision (#64).
- **Facet-count cache** (`docs/adr/0103`) — **adopted**: trigger-maintained SQLite facet table,
  closes ADR-0067's one measured miss without a new dependency. Clears budget 33-89x at 600k/2M.
  Only answers keyword-narrowed facet queries; an unfiltered facet count needs a separate query.
- **DuckDB as primary store** (`docs/adr/0107`) — **not adopted**: DuckDB compacts #22's planned
  append-only history log ~80x slower per op than SQLite (a real transaction-commit-overhead
  effect, not a benchmark artifact). DuckDB's JSON support is real but doesn't offset this.
- **libSQL** (`docs/adr/0113`) — **not adopted for v1, not a permanent rejection**: only KV/pure-
  Rust-adjacent candidate to cleanly pass the crash-safety gate. Real 1.3–4x per-op overhead
  (async-dispatch), ~5x larger dependency graph. Flagged as the leading candidate to revisit when
  #64 (multi-machine catalog) becomes active — built-in offline-first sync. **Cannot link into the
  same binary as `rusqlite`** (both bundle SQLite C symbols) — this required its own split test job
  in the now-deleted `spikes/den` spike (ADR-0113's Spike section).
- **fjall** (`docs/adr/0116`) — **not adopted**: cleanest Windows-build story (100% safe Rust, no
  `build.rs`) but fails 3/8 query gates at 600k already (the earliest/widest failure in the
  series), attributed to `Guard`/iterator overhead. Crash-safety inconclusive (same OS-lock class
  as Turso/redb). Links cleanly alongside every other candidate, unlike libSQL.

- **Keywords/collections/filter backend (#23, landed)**: `docs/adr/0023`. `asset.rating` is now
  nullable (unrated ≠ 0 stars, per `xmp-interop`/`lrc-migration`); `facet_counts` moved to
  `(volume_id, model, rating)` so `facet_count`/`facets` exclude offline-volume assets via a
  query-time join, not a trigger (SQLite's table-rebuild recipe, not `RENAME`, since
  `preview`/`edit_variant` FK-reference `asset(id)`). Hierarchical keywords (`keyword`/
  `asset_keyword`, id-based materialized path) and collections (`collection`/`collection_asset`,
  manual + smart) landed alongside it; `hunt.rs`'s `Filter`/`Sort`/keyset-paginated `hunt` is the
  query both the filter bar (#242) and a smart collection's saved rule resolve through. Filename
  search is a plain `GLOB` scan for now — FTS5 is a documented, not-yet-done follow-up.
- **Filter-bar listing queries (#242)**: `CatalogStore::list_keywords` (name-sorted, flat —
  caller rebuilds the tree from `parent_id`), `list_collections`, `distinct_makes`/`distinct_labels`
  (online volumes only, empties skipped) — the option lists `nicti-pelt`'s `filter_bar.rs` needs;
  no `Filter`/`hunt` change. `distinct_*` and `facets` are full scans, so they run on `with_scan_conn` (a snapshot reader for
  file-backed catalogs, like `hunt_ids`) so they never hold the shared mutex the UI thread needs.
- **Continuous backup (#25, landed)**: `docs/adr/0025`, "Nine Lives". `VACUUM INTO` from a second,
  independent read-only connection (`SqliteCatalog::open_snapshot_reader`) — proven, not just
  assumed, never to block a concurrent writer (`tests/ninelives.rs`'s own concurrent-writer test).
  Write `.partial` -> `PRAGMA integrity_check` + `user_version` match -> rename -> prune to newest
  9. `PRAGMA quick_check` on the live catalog gates every run before it touches any existing backup.
  Scheduled by `NineLives::due`, polled ~every 30s by `nicti-pelt`'s app loop — nothing runs at
  startup or on exit, by design. Runs as a 4-chunk Pounce job (`pounce_jobs::BackupJob`, new
  `JobKind::Backup`).
- **Verified folder move (#26, landed)**: `docs/adr/0026`, "Carry". Re-scoped from "RAW backup to
  TBD target" to LRC-style move-a-folder-to-another-drive. Copy each file to `.nicti-partial`, BLAKE3 as
  read, `sync_all`, re-read+re-hash the destination, rename in; one transaction re-points the
  `root` row (assets/edits/keywords follow, ids unchanged) and records `asset.content_hash`;
  source deleted only after commit. `fs::rename` fast path on the same volume. `root_move` journal
  + `carry::resume_open_moves` at startup for crash recovery; cancel = drop discards the
  destination. Runs as `pounce_jobs::MoveJob` (`JobKind::Move`). Startup recovery (#307) is
  `carry::ResumeMoves` (one journal row / one file per step) run as `pounce_jobs::ResumeMovesJob`
  under `JobKind::Move`, so every existing "a move is running" guard (import/sync/move/delete/loupe)
  holds until it finishes; `PeltApp::poll_recovery` folds in the report. `resume_open_moves` is the
  same stepper run to completion (tests). Not a backup copy, no
  archive-drive behavior (#72). Drag-a-folder-onto-a-drive UI: #303, `nicti-pelt`'s `folder_panel.rs`.

- **Folder bit-rot check (#304, landed)**: `verify.rs` `Verify` + `pounce_jobs::VerifyJob`
  (`JobKind::Verify`, CPU lane). Re-reads every asset under a root that has `asset.content_hash`
  (16 MiB slice per step, cancellable) and reports mismatched / missing / unreadable; assets with
  no stored hash are only counted (`unhashed`). Read-only. `CatalogStore::content_hashes_by_root`.
  **#386**: UI = `nicti-pelt`'s `verify_ui.rs` (folder-row context menu "Verify folder" -> report details;
  `folder_panel.rs` `PanelOutput.verify`). A cancelled pass lists `VerifyReport::unchecked`; an NFD on-disk name
  is matched through `verify::resolve_on_disk` (not a false `missing`; `patrol.rs` still has that limitation).
  Backfill = `verify::Baseline` + `pounce_jobs::BaselineJob` (`JobKind::Baseline`) -> `CatalogStore::record_baseline_hashes`
  (only rows with no hash and unchanged size/mtime; never overwrites). **Explicit user step**: it only proves a file
  matches itself as it is now, so it is never run implicitly.

- **Batch delete (#32, landed)**: `docs/adr/0032`, "Shred". `remove_assets` (chunked, one
  transaction, also clears the `delete_item` journal row) + `get_meta`/`set_meta` (batch marker
  read, and per-photo *mixed*-value restore for undo). Journal `delete_item` (schema v8) is written
  *before* files move to the Recycle Bin; `shred::resume_open_deletes` settles it at startup.
  Runs as `pounce_jobs::DeleteJob` (`JobKind::Delete`).

- **Master edit document (#57, landed)**: `CatalogStore::get_master_edit(asset_id) -> Option<
  EditDocument>` / `put_master_edit` on the existing `edit_variant` master row (canonical JSON via
  `nicti_pawprint::to_canonical_json`, non-finite floats refused → `CatalogError::Document`; a fresh
  asset reads an empty document, an unknown one `None`, `put` on an unknown asset errors). Survives
  a re-ingest (`insert_asset` only creates the row once). `edit_history` (undo) is #324. `nicti-pelt`
  saves Develop's edits (`PeltApp::save_develop_edits`) on pointer release, before switching photos or
  exporting, and on exit.

## Package contents

- **`crates/nicti-lair`** (#22, landed; #23 landed; #25 landed) — the real production catalog
  implementation: `schema.rs`/`sqlite.rs`/`scruff.rs`/`patrol.rs`/`hunt.rs`/`clowder.rs`/
  `ninelives.rs`/`carry.rs`, plus `larder.rs` (#27, T2 preview cache — see `preview-tiers`). Promotes the SQLite choice this topic's ADR series settled on. `spikes/den` (the
  throwaway comparison spike backing ADR-0067/0102/0106/0103/0107/0113/0115/0116 — one module per
  candidate engine plus a shared `Workload` trait and synthetic catalog generator) was deleted in
  #123 once #22 landed; the per-candidate findings above are the durable record, not the spike
  code itself.
