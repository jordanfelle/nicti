# Architecture Decision Records

Nicti records significant technical decisions as ADRs. See `CONTRIBUTING.md` for when to write
one.

**Numbering convention (changed 2026-09-27): a new ADR's number is its GitHub issue number, not
the next sequential count.** The 25 oldest ADRs were originally assigned sequential numbers
(`0001`-`0025`), one at a time by a single person; once multiple agents began working concurrent,
long-lived branches, that scheme collided three times in one afternoon (PR #167 vs. #152's
ADR-0143, vs. #159's ADR-0061, vs. #175's ADR-0048 — cited here by their current ticket-keyed
numbers) — each branch's "next free slot" guess, taken from whatever `main` looked like at branch
creation, silently went stale by merge time, since nothing else about a sequential counter is
knowable without checking every other in-flight branch. GitHub issue numbers don't have this
problem: they're allocated atomically and uniquely by GitHub itself, and every ADR already records
its own ticket in the `**Ticket:**` field, so this reuses information that already existed rather
than inventing new bookkeeping. Going forward: name a new ADR `docs/adr/00NN-slug.md` where `NN`
is its ticket's issue number (zero-padded to at least 4 digits, matching the existing files'
width), and title it `# ADR-00NN: ...` to match. If one ticket ever needs a second, later ADR,
append a lowercase letter (`0040a-...`, `0040b-...`) rather than claiming a new ticket number for
it. **The original `0001`-`0025` files have since been rekeyed to their ticket numbers
([#183](https://github.com/jordanfelle/nicti/issues/183))** — see the Index's "Formerly" column
for the old→new mapping, and each rekeyed file's own `**Formerly:**` line.

**ADRs are point-in-time records, not living documentation.** Phrases like "this session," "this
sandbox," "the reference machine," or "solo + agent productivity" describe the context the
decision was actually made in — often a single research pass run by one person with heavy AI-agent
assistance, before the project accepted outside contributors. Don't read those phrases as current
policy; check `CLAUDE.md` and `CONTRIBUTING.md` for how the project works today. A later ADR may
add a dated "Context update" section (see ADR-0015) rather than rewriting the original decision
text — the original reasoning stays intact even if its framing has since been revisited.

## Index

| # | Formerly | Title | Status |
|---|---|---|---|
| [0015](0015-language-and-stack.md) | 0001 | Implementation language and native stack | Accepted (see Context update) |
| [0016](0016-gpu-compute-api.md) | 0005 | GPU compute API (`wgpu`) | Accepted |
| [0018](0018-third-party-license-policy.md) | 0003 | Third-party license policy | Accepted |
| [0019](0019-module-plugin-architecture.md) | 0004 | Module/plugin architecture (Claw) | Accepted |
| [0021](0021-non-destructive-edit-model.md) | 0002 | Non-destructive edit model | Accepted |
| [0023](0023-keywords-collections-filter.md) | — | Keywords, collections, and filter/search backend | Accepted |
| [0024](0024-manual-catalog-sync.md) | — | Manual catalog sync, not a live filesystem watcher | Accepted |
| [0025](0025-continuous-catalog-backup.md) | — | Continuous catalog backup + integrity checks (Nine Lives) | Accepted |
| [0029](0029-preview-tier-strategy.md) | 0017 | Preview tier strategy | Accepted |
| [0026](0026-verified-folder-move.md) | — | Verified folder move (Carry) | Accepted |
| [0032](0032-culling-ux.md) | — | Culling UX (marking, survey/compare, filter-select-delete) | Accepted |
| [0033](0033-burst-duplicate-grouping.md) | 0025 | Burst/duplicate grouping | Proposed |
| [0034](0034-blur-misfocus-eye-detection.md) | — | Blur/misfocus/eye detection | Proposed |
| [0035](0035-subject-grouping.md) | — | Subject grouping | Proposed — measurement pending |
| [0037](0037-raw-decoder.md) | 0019 | RAW decoder | Accepted |
| [0038](0038-color-pipeline.md) | 0021 | Color pipeline | Proposed |
| [0040](0040-demosaic-and-denoise.md) | — | Demosaic and noise reduction | Proposed |
| [0042](0042-color-management.md) | — | Color management | Accepted |
| [0044](0044-stage-cached-render-graph.md) | — | Stage-cached render graph (Tapetum) | Proposed |
| [0047](0047-crop-straighten-autolevel.md) | — | Crop, straighten, and auto-level | Accepted |
| [0048](0048-masking.md) | 0024 | Masking | Proposed |
| [0049](0049-masking-build.md) | — | Masks and local adjustments — the build (engine, local adjustments, pluggable AI models, Masks tool) | Accepted |
| [0050](0050-healing-and-removal.md) | 0007 | Healing and removal | Proposed |
| [0051](0051-healing-removal-build.md) | — | Healing and removal — the build (GPU clone/heal, AI removal, model store) | Accepted |
| [0052](0052-presets-copy-paste-sync.md) | — | Develop presets, copy/paste settings, and sync across a selection | Accepted |
| [0053](0053-ai-auto-tone.md) | — | AI auto-tone (MLP, per-user edit history) | Proposed |
| [0054](0054-job-scheduler-pounce.md) | — | Job scheduler design (Pounce) | Proposed |
| [0056](0056-export-stack.md) | — | Export stack | Proposed |
| [0057](0057-export-pipeline.md) | — | Export pipeline (engine, chained Pounce jobs, naming/collisions, edit persistence) | Accepted |
| [0059](0059-xmp-interop.md) | — | XMP interop with Lightroom Classic | Proposed |
| [0061](0061-lrc-catalog-import-mapping.md) | 0023 | Lightroom Classic catalog import mapping | Accepted |
| [0062](0062-lrc-catalog-import.md) | — | Lightroom Classic catalog import (the build) | Accepted |
| [0066](0066-outbound-license-agpl.md) | 0013 | Nicti's outbound license — AGPL-3.0-or-later | Accepted |
| [0067](0067-catalog-database-engine.md) | 0008 | Catalog database engine | Accepted |
| [0068](0068-gui-framework.md) | 0006 | GUI framework | Accepted |
| [0069](0069-rapidraw-adopt-or-fork.md) | 0018 | RapidRAW as an adopt/fork candidate | Proposed (not adopted, study-only) |
| [0071](0071-volume-identity-and-remapping.md) | 0020 | Volume identity and drive remapping | Proposed |
| [0072](0072-tiered-thumbnail-storage.md) | — | Tiered thumbnail storage (SSD catalog / archive sidecars) | Accepted |
| [0099](0099-classic-auto-tone.md) | — | Classic (non-AI) auto-tone algorithm | Proposed |
| [0101](0101-auto-op-graceful-degradation.md) | — | Graceful degradation for automatic develop operations | Accepted |
| [0102](0102-turso-database-evaluation.md) | 0009 | Turso Database (catalog store candidate) | Rejected |
| [0103](0103-facet-count-cache.md) | 0011 | Facet-count cache for SQLite's faceted-filter gap | Accepted |
| [0106](0106-redb-evaluation.md) | 0010 | `redb` (catalog store candidate) | Rejected |
| [0107](0107-duckdb-as-primary-catalog-store.md) | 0012 | Reconsidering DuckDB as the v1 primary catalog store | Accepted (not adopted) |
| [0108](0108-yolo-culling-detection-candidate.md) | — | Ultralytics YOLO as a culling/detection candidate | Accepted (deferred, no dedicated integration) |
| [0113](0113-libsql-evaluation.md) | 0014 | libSQL (catalog store candidate) | Rejected for v1 |
| [0115](0115-rocksdb-evaluation.md) | 0015 | RocksDB (catalog store candidate) | Rejected |
| [0116](0116-fjall-evaluation.md) | 0016 | fjall (catalog store candidate) | Rejected |
| [0143](0143-preview-codec-followup.md) | 0022 | T2 preview-codec follow-up (faster AVIF speeds + lossy WebP) | Accepted |
| [0145](0145-rendered-screen-tier.md) | — | Rendered screen-preview tier (stale-while-revalidate) | Accepted |
| [0156](0156-lrcat-data-rocksdb-blob-linkage.md) | — | `.lrcat-data` blob linkage and #62 import policy | Accepted |
| [0158](0158-lrc-hash-relink-seeding.md) | — | LRC `md5`/`importHash` vs homing's relink fingerprints | Accepted |
| [0214](0214-claw-plugin-manifest-and-kill-switch.md) | — | Claw v2 plugin manifest, disclosure UX, and kill switch (Collar/Hiss) | Proposed |
| [0218](0218-local-only-ai.md) | — | Local-only default for AI culling/suggestion features | Accepted |
| [0249](0249-windows-installer-and-updates.md) | — | Windows installer, code signing, and auto-update | Proposed |
| [0353](0353-baked-alpha-disk-tier.md) | — | Disk tier for baked AI mask alphas, and the background pre-bake (Stash) | Accepted |
| [0381](0381-calibration-and-lrc-profile-resolution.md) | — | Camera Calibration, LRC camera-profile resolution, and a Look's own tone curve | Accepted |
| [0432](0432-develop-curves-grading-point-color.md) | — | Develop UI: point curves, colour mixer, Color Grading and Point Color (OkLab) | Accepted |
| [0428](0428-lens-stage-ca-dng-defringe.md) | — | The lens stage (DNG-embedded profile, automatic lateral CA) and global Defringe | Accepted |
| [0410](0410-nef-embedded-lens-corrections.md) | — | Nikon's embedded lens-correction data as the NEF lens source (opt-in until verified) | Accepted |
| [0380](0380-global-presence-and-effects.md) | — | Global Presence (texture/clarity/dehaze/saturation) and post-crop Effects (vignette/grain) | Accepted |

A per-topic summary (the actionable conclusion, without the full research trail) lives in
`docs/decisions/<topic>.md` — see `CLAUDE.md`'s Architecture decisions section for the topic map.

**Numbering note:** 0069 and 0037 both touch the RAW decoder question (adopt/fork research vs.
the decoder itself); this is intentional, not a numbering error — cross-reference both.

## Template

```markdown
# ADR-NNNN: Title

- **Status:** Proposed | Accepted | Rejected
- **Date:** YYYY-MM-DD
- **Ticket:** #N description

## Context

What prompted this decision, and what constraints/goals it has to satisfy.

## Decision

What was decided, and why, including alternatives seriously considered and why they lost.

## Consequences

What this unblocks, what it requires elsewhere, what's explicitly deferred.
```
