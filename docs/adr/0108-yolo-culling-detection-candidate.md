# ADR-0108: Ultralytics YOLO as a culling/detection candidate

- **Status:** Accepted — deferred, no dedicated integration this pass
- **Date:** 2026-09-27
- **Ticket:** [#108](https://github.com/jordanfelle/nicti/issues/108) Research: Ultralytics YOLO
  as a culling/detection candidate, now that AGPL is viable

## Context

`docs/licensing.md` named Ultralytics YOLO as a "culling/detection candidate" but no ticket had
actually researched whether/where a YOLO-family detector fits Nicti's culling pipeline. It was
excluded on license grounds (AGPL-3.0) until ADR-0066 made Nicti's own outbound license
AGPL-3.0-or-later, removing that exclusion (`docs/licensing.md`'s 2026-09-24 update).

Three questions, per the ticket:

1. Does a YOLO-family detector (bounding-box localization) add anything over what #34
   (blur/misfocus/eye scoring) and #35 (DINOv2/DINOv3 embedding-based subject grouping) already
   decided, or is it redundant with them?
2. What's its real accuracy on fursuiters specifically, not just human/COCO classes — the same
   bar #34/#35 were built around (per #4's requirement)?
3. Which YOLO version/weights, and does the AGPL-3.0 assumption in `docs/licensing.md` hold up
   against the actual repo/weights in question?

Both blocking issues (#34, #35) are closed on GitHub — their research passes shipped — even though
their own ADRs (ADR-0034, ADR-0035) remain **Proposed**, pending a later real-photo measurement
pass (#238/#243) rather than Accepted. Each already shipped its own "Consequences" section
addressing #108 directly, so this ADR mostly collects and confirms their findings rather than
starting from scratch; it doesn't depend on either ADR reaching Accepted first.

## Decision

**1. Complementary, not redundant.** ADR-0034 scores *sharpness within a region* (a frame's
sharpest tile, or the Nikon AF-area) — a different primitive from a detector's bounding box. A
detector doesn't do that job and isn't needed for it. ADR-0035's own pipeline question is closer:
its cited prior art, [Fursee: Hybrid YOLO-DINOv3 Framework for Fursuit Identity Retrieval and
Clustering](https://arxiv.org/html/2606.22872v1), runs YOLO26l as a head-crop *pre-processing*
step feeding a DINOv3 embedder — detection and embedding are sequential stages in that pipeline,
not competing choices for the same job. Whether that pre-crop stage is worth adding to Nicti's own
pipeline is exactly ADR-0035's own full-frame-vs-crop ablation (still pending real measurement,
#243) — this ADR doesn't duplicate that experiment, it names it as the actual answer mechanism.

**2. Real fursuiter accuracy: the only evidence found is the Fursee paper's downstream clustering
result**, not a standalone YOLO detection benchmark. Their hybrid YOLO26l→DINOv3→DBSCAN pipeline
reaches 93.33% retrieval hit rate / 0.8755 clustering F1 on fursuit identity, beating a
general-purpose VLM baseline (85% / 0.7043). That's evidence the *pipeline* works well on
fursuiters, but it measures end-to-end clustering accuracy, not YOLO's own detection
precision/recall in isolation — no ticket has run a standalone fursuit-head detection benchmark,
and this ADR doesn't fabricate one. If #243's ablation shows the crop step meaningfully helps,
that's the point to also measure detection accuracy directly (false-negative head-crops would
silently drop a subject from clustering).

**3. License re-verified against the primary source** (fetched 2026-09-27, not just re-trusting
`docs/licensing.md`'s existing 2026-09-23 entry): `github.com/ultralytics/ultralytics`'s `LICENSE`
file is the verbatim GNU AGPL v3 (19 November 2007) text. The repo dual-licenses — AGPL-3.0 for
open-source use, a separate paid Ultralytics Enterprise License for commercial use that bypasses
AGPL's obligations — matching `docs/licensing.md`'s existing footnote (`[^m10]`, "Same AGPL-3.0...
commercial license sold separately"). **No drift found; the existing row stands.** Current model
family is **YOLO26** (n/s/m/l/x variants — YOLO27 is previewed but not yet released), matching the
Fursee paper's YOLO26l. ONNX export is a first-class supported path
(`model.export(format="onnx")`), which fits this repo's existing `ort`-based inference pattern
(the DINOv2 wrapper `spikes/litter`/`spikes/rosette` already use) rather than requiring a new
inference runtime or FFI binding.

**4. No standalone YOLO integration this pass.** The concrete, already-scoped mechanism to decide
whether a detection stage earns its place in Nicti is ADR-0035's crop ablation, and that
measurement is still pending real labelled data (#243). Building a dedicated YOLO integration now
would duplicate that pending experiment with no new information. #108's own three questions are
answered (complementary primitive; license-clear with no drift; ONNX-exportable, fits existing
tooling) but *whether to adopt it* stays gated on #243:

- If #243 shows cropping meaningfully improves clustering accuracy, adopt YOLO26 (or measure a
  lighter detector against it) as the crop stage there.
- If full-frame performs comparably, YOLO becomes lower priority for subject grouping
  specifically — it would need its own justification elsewhere (e.g. #36's culling-assist scope,
  or #217's live reshoot-quality indicator) before its added AGPL-3.0 model-bundling weight is
  worth carrying.

**Alternative considered:** a non-YOLO, non-AGPL detector, to avoid an AGPL model dependency.
Nicti's own outbound license is already AGPL-3.0-or-later (ADR-0066), so this wouldn't avoid any
licensing category Nicti doesn't already carry — and no evidence was found that a smaller/different
detector performs differently on fursuit heads. Not pursued absent a concrete reason to avoid
Ultralytics's specific weights.

## Consequences

- No new crate, model weight, or `docs/licensing.md` row added this pass — the existing YOLO row
  (verified 2026-09-23, re-confirmed here) already covers it.
- ADR-0035's crop ablation (blocked on #243) remains the gating experiment for whether a detection
  stage gets built; this ADR doesn't change that ticket's scope.
- ADR-0218's local-only/user-initiated-download rules would apply to any future YOLO weight fetch,
  same as DINOv3's Hugging Face gate in ADR-0035.
- #36 (AI culling assist integration) stays blocked by #180/#238's own measurement passes, not by
  this ticket.
- Closing #108 against this ADR.
