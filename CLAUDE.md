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
history/compaction, and XMP round-trip proof) — not production code; don't build on top of a spike
crate, and expect it to be deleted once its ADR is accepted and #20/#22 land the real crate layout.
Update this section with a real package map (crate → path → role) as soon as the module
architecture is decided and real crates land. `bench/whisker` (a workspace member) is benchmark
tooling for #43, not a production crate either — same "don't build on top of it" caveat applies.

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
cargo test
cargo clippy --all-targets --all-features
cargo fmt --check
```

## CI

GitHub Actions, GitHub-hosted runners (`ubuntu-latest`/`windows-latest`) — this project has no
self-hosted runner infrastructure of its own and Shutterpaws' old self-hosted GitHub Actions
runner host was retired 2026-08-30, so don't copy the `runs-on: [self-hosted, linux]` pattern
from Shutterpaws repos here. See `.github/workflows/ci.yml`. A `cargo-deny` job checks Rust crate
licenses against `deny.toml` (the allowlist from `docs/adr/0003-third-party-license-policy.md`) —
it only covers Cargo dependencies, not native libraries, ML models, or data files, which still
rely on `docs/licensing.md` being updated at review time.
