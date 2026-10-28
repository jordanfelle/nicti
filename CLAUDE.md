# Nicti

Rust RAW photo editor + DAM, aiming to replace Adobe Lightroom Classic. Public repo:
`github.com/jordanfelle/nicti`. This file is Claude Code-specific guidance; **`CONTRIBUTING.md`
is the canonical human contributor guide** — read that first if you're new here.

## Architecture decisions

ADRs live in `docs/adr/` (see `docs/adr/README.md` for the numbering convention, index, and
template) -- numbered by the GitHub issue that prompted it, not sequentially. Per-ADR decisions,
measured results, and gotchas live in
`.claude/rules/<topic>/REFERENCE.md` + `docs/decisions/<topic>.md`, not inline here, to keep this
file under the line-count gate. Each topic has:

- `.claude/rules/<topic>/REFERENCE.md` — terse key:value/bullet compression, the actionable fact +
  a pointer, scoped with `paths:` frontmatter so Claude Code only auto-loads it when touching a
  matching file (unscoped `.claude/rules/**` files load unconditionally every session, which is why
  this repo scopes them).
- `docs/decisions/<topic>.md` — the full original prose, verbatim, with every issue ref and piece
  of reasoning. Not auto-loaded by Claude Code, but a normal repo doc any contributor can read.

Topics: `language-and-architecture` (0015/0021/0019/0218 v1 target, 0214 v2-only), `licensing` (0018/0066, 0069),
`gpu-gui-and-healing` (0016/0068/0050/0051, 0434, 0437), `catalog-engine` (0067/0102/0106/0103/0107, 0113/0115/0116, 0025, 0026, 0436, 0441),
`preview-tiers` (0029, 0143, 0072, 0145), `raw-decoder` (0037, 0438), `volume-identity` (0071, 0024), `color` (0038, 0042, 0381),
`lrc-migration` (0061, 0062, 0156, 0158, 0380, 0381, 0432, 0435), `masking` (0048, 0049, 0353), `culling` (0032, 0033, 0034, 0035, 0108, 0439), `denoise` (0040),
`xmp-interop` (0059), `render-graph` (0044, 0047, 0380, 0381, 0410, 0427, 0428, 0432), `develop` (0099, 0053, 0101, 0052, 0440), `jobs` (0054), `export` (0056, 0057),
`release` (0249). A new ADR adds a
bullet to both files of its topic (or a new topic) and to this list — not inline here.

## Performance targets and benchmarking

- **Performance targets + benchmark methodology**: `docs/benchmarks.md` — p95 targets per feature
  area, warm/cold measurement rules, and the `ref-10k` frozen reference dataset (manifest at
  `docs/ref-10k-manifest.csv`). Finalized 2026-09-23 (#14). Every render-engine/perf-sensitive
  ticket (#43, #17, #40, etc.) measures against this.
- **Hero-scenario benchmark (#43)**: spec at `docs/benchmarks/hero-scenario.md`, including
  interaction D's mixed-operation-sequence cross-regression check (#100) — see that doc's
  "D. Mixed sequence" section and `bench/whisker/README.md`'s "Interaction D" section for how the
  analyzer attributes flashes via a capture's `events.csv` sidecar. Tooling under
  `bench/`: `bench/select_hero_set.py` (deterministic 50-file working-set selection),
  `bench/lrc/` (AutoHotkey v2 driver + catalog setup for LRC — `hero.ahk` drives one timed pass,
  `navigate.ahk` positions the selection before a capture starts), `bench/run-hero.ps1` (one
  capture + orchestration), `bench/run-hero-series.ps1` (a full warm-up + 5-measured-run series,
  including crop/zoom's 5-image spread — this is what you actually invoke), `bench/whisker/`
  (Rust frame-diff analyzer, workspace member — not production code, see its own `Cargo.toml`
  description; its `analyze` subcommand pools a full results tree into per-config/interaction
  p50/p95/max). Requires PowerShell 7+, see `bench/lrc/README.md`.
- **Reusable harness (#17): `crates/nicti-prowl`** — a real production crate (not `bench/`-style
  tooling), since its manifest-verify/perf-protocol pieces are meant to be depended on by future
  research tickets' own exit criteria, not just the hero scenario. `manifest.rs`
  (`Manifest::load`/`verify`, checks a ref-10k copy's SHA-256 against `docs/ref-10k-manifest.csv`
  — `bench/run-hero.ps1` calls this via the `prowl` binary instead of its own inline
  `Get-FileHash` loop), `refset.rs` (`select` — general-purpose bucket-based picking, deliberately
  *not* a Rust port of `bench/select_hero_set.py`'s stratified sampler; `hero_set` reads the
  already-frozen `docs/benchmarks/hero-set.txt` instead), `perf.rs` (`Protocol` — the 1-warmup +
  5-measured-run/p50-p95-max protocol from `docs/benchmarks.md`, `run_verified` refuses to run
  against an unverified ref-10k copy), `golden.rs` (`GoldenStore`/`Render` trait — golden-image
  compare/store, hand-rolled single-scale SSIM rather than `dssim-core`, whose own published
  license string doesn't cleanly match an allowed SPDX id in `deny.toml`; tested only against
  synthetic images since no real NEF→render path exists yet, see the follow-up issue filed
  alongside #17 for wiring in real goldens once #41 lands). `prowl` (the bin target) exposes
  `verify`/`select`/`materialize` (#136, rebuilds a ref-10k from a live library by sha256) for PowerShell/CI callers.

## Naming convention: feline references

Name new crates, modules, internal tools, and subsystems with a feline-anatomy/behavior angle
rather than a purely descriptive name — the project itself is named after the nictitating
membrane, a cat's third eyelid that sweeps across the eye to clear debris without losing vision,
mirroring an editor that's non-destructive: it clears and reprocesses without ever losing the
original image data. That theme continues throughout. Examples already assigned
for planned subsystems: `Tapetum` (stage-cached render graph — the tapetum lucidum bounces light
back through the retina for reuse, mapping to reusing baked stage output), `Claw` (on-demand
module/plugin registry — claws stay sheathed until needed), `Pounce` (job scheduler with priority
preemption), `Sniff` (embedded-JPEG fast preview path for culling), `Scruff` (import/ingest
pipeline — the way a mother cat carries a kitten by the scruff of its neck is how a file gets
moved into the catalog).

## Package map

`crates/*` (workspace member glob) is the production crate layout from ADR-0019 §8; `spikes/*` are throwaway research spikes not yet promoted (do not build on one; each is deleted once its ticket promotes it). The root `src/main.rs` is only a placeholder so CI/lint have something to run. New `crates/*` members must be added to the `LAYERS` table in `scripts/check-repo-policy.py` (CI `repo-policy` job; also enforces GUI-crate containment and `assets/ATTRIBUTION.md` rows for fonts/images/models). Per-crate/per-spike module breakdowns live in each topic's `.claude/rules/<topic>/REFERENCE.md` ("Package contents"); the full crate/topic index is `.claude/docs/package-map/REFERENCE.md` (~22KB, NOT auto-loaded: `grep -n "<crate or topic>" .claude/docs/package-map/REFERENCE.md`).

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
`cargo build`/`cargo test` only when the binary or test execution is actually needed. (This
section's efficiency rules are agent-specific; a human contributor doesn't need them — see
CONTRIBUTING.md instead.)

## Issue lifecycle — assign + label the moment work starts

The instant a worktree/branch is created for a GitHub issue — before the first edit, not after —
run, for that issue number `N`:

```bash
gh issue edit N --repo jordanfelle/nicti --add-assignee jordanfelle --add-label in-progress
```

(`in-progress` is a real label in this repo, not a placeholder — create it with `gh label create`
if it's ever missing.) When the PR merges, `gh issue close N` and drop the `in-progress` label in
the same turn as the merge — don't leave it dangling on a closed issue.

This is this repo's equivalent of the Shutterpaws/Scrumboy board-sync rule (see the launch-root
`~/git/CLAUDE.md`'s ticket-lifecycle rule) — same reasoning, adapted to plain GitHub Issues
instead of a Scrumboy board: an issue sitting unassigned and unlabeled while a branch is actively
open on it is invisible to anyone (including a future session) checking what's already spoken
for. Missed once on #38 (2026-09-26) — the worktree and PR were created without this step.

## PR conventions

- Plain GitHub Issues/PRs — a bare `#N` in a commit/PR/ADR/issue body means a GitHub issue/PR in
  this repo. See CONTRIBUTING.md's Issue conventions section for the `**Part of:**`/
  `**Blocked by:**` link format.
- **PR titles**: a plain summary sentence.
- **This is a public repo — never include a `Claude-Session:` trailer or a "Generated with Claude
  Code" footer** in commit messages or PR descriptions here. `Co-Authored-By:` is fine to keep;
  strip the session-link trailer and the generated-with footer entirely. A session link on a
  public repo exposes the conversation transcript to anyone who reads the commit/PR.

## Adversarial review before opening any PR

Anything substantial goes through an adversarial review loop before it's called done — review →
verify each finding → fix → re-review if the fixes were non-trivial. Small, low-risk changes may
skip it (a copy tweak, a comment, a version bump, a one-line config edit).

**Run this BEFORE opening the PR, not after.** Spawn a fresh agent (no explicit `model` override)
pointed at the branch's diff, prompted hostilely: assume the author was overconfident, name
concrete areas to attack, require a CONFIRMED/SPECULATIVE split with a failing scenario per
finding. Verify every finding yourself before acting on it, and when a finding names one instance
of a pattern, grep for its siblings instead of fixing only the one named.

**Post the outcome as a PR comment before merge**, not just the local pass/fix cycle: state what
ran, the CONFIRMED/SPECULATIVE split (or "no findings"), and how any real finding was resolved.

## Testing

See CONTRIBUTING.md's "Building, testing, linting" section for the exact commands (they mirror
CI's exclude flag for `retina`) and why `--workspace`/`--all` are required — the
root `Cargo.toml` is both the workspace root and a real package (`nicti`), not a virtual manifest,
so a bare `cargo test`/`cargo clippy` (no `-p`/`--workspace`) or a bare `cargo fmt --check` (no
`--all`) silently only checks the root crate and skips `spikes/*`/`bench/whisker` entirely — this
exact gap produced a false-negative "clean" local result once on a real PR whose CI then failed
`cargo fmt` on six files in the (now-deleted) `spikes/den` (fixed alongside #19/ADR-0019).

## CI

GitHub Actions, GitHub-hosted runners only (no self-hosted infra); Windows is the required
(blocking) platform (#17), not Linux. Full gotchas (required-check-gate history #166, the
now-deleted GUI-framework spikes' path-gating #127/#232, the removed CodeQL workflow #281, the CI-duration
watcher #160) moved to [`ci`](.claude/rules/ci/REFERENCE.md) (#34's own PR, to keep this file under
its line-count gate) — read that before touching `.github/**` or a CI-adjacent spike.
