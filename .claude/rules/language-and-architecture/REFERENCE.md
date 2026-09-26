---
paths:
  - "crates/nicti-claw/**"
  - "crates/dewclaw/**"
  - "spikes/pawprint/**"
  - "src/**"
  - "Cargo.toml"
---

# Language and Architecture — Quick Reference

Full reasoning/history: `docs/decisions/language-and-architecture.md`.

- **Language: Rust**, C FFI to LibRaw (+lensfun) — `docs/adr/0001`. Chosen over C++/C#/Go/Zig/Swift
  on memory safety + agent-assisted-development productivity.
- **v1 target**: Windows only (macOS/Linux is v2, Linux stays CI-only). Nikon NEF only, but keep
  decoder/profile/lens/render/AI-model/exporter/catalog-store as extension points — wider
  camera-brand support is a long-term goal, don't hard-code Nikon assumptions.
- **Non-destructive edit model** — `docs/adr/0002`: catalog DB authoritative, fixed-order
  stage-parameter map (not reorderable op stack), `serde_json`+`blake3` per-stage hashing feeds
  Tapetum's (#44) cache key, append-only delta log + compaction + never-pruned snapshots, virtual
  copies = multiple edit rows, three XMP layers (LRC-convention, lossless `nicti:`, best-effort
  `crs:` for AI masks).
- **Module/plugin architecture (Claw)** — `docs/adr/0004`: v1 first-party modules are in-process
  Rust traits, lazy `OnceLock` registry. `cdylib` C-ABI isolation (`libloading` + ABI-version
  handshake) still required for `rawler` (LGPL, no confirmed "or-later") but no longer for
  `lensfun-rs` (see licensing topic, ADR-0013 amendment). v2 third-party plugins are WASM
  (`wasmtime`) for non-hot-path extension points only, never a per-pixel render stage.
