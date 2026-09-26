## Preview tiers

Covers the T0-T3 preview tier strategy, the JPEG-vs-AVIF format decision, and the cache backend split.

- **Preview tier strategy (#29)**: `docs/adr/0017-preview-tier-strategy.md` — T0 (grid,
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
