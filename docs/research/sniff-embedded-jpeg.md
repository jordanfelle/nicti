# Sniff: embedded-JPEG extraction across Nikon bodies

Findings for [#28](https://github.com/jordanfelle/nicti/issues/28), feeding [#29](https://github.com/jordanfelle/nicti/issues/29)'s
preview-tier design. Tooling: `spikes/sniff/` — see that crate's own module docs for the walker
design. This is a findings doc, not an ADR; the tier decision itself belongs to #29.

## Method

`sniff` implements its own minimal TIFF/EXIF/Nikon-MakerNote IFD walker (`spikes/sniff/src/ifd.rs`)
rather than depending on LibRaw or `rawler`, for two reasons: it avoids deciding #37's RAW-decoder
question early, and it avoids the LGPL-as-Cargo-dependency review `rawler` needs under ADR-0003.
It walks IFD0, the classic "next IFD" thumbnail chain, every SubIFD (including DNG-style
`NewSubfileType=1` reduced-resolution previews), and — the one genuinely nonstandard piece of the
walk — the Nikon MakerNote's PreviewIFD, whose internal offsets (including its own
JPEGInterchangeFormat offset) are relative to the MakerNote's own embedded TIFF header, not to the
file or to the IFD that contains the pointer.

Two subcommands:
- `sniff inventory <root> <manifest.csv> --out <out.csv>`: finds every embedded JPEG, inspects its
  SOF (real dimensions, chroma subsampling) and DQT (IJG-scale quality estimate) headers without
  decoding pixels, and verifies the file's SHA-256 against the committed manifest.
- `sniff bench <root> --mode {locate,read,decode-grid,decode-screen,full-read} --threads N --order
  {manifest,random} [--cold]`: per-file latency, written as JSON for later p50/p95/max pooling per
  `docs/benchmarks.md`'s methodology (1 discarded warm-up run, then 5 measured runs). Cold reads on
  Windows bypass the OS cache via `FILE_FLAG_NO_BUFFERING` (sector-aligned reads); this doesn't
  exist on other platforms, so cold numbers only come from the Windows-native reference-machine
  run, same rule `docs/benchmarks.md` already states for every other benchmark in this repo.

## Correctness validation

Before trusting `sniff`'s numbers, its output was cross-checked against `exiftool` (ground truth)
on real files from each body in `ref-10k`:

| File | Body | Source | `sniff` offset/length | `exiftool` tag | Match |
|---|---|---|---|---|---|
| ref-00001.nef | Z8 | `nikon_preview_ifd` | 101216 / 139987 | `PreviewImageStart`/`Length` | exact |
| ref-00001.nef | Z8 | `sub_ifd_0` | 1017856 / 2103724 | `JpgFromRawStart`/`Length` | exact |
| ref-00001.nef | Z8 | `ifd0` (thumbnail) | 241224 / 11496 | `ThumbnailOffset`/`Length` | exact |
| ref-08714.nef | D7500 | `nikon_preview_ifd` | 53876 / 137510 | `PreviewImageStart`/`Length` | exact |
| ref-08714.nef | D7500 | `sub_ifd_0` | 1069568 / 3165262 | `JpgFromRawStart`/`Length` | exact |
| ref-08843.dng | D3400 (DNG) | `sub_ifd_1` | 432720 / 64600 | `PreviewImageStart`/`Length` | exact |

Every byte offset and length matched exactly, on two different camera bodies and both NEF and DNG
containers. `ref-00001.nef` also surfaced a fourth embedded JPEG (`sub_ifd_2`, 1620x1080) that
isn't behind any of exiftool's named convenience tags but is confirmed present via its own
`Compression = JPEG (old-style)` tag — a genuine additional mid-size preview candidate for the
screen tier.

**Known gap:** the D3400 bucket is Lightroom-converted DNG, not native NEF (see
`docs/benchmarks.md`'s own note on this). Adobe's DNG converter stores the original Nikon MakerNote
as an opaque blob under `DNGPrivateData` (tag `0xC634`, `"Adobe\0MakN"` + length + original offset +
byte-order + the verbatim original MakerNote bytes) instead of exposing it as a normal EXIF
MakerNote tag (`0x8769` -> `0x927C`). `sniff` does not currently unwrap `DNGPrivateData`, so it
misses the Nikon-style PreviewIFD preview on D3400 DNGs specifically (confirmed via `exiftool -v3`:
the preview is genuinely there, just reachable only through the DNG-specific wrapper). It still
finds the DNG-native `SubIFD1` reduced-resolution preview (1024x683) via the ordinary SubIFD path,
so D3400 files aren't left with zero preview coverage — just less than native NEF bodies get.
Per `docs/benchmarks.md`, the D3400 bucket is "decoder-compatibility only, not performance-gated,"
so this gap is not blocking; flagged here rather than silently expanding `sniff`'s scope to chase
Adobe's wrapper format for a bucket the project's own methodology already excludes from every
performance target.

## Per-body findings

Full `sniff inventory` run against the complete frozen `ref-10k` set (all 9,142 files), on the
reference machine's NVMe (`H:\NictiBench\ref-10k`): **35,539 embedded-JPEG rows, 0 parse errors, 0
SHA-256 mismatches.** Every real file in the set parsed cleanly and matched its committed manifest
hash.

| Body | Source | n | Dimensions | Bytes (p50 / p95) | Quality (p50) | Subsampling |
|---|---|---|---|---|---|---|
| Z8 | `ifd0` (classic thumbnail) | 8,713 | 160x120 | 10.8 KB / 12.9 KB | 95 | 4:2:2 |
| Z8 | `nikon_preview_ifd` | 8,713 | 640x424 | 137 KB / 154 KB | 97 | 4:2:2 |
| Z8 | `sub_ifd_2` (mid preview) | 8,713 | 1620x1080 | 883 KB / 1.0 MB | 98 | 4:2:2 |
| Z8 | `sub_ifd_0` (`JpgFromRaw`, full-res) | 8,713 | 8256x5504 | 3.6 MB / 5.9 MB | 71 | 4:2:2 |
| D7500 | `nikon_preview_ifd` | 129 | 640x424 | 131 KB / 135 KB | 95 | 4:2:2 |
| D7500 | `sub_ifd_2` (mid preview) | 129 | 1620x1080 | 982 KB / 1.05 MB | 97 | 4:2:2 |
| D7500 | `sub_ifd_0` (`JpgFromRaw`, full-res) | 129 | 5568x3712 | 2.7 MB / 3.0 MB | 71 | 4:2:2 |
| D3400 (DNG) | `sub_ifd_1` (reduced preview) | 291 | 1024x683 | 54 KB / 126 KB | 81 | 4:2:0 |
| D3400 (DNG) | `sub_ifd_2`/`sub_ifd_3` (other aspect ratios) | 9 | ~1024xN | varies | 81 | 4:2:0 |

Every Z8 and D7500 file (all 8,842 of them) carries the same consistent 3-4-tier structure: a tiny
classic thumbnail (Z8 only), a small `nikon_preview_ifd` (~640x424, ~140 KB), a genuinely useful
mid-size `sub_ifd_2` (1620x1080, ~1 MB, quality 97-98), and the full-resolution `sub_ifd_0`
(`JpgFromRaw`) — which for High Efficiency/HE*-compressed Z8 files *is the actual compressed RAW
pixel data*, not just a preview (quality ~71, matching the codec's own working quality, not a
separately-chosen preview quality). Every D3400 DNG file has at least one usable preview via a
`SubIFD`, just smaller (max 1024px) and no MakerNote-based mid/large tier (see the DNGPrivateData
gap above).

## Throughput

Measured on the reference machine: AMD Ryzen 9 9950X (16-core), 93.7 GB RAM, Windows 11 Pro build
26200, NVIDIA GeForce RTX 5080 (driver 32.0.16.1656), `H:`/`E:` both labeled "Storage"/"Storage4"
local volumes. Windows-native via WSL→Windows cross-compile + interop, against an 800-file bounded
sample of `H:\NictiBench\ref-10k` (NVMe), 1 discarded warm-up + 3 measured runs per config.
**Gap vs. `docs/benchmarks.md`'s stated methodology:** that doc says "every result records CPU,
GPU, driver version, RAM, Windows build, and drive models" in the result itself — `sniff bench`'s
JSON output doesn't yet capture this (only mode/order/threads/cold/run_index/samples), so it's
recorded by hand here instead. Worth fixing in `sniff bench` itself before relying on its raw JSON
as a standalone artifact.

| Mode | Threads | Cold | p50 | p95 | max |
|---|---|---|---|---|---|
| `locate` | 1 | warm | 17.0 ms | 21.5 ms | 31.6 ms (248 ms once) |
| `locate` | 1 | cold | 14.0 ms | 20.6 ms | 32.9 ms |
| `locate` | 8 | cold | 47.3 ms | 66.6 ms | 101.6 ms |
| `decode-grid` | 1 | warm | 22.0 ms | 39.5 ms | 59-146 ms |
| `decode-grid` | 8 | warm | 46.6 ms | 69.7 ms | 84-118 ms |

**Important caveat on all of the above:** every mode, including `locate`, currently reads the
*entire* file (`fs::read`, ~4-20 MB for these bodies) before finding or decoding the target JPEG —
it does not yet seek-and-read only the specific byte range an optimized implementation would use.
These numbers are therefore a *pessimistic upper bound*, not what a real culling fast-path would
achieve; a targeted-read implementation should beat every number here, likely substantially for
`locate` (which currently pays the full file's I/O cost just to return an offset).

**Two real findings survive that caveat:**
1. Even with the pessimistic whole-file-read cost included, single-threaded `decode-grid` already
   clears the culling target (`keypress -> next image < 50ms`) at p50 (22 ms) and comes close at
   p95 (39.5 ms, vs. the 50ms bar).
2. **8-way parallelism made *per-file* latency worse, not better**, for both `locate` and
   `decode-grid` (roughly 3x the single-threaded p50 in both cases) — 8 threads each doing a large
   sequential whole-file read contend for the same physical NVMe queue, so aggregate throughput
   gains don't translate into lower per-file latency the way they would for many small, independent
   reads. This argues against naively parallelizing preview extraction across many worker threads
   in #29's design; a smaller pool, or single-threaded extraction with the OS's own read-ahead, may
   serve the `<50ms` interactive target better than throwing more threads at whole-file reads.

**Not yet measured** in this pass (left as exact repro commands below, since a full run bumped
into this session's time budget partway through `decode-screen`): `decode-screen`, the
`full-read` baseline, the full 9,142-file set (vs. this run's 800-file sample), and the HDD
(`E:\`) comparison. The design implication above (seek-and-read, not whole-file-read) should be
built into `sniff bench` before that fuller pass, since it changes every number materially.

## Reproducing / extending this run

All commands below assume `H:\NictiBench\ref-10k` (NVMe) / `E:\NictiBench\ref-10k` (HDD) per
`docs/benchmarks.md`, and a release build of `sniff` (cross-compiled from WSL to
`x86_64-pc-windows-gnu`, then run Windows-native via interop — the same approach
[#85](https://github.com/jordanfelle/nicti/issues/85)'s `glint` spike used):

```powershell
# Inventory (correctness + per-body size/quality/subsampling stats) -- already run, see above.
sniff.exe inventory H:\NictiBench\ref-10k docs\ref-10k-manifest.csv --out sniff-nvme.csv
sniff.exe inventory E:\NictiBench\ref-10k docs\ref-10k-manifest.csv --out sniff-hdd.csv --hdd-sample-every 20

# Throughput matrix: full file set, all modes, both drives, cold+warm, both orders.
# (This session ran an 800-file NVMe-only bounded sample -- see Throughput above.)
sniff.exe bench H:\NictiBench\ref-10k --mode locate --threads 1 --order manifest
sniff.exe bench H:\NictiBench\ref-10k --mode decode-screen --threads 1 --order manifest
sniff.exe bench H:\NictiBench\ref-10k --mode full-read --threads 1 --order manifest --sample-limit 500 --cold
sniff.exe bench E:\NictiBench\ref-10k --mode locate --threads 1 --order random --cold --sample-limit 500
sniff.exe bench E:\NictiBench\ref-10k --mode full-read --threads 1 --order random --cold --sample-limit 500
```

## Recommendations for #29

- **Tiers**: `nikon_preview_ifd` (640x424) for the grid tier, `sub_ifd_2` (1620x1080, quality
  97-98) for the loupe/screen tier — both consistently present on every Z8/D7500 file. The classic
  `ifd0` thumbnail (160x120, Z8 only) is too small to be useful as its own tier. `sub_ifd_0`
  (`JpgFromRaw`, full-res) is worth keeping as the zoom/1:1 source instead of a full RAW decode,
  given its quality (~71) is the codec's own working quality, not an artificially degraded preview.
- **Extraction strategy**: build `sniff`'s next iteration (or #29's production implementation)
  around seek-and-read of just the target byte range, not a full-file read — see the throughput
  caveat above. This alone should be the single biggest lever toward the `<50ms` culling target,
  bigger than any decode-side optimization.
- **Threading**: don't default to wide thread-pool parallelism for preview extraction; the 800-file
  sample shows it hurting per-file latency for whole-file reads. Re-measure once extraction is
  seek-based, since the contention picture may differ for small, targeted reads.
- If decode throughput (not I/O) turns out to be the bottleneck once extraction is seek-based, a
  DCT-domain scaled decode (e.g. libjpeg-turbo's 1/8 IDCT scaling) is a follow-up lever worth its
  own ticket rather than building it speculatively here.
- D3400/Adobe-DNG-native MakerNote preview support (the `DNGPrivateData` gap above) is not worth
  building given the D3400 bucket's "decoder-compatibility only" status — revisit only if that
  status changes.
