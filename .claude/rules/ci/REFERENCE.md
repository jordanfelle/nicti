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
(windows)`, `cargo test (windows)`, and `cargo build (windows, v1 target)` are the
branch-protection-required checks, matching the actual v1 target (README's Scope section).
`cargo clippy (linux)`/`cargo test (linux)` still run on every PR (Linux stays CI-only, catches
platform-specific bugs early) but aren't required to merge.

**`spikes/pelt`/`pelt-egui`/`pelt-iced`/`pelt-slint` (#68/ADR-0068's GUI-framework research
spikes) were deleted in #232** once ADR-0068 was Accepted (#90) — along with their path-gated
`pelt (linux)`/`pelt (windows)` CI jobs, the `pelt windows (required check gate)` job and its
branch-protection required-check entry (#166), the Renovate rule pinning `wgpu = "27"` in
`spikes/pelt-iced/Cargo.toml`, and the Slint-specific `deny.toml` license exceptions. `spikes/den`
went through the same lifecycle before it: slated for deletion once #22 landed, and #123 did that
cleanup — deleting the spike itself plus its CI jobs, the Renovate rule's original motivating
case, and two `deny.toml` exceptions. #154, since superseded by that deletion, is a real
historical incident worth knowing about if a future spike's own `save-if: false` cache setting is
ever reconsidered: it caused a measured 2-3min -> 25-33min regression, every run recompiling all
eight of `den`'s bundled native engines from scratch, which outweighed the ~1.5GiB cache-budget
saving. `spikes/**` is excluded from Renovate (`renovate.json`) since every spike is throwaway and
generates bump-PR churn nobody will act on before it's deleted or promoted.

**CodeQL was removed (#281)**: `.github/workflows/codeql.yml` (Analyze rust/actions on push,
PR and weekly) is deleted. It was never a required check, had zero open alerts, and its `rust` job
was the slowest thing on every PR (over its 10m budget on consecutive main runs, #281) -- largely
because the extractor's LoadManifest phase builds the whole crate graph, including `nicti-pelt`'s
LibRaw C++ (needs `submodules: true`). Re-add a scheduled-only scan if a real need appears; the
scoping trick that used to live here (a CI-only `Cargo.toml` `members` rewrite to
`[".", "crates/*"]` plus `paths-ignore` for `spikes/**`/`bench/**`/`docs/**`, #127) is in git
history at `codeql.yml`.

**A duration watcher (#160) files a `ci-slow` GitHub issue when a `CI` job
runs over its budget in `.github/ci-budgets.json` on two consecutive main-branch runs** (one slow
run alone is treated as noise, e.g. a cold cache right after a `Cargo.lock` bump) — see
`.github/scripts/ci_duration_watch.py` and `.github/workflows/ci-duration-watch.yml`. A PR that
legitimately makes a job slower raises that job's budget in `ci-budgets.json` in the same PR,
rather than leaving the watcher to keep re-filing against a budget everyone's already accepted
missing.

**`nicti-pelt`/`nicti` unconditionally build LibRaw's C++ now (#31 phase 1)** — `nicti-pelt`
depends on `nicti-cornea` with the `libraw` feature always on (the shipped app must actually
decode real NEFs), unlike `nicti-cornea`/`retina`/`knead` themselves, which stay path-gated
research/optional targets. Every job whose graph includes `nicti-pelt`/`nicti` needs
`submodules: true` as a result: `clippy`/`test` (linux+windows), `build-windows`,
and `release.yml`'s Windows build. `decode-linux`/`decode-windows` still exist for their own
direct-target coverage of `nicti-cornea`/`retina`/`knead` (excluded as direct targets elsewhere),
but no longer avoid the LibRaw C++ compile itself — a decode-path-touching PR now pays that cost
twice per platform (once transitively via `nicti-pelt` in the always-on jobs, once directly here).
