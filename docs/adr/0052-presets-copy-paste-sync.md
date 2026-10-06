# ADR-0052: Develop presets, copy/paste settings, and sync across a selection

- **Status:** Accepted
- **Date:** 2026-09-30
- **Ticket:** #52 Build: presets, copy/paste, sync across selection

## Context

#21 (ADR-0021) proved the building blocks: `EditDocument` is a map of stage id to `StageEntry`,
`History::apply_batch` records one atomic entry per photo, and `apply_relative` adds a delta to a
numeric stage. #44 (ADR-0044) keys every render by per-stage hash, so a synced *live* stage costs
no bake dispatch. Nothing on top existed: no preset store, no copy/paste or sync UI, no batch edit
write to the catalog, and `CatalogStore` could only read or write one photo's document at a time.

Constraints found while building:

- `nicti-pelt` has no per-user config directory; every setting is a sibling file next to the catalog
  (`export/presets.rs`, `cache_settings.rs`).
- Develop has one `DevelopView` shared with the Loupe, which autosaves its in-memory document every
  frame. A batch that rewrites the loaded photo in the catalog would be silently overwritten by it.
- Baked AI mask alphas live only in memory (the disk tier is #353), and the denoise stage is a
  no-op. There is nowhere to put a background pre-bake yet, and nothing to recompute for denoise.
- Develop has no undo yet (`History` is not wired into `nicti-pelt`, #324).

## Decision

**Semantics are per-stage and absolute.** A checked stage replaces the target's `StageEntry`
wholesale. A checked stage the source doesn't have is *removed* from the target (a reset), so "copy
the exposure of a photo I never touched" resets exposure instead of doing nothing. An unchecked
stage is never touched. Relative (additive) paste via `apply_relative` is deferred: it needs UI
for choosing which sliders are relative, and absolute paste is what "sync settings" means in LRC.

**The checklist** is the stage ids Develop's "Reset all" clears (white balance, exposure, tone,
tone curve, vibrance, HSL, sharpening, noise reduction, crop, heal spots, local corrections) plus
the camera profile. Crop, heal and masks describe one photo's own content and start unchecked. So
does the camera profile: it names a camera-specific `.dcp`, and a target from another camera
rejects it (Develop shows an error, export fails that photo), so pasting it is opt-in. The rest
start checked. The last checklist is remembered for the session.

**Masks travel as one stage** (`nicti.masks`). Geometry masks use normalized coordinates and mean
the same on a differently sized photo. AI masks are per photo (the bake key chains from the
photo's neutral render), so a synced AI mask **re-bakes lazily**: the batch only writes documents,
and the existing `MaskBakeService` bakes each one when that photo is opened in Develop. A
background batch pre-bake is deferred to #353 (the disk tier for baked alphas), because nothing can
hold the results until then. A sync of live-only stages dispatches no bakes at all (ADR-0044).

**One batch write.** `CatalogStore::get_master_edits`/`put_master_edits` read and write many
documents under one lock / one transaction (same shape as `set_meta`). The write serialises every
document before opening the transaction, so a bad document or a missing asset writes nothing.

**No-op targets are not written and get no undo entry** (ADR-0101 rule 6). The result is one summary
line, never a per-photo prompt (ADR-0101 rule 5): "Sync: 388 changed · 12 already matched".
ADR-0101 also asks for a jump-to-skipped filter; a sync skips nothing for low confidence (that is
the auto-ops' case), so the only leftovers are photos already matching and photos missing from the
catalog, and no filter is built for them.

**Presets** are a name plus the stage entries the checked groups had, stored in
`<catalog>.develop-presets.json` next to the catalog, written via temp file + rename, exactly like
export presets. A missing file means no presets. An unreadable or partly unusable file never blocks the app: the usable presets load, and the first save copies the original to `<file>.bad` (`.bad1`, ...) before replacing it, so nothing is silently lost. There are no
built-ins, a duplicate or empty name is refused (never silently replaced), and applying one sets
only the stages it holds: a preset never resets a stage it doesn't mention.

**Undo is session-local.** The last batch keeps each changed photo's previous document in memory.
Undo restores a photo only if its document still equals what the batch wrote, so an edit made since
is never overwritten; those photos are reported as left alone. This is a stand-in until #324 wires
real `History` into Develop.

**The loaded photo stays coherent.** Before a batch the loaded photo's unsaved edits are flushed;
after it, if the loaded photo was touched, its document is re-read into `DevelopView` and marked
saved (`DevelopView::replace_document`), so the per-frame autosave cannot write the old copy back.

## Consequences

- Unblocks the LRC-import side of presets: `Preset`/`ToggleStyleAmount`/`ToggleStyleDigest` (ADR-0061,
  `spikes/shed` `Owner::Presets`) are bookkeeping metadata for #62 to carry through, not develop
  parameters, and are not modelled here.
- Deferred: relative paste; background AI pre-bake of a synced selection (#353); real undo/redo
  through `History` (#324); rendering masks in export (#354).
- A paste runs synchronously on the UI thread: one read under the catalog lock and one write transaction over small JSON documents
  (1,000 photos is one batch and one undo, covered in `knead::batch` tests; it isn't timed). If a future catalog or
  document size makes that visible, move `run_batch` onto a Pounce job; its plan/write split already
  fits one.

## Context update (#353)

The two deferrals above are resolved by ADR-0353: baked AI alphas now have a disk tier (Larder keyed
entries), and a paste/sync/preset queues the touched photos for a background pre-bake
(`nicti-pelt/src/prebake.rs`), so a synced AI mask is usually a disk read when the photo is opened
rather than a model run. The decision text above is kept as written.
