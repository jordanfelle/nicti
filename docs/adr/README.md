# Architecture Decision Records

Nicti records significant technical decisions as ADRs, numbered sequentially starting at
`0001-*.md`. See `CONTRIBUTING.md` for when to write one.

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
| [0019](0019-raw-decoder.md) | RAW decoder | Proposed |
| [0020](0020-volume-identity-and-remapping.md) | Volume identity and drive remapping | Proposed |

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
