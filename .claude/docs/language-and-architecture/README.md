## Language and architecture

Covers the implementation-language decision, v1 platform/camera scope, the non-destructive edit model, and the Claw module/plugin architecture.

- **Implementation language: Rust**, with C FFI bindings to LibRaw (and lensfun if lens-correction
  data is used) — `docs/adr/0001-language-and-stack.md`. Decided over C++/C#/Go/Zig/Swift primarily
  on memory safety and solo+agent (Claude Code) productivity; C++ was close and wins on RAW-decode
  maturity, UI-toolkit production track record, and Windows-tooling depth. Unblocks #16 (GPU
  compute API), #19 (module/plugin architecture, Claw), #68 (GUI framework — its candidate list is
  Rust-only, now a valid constraint).
- **v1 target**: Windows only (macOS/Linux release builds are a v2 concern; Linux stays CI-only
  for now). Nikon NEF only — architecture should stay extensible (decoder/profile/lens/render
  stage/AI-model/exporter/catalog-store as extension points) without hard-coding Nikon
  assumptions, since a wider camera-brand open-source release is a long-term goal.
- **Non-destructive edit model**: `docs/adr/0002-non-destructive-edit-model.md` — catalog DB is
  authoritative; a fixed-order stage-parameter map (not a darktable-style reorderable op stack);
  canonical `serde_json` + `blake3` per-stage hashing feeds Tapetum's (#44) cache key; history is
  an append-only delta log with compaction (a slider-drag burst collapses to one undo step) plus
  never-pruned named snapshots; virtual copies are multiple edit rows per asset; XMP has three
  layers (LRC-convention metadata, a lossless `nicti:` namespace for catalog recovery, and a
  best-effort `crs:` projection for AI masks). Unblocks #22, #52; feeds #44, #59.
- **Module/plugin architecture (Claw)**: `docs/adr/0004-module-plugin-architecture.md` — v1
  first-party modules are in-process Rust traits with a lazy (`OnceLock`-backed) registry so heavy
  modules load on demand. Originally described isolating an LGPL native dependency (e.g. `rawler`/
  `lensfun-rs`) behind a checked C-ABI `cdylib` boundary (`libloading` + an explicit ABI-version
  handshake) specifically to satisfy LGPL's dynamic-linking safe harbor — **that specific reason no
  longer applies for `lensfun-rs`** as of ADR-0003's 2026-09-24 amendment (Nicti's own license is
  now copyleft, and `lensfun-rs`'s confirmed or-later dual license combines in cleanly regardless
  of link type; see the ADR-0013 bullet in `.claude/docs/licensing/README.md`) **but still applies
  for `rawler`**, whose LGPL grant
  isn't confirmed to include an "or later" option — don't drop its isolation/sign-off requirement
  without resolving that first. The `cdylib` boundary mechanism itself is still available and may
  still be worth using for other reasons (plugin flexibility, v2's WASM-plugin direction below). v2
  third-party plugins are directionally WASM (`wasmtime`) for non-hot-path extension points only —
  measured, not assumed, in `crates/nicti-claw/tests/wasm_vs_native.rs` — never for a third-party
  render stage's per-pixel loop, which would need GPU shaders instead. **#20 landed the
  `nicti-claw` + per-domain crate layout** — see CLAUDE.md's Package map section.
