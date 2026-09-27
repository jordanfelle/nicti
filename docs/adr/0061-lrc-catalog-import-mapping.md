# ADR-0061: Lightroom Classic catalog import mapping

- **Status:** Accepted
- **Date:** 2026-09-26
- **Ticket:** [#61](https://github.com/jordanfelle/nicti/issues/61) Research: .lrcat schema mapping
- **Formerly:** ADR-0023 (sequential numbering, pre-#183)

## Context

#62 (phase-2 LRC catalog import) and #53 (AI auto-tone training dataset) are both blocked on #61:
neither can be planned until Nicti knows what's actually in a `.lrcat` file. Adobe has never
published the schema (ADR-0021's footnote cites camerahacks/lightroom-database and
hfiguiere/lrcat-extractor's `doc/lrcat_format.md` as the best available secondary sources), and
until this pass, the repo had no field-level LRC-to-Nicti mapping anywhere — ADR-0021 named #61 as
the owner of that mapping and stopped there.

This ADR is grounded in the user's own real catalog: a 10.7GB, 380,300-asset `.lrcat` (matching
ADR-0029's own asset count from the same library, extracted the same way — a closed catalog
backup, read-only, aggregate findings only, never the live working copy). `spikes/shed` is the
throwaway crate this research produced; every number below is `shed`'s measured output against
that real file, not an estimate.

**Privacy note (this repo is public, and this catalog's keywords include real people's names):**
nothing below is a keyword, collection name, path, or filename. `shed privacy-check` greps every
file this PR touches against the real catalog's own keyword/collection/path-segment set before
each commit, and every number cited is a count, not a value.

## Decision

### Q1: Folder model

LRC's model (`AgLibraryRootFolder` → `AgLibraryFolder` → `AgLibraryFile` → `Adobe_images`) maps
directly onto ADR-0071's `volume`/`root`/`asset` three-level schema: `AgLibraryRootFolder` is
`root`, `AgLibraryFolder` (parented, nested) collapses into `asset.rel_path`'s directory portion,
and `AgLibraryFile` + `Adobe_images` together are `asset`.

Measured: 13 root folders, all 13 with a drive-letter-prefixed `absolutePath` (confirming
ADR-0071's assumption that LRC keys roots by drive letter, not a stable volume id), and 2 of the 13
additionally carry a non-empty `relativePathFromCatalog` (LRC's own portable-catalog fallback) —
this split is real and not universal, so #62's importer must handle a root having only an absolute
path. 1,057 folders, 380,298 files (2 fewer than `Adobe_images`' 380,300 — `Adobe_images` includes
2 virtual copies, which share `AgLibraryFile` rows with their master via `masterImage`, not a
missing-file bug). `AgLibraryFile.md5`/`importHash` were flagged here as candidate relink inputs
for #71's tiers, not yet cross-checked against `spikes/homing`'s own fingerprints in this pass —
**#158/ADR-0158 resolved this and found the original (b)/(a) tier pairing was wrong in kind**: an
MD5 value cannot seed a BLAKE3 tier at all, and if `md5` is a full-file hash it's closer to tier
(c) (excluded from routine import for the DNG-rewrite reason ADR-0071 already gives), not tier (b).
See ADR-0158 for the measured resolution.

### Q2: Library metadata

Ratings, picks, and color labels follow ADR-0021:183-188's "follow LRC conventions" rule directly:
`Adobe_images.rating` (REAL, `NULL`-able — 277,137 of 380,300 assets have no rating at all, not
zero), `.pick` (0/1, virtually never set: 1 non-default value across the whole library), and
`.colorLabels` (a label name string, e.g. `"Red"`; virtually unused here too, 4 non-empty rows).
**Real gotcha found**: `pick`/`rating` are stored as SQLite `REAL`, not `INTEGER` — a naive `SUM()`
or integer cast on `hasRetouch` (a *different*, 5-character bitmask-string column on
`Adobe_imageDevelopSettings`, not a rating/pick field) silently produces a nonsense number rather
than an error, since SQLite's numeric-affinity coercion parses only the leading digits of a string
like `"00101"`. #62's importer must treat every LRC boolean-looking column by its declared type,
not assume 0/1 integer semantics from the column name alone.

Virtual copies: 2 found (`Adobe_images.masterImage IS NOT NULL`), confirming #62 must model a
virtual copy as an additional edit-variant row against the same master asset, not a distinct file.

### Q3: Collections

`AgLibraryCollection.creationId` distinguishes real collections from LRC's own scratch state: this
catalog has exactly 1 row with `com.adobe.ag.library.collection` (a genuine user collection) and 4
`*.unsaved` rows (`layout.book`, `print`, `slideshow`, `webGallery` — LRC's own per-module working
state, never user-visible as a "collection," and never worth importing). **No smart collections
exist in this catalog** — the `com.adobe.ag.library.smart_collection` kind and its rule-criteria
storage are real per secondary sources but couldn't be measured against real data this pass;
mapping smart-collection rule criteria onto #23's filter bar stays unverified, flagged as a
follow-up. `AgLibraryCollectionImage`: 453 rows, the real collection's membership.

### Q4: Develop settings

`Adobe_imageDevelopSettings.text` and `Adobe_libraryImageDevelopHistoryStep`'s own payload are both
the same Lua table-literal format ADR-0021's footnote predicted: `s = { Key = Value, ... }`. The
`agprefs` crate (MIT, already on `deny.toml`'s allowlist) parses this format directly —
**`Agpref::parse` succeeded on all 380,307 real rows in this catalog, zero parse failures** —
settling the adopt-vs-hand-roll question this ADR's Context raised: adopt `agprefs`, don't hand-roll
a second Lua-literal parser.

197 distinct keys appear across the real catalog. `spikes/shed/src/develop.rs`'s `classify_key`
sorts all of them into an owner ticket by an exact-match table built from this real key set,
grouped by Develop-module panel (updated by #157, which resolved the 6 keys the initial pass left
unowned — see below):

| Owner ticket | Panel(s) | Key count |
|---|---|---|
| #42 (color) | Camera profile, white balance, HSL, split toning, color grading, tone curve, grayscale mix, Point Color, Look | 69 |
| #46 (global) | Basic/Tone, Detail (sharpen + luminance NR), Effects (grain + vignette), HDR/SDR rendition, auto-tone, parametric curve | 56 |
| #47 (crop/geometry) | Transform: crop, manual/auto perspective, Upright | 33 |
| #39 (lens) | Lens Corrections: profile-based + manual distortion/vignette, defringe (CA) | 24 |
| #51 (heal) | Spot heal, clone stamp, legacy red-eye, AI distraction removal, AI People/Reflection Removal (#157) | 7 |
| #40 (AI denoise) | `FilterList`/`AllowFilters`, key-level default (see #157 below) | 2 |
| #49 (masks) | Local adjustment groups, range masks | 2 |
| #52 (presets) | `Preset`, `ToggleStyleAmount`, `ToggleStyleDigest` (#157) | 3 |
| provenance-only, no owner ticket | `LensBlur` (#157) | 1 |

**Scope note added by #46 (2026-09-27)**: this table is `spikes/shed`'s measured *key-import*
routing, unchanged by #46 -- `classify_key` still sorts white balance/HSL/tone-curve LRC keys to
`#42` here, and #62's future importer should keep doing so. Separately, #46 (not #62) is where
those same three *user-facing sliders* (WB temp/tint, the parametric + point tone curve, HSL) are
actually implemented as Tapetum render stages, rather than waiting on #42's still-Proposed
DCP-profile color pipeline -- #42 keeps the DCP HueSatMap/LookTable camera-profile machinery, the
working-space pick, and display/export color management. When #62 lands, its importer routes
these three keys' *values* into the `nicti.wb`/`nicti.tone_curve`/`nicti.hsl` stage params #46
defines, not into a `#42`-owned stage.

**#157 resolved the 6 originally-unowned keys** (`FilterList`/`AllowFilters`/`LensBlur`/`Preset`/
`ToggleStyleAmount`/`ToggleStyleDigest`) by measuring real presence/active-usage counts against
this same catalog, rather than guessing from the key names alone:

- **`FilterList`/`AllowFilters`** turned out to gate 4 distinct LRC AI filters, not one, with very
  uneven real usage across `FilterList.Filters[].Title` entries: **20,303 Denoise, 47 People
  Removal, 6 Super Resolution, 1 Reflection Removal** — summing to 20,357 *entries* across 20,356
  *active rows* of 380,307 (one row has 2 `Filters[]` entries; entry count and row count are
  different things, both real, not a typo). `classify_key` operates per-key, not per-filter-entry,
  so it defaults `FilterList`/`AllowFilters` to `#40` (the dominant case) — but #62's importer must
  inspect each `Filters[]` entry's `Title` and route People/Reflection Removal to **#51** and Super
  Resolution to the new **#174** instead of assuming every entry is Denoise.
- **`LensBlur`** is present in nearly every row (380,300/380,307) but *always* as an empty
  bookkeeping table (`{  }`) — **0 rows had real content**, i.e. the feature has never actually
  been used in this catalog. Not worth a render-owning ticket for zero real usage; #62 keeps it
  verbatim in the provenance blob (`Owner::ProvenanceOnly`) rather than silently dropping it.
- **`Preset`/`ToggleStyleAmount`/`ToggleStyleDigest`** are style-preset apply/toggle bookkeeping
  (which preset was last applied, digest of its content), not develop parameters themselves →
  **#52** (presets, copy/paste, sync).

See `spikes/shed/src/develop.rs`'s `analyze_unowned_keys`/`UNOWNED_KEYS` (the `shed develop-usage`
subcommand) for the measurement, and its module doc comment for the full reasoning.

`Enable*` boolean toggles (`EnableLensCorrections`, `EnableRetouch`, `EnableSplitToning`, etc.) are
filed under the panel they gate, not a separate bucket — a first draft of `classify_key` used
prefix-only matching and missed all of these (no shared prefix with their panel's other keys),
which is why the real table is exact-match-first with a suffix/prefix fallback for a future
key this catalog didn't happen to contain (documented in `develop.rs`'s own doc comment).

Mask/AI-develop flags: `hasMasks` set on 13,258 of 380,307 rows, `hasAIMasks` on 12,015,
`hasBigData` (external large-data blob reference, see Q5) on 26,195. `processVersion`: 380,300 rows
at `15.4` (current), 7 at `10.0` (legacy, pre-2012-process) — #62's importer needs both, but can
treat `10.0` as rare enough not to block v1 scope.

**Import policy**: keep every row's raw LRC develop-settings text verbatim in a provenance blob on
import (nothing is lost, including the 6 unowned keys and any future key), and translate only the
keys #62 actually needs at import time — this ADR settles *which* keys map to *which* ticket, not
the per-parameter conversion math (that's each owner ticket's own scope).

### Q5: `.lrcat-data` blobs

The backup zip's `.lrcat-data` directory holds ~590 files named `<id>.blob`, ~270MB each (measured
from the zip's own directory listing, never extracted — ADR-0018 forbids committing real Adobe
data, and extracting 590 files at ~270MB each was out of scope for this pass's storage budget).
`hasBigData`'s 26,195-row count on `Adobe_imageDevelopSettings` is the only in-catalog signal this
pass found referencing them; the exact blob-to-asset linkage (whether it's `historySettingsID`,
`digest`, or a separate id) is unconfirmed. Flagged as a follow-up, not resolved here.

**Resolved by #156/ADR-0156**: `.lrcat-data` is a RocksDB database with integrated BlobDB, not a
bespoke format; its keys are `MaskDigest`/`OriginalInstanceDigest` content-addressed digests
embedded in the develop-settings Lua text (not `historySettingsID` or the catalog's `digest`
column — neither matched). #62's importer does not need to read it: see ADR-0156 for the full
linkage and the re-derive-don't-migrate policy.

### Q6: #53 (AI auto-tone training dataset) feasibility

380,300 assets, only 2 with no develop settings row at all (`Adobe_imageDevelopSettings` has
380,307 rows against 380,300 images). Those two counts don't fully reconcile as reported: if
exactly 2 images have zero rows, the remaining 380,298 images must together account for at least
9 more rows than images, not 7 — this pass measured the row/image counts but never actually
queried which images have more than one settings row (multiple snapshots? a duplicate?) or
confirmed the "2 missing" count against a `LEFT JOIN`. Both the exact reconciliation and its
mechanism are unconfirmed and flagged as a follow-up, not resolved here. A "before" state is
reconstructable from the first
`Adobe_libraryImageDevelopHistoryStep` row per image (1,771,117 total history-step rows across
380,300 images, avg ~4.7 steps/image) or from LRC's known per-process-version defaults. This
catalog is large enough to be a real, usable training set — dataset construction specifics (which
before/after pairs, how process-version defaults are sourced) are #53's own scope, not resolved
here.

### Q7: Version support

Only catalog version 13 (per this backup's filename and the RootFolder/schema shape) was available
to measure. Policy: #62 targets v13 as the baseline (matching the user's own current LRC version);
older-version support is a follow-up if/when a v12-or-earlier catalog ever needs importing, not
built speculatively now.

### Q8: `lrcat-extractor` adopt/fork

**Not adopted as a compiled dependency — evaluated standalone instead, and this is itself a real
finding.** `lrcat-extractor` 0.7.0 (MPL-2.0, already allowlisted) hard-pins `rusqlite = "0.38"` in
its own manifest. This workspace's `den` spike unconditionally requires `rusqlite = "^0.40"`, and
Cargo's `links = "sqlite3"` uniqueness rule is enforced across the **entire workspace's single
Cargo.lock**, not per binary target — confirmed by reproducing the exact failure
(`cargo check -p shed` alone fails to resolve once `lrcat-extractor` is added as *any* dependency,
even an optional, default-off one, since Cargo still solves for every feature combination the
workspace could activate). Attempting to force unification by pinning `shed`'s own `rusqlite` to
`"0.38"` to match `lrcat-extractor` does not help — it just relocates the same conflict onto `den`'s
`^0.40` requirement instead. This is the same class of issue CLAUDE.md's package map already
documents for `den`'s own `libsql`-vs-`sqlite` pairing, but manifesting here as a hard Cargo
resolve-time error rather than a link-time symbol collision, and across two entirely different
spike crates rather than within one.

Given that, `lrcat-extractor` was evaluated as a fully standalone scratch crate (outside this
workspace, per this session's own scratch directory, never committed) against the real catalog.
It opened and read the real v13 catalog without error via its own `rusqlite` 0.38. A full
feature-parity comparison against `shed`'s own schema/develop reading wasn't completed in this
pass — #62 should re-evaluate adopting it at that point, now armed with the real Cargo-graph
constraint above (either the workspace's `den` spike is gone by then per its own deletion plan, or
`lrcat-extractor` needs its own isolated build, e.g. a separate Cargo workspace under
`tools/` outside this repo's own `[workspace]` member glob).

## Consequences

- **#62** inherits: the root/folder/file→volume/root/asset mapping (Q1), the develop-key→owner
  table (Q4) as its translation starting point, the `pick`/`rating` type gotcha (Q2), and the
  provenance-blob-first import policy (Q4).
- **#53** inherits: 380,300 real assets, ~1.77M history-step rows for before/after pairs, and this
  ADR's confirmation that the dataset is large enough to be usable (Q6) — dataset construction is
  #53's own scope.
- **New follow-up issues** (filed alongside this ADR, `**Part of:** #11`):
  - Smart-collection rule-criteria mapping onto #23's filter bar — unverified, no smart collections
    existed in the real catalog to measure against (Q3).
  - ~~`.lrcat-data` blob linkage and whether #62 needs to import them (Q5)~~ **Resolved by
    #156/ADR-0156.**
  - ~~The 6 unowned develop-setting keys~~ **Resolved by #157** (Q4): `FilterList`/`AllowFilters`
    default to #40 with a real per-filter-type split #62 must apply (#51 for People/Reflection
    Removal, the new #174 for Super Resolution); `LensBlur` is provenance-only (zero real usage
    measured); `Preset`/`ToggleStyleAmount`/`ToggleStyleDigest` → #52.
  - ~~`AgLibraryFile.md5`/`importHash` cross-check against `spikes/homing`'s relink fingerprints
    (Q1)~~ **Resolved by #158/ADR-0158.**
  - `lrcat-extractor` adopt/fork re-evaluation once the `den`/`rusqlite` version conflict is no
    longer live in this workspace (Q8).
- **`spikes/shed`** stays in the repo as #62's own starting point (schema/inventory/develop/
  privacy-check commands), same "don't build on top of a spike, expect deletion once promoted"
  caveat as every other `spikes/*` crate.

## Measured results

All numbers above are `shed`'s real output against the user's own 380,300-asset catalog backup
(`shed inventory`/`shed develop`, see `docs/research/shed-lrcat-schema.md` for the full tables).
Sandbox-measured, real (not TBD):

- `cargo test -p shed`: 17/17 unit tests pass on Unix (16/16 on Windows; the URI-special-character
  test is Unix-only, see `open.rs`'s own doc comment) — open-guard live-catalog detection
  (including a real false-positive this pass found and fixed), inventory aggregation, develop-key
  classification (pinned against the real 197-key catalog dump), and privacy-check.
- `cargo clippy -p shed --all-targets -- -D warnings`: clean.
- `cargo fmt --all -- --check` and the full workspace `cargo clippy --workspace --exclude den
  --exclude pelt-egui --exclude pelt-iced --exclude pelt-slint --exclude retina --all-targets
  --all-features -- -D warnings` / `cargo test` (same excludes): clean, `shed` included (not
  path-gated — like `sniff`/`homing`, it needs no heavy native build).
- `cargo deny --workspace --all-features check licenses`: `licenses ok` — `agprefs` (MIT) needs no
  new `deny.toml` entry (MIT is already allowlisted).
- Real-catalog run: 380,300 assets, 380,307 develop-settings rows, 0 parse failures, 197 distinct
  develop keys (191 classified, 6 confirmed-unowned), 137 tables inventoried, 16,225,311 total rows
  across the catalog.
