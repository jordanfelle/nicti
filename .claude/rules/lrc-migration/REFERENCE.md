---
paths:
  - "spikes/shed/**"
  - "crates/nicti-lair/**"
  - "crates/nicti-stray/**"
  - "docs/adr/0061*"
  - "docs/adr/0062*"
---

# LRC Migration — Quick Reference

Full reasoning/history: `docs/decisions/lrc-migration.md`.

- **Folder model**: LRC's `AgLibraryRootFolder`→`AgLibraryFolder`→`AgLibraryFile`+`Adobe_images`
  maps directly onto ADR-0071's `volume`/`root`/`asset`. All real roots use a drive-letter absolute
  path; some also carry a relative-path fallback, not universal — #62 handles both.
- **`pick`/`rating` are SQLite `REAL`, not `INTEGER`** — don't assume integer semantics from the
  column name; a naive numeric op on `hasRetouch` (a bitmask-string column) silently produces a
  wrong number instead of erroring.
- **Collections**: `AgLibraryCollection.creationId` — `com.adobe.ag.library.collection` is a real
  user collection; `*.unsaved` kinds are LRC's own per-module scratch state, never import those.
  Smart-collection rule-criteria mapping is unverified (none existed in the measured catalog).
- **Develop settings**: `agprefs` crate (MIT) parses LRC's `s = { Key = Value }` Lua literal with
  zero failures on a real 380,307-row catalog — adopted over hand-rolling. `develop.rs`'s
  `classify_key` is an exact-match table (not prefix heuristics — those missed `Enable*` toggles
  and per-channel HSL keys) sorting all real keys to owner tickets #42/#46/#47/#39/#51/#49/#40/#52.
  Keep raw LRC text verbatim in a provenance blob on import; translate only what's needed.
- **`FilterList`/`AllowFilters` gate 4 distinct AI filters (#157)**, not just Denoise: real usage
  is 20,303 Denoise / 47 People Removal / 6 Super Resolution / 1 Reflection Removal entries
  (summing to 20,357 across 20,356 active rows of 380,307 — one row holds 2 entries), read from
  `FilterList.Filters[].Title`. `classify_key` defaults the raw key to `#40`
  (dominant case) — #62's importer must inspect each `Filters[]` entry and route People/Reflection
  Removal to `#51`, Super Resolution to `#174`. `LensBlur` is present almost everywhere but always
  empty (0/380,307 rows with real content) — provenance-only, no owner ticket needed.
- **`lrcat-extractor` not a dependency** — its `rusqlite = "0.38"` pin conflicts with every
  workspace member's `^0.40` (`nicti-lair`, `homing`, `pupil`, `shed`, `sniff`) via Cargo's
  workspace-wide `links = "sqlite3"` uniqueness (not per-binary, holds even behind an optional
  feature). Evaluate it standalone outside the workspace, not as a `spikes/*` dependency, until
  `nicti-lair`/its callers get their own isolated build or `lrcat-extractor` bumps its pin.
  (This conflict originally involved `spikes/den`'s own `^0.40` pin too, before #123 deleted it —
  the constraint didn't go away, it just moved onto the real crates.)
- **`shed`'s `open_backup` guard**: refuses a `.lock`/`-wal` sibling only when it has real content
  — a plain read-only open of an already-closed WAL-mode catalog leaves harmless zero-byte
  siblings behind, and presence-alone was a real false-positive this pass hit and fixed.
- **`AgLibraryFile.md5` is unusable for relink/dedupe — never populated (#158/ADR-0158)**: 0/380,298
  rows non-NULL in the real catalog, across every extension. `importHash` is 99.97% present and
  100% distinct among non-NULL values (no duplicate groups) — shaped like a per-file identifier,
  but its derivation is unconfirmed, so #62 keeps it provenance-only, not a relink/dedupe input.
- **`.lrcat-data` is RocksDB + BlobDB, keys are `MaskDigest`/`OriginalInstanceDigest`, not any
  catalog column (#156/ADR-0156)**: `<id>.blob` = RocksDB's own file-number naming
  (`blob_file_size=256MiB` explains the ~270MB size). Keys are 32-char hex content-addressed
  digests embedded in the develop-settings Lua text — `MaskDigest` (AI mask rasters) and
  `OriginalInstanceDigest` (AI Denoise/Enhance output, inside `ImageGroup`), TIFF-wrapped, 97.8% of
  keys measured. **#62's importer does not need to read `.lrcat-data`** — these are LRC's own
  cached AI outputs, re-derive natively (masking/denoise's existing stance) rather than migrate.
  Reproduce via `spikes/shed/tools/lrcat_data_linkage.py` + `rocksdb_sst_dump`
  (`brew install rocksdb`) — no `rocksdb` Cargo dependency, same `links = "sqlite3"`-style
  workspace-uniqueness risk `lrcat-extractor` already hit.

- **The import build (#62, ADR-0062)**: `crates/nicti-stray`'s `LrcImportJob` — open (live-catalog
  guard) → ingest roots via Scruff → match by `(root, folded rel_path)` → keywords/collections
  (additive) → items (`apply_lrc_chunk`, one transaction per 500) → mark catalog-dirty (so XMP sync
  doesn't revert). Schema v10 `lrc_provenance` keeps verbatim develop text + what the import last
  wrote, so a re-run never overwrites a later nicti edit. **`step()` never returns `Err`** (slot must
  resolve; `Drop` = cancelled). Real-catalog gotchas: `fileWidth`/`fileHeight` are `REAL` (read
  numeric columns by stored value); `LocalExposure2012` is stops/4; a radial's `MaskInverted=true`
  = effect outside; AI mask space = uncropped frame; legacy PV2003 keys (`Contrast`, `Shadows`,
  `Exposure` …) sit beside PV2012 ones in every image. Not translated: brushes, People masks, rotated
  crops/geometric masks, retouch, Clarity/Texture/Dehaze, point curves (all in provenance).

## Package contents

- **`spikes/shed`** (#61/ADR-0061's `.lrcat` schema-mapping research) — schema/inventory/
  develop-settings reading plus a pre-commit privacy check against the real catalog's own
  keyword/path strings, plus #157's `develop-usage` subcommand (`analyze_unowned_keys`) which
  measured real presence/active-use counts for the 6 keys `classify_key` originally left unowned
  and resolved all of them to an owner (or `ProvenanceOnly`, for `LensBlur`'s confirmed-zero-real-
  usage case), plus #158's `hash-stats`/`verify-md5` subcommands (`hashes.rs`) which found
  `AgLibraryFile.md5` unusable for `spikes/homing`'s relink tiers (never populated, 0/380,298 real
  rows) and `importHash` provenance-only (present and unique per-file, but its derivation is
  unconfirmed). Real, tested (38 unit tests on Unix, 37 on Windows — the one Unix-only test,
  `open.rs`'s pre-existing URI-special-character case, predates #158 and is unrelated to it), not
  path-gated. See `docs/research/shed-lrcat-schema.md` and `docs/adr/0158-lrc-hash-relink-seeding.md`.
- **`crates/nicti-stray`** (#62/ADR-0062) — `open.rs` (guarded read-only open + v13 schema check,
  promoted from the spike), `read.rs` (paged `Reader`), `paths.rs` (remap, drive-letter mapping),
  `develop/` (Lua → `EditDocument`: `basic`/`hsl`/`detail`/`crop`/`masks`/`heal`/`filters`),
  `job.rs` (`LrcImportJob`), `report.rs`; `tests/import.rs` end to end on a synthetic `.lrcat`,
  `tests/real_catalog.rs` (`#[ignore]`, `NICTI_TEST_LRCAT=<closed backup>`) against a real one.
  UI: `nicti-pelt`'s `lrc_import.rs` (Library controls).
