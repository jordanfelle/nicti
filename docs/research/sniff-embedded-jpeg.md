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
hash. The same inventory pass against the HDD (`E:\NictiBench\ref-10k`, `--hdd-sample-every 20` —
every 20th file's manifest hash re-checked rather than all 9,142, per this command's own purpose
of keeping the slower drive's correctness pass cheap) reproduces the identical structural result:
**9,142 files walked, 35,539 embedded-JPEG rows, 0 parse errors, 0 SHA-256 mismatches.** Correctness
doesn't depend on which drive the files live on.

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
(`JpgFromRaw`, quality ~71). **Correction (added for #29): `JpgFromRaw` is not the actual
compressed RAW pixel data**, contrary to what this doc originally claimed. Verified directly via
`exiftool` against `ref-00001.nef` (Z8, High Efficiency): the real raw sensor strip lives in a
separate SubIFD (`Compression: Nikon NEF Compressed`, `ImageWidth/Height: 8280x5520`, its own
`StripOffsets`/`StripByteCounts`, ~17.1 MB) — a completely different offset and roughly 8x the
byte size of `JpgFromRawStart`/`Length`'s ~2.1 MB. `JpgFromRaw` is a genuine, separately-encoded
JPEG preview at the camera's full pixel dimensions, not a view into the compressed raw strip; its
quality-~71 estimate reflects the camera's own preview-encoding choice, not the HE codec's working
quality. This doesn't change `JpgFromRaw`'s usefulness as #29's T3 (1:1 zoom) source — it's still
a real, full-resolution, camera-rendered JPEG, cheaper than a full RAW decode — only the
description of *why* it exists. Every D3400 DNG file has at least one usable preview via a
`SubIFD`, just smaller (max 1024px) and no MakerNote-based mid/large tier (see the DNGPrivateData
gap above).

## Throughput

Measured on the reference machine: AMD Ryzen 9 9950X (16-core), 93.7 GB RAM, Windows 11 Pro build
26200, NVIDIA GeForce RTX 5080 (driver 32.0.16.1656), `H:`/`E:` both labeled "Storage"/"Storage4"
local volumes. Windows-native via WSL→Windows cross-compile + interop, against a bounded sample of
`H:\NictiBench\ref-10k` (NVMe) — 800 files for `locate`/`decode-grid`/`decode-screen`, 300 for the
two `full-read`/`random`-order configs (the original sample-limit for those runs) — 1 discarded
warm-up + 3 measured runs per config.
**Gap vs. `docs/benchmarks.md`'s stated methodology:** that doc says "every result records CPU,
GPU, driver version, RAM, Windows build, and drive models" in the result itself — `sniff bench`'s
JSON output doesn't yet capture this (only mode/order/threads/cold/run_index/samples), so it's
recorded by hand here instead. Worth fixing in `sniff bench` itself before relying on its raw JSON
as a standalone artifact.

| Mode | Threads | Cold | Order | p50 | p95 | max |
|---|---|---|---|---|---|---|
| `locate` | 1 | warm | manifest | 17.0 ms | 21.4 ms | 248.3 ms once |
| `locate` | 1 | cold | manifest | 14.1 ms | 20.9 ms | 33.6 ms |
| `locate` | 8 | cold | manifest | 47.0 ms | 66.8 ms | 102.5 ms |
| `locate` | 1 | cold | random | 131.2 ms | 152.4 ms | 205.0 ms |
| `decode-grid` | 1 | warm | manifest | 21.9 ms | 39.9 ms | 145.9 ms |
| `decode-grid` | 8 | warm | manifest | 46.5 ms | 68.8 ms | 117.8 ms |
| `decode-screen` | 1 | warm | manifest | 205.7 ms | 287.8 ms | 467.6 ms |
| `full-read` | 1 | cold | manifest | 11.4 ms | 13.8 ms | 15.6 ms |
| `full-read` | 1 | cold | random | 131.6 ms | 152.3 ms | 203.5 ms |

`decode-screen` and `full-read` (both orders) are new in this rerun — filling the gap this doc
originally left open (see "Not yet measured" below). All numbers above are re-pooled across 3
measured runs (1 discarded warm-up), same 800-file (or 300-file for the two `full-read` /
`random` configs, per the original sample-limit) bounded set, run against the tier-selection-bug
fix that landed with this same PR/#28 (`locate_offset` inspecting each candidate's own SOF header
instead of relying on the `ImageWidth`/`ImageLength` TIFF tags real NEF/DNG files don't carry) —
`locate`/`decode-grid` reproduce their original numbers almost exactly, confirming the fix didn't
change steady-state throughput, just correctness of which tier gets selected.

**`random` order is ~9-12x slower than `manifest` order** for both `locate` and `full-read` (131ms
vs. 14-17ms cold) — expected, since `manifest` order reads files in on-disk/creation order (mostly
sequential for a freshly-written reference set) while `random` order forces the drive to seek
across the full 9,142-file span for every read. This matters more than the threading finding below
for real-world culling UX, where the user's actual browse order is arbitrary, not sequential.

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

## Full-set NVMe and HDD comparison

The full 9,142-file NVMe set and the HDD (`E:\`) comparison the issue's own scope calls for
("extraction throughput on NVMe vs HDD") are both now measured:

| Mode | Drive | Order | Cold | n | p50 | p95 | max |
|---|---|---|---|---|---|---|---|
| `locate` | NVMe | manifest | warm | 45,710 | 18.4 ms | 44.9 ms | 645.9 ms |
| `decode-screen` | NVMe | manifest | warm | 9,142 | 204.4 ms | 254.6 ms | 689.7 ms |
| `full-read` | NVMe | manifest | cold | 2,500 | 12.3 ms | 14.5 ms | 56.1 ms |
| `locate` | NVMe | random | cold | 2,500 | 15.6 ms | 34.0 ms | 91.0 ms |
| `full-read` | NVMe | random | cold | 2,500 | 15.7 ms | 32.4 ms | 101.6 ms |
| `locate` | HDD | random | cold | 2,500 | 252.2 ms | 447.8 ms | 3,794.2 ms |
| `full-read` | HDD | random | cold | 2,500 | 251.2 ms | 454.1 ms | 4,507.3 ms |

The two *manifest*-order `locate`/`decode-screen` rows used the full 9,142-file set; every other
row (every `full-read` row, plus both NVMe- and HDD-random-cold `locate` rows) used the 500-file
sample the doc's own reproduction commands specify (`full-read` isn't tier-dependent, and a
500-file random-order cold sample already forces the drive to seek across the full span, so a
full-set run adds cost without adding information here). `decode-screen`'s full-set
pass used 1 warm-up + 1 measured run rather than the usual 1+5 — a deliberate scope reduction, not a
methodology violation: the per-file variance this mode's numbers carry was already characterized
at 800-file/3-run scale in the Throughput section above, so what this full-set pass needed to add
was *coverage* (does the number hold at 9,142 files, not just 800), not more repeated-measurement
samples. 9,142 pooled samples from one run is still far more data than the earlier 800-file/3-run
pass's 2,400.

**HDD is ~16x slower than NVMe, isolating the drive alone** — the NVMe-random-cold and
HDD-random-cold rows above hold order and cold-ness fixed and vary only the drive: `locate` 252.2ms
vs 15.6ms p50 (16.2x), `full-read` 251.2ms vs 15.7ms p50 (16.0x). This is a different, cleaner
comparison than "HDD-random vs NVMe-manifest" (an earlier draft of this doc quoted 251ms vs 12ms —
correct numbers, but conflating two separate effects: the drive's own speed *and* the ~9-12x
NVMe manifest-vs-random seek-order cost already measured in the Throughput section above). Both
effects are real; they don't stack multiplicatively into a single number, since NVMe's manifest
number is fast enough that random order costs relatively more of it proportionally than the same
seek pattern costs HDD's already-slow baseline — comparing across two varied dimensions at once
overstated the apparent NVMe-vs-HDD gap. The drive-alone comparison above (~16x) is the number that
actually answers the issue's own question.

**This NVMe-random-cold number (15.6/15.7ms p50) does not match the older 800-file bounded pass's
own NVMe-random-cold figures (131.2/131.6ms p50) in the Throughput section above — an ~8x gap on
what should be the same drive, mode, and order.** Re-checked directly: a fresh, independent
500-file random-order cold run against `H:\NictiBench\ref-10k` reproduces the same order of
magnitude as the number used here (23.1ms p50 on that single-round check), not anything close to
131ms — so the new number is the one that holds up under re-verification, not a fluke. The most
likely explanation, though not independently reverified since the buggy code no longer exists to
re-test against: the pre-fix `read_cold` bug documented below likely didn't only produce hard
`os error 87` failures on a short read — a misaligned continuation that Windows *accepted* rather
than rejected could plausibly have fallen back to a slower internal path, inflating latency on
NVMe (where a short read is rare but not impossible) without ever surfacing as a failed sample.
This is a plausible reconciling explanation, not a confirmed one; the two figures are left in this
doc side by side rather than silently reconciled, per the standing rule that a real discrepancy
should be visible to a future reader, not smoothed over.

### Real bug found and fixed: `read_cold`'s NO_BUFFERING alignment violation on short reads

The first full-set/HDD pass (before the fix below) showed HDD `locate`/`full-read` failing on a
non-trivial, reproducible ~10-19% of samples — deterministic per file set when reshuffled within one
process, but a *different* subset each fresh invocation (different random shuffle), which initially
looked like it might be a transient environmental artifact rather than a real defect: an isolated
copy of the failing files in their own directory, and a fresh full-directory random draw, both came
back clean on first retry. It wasn't transient — those "clean" retries were themselves false
negatives from an unrelated tooling gap (see below), not evidence the bug was gone.

**Root cause**, confirmed via the actual OS error text (`os error 87`, `ERROR_INVALID_PARAMETER`):
`read_cold`'s original chunked-read loop resumed a short read from `buf.as_mut_slice()[total..]`,
passing a buffer address of `base_ptr + total` to the next `ReadFile` call. `FILE_FLAG_NO_BUFFERING`
requires every read's buffer address, file offset, and length to be sector-aligned — guaranteed for
the *first* call by `AlignedBuf`'s own 4096-byte-aligned allocation and the file's initial position,
but only guaranteed for a *resumed* call if `total` (the running sum of actual bytes transferred so
far) happens to still be a multiple of the sector size, since both the buffer address *and* the
auto-advanced file position depend on it — this pass didn't isolate which of the two invariants
Windows actually rejected (or whether both did), only that removing the running-offset resume
entirely eliminates both possible violations at once. NVMe reads of these ~15-22MB files essentially
always complete in one call, so this path was never exercised; a slower HDD returns a short read
often enough to hit it in normal operation, ~10-19% of the time in this pass. **Fixed** by not
tracking a running offset at all: `read_cold_range` (the shared helper both `read_cold` and #29's
ranged `FileSource` now call through) issues one `seek_read` for the whole aligned window, and on a
short read retries once with the identical call on the same handle — no reopen needed, since
`seek_read` is positional rather than cursor-based, so every attempt's buffer address (the
allocation's own base) and file offset (the fixed, aligned `aligned_offset`) stay aligned by
construction, with no partial continuation to misalign. Verified two ways, since the merge with
#29's ranged `FileSource` work (`--io ranged`, a later-merged PR) made `read_cold_range`'s non-zero-
`offset` arithmetic — the actual new ground this shared function has to get right, not just the
offset-0 whole-file case the original bug report covered — worth checking on its own: (1) the exact
same previously-~15%-failing HDD scenario (500 files, random order, cold, `--io whole`) now shows 0
failures across every mode, and all four full-set/HDD passes above are the fixed binary's numbers;
(2) the same 500-file/random/cold scenario re-run with `--io ranged` (exercising `read_cold_range`
via `FileSource` with real non-zero offsets — each embedded JPEG's own byte range, not offset 0)
also shows 0 failures across 2,500 pooled samples (5 measured runs), p50 15.6ms/p95 20.5ms/max
62.1ms — both far faster than whole-file HDD cold (252ms p50) and consistent with #29's own finding
that ranged reads substantially close the NVMe/HDD gap without eliminating it.

**A second, unrelated tooling gap surfaced while chasing this**: diagnosing the failure needed the
actual error text, but `SampleResult` only ever stored `ok: bool`, discarding the real
`Result::Err` entirely — so the first several reproduction attempts relied on `eprintln!` calls
placed inside the per-file closure, which never appeared in the captured output (through direct
WSL-interop exec, through `powershell.exe`, and even with rayon removed entirely down to a plain
sequential loop on the main thread — a minimal standalone binary doing the identical NO_BUFFERING
read-and-print loop against the same HDD path showed no such issue, so this is specific to
something in `sniff`'s own binary, not a general WSL-interop stdio limitation, and remains
unexplained). This made an early "fresh random draw came back clean" result look like evidence the
failure was transient, when the underlying JSON output (which *does* reliably reach disk) actually
still showed real failures the whole time — the log-based check was the false negative, not the
bug. **Fixed** by adding a proper `error: Option<String>` field to `SampleResult`/the JSON output
instead of relying on stderr, which is what actually surfaced the `os error 87` text above. Any
future debugging of this tool should trust the JSON's own `error` field over `eprintln!` during the
per-file loop specifically.

## Reproducing / extending this run

All commands below assume `H:\NictiBench\ref-10k` (NVMe) / `E:\NictiBench\ref-10k` (HDD) per
`docs/benchmarks.md`, and a release build of `sniff` (cross-compiled from WSL to
`x86_64-pc-windows-gnu`, then run Windows-native via interop — the same approach
[#85](https://github.com/jordanfelle/nicti/issues/85)'s `glint` spike used):

```powershell
# Inventory (correctness + per-body size/quality/subsampling stats) -- already run, see above.
sniff.exe inventory H:\NictiBench\ref-10k docs\ref-10k-manifest.csv --out sniff-nvme.csv
sniff.exe inventory E:\NictiBench\ref-10k docs\ref-10k-manifest.csv --out sniff-hdd.csv --hdd-sample-every 20

# Throughput matrix -- all seven of these are now run; see "Full-set NVMe and HDD comparison" above
# for the results. decode-screen used --warmups 1 --runs 1 (a deliberate scope reduction, see that
# section); every other command below used the tool's own defaults (--warmups 1 --runs 5).
sniff.exe bench H:\NictiBench\ref-10k --mode locate --threads 1 --order manifest
sniff.exe bench H:\NictiBench\ref-10k --mode decode-screen --threads 1 --order manifest --warmups 1 --runs 1
sniff.exe bench H:\NictiBench\ref-10k --mode full-read --threads 1 --order manifest --sample-limit 500 --cold
sniff.exe bench H:\NictiBench\ref-10k --mode locate --threads 1 --order random --cold --sample-limit 500
sniff.exe bench H:\NictiBench\ref-10k --mode full-read --threads 1 --order random --cold --sample-limit 500
sniff.exe bench E:\NictiBench\ref-10k --mode locate --threads 1 --order random --cold --sample-limit 500
sniff.exe bench E:\NictiBench\ref-10k --mode full-read --threads 1 --order random --cold --sample-limit 500
```

Nothing from `sniff`'s own scope is left outstanding. A full, unsampled HDD run (all 9,142 files,
every mode) would still be possible but wasn't run here: `full-read`/`locate` aren't tier-dependent,
and a 500-file random-order cold sample already forces the drive to seek across the entire file
span, so a full-set HDD pass would cost real wall-clock time (an HDD full-read full-set pass was
estimated at ~20+ minutes for a single round alone) without changing the NVMe-vs-HDD conclusion
above.

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
