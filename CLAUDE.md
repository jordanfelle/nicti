# Nicti

Personal (non-Shutterpaws) Rust RAW photo editor + DAM, replacing Adobe Lightroom Classic. Scrumboy
board: `lightroom-classic-replacement` (display name "Nicti"). See the root `/home/jordan/git/CLAUDE.md`
cross-repo map for how this fits alongside other repos.

## Architecture decisions

- **Implementation language: Rust**, with C FFI bindings to LibRaw (and lensfun if lens-correction
  data is used) — `docs/adr/0001-language-and-stack.md`. Decided over C++/C#/Go/Zig/Swift primarily
  on memory safety and solo+agent (Claude Code) productivity; C++ was close and wins on RAW-decode
  maturity, UI-toolkit production track record, and Windows-tooling depth. Unblocks #14 (GPU
  compute API), #17 (module/plugin architecture, Claw), #68 (GUI framework — its candidate list is
  Rust-only, now a valid constraint).

ADRs live in `docs/adr/`, numbered sequentially.
