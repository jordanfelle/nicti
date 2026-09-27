## Language and architecture

Covers the implementation-language decision, v1 platform/camera scope, the non-destructive edit model, and the Claw module/plugin architecture.

- **Implementation language: Rust**, with C FFI bindings to LibRaw (and lensfun if lens-correction
  data is used) — `docs/adr/0015-language-and-stack.md`. Decided over C++/C#/Go/Zig/Swift primarily
  on memory safety and agent-assisted-development productivity; C++ was close and wins on RAW-decode
  maturity, UI-toolkit production track record, and Windows-tooling depth. Unblocks #16 (GPU
  compute API), #19 (module/plugin architecture, Claw), #68 (GUI framework — its candidate list is
  Rust-only, now a valid constraint).
- **v1 target**: Windows only (macOS/Linux release builds are a v2 concern; Linux stays CI-only
  for now). Nikon NEF only — architecture should stay extensible (decoder/profile/lens/render
  stage/AI-model/exporter/catalog-store as extension points) without hard-coding Nikon
  assumptions, since a wider camera-brand open-source release is a long-term goal.
- **Non-destructive edit model**: `docs/adr/0021-non-destructive-edit-model.md` — catalog DB is
  authoritative; a fixed-order stage-parameter map (not a darktable-style reorderable op stack);
  canonical `serde_json` + `blake3` per-stage hashing feeds Tapetum's (#44) cache key; history is
  an append-only delta log with compaction (a slider-drag burst collapses to one undo step) plus
  never-pruned named snapshots; virtual copies are multiple edit rows per asset; XMP has three
  layers (LRC-convention metadata, a lossless `nicti:` namespace for catalog recovery, and a
  best-effort `crs:` projection for AI masks). Unblocks #22, #52; feeds #44, #59.
- **Module/plugin architecture (Claw)**: `docs/adr/0019-module-plugin-architecture.md` — v1
  first-party modules are in-process Rust traits with a lazy (`OnceLock`-backed) registry so heavy
  modules load on demand. Originally described isolating an LGPL native dependency (e.g. `rawler`/
  `lensfun-rs`) behind a checked C-ABI `cdylib` boundary (`libloading` + an explicit ABI-version
  handshake) specifically to satisfy LGPL's dynamic-linking safe harbor — **that specific reason no
  longer applies for `lensfun-rs`** as of ADR-0018's 2026-09-24 amendment (Nicti's own license is
  now copyleft, and `lensfun-rs`'s confirmed or-later dual license combines in cleanly regardless
  of link type; see the ADR-0066 bullet in `docs/decisions/licensing.md`) **and no longer applies
  to `rawler` under LGPL-2.1 §§5-6 either** (#37's correction: LGPL-2.1 §§5-6 already permit
  combining an LGPL library into a differently-licensed larger work with no relicensing at all,
  regardless of an "or later" grant — see the licensing topic's REFERENCE.md). The remaining
  requirement is a distribution-mechanics + notice checklist to verify per release, not an
  isolation blocker. The `cdylib` boundary mechanism itself is still available and may
  still be worth using for other reasons (plugin flexibility, v2's WASM-plugin direction below). v2
  third-party plugins are directionally WASM (`wasmtime`) for non-hot-path extension points only —
  measured, not assumed, in `crates/nicti-claw/tests/wasm_vs_native.rs` — never for a third-party
  render stage's per-pixel loop, which would need GPU shaders instead. **#20 landed the
  `nicti-claw` + per-domain crate layout** — see CLAUDE.md's Package map section.
- **AI modules are local-only by default** — `docs/adr/0218-local-only-ai.md`: any `ModelProvider`
  (`crates/nicti-stalk`) performing batch culling or edit-suggestion inference/training must not
  telemetry or call a hosted API by default. Fetching model weights over the network is allowed
  only as an explicit, checksum-verified user action, never a silent auto-fetch; once installed,
  inference itself stays offline. A cloud-AI feature must be a separate, clearly distinct feature
  behind a guided opt-in, not a settings toggle. Applies to third-party Claw modules too — #214's
  manifest/consent/kill-switch research is the enforcement mechanism there. Feeds #49, #51, #53,
  #34, #35, #36.
- **Claw v2 plugin manifest, disclosure UX, and kill switch (Collar/Hiss)** —
  `docs/adr/0214-claw-plugin-manifest-and-kill-switch.md`: a third-party module manifest
  (`collar.toml`) declares every capability (network/filesystem/gpu/extension-point) with a scope
  and a required plain-language justification, shown at install; an update widening a capability
  disables the module until re-consented. Network access is action-scoped
  (`weight-download`/`cloud-feature` only, per ADR-0218) and enforced by not linking the
  corresponding WASI import (`wasi:http`'s `outgoing-handler`) unless granted — an undeclared
  capability is structurally unreachable, not just policy-denied. A kill switch (menu item +
  candidate `Ctrl+Shift+Escape` shortcut, pending a manual LRC-collision check) uses wasmtime
  epoch interruption to stop an in-flight call immediately, including a tight compute loop with no
  host-call boundary, and quarantines the module; a persisted pre-launch safe-mode flag (Obsidian
  Restricted Mode precedent) is the fallback if the UI itself is compromised. Distinguishes a soft
  "bug" (auto-disable + notice, one-click re-enable) from a hard "violation" (attempted undeclared
  capability use — immediate quarantine, explicit re-enable required). Documentary only — no
  sandbox implementation; a follow-up `build`/v2 ticket implements it. Feeds ADR-0218 §4's
  enforcement requirement.
