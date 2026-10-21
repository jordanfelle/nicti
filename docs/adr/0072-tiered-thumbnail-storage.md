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
   The UI-thread readers (cull tiles, loupe fallback) only query the catalog; on a miss the sidecar
   read -- which can block on an offline archive drive -- runs on a Pounce worker
   (`nicti-pelt/src/t0_fetch.rs`). The grid's thumbnail job already runs on a worker.
   T2 for archived folders isn't cached (ADR-0029: don't populate for an archive drive).
8. **Scruff only ingests `nef`/`nrw`**, so sidecars aren't picked up as assets today;
   `thumb_sidecar::is_sidecar_name` is the guard for when JPEG ingest lands.

## Consequences

- The startup recovery path only re-settles roots whose move was resumed; a changed archive-drive
  setting does not retro-actively archive existing folders (only a Move does).
- Behaviour on a real removable drive (unmount/remount, exFAT timestamps) is untested here --
  tracked as a `needs-physical-testing` follow-up.
- Follow-ups: role keyed on real volume identity. (The user-selectable sidecar codec landed in #393, below.)

## Amendment (#393): selectable sidecar codec

`SidecarCodec` gains `Avif` (`.thumb.avif`); the choice is `sidecar_codec` in
`<catalog>.archive-drives.json` (default JPEG, an old file loads as JPEG) and a combo in the Library
view, applied to folders archived from then on. `ravif` quality 80, speed 9 (ADR-0143's archival
candidate), single-threaded; lossy WebP stays rejected.

- **Reads are codec-agnostic**: `thumb_sidecar::read` tries the preferred suffix, then the other, so
  a folder stays browsable whichever codec it was exported in. `export` removes the other codec's
  file so one RAW never has two.
- **ICC-tagged T0 stays JPEG** in AVIF mode (ravif can't carry a profile, nicti-lair has no colour
  management), as does a T0 that fails to decode; `export` returns the codec actually written.
- **Settle**: a JPEG sidecar still has to equal the blob byte for byte. An AVIF one is a re-encode,
  so it is accepted when it is a whole, parseable file whose pixel size equals the blob's (a
  re-ingested blob of the same size but different content would keep the old thumbnail; acceptable
  for a grid thumbnail, and the blob is not dropped on any mismatch).
- **Unarchive never ingests AVIF**: the catalog's T0 stays the RAW's own JPEG (ADR-0029), so an AVIF
  sidecar is replaced by re-extracting T0 from the RAW; if that fails the sidecar is kept.
- `avif-parse` panics (asserts) on some truncated files instead of returning `Err`, so
  `avif_dimensions` catches it: a bad file reads as absent.
- `nicti-pelt::preview_color::decode` sniffs `ftypavif` and decodes via `avif-decode` (ravif's default
  10-bit output comes back as 16-bit RGB).
- Measured (one non-Nikon 115 KB JPEG, so not ADR-0143's corpus): 78 KB AVIF, ~250 ms encode. The
  ~2.7x figure is ADR-0143's; re-measure on real T0s before making AVIF the default.
