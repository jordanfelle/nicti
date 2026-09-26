## Lightroom Classic catalog import mapping

Covers #61's `.lrcat` schema mapping — full reasoning and every measured number in
`docs/adr/0022-lrc-catalog-import-mapping.md` and `docs/research/shed-lrcat-schema.md`. This file
is the per-topic summary; those two are the full research trail.

- **Real data, real numbers**: this ADR is grounded against the user's own 380,300-asset catalog
  (matching ADR-0017's own count from the same library), not secondary-source guesses. Never a
  keyword, collection name, or path — every number is a count.
- **Folder model (#22/#71)**: LRC's `AgLibraryRootFolder`→`AgLibraryFolder`→`AgLibraryFile`+
  `Adobe_images` maps directly onto ADR-0020's `volume`/`root`/`asset` shape. All 13 real root
  folders use a drive-letter-prefixed absolute path (confirms ADR-0020's assumption); 2 of 13 also
  carry LRC's own portable-catalog relative-path fallback, which isn't universal.
- **Library metadata**: ratings/picks/color labels follow LRC conventions directly (already
  ADR-0002's rule). Real gotcha: `pick`/`rating` are SQLite `REAL`, not `INTEGER` — a naive numeric
  op on a *different* bitmask-string column (`hasRetouch`) silently coerces to a wrong number
  rather than erroring, so #62 must respect each column's real declared type.
- **Collections**: `AgLibraryCollection.creationId` separates real user collections
  (`com.adobe.ag.library.collection`) from LRC's own per-module scratch state (`*.unsaved` kinds) —
  the real catalog had 1 real collection and 4 scratch rows. No smart collections existed to
  measure against; their rule-criteria mapping onto #23's filter bar stays a flagged follow-up.
- **Develop settings (#46/#47/#49/#39/#42/#51)**: the `s = { Key = Value, ... }` Lua-literal format
  ADR-0002 predicted, parsed by the `agprefs` crate (MIT) with **zero failures across 380,307 real
  rows** — settles adopt-vs-hand-roll in `agprefs`'s favor. 197 distinct real keys, 191 (97%)
  classified into an owner ticket by an exact-match table (grouped by real Develop-module panel,
  not guessed from naming conventions — a first prefix-only draft missed 83/197 keys, mostly
  `Enable*` panel toggles and per-channel HSL keys with no shared prefix). 6 keys are a real,
  current gap: `FilterList`/`AllowFilters` (AI filter stack), `LensBlur` (synthetic depth blur),
  `Preset`/`ToggleStyleAmount`/`ToggleStyleDigest` (preset bookkeeping) — none has an owner ticket
  yet. Import policy: keep every row's raw LRC text verbatim in a provenance blob, translate only
  what's needed at import time.
- **`.lrcat-data` blobs**: ~590 files, ~270MB each, referenced by `hasBigData` (26,195 of 380,307
  develop-settings rows) — exact linkage unconfirmed, flagged as a follow-up, not resolved.
- **#53 feasibility**: 380,300 assets, ~1.77M develop-history-step rows (~4.7/image) give a real,
  usable before/after training set. Dataset construction itself is #53's own scope.
- **`lrcat-extractor` (MPL-2.0) — real Cargo constraint found, not adopted as a dependency**: its
  own `rusqlite = "0.38"` pin cannot coexist in this workspace's single Cargo.lock with `den`'s
  unconditional `rusqlite = "^0.40"` — Cargo's `links = "sqlite3"` uniqueness is enforced
  workspace-wide, not per binary, and this holds even with `lrcat-extractor` behind an optional,
  default-off feature (Cargo still solves for every feature combination the workspace could
  activate). Evaluated standalone (outside the workspace) instead: it opens/reads the real v13
  catalog fine; a full feature-parity comparison against `shed`'s own reading is deferred to #62,
  once `den` is gone or `lrcat-extractor` gets its own isolated build.
- **Version support**: only v13 (the user's current LRC) was available to measure; older-version
  support is a follow-up if/when needed, not built speculatively.
