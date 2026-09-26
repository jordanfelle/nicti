---
paths:
  - "deny.toml"
  - "docs/licensing.md"
  - "**/Cargo.toml"
  - "LICENSE"
---

# Licensing — Quick Reference

Full reasoning/history: `docs/decisions/licensing.md`.

- **Third-party license policy** — `docs/adr/0003` + full audit in `docs/licensing.md` (Rust crate
  allowlist, ML-model bundle-vs-download criteria, no Adobe DCP/LCP data). **Amended 2026-09-24**
  (load-bearing, not historical) — read the Amendments section, not just the original Decision
  text. Update `docs/licensing.md` in the same PR as any new dependency/model.
- **Outbound license: AGPL-3.0-or-later** — `docs/adr/0013`. Chosen over plain GPL-3.0 for the
  network-use clause (§13) — closes the hosted-service loophole, relevant to #58/#64. Un-excludes
  Ultralytics YOLO and exiv2/rexiv2 on license grounds. Removes the LGPL-cdylib-isolation
  requirement for `lensfun-rs` (its `LGPL-3.0-or-later OR GPL-3.0` dual license combines cleanly),
  and — **corrected 2026-09-25 by #37** — for `rawler`/LibRaw too: an earlier draft treated their
  bare `LGPL-2.1` grants (no confirmed `-or-later`) as blocking, on the theory that LGPL-2.1 §3's
  relicense option would force GPL-2.0-only. That's wrong on the primary-source text: §3 lets
  whoever exercises it pick any GPL version, and more importantly **§§5-6 already permit combining
  an LGPL-2.1 library into a differently-licensed larger work with no relicensing at all** —
  exactly LGPL's purpose, given that AGPL permits modification/reverse-engineering (it does, by its
  copyleft nature). §6(a)'s source condition (for static linking: the *whole combined executable*,
  not just the library, in relinkable form) is satisfied via **§6(d)** instead (equivalent access
  from the *same place* the binary ships from — a GitHub Release does this naturally, since Nicti
  is itself AGPL-3.0-or-later open source), not automatically by vendoring or public-repo existence
  alone. See `docs/licensing.md`'s Flags §2 for the full citation trail (gnu.org primary sources).
  What's left is a real distribution-mechanics + notice checklist to verify per release (§6(d)'s
  same-place condition + prominent notice + LGPL license text in the shipped product), not a
  licensing blocker. Reopens RapidRAW (#69) as an adopt/fork
  candidate — but **#37 found RapidRAW doesn't actually decode HE/HE\* either** (its own rawler
  fork still rejects it, falling back to the embedded JPEG), so it isn't a shortcut past #37's own
  decoder work.
- **RapidRAW adopt/fork, resolved** — `docs/adr/0018`: **not adopted, study-only**. Checked its
  real ~583-crate dependency graph against `deny.toml`: 582 crates pass cleanly, one rejection
  (`rawler`'s bare `LGPL-2.1`, already resolved compatible per the bullet above) — real
  corroborating evidence, not a blocker either way. Architecturally incompatible regardless of
  licensing: no DAG/per-stage
  cache (opposite of Tapetum/#44's design), monolithic module structure (no Claw-style
  extension points), untyped JSON sidecar edit storage (no catalog DB). See
  `docs/research/stalk-prior-art.md` for the full RapidRAW/vkdt/Ansel findings and file:line
  citations.
