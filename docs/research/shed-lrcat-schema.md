# #61: .lrcat schema mapping (`spikes/shed`)

See `docs/adr/0023-lrc-catalog-import-mapping.md` for the decision and its rationale. This document
is the write-up: the full table-by-table map, the develop-key→owner table, and the inventory
aggregates `shed` measured against the user's own real 380,300-asset catalog backup (never the
live working copy, never a keyword/collection/path value — see the ADR's privacy note).

## Table inventory

137 tables total (`shed schema`), 16,225,311 rows across all of them. The tables #61's own scope
names, with real row counts from the measured catalog:

| Table | Rows | Role |
|---|---|---|
| `Adobe_images` | 380,300 | asset (rating/pick/color label/develop-cache/orientation) |
| `AgLibraryFile` | 380,298 | file identity (name, extension, md5, importHash) |
| `AgLibraryFolder` | 1,057 | nested folder path, parented |
| `AgLibraryRootFolder` | 13 | volume-level root, drive-letter absolute path |
| `AgLibraryKeyword` | 208 | keyword tree node (genealogy = `/`-separated ancestor-id chain) |
| `AgLibraryKeywordImage` | 748,535 | keyword↔asset link |
| `AgLibraryKeywordSynonym` | 0 | keyword synonym (unused in this catalog) |
| `AgLibraryCollection` | 5 | collection/scratch-state row (see below) |
| `AgLibraryCollectionImage` | 453 | collection membership |
| `AgLibraryIPTC` | 380,300 | caption/copyright/accessibility text, 1:1 with `Adobe_images` |
| `AgLibraryFolderStack` | 19,437 | folder-level stack marker |
| `Adobe_imageDevelopSettings` | 380,307 | current develop state, Lua-literal `text` column |
| `Adobe_libraryImageDevelopHistoryStep` | 1,771,117 | per-edit history steps (~4.7/image) |

## Folder model

All 13 real root folders have a drive-letter-prefixed `absolutePath` (`[A-Za-z]:...`), confirming
ADR-0020's assumption that LRC keys roots by drive letter rather than a stable volume id. 2 of the
13 *also* carry a non-empty `relativePathFromCatalog` (LRC's own portable-catalog fallback,
`../../<drive>:/...`) — not universal, so #62 must not assume every root has one.

380,298 `AgLibraryFile` rows against 380,300 `Adobe_images` rows: the 2-row gap is exactly this
catalog's 2 virtual copies (`Adobe_images.masterImage IS NOT NULL`), which share their master's
`AgLibraryFile` row rather than having their own file-system entry — expected, not a bug.

## Library metadata

| Field | Distribution (measured) |
|---|---|
| `fileFormat` | JPG 274,499 · RAW 68,352 · DNG 37,449 |
| `pick` (REAL, 0/1) | 0.0 → 380,299 · 1.0 → 1 |
| `rating` (REAL, nullable) | none → 277,137 · 1★ → 246 · 2★ → 61 · 3★ → 2,135 · 4★ → 25,602 · 5★ → 75,119 |
| `colorLabels` | none → 380,296 · Green → 2 · Red → 2 |

**Real gotcha**: `pick`/`rating` are SQLite `REAL` columns, not `INTEGER` — `Adobe_images` declares
them with no explicit type affinity beyond a default. A separate, unrelated column
(`Adobe_imageDevelopSettings.hasRetouch`) is a **5-character bitmask string** (e.g. `"00101"`), and
a naive `SUM()` across it silently coerces to a number via SQLite's leading-digit numeric affinity
rather than erroring — this was caught only by cross-checking `GROUP BY hasRetouch` against the
`SUM()` result and finding they didn't agree. #62 must read every column by its real declared type,
not assume boolean/integer semantics from a column name that merely sounds like one.

## Collections

| `creationId` | Count | Real meaning |
|---|---|---|
| `com.adobe.ag.library.collection` | 1 | a genuine user-created collection |
| `com.adobe.ag.layout.book.unsaved` | 1 | Book module scratch state |
| `com.adobe.ag.print.unsaved` | 1 | Print module scratch state |
| `com.adobe.ag.slideshow.unsaved` | 1 | Slideshow module scratch state |
| `com.adobe.ag.webGallery.unsaved` | 1 | Web Gallery module scratch state |

Only the first kind is user data worth importing; the four `*.unsaved` rows are LRC's own per-module
working-state placeholders and always exist regardless of whether the user ever opens that module.
**No smart collections** (`com.adobe.ag.library.smart_collection`) existed in this catalog — their
rule-criteria storage (`AgLibraryCollectionContent`'s Lua-literal text, per secondary sources)
couldn't be measured against real data this pass; flagged as a follow-up.

## Keywords

208 keyword nodes, max nesting depth 4 (from `AgLibraryKeyword.genealogy`'s `/`-separated ancestor
chain), 0 synonym rows (unused in this catalog, though the table exists), 748,535
keyword↔asset links.

## Develop settings

`agprefs::Agpref::parse` against every `Adobe_imageDevelopSettings.text` row: **0 parse failures
across 380,307 rows.** 197 distinct keys observed. `classify_key`'s exact-match table (built from
this real key set, not guessed) now sorts every one of them to an owner (updated by #157, which
resolved the 6 this pass originally left unowned):

| Owner | Real key count | Panel(s) |
|---|---|---|
| #42 color | 69 | camera profile, white balance, HSL, split toning, color grading, tone curve, grayscale mix, Point Color, Look |
| #46 global | 56 | Basic/Tone, Detail (sharpen + luminance NR), Effects (grain + vignette), HDR/SDR rendition, auto-tone, parametric curve |
| #47 crop/geometry | 33 | Transform: crop, manual/auto perspective, Upright |
| #39 lens | 24 | Lens Corrections: profile-based + manual distortion/vignette, defringe (CA) |
| #51 heal | 7 | spot heal, clone stamp, legacy red-eye, AI distraction removal, AI People/Reflection Removal |
| #40 AI denoise | 2 | `FilterList`/`AllowFilters`, key-level default -- see below |
| #49 masks | 2 | local adjustment groups, range masks |
| #52 presets | 3 | `Preset`, `ToggleStyleAmount`, `ToggleStyleDigest` |
| provenance-only | 1 | `LensBlur` -- present everywhere, never actually used |

**#157's `shed develop-usage` measurement** (`analyze_unowned_keys`) resolved the original 6
unowned keys (`FilterList`, `AllowFilters`, `LensBlur`, `Preset`, `ToggleStyleAmount`,
`ToggleStyleDigest`) by counting real presence and active usage against this same catalog:

- `FilterList`/`AllowFilters` gate 4 distinct AI filters, with real but very uneven usage:
  **20,303 Denoise, 47 People Removal, 6 Super Resolution, 1 Reflection Removal** entries (of
  380,307 rows), read from `FilterList.Filters[].Title` -- summing to 20,357 entries across 20,356
  active rows (one row holds 2 `Filters[]` entries). `classify_key` is per-key, so it defaults both
  to `#40` (the dominant case) -- `#62`'s importer must still inspect each `Filters[]` entry's
  `Title` to route People/Reflection Removal to `#51` and Super Resolution to the new `#174`.
- `LensBlur` is present in 380,300/380,307 rows but **always as an empty bookkeeping table**
  (`{  }`) -- 0 rows had real content. The AI Lens Blur filter has never actually been used in this
  catalog; kept verbatim in the provenance blob, no render-owning ticket.
- `Preset`/`ToggleStyleAmount`/`ToggleStyleDigest` (960/11/11 rows respectively) are style-preset
  apply/toggle bookkeeping, not develop parameters -> `#52`.

`Enable*` boolean toggles (e.g. `EnableLensCorrections`, `EnableRetouch`, `EnableSplitToning`) are
filed under the panel they gate rather than a separate bucket — this needed an exact-match table,
not prefix matching: a first draft using prefix/substring heuristics only classified 114/197 keys
(58%), missing every `Enable*` toggle (no shared prefix with its panel's other keys) and every
per-channel HSL key from either naming generation (`BlueHue`/`SaturationAdjustmentBlue` share no
common substring). The exact-match table plus a suffix/prefix fallback (for a key a *different*
catalog might contain that this one didn't) closed that gap, and #157 closed the remaining 6.

Mask/AI flags: `hasMasks` set on 13,258/380,307 rows, `hasAIMasks` on 12,015, `hasBigData`
(external large-data reference, see below) on 26,195. `processVersion`: 380,300 rows at `15.4`
(current), 7 at `10.0` (legacy pre-2012 process) — rare enough not to block v1 scope, but #62 must
still handle both.

## `.lrcat-data` blobs

The backup zip's `.lrcat-data` directory lists ~590 `<id>.blob` files, ~270MB each (from the zip's
own directory listing — never extracted; ADR-0003 forbids committing real Adobe data, and 590
files at ~270MB each was out of this pass's storage budget). `hasBigData`'s 26,195-row count is the
only in-catalog signal this pass found referencing them; the exact blob-to-asset linkage is
unconfirmed — flagged as a follow-up for whoever picks up the filed issue.

## #53 (AI auto-tone) feasibility

380,300 assets, 1,771,117 develop-history-step rows (~4.7 steps/image average) — a real, sizeable
before/after training set. `Adobe_imageDevelopSettings` has 380,307 rows against 380,300 images,
and only 2 images have zero settings rows — those two counts don't fully reconcile (the remaining
380,298 images must then account for at least 9 more rows than images, not 7), since this pass
never queried which images have more than one row or confirmed the "2 missing" count via a
`LEFT JOIN`. Flagged as an open reconciliation, not resolved here.

## `lrcat-extractor` evaluation

Not adopted as a compiled workspace dependency — see ADR-0023 Q8 for the full Cargo `links`
uniqueness conflict this pass found with `den`'s `rusqlite = "^0.40"` pin. Evaluated standalone
(outside this workspace, in a scratch Cargo project, per this session): it opened and read the
real v13 catalog without error via its own `rusqlite = "0.38"`. A full feature-parity comparison
against `shed`'s own reading wasn't completed this pass.

## What's real vs. deferred

Real, measured this pass: table/column shapes (137 tables), all aggregate counts above, the
`agprefs` parse-failure rate (0/380,307), the develop-key classification (191/197 initially,
197/197 after #157), the collection
kind split, the root-folder drive-letter/relative-path split, and the `pick`/`rating` type gotcha.

Deferred, not resolved here: `.lrcat-data` blob linkage, smart-collection rule-criteria mapping (no
real example to measure against), `AgLibraryFile.md5`/`importHash` cross-check against
`spikes/homing`'s own fingerprint tiers, and the exact per-parameter develop-setting conversion
math (each owner ticket's own scope, not #61's).
