# ADR-0218: Local-only default for AI culling/suggestion features

- **Status:** Accepted
- **Date:** 2026-09-27
- **Ticket:** #218 Requirements: local-only constraint for AI culling/suggestion features (nicti-ai)

## Context

`crates/nicti-stalk` defines `ModelProvider` (ADR-0019 §7/§8) as an extension-point trait only —
identity and versioning via `Module`, no inference method yet. Real implementations are the scope
of the AI-labeled tickets: masking (#49), healing/removal (#51), auto-tone (#53), and culling
(#34/#35/#36). Before any of those lands a real model, this ADR makes explicit a constraint that
wasn't written down anywhere: any AI feature that learns from or analyzes a photographer's own
library — much of it personal or client work — must not touch the network by default.

This also has to cover third-party `ModelProvider` modules, not just first-party ones. Claw's v2
WASM plugin direction (ADR-0019) and its manifest/consent/kill-switch model (#214, still in
research) are the enforcement mechanism for that half of the requirement.

## Decision

1. **Local inference and training are offline by default.** No telemetry, no calling out to a
   hosted API, for any `ModelProvider` implementation performing batch culling or edit-suggestion
   analysis. This is the default with no configuration required — not an opt-out.
2. **Weight delivery may use the network, but only user-initiated.** A model's weights (e.g.
   BiRefNet's ~970MB export, per the masking topic) may be fetched over the network only from an
   explicit user action (e.g. an "Install AI models" step) that states the source and size before
   downloading, and verifies a checksum after. There is no silent first-run auto-fetch. Once
   installed, inference itself never touches the network — a weight update is a new instance of
   the same user-initiated, checksummed download, not a background refresh.
3. **Cloud AI is a separate, distinct feature.** A provider may offer a feature that sends data to
   an external/cloud AI service, but it must be clearly separate from the local-only default (a
   different named feature, not a toggle on the local one) and gated behind an explicit opt-in
   that walks the user through the actual risk, cost, and privacy tradeoffs before enabling it —
   not a checkbox buried in general settings.
4. **Applies to every `ModelProvider` implementation, first- or third-party.** A first-party
   module (masking, healing, auto-tone, culling) follows this by construction, reviewed the same
   as any other PR. A third-party module loaded through Claw needs an enforcement mechanism, since
   its code isn't first-party-reviewed — that's #214's manifest/consent/kill-switch scope: network
   access must be an action-scoped capability the manifest declares (granted only for an explicit,
   user-initiated weight download or a separately opted-in cloud feature per points 2-3 above),
   denied by default during local inference/training, and a module that doesn't declare it must
   not get it at all.

## Consequences

- #49 (masks + local adjustments), #51 (healing/removal), #53 (AI auto-tone), #34/#35/#36
  (culling: blur/misfocus, face/subject grouping, AI culling assist) each build their real
  `ModelProvider` against this constraint — no network call in the default inference/training
  path, and any weight-fetch UI they add follows the user-initiated + checksummed shape above.
- #214's Claw manifest design must include a declarable network-access permission (or equivalent
  capability gate) so a third-party AI module's own runtime enforces this without relying on the
  module's own code to behave.
- Deferred: the exact UI for the weight-installation flow and for the cloud-AI opt-in walkthrough
  are each their own ticket's scope (whichever of #49/#51/#53/#34-#36 ships the first real model),
  not decided here. **Update (2026-09-29):** #51 shipped first. Its weight-installation flow is
  `nicti-stalk::models` (a pinned, checksummed, atomic, user-initiated store) plus the Develop
  panel's download prompt, which shows sources, size and license before the user agrees — see
  [ADR-0051](0051-healing-removal-build.md). The cloud-AI opt-in walkthrough is still open.
