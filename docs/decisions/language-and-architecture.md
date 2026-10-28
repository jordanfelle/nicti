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
  `nicti-claw` + per-domain crate layout** — see `.claude/docs/package-map/REFERENCE.md` (the Package map).
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
  and a required plain-language justification, shown at install; an update widening a capability,
  down to individual fields like an added `allowed_hosts` entry, disables the module until
  re-consented. Network access is action-scoped (`weight-download`/`cloud-feature` only, per
  ADR-0218), HTTPS-only, and enforced by not linking the corresponding WASI import (`wasi:http`'s
  `outgoing-handler`) unless granted — an undeclared capability is structurally unreachable, not
  just policy-denied. A kill switch (menu item + candidate `Ctrl+Shift+Escape` shortcut, pending a
  manual LRC-collision check) uses wasmtime epoch interruption to stop an in-flight call, including
  a tight compute loop with no host-call boundary, once the target module's own epoch deadline is
  crossed (`increment_epoch()` itself advances a counter shared by the whole `Engine` — isolating
  one module needs a separate `Engine` or carefully spaced per-`Store` deadlines), and quarantines
  the module; a persisted pre-launch safe-mode flag (Obsidian Restricted Mode precedent) is the
  fallback if the UI itself is compromised. Not yet solved: epoch interruption doesn't stop GPU
  work already submitted to the device. Distinguishes a soft
  "bug" (auto-disable + notice, one-click re-enable) from a hard "violation" (attempted undeclared
  capability use — immediate quarantine, explicit re-enable required). Documentary only — no
  sandbox implementation; a follow-up `build`/v2 ticket implements it. Feeds ADR-0218 §4's
  enforcement requirement.

## #230: Collar/Hiss implementation slice (ADR-0214)

`crates/nicti-collar` (layer 0, its own crate so `nicti-claw` does not take wasmtime) implements the parts of ADR-0214 that
need no plugin host. **Manifest** (`manifest::parse`): strict TOML, unknown fields refused, a justification and a scope
required on every capability, `RenderStage` rejected, hosts canonical lowercase DNS names (no wildcard, IP literal, port,
scheme, trailing dot or non-ASCII), one capability per (kind, scope), budgets only tighten the host ceilings, `nicti.`
ids reserved. **Re-consent** (`consent::widenings`): field-level, so a new host in an existing network capability, a new
filesystem scope, `gpu`, a new extension point or a looser budget (including dropping an override) all widen; removals and
reworded justifications never do. **Authorizers** (`authorize`): a request must be `https://`, port 443, a canonical host
exactly in `allowed_hosts` (no userinfo, escapes, brackets, trailing dot or subdomain implication) and in the *active*
action scope; a path is judged by canonical containment under the one granted directory (symlinks, `..`, a sibling with a
shared prefix, a not-yet-existing file via its parent). **Hiss** (`hiss`): states NeedsConsent / Enabled / AutoDisabled /
Killed / Quarantined; unknown and never-consented modules do not run; N bugs in a window auto-disable with one-click
re-enable; a violation quarantines at once and only an exact acknowledgement of what it attempted re-enables it (no
update, consent or kill clears it); state persists atomically, a corrupt file is an *error* (fail closed to safe mode, never
an empty registry that forgets a quarantine); the safe-mode marker file reads as ON when it cannot be checked.
**Sandbox** (`sandbox`): an `Engine` per module with fuel, an epoch deadline, a wall-clock watchdog and a memory cap;
`Killer::kill` interrupts a tight loop with no host call (verified: ADR-0214's claim holds on wasmtime 50), a module's own
timeout is a Bug not a Kill, killing one module never interrupts another, and any import is a Violation (nothing is linked
yet). Surviving equivalent mutants: the explicit `@` ban duplicates the canonical-host check, and the post-deadline kill
flag check duplicates the one at the top of `run` (the window between them is not deterministically testable).
Follow-ups: WASI linking per capability, the consent screen and kill-switch UI/shortcut (the Adobe binding cross-check is
still open), lazy requests, a host-call timeout and a GPU cancellation path.
