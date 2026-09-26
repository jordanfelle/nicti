# Nicti

Rust RAW photo editor + DAM, aiming to replace Adobe Lightroom Classic. Public repo:
`github.com/jordanfelle/nicti`. This file is Claude Code-specific guidance; **`CONTRIBUTING.md`
is the canonical human contributor guide** — read that first if you're new here.

## Architecture decisions

ADRs live in `docs/adr/` (see `docs/adr/README.md` for the index and template), numbered
sequentially. Per-ADR decisions, measured results, and gotchas live in
`.claude/rules/<topic>/REFERENCE.md` + `docs/decisions/<topic>.md`, not inline here, to keep this
file under the line-count gate. Each topic has:

- `.claude/rules/<topic>/REFERENCE.md` — terse key:value/bullet compression, the actionable fact +
  a pointer, scoped with `paths:` frontmatter so Claude Code only auto-loads it when touching a
  matching file (unscoped `.claude/rules/**` files load unconditionally every session, which is why
  this repo scopes them).
- `docs/decisions/<topic>.md` — the full original prose, verbatim, with every issue ref and piece
  of reasoning. Not auto-loaded by Claude Code, but a normal repo doc any contributor can read.

Topics: `language-and-architecture` (0001/0002/0004, v1 target), `licensing` (0003/0013, 0018),
`gpu-gui-and-healing` (0005/0006/0007), `catalog-engine` (0008–0012, 0014–0016),
`preview-tiers` (0017), `raw-decoder` (0019), `volume-identity` (0020). A new ADR adds a bullet to
both files of its topic (or a new topic) and to this list — not inline here.

## Performance targets and benchmarking

- **Performance targets + benchmark methodology**: `docs/benchmarks.md` — p95 targets per feature
  area, warm/cold measurement rules, and the `ref-10k` frozen reference dataset (manifest at
  `docs/ref-10k-manifest.csv`). Finalized 2026-09-23 (#14). Every render-engine/perf-sensitive
  ticket (#43, #17, #40, etc.) measures against this.
- **Hero-scenario benchmark (#43)**: spec at `docs/benchmarks/hero-scenario.md`. Tooling under
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
preemption), `Sniff` (embedded-JPEG fast preview path for culling).

## Package map

`crates/*` (a Cargo workspace member glob, landed in #20) holds the real production crate layout
from ADR-0004 §8:

- **`crates/nicti-claw`** — the `Module` trait (identity/versioning shared by every extension
  point), the lazy `OnceLock`-backed `Registry` (`crates/nicti-claw/src/registry.rs`), and the
  checked C-ABI dylib handshake (`crates/nicti-claw/src/dylib.rs`) — the load-bearing crate every
  other `nicti-*` crate builds on. Generalized from the now-deleted `spikes/sheath` spike.
- **`crates/dewclaw`** — test fixture (cdylib) for `nicti-claw`'s dylib tests, generalized from
  the now-deleted `spikes/dewclaw`.
- **`crates/nicti-prowl`** — the benchmark + golden-image harness (#17); see the Performance
  targets and benchmarking section above for its module breakdown.
- One crate per extension point, each holding only its supertrait plus a `Registry` type alias —
  no execution methods yet, those belong to the tickets named below: `nicti-decode` (`RawDecoder`,
  #37/#40/#41), `nicti-color` (`ColorProfile`, #38/#42), `nicti-lens` (`LensCorrection`, #39),
  `nicti-render` (`RenderStage`, Tapetum's future home, #44/#45), `nicti-ai` (`ModelProvider`,
  #48-#53/#33-#36), `nicti-export` (`Exporter`, #56/#57), `nicti-catalog` (`CatalogStore`, #22's
  future home).

The root placeholder binary crate (`src/main.rs`) still exists only so CI/lint tooling has
something real to run against; it is not the shipping v1 target's home yet. `spikes/*` still
holds throwaway research spikes not yet promoted — `spikes/pawprint` (#21/ADR-0002's
edit-document hashing, history/compaction, and XMP round-trip proof), `spikes/glint`
(#16/ADR-0005's wgpu-vs-CUDA measured comparison — correctness, feature/limit availability,
throughput, dispatch overhead, host↔device interop cost), and `spikes/pelt` +
`spikes/pelt-egui`/`spikes/pelt-iced`/`spikes/pelt-slint` (#68/ADR-0006's GUI-framework research —
`pelt` is the toolkit-agnostic shared fixture/math crate, each `pelt-*` is one candidate's
virtualized-grid + loupe + custom-wgpu-viewport spike; no `spikes/pelt-gpui` exists, see
ADR-0006's Hard-gate-1 early exit), and `spikes/sniff` (#28's embedded-JPEG research, extended for
#29's preview-tier-strategy comparison: a from-scratch TIFF/EXIF/Nikon-MakerNote IFD walker (now
generic over `source::ByteSource` — `SliceSource`/`FileSource` — for #29's ranged, seek-and-read
extraction) — no LibRaw/rawler dependency, deliberately, to stay clear of #37's still-open decoder
choice — plus a `zune-jpeg`/`fast_image_resize` decode/resize path, `codec.rs`'s JPEG-vs-AVIF
tier-payload-format comparison (`ravif`/`avif-decode`, pure Rust), `cache.rs`'s three
cache-backend candidates (SQLite BLOBs/pack-file/file-per-preview), `tier_bench.rs`'s end-to-end
per-tier harness, and a locate/read/decode-grid/decode-screen/extract-index/full-read latency
benchmark with a `--io {whole,ranged}` axis; `sniff inventory` cross-checked byte-exact against
`exiftool` on real Z8/D7500 files. **#28 closed**: full 9,142-file NVMe set plus the HDD (`E:\`)
comparison the issue's own scope called for are both measured — HDD is ~16x slower than NVMe for
cold, randomly-ordered reads, same order/cold-ness on both drives (the realistic culling-browse
case). Also found and fixed a real `FILE_FLAG_NO_BUFFERING` sector-alignment bug shared by
`read_cold`/`read_cold_range` (a resumed short read could pass a misaligned buffer address/file
offset to the next call, surfacing as `os error 87` on HDD ~10-19% of the time, never on NVMe) —
fixed by retrying the identical `seek_read` call on the same handle rather than resuming from a
running offset. See `docs/research/sniff-embedded-jpeg.md` for #28's write-up and
`docs/adr/0017-preview-tier-strategy.md` for #29's), `spikes/groom` (#50/ADR-0007's
healing-and-removal research: CPU
clone-stamp/Poisson-heal/auto-source-pick reference plus a `wgpu` compute-shader Poisson twin
proven correct against it, `ort`/`load-dynamic` MobileSAM+LaMa wrapper scaffolding with no real
ONNX weights in this sandbox, crop/resize/feather compositing, and the `HealStage`/`Spot`
edit-model representation with a pawprint-style `cache_key()`; see
`docs/research/groom-healing-removal.md` for the LaMa/MI-GAN licensing findings), and `spikes/den`
(#67/ADR-0008's catalog-database-engine comparison plus #102/ADR-0009's Turso follow-up,
#106/ADR-0010's `redb` follow-up, #103/ADR-0011's facet-count-cache follow-up, #107/ADR-0012's
schema-fit reconsideration, #113/ADR-0014's `libSQL` follow-up, #115/ADR-0015's RocksDB follow-up,
and #116/ADR-0016's `fjall` follow-up — one module per candidate,
`sqlite.rs`/`duckdb_engine.rs`/`lmdb.rs`/`turso_engine.rs`/`redb_engine.rs`/`libsql_engine.rs`/
`rocksdb_engine.rs`/`fjall_engine.rs`/`facet_cache_trigger.rs`/`facet_cache_duckdb.rs`, behind
matching Cargo features (`turso`, `redb`, `libsql`, `rocksdb`, and `fjall` are all default-off,
evaluated-not-adopted, kept for reference — `libsql` additionally cannot be enabled in the same
binary as `sqlite`, both bundle their own SQLite C symbols and collide at link time, see
ADR-0014's Spike section; `rocksdb` and `fjall` have no such collision, they link cleanly
alongside every other engine, see ADR-0015's/ADR-0016's Consequences; the two facet-cache modules
require `sqlite`, and `facet_cache_duckdb` additionally requires `duckdb`), plus `schema_fit.rs`
(ADR-0002's JSON-column + append-only/burst-compacted history-table shape, gated on both `sqlite`
and `duckdb`) and `concurrent_bench.rs` (#115's own reason for existing — a genuinely concurrent
multi-writer-thread comparison between RocksDB and SQLite, gated on both `rocksdb` and `sqlite`,
not part of the shared `Workload` trait since only these two engines are compared this way);
`gen.rs`'s synthetic-catalog generator is reusable for future Library-scale benchmarks, see
`docs/benchmarks.md`) — not production code; don't build on top of a spike crate, and expect each
to be deleted once its own ticket promotes it (as #20 just did for
`spikes/sheath`/`spikes/dewclaw`), and `spikes/retina` (#37/ADR-0019's RAW decoder comparison —
vendors LibRaw's HE/HE\*-capable fork as a git submodule at `spikes/retina/vendor/LibRaw`, compiled
via the `cc` crate through a hand-written shim, no bindgen; `sweep`/`compare`/`diff` against rawler
0.8.0, plus `scan` for a manifest-free directory walk and `watch` for #24's `notify` research; see
`docs/research/retina-raw-decoder.md`). Its own `vendor/LibRaw` submodule needs
`git submodule update --init spikes/retina/vendor/LibRaw` before it builds.
`bench/whisker` (a workspace member) is benchmark tooling for #43, not a production crate either —
same "don't build on top of it" caveat applies. `spikes/homing` (#71/ADR-0020's volume-identity
research: candidate identity keys measured against a drive-letter change/detach-reattach/reformat
survival table, a `volume`/`root`/`asset` SQLite schema, size+name/partial-BLAKE3/full-BLAKE3/
EXIF-natural-key relink tiers, and a `sysinfo`-poll-vs-`CM_Register_Notification` mount-detection
comparison — lib+bin split so its currently-CLI-unwired helpers don't trip `dead_code` the way a
bin-only spike would; `lib.rs` re-exports `fingerprint`/`mount_events`/`path`/`relink`/`schema`/
`volume`) is Windows-only research written in a Linux/WSL sandbox with no mountable NTFS volume:
its `windows_impl` modules are unverified against real hardware (see ADR-0020's own sandbox-note
and Measured-results section, all marked TBD pending a reference-machine pass), while its
cross-platform schema/fingerprint/path logic is real, tested (20 unit tests), and — unlike
`den`/`pelt-*`/`retina` — not path-gated out of CI's normal `clippy`/`test` jobs, since it needs no
heavy native build (same as `sniff`).

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
`cargo fmt` on six files in `spikes/den` (fixed alongside #19/ADR-0004).

## CI

GitHub Actions, GitHub-hosted runners (`ubuntu-latest`/`windows-latest`) — this project has no
self-hosted runner infrastructure of its own; don't add a `runs-on: [self-hosted, ...]` job here.
See `.github/workflows/ci.yml`. A `cargo-deny` job checks Rust crate
licenses against `deny.toml` (the allowlist from `docs/adr/0003-third-party-license-policy.md`) —
it only covers Cargo dependencies, not native libraries, ML models, or data files, which still
rely on `docs/licensing.md` being updated at review time.

**Windows is the required (blocking) platform (#17)**, not Linux: `cargo fmt`, `cargo clippy
(windows)`, `cargo test (windows)`, `cargo build (windows, v1 target)`, `den (windows)`, and
`pelt (windows)` are the branch-protection-required checks, matching the actual v1 target
(README's Scope section). `cargo clippy (linux)`/`cargo test (linux)`/`den (linux)`/
`pelt (linux)` still run on every PR (Linux stays CI-only, catches platform-specific bugs early)
but aren't required to merge.

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
den's bundled C/C++ builds, which was most of that cache size. `spikes/**` is also excluded from
Renovate (`renovate.json`) for the same reason — den bundles five already-rejected engine
candidates (ADR-0009/0010/0014/0015/0016) that generate bump-PR churn nobody will act on.
**`spikes/den` itself is slated for deletion once #22 lands, and `spikes/pelt-*` once ADR-0006
resolves** — see #123 for the den follow-up cleanup (CI jobs, the Renovate rule, `deny.toml`
exceptions, this section); the pelt-* cleanup isn't filed as its own issue yet since ADR-0006 is
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
