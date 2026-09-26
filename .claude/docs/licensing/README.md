## Licensing

Covers the third-party license policy (and its 2026-09-24 amendment) and the outbound AGPL-3.0-or-later decision.

- **Third-party license policy**: `docs/adr/0003-third-party-license-policy.md`, backed by the
  full per-dependency/per-model audit in `docs/licensing.md` — Rust crate allowlist, ML-model
  bundle-vs-on-demand-download criteria, and the "no Adobe DCP/LCP data" rule. **Amended
  2026-09-24** (see ADR-0003's own Amendments section, load-bearing not historical): the original
  LGPL-native-lib dynamic-linking rule and the blanket GPL/AGPL denial are both superseded now that
  Nicti's own outbound license is decided (see the ADR-0013 bullet below) — read the amendment,
  not just the original 2026-09-23 Decision text. Update `docs/licensing.md` in the same PR as any
  new dependency or model.
- **Outbound license: AGPL-3.0-or-later** — `docs/adr/0013-outbound-license-agpl.md`, resolving
  #66 (open-source release prep) early because ADR-0003's permissive-only default was already
  actively constraining in-flight decisions. Chosen specifically over plain GPL-3.0 for the
  network-use clause (§13) — closes the "run it as a hosted service, never share the source"
  loophole plain GPL leaves open, which matters given #58 (web gallery/upload) and #64
  (multi-machine catalog) are real planned v2 network-facing features, not hypothetical ones.
  Un-excludes Ultralytics YOLO (culling/detection) and exiv2/rexiv2 (EXIF/XMP/IPTC, though
  kamadak-exif/little_exif remain the current unforced choice) on license grounds; removes the
  LGPL-as-Cargo-dependency sign-off/`cdylib`-isolation requirement **for `lensfun-rs` specifically**
  (its confirmed `LGPL-3.0-or-later OR GPL-3.0` dual license combines cleanly now that Nicti's own
  license is already copyleft) — **but not for `rawler`**, whose bare `license = "LGPL-2.1"` (no
  `-only`/`-or-later` suffix, and no project-specific evidence either way beyond that) could still
  mean GPL-2.0-only if relicensed, which this same amendment denies; #37 still needs to resolve
  that before treating rawler as pre-cleared. Reopens RapidRAW (#69) as a potential adopt/fork
  candidate, not just prior-art study, since it's also AGPL-3.0 — see the new tickets filed
  alongside this ADR for follow-up.
- **RapidRAW adopt/fork question, resolved**: `docs/adr/0018-rapidraw-adopt-or-fork.md` —
  **not adopted, whole or by module; study-only**. Full findings (RapidRAW, vkdt, Ansel; file:line
  cited architecture and license checks) in `docs/research/stalk-prior-art.md`. The decisive
  licensing result: running Nicti's own `deny.toml` against RapidRAW's real resolved dependency
  graph (`cargo deny check licenses`, 583 unique crates) produced exactly one rejection — `rawler
  v0.7.1`'s bare `LGPL-2.1` (no `-or-later`), the same open question the ADR-0013 bullet above
  already flagged; RapidRAW's own fork of it (`RapidRAW-DngLab`) doesn't resolve the ambiguity,
  it only patches a highlights-clamping behavior with the license headers untouched. Every other
  crate in the graph clears Nicti's existing allowlist with zero additions needed — real, useful
  corroborating evidence for #37 either way, independent of the adopt/fork question. The adopt/fork
  question itself is answered no regardless of licensing: a per-Nicti-ADR compatibility table shows
  RapidRAW's actual architecture conflicts with ADR-0002 (untyped JSON sidecar edit storage, no
  catalog DB), ADR-0004 (monolithic free functions, no Claw-style extension points), and — the most
  consequential finding — has **no DAG or per-stage render cache at all**, confirmed absent: every
  slider tweak re-runs the entire GPU pipeline top-to-bottom through one 1,997-line monolithic WGSL
  shader, the architectural opposite of Tapetum's (#44) planned stage-cached design. RapidRAW
  remains a valuable reference implementation to read while building #37/#44/#48 (real, shipping
  SAM-ViT-B/LaMa/Depth-Anything-V2/U-2-Net/CLIP integrations, all downloaded on demand rather than
  bundled), just not code to fork or a dependency to add. Also corrected two prior-art citations
  found wrong during this research: ADR-0006 had claimed RapidRAW uses egui/eframe (it's actually
  Tauri+React) and ADR-0005's wgpu note now reflects RapidRAW's real wgpu-29 pin — see those ADRs'
  own Amendments/Prior-art sections in `.claude/docs/gpu-gui-and-healing/README.md`'s topic.
