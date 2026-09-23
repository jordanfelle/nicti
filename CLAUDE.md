# Nicti

Personal Rust RAW photo editor + DAM, replacing Adobe Lightroom Classic.

## Architecture decisions

- **Implementation language: Rust**, with C FFI bindings to LibRaw (and lensfun if lens-correction
  data is used) — `docs/adr/0001-language-and-stack.md`. Decided over C++/C#/Go/Zig/Swift primarily
  on memory safety and solo+agent (Claude Code) productivity; C++ was close and wins on RAW-decode
  maturity, UI-toolkit production track record, and Windows-tooling depth. Unblocks #14 (GPU
  compute API), #17 (module/plugin architecture, Claw), #68 (GUI framework — its candidate list is
  Rust-only, now a valid constraint).

ADRs live in `docs/adr/`, numbered sequentially.

## Performance targets and benchmarking

- **Performance targets + benchmark methodology**: `docs/benchmarks.md` — p95 targets per feature
  area, warm/cold measurement rules, and the `ref-10k` frozen reference dataset (manifest at
  `docs/ref-10k-manifest.csv`). Finalized 2026-09-23 (#11). Every render-engine/perf-sensitive
  ticket (#43, #15, #40, etc.) measures against this.
