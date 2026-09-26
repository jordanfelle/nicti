---
paths:
  - "deny.toml"
  - "docs/licensing.md"
  - "**/Cargo.toml"
  - "LICENSE"
---

# Licensing — Quick Reference

Full reasoning/history: `.claude/docs/licensing/README.md`.

- **Third-party license policy** — `docs/adr/0003` + full audit in `docs/licensing.md` (Rust crate
  allowlist, ML-model bundle-vs-download criteria, no Adobe DCP/LCP data). **Amended 2026-09-24**
  (load-bearing, not historical) — read the Amendments section, not just the original Decision
  text. Update `docs/licensing.md` in the same PR as any new dependency/model.
- **Outbound license: AGPL-3.0-or-later** — `docs/adr/0013`. Chosen over plain GPL-3.0 for the
  network-use clause (§13) — closes the hosted-service loophole, relevant to #58/#64. Un-excludes
  Ultralytics YOLO and exiv2/rexiv2 on license grounds. Removes the LGPL-cdylib-isolation
  requirement for `lensfun-rs` specifically (its `LGPL-3.0-or-later OR GPL-3.0` dual license
  combines cleanly) — **not** for `rawler` (bare `LGPL-2.1`, no `-or-later` confirmed; #37 must
  resolve before treating it as pre-cleared). Reopens RapidRAW (#69) as an adopt/fork candidate.
- **RapidRAW adopt/fork, resolved** — `docs/adr/0018`: **not adopted, study-only**. Checked its
  real ~583-crate dependency graph against `deny.toml`: 582 crates pass cleanly, one rejection
  (`rawler`'s bare `LGPL-2.1`, same open #37 question above) — real corroborating evidence, not a
  blocker either way. Architecturally incompatible regardless of licensing: no DAG/per-stage
  cache (opposite of Tapetum/#44's design), monolithic module structure (no Claw-style
  extension points), untyped JSON sidecar edit storage (no catalog DB). See
  `docs/research/stalk-prior-art.md` for the full
  RapidRAW/vkdt/Ansel findings and file:line citations.
