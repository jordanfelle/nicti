---
paths:
  - "crates/nicti-siamese/**"
  - "crates/nicti-stalk/**"
  - "crates/nicti-tapetum/src/mask/**"
  - "crates/nicti-tapetum/shaders/mask_*.wgsl"
  - "crates/nicti-tapetum/shaders/guide*.wgsl"
  - "crates/nicti-tapetum/shaders/dehaze_*.wgsl"
  - "crates/nicti-pelt/src/mask_*.rs"
  - "docs/adr/0048-masking.md"
  - "docs/adr/0049-masking-build.md"
---

# Masking — Quick Reference

Full reasoning/history: `docs/decisions/masking.md`; the build is `docs/adr/0049-masking-build.md`.

- **What exists (#49)**: one `nicti.masks` stage (`MaskParams` -> `LocalCorrection { mask: MaskGroup,
  adjust: LocalAdjust, amount }`), `nicti-tapetum/src/mask/`. Models in `nicti-siamese`, pluggable via
  `nicti-stalk`'s `SegmentationProvider`/`SegmentationRegistry`. UI in `nicti-pelt`'s `mask_panel.rs`
  (egui), `mask_edit.rs` (egui-free ops, unit-tested), `mask_tool.rs` (`MaskBakeService`).
- **Normalized coordinates**: points = x/width, y/height of the *uncropped* frame; lengths = fraction of
  the long edge. `MaskParams::sanitized` caps counts (16 corrections/16 components/200k brush points)
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
- **Engine costs** (tested with `MaskStats`): slider drag = uniform-only (0 recomposes); a geometry edit
  recomposes that correction only; painting = one GPU pass/frame (prefix `[..n-1]` cached); an AI alpha
  recomposes only its corrections; a new guide rebuilds AI/range masks, not gradients/brushes.
- **Local adjustments stack additively** (`global + Σ weight·amount·delta`) in `live_suffix.wgsl`; each
  step is skipped at exactly-zero delta so an empty mask is bit-identical. Local temp/tint are per-channel
  gains (NOT a camera WB solve). CPU twin: `mask/local.rs::live_pixel`. Spatial ones (clarity/texture/
  dehaze) read bases cached per baked frame (`bases.rs`); sharpness/noise add into `detail_combine.wgsl`.
- **Halo gotcha**: the clarity/texture guided-filter `eps` was first too large (1.6e-2) and haloed a hard
  step by 25%; now coarse 2e-3 / fine 1e-3 (`bases.rs`). Dark-channel dehaze needs a sky/dense-haze pixel to
  anchor the airlight -- a uniformly hazy scene with none under-estimates it.
- **Mask extent** = frame extent capped at 4096 long edge; atlas = Rgba16Float, 4 corrections/layer.
- **Models are data**: recipe = `model_id`+`model_version`+`params.target`; `resolve_provider` returns a
  typed error for unknown model / version mismatch (never a silent newer model) / unsupported target;
  `providers::default_model_for(target)` is the one table for new masks. Registry ids allow only
  `[a-z0-9_]` segments (a hyphen panics at registration -- `nicti.mask.sky_heuristic`).
- **BiRefNet**: fp32 ONNX conversion `onnx-community/BiRefNet-ONNX` @ `534d3c82`, 972,666,916 B, pinned by
  SHA-256, input `input_image` `[1,3,1024,1024]` ImageNet-normalized planar, output logits `[1,1,1024,1024]`
  (sigmoid); names read from the graph. On-demand download only. **CPU bake = 9.2 s warm / 15.3 s cold on the
  Windows reference machine** (ADR-0048's <=1 s assumed CUDA) -> #345. Provenance: upstream "trained on
  DIS-TR"; the *conversion* vs upstream is unverified (#348).
- **Sky** = interim flood-filled heuristic (`nicti-siamese/src/sky.rs`), beta (#347).
- **Real-hardware numbers (RTX 5080, 45 MP)**: 16 stacked masks +1.4 ms (Vulkan)/+1.9 ms (Dx12) over the
  no-mask live pass; 2.5/3.2 ms p95 in total with spatial adjustments -- inside the 4 ms rule, so no
  culling/preview split yet. Re-run: `cargo test -p nicti-tapetum --release mask::engine::tests::throughput
  -- --ignored --nocapture` (cross-built `.exe`, `NICTI_WGPU_BACKEND=vulkan|dx12`).
- **Overlay** = a CPU preview painted by egui (geometry, low-res AI alpha, range thumbnail), not a shader.
- **Real-weights test**: `crates/nicti-siamese/tests/real_models.rs` (`#[ignore]`; needs
  `NICTI_MODELS_DIR=<root>` with `birefnet/birefnet_fp32.onnx` at the exact pinned bytes +
  `NICTI_TEST_ORT_DYLIB`).
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
  (`mask_bake_requests`, `set_ai_alpha`, `prune_ai_alphas`, `neutral_key`).
