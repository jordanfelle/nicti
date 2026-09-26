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
  resolve before treating it as pre-cleared). **#37 found LibRaw itself has the identical
  ambiguity** (its own per-file header is a bare "version 2.1" too) — its CDDL-1.0 arm is
  unambiguous but GPL-incompatible, so it can't substitute; see `raw-decoder` topic. Reopens
  RapidRAW (#69) as an adopt/fork candidate — but **#37 found RapidRAW doesn't actually decode
  HE/HE\* either** (its own rawler fork still rejects it, falling back to the embedded JPEG), so
  it isn't a shortcut past #37's own decoder work.
