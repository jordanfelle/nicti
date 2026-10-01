# ADR-0072: Tiered thumbnail storage (SSD catalog / archive sidecars)

- **Status:** Accepted
- **Date:** 2026-10-01
- **Ticket:** [#72](https://github.com/jordanfelle/nicti/issues/72) Build: tiered thumbnail
  storage (SSD cache + archive sidecars)

## Context

While a folder is under active edit on the SSD, its grid thumbnails (T0, ADR-0029) live in the
catalog's `preview` table. When the user's workflow moves that folder to an archive drive, the
catalog should stop carrying them and the thumbnails should live next to the RAWs, so the archive
can be browsed from a re-mounted drive and the catalog stays small. Moving the folder back must
reverse this cleanly. ADR-0026 (Carry) left the archive behaviour to this ticket; ADR-0071 gave
`root.archived` for it.

The issue text names `.thumb.webp`; ADR-0143 measured lossy WebP and rejected it (worse quality
than AVIF at the same size, ~3x JPEG's decode p95, native C dependency).

## Decision

1. **Archive role is a path-prefix setting** (`crates/nicti-pelt/src/archive_drives.rs`,
   `<catalog>.archive-drives.json`), set per drive in the folder panel. ADR-0071's volume identity
   is not wired into the app yet (every root sits under a placeholder volume), so a per-volume role
   is not possible; `ArchiveDrives::is_archive` is the single function to swap when it is.
2. **A Move across the boundary changes tier.** Moving into an archive location archives the
   folder; moving out to a non-archive location reactivates it. A plain move within one side does
   nothing extra. `root.archived` is flipped in the same transaction as the root re-point
   (`commit_root_move_archived`).
3. **Sidecars are `<raw file name>.thumb.jpg`** (`thumb_sidecar.rs`) -- the full RAW name kept, not
   LRC-style extension replacement, so `.NEF`/`.NRW` with one stem can't collide. The payload is the
   T0 JPEG bytes unchanged: no re-encode, no new dependency. `SidecarCodec` has only `Jpeg`; a
   user-selectable codec (AVIF per ADR-0143) is a follow-up. Written atomically (temp + rename).
4. **Archive = export, move, settle.** Before anything moves, `Carry` writes each asset's sidecar
   into the *source* folder (`CarryOptions::export_sidecars`), so sidecars travel with the folder
   over rename or verified copy. After the commit and cleanup, a chunked `Settle` phase drops the
   catalog blobs (`tier::settle_asset`) and the app drops their T2 Larder entries. A failed sidecar
   write fails the move before it starts.
5. **Reactivate = move, settle.** After the commit, `Settle` reads each sidecar back into the
   catalog and deletes it.
6. **Never lose the only copy.** A catalog blob is only dropped when its sidecar reads back as a
   JPEG; a sidecar is only deleted after its blob is in the catalog. Both steps are idempotent, so
   a crash anywhere leaves a folder with at least one tier intact and `tier::settle_root` (re-run
   at startup for roots touched by crash recovery, along with re-deriving `archived` from the
   setting) finishes the job.
7. **Read path** (`tier::load_t0_by_id`, used by the grid, cull and loupe readers): catalog first,
   then the sidecar. A settled archived folder has no catalog blobs by construction, so its reads
   land on the sidecar; a folder mid-transition is served from whichever tier still has the bytes.
   T2 for archived folders isn't cached (ADR-0029: don't populate for an archive drive).
8. **Scruff only ingests `nef`/`nrw`**, so sidecars aren't picked up as assets today;
   `thumb_sidecar::is_sidecar_name` is the guard for when JPEG ingest lands.

## Consequences

- The startup recovery path only re-settles roots whose move was resumed; a changed archive-drive
  setting does not retro-actively archive existing folders (only a Move does).
- Behaviour on a real removable drive (unmount/remount, exFAT timestamps) is untested here --
  tracked as a `needs-physical-testing` follow-up.
- Follow-ups: user-selectable sidecar codec; role keyed on real volume identity.
