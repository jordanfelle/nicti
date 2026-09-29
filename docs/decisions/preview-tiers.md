## Preview tiers

Covers the T0-T3 preview tier strategy, the JPEG-vs-AVIF format decision, and the cache backend split.

- **Preview tier strategy (#29)**: `docs/adr/0029-preview-tier-strategy.md` — T0 (grid,
  `nikon_preview_ifd` verbatim) → T1 (loupe-fast, `sub_ifd_2`, RAM-only) → T2 (screen, `JpgFromRaw`
  decoded+resized to the confirmed reference display's 3840px long edge, re-encoded JPEG) → T3
  (1:1, `JpgFromRaw` full decode). Real-catalog sizing (380,300 assets, only 28% Nikon-body — most
  is already-decoded JPEG needing a separate resize-the-master path, not embedded-tier
  extraction) and a correction to #28's finding (`JpgFromRaw` is a real distinct preview JPEG, not
  the actual raw sensor strip — that lives in its own SubIFD, confirmed via `exiftool`) both feed
  this ADR. Seek-and-read (`spikes/sniff/src/source.rs`'s `ByteSource`/`FileSource`, `ifd::Walker`
  now generic over it) measured **~250x faster at p50** than #28's original whole-file-read locate
  path.
  **AVIF measured against JPEG for the T2 tier at the user's explicit request** (pure-Rust
  `ravif`/`avif-decode`, no C toolchain): ~4.2x smaller but ~9.5x slower to encode and misses the
  &lt;50ms interactive decode budget at p95 — **JPEG stays the v1 choice**, AVIF's compression win
  doesn't clear its own throughput/latency costs. **Cache backend split by tier size**: SQLite for
  T0 (small, ~138KB, fastest+most-consistent reads) vs. a pack-file format for T2 (large, ~1.2MB,
  SQLite's write path is the bottleneck at this size, confirmed after fixing a real
  batched-transaction benchmarking bug). Hardware-accel-aware format auto-selection explicitly
  deferred to a future issue against the real (non-spike) preview pipeline, not built into this
  research spike. `ravif`/`rav1d` need `nasm` to build — added to `deny.toml` (`MPL-2.0`, `IJG`)
  and `.github/workflows/ci.yml`'s four general clippy/test jobs in the same PR. **A hostile
  pre-PR review caught a real measurement bug**: `source::FileSource::open` never actually
  requested `FILE_FLAG_NO_BUFFERING` on Windows even when `cold=true` (that flag can only be set
  at open time, not per-read), so every "cold, ranged" number was silently served from the OS page
  cache — this produced a plausible-looking but wrong headline claim in an earlier draft ("ranged
  reads collapse the NVMe/HDD gap even when cold"). Fixed and re-measured: HDD cold ranged reads
  are genuinely ~11-17x slower than NVMe (not collapsed) — still a large win over whole-file HDD
  reads (~8.7x), just not the gap-eliminating result the bug produced. See the ADR's own
  Measured-results section for the full account.

- **Follow-up (#143)**: `docs/adr/0143-preview-codec-followup.md` — ADR-0029 left two things
  unmeasured: whether a faster `ravif` speed preset (fixed at 6 there) tunes away AVIF's
  encode-throughput cost, and real lossy WebP (needs the C-linked `webp` crate, avoided in the
  original spike-stage comparison). Both measured this pass, plus a new SSIM quality axis
  ADR-0029's own table didn't have (`nicti_prowl::golden::ssim`, reused rather than duplicated).
  Measured against #37/#136's 261-file stratified subset, not ref-10k (which no longer exists —
  see #136) — not directly comparable to ADR-0029's own absolute numbers, only internally
  comparable across this follow-up's own configs.
  **Result: JPEG stays the T2 v1 choice, confirmed rather than revisited.** A faster `ravif` speed
  narrows AVIF's encode-throughput gap from ~11.9x to ~5.1x slower than JPEG (speed 10 vs. speed 6,
  this subset), and speed 10's p50 decode latency is closer to JPEG's own too (32.8ms vs. 24.9ms) —
  though decode latency is noisy across speeds, not a clean trend. Doesn't close the gap outright
  — worth reconsidering for #64/#72's archival tiers, where the throughput budget doesn't bind the
  same way. SSIM revealed AVIF q75 is
  *not* actually comparable perceptual quality to JPEG q85 as ADR-0029 assumed (0.898-0.902 vs.
  0.9335), despite being ~3x smaller — a correction to that assumption, not to ADR-0029's own
  numeric findings. Real lossy WebP was measured and rejected outright: worse SSIM than AVIF at
  comparable size, worse encode/decode latency than JPEG at comparable quality, plus a native C
  dependency (`libwebp-sys`) neither JPEG nor AVIF requires. See the ADR for the full table.

- **T2 cache management (#27, landed)**: `crates/nicti-lair/src/larder.rs` (`Larder`) implements
  the bounded T2 disk cache ADR-0029 chose a pack-file format for. Cap is on *live* payload bytes;
  eviction is LRU by a per-entry sequence number kept in the SQLite index (persisted lazily, so a
  restart can only under-order the last <256 touches, never reorder old entries past new ones).
  Eviction and purge only drop index rows, leaving dead bytes in the pack file, so `compact`
  rewrites live entries into `pack-<gen+1>.bin` and adopts it by committing new offsets and the
  new generation in one SQLite transaction, then deleting the old file: a crash leaves either
  generation fully consistent, and nothing renames over an open file (Windows would refuse).
  Auto-compaction triggers once dead bytes exceed both a floor and the live bytes, bounding the
  pack file to roughly `2 * cap + floor`. Every payload carries a blake3 checksum in its index row;
  a checksum failure, short read, stale `render_hash`, or a row pointing past the end of the pack
  file (crash before the OS flushed the tail) is dropped and reported as a miss, since the cache
  is fully regenerable. A payload larger than the whole cap is rejected without evicting anything.
  T0 stays in the catalog's `preview` table and is neither capped nor purged by this module.

