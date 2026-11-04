## XMP interop with Lightroom Classic

Covers #59's XMP-interop research — full reasoning and every measured number in
`docs/adr/0059-xmp-interop.md`. This file is the per-topic summary; that ADR is the full research
trail.

- **Handoff from ADR-0021**: the non-destructive edit model ADR set three XMP responsibilities
  (LRC-convention metadata, a lossless `nicti:` recovery namespace, a best-effort write-only `crs:`
  projection) and left this ticket the field-level mapping, the write library, and the `crs:`
  write-gating policy.
- **Field mapping**: `xmp:Rating` (`-1..=5`), `xmp:Label` (text), `dc:subject`/
  `lr:hierarchicalSubject` (flat/hierarchical keywords). Unrated stays `None`, distinct from a `0`
  rating — `docs/research/shed-lrcat-schema.md` found the real `.lrcat`'s `rating` column is
  nullable (277k/380k rows NULL), so collapsing the two would silently invent data on import.
  LRC's positive Pick flag has no confirmed XMP field and isn't mapped here.
- **Library: `quick-xml`**, chosen for its event-based Reader/Writer API — the spike's `packet.rs`
  parses a full packet into events and only rewrites the properties it owns, re-emitting `crs:`/
  `exif:`/`aux:`/any other namespace completely unchanged. Proven by test, not just asserted.
- **Embedded formats**: `shed-lrcat-schema.md`'s real 380,307-row catalog is ~82% JPEG/DNG, where
  LRC embeds XMP in the file rather than a `.xmp` sidecar. JPEG's APP1 XMP segment is implemented
  (read + write, with a from-scratch segment walker and a byte-diff-guard test proving nothing
  outside the segment changes). DNG/TIFF tag-700 write is deferred — TIFF's absolute IFD offsets
  make a naive splice unsafe in a way JPEG's flat segment list isn't; v1 treats DNG as read-only
  for embedded XMP and writes a `.xmp` sidecar for DNG assets instead.
- **`crs:` write gate**: write only if the sidecar's hash still matches what Nicti itself last
  wrote there, otherwise skip and flag — keeps the projection live during normal editing without
  risking clobbering an LRC edit made to the same file in between. **The hash comparison alone
  cannot close the race**: an LRC save landing between Nicti's hash read and its write is still
  possible; `conflict.rs`'s own docs specify the lock/single-writer discipline a real caller needs
  to close that window, which this gate function doesn't itself enforce.
- **No hands-on LRC session was run this pass** — real evidence came from `shed-lrcat-schema.md`,
  the public XMP/`crs:`/`lr:` specs, and this spike's own real-code test suite (38 tests, all
  passing), not from a live Lightroom install. The ADR merges as Proposed; a follow-up issue (part
  of #11) runs a scripted LRC session (rating/reject/pick/custom label set/hierarchical keywords on
  throwaway copies) to settle several open questions (does Reject/Pick reach XMP at all? does a
  renamed label set change the written text? what does "Read Metadata from File" do with a
  Nicti-written field?) before promoting this to Accepted.

- **#60 implementation (phase-1 coexistence)**: `crates/nicti-scent` (promoted from `spikes/scent`,
  spike deleted) + `nicti-lair`'s `scent_sync.rs` + `nicti-pelt`'s `xmp_sync.rs`; see the ADR's
  "Implemented in #60" section for the conflict/hold-for-review rules, the schema v9
  `asset_sidecar` state, and the known limits. ADR-0059 is still Proposed pending #187.


## #371: JPEG assets sync through their embedded XMP

Once JPEG ingest (#489) landed, `nicti-lair`'s `scent_sync` routes JPEG assets to the APP1 packet
inside the file instead of a sidecar (`xmp_target`). The conflict hash covers the XMP packet text
only, so re-encoding pixels or touching EXIF is never read as an XMP change. A JPEG with no packet
imports as `NoSidecar`; a write splices a new packet in (atomic temp-file + rename, everything
outside the APP1-XMP segment byte-identical) and never creates the file. `IMG.xmp` stays the RAW's
in a RAW+JPEG pair. DNG remains read-only for embedded XMP (writes go to a sidecar).

## #536: geotag engine

`asset_gps(asset_id, latitude, longitude, altitude)` (schema v12, cascade-deleted with the asset) is a side table so the wide `asset` table is untouched. `trail::geotag_assets` takes `(asset_id, path)` items plus a parsed `Tracklog` and `GeotagOptions`, and per asset: reads `captured_at`, locates a position, stores it, then writes it through the normal sidecar path (so a sidecar changed under us is held for review, never clobbered). With auto-write off the asset is only marked dirty, like a marker edit. An asset that already has a position is left alone unless `overwrite`.

Sync semantics: `Markers.gps` is carried next to the markers but excluded from `==` and the conflict hash, so a position never raises a conflict by itself. The catalog's position is written whenever it has one and the file's differs; a position only the file has is left in place and adopted by the catalog when it has none; a file with no position never clears the catalog's. A pending catalog position with the catalog marked newer is pushed even when every marker agrees (otherwise the dirty flag would be cleared and the position lost). `gpx::read_gps` reads attribute and element forms and rounds-trips what `Patch.gps` writes (1e-5 degree / 2 cm tolerance).

Known limitation, unchanged from #431: a log with overlapping tracks from two devices is interpolated in global time order. The UI half (apply dialog, Info display) is #668.

#665/#666 (JPEG rescan cost): `import_sidecar` on a JPEG with no APP1 packet used to return `NoSidecar` without recording anything, so every rescan read and hashed the whole file (NFS-heavy). It now records a marker row holding only the trusted mtime (no hashes, no dirty/review state); the stat-only precheck then answers the rescan. `is_blank_marker` keeps that row from counting as a prior sync, so a packet that appears later resolves exactly as with no row. Separately, our own embedded-XMP splice changes size, mtime and the head-64KB fingerprint; `write_locked` now calls `CatalogStore::refresh_file_identity` (size, mtime, fingerprint; clears the whole-file content_hash, keeps T0/orientation/lens/ISO because pixels and EXIF are untouched) so the next scan skips the file and a rename before it still relinks by fingerprint.


**Geotag UI (#668).** The engine of #536 had no UI. `geotag_ui.rs` adds the apply-GPX form and an Info section to the Library controls. Decisions: (1) no file picker -- none exists in the tree and the ticket forbids a new dependency -- so the GPX is a path text box like the Folder field; (2) nothing heavy on the UI thread: `GeotagJob` takes asset ids and a `GpxSource::File`, reads (size-capped at 64 MiB) and parses the log in its first `step`, resolves each asset's path from `root path + rel_path` per 16-asset chunk, and merges `GeotagReport`s; a bad log is `gpx_error` with nothing tagged, a cancelled or dropped job still resolves its slot (`stopped`); (3) **clearing needs its own write**: `Markers::to_patch` leaves a file-only position alone (so that an external position is adopted, not erased), which means set_gps(None) + an ordinary sidecar write would be undone by the next import. `scent_sync::clear_position` writes an explicit `Patch.gps = Some(None)` through the usual write path (`write_locked(.., clear_gps)`), clears the catalog only after that write succeeded, returns `NeedsAutoWrite` (changing nothing) when the file has a position and auto-write is off, and `HeldForReview` when the sidecar changed under us; (4) the overlapping-tracks limitation is stated in the form rather than adding a track picker (a follow-up if it bites). `nicti-lair` re-exports `Gps`/`GeotagOptions` so the UI needn't depend on `nicti-scent`. Info shows file, capture time, camera (the make is dropped when the model repeats its first word: `NIKON CORPORATION` / `NIKON Z 8`), size, lens, ISO and the position as `46.000500° N, 7.001000° E, 1605 m`.

Review of the geotag UI (#668) changed three things: `clear_position` returns `Unreachable` (nothing changed, reported as a failure by the job) when the asset's file is not there, because clearing only the catalog of an offline photo would let the position the file still carries be re-adopted when the volume returns -- the same rule `geotag_assets` applies; `App::job_active` treats a running geotag/clear job as blocking Import, Sync, Move, Delete, Verify and Baseline (the guard was one-directional); and the Info section re-reads its photo every 3 s while open, so a position changed by a rescan or XMP import shows without moving the cursor. The target list is built only when a button is clicked (the per-frame cost is `target_count()`). Known and not changed: a clear on a file whose position was never recorded in `asset_sidecar` (never imported) is held for review, and resolving that review with 'use catalog' does not clear the file.
