# ADR-0156: `.lrcat-data` blob linkage and #62 import policy

- **Status:** Accepted
- **Date:** 2026-09-27
- **Ticket:** [#156](https://github.com/jordanfelle/nicti/issues/156) Confirm `.lrcat-data` blob
  linkage and whether #62 needs to import them

## Context

ADR-0061 Q5 measured the backup zip's `.lrcat-data` directory from its listing alone (~590 files
named `<id>.blob`, ~270MB each) but never extracted it, leaving the blob-to-asset linkage
unconfirmed and #62 unable to decide whether its importer must read/migrate this data or can treat
it as safely re-derivable. This ADR extracts and analyzes it against the same real, closed catalog
backup ADR-0061/ADR-0158 used (never the live/locked copy — see this repo's privacy note below).

## Decision

### `.lrcat-data` is a RocksDB database with integrated BlobDB, not a bespoke format

Extracting the small metadata files (`CURRENT`, `MANIFEST-172863`, `OPTIONS-172865`, one 2.5MB
`.sst`) shows `.lrcat-data` is a **RocksDB 10.6.2** database. Its `OPTIONS` file has
`enable_blob_files=true`, `blob_file_size=268435456` (256MiB — matches ADR-0061's ~270MB
observation, since a blob log file rolls once it crosses this threshold), `min_blob_size=256`,
`blob_compression_type=kNoCompression`, and `enable_blob_garbage_collection=true`. `<id>.blob` is
therefore RocksDB's own blob-file naming (`<file_number>.blob`), not an LRC-specific id.

`rocksdb_sst_dump --command=scan --output_hex` (from `brew install rocksdb`) lists every key/value
in the `.sst`. Real keys are **32-character uppercase-hex ASCII strings** (one internal bookkeeping
key, `rocksdbIntegrityId`, is excluded). Each value decodes as a standard RocksDB blob-index record
(type byte `0x01` = `kBlobType`, then varint `file_number`, `offset`, `size`) pointing directly at
`<file_number>.blob` — no other structure to reverse-engineer.

### The keys are content-addressed digests embedded in develop-settings text, not any catalog column

None of the catalog's own `digest`-shaped columns match (`Adobe_imageDevelopSettings.digest`,
`historySettingsID`, `AgLibraryFileDigest.digest`, `AgRemotePhoto.developSettingsDigest`/
`metadataDigest`, `Adobe_AdditionalMetadata.internalXmpDigest`, etc. — zero overlap with the
RocksDB key set). The real linkage is inside the Lua-literal develop-settings text ADR-0061 Q4
already identified (`Adobe_imageDevelopSettings.text` and related `text` columns): two named
fields inside nested structures account for the overwhelming majority of keys:

- **`MaskDigest`** (21,124 distinct values, **100% overlap** with RocksDB keys) — appears inside a
  `MaskGroupBasedCorrections`-style local-adjustment mask entry (alongside `MaskID`, `MaskName`,
  `MaskSubType`, `MaskSyncID`). This is the AI local-adjustment mask raster (Select Subject/Sky/
  People, etc. — ADR-0048's masking scope).
- **`OriginalInstanceDigest`** (20,607 distinct, **100% overlap**) — appears inside an `ImageGroup`
  structure alongside `GroupDigest` (itself *not* a blob key — 0 overlap), `PixelType`, `Planes`,
  `SizeX`/`SizeY` matching the image's full resolution. This is the AI Denoise/Enhance enhanced
  raster (ADR-0040's denoise scope; the ~20,303-entry `FilterList` Denoise count ADR-0061/#157
  already measured is consistent with this).
- Three smaller `pm_*` fields (`pm_patch_variation`: 2,090, `pm_patch`: 1,062, `pm_patch_mask`:
  1,045 — all 100% overlap) are Photo Merge/panorama patch data, out of scope for either #48 or #40.

Together these account for **45,985 of 47,015 real blob keys (97.8%)**; the remaining 1,030
(2.2%) are unaccounted for by this pass — most likely orphaned entries pending RocksDB's own blob
garbage collection, or referenced through an encoding this pass's plain-text field scan didn't
catch. Not resolved further here; not blocking, since the policy below doesn't depend on 100%
accounting.

**Payload format**: peeking the first ~4KB of three `.blob` files (streamed directly out of the
zip, never fully extracted) shows each blob-file record's value begins with `II*\x00` — the TIFF
little-endian magic number. The payloads are raw TIFF-wrapped rasters (matching the RGB/3-plane
full-resolution shape seen in `ImageGroup`), not a portable interchange format.

### Import policy: #62's importer skips it; a golden-dataset use is a separate, deferred question

`.lrcat-data` is a local, content-addressed cache of **LRC's own proprietary AI outputs**
(Select Subject/Sky/People masks, AI Denoise/Enhance rasters, Photo Merge patches) — not
develop-parameter data, and reading/migrating a RocksDB BlobDB (hundreds of MB per catalog,
TIFF-wrapped, LRC-version-and-model-specific) is real complexity #62's *import* path has no use
for: nothing in the `volume`/`root`/`asset`/`edit_variant` schema (ADR-0071/ADR-0021) has a slot
for "a cached raster from someone else's AI model," and ADR-0048/ADR-0040 already commit Nicti to
running its own mask/denoise models rather than depending on LRC's at develop/render time.

**Correction to an earlier framing**: ADR-0048's "LRC itself has no exportable ground-truth mask"
is not the right justification for skipping this data — this pass's own finding is that
`MaskDigest`/`OriginalInstanceDigest` blobs *are* exportable, real raster output. The actual reason
#62 doesn't need them is narrower: **#62 is an import pipeline** (get the user's existing catalog
into Nicti's own schema), not a model-training or model-validation pipeline, and an import
pipeline has no destination for a third-party raster it isn't going to render with.

**Explicitly not foreclosed by this ADR**: using `.lrcat-data`'s AI outputs as a **golden/reference
dataset** — comparing Nicti's own BiRefNet/MobileSAM masks or denoise output against LRC's, as a
quality bar or regression check — is a plausible, legitimate use this pass did not evaluate and is
not deciding against. That's a #48/#40-scoped research question (would need its own storage/tooling
decision, and possibly LRC version-pinning to keep the comparison meaningful), not #62's import
path, and is flagged below as an open follow-up rather than resolved either way here.

**#62's importer does not need to read `.lrcat-data` at all.** It keeps the `hasMasks`/`hasAIMasks`/
`hasBigData` flags and the raw develop-settings text (including `MaskDigest`/`OriginalInstanceDigest`
literals) in the provenance blob per ADR-0061 Q4's existing policy — enough to know a mask/denoise
result *existed* in LRC, without needing its bytes.

## Consequences

- **#62** is unblocked on this question: no RocksDB reader, no blob extraction, no new Cargo
  dependency. Provenance-blob-only, consistent with ADR-0061 Q4.
- **#48/#40** are unaffected for their existing render-time re-derive stance, but each gets a new,
  explicitly open (not resolved here) follow-up question: whether `.lrcat-data`'s AI outputs are
  worth adopting as a golden/reference comparison dataset.
- `docs/research/shed-lrcat-schema.md` records the reproducible method
  (`spikes/shed/tools/lrcat_data_linkage.py` + `rocksdb_sst_dump`).
- The 1,030-key (2.2%) unaccounted residue and the `Adobe_imageDevelopSettings` row-count
  reconciliation (ADR-0061 Q6) are both left as open, non-blocking follow-ups — neither affects the
  import-policy decision above.

## Privacy note

Same policy as ADR-0061: this repo is public, and the real backup this pass measured against may
contain real people's names in keywords/collections/paths. Nothing above is a real key, digest,
path, or filename — every number is a count, and `shed privacy-check` was run against this diff
before committing.
