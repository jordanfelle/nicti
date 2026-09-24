# Nicti

Personal Rust RAW photo editor + DAM, replacing Adobe Lightroom Classic. Public repo:
`github.com/jordanfelle/nicti`. Not a Shutterpaws project.

## Architecture decisions

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
- **Third-party license policy**: `docs/adr/0003-third-party-license-policy.md`, backed by the
  full per-dependency/per-model audit in `docs/licensing.md` — Rust crate allowlist, LGPL-native-lib
  dynamic-linking rule, ML-model bundle-vs-on-demand-download criteria, and the "no Adobe
  DCP/LCP data" rule. Update `docs/licensing.md` in the same PR as any new dependency or model.
  Unblocks #66 (open-source release prep).
- **Module/plugin architecture (Claw)**: `docs/adr/0004-module-plugin-architecture.md` — v1
  first-party modules are in-process Rust traits with a lazy (`OnceLock`-backed) registry so heavy
  modules load on demand; an LGPL native dependency (e.g. a future `rawler`/`lensfun-rs`) is
  isolated behind a checked C-ABI `cdylib` boundary (`libloading` + an explicit ABI-version
  handshake) rather than statically linked in. v2 third-party plugins are directionally WASM
  (`wasmtime`) for non-hot-path extension points only — measured, not assumed, in
  `spikes/sheath/tests/wasm_vs_native.rs` — never for a third-party render stage's per-pixel loop,
  which would need GPU shaders instead. Proposes the `nicti-claw` + per-domain crate layout for
  #20. Unblocks #20; feeds #37/#39's LGPL isolation requirement from ADR-0003.
- **GPU compute API**: `docs/adr/0005-gpu-compute-api.md` — `wgpu` (WGSL), Vulkan backend on
  Windows (not Dx12 — Dx12 doesn't expose `SHADER_F16` on wgpu 30/current driver, Vulkan does, and
  Tapetum's cache tiers need f16). Measured on the reference RTX 5080 in `spikes/glint/`: live-stage
  chain at 4K and 45MP both clear the decision rule (within 2x of an equivalent CUDA kernel, one
  wgpu backend actually faster); `max_buffer_size`/`max_storage_buffer_binding_size` clear the 45MP
  RGBA16F (~360MB) hero-frame size by 5–10x on real hardware. Found and fixed a real
  dispatch-dimensioning bug along the way: a naive 1D dispatch overflows wgpu's 65535-per-dimension
  workgroup limit at hero-scenario resolution — any future wgpu compute-stage code must dispatch as
  a 2D grid (`gpu.rs::workgroup_grid`), not assume 1D is safe. Never do a full-frame host↔device
  round-trip in the hot path (confirmed expensive, 0.8–1.5s, by the spike's own harness) — baked
  stage output must stay GPU-resident, per ADR-0002/#44. Unblocks #20, #41, #45; feeds #68 (GUI
  framework)'s wgpu-interop question.
- **GUI framework**: `docs/adr/0006-gui-framework.md` — **Proposed, pending a reference-machine
  measurement pass**; the hard-gate findings are final. GPUI is eliminated outright: its Windows
  backend is a bespoke Direct3D11 renderer (`windows-rs`), with no `wgpu`/Vulkan path in its own
  dependency graph on that platform at all (`blade-graphics` is Linux/macOS-only) — no
  `spikes/pelt-gpui` was built. Of the other three, egui (via eframe) currently leads: the only
  candidate whose own `wgpu` dependency (30.0.0) matches ADR-0005's choice exactly, with a clean
  MIT/Apache-2.0 license and a mature `egui_wgpu::CallbackTrait` custom-viewport story. Iced works
  but pins `wgpu` 27, not 30 (a real version-compatibility cost). Slint's GPU-resident
  `Image::try_from(wgpu::Texture)` integration is the cleanest of the three mechanically, but its
  own license (`GPL-3.0-only OR LicenseRef-Slint-*`) only passes today under a spike-scoped
  `deny.toml` exception — shipping it needs its own ADR-0003 amendment. Neither Iced nor Slint has
  egui's/GPUI's built-in virtualized-list primitive, so both had to hand-roll grid-windowing math
  (`spikes/pelt/src/virtualize.rs`) for #68's grid gate. Final selection waits on
  `bench/pelt/pelt.ahk`+`run-pelt.ps1` numbers from the reference machine.

ADRs live in `docs/adr/`, numbered sequentially.

## Performance targets and benchmarking

- **Performance targets + benchmark methodology**: `docs/benchmarks.md` — p95 targets per feature
  area, warm/cold measurement rules, and the `ref-10k` frozen reference dataset (manifest at
  `docs/ref-10k-manifest.csv`). Finalized 2026-09-23 (#14). Every render-engine/perf-sensitive
  ticket (#43, #17, #40, etc.) measures against this.
- **Hero-scenario benchmark (#43)**: spec at `docs/benchmarks/hero-scenario.md`. Tooling under
  `bench/`: `bench/select_hero_set.py` (deterministic 50-file working-set selection),
  `bench/lrc/` (AutoHotkey v2 driver + catalog setup for LRC), `bench/run-hero.ps1` (capture +
  orchestration), `bench/whisker/` (Rust frame-diff analyzer, workspace member — not production
  code, see its own `Cargo.toml` description).

## Naming convention: feline references

Name new crates, modules, internal tools, and subsystems with a feline-anatomy/behavior angle
rather than a purely descriptive name — the project itself is named after the nictitating
membrane (a cat's third eyelid), and that theme continues throughout. Examples already assigned
for planned subsystems: `Tapetum` (stage-cached render graph — the tapetum lucidum bounces light
back through the retina for reuse, mapping to reusing baked stage output), `Claw` (on-demand
module/plugin registry — claws stay sheathed until needed), `Pounce` (job scheduler with priority
preemption), `Sniff` (embedded-JPEG fast preview path for culling).

## Package map

No production crates exist yet — this repo is still in bootstrap/scaffolding. A root placeholder
binary crate (`src/main.rs`) exists only so CI/lint tooling has something real to run against; it
is not a commitment to final crate layout. `spikes/*` (a Cargo workspace member glob) holds
throwaway research spikes — e.g. `spikes/pawprint` (#21/ADR-0002's edit-document hashing,
history/compaction, and XMP round-trip proof), `spikes/sheath` + fixture `spikes/dewclaw`
(#19/ADR-0004's lazy module registry, checked C-ABI dylib boundary, and WASM-vs-native pixel-kernel
timing), `spikes/glint` (#16/ADR-0005's wgpu-vs-CUDA measured comparison — correctness,
feature/limit availability, throughput, dispatch overhead, host↔device interop cost), and
`spikes/pelt` + `spikes/pelt-egui`/`spikes/pelt-iced`/`spikes/pelt-slint` (#68/ADR-0006's
GUI-framework research — `pelt` is the toolkit-agnostic shared fixture/math crate, each `pelt-*`
is one candidate's virtualized-grid + loupe + custom-wgpu-viewport spike; no `spikes/pelt-gpui`
exists, see ADR-0006's Hard-gate-1 early exit) — not
production code; don't build on top of a spike crate, and expect all of these to be deleted once
#20 lands the real crate layout. **Proposed real package map (from ADR-0004, pending #20):**
`nicti-claw` (module registry + dylib loader — generalizes `spikes/sheath`), then one crate per
domain implementing its traits: `nicti-decode`, `nicti-color`, `nicti-lens`, `nicti-render`
(Tapetum's future home, #44), `nicti-ai`, `nicti-export`, `nicti-catalog` (#22's future home).
`bench/whisker` (a workspace member) is benchmark tooling for #43, not a production crate either —
same "don't build on top of it" caveat applies.

## Development workflow

**Always pull main before starting any work:**
```bash
git checkout main && git pull origin main
```

**Use worktrees for feature branches** — never work directly on the main checkout:
```bash
git worktree add ../nicti-wt-myfeature -b feat/myfeature
```

Compile-feedback loop: `cargo check`, not `cargo build` — skips codegen/linking. Full
`cargo build`/`cargo test` only when the binary or test execution is actually needed. See
`~/.claude/rules/rust-workflow/REFERENCE.md` for the fuller set of Rust-specific efficiency rules
(lower Gemini-routing threshold, targeted `cargo clippy`, the `LSP` tool over grep+full-file reads,
`cargo watch -x check` for long edit loops).

## PR conventions

- **No issue tracker integration yet.** This project isn't tracked on any external tracker —
  plain GitHub Issues/PRs only, with the intent to formalize on GitHub Issues once the project is
  past its first phase. Don't add ticket-reference conventions, ticket-ID trailers, or
  cross-tool links until that happens; a bare `#N` in a commit/PR/ADR here means a GitHub
  issue/PR in this repo.
- **PR titles**: a plain summary sentence.
- **This is a public repo — never include a `Claude-Session:` trailer or a "Generated with Claude
  Code" footer** in commit messages or PR descriptions here. `Co-Authored-By:` is fine to keep;
  strip the session-link trailer and the generated-with footer entirely. A session link on a
  public repo exposes the conversation transcript to anyone who reads the commit/PR.

## Adversarial review before opening any PR

Anything substantial goes through an adversarial review loop before it's called done — review →
verify each finding → fix → re-review if the fixes were non-trivial. Small, low-risk changes may
skip it (a copy tweak, a comment, a version bump, a one-line config edit).

**Run this BEFORE opening the PR, not after.** Spawn a fresh agent (no explicit `model` override
— see the account-wide Fable rule in `~/.claude/CLAUDE.md`) pointed at the branch's diff, prompted
hostilely: assume the author was overconfident, name concrete areas to attack, require a
CONFIRMED/SPECULATIVE split with a failing scenario per finding. Verify every finding yourself
before acting on it, and when a finding names one instance of a pattern, grep for its siblings
instead of fixing only the one named.

**Post the outcome as a PR comment before merge**, not just the local pass/fix cycle: state what
ran, the CONFIRMED/SPECULATIVE split (or "no findings"), and how any real finding was resolved.

## Testing

```bash
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features
cargo fmt --check
```

**Always pass `--workspace`** for `test`/`clippy` in this repo: the root `Cargo.toml` is both the
workspace root and a real package (`nicti`), not a virtual manifest, so a bare `cargo test`/`cargo
clippy` without `-p`/`--workspace` silently checks only the root crate and skips `spikes/*` and
`bench/whisker` entirely — confirmed as a real gap (CI's own `clippy`/`test` jobs had been doing
exactly this since `spikes/pawprint` landed, until fixed alongside #19/ADR-0004).

## CI

GitHub Actions, GitHub-hosted runners (`ubuntu-latest`/`windows-latest`) — this project has no
self-hosted runner infrastructure of its own and Shutterpaws' old self-hosted GitHub Actions
runner host was retired 2026-08-30, so don't copy the `runs-on: [self-hosted, linux]` pattern
from Shutterpaws repos here. See `.github/workflows/ci.yml`. A `cargo-deny` job checks Rust crate
licenses against `deny.toml` (the allowlist from `docs/adr/0003-third-party-license-policy.md`) —
it only covers Cargo dependencies, not native libraries, ML models, or data files, which still
rely on `docs/licensing.md` being updated at review time.
