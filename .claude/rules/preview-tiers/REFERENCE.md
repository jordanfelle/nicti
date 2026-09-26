---
paths:
  - "spikes/sniff/**"
---

# Preview Tiers — Quick Reference

Full reasoning/history: `.claude/docs/preview-tiers/README.md`.

- **Preview tier strategy (#29)** — `docs/adr/0017`: T0 (grid, `nikon_preview_ifd` verbatim) → T1
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
