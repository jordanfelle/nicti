# ADR-0353: Disk tier for baked AI mask alphas, and the background pre-bake (Stash)

**Status:** Accepted
**Ticket:** #353
**Part of:** #8
**Related:** #190, #52, #354

## Context

A finished AI mask alpha (`DevelopView::ai_alphas`) lived only in memory and was cleared whenever the
photo changed. A bake is ~9 s on the CPU build (ADR-0049; ~0.2 s with the GPU pack, #345), so
reopening a photo re-ran its models. ADR-0044's cache-tier table already reserved a disk row for
"mask alpha" and left its codec to #190. ADR-0052 made presets/copy/paste/sync write only edit
documents, so a synced AI mask re-bakes lazily when each photo is opened, and deferred a background
pre-bake of the synced selection to this issue "because nothing can hold the results until then".

The Larder (`nicti-lair/src/larder.rs`, ADR-0029/#27) is the existing byte-capped, checksummed,
compacting disk cache, but its primary key is `(asset_id, tier)`: one entry per photo per tier. A
photo can carry several AI masks (subject, sky, ...), each with its own bake key.

## Decision

1. **Keyed entries in the Larder, not a second store.** A new `keyed_entry` table
   `(kind, key) -> (asset_id, offset, len, checksum, seq)` shares the Larder's pack file, byte cap,
   LRU order (one `seq` counter across both tables), compaction and `purge_all`, so the existing
   cache-size setting and "purge all" cover alphas with no UI change. API: `put_keyed`, `get_keyed`,
   `contains_keyed`, `forget_keyed`, `purge_kind`; `purge_asset` also drops an asset's keyed rows.
   The table is created with `IF NOT EXISTS`, so an existing Larder needs no migration. `LarderKind::AiAlpha`
   puts the kind in the key so other derived blobs can share the store later.
2. **The key is the bake key alone** (`compose::ai_bake_key`: blake3 of the neutral render's cache
   key and the recipe). It chains from the photo's identity and the baked prefix, so a stored alpha
   can never be applied to different pixels: a change that would alter the bake changes the key
   and the old entry ages out under the cap. `asset_id` is only for `purge_asset`.
3. **Quantise to 8 bits at bake time; deflate on disk.** `AiAlpha::quantized` rounds every value to
   a multiple of 1/255 (what `MaskBakeJob` builds), so the in-memory alpha *is* what a reload
   returns: the same `content_hash`, hence the same stamped "ready" state, refined-alpha cache key
   and rendered-preview hash before and after a reload. 8 bits is below what a sigmoid mask can show
   on screen. The payload is `NAL1`, `width`/`height` as little-endian `u32`, then zlib
   (`flate2`, fast level) over the 8-bit plane; `AiAlpha::decode` rejects bad magic, an absurd or
   zero extent, a short or over-long plane. `flate2` is already in the tree (`nicti-calico`); no
   new crate. A 1024x1024 mostly-0/1 mask compresses to well under 64 KiB (tested). **zstd/lz4 were
   not adopted:** ADR-0044's synthetic-gradient ratios (~3688x / ~247x) say nothing about a real
   mask, they would be a new dependency and licence row, and deflate is already small enough that
   the codec is not the bottleneck (the Larder write is).
4. **Fetch before bake.** `MaskBakeService::request_missing` first submits a Foreground
   `AlphaFetchJob` (CPU lane, `JobKind::Preview`) for a missing alpha, even when its model is not
   installed (a stored alpha needs no model). A hit is applied through the same path as a bake
   result, with the same "result for a photo the user has left is dropped" guard; a miss falls
   through to the bake exactly as before. A busy Larder (a compaction) or an unreadable payload is
   a miss. A payload that passes its checksum but will not decode (another build's format) is
   *forgotten*, or `contains_keyed` would stay true and every later store would skip as "already
   there". The lookup is remembered per `(photo, bake key)` so a miss is not re-asked every frame
   and is cleared on a hit or when the bake lands, so an alpha pruned from memory (mask deleted,
   then undone) is looked up again rather than re-baked.
5. **Store after bake.** Every successful bake queues a Background `AlphaStoreJob` (encode + put,
   best effort), including a bake that finishes after the user has moved on (still valid work),
   filed under the catalog id it was requested for. Photos without a catalog id (a synthetic
   frame) and a missing Larder leave behaviour exactly as before.
6. **Background pre-bake (`prebake.rs`, `PrebakeService`).** After a paste/sync/preset batch (not an
   undo) the touched photos, minus the one open in Develop, are queued **nearest the grid cursor
   first** and baked in the background, **one photo at a time** (a decoded full-resolution frame
   is hundreds of MB). Per photo: `PlanJob` (CPU) names the bakes from the document and identity
   without pixels, via `spine::neutral_key` (a test pins it to `DevelopView::neutral_key`) and drops
   the ones already on disk; `DecodeJob` (CPU) decodes the RAW and submits one `MaskBakeJob` per
   recipe at **Background** priority (`MaskBakeJob::with_priority`; `new` stays Foreground), so a
   queued foreground mask is taken ahead of them and a slider drag pauses them (`IS_EDITING`); a
   bake already running finishes first (one GPU worker, one non-interruptible chunk). A Background
   bake declares 0 VRAM: Pounce drops a Background job whose declaration exceeds the lane's whole
   budget, which the GPU pack's 8 GiB would do against today's 512 MiB placeholder;
   an `AlphaStoreJob` per alpha, with a finished flag, so the photo's keys stay "in flight" until
   its alphas are actually on disk.
7. **No download, no duplicate work.** A recipe whose model is not installed is skipped: the
   pre-bake never downloads one (ADR-0218). While a photo's bakes are in flight their keys are
   announced to `MaskBakeService` (`set_deferred_keys`), which neither fetches nor bakes them
   (they show as pending) and finds them on disk afterwards; a queued photo the user opens first
   is simply not pre-baked, and one the user opens mid-chain is handed back to the foreground
   (the pre-bake abandons it so nothing waits behind Background work). Cancelling a decode or a
   bake from the activity panel stops the whole pre-bake; cancelling a plan or a store only
   skips that step (a corrupt file likewise only skips its own photo).

## Consequences / limits

- A disk hit skips the model only; the neutral-render guide and the guided-filter refine still run
  when the mask is composed, as for a fresh bake.
- The cap is shared with the T2/rendered previews, so a very large alpha population can evict
  previews (and vice versa). An alpha is tens of KiB; this is far below the 8 GiB default.
- The pre-bake plans a photo against its document at the time it is reached, so a photo edited
  while queued is planned against the new document, and one edited mid-chain stores the alphas for
  the old keys (harmless: unused keys age out).
- Two catalog assets with the same fingerprint share bake keys; the one keyed row belongs to the
  asset that stored it, so `purge_asset` of that asset drops it for both (a re-bake, nothing worse).
- While Develop's "show before" is held the mask tool neither fetches nor bakes (that render keys
  the neutral frame from the default document).
- A bake that finishes for a photo the user has just left is stored by a Background job; if the
  user returns before that job runs, the lookup can miss and bake again. Narrow, and only costs the
  work the old behaviour always did.
- Timing on real hardware, the activity panel presentation and a real-model pre-bake are not
  verified in the dev sandbox (software adapter, no model weights); see the PR's checklist.
- Not done: pre-baking on import or folder open, and pre-baking when an AI mask is *added* to
  the open photo for its neighbours (nothing writes their documents).
