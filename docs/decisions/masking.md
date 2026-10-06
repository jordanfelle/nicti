## Masking

Covers the AI-segmentation model choice, the brush/gradient local-adjustment geometry model, and
the mask-group compose model that ties them together.

- **Masking**: `docs/adr/0048-masking.md` — **Proposed, pending a reference-machine pass** (real
  BiRefNet/MobileSAM weights, real photos including fursuiters, and #44's own gating). Model
  choice: BiRefNet (one-shot subject/background) + MobileSAM (interactive click/box refine, real
  two-session encoder/decoder split, unlike `spikes/groom`'s single-tensor collapse for its own
  healing case). SAM2 re-checked (its SA-V dataset license, previously flagged unverified, is
  confirmed CC-BY-4.0) but not adopted — heavier, video-oriented, no gap MobileSAM doesn't already
  cover for stills. Sky segmentation: no clean model adopted this pass (RapidRAW's own `skyseg`
  fine-tune of U-2-Net has an unverified third-party checkpoint provenance); ships as a classic
  luminance/blue-dominance heuristic, flood-filled from the top row so a disconnected bright/blue
  region elsewhere in frame isn't picked up.
- **Mask-group model**: `spikes/siamese/src/compose.rs`'s `MaskGroup`/`MaskComponent`, named to
  match Lightroom Classic's own `MaskGroupBasedCorrections` shape so #49/#62's importer maps onto
  it directly. Resolves two conflicts the research pass found: `model_version` is a `String` (not
  groom's `u32` — groom/#51 should align to this), and a mask's inverse is expressed as
  `invert: bool` on a *component* sharing the same `AiRecipe`, not a second model recipe — the
  bake key (`ai_bake_key()`) is defined independently of `invert`/`opacity`, so a mask and its
  inverse (the hero scenario's own "Select Subject" + "Select Subject, Invert" pair) share one bake
  key and the model runs once.
- **Cache-key design**: AI masks infer against a **fixed neutral render** (post-lens-correction,
  default tone), not the user's live edit stack — otherwise every slider drag would invalidate
  every AI mask, and the hero scenario's 50-image bulk-sync would re-run the model 50 times over
  for reasons unrelated to what's actually being selected.
- **Geometry**: `spikes/siamese/src/geometry.rs` — linear gradient, radial gradient, and brush
  (ordered strokes, each own add/erase, dabs blend via `max` within a stroke so overlapping dabs
  don't double-darken).
- **Refinement**: `spikes/siamese/src/refine.rs` — a guided filter (He, Sun & Tang), not a plain
  bilinear alpha upsample, so a preview-resolution AI mask's boundary snaps back to the full-res
  photo's own edges rather than staying a soft blur across the subject boundary.
- **GPU**: five WGSL kernels (gradient rasterize x2, brush rasterize, compose step, masked-adjust
  apply), each checked against its CPU reference within `1e-4` in `spikes/siamese/tests/gpu_parity.rs`
  (9 tests, passing against lavapipe in this sandbox).
- **No real ONNX weights obtained this pass** — a full BiRefNet export exists publicly (~970MB)
  but downloading/running it was out of this pass's time budget, same call ADR-0050 made for
  LaMa/MobileSAM. `spikes/siamese/src/segment.rs` proves only the `ort`/`load-dynamic`
  loading/error-handling shape (`ModelNotFound` on a missing file), same as groom's own `ai.rs`.

### #49 -- the build (`docs/adr/0049-masking-build.md`)

ADR-0049 built the design above and records every departure from it. In one place: **one `nicti.masks`
stage** holds every local correction (normalized coordinates, sanitized on the way in); the **neutral
render taps post-lens, pre-heal** and `Renderer::render_baked` lets the engine read the baked frame; `Add`
is a **union** (`max`), not `min(a+w, 1)`; bakes are requested for *enabled* corrections; the **engine
caches every expensive intermediate** so a slider drag is uniform-only, a geometry edit recomposes one
correction and painting is one GPU pass per frame; local adjustments **stack additively** in the fused
live shader (clarity/texture/dehaze from bases cached per baked frame; sharpness/noise inside
`detail_combine`); **model choice is data** behind a backend-agnostic `SegmentationProvider` registry
(a version mismatch is a typed error, never a silent newer model); BiRefNet ships as a pinned fp32 ONNX
conversion, on-demand only.

**What running the real thing showed** (RTX 5080, release, Windows): 16 masks at 45 MP cost +1.4-1.9 ms
over the no-mask live pass (2.5-3.2 ms p95 total with spatial adjustments) -- inside the 4 ms rule; the real
BiRefNet bake is **9.2 s warm / 15.3 s cold on the CPU provider** (ADR-0048's <= 1 s assumed CUDA), with the
tensor contract verified against the real weights; and the real-hardware pass found a **pre-existing Dx12
bug on `main`** (`detail_blur.wgsl` reading a storage texture) that fails an existing end-to-end test on the
5080, fixed here. Quality on real photos (fursuiters) is still open (#171).

**CPU-viable subject model (#349, research):** nothing is both interactive and BiRefNet-grade on CPU. fp16 BiRefNet is slower than fp32 (9.9 vs 8.0 s); BiRefNet-lite gives near-identical masks at 4.9 s (1.6x faster, 224 MB, still not interactive); IS-Net general-use runs in 0.56 s and finds the right subject but with soft, leaky edges; U2-Net(p) is worse. No model was registered -- each candidate still needs a pinned artifact, a licensing row and a real-photo pass (#171). The GPU execution provider (#345, landed as the optional NVIDIA GPU pack -- CUDA + fp16, 0.2 s per bake on an RTX 5080; DirectML measured 6 s) is the route to interactive Select Subject. Table, method and caveats: `docs/research/cpu-subject-model.md`.

**Disk tier + background pre-bake (#353, `docs/adr/0353-baked-alpha-disk-tier.md`):** baked AI alphas now persist, so reopening a photo loads its masks instead of re-running the model (9 s on the CPU build). They live in the Larder as *keyed* entries -- a second index table beside `(asset, tier)`, because a photo can carry several AI masks -- sharing its pack file, byte cap, LRU and compaction, so the existing cache-size setting and "purge all" cover them. The key is the bake key alone (it chains from the photo's identity and baked prefix, so a stored alpha can never be applied to different pixels). Alphas are quantised to 8 bits **at bake time** (not at store time) so the in-memory value equals the reloaded one and every hash derived from `content_hash` is stable across a reload; on disk they are zlib over the 8-bit plane (`flate2` is already in the tree; zstd/lz4 would be a new dependency for a gain that is not the bottleneck). The service looks on disk first -- even when the model is not installed -- and stores every finished bake, including one that lands after the user has left the photo. After a paste/sync/preset batch the touched photos are pre-baked in the background, one at a time, nearest the grid cursor first, at Background priority, never downloading a model; ADR-0052's deferral is thereby resolved.

## #380: global Presence stacks with the local adjustments

Global Texture/Clarity/Dehaze/Saturation (`nicti.presence`) are added to the stacked local delta of
the same name in `live_suffix.wgsl` (ADR-0380), so a global +0.3 and a local +0.2 act as +0.5, and
the spatial ones read the same cached bases. `MaskEngine::prepare` takes `MaskInputs.presence` and
builds the bands/haze bases even with no active correction (returning a `MaskFrame` with no
corrections); it returns `None` only when neither a correction nor a spatial presence needs anything.
