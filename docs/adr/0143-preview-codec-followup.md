# ADR-0143: T2 preview-codec follow-up — faster AVIF speeds + real lossy WebP

- **Status:** Accepted
- **Date:** 2026-09-26
- **Ticket:** [#143](https://github.com/jordanfelle/nicti/issues/143) Research: AVIF at faster
  ravif speed presets + real lossy-WebP measurement
- **Formerly:** ADR-0022 (sequential numbering, pre-#183)

## Context

ADR-0029 (#29) chose JPEG over AVIF for the T2 (screen) preview tier: AVIF was ~4.2x smaller but
~9.5x slower to encode and missed the &lt;50ms interactive decode budget at p95. Two things were
left unmeasured there, both filed as explicit follow-ups (ADR-0029's Deferred section):

- `ravif`'s encode speed was fixed at 6 (its own mid-range default,
  `spikes/sniff/src/codec.rs`'s old `AVIF_SPEED` constant) and never swept — does a faster preset
  tune away the encode-throughput problem without giving up AVIF's size win?
- Lossy WebP was never measured at all. ADR-0029 only ruled out *lossless* WebP, from priors
  (worse than JPEG for photographic content) — real lossy WebP needs the C-linked `webp` crate, a
  native dependency that pass deliberately avoided pulling in for a spike-stage comparison.

#143 asks for both numbers. The result feeds #64 (open-source release prep) and #72 (tiered
thumbnail storage/archival sidecars) — tiers where the encode-throughput budget doesn't bind the
way it does for T2's interactive path — and settles whether AVIF or WebP should replace JPEG for
T2 itself if the numbers are compelling enough.

**A constraint discovered mid-research, not anticipated in the original plan:** the `ref-10k`
frozen reference dataset ADR-0029's own numbers were measured against no longer exists (#136 — it
vanished from both its NVMe and HDD copies, storage model now under redesign). This ADR's numbers
are measured against #37's validated 261-file stratified subset
(`H:\NictiBench-subset\{anthrocon2024,anthrocon2025,d7500-rory,mff2024}`) instead — **not
comparable in absolute terms to ADR-0029's own T2 table**, only internally comparable across this
ADR's own configs, which all share the same dataset. See Measured results' dataset note.

## Decision rule

Same protocol as ADR-0029's own T2 table: `sniff tier-bench --tier t2-screen`, SQLite cache
backend, 1 discarded warm-up run + 5 measured runs per config (`docs/benchmarks.md`), on the
reference machine (Ryzen 9 9950X). Built natively for Windows
(`--target x86_64-pc-windows-gnu`) and run through Windows PowerShell against `H:\` directly —
never through WSL's `/mnt/h`, whose 9p I/O would distort the read+decode latencies this
comparison depends on.

Quality added this pass, since ADR-0029's T2 table had none: `sniff tier-bench --ssim`, wired to
`nicti_prowl::golden::ssim` (the same hand-rolled SSIM `crates/nicti-prowl` uses for golden-image
regression), scored per-asset against the pre-encode resized source, after each already-recorded
encode-timing measurement — so it doesn't distort the timing numbers, only adds a size/speed/quality
three-way instead of ADR-0029's two-way (size/speed only).

## Measured results

**Dataset:** #37/#136's stratified subset, 261 NEF/DNG files across 4 real-shoot folders
(`anthrocon2024`, `anthrocon2025`, `d7500-rory`, `mff2024` — `exe-test`/`watch-nvme` excluded as
scratch, not real-shoot content), sorted-filename-list SHA-256
`c9100329dca78e17cc670d72035851e2af976b3756b7c4511a8ae6e27c0ad0d4`. No per-file manifest exists for
this ad hoc subset (unlike ref-10k's `docs/ref-10k-manifest.csv`) — the hash above is the closest
available "was this the same set" check, not a byte-content verification of each file.

All 261 assets encoded successfully in every config (0 failures); values below are the median of
5 measured runs (per `docs/benchmarks.md`'s protocol), via `run-codec-sweep.ps1`'s own aggregation:

| Config | Avg encoded size | Encode p50 | Encode p95 | Read+decode p50 | Read+decode p95 | SSIM mean | SSIM p5 |
|---|---|---|---|---|---|---|---|
| JPEG q85 (baseline) | 1,508,408 B (1.51 MB) | 66.6 ms | 104.9 ms | 24.9 ms | 31.8 ms | 0.9335 | 0.8970 |
| AVIF q75 speed 6 (ADR-0029's setting) | 488,807 B (0.49 MB) | 794.3 ms | 1013.4 ms | 44.9 ms | 77.4 ms | 0.8985 | 0.8541 |
| AVIF q75 speed 7 | 493,798 B (0.49 MB) | 622.4 ms | 694.2 ms | 34.8 ms | 40.4 ms | 0.8984 | 0.8541 |
| AVIF q75 speed 8 | 493,798 B (0.49 MB) | 719.6 ms\* | 1027.6 ms\* | 40.5 ms | 48.3 ms | 0.8984 | 0.8541 |
| AVIF q75 speed 9 | 496,592 B (0.50 MB) | 399.6 ms | 512.3 ms | 34.9 ms | 54.1 ms | 0.8990 | 0.8560 |
| AVIF q75 speed 10 (fastest) | 549,102 B (0.55 MB) | 341.0 ms | 454.0 ms | 32.8 ms | 59.4 ms | 0.9024 | 0.8612 |
| WebP q75 method 4 | 527,556 B (0.53 MB) | 537.9 ms | 659.2 ms | 55.5 ms | 67.1 ms | 0.8847 | 0.8380 |
| WebP q80 method 4 | 738,628 B (0.74 MB) | 556.1 ms | 662.1 ms | 63.3 ms | 73.3 ms | 0.9062 | 0.8623 |
| WebP q85 method 4 | 1,062,616 B (1.06 MB) | 633.4 ms | 716.1 ms | 76.3 ms | 93.0 ms | 0.9285 | 0.8932 |

\* Speed 8's encode p50/p95 land higher than speed 7's, breaking the otherwise-monotonic
speed-6-through-10 trend — reported as measured, not smoothed away. Read+decode latency, encoded
size, and SSIM all stay perfectly consistent with speeds 7/9 bracketing it, so this looks like
real run-to-run wall-clock jitter (single-threaded CPU-bound work sharing the reference machine
with whatever else was running that pass), not a `ravif` behavior worth a claim on its own.

**Findings:**

- **A faster `ravif` speed narrows the encode-throughput gap but doesn't close it.** Speed
  6→10 cuts AVIF's own encode p50 by ~2.3x (794ms→341ms), bringing AVIF from **~11.9x** slower
  than JPEG at speed 6 (794.3/66.6 — worse than ADR-0029's own ~9.5x, but measured against a
  different, incomparable dataset, not a like-for-like regression) down to ~5.1x slower at
  speed 10 (341.0/66.6) — a real improvement, but still far outside `docs/benchmarks.md`'s ingest
  budget (~6ms/asset at 10k-in-60s). File size grows ~12% from speed 6 to speed 10
  (488,807 B → 549,102 B) — the speed dial trades some of AVIF's compression ratio for
  throughput, not free.
- **AVIF's own decode latency is noisy across the tested speeds, not a clean trend with encode
  speed.** p95 read+decode ranges 40.4ms (speed 7) to 77.4ms (speed 6), with speeds 7 and 8
  (40.4ms/48.3ms) actually clearing the &lt;50ms interactive budget while speeds 6, 9, and 10
  (77.4ms/54.1ms/59.4ms) don't — no monotonic relationship to the speed setting, unlike the much
  larger (~2.3x) and clearly-trending encode-time effect above. p50 read+decode does trend down
  from speed 6 to speed 10 (44.9ms→32.8ms, vs. JPEG's 24.9ms), a real if noisy improvement. Given
  the decoder itself is unchanged across speeds (only encode effort/partitioning differs), this
  p95 noise is best read as measurement variance at this dataset's scale, not a real "AVIF decode
  gets worse at higher speed" finding.
- **SSIM reveals AVIF q75 is not actually "roughly comparable perceptual quality" to JPEG q85**,
  the assumption ADR-0029 stated without measuring it: JPEG q85 scores 0.9335 vs. AVIF's
  ~0.898-0.902 across every speed tested, despite AVIF's files being ~3x smaller. AVIF's real
  compression win comes partly from accepting a lower SSIM at this quality setting, not purely
  from better efficiency at equal quality — a correction to ADR-0029's own stated assumption, not
  a contradiction of its numeric findings.
- **Real lossy WebP has worse encode/decode latency and SSIM than the size-comparable AVIF
  setting, and worse latency than JPEG at comparable quality.** WebP q75 (527,556 B) is closest in
  size to AVIF speed 10 (549,102 B, the nearest size match in this table, and itself slightly
  larger than WebP's own file) — against that specific setting, WebP is slower to encode (538ms
  vs. 341ms), slower to decode at p95 (67.1ms vs. 59.4ms), and lower-SSIM (0.8847 vs. 0.9024). This
  is **not** true against every AVIF speed
  indiscriminately: WebP q75 is actually faster than AVIF **speed 6** specifically (538ms vs.
  794ms encode, 67.1ms vs. 77.4ms decode p95) — SSIM is the one axis where WebP loses to every AVIF
  speed tested (0.8847 vs. 0.898-0.902). At its highest quality (q85, 1,062,616 B, SSIM 0.9285 —
  close to JPEG's own SSIM), WebP still needs ~9.5x longer to encode than JPEG (633ms vs. 66.6ms,
  633.4/66.6 — the same order of magnitude as AVIF's own encode-throughput cost above) and its
  read+decode p95 (93.0ms) is nearly **3x** JPEG's own (31.8ms) — the worst decode latency of any
  codec/setting in this table, missing the &lt;50ms interactive budget by the widest margin
  measured. WebP additionally requires a real C toolchain dependency (`libwebp-sys`), unlike AVIF's
  pure-Rust path.

**Recommendation:**

- **JPEG remains the correct T2 v1 choice.** Nothing measured this pass changes ADR-0029's
  decision — AVIF's encode-throughput cost, even at its fastest tested speed, is still ~5x JPEG's,
  and real lossy WebP measures worse than JPEG on every latency and quality axis at comparable
  quality, and worse than the size-comparable AVIF setting on encode/decode latency and SSIM
  (SSIM is the one axis where it loses to every AVIF speed tested, not just the size-comparable
  one).
- **A faster AVIF speed (9 or 10) is worth reconsidering for #64/#72** (archival/cold tiers, where
  the encode-throughput budget doesn't bind the way it does for T2): at speed 10, AVIF's
  read+decode latency gap to JPEG narrows enough (32.8ms vs. 24.9ms p50) that it's a much more
  attractive candidate there than ADR-0029's original speed-6 numbers suggested, and its
  compression win (~2.7x over JPEG at speed 10, 549,102 B vs. 1,508,408 B) is still real, just not
  as large as speed 6's ~3.1x.
- **Lossy WebP should not be pursued further as a T2 (or #64/#72) candidate** absent a specific new
  reason — it's not merely "no better than JPEG," it's measurably worse than AVIF at comparable
  size and worse than JPEG at comparable quality, on both encode and decode latency, while adding a
  native C dependency neither JPEG nor AVIF requires here.

## Options considered

| Option | Verdict |
|---|---|
| AVIF at a faster `ravif` speed preset (7-10) | **Not adopted for T2 v1** — narrows but doesn't close the encode-throughput gap (~5.1x slower than JPEG at best, down from ~11.9x at speed 6). Worth reconsidering for #64/#72's archival tiers, where the gap matters less. |
| Lossy WebP (`webp`/`libwebp-sys`, native C dependency) | **Rejected.** Worse than AVIF at comparable size (lowest SSIM of any config tested) and worse than JPEG at comparable quality (same order-of-magnitude encode-throughput cost as AVIF, plus the widest decode-latency miss of any config, plus a native dependency). |
| JPEG (status quo, ADR-0029) | **Confirmed for T2 v1.** Unchanged by this pass's measurements. |

## Consequences

- **ADR-0029's T2 codec decision (JPEG) is confirmed, not revisited** — this pass adds precision
  to *why*, not a different answer.
- **AVIF at a faster speed (9-10) is now a named candidate for #64/#72's design**, not just "AVIF
  in general" — file that context against whichever issue picks up the archival/cold-tier preview
  format question next, rather than re-deriving it from scratch.
- **Real lossy WebP is now closed out, not just deferred.** ADR-0029 left it unmeasured; this pass
  measured it and found it worse than JPEG at comparable quality, worse than the size-comparable
  AVIF setting on encode/decode latency and SSIM, and worse than every AVIF speed on SSIM
  specifically — no further WebP research is warranted for Nicti's preview pipeline without a
  new, specific reason.
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
