# ADR-0021: T2 preview-codec follow-up — faster AVIF speeds + real lossy WebP

- **Status:** Accepted
- **Date:** 2026-09-26
- **Ticket:** [#143](https://github.com/jordanfelle/nicti/issues/143) Research: AVIF at faster
  ravif speed presets + real lossy-WebP measurement

## Context

ADR-0017 (#29) chose JPEG over AVIF for the T2 (screen) preview tier: AVIF was ~4.2x smaller but
~9.5x slower to encode and missed the &lt;50ms interactive decode budget at p95. Two things were
left unmeasured there, both filed as explicit follow-ups (ADR-0017's Deferred section):

- `ravif`'s encode speed was fixed at 6 (its own mid-range default,
  `spikes/sniff/src/codec.rs`'s old `AVIF_SPEED` constant) and never swept — does a faster preset
  tune away the encode-throughput problem without giving up AVIF's size win?
- Lossy WebP was never measured at all. ADR-0017 only ruled out *lossless* WebP, from priors
  (worse than JPEG for photographic content) — real lossy WebP needs the C-linked `webp` crate, a
  native dependency that pass deliberately avoided pulling in for a spike-stage comparison.

#143 asks for both numbers. The result feeds #64 (open-source release prep) and #72 (tiered
thumbnail storage/archival sidecars) — tiers where the encode-throughput budget doesn't bind the
way it does for T2's interactive path — and settles whether AVIF or WebP should replace JPEG for
T2 itself if the numbers are compelling enough.

**A constraint discovered mid-research, not anticipated in the original plan:** the `ref-10k`
frozen reference dataset ADR-0017's own numbers were measured against no longer exists (#136 — it
vanished from both its NVMe and HDD copies, storage model now under redesign). This ADR's numbers
are measured against #37's validated 261-file stratified subset
(`H:\NictiBench-subset\{anthrocon2024,anthrocon2025,d7500-rory,mff2024}`) instead — **not
comparable in absolute terms to ADR-0017's own T2 table**, only internally comparable across this
ADR's own configs, which all share the same dataset. See Measured results' dataset note.

## Decision rule

Same protocol as ADR-0017's own T2 table: `sniff tier-bench --tier t2-screen`, SQLite cache
backend, 1 discarded warm-up run + 5 measured runs per config (`docs/benchmarks.md`), on the
reference machine (Ryzen 9 9950X). Built natively for Windows
(`--target x86_64-pc-windows-gnu`) and run through Windows PowerShell against `H:\` directly —
never through WSL's `/mnt/h`, whose 9p I/O would distort the read+decode latencies this
comparison depends on.

Quality added this pass, since ADR-0017's T2 table had none: `sniff tier-bench --ssim`, wired to
`nicti_prowl::golden::ssim` (the same hand-rolled SSIM `crates/nicti-prowl` uses for golden-image
regression), scored per-asset against the pre-encode resized source, after each already-recorded
encode-timing measurement — so it doesn't distort the timing numbers, only adds a size/speed/quality
three-way instead of ADR-0017's two-way (size/speed only).

## Measured results

**Dataset:** #37/#136's stratified subset, 261 NEF/DNG files across 4 real-shoot folders
(`anthrocon2024`, `anthrocon2025`, `d7500-rory`, `mff2024` — `exe-test`/`watch-nvme` excluded as
scratch, not real-shoot content), sorted-filename-list SHA-256
`c9100329dca78e17cc670d72035851e2af976b3756b7c4511a8ae6e27c0ad0d4`. No per-file manifest exists for
this ad hoc subset (unlike ref-10k's `docs/ref-10k-manifest.csv`) — the hash above is the closest
available "was this the same set" check, not a byte-content verification of each file.

| Config | Avg encoded size | Encode p50 | Encode p95 | Read+decode p50 | Read+decode p95 | SSIM mean | SSIM p5 |
|---|---|---|---|---|---|---|---|
| JPEG q85 (baseline) | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| AVIF q75 speed 6 (ADR-0017's setting) | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| AVIF q75 speed 7 | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| AVIF q75 speed 8 | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| AVIF q75 speed 9 | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| AVIF q75 speed 10 (fastest) | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| WebP q75 method 4 | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| WebP q80 method 4 | TBD | TBD | TBD | TBD | TBD | TBD | TBD |
| WebP q85 method 4 | TBD | TBD | TBD | TBD | TBD | TBD | TBD |

TBD — full sweep in progress, see `spikes/sniff/run-codec-sweep.ps1`'s committed output for the
raw per-run JSON this table summarizes.

**Findings:** TBD.

**Recommendation:** TBD.

## Options considered

| Option | Verdict |
|---|---|
| AVIF at a faster `ravif` speed preset | TBD |
| Lossy WebP (`webp`/`libwebp-sys`, native C dependency) | TBD |
| JPEG (status quo, ADR-0017) | TBD |

## Consequences

- TBD, pending the measured numbers above.
- `spikes/sniff/src/codec.rs`'s `Codec` enum now has three variants (`Jpeg`/`Avif`/`Webp`) and
  `encode()` takes an explicit `speed: u8` parameter instead of the old hardcoded
  `AVIF_SPEED` constant — any future codec research building on this spike should extend that
  enum, not add a fourth ad hoc comparison path.
- `spikes/sniff`'s Cargo.toml now pulls in a real C dependency (`webp` → `libwebp-sys`, compiled
  via `cc`) for the first time in this crate beyond `rusqlite`'s `bundled` SQLite. Confirmed this
  builds cleanly cross-compiled to `x86_64-pc-windows-gnu` (the CI-required target) before
  committing to this approach.
- `docs/licensing.md` gains a native-library row for `libwebp` (BSD-3-Clause + a separate WebM
  patent grant) — see that file's Native libraries table.
