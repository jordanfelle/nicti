## Lightroom Classic catalog import mapping

Covers #61's `.lrcat` schema mapping — full reasoning and every measured number in
`docs/adr/0061-lrc-catalog-import-mapping.md` and `docs/research/shed-lrcat-schema.md`. This file
is the per-topic summary; those two are the full research trail.

- **Real data, real numbers**: this ADR is grounded against the user's own 380,300-asset catalog
  (matching ADR-0029's own count from the same library), not secondary-source guesses. Never a
  keyword, collection name, or path — every number is a count.
- **Folder model (#22/#71)**: LRC's `AgLibraryRootFolder`→`AgLibraryFolder`→`AgLibraryFile`+
  `Adobe_images` maps directly onto ADR-0071's `volume`/`root`/`asset` shape. All 13 real root
  folders use a drive-letter-prefixed absolute path (confirms ADR-0071's assumption); 2 of 13 also
  carry LRC's own portable-catalog relative-path fallback, which isn't universal.
- **Library metadata**: ratings/picks/color labels follow LRC conventions directly (already
  ADR-0021's rule). Real gotcha: `pick`/`rating` are SQLite `REAL`, not `INTEGER` — a naive numeric
  op on a *different* bitmask-string column (`hasRetouch`) silently coerces to a wrong number
  rather than erroring, so #62 must respect each column's real declared type.
- **Collections**: `AgLibraryCollection.creationId` separates real user collections
  (`com.adobe.ag.library.collection`) from LRC's own per-module scratch state (`*.unsaved` kinds) —
  the real catalog had 1 real collection and 4 scratch rows. No smart collections existed to
  measure against; their rule-criteria mapping onto #23's filter bar stays a flagged follow-up.
- **Develop settings (#46/#47/#49/#39/#42/#51/#40/#52)**: the `s = { Key = Value, ... }`
  Lua-literal format ADR-0021 predicted, parsed by the `agprefs` crate (MIT) with **zero failures
  across 380,307 real rows** — settles adopt-vs-hand-roll in `agprefs`'s favor. 197 distinct real
  keys, all classified into an owner ticket by an exact-match table (grouped by real Develop-module
  panel, not guessed from naming conventions — a first prefix-only draft missed 83/197 keys, mostly
  `Enable*` panel toggles and per-channel HSL keys with no shared prefix). **#157 resolved the 6
  keys this pass originally left unowned** by measuring real presence/active usage rather than
  guessing: `FilterList`/`AllowFilters` gate 4 distinct AI filters with very uneven real usage
  (20,303 Denoise / 47 People Removal / 6 Super Resolution / 1 Reflection Removal entries, summing
  to 20,357 across 20,356 active rows of 380,307 — one row holds 2 entries) — `classify_key`
  defaults both to `#40` (dominant case), but #62's importer must inspect
  each `FilterList.Filters[].Title` and route People/Reflection Removal to `#51` and Super
  Resolution to the new `#174`. `LensBlur` is present in 380,300/380,307 rows but always as an
  empty bookkeeping table (0 rows with real content, i.e. never actually used) — kept
  provenance-only, no render-owning ticket. `Preset`/`ToggleStyleAmount`/`ToggleStyleDigest` are
  style-preset bookkeeping → `#52`. Import policy: keep every row's raw LRC text verbatim in a
  provenance blob, translate only what's needed at import time.
- **`.lrcat-data` blobs (#156/ADR-0156)**: it's a RocksDB database with integrated BlobDB (`<id>.blob`
  is RocksDB's own file naming; 256MiB blob-file-size setting explains the ~270MB size). Keys are
  `MaskDigest`/`OriginalInstanceDigest` content-addressed digests embedded in the develop-settings
  Lua text (not any catalog column) — AI mask rasters and AI Denoise/Enhance output rasters,
  TIFF-wrapped, accounting for 97.8% of real blob keys measured. These are LRC's own cached AI
  outputs, not develop parameters: **#62's importer doesn't need to read `.lrcat-data` at all**,
  consistent with masking/denoise's existing re-derive-don't-migrate stance.
- **`md5`/`importHash` cross-check against homing's relink tiers (#158/ADR-0158)**: `md5` is NULL
  on all 380,298 rows in this catalog — not a partial gap, nothing is there at all, so #62 must not
  plan on it as a relink or dedupe input in any role. `importHash` is 99.97% present with every
  non-NULL value distinct (no duplicate groups, shaped like a per-file id rather than a
  per-import-session token), but its derivation is unconfirmed — kept as opaque provenance only,
  same policy as every other raw LRC field this ADR already covers.
- **#53 feasibility**: 380,300 assets, ~1.77M develop-history-step rows (~4.7/image) give a real,
  usable before/after training set. Dataset construction itself is #53's own scope.
- **`lrcat-extractor` (MPL-2.0) — real Cargo constraint found, not adopted as a dependency**: its
  own `rusqlite = "0.38"` pin cannot coexist in this workspace's single Cargo.lock with the
  workspace's own unconditional `rusqlite = "^0.40"` pins (`spikes/den` originally, now
  `crates/nicti-catalog`/`homing`/`pupil`/`shed`/`sniff` since #123 deleted `den`) — Cargo's
  `links = "sqlite3"` uniqueness is enforced workspace-wide, not per binary, and this holds even
  with `lrcat-extractor` behind an optional, default-off feature (Cargo still solves for every
  feature combination the workspace could activate). Evaluated standalone (outside the workspace)
  instead: it opens/reads the real v13 catalog fine; a full feature-parity comparison against
  `shed`'s own reading is deferred to #62, once `nicti-catalog`/its callers get their own isolated
  build or `lrcat-extractor` bumps its pin (deleting `den` did not resolve this constraint — it
  just moved onto the real crates).
- **Version support**: only v13 (the user's current LRC) was available to measure; older-version
  support is a follow-up if/when needed, not built speculatively.
