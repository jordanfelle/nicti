---
paths:
  - "spikes/siamese/**"
  - "crates/nicti-ai/**"
  - "docs/adr/0024-masking.md"
---

# Masking — Quick Reference

Full reasoning/history: `docs/decisions/masking.md`.

- **Masking (#48)** — `docs/adr/0024-masking.md`: **Proposed**, pending a reference-machine pass
  (real BiRefNet/MobileSAM weights, real photos including fursuiters, #44's own gating). Model
  choice: BiRefNet (one-shot subject/background) + MobileSAM (interactive click/box, real
  encoder/decoder split — the embedding bakes once, only the decoder re-runs per click). SAM2
  re-checked (SA-V dataset now confirmed CC-BY-4.0) but not adopted. Sky: no clean model adopted,
  ships as a top-row-flood-filled luminance/blue-dominance heuristic in `sky.rs`.
- **`MaskGroup`/`MaskComponent`** (`spikes/siamese/src/compose.rs`) — named to match LRC's
  `MaskGroupBasedCorrections` for #49/#62. `AiRecipe.model_version: String` (not `u32` — differs
  from `spikes/groom/src/spot.rs::MaskRecipe`, groom/#51 should align to this). A mask's inverse is
  `invert: bool` on a component sharing the same recipe, never a second recipe — `ai_bake_key()` is
  independent of `invert`/`opacity`, so the model runs once for a mask + its inverse pair.
- **Cache key**: AI masks infer against a fixed neutral render (post-lens-correction, default
  tone), never the live edit stack — a tone slider must not invalidate an AI mask's bake.
- **Geometry** (`geometry.rs`): linear/radial gradient + brush (ordered strokes, per-stroke
  add/erase, dabs blend via `max` within a stroke).
- **Refine** (`refine.rs`): guided filter (He/Sun/Tang), preview-res alpha → full-res, edge-aware
  via the photo's own luminance — not a plain bilinear alpha upsample.
- **GPU**: 4 WGSL kernels (`gradient_linear`/`gradient_radial`/`brush`/`compose`/`masked_adjust` —
  5 files, `compose` called once per component), each its own file (this repo's convention: one
  entry point per `.wgsl` file, since WGSL requires unique `@group`/`@binding` pairs module-wide,
  not just per entry point — a single multi-kernel file fails to compile). Parity-tested within
  `1e-4` against lavapipe (9 tests).
- **No real ONNX weights** — same "prove the loading shape, not real inference" posture as
  `spikes/groom/src/ai.rs` (ADR-0007). A full BiRefNet ONNX export exists publicly (~970MB) but
  wasn't downloaded (time budget, not a technical block — network access was confirmed available).
