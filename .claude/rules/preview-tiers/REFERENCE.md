---
paths:
  - "spikes/sniff/**"
---

# Preview Tiers — Quick Reference

Full reasoning/history: `docs/decisions/preview-tiers.md`.

- **Preview tier strategy (#29)** — `docs/adr/0029`: T0 (grid, `nikon_preview_ifd` verbatim) → T1
  (loupe-fast, `sub_ifd_2`, RAM-only) → T2 (screen, `JpgFromRaw` decoded+resized to 3840px long
  edge, re-encoded JPEG) → T3 (1:1, `JpgFromRaw` full decode). Seek-and-read
  (`spikes/sniff/src/source.rs`) measured ~250x faster at p50 than #28's whole-file-read locate.
- **JPEG over AVIF for T2**: AVIF is ~4.2x smaller but ~9.5x slower to encode, misses the <50ms
  interactive decode budget at p95 — compression win doesn't clear its own latency cost.
- **Cache backend split by tier size**: SQLite for T0 (small, ~138KB), pack-file format for T2
  (large, ~1.2MB) — SQLite's write path is the bottleneck at this size.
- **Gotcha**: `FILE_FLAG_NO_BUFFERING` can only be set at file-open time, not per-read — a bug that
  silently served "cold" reads from the OS page cache and produced a wrong headline number, caught
  by hostile pre-PR review. Real HDD-cold-ranged numbers: ~11-17x slower than NVMe.
- **Follow-up (#143)** — `docs/adr/0143`: AVIF re-measured at faster `ravif` speeds (7-10, vs.
  ADR-0029's fixed 6) against the #37/#136 stratified subset (ref-10k no longer exists, see #136).
  Faster speed narrows but doesn't close the gap (~5.1x slower than JPEG at speed 10, down from
  ~11.9x at speed 6 on this subset) — **JPEG stays the T2 v1 choice**, but AVIF speed 9-10 is now a named candidate
  for #64/#72's archival tiers. SSIM (new this pass) shows AVIF q75 is *not* actually comparable
  quality to JPEG q85 as ADR-0029 assumed (0.898-0.902 vs. 0.9335) despite being ~3x smaller.
  **Real lossy WebP measured and rejected** — worse SSIM than AVIF at comparable size, worse
  encode/decode latency than JPEG at comparable quality, plus a native C dependency neither JPEG
  nor AVIF needs.

## Package contents

- **`spikes/sniff`** (#28's embedded-JPEG research, extended for #29's preview-tier-strategy
  comparison) — a from-scratch TIFF/EXIF/Nikon-MakerNote IFD walker (generic over
  `source::ByteSource`, `SliceSource`/`FileSource`, for #29's ranged seek-and-read extraction), no
  LibRaw/rawler dependency (deliberately, to stay clear of #37's decoder choice); a
  `zune-jpeg`/`fast_image_resize` decode/resize path; `codec.rs`'s JPEG-vs-AVIF-vs-WebP
  tier-payload-format comparison (`ravif`/`avif-decode`, plus lossy WebP via `webp`/`libwebp-sys`,
  added for #143's ADR-0143 follow-up, plus a swept `avif-speed` axis and `nicti-prowl`-reused SSIM
  scoring — see the committed `run-codec-sweep.ps1`); `cache.rs`'s three cache-backend candidates
  (SQLite BLOBs/pack-file/file-per-preview); `tier_bench.rs`'s end-to-end per-tier harness; and a
  locate/read/decode-grid/decode-screen/extract-index/full-read latency benchmark with a
  `--io {whole,ranged}` axis. `sniff inventory` cross-checked byte-exact against `exiftool` on real
  Z8/D7500 files. **#28 closed**: full 9,142-file NVMe set plus the HDD (`E:\`) comparison both
  measured — HDD is ~16x slower than NVMe for cold, randomly-ordered reads (the realistic
  culling-browse case). Also found and fixed a real `FILE_FLAG_NO_BUFFERING` sector-alignment bug
  shared by `read_cold`/`read_cold_range` (a resumed short read could pass a misaligned buffer
  address/file offset to the next call, surfacing as `os error 87` on HDD ~10-19% of the time,
  never on NVMe) — fixed by retrying the identical `seek_read` call on the same handle rather than
  resuming from a running offset. See `docs/research/sniff-embedded-jpeg.md` for #28's write-up and
  `docs/adr/0029-preview-tier-strategy.md` for #29's.
