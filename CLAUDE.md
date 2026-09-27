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

Topics: `language-and-architecture` (0015/0021/0019, v1 target), `licensing` (0018/0066, 0069),
`gpu-gui-and-healing` (0016/0068/0050), `catalog-engine` (0067/0102/0106/0103/0107, 0113/0115/0116),
`preview-tiers` (0029, 0143), `raw-decoder` (0037), `volume-identity` (0071), `color` (0038),
`lrc-migration` (0061, 0158), `masking` (0048), `culling` (0033), `denoise` (0040),
`xmp-interop` (0059), `render-graph` (0044). A new ADR adds a bullet to both files of its topic (or
a new topic) and to this list — not inline here.

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
  `verify`/`select` for PowerShell/CI callers.

## Naming convention: feline references

Name new crates, modules, internal tools, and subsystems with a feline-anatomy/behavior angle
rather than a purely descriptive name — the project itself is named after the nictitating
membrane (a cat's third eyelid), and that theme continues throughout. Examples already assigned
for planned subsystems: `Tapetum` (stage-cached render graph — the tapetum lucidum bounces light
back through the retina for reuse, mapping to reusing baked stage output), `Claw` (on-demand
module/plugin registry — claws stay sheathed until needed), `Pounce` (job scheduler with priority
preemption), `Sniff` (embedded-JPEG fast preview path for culling), `Scruff` (import/ingest
pipeline — the way a mother cat carries a kitten by the scruff of its neck is how a file gets
moved into the catalog).

## Package map

`crates/*` (a Cargo workspace member glob, landed in #20) holds the real production crate layout
from ADR-0019 §8. `spikes/*` holds throwaway research spikes not yet promoted — don't build on top
of one; each is deleted once its own ticket promotes it (as #20 already did for
`spikes/sheath`/`spikes/dewclaw`). Full per-spike/per-crate module breakdown moved out to each
topic's own `.claude/rules/<topic>/REFERENCE.md` "Package contents" section (#173) — this stays a
terse index: crate/spike → purpose → owning topic.

- **`crates/nicti-claw`**, **`crates/dewclaw`** — Claw registry + its dylib test fixture →
  [`language-and-architecture`](.claude/rules/language-and-architecture/REFERENCE.md)
- **`crates/nicti-prowl`** — benchmark + golden-image harness (#17); see the Performance targets
  and benchmarking section above
- **`crates/nicti-color`/`nicti-lens`/`nicti-render`/`nicti-ai`/`nicti-export`**
  — extension-point crates (supertrait + `Registry` alias only, no execution methods yet):
  `ColorProfile` (#38/#42), `LensCorrection` (#39), `RenderStage`
  (Tapetum's future home, #44/#45), `ModelProvider` (#48-#53/#33-#36), `Exporter` (#56/#57)
- **`crates/nicti-decode`** — `RawDecoder` extension point (#37/#40/#41, still unimplemented), plus
  `embedded` (#22): the TIFF/EXIF/Nikon-MakerNote IFD walker promoted from `spikes/sniff`
  (`Walker`/`FileSource`/`SliceSource`), used at import time to extract a NEF/DNG's embedded T0
  grid preview without a RAW decode. Trimmed vs. `sniff`'s own copy: no cold/warm
  `FILE_FLAG_NO_BUFFERING` benchmarking distinction, which stays `sniff`-only
- **`crates/nicti-catalog`** — `CatalogStore` extension point plus its real implementation (#22,
  landed): `schema.rs` (SQLite migrations — `volume`/`root`/`asset`, ADR-0071; `preview`, ADR-0029;
  `edit_variant`/`edit_history`, ADR-0021; trigger-maintained `facet_counts`, ADR-0103),
  `sqlite.rs` (`SqliteCatalog`), and `scruff.rs` (the Scruff import/ingest pipeline: scan → stat →
  partial-BLAKE3 fingerprint → EXIF → T0 preview extraction → upsert, one bad file recorded in
  `IngestReport::failed` rather than aborting the run). Runs serially; Pounce integration is a
  follow-up. See `catalog-engine`/`volume-identity`/`preview-tiers` topics for the design this
  promotes.
- **`spikes/pawprint`** (#21/ADR-0021) → [`language-and-architecture`](.claude/rules/language-and-architecture/REFERENCE.md)
- **`spikes/glint`** (#16/ADR-0016) → [`gpu-gui-and-healing`](.claude/rules/gpu-gui-and-healing/REFERENCE.md)
- **`spikes/pelt` + `pelt-egui`/`pelt-iced`/`pelt-slint`** (#68/ADR-0068) →
  [`gpu-gui-and-healing`](.claude/rules/gpu-gui-and-healing/REFERENCE.md)
- **`spikes/groom`** (#50/ADR-0050) → [`gpu-gui-and-healing`](.claude/rules/gpu-gui-and-healing/REFERENCE.md)
- **`spikes/sniff`** (#28/#29/ADR-0029) → [`preview-tiers`](.claude/rules/preview-tiers/REFERENCE.md)
- **`spikes/den`** (#67+/ADR-0067/0102/0106/0103/0107/0066/0113/0115/0116, slated for deletion once #22 lands, see #123) →
  [`catalog-engine`](.claude/rules/catalog-engine/REFERENCE.md)
- **`spikes/retina`** (#37/ADR-0037) → [`raw-decoder`](.claude/rules/raw-decoder/REFERENCE.md)
- **`spikes/homing`** (#71/ADR-0071, Windows-only, unverified in this sandbox) →
  [`volume-identity`](.claude/rules/volume-identity/REFERENCE.md)
- **`spikes/calico`** (#38/ADR-0038) → [`color`](.claude/rules/color/REFERENCE.md)
- **`spikes/shed`** (#61/ADR-0061) → [`lrc-migration`](.claude/rules/lrc-migration/REFERENCE.md)
- **`spikes/siamese`** (#48/ADR-0048) → [`masking`](.claude/rules/masking/REFERENCE.md)
- **`spikes/litter`** (#33/ADR-0033) → [`culling`](.claude/rules/culling/REFERENCE.md)
- **`spikes/rods`** (#40/ADR-0040) → [`denoise`](.claude/rules/denoise/REFERENCE.md)
- **`spikes/scent`** (#59/ADR-0059) → [`xmp-interop`](.claude/rules/xmp-interop/REFERENCE.md)
- **`spikes/loaf`** (#44/ADR-0044) → [`render-graph`](.claude/rules/render-graph/REFERENCE.md)
- **`bench/whisker`** (workspace member) — benchmark tooling for #43, not a production crate; same
  "don't build on top of it" caveat as a spike

The root placeholder binary crate (`src/main.rs`) still exists only so CI/lint tooling has
something real to run against; it is not the shipping v1 target's home yet.

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
CI's exclude flags for `den`/`pelt-*`/`retina`) and why `--workspace`/`--all` are required — the
root `Cargo.toml` is both the workspace root and a real package (`nicti`), not a virtual manifest,
so a bare `cargo test`/`cargo clippy` (no `-p`/`--workspace`) or a bare `cargo fmt --check` (no
`--all`) silently only checks the root crate and skips `spikes/*`/`bench/whisker` entirely — this
exact gap produced a false-negative "clean" local result once on a real PR whose CI then failed
`cargo fmt` on six files in `spikes/den` (fixed alongside #19/ADR-0019).

## CI

GitHub Actions, GitHub-hosted runners (`ubuntu-latest`/`windows-latest`) — this project has no
self-hosted runner infrastructure of its own; don't add a `runs-on: [self-hosted, ...]` job here.
See `.github/workflows/ci.yml`. A `cargo-deny` job checks Rust crate
licenses against `deny.toml` (the allowlist from `docs/adr/0018-third-party-license-policy.md`) —
it only covers Cargo dependencies, not native libraries, ML models, or data files, which still
rely on `docs/licensing.md` being updated at review time.

**Windows is the required (blocking) platform (#17)**, not Linux: `cargo fmt`, `cargo clippy
(windows)`, `cargo test (windows)`, `cargo build (windows, v1 target)`, and `den/pelt windows
(required check gate)` are the branch-protection-required checks, matching the actual v1 target
(README's Scope section). `cargo clippy (linux)`/`cargo test (linux)`/`den (linux)`/
`pelt (linux)` still run on every PR (Linux stays CI-only, catches platform-specific bugs early)
but aren't required to merge.

**`den (windows)`/`pelt (windows)` are themselves NOT in the required-checks list (#166)** --
despite being the jobs that actually do the Windows den/pelt work, listing them directly caused
classic branch protection to block merge on every PR that path-gates them out to `skipped`
(GitHub treats a required check reporting `skipped` as not satisfying the requirement, contrary
to what this file used to claim -- confirmed on #162, which needed `gh pr merge --admin` twice).
`den/pelt windows (required check gate)` is the actual required check instead: it has no
path-gated `if:` of its own (so it's never itself skipped), and only fails when
`den-windows`/`pelt-windows` genuinely failed or were cancelled -- a skipped upstream result
still passes the gate. If a new path-gated Windows-required job is ever added, route it through
this same gate rather than listing it directly in branch protection. Trade-off: the gate trusts
`skipped` unconditionally, so it can't tell "correctly path-gated" apart from "`dorny/paths-filter`
patterns drifted and should have matched but didn't" -- before #166 that case was accidentally
fail-closed (blocked merge, forcing a human to look), after it's fail-open (merges silently). Same
failure class the `changes` job's own comment above already worries about; worth remembering if
den/pelt's path-filter patterns are ever restructured.

**`spikes/den`'s eight bundled native catalog-engine builds, and `spikes/pelt-egui`/`pelt-iced`/
`pelt-slint`'s GUI-framework spikes, are both path-gated the same way (#117, #127)**, not part of
the `clippy`/`test`/`build-windows` jobs every PR pays for: a `changes` job (`dorny/paths-filter`)
only routes into `den (linux)`/`den (windows)` when `spikes/den/**` changed (or `pelt (linux)`/
`pelt (windows)` when `spikes/pelt-egui|iced|slint/**` changed), or `Cargo.{toml,lock}`/the
workflow file itself changed, plus a weekly Monday schedule and `workflow_dispatch` so a
non-touching dependency bump can't silently break either forever between den/pelt PRs. The pelt
split matters because `pelt-egui`/`pelt-iced`/`pelt-slint` alone account for 311 of the 543 unique
crates in a full Windows build (measured via `cargo tree --workspace --exclude den`) — the largest
single driver of #126's cold-build Windows clippy/test times (5m47s/8m35s of actual compile,
confirmed via CI logs to be cold builds, not slow warm ones: `rust-cache` had "No cache found" on
both, since these were brand-new jobs). `Swatinem/rust-cache` only saves (`save-if`) from a push to
`main`, for every job except `den (linux)`/`pelt (linux)` which never save at all (`save-if:
false`, #127) — neither is a required check, and den is already slated for deletion. PRs restore
main's cache and skip the save step, which used to be a 20-30 minute cost on the Windows job by
itself and was pushing this repo's cache usage over GitHub's 10GB/repo limit.
`CARGO_PROFILE_DEV_DEBUG: 0` (workflow-level env) additionally strips debuginfo from both Rust and
den's bundled C/C++ builds, which was most of that cache size. **den (linux)'s own `save-if:
false` was reverted in #154** (2026-09-26): it caused a measured 2-3min -> 25-33min regression
(every run recompiling all eight bundled native engines from scratch) that outweighed the ~1.5GiB
cache-budget saving -- it now saves on push to main like every other job here. `pelt-linux`/
`retina-linux` still use `save-if: false` (cold cost is only 4min/2.5min). `spikes/**` is also excluded from
Renovate (`renovate.json`) for the same reason — den bundles five already-rejected engine
candidates (ADR-0102/0106/0113/0115/0116) that generate bump-PR churn nobody will act on.
**`spikes/den` itself is slated for deletion once #22 lands, and `spikes/pelt-*` once ADR-0068
resolves** — see #123 for the den follow-up cleanup (CI jobs, the Renovate rule, `deny.toml`
exceptions, this section); the pelt-* cleanup isn't filed as its own issue yet since ADR-0068 is
still Proposed pending #90's reference-machine run.

**CodeQL's `rust` analysis (`.github/workflows/codeql.yml`) is scoped to the shipping crates
only (#127)**: before `codeql-action/init` runs, a CI-only step rewrites the checked-out
`Cargo.toml`'s workspace `members` to `[".", "crates/*"]` (never committed — the real file on disk
is untouched) and the `init` step's inline `config.paths-ignore` excludes `spikes/**`/`bench/**`/
`docs/**`. Without this, the extractor's own manifest-loading phase built every workspace member
to resolve its crate graph — including running `den`'s bundled DuckDB/RocksDB/libSQL C/C++ build
scripts from scratch — which measured 18m34s of a 35m51s total run on #126's PR, almost entirely
spent on code that never ships. CodeQL isn't a required check, so this was pure runner-time waste,
not a merge blocker.

**A duration watcher (#160) files a `ci-slow` GitHub issue when a `CI`/`CodeQL Advanced` job
runs over its budget in `.github/ci-budgets.json` on two consecutive main-branch runs** (one slow
run alone is treated as noise, e.g. a cold cache right after a `Cargo.lock` bump) — see
`.github/scripts/ci_duration_watch.py` and `.github/workflows/ci-duration-watch.yml`. A PR that
legitimately makes a job slower raises that job's budget in `ci-budgets.json` in the same PR,
rather than leaving the watcher to keep re-filing against a budget everyone's already accepted
missing.
