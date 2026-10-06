---
paths:
  - "spikes/sniff/**"
  - "crates/nicti-lair/src/tier.rs"
  - "crates/nicti-lair/src/thumb_sidecar.rs"
  - "crates/nicti-lair/src/larder.rs"
  - "crates/nicti-pelt/src/t2.rs"
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
- **T2 cache mgmt (#27, landed)** — `crates/nicti-lair/src/larder.rs` (`Larder`): the bounded T2
  disk cache ADR-0029 chose a pack file for. Byte cap + LRU eviction (recency = per-entry `seq`
  in the SQLite index), `purge_asset`/`purge_tier`/`purge_all`, generation-numbered pack files so
  `compact` switches over atomically (index offsets + generation commit in one transaction; no
  rename over an open file, which Windows refuses). Self-healing: blake3 checksum per payload,
  bad/stale/truncated entries become misses. T0 stays in the catalog `preview` table (not capped
  or purged here). Settings UI (#302, landed): `crates/nicti-pelt/src/cache_settings.rs` (Library view) — live/file bytes vs cap, editable cap (persisted to `<catalog>.larder.cap.json`, loaded by `t2::open_larder`), purge-all + reclaim (`CompactJob`); UI-thread Larder access is `try_lock` only (busy = refuse, never block). T0 purge not built (catalog `preview` table, not Larder).
- **T2 wired into the loupe (#301, landed)** — `crates/nicti-pelt/src/t2.rs` (`T2Job`, `CompactJob`,
  `generate_t2`): embedded `JpgFromRaw` (or a plain JPEG's own file) → 3840px long edge → JPEG q85,
  no RAW decode; `LoupeSession::with_larder` queues one per prefetch-window asset the Larder lacks
  (`render_hash` = `embedded:<asset identity>`, so a re-ingest invalidates it). Unreachable files
  (unmounted archive drive) / busy Larder = quiet `Retry`; unfixable ones `Failed` once per identity. Compaction is a
  Pounce `CompactJob` (`open_larder` sets `set_auto_compact(false)`; `compaction_due`), not inline in `put`.
  UI reads use `try_lock` (busy = miss). `app.rs` upgrades T0 → T2 as the pre-decode fallback.
- **Rendered screen tier (#145, built)** — `docs/adr/0145`: `LarderTier::Rendered` (`"r2"`) beside the camera T2
  (primary key is `(asset_id, tier)`, so both coexist); `Larder::get_latest` = stale-while-revalidate read
  (hash returned, entry kept), `stored_hash` = index-only check. `nicti-pelt/src/eyeshine.rs`: `rendered_hash`
  (`:partial` suffix for masks/AI removals), `choose_preview`/`Badge` (pure display rule), 3-job chain (decode CPU →
  tiled render GPU → encode CPU; render shared with export via `export/render_core.rs`), `EyeshineService`
  (dedupe/supersede). Settings `preview_settings.rs` (`<catalog>.previews.json`: Off/Edited/All + loupe/grid).
  Loupe fallback swaps the render in, never downgrades; grid only flags edited photos (`ThumbImage.edited`,
  `cull::badges::paint_stale`); survey/compare unchanged (ADR-0032). Gotcha: `Larder::get` with a *different* hash
  deletes the entry -- use `get_latest`/`stored_hash` when comparing hashes.
- **Keyed entries (#353, `docs/adr/0353`)**: a second Larder index table `keyed_entry (kind, key)` for content-addressed
  blobs that are many-per-photo -- today `LarderKind::AiAlpha` (baked AI mask alphas, key = bake key). Same pack
  file/cap/LRU (one shared `seq`)/compaction/`purge_all`; `put_keyed`/`get_keyed`/`contains_keyed`/`forget_keyed`/
  `purge_kind`; `purge_asset` drops an asset's keyed rows too. Existing Larders need no migration (`IF NOT EXISTS`).
  See `masking`.

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
- **Tiered thumbnail storage (#72)** — `docs/adr/0072`: archive-drive folders keep T0 as
  `<raw>.thumb.jpg` sidecars (T0 JPEG bytes unchanged; WebP stays rejected per #143), active folders
  in the catalog. Archive-drive role = path-prefix setting (`nicti-pelt/src/archive_drives.rs`) until
  volume identity is wired in. `Carry` exports sidecars before the move (`CarryOptions::export_sidecars`),
  flips `root.archived` in the commit txn, then a chunked `Settle` phase (`nicti-lair/src/tier.rs`)
  drops blobs / re-ingests sidecars; blob-only-dropped-if-sidecar-readable keeps it crash-safe.
  Readers use `tier::load_t0_by_id` (catalog, then sidecar).
