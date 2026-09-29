# ADR-0026: Verified folder move (Carry)

- **Status:** Accepted
- **Date:** 2026-09-28
- **Ticket:** [#26](https://github.com/jordanfelle/nicti/issues/26) Build: RAW file backup with
  checksum verification (re-scoped, see Decision)

## Context

#26 was filed as "automated, checksum-verified backup of RAW files to backup drives/cloud (target
TBD) -- replaces today's manual copy/verify toil". Asked what the toil actually is, the user's
answer was concrete: in Lightroom Classic they **drag a folder to another drive**, and want that
same gesture here -- later marking a drive as "archive" so archived folders are treated differently
from the active, fast ones (#72's territory: `root.archived`, thumbnail sidecars).

So the first thing to build is not a second copy of the photos, it is a *safe move*: copy, prove the
copy, only then let go of the original, with the catalog following the folder.

## Decision

**Carry** (`crates/nicti-lair/src/carry.rs`; a mother cat carrying her litter to a new nest):

1. **Catalog follows by re-pointing one row.** Every root sits under a volume and stores its path
   in `root.rel_path`; assets reference `(root_id, rel_path)`. Moving a folder therefore rewrites
   exactly one `root` row -- asset ids, edit history, keywords, collections, ratings and Larder
   entries (all keyed by asset id) are untouched.
2. **The source is never touched before the catalog commit.** Preflight (source exists, destination
   is an existing folder, not inside the source, target name free or an empty folder, not already
   a registered root, no symlinks in the tree) -> journal row -> fast path or copy.
3. **Fast path:** `fs::rename` first. Same-volume moves are atomic and copy nothing. Any failure
   (cross-device, locks) falls through to the copy path. No per-file hashes are recorded on this
   path (nothing was read).
4. **Copy path:** every file under the source -- not only cataloged assets, so XMP sidecars and
   stray files move too -- is streamed to `<name>.partial`, BLAKE3-hashed as read, mtime carried
   over, `sync_all`'d, then **re-read from the destination and re-hashed**. Only an exact match (and
   an unchanged length) is renamed into place. The verify re-read hits the OS page cache on small
   files, so it proves the bytes we wrote, not that the platter survived; a later "Verify folder"
   job re-hashing an archive drive against the stored `content_hash` covers that (follow-up).
5. **Commit = one transaction** (`CatalogStore::commit_root_move`): re-point the root, record each
   cataloged asset's full-file BLAKE3 in the new `asset.content_hash` column (the existing
   `fingerprint` is only a 64KB head+tail hash), flip the journal row to `committed`.
6. **Cleanup:** delete verified source files -- skipping any whose size or mtime changed since it
   was copied -- then empty directories. A file that can't be deleted (antivirus, Explorer) is
   reported as a leftover, not a failure: the move already committed.
7. **Crash safety via a `root_move` journal** (migration v6; at most one open row per root).
   `carry::resume_open_moves` runs at startup: a `copying` row rolls back (delete the half-built
   destination; the source is intact), unless the source is gone and the destination exists (the
   rename landed) in which case it commits; a `committed` row finishes cleanup, deleting only source
   files whose bytes match the destination's.
8. **Cancel:** Pounce cancels by no longer stepping the job, so `Carry`'s `Drop` discards the
   half-built destination and closes the journal. After commit there is nothing to undo.
9. **Runs as `pounce_jobs::MoveJob`** (`JobKind::Move`, CPU lane, background), chunked at 64MiB so
   cancel is responsive. Never returns `Err`; every failure is a `CarryOutcome` in the report slot
   (same lesson as `BackupJob`). The Library view refuses import/sync while a move runs, and a move
   while an import/sync/move runs.

Also fixed while here: `SqliteCatalog::quick_check` now reports a corrupt-database *error* from
the pragma itself as a problem string (`Ok(Some(_))`) instead of a generic `Err`. The extra v6
tables shifted the file layout enough that `tests/ninelives.rs`'s crude mid-file corruption started
making the pragma fail outright, which Nine Lives surfaced as an I/O-style error, not `LiveCorrupt`.

## What this doesn't do

- **No backup copy.** Nothing here keeps a second copy of anything. Cloud/backup-drive targets are
  a separate follow-up.
- **No drag-and-drop.** The UI is a destination text field plus a per-folder Move button; a
  folder/drive tree that can drop onto a drive needs the folder panel, which doesn't exist yet.
- **No archive-drive behavior.** Marking a drive "archive" and the thumbnail-sidecar switch are #72.
- **No re-verification of a stored `content_hash`,** and none recorded on the rename fast path.
- **Not verified on Windows hardware or across real drives** in the sandbox this was written in;
  the cross-device copy path is exercised on one filesystem via `CarryOptions::force_copy`.
