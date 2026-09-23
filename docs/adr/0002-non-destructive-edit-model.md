# ADR-0002: Non-destructive edit model

- **Status:** Accepted
- **Date:** 2026-09-23
- **Ticket:** #21 Research: non-destructive edit model

## Context

Nicti needs a non-destructive edit model before the catalog schema (#22), presets/sync (#52), or
XMP interop (#59) can be built, and before the Tapetum stage-cached render graph (#44) has
something concrete to key its cache on. The E0 PRD (#4) flagged this as missing from the original
architecture brief entirely. This ADR defines the edit-stack schema, undo/history, virtual
copies, and the XMP interop boundary — not the physical catalog schema (#22, gated on #67's DB
engine choice) or the field-level XMP/`crs:` mapping (#59), which are explicit handoffs below.

Constraints this ADR designs against, from the PRD and `docs/benchmarks.md`:
- 600k-2M asset scale, "no optimize catalog" (bounded, continuous maintenance, not a periodic
  compaction pass the user has to remember to run).
- The hero scenario (#43): bulk WB + vibrance + 2 AI masks + AI denoise across a selection, then
  switch between already-edited images <100ms warm, crop/zoom at 60fps regardless of edit-stack
  weight — every render-engine decision is measured against this.
- Extensibility for plugin render stages (Claw, #19) without hard-coding Nikon or v1-only
  assumptions.
- Phase-1 coexistence with Lightroom Classic via XMP (#11/#59/#60), phase-2 `.lrcat` import (#61/#62).

## Decision

**The catalog database is authoritative.** XMP sidecars are an interop and recovery layer, not
the source of truth — this is the inverse of darktable's sidecar-first model and matches
Lightroom Classic's own catalog-primacy, which is the tool Nicti's phase-1 users will be running
side-by-side with. Two sidecar responsibilities beyond LRC's own scope:

1. **A lossless `nicti:` namespace** carries the full current edit document, so a lost or
   corrupted catalog can be rebuilt from sidecars alone — a capability neither LRC's opaque
   `.lrcat` blob nor a `crs:`-only projection gives us. See "Recovery from sidecars" below.
2. **A best-effort `crs:` (Camera Raw Settings) projection**, primarily for AI masks so LRC/ACR
   can display Nicti's edits during phase-1 coexistence. Recommendation: **write it, but
   defensively** — see "The `crs:` projection" below. The field-level mapping itself belongs to #59.

### Edit document: a fixed-order stage map, not a user-ordered op list

```rust
struct StageEntry { schema_version: u32, params: serde_json::Value }
struct EditDocument { stages: BTreeMap<String, StageEntry> }  // stage_id -> params
```

This is Lightroom-style (a flat parameter set per pipeline stage) rather than darktable-style (a
reorderable module stack where render order *is* history order). darktable's own docs are explicit
that its history stack "stores the entire editing history... in the order in which those edits
were applied" and that this is deliberately *not* the pixelpipe execution order[^d1] — darktable
needs a separate `iop_order` concept to decouple the two. Nicti sidesteps that entirely: render
order is owned by the pipeline (#44/Tapetum), and the document only holds parameters, keyed by a
fixed stage id. A plugin stage (Claw, #19) uses a namespaced id (`vendor.stage_name`) and opaque
`params`; a build that doesn't have that plugin installed still round-trips the entry byte-for-byte
because `params` is untyped `Value`, not a struct only the plugin's code can deserialize — proven
in `spikes/pawprint/tests/unknown_stage_roundtrip.rs`.

`schema_version` is darktable's answer to module evolution, generalized: darktable pins each
history entry to the module version that produced it and migrates forward via a per-module
`legacy_params()` function when present, so an old edit keeps rendering as originally
authored instead of silently reinterpreting under a newer version[^d1][^d3]. Nicti adopts the same
shape at the stage-entry level.

### Canonical serialization and per-stage hashing

Tapetum's cache key needs `hash(stage params)` to be **stable**: the same logical edit must always
hash identically, regardless of struct-field insertion order, HashMap iteration order, or
byte-for-byte serialization quirks. Three points settle the format choice:

- **`serde_json`, without the `preserve_order` feature, already gives canonical key ordering for
  free.** Its `Map` type is backed by `BTreeMap` (sorted keys) unless `preserve_order` is enabled,
  which this project doesn't. `serde_jcs` (RFC 8785 JSON Canonicalization Scheme) exists as a
  belt-and-suspenders crate (`crates.io/serde_jcs`, v0.2.0, published 2026-03-25 — actively
  maintained) but isn't needed for the core guarantee, only as a hedge if `HashMap`-backed
  intermediate structures ever leak into the serialization path.
- **`postcard` and `ciborium` were evaluated and rejected for this specific purpose.** `postcard`
  is genuinely deterministic (struct fields serialize in declaration order, no padding — its own
  wire-format spec is explicit on this[^s2]), but it's a `no_std` binary format with no natural
  human-diffable/debuggable representation, which matters for a spike-stage project where a
  developer will regularly want to eyeball a stage's stored params. `ciborium` does **not** apply
  RFC 8949's deterministic-encoding mode by default (that mode is opt-in per the RFC text
  itself[^s4]), and its maps preserve insertion order rather than enforcing sorted keys[^s3] — it
  would need the same manual pre-sort `serde_json` already gives for free, with less mature
  tooling (last published 2024-01-24, over two years stale) and no debuggability win over JSON.
- **Floats need explicit normalization independent of format choice.** None of the three formats
  solve this: RFC 8949 itself punts float canonicalization to the application[^s4], and
  `-0.0`/`0.0` have distinct bit patterns despite comparing equal under `==`[^s5]. The spike
  normalizes `-0.0 → 0.0` before hashing (`canonicalize()` in `spikes/pawprint/src/lib.rs`), proven
  stable in `hash_stability.rs`. NaN never reaches this code in the first place — `serde_json`
  refuses to serialize it (returns an error) unless `arbitrary_precision` is enabled, which this
  project doesn't enable.
- **`blake3`** for the hash itself: actively maintained (crates.io v1.8.7, published 2026-08-20),
  used in production by IPFS, Solana, and Cargo itself[^s6].

**Decision: canonical `serde_json` (default `Map`, `preserve_order` off) + explicit float
normalization + `blake3`.** Proven in `spikes/pawprint/tests/hash_stability.rs`: hash is stable
across key-insertion order, across a full serialize→deserialize→serialize round-trip, and across
`-0.0` vs `0.0`. The cache key itself chains `hash(asset content identity, upstream stage hashes,
this stage's hash)`, so changing one stage's params only invalidates that stage and everything
downstream of it — also proven in the same test file
(`changing_one_stage_leaves_other_stages_hashes_untouched`,
`cache_key_changes_when_upstream_hash_changes_but_not_otherwise`).

### AI masks: the recipe, not the pixels

A mask stage stores `{model_id, model_version, params, seed?}` for AI-generated masks (subject,
sky, background) and vector geometry for brush/gradient masks — never the derived pixel mask
itself, which is Tapetum's job to compute and cache. Pinning `model_version` means upgrading the
segmentation model is an explicit, opt-in re-run on old edits rather than a silent quality change
the next time a photo is opened.

### History: append-only delta log + compaction + named snapshots

Persisted per photo (per the requirement that history survive a restart, unlike a session-only
undo stack), but **bounded**, to hold the "no optimize catalog" target: an unbounded per-tick
history is a documented real-world problem in Lightroom Classic itself — one long-time user's
4GB catalog had ~2.4GB (60%) consumed by develop-history data, with no automatic pruning and
manual deletion as the only way to reclaim space[^l3]. Nicti's history log instead:

- Records one delta per stage change, tagged with a coalescing `control` key (e.g.
  `"exposure_slider"`) and a timestamp.
- **Compacts** consecutive deltas sharing `(stage_id, control)` within a time window into a single
  delta spanning the run (first entry's `before`, last entry's `after`) — a 200-tick slider drag
  becomes one history step, and undoing it lands on the pre-drag value, not a mid-drag tick.
  Compaction only touches the applied log prefix; it refuses to run while there's a pending redo,
  so it can never corrupt a redo chain the user might still walk forward into.
- **Named snapshots** (LRC's "Snapshots" concept) are never merged away by compaction and are
  never pruned — they're the durable "I might want to come back to exactly this" marker.
- **Batches** (bulk paste/sync, #52) share one `batch_id` across every stage they touch, so undo
  reverts the whole batch as a single step instead of once per stage per photo.

All five properties (undo/redo, burst compaction preserving the pre-drag undo target, snapshot
survival across compaction, batch atomicity, refusal to compact with a pending redo) are proven in
`spikes/pawprint/tests/history_and_compaction.rs` and `bulk_paste.rs`.

darktable's own doc confirms the general shape (history stack stored in both its DB and the XMP
sidecar, in edit order)[^d1] but doesn't do compaction — its history is a straight append with no
merging, which is workable for darktable's scale but not for the 2M-asset / continuous-maintenance
target here.

### Virtual copies

`asset (file identity, #71) 1—N edit_variant`, each with its own `EditDocument` and its own
`History`, one flagged as master. This ADR defines only the logical shape; the physical schema is
#22's decision once #67 picks the DB engine.

### Sizing (from the spike, `spikes/pawprint/tests/sizing.rs`)

A realistic single-variant document (white balance + global tone + 2 AI masks + denoise, 5 stages)
serializes to **563 bytes**. A compacted history of 5 coarse edit steps plus one named snapshot
(which carries a full document copy) comes to **~1.1KB**. At 2M assets, even a generous 3x
real-world-editing multiplier over this reference session stays in the single-digit-GB range
total across the whole catalog — nowhere near the LRC-scale bloat problem cited above, because
compaction keeps per-photo step counts low regardless of how many raw slider ticks a user made.
These are throwaway-spike numbers for order-of-magnitude planning, not a commitment to the exact
byte layout #22 will ship.

### XMP layers and the LRC coexistence boundary

Three distinct XMP responsibilities, not one:

**(a) Standard metadata** — rating, flag, color label, keywords — follows LRC's own conventions so
both tools read/write the same fields during phase-1 coexistence. Field-level detail is #59's job.

**(b) `nicti:` recovery namespace** — the full current edit document (latest state only, not the
history log, to keep sidecars small), schema-versioned. Proven losslessly round-trippable in
`spikes/pawprint/tests/xmp_recovery.rs`, including an unrecognized plugin stage. The **recovery
procedure** this enables: if the catalog is lost or corrupted, every asset's current edit state
can be reconstructed by re-scanning sidecars — something neither LRC's proprietary catalog format
nor a `crs:`-only projection supports, since `crs:` (below) is lossy and one-directional.
Conflict rule when catalog and sidecar disagree (e.g. the catalog was restored from an older
backup than the sidecars on disk, or a sidecar was hand-edited): **newer wins, by content hash
then mtime** — compare the sidecar's embedded document hash against the catalog's; if they match,
no conflict; if they differ, prefer whichever side has the later modification time, and flag the
asset for manual review if the timestamps are ambiguous (e.g. within the same filesystem
mtime-resolution window). This deliberately avoids LRC's own approach: LRC surfaces an explicit
conflict dialog ("Import settings from disk" vs. "overwrite settings on disk") rather than
auto-resolving[^l4] — reported behavior, since Adobe has never published this algorithm
officially[^l4]. Nicti's newer-wins rule is simpler and matches this project's "no manual busywork"
maintenance target, at the cost of occasionally picking the wrong side of a genuine simultaneous
edit in both tools; that risk is judged acceptable because phase-1 coexistence is a transitional
state, not the end goal.

**(c) `crs:` projection** — optional, lossy, **write-only** (Nicti never reads its own `crs:`
output back as authoritative; the `nicti:` namespace is always the read-back source). Evaluated
primarily for AI masks, since ACR/LRC's `crs:MaskGroupBasedCorrections` structure already encodes
subject/sky/background segmentation masks that LRC can display[^l2] — meaning a well-formed
projection lets a user glance at an edited photo in Lightroom during the transition period and see
approximately the right thing, even for AI-masked edits. Two real risks this ADR has to name
explicitly: Adobe has never published `crs:MaskGroupBasedCorrections`'s structure officially — the
only source found is third-party reverse-engineering (the open-source Maple project's XMP-parsing
code[^l2]), so this mapping should be treated as **best-effort and expected to need
correction** once #59 validates it against real Lightroom-written files; and writing `crs:`
unconditionally risks clobbering an edit LRC itself made to the same file during coexistence.
**Recommendation: write `crs:` only when Nicti can confirm LRC hasn't independently edited the
photo since Nicti's last write** (compare `crs:` mtime/hash against what Nicti last wrote), or
restrict `crs:` writes to explicit "export for LRC preview" actions rather than every save — #59
picks between these.

### Schema evolution

Each stage entry carries its own `schema_version`. A future version of a stage's param schema
provides a forward migration (mirroring darktable's `legacy_params()` pattern[^d3]); a version this
build doesn't recognize at all (older Nicti reading a newer file, or a plugin stage's schema jump)
stays read-only rather than being dropped or guessed at.

## Consequences

- **Unblocks #22** (catalog schema): `EditDocument`/`StageEntry`/history-log shapes above are the
  logical schema to map onto #67's chosen DB engine.
- **Unblocks #52** (presets/copy-paste/sync): the batch-id history model and `apply_relative`
  (absolute vs. relative paste) are proven in `spikes/pawprint/tests/bulk_paste.rs`.
- **Feeds #44** (Tapetum): the per-stage cache-key chain (`cache_key()` in
  `spikes/pawprint/src/lib.rs`) is exactly the "hash of input + params" invalidation key Tapetum's
  design calls for.
- **Feeds #59** (XMP interop): this ADR sets the three-layer boundary
  (metadata / `nicti:` recovery / `crs:` projection) and the conflict-resolution rule; #59 owns the
  field-level mapping for (a) and the concrete write-gating logic for (c).
- **Explicitly out of scope**, deferred to their own tickets: the physical catalog schema and SQL
  (#22, #67), the real XMP/RDF serialization library and packet structure (#59 — the spike's XMP
  packet format is a throwaway placeholder, not a proposal), and UI-level undo/redo/history-panel
  design.

---

## Prior-art appendix

All claims fetched/verified 2026-09-23 by three parallel research passes (LRC/ACR; darktable +
RawTherapee; Rust serialization formats), each citing a primary source where one exists. Adobe has
never published the `.lrcat` schema or its sidecar-conflict-resolution algorithm — those claims
rest on reverse-engineering and community reports, flagged below as such, matching how ADR-0001
flagged its own unverifiable claims rather than passing them off as primary-sourced.

### Lightroom Classic / ACR

[^l1]: `.lrcat` develop-history tables (`Adobe_libraryImageDevelopHistoryStep`,
    `Adobe_imageDevelopSettings`) and virtual-copy linkage (`Adobe_images.masterImage`,
    `copyName`) — https://github.com/camerahacks/lightroom-database and
    https://github.com/hfiguiere/lrcat-extractor/blob/main/doc/lrcat_format.md — **best-available-secondary**;
    Adobe has never published this schema, both sources are independent reverse-engineering, and
    `lrcat-extractor` only partially corroborates (confirms `masterImage`/`copyName`, doesn't cover
    the history tables).
[^l3]: Unbounded develop-history growth, no built-in pruning —
    https://www.pointsinfocus.com/learning/digital-darkroom/the-lightroom-catalog-and-develop-history-states/
    — **best-available-secondary** (independent photography blog, not Adobe); cites a real
    4GB-catalog/2.4GB-history example and ~15 history states/image average.
[^l2]: `crs:` namespace (`http://ns.adobe.com/camera-raw-settings/1.0/`) is Adobe-documented for
    ~40 global develop properties —
    https://developer.adobe.com/xmp/docs/xmp-namespaces/crs/ and
    https://github.com/adobe/xmp-docs/blob/master/XMPNamespaces/crs.md — **primary-source-verified**
    for what's documented, and the *absence* of any mask-related property in these same pages is a
    direct observation, not an inference. `crs:MaskGroupBasedCorrections`'s actual structure
    (AI subject/sky/background masks, `MaskSubType`, `CorrectionMasks`) —
    https://github.com/zubair-io/Maple/pull/3285 and .../pull/3282 — **best-available-secondary,
    explicitly not Adobe**; no official Adobe documentation of this structure was found.
[^l4]: Catalog-vs-sidecar conflict handling shows an explicit user-facing conflict indicator/dialog
    ("Import settings from disk" vs. "overwrite settings on disk"), not silent auto-merge —
    https://community.adobe.com/questions-675/metadata-conflicts-1617362 — **best-available-secondary**;
    Adobe's own help pages (`helpx.adobe.com`) returned HTTP 403 to automated fetch during this
    research pass and were not independently confirmed. No evidence found of timestamp-based
    auto-resolution; some community reports describe spurious conflict flags unrelated to real
    edits (GPS precision, face-data sync) — a known source of user frustration, not a documented
    algorithm.

### darktable / RawTherapee

[^d1]: History stack stored in both `library.db` and the XMP sidecar, "in the order in which those
    edits were applied" (explicitly *not* pixelpipe execution order) —
    https://docs.darktable.org/usermanual/development/en/darkroom/pixelpipe/history-stack/ —
    **primary-source-verified**.
[^d2]: Database takes precedence over the XMP sidecar once an image is imported; XMP is positioned
    as the disaster-recovery copy —
    https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar/ —
    **primary-source-verified**.
[^d3]: Per-entry fields (`darktable:operation`, `darktable:enabled`, `darktable:modversion`,
    `darktable:params`, `darktable:multi_name`, `darktable:multi_priority`) —
    https://github.com/darktable-org/darktable/blob/master/tools/dtstyle_to_xmp.py —
    **primary-source-verified** for the fields shown in that script. Module-version pinning via
    `legacy_params()`, with a version-mismatch error when a migration path is missing, and
    deprecated modules kept in the codebase indefinitely to preserve old edits — **best-available-secondary**
    (GitHub issue discussion + a docs page not independently re-fetched verbatim in this pass).
[^d4]: RawTherapee's `.pp3` sidecar is a layered stack of *final* key/value overrides (neutral →
    default profile → `-p` profile → sidecar, applied in that order), not a linear undo history —
    https://man.archlinux.org/man/rawtherapee.1.en (RawTherapee's own man page) —
    **primary-source-verified** for this specific claim. RawTherapee's actual manual (RawPedia)
    was unreachable (maintenance-mode redirect) during this research pass; a forum thread
    (https://discuss.pixls.us/t/history-and-or-reset-seeing-our-footprints/1843, RawTherapee's own
    community/dev forum) corroborates that the in-app History panel isn't persisted to the pp3 —
    **best-available-secondary**. No Capture One findings: none turned up quickly from an
    authoritative source, and none were fabricated to fill the gap.

### Rust canonical serialization

[^s1]: `serde_json::Map` is `BTreeMap`-backed (sorted keys) unless the `preserve_order` feature is
    enabled — struct field order follows declaration order via serde's derive mechanics. `BTreeMap`
    sorted-iteration is documented stdlib behavior (https://doc.rust-lang.org/std/collections/struct.BTreeMap.html).
    The struct-field-order guarantee itself is **best-available-secondary** (serde-rs/json GitHub
    issue discussion, not an explicit docs.rs statement); `serde_jcs` (RFC 8785 JCS) exists as a
    dedicated canonical-JSON crate, crates.io v0.2.0 published 2026-03-25 — **primary-source-verified**
    (crates.io API).
[^s2]: `postcard`: fields encode in declaration order, no padding/length metadata on the wire —
    https://postcard.jamesmunns.com/wire-format (the crate's own spec) — **primary-source-verified**.
    crates.io v1.1.3, published 2025-07-24.
[^s3]: `ciborium`: maps preserve insertion order, not sorted-key order, out of the box —
    https://github.com/enarx/ciborium (README) — **primary-source-verified**. crates.io v0.2.2,
    published 2024-01-24 (stale relative to this research date — flagged as a maintenance concern).
[^s4]: RFC 8949 §4.2 "Core Deterministic Encoding" is opt-in, protocol-defined, not CBOR's default;
    §4.2.2 explicitly leaves NaN/signed-zero canonicalization as something "protocols... may need
    to define extra requirements" for — https://www.rfc-editor.org/rfc/rfc8949.html —
    **primary-source-verified**.
[^s5]: `+0.0`/`-0.0` have distinct IEEE-754 bit patterns but compare equal under `==` —
    https://en.wikipedia.org/wiki/Signed_zero — **primary-source-adjacent** (tertiary reference for
    a standard fact, not the IEEE 754-2019 spec text itself). NaN has multiple valid bit patterns —
    https://en.wikipedia.org/wiki/NaN — same confidence level.
[^s6]: `blake3`: crates.io v1.8.7, published 2026-08-20; production users listed in its own README
    include IPFS, Solana, OpenZFS, LLVM, Cargo — https://github.com/BLAKE3-team/BLAKE3 —
    **primary-source-verified**.

*Not independently verifiable with a primary source: the exact `.lrcat` develop-history table
names and virtual-copy foreign-key structure (Adobe has never published this schema); LRC's
sidecar-conflict-resolution algorithm (Adobe's own help pages returned HTTP 403 to automated fetch
during this research pass — worth a manual retry before treating [^l4] as settled); RawTherapee's
RawPedia manual (unreachable, maintenance-mode redirect, for the full research pass); darktable's
`iop_order` field and deprecated-modules page content (not independently re-fetched verbatim).*
