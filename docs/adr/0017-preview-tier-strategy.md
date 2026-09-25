# ADR-0017: Preview tier strategy

- **Status:** Accepted
- **Date:** 2026-09-25
- **Ticket:** [#29](https://github.com/jordanfelle/nicti/issues/29) Research: preview tier strategy

## Context

#29 asks for the preview tier ladder (embedded JPEG for grid/first-pass cull → screen-res loupe →
1:1 on demand) plus a cache format. #28's findings (`docs/research/sniff-embedded-jpeg.md`) give
the embedded-JPEG inventory but left two real gaps this ADR closes: every #28 benchmark number was
a whole-file-read pessimistic bound (no seek-and-read), and #28 misidentified `JpgFromRaw` as "the
actual compressed RAW pixel data" (corrected below). This ADR blocks #27 (preview cache mgmt), #30
(grid at 1M), #31 (loupe prefetch/zoom).

**Reference display** (confirmed via `Get-CimInstance Win32_VideoController` on the reference
machine): NVIDIA RTX 5080 driving 2x ~32" panels at **3840x2160**. In Lightroom Classic's own
Develop view, the loupe viewport occupies roughly 75-79% of the panel, i.e. ~2900x1700 physical
px — a 3:2 landscape image needs ~2550px wide to fill it at 1:1 scale. The `sub_ifd_2` mid preview
(1620x1080) #28 measured is **not enough** for this loupe viewport (a ~1.6x upscale); a generated
screen-resolution tier is required, sized to the display's own long edge (3840), not the 2560 this
ADR's own first draft assumed before the display was confirmed.

**Real-catalog sizing** (read-only query against the user's own LRC catalog backup,
`Lightroom Catalog-2-2-v13-3.lrcat`, extracted from a zip and deleted immediately after querying —
summaries only, no paths/names, per this repo being public):
- **380,300 assets** total, not the assumed 1M planning ceiling — recent growth ~50-70k/yr (2024:
  14.2k, 2025: 52.1k, 2026 partial: 20.9k).
- **File-format mix: 274,499 JPG / 68,352 RAW / 37,449 DNG.** Only **~105.8k assets (28%)** are
  from Nikon bodies (Z8 87,001 + D7500 13,610 + D3400 4,215) — v1's Nikon-NEF-only scope covers
  barely more than a quarter of this real library. The other 72% (dominated by Sony/Panasonic
  point-and-shoot JPEGs — the `DSC-RX10` family alone is 212k assets) is already-decoded
  JPEG/DNG with no embedded-tier-extraction question at all.
  **Design implication**: for a plain-JPEG asset, every tier below is just "resize the master
  file," a separate, much simpler code path from the NEF-embedded-JPEG extraction this ADR
  otherwise covers — the tier design must not be built NEF-only even though this ADR's own
  measurements are all NEF-based.
- **Folder sizes**: 22 folders ≥3,000 images (largest 5,000), 94 folders in the 1,000-2,999 range
  — sizes the "active event" working set the T2 LRU default (below) should cover comfortably.
- `hasDevelopAdjustments` is NULL for every row in this catalog version, so edited-vs-default-only
  share could not be determined from the DB — left unknown rather than guessed.

**Correction to #28's finding** (see `docs/research/sniff-embedded-jpeg.md`'s own Correction
note): `JpgFromRaw` is **not** the actual compressed RAW pixel data, contrary to what #28's doc
originally claimed. Verified via `exiftool` against `ref-00001.nef` (Z8, High Efficiency): the
real raw sensor strip lives in a separate SubIFD (`Compression: Nikon NEF Compressed`,
`ImageWidth/Height: 8280x5520`, its own `StripOffsets`/`StripByteCounts`, ~17.1 MB) — a completely
different offset and ~8x the byte size of `JpgFromRawStart`/`Length` (~2.1 MB). `JpgFromRaw` is a
genuine, separately-encoded, full-pixel-dimension camera JPEG preview (quality ~71 reflects the
camera's own preview-encoding choice, not the HE codec's working quality). This doesn't change its
usefulness as this ADR's T2/T3 source — still a real, camera-rendered JPEG, cheaper than a full
RAW decode — only the description of *why* it exists.

## Tier design

| Tier | Use | Source (NEF/DNG, unedited) | Source (plain JPEG asset) | Persistence |
|---|---|---|---|---|
| T0 grid | grid, filmstrip | `nikon_preview_ifd`, 640x424, verbatim | resize master to 512px long edge | persistent, every asset, `previews.db` (SQLite) |
| T1 loupe-fast | instant loupe placeholder | `sub_ifd_2`, 1620x1080, one ranged read | resize master to ~1620px | RAM ring only, not persisted |
| T2 screen | loupe/cull at 4K | `JpgFromRaw` decoded, resized to 3840px, re-encoded JPEG | resize master to 3840px | bounded LRU disk cache (pack-file format, below) + RAM/VRAM prefetch N±k |
| T3 1:1 | 100% focus-check zoom | `JpgFromRaw` full decode | master at full res | RAM only (current + next), never persisted |

Cross-cutting design points:
- **Ingest records per-tier `(offset, len)`** for every NEF/DNG asset (this ADR's `source.rs`
  ranged reads make this a handful of small positioned reads, not a whole-file read or a
  re-walked IFD tree per later access).
- **Cache key**: `(asset_id, tier, render_hash)`. `render_hash` is ADR-0002's blake3 edit-document
  hash for a stage-rendered tier, or a fixed `embedded` sentinel for camera-JPEG-derived tiers
  (T0/T1/T2/T3 as specified above — none of them reflect user edits yet, since that needs Tapetum/
  #44/#41, out of scope here). Once an image has real develop edits, its T2/T3 tiers should show
  the embedded-camera version stale-while-revalidate until Tapetum produces a real rendered
  screen-tier — flagged, not solved, by this ADR. The camera's own Picture-Control-vs-Nicti-default
  render color mismatch is the same kind of flagged-not-solved gap.
- **Preview store is separate from the catalog DB** (`previews.db`/pack file, not
  `catalog.db`) — fully regenerable from source files, explicitly excluded from #25's backup
  scope.
- **Threading**: #28 already found 8-way parallel whole-file reads hurt per-file latency (NVMe
  queue contention on large sequential reads). This ADR's ranged reads are much smaller
  individual I/Os; the full sweep's thread-count numbers (pending, see below) will confirm
  whether that finding still holds once reads are targeted instead of whole-file.
- **HDD/archive**: T0 stays persistent regardless of drive; T2's LRU cache should not attempt to
  populate for assets on a currently-unmounted archive drive. This feeds #72's sidecar-export
  design (T0 as a `.thumb.jpg` sidecar) but doesn't implement it here.

## Ranged I/O implementation

`spikes/sniff` (research/build tooling, not production) was extended, not rewritten:
- **`source.rs`** (new): `ByteSource` trait (`read_at(offset, len)`), `SliceSource` (in-memory,
  tests) and `FileSource` (positioned reads — Windows `FileExt::seek_read`, Unix `read_at`; a
  256 KB head-prefetch serves the IFD walk's small, clustered reads without a second syscall for
  most files; cold reads reuse `bench.rs`'s `FILE_FLAG_NO_BUFFERING`/`AlignedBuf` machinery, now
  generalized to an arbitrary byte range via `bench::read_cold_range`, not just offset 0).
- **`ifd.rs`**: `Walker` is now generic over `ByteSource` (`Walker<SliceSource>` /
  `Walker<FileSource>`), so the exact same IFD-parsing logic runs against a slice or a ranged file
  handle. Every read inside the walk (TIFF header, one IFD's entries, an external offset array,
  the Nikon MakerNote's 18-byte header) is now a small, explicit range — never a whole-file read.
- **`bench.rs`**: new `--io {whole,ranged}` flag on the existing `locate`/`read`/`decode-grid`/
  `decode-screen` modes, plus a new `extract-index` mode (walks once, collects every embedded
  JPEG's offset/len — simulates the per-tier ingest-index build). Tier candidate-picking
  (`pick_candidate`) now reads only a bounded 64 KB header prefix per candidate (enough to reach
  past SOF/DQT on every real file checked) instead of each candidate's full bytes, so scanning the
  giant `JpgFromRaw` candidate to measure its dimensions no longer means reading multiple MB.
  `SCREEN_TIER_LONG_EDGE` corrected from an assumed 2560 to the confirmed-display 3840. Host
  identity (`os`/`arch`/`hostname`) is now recorded in the JSON output (a gap #28's own doc flagged
  as missing).
- **`codec.rs`** (new): JPEG (existing `zune-jpeg`/`jpeg-encoder` path) vs. **AVIF**
  (`ravif`/`avif-decode`, both pure Rust — no C-toolchain dependency, unlike `image`'s
  `avif-native`/`dav1d` feature) as tier-payload-format candidates.
- **`cache.rs`** (new): three `CacheFormat` backends — SQLite BLOBs (`previews.db`, WAL), an
  append-only pack file + a SQLite offset/len index, and a plain file-per-preview baseline (LRC's
  own `Previews.lrdata` shape). All three support a `begin_batch`/`commit_batch` bracket so a
  bulk-populate pass isn't paying SQLite's per-row implicit-commit cost unfairly (see Measured
  results — this was caught and fixed mid-measurement, not assumed correct).
- **`tier_bench.rs`** (new): wires ranged extraction + codec re-encode + a cache backend together
  for the real T0/T2 numbers below (`sniff tier-bench --tier {t0-grid,t2-screen} --codec
  {jpeg,avif} --cache-format {sqlite,pack,file}`).

**Correctness**: `ifd::tests::file_source_and_slice_source_agree_on_nikon_maker_note_file`
confirms `Walker<FileSource>` and `Walker<SliceSource>` find byte-identical embedded JPEGs
(including via the Nikon-MakerNote-relative-offset path) on the same underlying bytes — ranged
reads change *how much* is read, never *what* is found.

## Measured results

All numbers below are single-threaded, warm reads, manifest order, on the reference machine (AMD
Ryzen 9 9950X, RTX 5080, NVMe `H:\NictiBench\ref-10k`) unless stated otherwise, following
`docs/benchmarks.md`'s 1-discarded-warm-up + 5-measured-run protocol.

### Seek-and-read vs. whole-file (the #28 gap this ADR closes)

Full `docs/benchmarks.md` protocol (1 discarded warm-up, 5 measured runs, pooled). NVMe, warm,
manifest order, 800-file sample:

| Mode | I/O | p50 | p95 | max |
|---|---|---|---|---|
| `locate` | whole | 21.17 ms | 30.56 ms | 209.08 ms |
| `locate` | ranged | 0.085 ms | 0.148 ms | 0.341 ms |
| `read` | whole | 18.32 ms | 31.59 ms | 293.16 ms |
| `read` | ranged | 3.52 ms | 10.38 ms | 24.10 ms |
| `decode-grid` | whole | 22.23 ms | 37.90 ms | 188.24 ms |
| `decode-grid` | ranged | 2.08 ms | 3.40 ms | 14.04 ms |
| `decode-screen` | whole | 255.46 ms | 340.01 ms | 839.93 ms |
| `decode-screen` | ranged | 244.37 ms | 304.96 ms | 602.91 ms |

**`locate`: ~250x faster at p50, ~207x at p95.** **`decode-grid`: ~11x faster at p50** (and clears
the culling target — keypress→next-image &lt;50ms — comfortably at both p50 and p95, warm or
cold). `decode-screen` barely benefits from ranged I/O (its cost is dominated by decoding a large
JPEG and resizing, not the read itself) — this is expected and correctly reflects where the
seek-and-read win applies (extraction) vs. doesn't (decode/resize cost, which #29's cache tier
exists specifically to avoid paying on every access).

NVMe, cold (`FILE_FLAG_NO_BUFFERING`), 300-file sample:

| Mode | I/O | p50 | p95 | max |
|---|---|---|---|---|
| `locate` | whole | 18.57 ms | 29.26 ms | 41.26 ms |
| `locate` | ranged | 0.165 ms | 0.804 ms | 10.05 ms |
| `read` | whole | 18.98 ms | 28.95 ms | 44.04 ms |
| `read` | ranged | 3.56 ms | 5.50 ms | 8.62 ms |
| `decode-grid` | whole | 22.10 ms | 31.66 ms | 37.26 ms |
| `decode-grid` | ranged | 2.49 ms | 3.07 ms | 4.51 ms |

Cold reads on this NVMe drive cost almost nothing extra over warm — the reference SSD's own
hardware is fast enough that OS-cache-bypass barely shows up, unlike a mechanical drive (below).

NVMe, cold, **random** order (the real arbitrary-browse-order case #28 flagged as the one that
matters for actual culling UX), 300-file sample:

| Mode | I/O | p50 | p95 | max |
|---|---|---|---|---|
| `locate` | whole | 19.97 ms | 28.26 ms | 43.72 ms |
| `locate` | ranged | 0.138 ms | 0.266 ms | 0.741 ms |

**No meaningful random-order penalty on NVMe** — #28's own finding of a ~9-12x random-order
penalty was a *whole-file-read, mechanical-seek* phenomenon; it doesn't reproduce on this SSD
(no physical seek cost) even for whole-file reads, and ranged reads show no penalty either.

HDD (`E:\`), cold, manifest order, 300-file sample — the comparison #28's own doc left as an open
gap:

| Mode | I/O | p50 | p95 | max |
|---|---|---|---|---|
| `locate` | whole | 127.10 ms | 150.05 ms | 194.97 ms |
| `locate` | ranged | 0.134 ms | 0.390 ms | 0.568 ms |
| `read` | whole | 126.61 ms | 150.25 ms | 173.43 ms |
| `read` | ranged | 3.51 ms | 5.43 ms | 9.32 ms |
| `extract-index` | ranged | 0.154 ms | 0.279 ms | 0.504 ms |

**The real headline finding**: whole-file reads on HDD are ~6.8x slower than NVMe at p50 (127.1ms
vs. 18.6ms) — a real, expected mechanical-seek cost — but **ranged reads on HDD are barely
distinguishable from NVMe** (0.134ms vs. 0.165ms p50 for `locate`; 3.51ms vs. 3.56ms for `read`).
This makes sense once traced through `source.rs`'s design: the IFD walk's handful of small reads
(TIFF header, IFD0, MakerNote) all land inside the first 256KB `FileSource::open`'s head-prefetch
already reads in one sequential request — sequential throughput is a mechanical drive's strength,
unlike the random seeks a naive per-tag read pattern (or a naive whole-file read across many files
in non-sequential order) would cost. **Seek-and-read doesn't just avoid re-reading unnecessary
bytes — it collapses the NVMe/HDD gap that #28's whole-file-read numbers showed for exactly this
operation.**

**Full-scale ingest gate** (`docs/benchmarks.md`: "10k NEFs grid-browsable &lt; 60s from NVMe";
this real catalog has 105.8k Nikon-eligible assets, not 10k, so the "100k &lt; 10 min" gate is the
one that actually applies): full 9,142-file NVMe run, warm, ranged, `extract-index` mode (builds
the per-tier offset/len index — the real ingest-time cost this ADR's design adds):

| Mode | p50 | p95 | max |
|---|---|---|---|
| `extract-index` (9,142 files) | 0.069 ms | 0.103 ms | 5.61 ms |

Single-threaded wall-clock for the full set ≈ 9,142 × ~0.07ms ≈ **0.64s** — clears the 10k/60s
gate by roughly two orders of magnitude, and scaling linearly to the real catalog's 105.8k
Nikon-eligible assets (≈7.4s) clears the 100k/10min gate with enormous margin. Combined with T0's
own populate cost (SQLite, ~1.3ms/asset measured below) — ≈137s (2.3 min) for the full 105.8k-asset
set — the *combined* index-build + T0-populate ingest cost for the entire real Nikon-eligible
library is still comfortably inside the 100k/10min budget, single-threaded, with no need to invoke
the "don't default to wide thread-pool parallelism" caution from #28 at all for this workload.

### T2 tier-payload format: JPEG vs. AVIF

Added at the user's explicit request after pushing back on ruling out AVIF from priors
("numbers tell it all") — measured, not assumed. 300-asset sample, `JpgFromRaw` decoded/resized to
3840px, re-encoded at JPEG q85 / AVIF q75 (roughly comparable perceptual quality), SQLite cache
backend:

| Codec | Avg encoded size | Encode p50 | Encode p95 | Read+decode p50 | Read+decode p95 |
|---|---|---|---|---|---|
| JPEG q85 | 1,197,627 B (1.19 MB) | 82.2 ms | 95.4 ms | 34.3 ms | 42.4 ms |
| AVIF q75 | 286,352 B (0.28 MB) | 778.8 ms | 893.4 ms | 47.5 ms | 65.0 ms |

**AVIF is ~4.2x smaller** — a real, substantial win, bigger than this ADR's own prior estimate
(which was reasoning from WebP-vs-JPEG ratios, not AVIF-vs-JPEG — a mistake to note explicitly:
don't extrapolate one codec's ratio from another's). But: **encode is ~9.5x slower** (779ms vs.
82ms p50) — at 380k-asset real-catalog scale, single-threaded AVIF encode for every T2 tier would
take roughly 82 hours vs. JPEG's roughly 9 hours, blowing through `docs/benchmarks.md`'s ingest
budget (10k &lt; 60s, i.e. ~6ms/asset) by orders of magnitude unless heavily parallelized or run at
a faster `ravif` speed preset (fixed at 6/mid-range here — not explored, a real follow-up). And
**AVIF decode misses the &lt;50ms interactive budget at p95** (65.0ms vs. JPEG's 42.4ms, vs. LRC's
own next/prev target) using `rav1d` single-threaded on this hardware.

**Recommendation: JPEG for T2 in v1.** AVIF's compression win is real, but its encode-throughput
cost breaks the ingest budget and its decode latency already misses the interactive target on
today's measured single-threaded path — not a close call either way. AVIF (and WebP, not measured
this pass — pure-Rust *lossy* WebP encode isn't available, only lossless, which isn't competitive
for photographic content) stay real candidates once #64/#72's archival/cold-storage paths exist,
where encode time and decode-latency budgets don't bind the way they do for the interactive T2
tier. **Follow-up filed, not built here** (per the user's explicit steer): per-user preview-format
choice with hardware-accel-aware auto-selection (preferring AVIF only where the OS/GPU actually
offers hardware AV1 decode, JPEG XL where available) is a real feature for the eventual
non-spike preview-generation pipeline — building that detection logic now, inside a throwaway
research spike, would be scope creep into code this repo's own conventions already mark as
disposable. Track as a new issue once the real preview pipeline (post-#22/#44) exists.

### Cache backend: SQLite vs. pack-file vs. file-per-preview

Two very different payload sizes, since the right backend differs by size:

**T0 (verbatim, ~138 KB avg, 800-asset sample):**

| Backend | Populate (800 assets) | Read+decode p50 | Read+decode p95 | Read+decode max |
|---|---|---|---|---|
| SQLite | 1,042 ms | 1.33 ms | 1.61 ms | 2.20 ms |
| Pack file | 48 ms | 1.43 ms | 2.63 ms | 7.17 ms |
| File-per-preview | 421 ms | 1.48 ms | 2.61 ms | 13.86 ms |

At this size, **SQLite has the best and most consistent read latency** (lowest p95 *and* lowest
max) despite a slower populate than Pack — and 1,042ms for 800 assets (~1.3ms/asset) is nowhere
near the ingest budget regardless. **Recommendation: SQLite (`previews.db`) for T0** — consolidated
single-file storage also matches this repo's general SQLite-where-it-fits preference (ADR-0008).

**T2 (re-encoded JPEG q85, ~1.2 MB avg, 300-asset sample):**

| Backend | Populate (300 assets) | Read+decode p50 | Read+decode p95 | Disk overhead |
|---|---|---|---|---|
| SQLite | 2,333 ms | 31.4 ms | 38.5 ms | +0.4% |
| Pack file | 138 ms | 31.8 ms | 39.1 ms | ~0% |
| File-per-preview | 223 ms | 31.0 ms | 37.0 ms | 0% (by definition) |

Read latency is essentially tied across all three at this size — the real differentiator is
populate throughput: **SQLite is ~10-17x slower to populate** than Pack/File even after fixing a
real benchmarking bug this pass caught (wrapping the populate loop in one `BEGIN`/`COMMIT`
transaction instead of paying SQLite's per-row implicit-commit/WAL-fsync cost; this took populate
from 2,946ms down to 2,333ms — better, but SQLite's own large-BLOB write path is still the
bottleneck, not transaction overhead). This matches this ADR's own prior expectation (SQLite's
internal-vs-external-BLOB crossover, ~100 KB at default page size) — confirmed, not just assumed.
**Recommendation: pack-file format for T2** (and by extension T1's on-demand reads, same size
class) — SQLite stays for T0 and the catalog DB itself, not every preview tier uniformly.

Fixed along the way: `SqliteBlobCache::disk_bytes` originally reported ~2x the real footprint
after the batched-transaction fix, because a single big transaction leaves everything in the
`-wal` file until the next checkpoint — added an explicit `PRAGMA wal_checkpoint(TRUNCATE)`
before measuring, not left as a misleading number.

**A second real benchmarking bug, found and fixed the same way**: the reference-machine sweep
script (ad hoc, not committed — `spikes/sniff/run-tier-sweep.ps1`) used the same output directory
for both the NVMe-cold and HDD-cold configuration blocks. Since `bench.rs`'s JSON filenames encode
`(mode, io, order, cold, threads)` but not which drive root was used, the HDD-cold run (which ran
second) **silently overwrote** the NVMe-cold `locate`/`read`/`extract-index` result files —
identical filenames, no error, no warning. Caught by cross-checking the pooled numbers against
physical expectations (a "NVMe cold" locate number that matched HDD-seek-cost magnitude, not
NVMe's), not assumed correct. Fixed by re-running the five affected NVMe-cold configs into a
clearly separate directory (`sniff-nvme-cold-redo`) — the HDD-cold numbers in the tables above were
themselves undamaged (the last write wins, and HDD ran last), only the NVMe-cold values needed
recovery. `bench.rs`'s `RunResult` JSON now also records the sample `root` actually used (it
didn't before, which is exactly why this collision went undetected until the numbers themselves
looked physically wrong) — fixed in this same PR, not left as a known gap for whoever reuses this
tooling next.

## Decision

- **Tier ladder**: T0 (verbatim `nikon_preview_ifd` / resized-master JPEG, SQLite) → T1 (verbatim
  `sub_ifd_2` / resized-master, RAM-only, no persistence) → T2 (`JpgFromRaw` decoded/resized to
  3840px / resized-master, JPEG-encoded, pack-file cache) → T3 (`JpgFromRaw` full decode /
  full-res master, RAM-only). Plain-JPEG assets (72% of the real catalog) use a resize-the-master
  path at every tier, not embedded-JPEG extraction.
- **Extraction**: ranged reads via `Walker<FileSource>`, never whole-file — ~250x faster at p50
  (~207x at p95) for `locate`.
- **Tier-payload format**: JPEG, not AVIF, for v1's interactive tiers — AVIF's real ~4.2x size win
  doesn't clear its own encode-throughput and decode-latency costs against this project's ingest
  and interactivity budgets.
- **Cache backend**: SQLite for T0 (small, consistent, fits the catalog's existing SQLite
  preference); a pack-file format for T2 (large blobs, SQLite's write path is the bottleneck at
  this size).

## Deferred / follow-ups (not built in this pass)

- Per-user preview-format choice with hardware-accel-aware auto-selection (AVIF/JPEG XL where
  hardware decode exists) — file as a new issue against the real (non-spike) preview pipeline.
- A faster `ravif` speed preset, to see whether AVIF's encode-throughput problem can be tuned away
  without giving up its size win.
- Real lossy-WebP measurement (needs the C `libwebp`-linked `webp` crate, a native dependency this
  pass deliberately avoided pulling in for a spike-stage comparison).
- LRU eviction/compaction for the pack-file cache (not simulated this pass).
- `.github/workflows/ci.yml` now installs `nasm` in all four general clippy/test jobs for
  `ravif`/`rav1d`'s SIMD build — done in this PR, not deferred, but worth flagging here since it
  touches every PR's CI setup time going forward.
