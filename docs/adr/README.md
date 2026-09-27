# Architecture Decision Records

Nicti records significant technical decisions as ADRs. See `CONTRIBUTING.md` for when to write
one.

**Numbering convention (changed 2026-09-27): a new ADR's number is its GitHub issue number, not
the next sequential count.** `0001`-`0025` were assigned sequentially, one at a time by a single
person; once multiple agents began working concurrent, long-lived branches, that scheme
collided three times in one afternoon (PR #167 vs. #152's `0022`, vs. #159's `0023`, vs. #175's
`0024`) — each branch's "next free slot" guess, taken from whatever `main` looked like at branch
creation, silently went stale by merge time, since nothing else about a sequential counter is
knowable without checking every other in-flight branch. GitHub issue numbers don't have this
problem: they're allocated atomically and uniquely by GitHub itself, and every ADR already records
its own ticket in the `**Ticket:**` field, so this reuses information that already existed rather
than inventing new bookkeeping. Going forward: name a new ADR `docs/adr/00NN-slug.md` where `NN`
is its ticket's issue number (zero-padded to at least 4 digits, matching the existing files'
width), and title it `# ADR-00NN: ...` to match. If one ticket ever needs a second, later ADR,
append a lowercase letter (`0040a-...`, `0040b-...`) rather than claiming a new ticket number for
it. **The existing `0001`-`0025` files keep their original sequential numbers** — rekeying them to
this convention is tracked as its own backlog item ([#183](https://github.com/jordanfelle/nicti/issues/183)),
not done as part of this change.

**ADRs are point-in-time records, not living documentation.** Phrases like "this session," "this
sandbox," "the reference machine," or "solo + agent productivity" describe the context the
decision was actually made in — often a single research pass run by one person with heavy AI-agent
assistance, before the project accepted outside contributors. Don't read those phrases as current
policy; check `CLAUDE.md` and `CONTRIBUTING.md` for how the project works today. A later ADR may
add a dated "Context update" section (see ADR-0001) rather than rewriting the original decision
text — the original reasoning stays intact even if its framing has since been revisited.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-language-and-stack.md) | Implementation language and native stack | Accepted (see Context update) |
| [0002](0002-non-destructive-edit-model.md) | Non-destructive edit model | Accepted |
| [0003](0003-third-party-license-policy.md) | Third-party license policy | Accepted |
| [0004](0004-module-plugin-architecture.md) | Module/plugin architecture (Claw) | Accepted |
| [0005](0005-gpu-compute-api.md) | GPU compute API (`wgpu`) | Accepted |
| [0006](0006-gui-framework.md) | GUI framework | Proposed |
| [0007](0007-healing-and-removal.md) | Healing and removal | Proposed |
| [0008](0008-catalog-database-engine.md) | Catalog database engine | Accepted |
| [0009](0009-turso-database-evaluation.md) | Turso Database (catalog store candidate) | Rejected |
| [0010](0010-redb-evaluation.md) | `redb` (catalog store candidate) | Rejected |
| [0011](0011-facet-count-cache.md) | Facet-count cache for SQLite's faceted-filter gap | Accepted |
| [0012](0012-duckdb-as-primary-catalog-store.md) | Reconsidering DuckDB as the v1 primary catalog store | Accepted (not adopted) |
| [0013](0013-outbound-license-agpl.md) | Nicti's outbound license — AGPL-3.0-or-later | Accepted |
| [0014](0014-libsql-evaluation.md) | libSQL (catalog store candidate) | Rejected for v1 |
| [0015](0015-rocksdb-evaluation.md) | RocksDB (catalog store candidate) | Rejected |
| [0016](0016-fjall-evaluation.md) | fjall (catalog store candidate) | Rejected |
| [0017](0017-preview-tier-strategy.md) | Preview tier strategy | Accepted |
| [0018](0018-rapidraw-adopt-or-fork.md) | RapidRAW as an adopt/fork candidate | Proposed (not adopted, study-only) |
| [0019](0019-raw-decoder.md) | RAW decoder | Accepted |
| [0020](0020-volume-identity-and-remapping.md) | Volume identity and drive remapping | Proposed |
| [0021](0021-color-pipeline.md) | Color pipeline | Proposed |
| [0022](0022-preview-codec-followup.md) | T2 preview-codec follow-up (faster AVIF speeds + lossy WebP) | Accepted |
| [0023](0023-lrc-catalog-import-mapping.md) | Lightroom Classic catalog import mapping | Accepted |
| [0024](0024-masking.md) | Masking | Proposed |
| [0025](0025-burst-duplicate-grouping.md) | Burst/duplicate grouping | Proposed |
| [0040](0040-demosaic-and-denoise.md) | Demosaic and noise reduction | Proposed |

A per-topic summary (the actionable conclusion, without the full research trail) lives in
`docs/decisions/<topic>.md` — see `CLAUDE.md`'s Architecture decisions section for the topic map.

**Numbering note:** 0018 and 0019 both touch the RAW decoder question (adopt/fork research vs.
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
