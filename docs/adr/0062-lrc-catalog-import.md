# ADR-0062: Lightroom Classic catalog import (the build)

- **Status:** Accepted
- **Date:** 2026-10-01
- **Ticket:** [#62](https://github.com/jordanfelle/nicti/issues/62) Build: phase 2 — LRC catalog
  import

## Context

ADR-0061 (#61), ADR-0156 (#156) and ADR-0158 (#158) mapped the `.lrcat` schema and settled the
import policy; #60 shipped phase 1 (XMP coexistence). This is phase 2: bring a Lightroom Classic
library into nicti. The ticket's own words: metadata first, then a best-effort translation of
develop settings. Constraints already decided: read a **closed backup** only, never `.lrcat-data`
(ADR-0156), never seed relink hashes from LRC data (ADR-0158), keep the verbatim develop text as
provenance (ADR-0061 Q4), and `agprefs` parses the Lua.

## Decision

A new crate, **`crates/nicti-stray`** (a stray taken into a new home; `shed` is the in-app updater),
runs the import as one cancellable Pounce job (`LrcImportJob`, CPU lane, `JobKind::Import`). Its
LRC-reading half is promoted from `spikes/shed` (`open_backup`'s live-catalog guard,
`join_lrc_path`); the spike stays as the research/analysis tool.

**Matching: ingest, then match.** Each LRC root folder is registered (same placeholder volume as
the Import button, via `nicti_lair::scruff::register_root`) and ingested by the normal Scruff pass,
so fingerprints, previews and EXIF are computed fresh. LRC images are then matched to cataloged
assets by `(root, case-folded rel_path)`. A root whose folder is absent on this machine, or an
image with no file, is counted **missing and skipped** — never invented. Root paths are normalised (no trailing separator) so the same folder is the same `root` row as the
Import button's, not a second one that would ingest every file twice. A user-supplied prefix remap
handles moved drives; a drive-letter path maps to `/mnt/<x>/` off Windows.

**Phases** (one bounded chunk per `step()`): open → ingest → match → keywords → collections →
items → mark-dirty. Keywords and collections are found-or-created and only ever *added*.
`CatalogStore::apply_lrc_chunk` writes each 500-image chunk in one transaction.

**Markers.** LRC's *absence* of a value never erases one nicti already has (a photo rated in nicti,
or newer XMP): only a rating/flag/label LRC actually carries overrides, on the first run and on
re-runs. `rating` NULL → no change; `pick = -1` → reject (stored on `rating`, stars kept in
provenance); `pick = 1` → flag; `colorLabels` → label. Virtual copies become extra non-master
`edit_variant` rows named by `copyName`; only the master image's markers apply (nicti's are per
asset). Imported assets are marked catalog-dirty (`mark_catalog_dirty_many`) so the XMP sidecar sync
does not revert them on the next rescan.

**Idempotent, never destructive.** Schema v10 adds `lrc_provenance` (keyed by the `edit_variant`;
unique on `Adobe_images.id_global`) holding the verbatim develop text, `importHash`, the
mask/AI/big-data flags, IPTC caption/copyright (no nicti field yet), raw rating/pick, the
untranslated-key list, and the BLAKE3 of the document/markers the import last wrote. A re-run
overwrites a document or marker set only while it still equals that hash; otherwise the user's
edit in nicti is kept and counted (`kept_local_*`). Two LRC masters that resolve to one asset (two
roots remapped to one folder), or an LRC image that now resolves to a different asset than last
time, are left untouched and counted (`skipped_conflicts`) rather than corrupting the provenance
row. Crop pixels use LRC's own `fileWidth`/`fileHeight` (`asset.width/height` is only the T0
preview's declared size). The first import does override what XMP ingest
filled in, but never a master that already carries edits.

**Failure model.** `step()` never returns `Err` (Pounce would drop the job without resolving the
slot); a failure lands in `LrcImportReport::error`, and `Drop` resolves the slot as cancelled.

### Develop translation

Slider units: LRC's global sliders are -100..100 → nicti's -1..1; exposure stays in stops.

Translated: Exposure2012, Contrast/Highlights/Shadows/Whites/Blacks2012, Vibrance, white balance
(`As Shot` is left to nicti; any other preset is an explicit override), the four parametric
tone-curve sliders, the eight HSL bands, Sharpness/Radius/Detail, luminance/colour noise
reduction, and the crop rectangle; plus `MaskGroupBasedCorrections` → `nicti.masks`. The
`Enable*` toggles that exist in real catalogs (Color adjustments, Detail, Mask corrections)
zero what they gate; a panel with no toggle key is simply always on. Only `processVersion` below 11 (PV2003/2010) skips translation; every later version uses the
PV2012 keys. LRC's untouched-image defaults (Sharpness 40,
ColorNoiseReduction 25 …) are imported faithfully — that is what the photo looks like in LRC.

**Conventions verified against the real v13 catalog** (not just the #49 note):

- `LocalExposure2012` is stored as **stops/4** (every real value is a multiple of the UI's 0.05 step
  only after ×4); the other `Local*` sliders are already -1..1.
- A radial gradient's `MaskInverted = true` pairs with *darkening* in 1,804 of 1,863 real cases
  (effect applied outside) → nicti's own `invert`.
- AI mask rasters (`FullMaskSize`) keep the **uncropped** frame's aspect on cropped photos → mask
  space is the uncropped frame, nicti's own convention.
- `Mask/Image` `MaskSubType`: 1 = Subject, 2 = Sky; `0` + `MaskSubCategoryID 22` = Background
  (subject, inverted); `3`/other categories (People, objects) have no nicti model.
- Brushes are `Mask/Aggregate` → `Mask/Paint` with `Dabs` strings (`M`/`d`/`r`/`h`/`f` records) and
  a `Radius` whose units are not pinned (values up to 3.7) → not translated.

**Not translated** (kept in provenance, counted in the report, listed in the untranslated-key
histogram): brushes, People/object masks, range masks, **the whole correction** when any component
is untranslatable (a partial selection would be wrong); geometric masks and crops on rotated
originals or with a straighten angle (LRC's sign/orientation convention is unpinned), and radials with a non-zero `Angle` or `Roundness`;
a radial's `Feather` is applied as nicti's outward feather (not verified against LRC renders); a
skipped correction also leaves `MaskGroupBasedCorrections` in the untranslated list; retouch
areas; global Clarity/Texture/Dehaze/Saturation, point curves, colour grading, calibration, lens,
transform/Upright, grain/vignette, `CameraProfile` (a name with no resolvable `.dcp` has no render
effect, so no stage is written), `FilterList` (counted per `Title`: Denoise / removal / Super
Resolution), PV2003/2010 images. Non-RAW originals (JPEG/TIFF/PSD/video) are not Scruff candidates, so they match nothing and are
reported as missing. A subset import (`only_roots`) creates only the keywords and collections that
hold a matched photo. Smart collections are counted, not imported (rule mapping
unverified, ADR-0061 Q3). `.lrcat-data` is never read.

## Measured (real 380,228-image v13 backup, `tests/real_catalog.rs`, ignored by default)

0 develop-parse failures; 380,153 images produce a translated edit; 13 s for the whole catalog
(release). 32,858 mask corrections translated (32,521 AI components), 2,076 skipped; 259 crops
translated, 21,550 skipped (overwhelmingly rotated originals: 79,206 of 380k are `DA`); 293 retouch
areas skipped; 1,533 images moved a tone-curve split point; denoise 20,381 / removal 48 / super
resolution 6. A real read also caught a bug no synthetic fixture could: `fileWidth`/`fileHeight`
are stored as `REAL`, which `rusqlite`'s `i64` refuses (ADR-0061 Q2's "read by declared type" rule,
now applied to every numeric column).

## Consequences

- Follow-ups filed under #11: global Clarity/Texture/Dehaze/Saturation/grain/vignette (#46); point
  curves, colour grading, calibration and `.dcp` resolution (#42); lens (#39); transform (#47);
  AI removal (#51); Super Resolution (#174); brush masks and retouch areas (units + source offset);
  crop/geometric masks on rotated originals and `CropAngle`; smart collections; IPTC fields; a
  variant UI; a native file picker; PV2003/2010; older catalog versions.
- Gradient mask space on *cropped* photos is verified only for AI rasters; gradients assume the
  same. Pin with a real LRC render comparison.
- `lrcat-extractor` stays unused (ADR-0061 Q8): reading is plain `rusqlite` + `agprefs`.
