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
  full per-dependency/per-model audit in `docs/licensing.md` — Rust crate allowlist, ML-model
  bundle-vs-on-demand-download criteria, and the "no Adobe DCP/LCP data" rule. **Amended
  2026-09-24** (see ADR-0003's own Amendments section, load-bearing not historical): the original
  LGPL-native-lib dynamic-linking rule and the blanket GPL/AGPL denial are both superseded now that
  Nicti's own outbound license is decided (see the ADR-0013 bullet below) — read the amendment,
  not just the original 2026-09-23 Decision text. Update `docs/licensing.md` in the same PR as any
  new dependency or model.
- **Outbound license: AGPL-3.0-or-later** — `docs/adr/0013-outbound-license-agpl.md`, resolving
  #66 (open-source release prep) early because ADR-0003's permissive-only default was already
  actively constraining in-flight decisions. Chosen specifically over plain GPL-3.0 for the
  network-use clause (§13) — closes the "run it as a hosted service, never share the source"
  loophole plain GPL leaves open, which matters given #58 (web gallery/upload) and #64
  (multi-machine catalog) are real planned v2 network-facing features, not hypothetical ones.
  Un-excludes Ultralytics YOLO (culling/detection) and exiv2/rexiv2 (EXIF/XMP/IPTC, though
  kamadak-exif/little_exif remain the current unforced choice) on license grounds; removes the
  LGPL-as-Cargo-dependency sign-off/`cdylib`-isolation requirement **for `lensfun-rs` specifically**
  (its confirmed `LGPL-3.0-or-later OR GPL-3.0` dual license combines cleanly now that Nicti's own
  license is already copyleft) — **but not for `rawler`**, whose bare `license = "LGPL-2.1"` (no
  `-only`/`-or-later` suffix, and no project-specific evidence either way beyond that) could still
  mean GPL-2.0-only if relicensed, which this same amendment denies; #37 still needs to resolve
  that before treating rawler as pre-cleared. Reopens RapidRAW (#69) as a potential adopt/fork
  candidate, not just prior-art study, since it's also AGPL-3.0 — see the new tickets filed
  alongside this ADR for follow-up.
- **Module/plugin architecture (Claw)**: `docs/adr/0004-module-plugin-architecture.md` — v1
  first-party modules are in-process Rust traits with a lazy (`OnceLock`-backed) registry so heavy
  modules load on demand. Originally described isolating an LGPL native dependency (e.g. `rawler`/
  `lensfun-rs`) behind a checked C-ABI `cdylib` boundary (`libloading` + an explicit ABI-version
  handshake) specifically to satisfy LGPL's dynamic-linking safe harbor — **that specific reason no
  longer applies for `lensfun-rs`** as of ADR-0003's 2026-09-24 amendment (Nicti's own license is
  now copyleft, and `lensfun-rs`'s confirmed or-later dual license combines in cleanly regardless
  of link type; see the ADR-0013 bullet above) **but still applies for `rawler`**, whose LGPL grant
  isn't confirmed to include an "or later" option — don't drop its isolation/sign-off requirement
  without resolving that first. The `cdylib` boundary mechanism itself is still available and may
  still be worth using for other reasons (plugin flexibility, v2's WASM-plugin direction below). v2
  third-party plugins are directionally WASM (`wasmtime`) for non-hot-path extension points only —
  measured, not assumed, in `crates/nicti-claw/tests/wasm_vs_native.rs` — never for a third-party
  render stage's per-pixel loop, which would need GPU shaders instead. **#20 landed the
  `nicti-claw` + per-domain crate layout** — see the Package map section below.
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
- **Healing/removal**: `docs/adr/0007-healing-and-removal.md` — **Proposed, pending a
  reference-machine measurement pass**. Ships both classic clone/heal (CPU Poisson-Jacobi solve +
  a `wgpu` compute-shader twin, proven correct against each other in `spikes/groom/`) and AI
  removal (MobileSAM+LaMa via `ort`/`load-dynamic`, per ADR-0004 §3's already-decided pattern) as
  two `SpotKind` variants of one `HealStage`, not competing alternatives. No real ONNX weights
  exist in this sandbox — the AI-removal wrappers prove the loading/error-handling shape only.
  Re-verified LaMa's Places2 training-data flag (still unresolved — the primary source stays
  unreachable, a mirror confirms Places2's own non-commercial/no-redistribution terms) and
  researched MI-GAN as an alternative, which turned out **not** to be cleaner (same Places2
  exposure, plus its own unresolved weights-license-legitimacy question) — see
  `docs/research/groom-healing-removal.md`. Measured CPU-only timings (clone_stamp 0.12ms/op,
  spot_heal 0.25ms/op, auto_source_pick 0.05ms/op) and `HealStage` serialized sizes (120/1,511/
  7,531 bytes at 1/10/50 spots); GPU/CUDA numbers deferred to the reference machine. Proposes
  (not commits) heal/remove's stage-order placement for #44: after lens correction, before global
  tone, in linear space.
- **Catalog database engine**: `docs/adr/0008-catalog-database-engine.md` — **SQLite** (`rusqlite`,
  WAL), with a `(model, rating)` composite index and `GLOB` (not `LIKE`) for every prefix-scan
  predicate — this build's `LIKE`-to-index-range-scan transform never triggered, confirmed via
  `EXPLAIN QUERY PLAN`. Measured in `spikes/den/` against a corrected-cardinality synthetic
  generator (`gen.rs`'s per-event `BENCH_LEAF_KEYWORD`, after the first-pass 11-value keyword
  vocabulary turned out to make every hierarchical query artificially non-selective): clears every
  gate at 2M assets except faceted-filter-with-facet-counts (151ms p95 vs the 100ms budget — a
  known, unattempted mitigation is a trigger-maintained facet table). **DuckDB passed every gate
  with the best margins of any candidate, including the point-update gate the issue predicted it
  would fail** — not chosen for v1 only because #22's row-store schema fits SQLite's maturity
  better, kept explicit as the fallback if the facet-query ceiling becomes a real problem. **LMDB
  had the best raw numbers on every indexable op, but its crash-safety gate could not be
  measured**: `heed`/`liblmdb`'s process-wide open-environment guard makes the in-process
  `mem::forget`-based crash simulation this spike used structurally inapplicable (confirmed, not
  assumed) — a real fork+exec+SIGKILL harness is the only way to test it, out of scope this pass.
  Both embedded-Postgres candidates named in the issue were eliminated at the hard-gate stage
  before either got a spike: `pglite-rs`'s `build.rs` unconditionally emits a Unix-only linker
  flag (no Windows path exists in the crate); `pglite-oxide` doesn't compile against its own
  published dependency graph on any target (`wasmer-wasix` vs `virtual-net`, confirmed on two
  versions). Unblocks #22, #23, #24, #25, #71.
- **Turso Database, evaluated post-ADR-0008**: `docs/adr/0009-turso-database-evaluation.md` —
  **not adopted**. The only pure-Rust catalog candidate (matching ADR-0001's own stated
  preference), with real Windows-CI and MIT-license evidence, but its pre-1.0 status shows up
  where it matters: two query shapes already sit at the 600k-scale budget edge due to a
  confirmed-missing `LIKE`/`GLOB` prefix-scan optimization (a competing "missing index"
  explanation was tested and ruled out), and — the deciding factor — a 2M-row bulk-ingest run was
  stopped after its WAL file passed 19GB and was still climbing linearly (data that should be a
  few hundred MB in any other engine; confirmed not explained by a durability-pragma mismatch,
  tested directly). Crash-safety is left **inconclusive**, not failed: every reopen after an
  abandoned transaction hit `database is locked`, but a hostile re-review and a follow-up
  experiment produced two results that don't fully agree on why — see the ADR's own crash-safety
  row for the full, genuinely unresolved account, plus a new `Workload::prepare_for_forget` hook
  this investigation added. ADR-0008 is unchanged: SQLite stays chosen, DuckDB stays the fallback.
  Revisit post-1.0, not never.
- **`redb`, evaluated post-ADR-0009**: `docs/adr/0010-redb-evaluation.md` — **not adopted**. The
  strongest hard-gate evidence of any pure-Rust candidate so far (real Windows CI, MIT/Apache-2.0
  license, 1.0-plus and zero dependencies, unlike Turso's pre-1.0 status) — but two query shapes
  miss the 2M budget (range query ~1.5x over, filename search ~5x over), root-caused to a real,
  documented cost: redb checksums every page on read (confirmed via its own design docs and its
  own upstream benchmark table, which independently shows the same ~1.5–2x-slower-than-LMDB
  pattern on read-heavy workloads specifically). Crash-safety is **inconclusive**, but for a
  cleanly understood reason this time, not a murky one like Turso's: a direct follow-up experiment
  confirmed the reopen failure is an OS-level advisory lock scoped to the one leaked file
  descriptor (not a process-wide guard like LMDB's) — the same *class* of fd-scoped lock ADR-0009
  found in Turso, which this in-process `mem::forget` technique can never get past regardless of
  engine. ADR-0008 is unchanged: SQLite stays chosen, DuckDB stays the fallback.
- **RocksDB, evaluated post-ADR-0010**: `docs/adr/0015-rocksdb-evaluation.md` — **not adopted**.
  Strong Windows-build/license/maintenance evidence (including a real Windows CI gotcha found and
  mirrored into this repo's own CI: `librocksdb-sys`'s `bindgen`-generated FFI bindings need
  libclang, which conflicts with GitHub's `windows-latest` runner's bundled msys64 install unless
  removed first). Two query shapes miss the 2M budget (range query ~1.8x over, filename search
  ~4.5x over, the worst of any candidate on this shape) — root-caused (compaction was tried and
  ruled out as the cause) to RocksDB's per-read LSM cost (bloom-filter + block-cache misses) under
  this pass's default, untuned configuration. Crash-safety is **inconclusive**, same class of
  finding as LMDB/Turso/redb: a leaked `LOCK` file blocks reopening the same forgotten path in this
  in-process technique, confirmed (via a direct probe, same methodology as ADR-0009/0010) to be
  scoped to that specific path, not a process-wide guard like LMDB's. **The concurrent-multi-writer
  comparison this ADR exists to produce — measured for the first time in this series, since every
  prior candidate was only ever benchmarked single-threaded — has a genuine, nuanced answer**:
  RocksDB does not show SQLite's own textbook single-writer-serialization signature (SQLite's
  aggregate throughput stayed flat at ~61k-76k writes/sec regardless of thread count, 1-16 threads,
  while its own max latency grew monotonically from 5ms to 1,458ms under contention — a clean,
  reproduced confirmation of the exact concern #115 was filed to test), and RocksDB's peak observed
  throughput (1.05M writes/sec) was roughly 15x SQLite's ceiling — but RocksDB's own default
  (untuned) configuration showed large, non-monotonic run-to-run variance under sustained
  concurrent load (as low as 47.7k writes/sec in one 16-thread run, at or below SQLite's own
  ceiling), consistent with its own documented write-stall backpressure mechanism, not a clean win
  either way. A tuned re-run (larger block cache/write buffers, bloom-filter tuning) is the named,
  unattempted follow-up if RocksDB is ever reconsidered. ADR-0008 is unchanged: SQLite stays
  chosen, DuckDB stays the fallback — but this ADR's own numbers are the first real evidence in
  this repo of what SQLite's concurrency tradeoff actually costs, worth remembering if a future
  multi-writer feature (#64) ever forces a re-decision.
- **Facet-count cache for SQLite's faceted-filter gap**: `docs/adr/0011-facet-count-cache.md` —
  **trigger-maintained SQLite facet table**, closing ADR-0008's one measured miss (faceted-filter
  at 2M) without adding a new dependency. Clears the <100ms budget by ~33-89x at 600k/2M (well
  under 1ms-3ms p95); the actual per-write gate ADR-0008 sets (`write_rating` ≤5ms) clears with
  10x+ margin at both scales, though a 100-row rating burst (a proxy for #43's rate-and-advance
  culling pattern) is a real, non-negligible added cost (9-20ms, vs. plain SQLite's <2.1ms) — found
  via a benchmark bug (a trigger `WHEN`-guard no-op on repeated same-value writes) that a hostile
  review caught and this ADR documents in full. Verified correct against a from-scratch
  recomputation both at ingest and under a write burst. A DuckDB-backed read-side cache alternative
  was also built and measured (also clears budget, but adds a second store, a real multi-second
  refresh cost at 2M, and a demonstrated staleness window between refreshes) and is kept as the
  explicit fallback, not adopted. Both candidates share a real, verified scope limitation: their
  `(model, rating, keyword)`-grain facet table only answers a *keyword-narrowed* facet query
  correctly — an unfiltered/no-keyword facet count needs a separate table or `sqlite.rs`'s own
  from-scratch query. Unblocks #22's facet-count implementation.
- **DuckDB as v1 primary catalog store, reconsidered post-ADR-0008**:
  `docs/adr/0012-duckdb-as-primary-catalog-store.md` — **not adopted**. ADR-0008's decision is
  unchanged, now for a demonstrated technical reason instead of a soft one: #22's planned
  append-only history log with burst-compaction (ADR-0002) needs frequent UPDATE+DELETE-heavy
  operations, and a real prototype (`spikes/den/src/schema_fit.rs`) measured DuckDB compacting the
  same history runs SQLite compacts **~80x slower per operation** (4.297ms/op vs 0.054ms/op,
  960,000 raw rows down to 19,963 compacted rows), turning a ~1-second workload into an ~86-second
  one — a well-understood transaction-commit-overhead effect (matches ADR-0008's own single-row
  `write_rating` cost roughly doubled), not a benchmark artifact. DuckDB's JSON support
  (`json_extract`) is confirmed real and usable — that part of the schema-fit question favors
  DuckDB — but doesn't offset the compaction cost. #103's separate SQLite-trigger-vs-DuckDB-sidecar
  facet-cache work is unaffected by this outcome.

ADRs live in `docs/adr/`, numbered sequentially.

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
ADR-0006's Hard-gate-1 early exit), and `spikes/sniff` (#28's embedded-JPEG research: a from-scratch
TIFF/EXIF/Nikon-MakerNote IFD walker — no LibRaw/rawler dependency, deliberately, to stay clear of
#37's still-open decoder choice — plus a `zune-jpeg`/`fast_image_resize` decode/resize path and a
locate/read/decode-grid/decode-screen/full-read latency benchmark; `sniff inventory` cross-checked
byte-exact against `exiftool` on real Z8/D7500 files, see `docs/research/sniff-embedded-jpeg.md`
for the full write-up), `spikes/groom` (#50/ADR-0007's healing-and-removal research: CPU
clone-stamp/Poisson-heal/auto-source-pick reference plus a `wgpu` compute-shader Poisson twin
proven correct against it, `ort`/`load-dynamic` MobileSAM+LaMa wrapper scaffolding with no real
ONNX weights in this sandbox, crop/resize/feather compositing, and the `HealStage`/`Spot`
edit-model representation with a pawprint-style `cache_key()`; see
`docs/research/groom-healing-removal.md` for the LaMa/MI-GAN licensing findings), and `spikes/den`
(#67/ADR-0008's catalog-database-engine comparison plus #102/ADR-0009's Turso follow-up,
#106/ADR-0010's `redb` follow-up, #103/ADR-0011's facet-count-cache follow-up, #107/ADR-0012's
schema-fit reconsideration, and #115/ADR-0015's RocksDB follow-up — one module per candidate,
`sqlite.rs`/`duckdb_engine.rs`/`lmdb.rs`/`turso_engine.rs`/`redb_engine.rs`/`rocksdb_engine.rs`/
`facet_cache_trigger.rs`/`facet_cache_duckdb.rs`, behind matching Cargo features (`turso`, `redb`,
and `rocksdb` are all default-off, evaluated-not-adopted, kept for reference; the two facet-cache
modules require `sqlite`, and `facet_cache_duckdb` additionally requires `duckdb`), plus
`schema_fit.rs` (ADR-0002's JSON-column + append-only/burst-compacted history-table shape, gated
on both `sqlite` and `duckdb`) and `concurrent_bench.rs` (#115's own reason for existing — a
genuinely concurrent multi-writer-thread comparison between RocksDB and SQLite, gated on both
`rocksdb` and `sqlite`, not part of the shared `Workload` trait since only these two engines are
compared this way); `gen.rs`'s synthetic-catalog generator is reusable for future
Library-scale benchmarks, see `docs/benchmarks.md`) — not production code; don't build on top of a
spike crate, and expect each
to be deleted once its own ticket promotes it (as #20 just did for
`spikes/sheath`/`spikes/dewclaw`).
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
cargo fmt --all -- --check
```

**Always pass `--workspace`** for `test`/`clippy`, and **`--all`** for `fmt`, in this repo: the
root `Cargo.toml` is both the workspace root and a real package (`nicti`), not a virtual manifest,
so a bare `cargo test`/`cargo clippy` without `-p`/`--workspace`, or a bare `cargo fmt --check`
without `--all`, silently checks only the root crate and skips `spikes/*` and `bench/whisker`
entirely — this exact command (`cargo fmt --check`, no `--all`) used to be what this file itself
documented above, and following it produced a false-negative "clean" result on a real PR whose
CI then failed `cargo fmt` on six files in `spikes/den` — confirmed as a real gap (CI's own
`clippy`/`test` jobs had been doing
exactly this since `spikes/pawprint` landed, until fixed alongside #19/ADR-0004).

## CI

GitHub Actions, GitHub-hosted runners (`ubuntu-latest`/`windows-latest`) — this project has no
self-hosted runner infrastructure of its own and Shutterpaws' old self-hosted GitHub Actions
runner host was retired 2026-08-30, so don't copy the `runs-on: [self-hosted, linux]` pattern
from Shutterpaws repos here. See `.github/workflows/ci.yml`. A `cargo-deny` job checks Rust crate
licenses against `deny.toml` (the allowlist from `docs/adr/0003-third-party-license-policy.md`) —
it only covers Cargo dependencies, not native libraries, ML models, or data files, which still
rely on `docs/licensing.md` being updated at review time.
