---
paths:
  - "crates/nicti-siamese/**"
  - "crates/nicti-stalk/**"
  - "crates/nicti-tapetum/src/mask/**"
  - "crates/nicti-tapetum/shaders/mask_*.wgsl"
  - "crates/nicti-tapetum/shaders/guide*.wgsl"
  - "crates/nicti-tapetum/shaders/dehaze_*.wgsl"
  - "crates/nicti-pelt/src/mask_*.rs"
  - "crates/nicti-pelt/src/stash.rs"
  - "crates/nicti-pelt/src/prebake.rs"
  - "docs/adr/0048-masking.md"
  - "docs/adr/0049-masking-build.md"
  - "docs/adr/0353-baked-alpha-disk-tier.md"
---

# Masking — Quick Reference

Full reasoning/history: `docs/decisions/masking.md`; the build is `docs/adr/0049-masking-build.md`.

- **What exists (#49)**: one `nicti.masks` stage (`MaskParams` -> `LocalCorrection { mask: MaskGroup,
  adjust: LocalAdjust, amount }`), `nicti-tapetum/src/mask/`. Models in `nicti-siamese`, pluggable via
  `nicti-stalk`'s `SegmentationProvider`/`SegmentationRegistry`. UI in `nicti-pelt`'s `mask_panel.rs`
  (egui), `mask_edit.rs` (egui-free ops, unit-tested), `mask_tool.rs` (`MaskBakeService`).
- **Normalized coordinates**: points = x/width, y/height of the *uncropped* frame; lengths = fraction of
  the long edge. `MaskParams::sanitized` caps counts (16 corrections/16 components/200k brush points document-wide, 4096 strokes per brush)
  and scrubs NaN (the canonical hasher refuses JSON `null`). Documents are untrusted.
- **Fold**: `Add` = `max` (union), `Subtract` = `acc*(1-w)`, `Intersect` = `acc*w` (`compose::fold_step`;
  the spike's `min(a+w,1)` seamed overlapping feathers). An unavailable component (model missing / not
  baked yet) is *skipped*, even inverted -- a missing model never selects everything.
- **Bake key** = `hash(neutral_key, recipe)`, independent of `invert`/`opacity`/`op`: a mask and its
  inverse share one model run. Bakes are requested for *enabled* corrections (a fresh Select Subject has
  no adjustment yet but the user is looking at it).
- **Neutral render taps post-lens, PRE-heal** (`nicti.neutral`, keying-only, upstream `nicti.lens`): no
  tone/WB/crop/local/heal edit can re-run a model. Renderer stays a linear chain; `Renderer::render_baked`
  bakes+submits first so the engine can read the baked frame (it cannot from inside `LiveExec::encode` --
  one encoder, submitted at the end).
- **Engine costs** (tested with `MaskStats`): slider drag = uniform-only (0 recomposes; keys first, atlas reused, composites untouched even if evicted); a geometry edit
  recomposes that correction only; painting = one GPU pass/frame (prefix `[..n-1]` cached); an AI alpha
  recomposes only its corrections; a new guide rebuilds AI/range masks, not gradients/brushes.
- **Local adjustments stack additively** (`global + Σ weight·amount·delta`) in `live_suffix.wgsl`; the
  dehaze/clarity/texture/sat/hue/tint steps are skipped at exactly-zero delta and the rest are identities at
  zero, so an empty mask is bit-identical for in-range globals. Local temp/tint are per-channel
  gains (NOT a camera WB solve). CPU twin: `mask/local.rs::live_pixel`. Spatial ones (clarity/texture/
  dehaze) read bases cached per baked frame (`bases.rs`); sharpness/noise add into `detail_combine.wgsl`.
- **Halo gotcha**: the clarity/texture guided-filter `eps` was first too large (1.6e-2) and haloed a hard
  step by 25%; now coarse 2e-3 / fine 1e-3 (`bases.rs`). Dark-channel dehaze needs a sky/dense-haze pixel to
  anchor the airlight -- a uniformly hazy scene with none under-estimates it.
- **Mask extent** = frame extent capped at 4096 long edge; atlas = Rgba16Float, 4 corrections/layer; composite cache 1 GiB (16 worst-case fields).
- **Brush bounds**: dabs cap at 20k/stroke (dabs, NOT points -- `cap - points.len()` once collapsed a stroke) AND 2M tile-list entries/stroke (`raster::MAX_TILE_ENTRIES_PER_STROKE`); `compose::hash_group` hashes points as raw bytes (canonical JSON was 45 ms/frame at the point cap) with exhaustive destructuring -- a new field must be hashed or it won't compile.
- **`MaskBakeJob` Drop** fills its slot with `CANCELLED` if Pounce drops it unrun; `MaskBakeService::poll` treats that as retryable, never a failure. `set_stage_params(MASKS, ..)` sanitizes the typed value (a NaN -> `null` would wipe the parse).
- **Models are data**: recipe = `model_id`+`model_version`+`params.target`; `resolve_provider` returns a
  typed error for unknown model / version mismatch (never a silent newer model) / unsupported target;
  `providers::default_model_for(target)` is the one table for new masks. Registry ids allow only
  `[a-z0-9_]` segments (a hyphen panics at registration -- `nicti.mask.sky_heuristic`).
- **BiRefNet**: fp32 ONNX conversion `onnx-community/BiRefNet-ONNX` @ `534d3c82`, 972,666,916 B, pinned by
  SHA-256, input `input_image` `[1,3,1024,1024]` ImageNet-normalized planar, output logits `[1,1,1024,1024]`
  (sigmoid); names read from the graph. On-demand download only. **CPU bake = 9.2 s warm / 15.3 s cold on the
  Windows reference machine** (ADR-0048's <=1 s assumed CUDA) -> #345 (done, below). Provenance: upstream "trained on
  DIS-TR"; the *conversion* matches upstream on outputs (#348, `bench/birefnet-verify`, mask IoU >= 0.997).
- **GPU pack (#345)**: optional, Windows+NVIDIA, ~1.7 GB (ORT CUDA 13 + cuDNN 9.27 + cuBLAS 13.8 + fp16 BiRefNet) -> `nicti_stalk::models::gpu_pack_artifacts`, installed into `<store>/ort-cuda/` (`Payload::ZipMembers`, shared dir, all-or-nothing). **CUDA fp16 = 0.17-0.21 s warm bake, ~8 GB VRAM; DirectML = 6 s (no faster than CPU), fp32 CUDA = 0.27 s but ~13 GB.** EP chosen in `nicti-haw::session_builder` (CUDA only if `onnxruntime_providers_cuda.dll` sits beside the runtime; `NICTI_ORT_EP=cpu|cuda|directml` overrides); a failed register/load/run falls back to CPU fp32 (`birefnet.rs::open_session`). `ensure_ort_environment` prepends the runtime dir to `PATH` (cuDNN/cuBLAS load by bare name -- not found beside the DLL otherwise, -- before that fix a clean-`PATH` session registered CUDA fine and only failed at the first run). Pack takes effect after restart: the runtime is pinned per process on first resolve (`gpu_runtime_path`), and CUDA is only requested when fp16 is installed too. `MaskBakeJob` declares 8 GiB via `birefnet::declared_vram_bytes()` (informational: Foreground never reserves). Run-time mid-bake CPU retry is not exercised end to end.
- **CPU-viable model (#349)**: nothing is both interactive and BiRefNet-grade -- fp16 BiRefNet is *slower* on CPU (9.9 vs 8.0 s), BiRefNet-lite 4.9 s (near-identical masks, 1.6x), IS-Net 0.56 s (right subject, soft/leaky edges). GPU EP (#345) is the path; lite/IS-Net are optional extra providers (not registered; need pin+licence row+#171 pass). `docs/research/cpu-subject-model.md`, `bench/subject-model-bench`.
- **Disk tier for baked alphas (#353, `docs/adr/0353`)**: alphas persist in the Larder as *keyed* entries
  (`LarderKind::AiAlpha`, key = the 32-byte bake key), 8-bit quantised at bake time (`AiAlpha::quantized`, so a
  reload has the same `content_hash`) + zlib (`AiAlpha::encode`/`decode`, magic `NAL1`). `MaskBakeService` fetches
  before baking (`stash.rs` `AlphaFetchJob`, Foreground, even with the model missing) and stores after
  (`AlphaStoreJob`, Background, also for a bake that lands after the user left). Gotcha: an intact-but-undecodable
  payload is `forget_keyed`'d or `contains_keyed` blocks the replacement store.
- **Pre-bake (#353, `prebake.rs`)**: after a paste/sync/preset (not undo), touched photos minus the open one are
  baked in the background, nearest the grid cursor first, ONE photo at a time (plan -> decode -> Background
  `MaskBakeJob`s -> store). Keys come from `spine::neutral_key` (pixel-free; pinned to `DevelopView::neutral_key`
  by a test). Never downloads a model; its in-flight keys are `set_deferred_keys`'d so the foreground waits instead
  of baking twice; cancelling any of its jobs from the activity panel stops the whole run.
- **Sky** = interim flood-filled heuristic (`nicti-siamese/src/sky.rs`), beta (#347).
- **Real-hardware numbers (RTX 5080, 45 MP)**: 16 stacked masks +1.4 ms (Vulkan)/+1.9 ms (Dx12) over the
  no-mask live pass; 2.5/3.2 ms p95 in total with spatial adjustments -- inside the 4 ms rule, so no
  culling/preview split yet. Re-run: `cargo test -p nicti-tapetum --release mask::engine::tests::throughput
  -- --ignored --nocapture` (cross-built `.exe`, `NICTI_WGPU_BACKEND=vulkan|dx12`).
- **Overlay** = a CPU preview painted by egui (geometry, low-res AI alpha, range thumbnail), not a shader.
- **Real-weights test**: `crates/nicti-siamese/tests/real_models.rs` (`#[ignore]`; needs
  `NICTI_MODELS_DIR=<root>` with `birefnet/birefnet_fp32.onnx` at the exact pinned bytes +
  `NICTI_TEST_ORT_DYLIB`).
- **Export ignores masks** (its live pass never gets an atlas): `export/jobs.rs` warns per batch naming the masked photos (tested) until #354 lands -- don't remove the warning before then.
- **Not done**: Moire/Defringe (#351), undo (#324), full-res export masks (#354), post-lens neutral image
  when lens is real (#358), real-photo quality (#171).

## Package contents

- **`crates/nicti-tapetum/src/mask/`** -- `params.rs`, `raster.rs` (CPU references), `compose.rs` (fold, bake
  key, stamp), `kernels.rs`/`guided.rs`/`bases.rs` (GPU: gradients, brush strokes, compose, pack, range, guided
  filter, bands, dehaze), `local.rs` (per-mask adjustments + CPU twin), `atlas.rs`, `engine.rs` (`MaskEngine`).
- **`crates/nicti-siamese`** -- `birefnet.rs`, `sky.rs`, `providers.rs`, `neutral.rs`, `backend.rs`, `job.rs`.
- **`crates/nicti-stalk`** -- the provider traits + `resolve_provider`; `models.rs`'s `BIREFNET`, `mask_artifacts`,
  `MaskModels`, `verify_artifacts`.
- **`crates/nicti-pelt`** -- `mask_panel.rs`, `mask_edit.rs`, `mask_tool.rs`; `render.rs`'s mask API
  (`mask_bake_requests`, `set_ai_alpha`, `prune_ai_alphas`, `neutral_key`); `stash.rs` (#353: `AlphaFetchJob`/
  `AlphaStoreJob`), `prebake.rs` (#353: `PrebakeService`, `nearest_first`).
