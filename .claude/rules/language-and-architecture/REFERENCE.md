---
paths:
  - "crates/nicti-claw/**"
  - "crates/dewclaw/**"
  - "crates/nicti-pawprint/**"
  - "src/**"
  - "Cargo.toml"
---

# Language and Architecture — Quick Reference

Full reasoning/history: `docs/decisions/language-and-architecture.md`.

- **Language: Rust**, C FFI to LibRaw (+lensfun) — `docs/adr/0015`. Chosen over C++/C#/Go/Zig/Swift
  on memory safety + agent-assisted-development productivity.
- **v1 target**: Windows only (macOS/Linux is v2, Linux stays CI-only). Nikon NEF only, but keep
  decoder/profile/lens/render/AI-model/exporter/catalog-store as extension points — wider
  camera-brand support is a long-term goal, don't hard-code Nikon assumptions.
- **Non-destructive edit model** — `docs/adr/0021`: catalog DB authoritative, fixed-order
  stage-parameter map (not reorderable op stack), `serde_json`+`blake3` per-stage hashing feeds
  Tapetum's (#44) cache key, append-only delta log + compaction + never-pruned snapshots, virtual
  copies = multiple edit rows, three XMP layers (LRC-convention, lossless `nicti:`, best-effort
  `crs:` for AI masks).
- **Module/plugin architecture (Claw)** — `docs/adr/0019`: v1 first-party modules are in-process
  Rust traits, lazy `OnceLock` registry. `cdylib` C-ABI isolation (`libloading` + ABI-version
  handshake) still required for `rawler` (LGPL, no confirmed "or-later") but no longer for
  `lensfun-rs` (see licensing topic, ADR-0066 amendment). v2 third-party plugins are WASM
  (`wasmtime`) for non-hot-path extension points only, never a per-pixel render stage.

## Package contents

- **`crates/nicti-claw`** — the `Module` trait (identity/versioning shared by every extension
  point), the lazy `OnceLock`-backed `Registry` (`registry.rs`), and the checked C-ABI dylib
  handshake (`dylib.rs`) — the load-bearing crate every other `nicti-*` crate builds on.
  Generalized from the now-deleted `spikes/sheath` spike.
- **`crates/dewclaw`** — test fixture (cdylib) for `nicti-claw`'s dylib tests, generalized from the
  now-deleted `spikes/dewclaw`.
- **`crates/nicti-pawprint`** (#21/#44/#45/ADR-0021, landed) — `EditDocument`/`StageEntry`
  (`lib.rs`), canonical-JSON + blake3 stage hashing generalized to Tapetum's DAG (`canonical.rs`,
  merges `spikes/pawprint`'s original one-upstream `hash_stage`/`cache_key` with
  `spikes/loaf/src/hash.rs`'s DAG-generalized `chain`), and append-only edit history with
  slider-drag compaction (`history.rs`) — promoted from `spikes/pawprint` (now deleted). Its XMP
  round-trip proof stayed with #59/`spikes/scent` rather than being promoted here.
