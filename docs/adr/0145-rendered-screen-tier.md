# ADR-0145: Rendered screen-preview tier (stale-while-revalidate)

**Status:** Accepted
**Ticket:** #145
**Part of:** #6

## Context

ADR-0029 left two gaps open ("flagged, not solved"): every T0-T3 preview is derived from the camera's
embedded JPEG, so none of them shows the user's develop edits, and that JPEG carries the camera's
Picture Control look rather than Nicti's own colour rendering (ADR-0038). In the loupe an edited
photo therefore showed an unedited preview, with no hint it was stale, until its RAW decode landed.

Nothing rendered an edited photo to a screen-size image or cached one, and the Larder
(`nicti-lair/src/larder.rs`) keys entries `(asset_id, tier)` with `get` *deleting* an entry whose
`render_hash` differs -- so asking it for an "edited T2" would have destroyed the camera T2.

## Decision

1. **A second Larder tier, `LarderTier::Rendered`** (`"r2"`), beside the camera `T2`. Same pack
   file, byte cap and LRU. `Larder::get_latest(asset, tier)` returns the stored entry *with* its
   `render_hash` without dropping it on a mismatch (the stale-while-revalidate read);
   `Larder::stored_hash` is the index-only "is a current one cached?" check.
2. **`render_hash`** = `rendered:v{EYESHINE_VERSION}:{blake3(identity || EditDocument::content_hash)}`,
   plus a `:partial` suffix when the document holds adjustments the preview render cannot show yet
   (active local masks, #354; AI-removal spots, #324 -- patches aren't persisted). Bump
   `EYESHINE_VERSION` whenever the render pipeline's output changes.
3. **The render job ("Eyeshine", `nicti-pelt/src/eyeshine.rs`)**: three chained Pounce jobs per
   photo, the lane split export (#57) uses -- decode (CPU) -> render, one tile per step (GPU,
   `vram_bytes: 0`) -> encode + store (CPU) -- through a `Submitter`. The render step is shared with
   export (`export/render_core.rs`: `ExportRenderer`, `render_live_frame`). The crop is tiled
   straight to a 3840px long edge (the crop transform's input axes scaled by the downscale), so the
   ~540 MB full-crop linear buffer export needs is never allocated. Encode is JPEG q85 sRGB through
   `nicti_preen::export_frame`, which also applies the file's EXIF orientation.
4. **`EyeshineService`** keeps at most one render per photo in flight (a newer edit sets a
   cancel flag every later stage checks and cancels the queued first stage; the encode step
   re-checks it under the Larder lock so a superseded render can't overwrite the newer one) and
   only one photo renders on the GPU at a time. A render that returns `Retry` (unreachable file,
   busy Larder) or `Failed` (a decode error, a panic, a full disk) backs off (5 s / 60 s) per
   `(asset, hash)` rather than being retried every frame or never. A photo's edit document is
   re-read after a 1 s TTL, so edits written by paste/sync, undo or an LRC import are picked up
   without each of those writers having to invalidate.
5. **`choose_preview`** (pure, tested) is the display rule: a current render wins; an older render
   beats the camera previews and is badged "updating"; otherwise the camera T2/T0 is shown, badged
   "stale" when the photo has edits *and rendering is on for that view* (with rendering off the
   camera preview is simply what is shown, with no promise of a render). A render that no longer
   applies (Reset all, rendering switched off) is dropped so the camera preview returns.
6. **Settings** (`<catalog>.previews.json`): render `Off` / `Edited photos` (default) / `All photos`,
   and per-view switches. `All photos` also renders unedited photos, which closes the Picture
   Control gap: every preview then uses Nicti's own colour. Switching to `Off` empties the rendered
   tier (retried each frame while the Larder is busy, e.g. during a compaction).

## Surfaces

- **Loupe pre-decode fallback**: full behaviour (render queued, swapped in, never downgraded, badge
  text).
- **Library grid**: a dot on every cell whose photo has edits, because its thumbnail is camera-derived
  and never shows them (it does not go away when a render exists). Showing a
  *rendered* thumbnail would need a small rendered tier -- decoding a 3840px JPEG per cell is far
  too slow -- so it is a follow-up.
- **Survey/compare**: unchanged by design (ADR-0032: culling runs on embedded previews and never
  touches the develop pipeline).

## Consequences / limits

- The preview render downsamples with the crop pass's bilinear sampler; at ~2x downscale this is
  slightly more aliased than a Lanczos resize. Acceptable for a transient screen preview.
- Masks and AI removals are not rendered (flagged `Partial`); the live Develop render, which runs
  once the RAW decode lands, remains the source of truth.
- Real-photo timing, Vulkan/Dx12 behaviour and the on-screen swap are not verified in the dev
  sandbox (software adapter only); see the PR's verification checklist.
