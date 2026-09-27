---
paths:
  - ".github/**"
---

# CI — Quick Reference

Moved out of `CLAUDE.md` (#34's own PR) to keep that file under its line-count gate — this is the
full verbatim content, not a compression, since it's already terse/actionable rather than prose to
summarize further.

GitHub Actions, GitHub-hosted runners (`ubuntu-latest`/`windows-latest`) — this project has no
self-hosted runner infrastructure of its own; don't add a `runs-on: [self-hosted, ...]` job here.
See `.github/workflows/ci.yml`. A `cargo-deny` job checks Rust crate
licenses against `deny.toml` (the allowlist from `docs/adr/0018-third-party-license-policy.md`) —
it only covers Cargo dependencies, not native libraries, ML models, or data files, which still
rely on `docs/licensing.md` being updated at review time.

**Windows is the required (blocking) platform (#17)**, not Linux: `cargo fmt`, `cargo clippy
(windows)`, `cargo test (windows)`, `cargo build (windows, v1 target)`, and `pelt windows
(required check gate)` are the branch-protection-required checks, matching the actual v1 target
(README's Scope section). `cargo clippy (linux)`/`cargo test (linux)`/`pelt (linux)` still run on
every PR (Linux stays CI-only, catches platform-specific bugs early) but aren't required to merge.

**`pelt (windows)` is itself NOT in the required-checks list (#166)** -- despite being the job
that actually does the Windows pelt work, listing it directly caused classic branch protection to
block merge on every PR that path-gates it out to `skipped` (GitHub treats a required check
reporting `skipped` as not satisfying the requirement, contrary to what this file used to claim
-- confirmed on #162, which needed `gh pr merge --admin` twice). `pelt windows (required check
gate)` is the actual required check instead: it has no path-gated `if:` of its own (so it's never
itself skipped), and only fails when `pelt-windows` genuinely failed or was cancelled -- a
skipped upstream result still passes the gate. If a new path-gated Windows-required job is ever
added, route it through this same gate rather than listing it directly in branch protection.
Trade-off: the gate trusts `skipped` unconditionally, so it can't tell "correctly path-gated"
apart from "`dorny/paths-filter` patterns drifted and should have matched but didn't" -- before
#166 that case was accidentally fail-closed (blocked merge, forcing a human to look), after it's
fail-open (merges silently). Same failure class the `changes` job's own comment above already
worries about; worth remembering if pelt's path-filter patterns are ever restructured. (This gate
originally also covered `spikes/den`'s own Windows job, `den (windows)`, until #123 deleted
`spikes/den`.)

**`spikes/pelt-egui`/`pelt-iced`/`pelt-slint`'s GUI-framework spikes are path-gated (#127)**, not
part of the `clippy`/`test`/`build-windows` jobs every PR pays for: a `changes` job
(`dorny/paths-filter`) only routes into `pelt (linux)`/`pelt (windows)` when
`spikes/pelt-egui|iced|slint/**` changed, or `Cargo.{toml,lock}`/the workflow file itself changed,
plus a weekly Monday schedule and `workflow_dispatch` so a non-touching dependency bump can't
silently break pelt forever between pelt-touching PRs. This matters because `pelt-egui`/
`pelt-iced`/`pelt-slint` alone account for 311 of the 543 unique crates in a full Windows build
(measured via `cargo tree --workspace`) — the largest single driver of #126's cold-build Windows
clippy/test times (5m47s/8m35s of actual compile, confirmed via CI logs to be cold builds, not
slow warm ones: `rust-cache` had "No cache found" on both, since these were brand-new jobs).
`Swatinem/rust-cache` only saves (`save-if`) from a push to `main`, for every job except
`pelt (linux)` which never saves at all (`save-if: false`, #127) — not a required check. PRs
restore main's cache and skip the save step, which used to be a 20-30 minute cost on the Windows
job by itself and was pushing this repo's cache usage over GitHub's 10GB/repo limit.
`CARGO_PROFILE_DEV_DEBUG: 0` (workflow-level env) additionally strips debuginfo from Rust and
native build scripts (e.g. nicti-cornea's bundled LibRaw), which was most of that cache size.
`retina-linux` also uses `save-if: false` (cold cost is only 2.5min). `spikes/**` is also excluded
from Renovate (`renovate.json`) since every spike is throwaway and generates bump-PR churn nobody
will act on before it's deleted or promoted.

**`spikes/pelt-*` is slated for deletion now that ADR-0068 is Accepted (#90)** — tracked in #232.
(`spikes/den` went through
the same lifecycle: slated for deletion once #22 landed, and #123 did that cleanup — deleting the
spike itself plus its CI jobs, the Renovate rule's original motivating case, and two `deny.toml`
exceptions. #154, since superseded by that deletion, is a real historical incident worth knowing
about if a future spike's own `save-if: false` cache setting is ever reconsidered: it caused a
measured 2-3min -> 25-33min regression, every run recompiling all eight of `den`'s bundled native
engines from scratch, which outweighed the ~1.5GiB cache-budget saving.)

**CodeQL's `rust` analysis (`.github/workflows/codeql.yml`) is scoped to the shipping crates
only (#127)**: before `codeql-action/init` runs, a CI-only step rewrites the checked-out
`Cargo.toml`'s workspace `members` to `[".", "crates/*"]` (never committed — the real file on disk
is untouched) and the `init` step's inline `config.paths-ignore` excludes `spikes/**`/`bench/**`/
`docs/**`. Without this, the extractor's own manifest-loading phase built every workspace member
to resolve its crate graph — including, at the time this was measured, running `spikes/den`'s
bundled DuckDB/RocksDB/libSQL C/C++ build scripts from scratch — which measured 18m34s of a
35m51s total run on #126's PR, almost entirely spent on code that never ships. CodeQL isn't a
required check, so this was pure runner-time waste, not a merge blocker.

**A duration watcher (#160) files a `ci-slow` GitHub issue when a `CI`/`CodeQL Advanced` job
runs over its budget in `.github/ci-budgets.json` on two consecutive main-branch runs** (one slow
run alone is treated as noise, e.g. a cold cache right after a `Cargo.lock` bump) — see
`.github/scripts/ci_duration_watch.py` and `.github/workflows/ci-duration-watch.yml`. A PR that
legitimately makes a job slower raises that job's budget in `ci-budgets.json` in the same PR,
rather than leaving the watcher to keep re-filing against a budget everyone's already accepted
missing.
