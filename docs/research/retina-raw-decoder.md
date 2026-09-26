# Retina: RAW decoder comparison (LibRaw+#826 vs rawler)

Findings for [#37](https://github.com/jordanfelle/nicti/issues/37). Tooling: `spikes/retina/` —
this is a findings doc, not an ADR; the decision itself is `docs/adr/0019-raw-decoder.md`.

## Method

`retina` decodes each file with either LibRaw (vendored fork
`spikes/retina/vendor/LibRaw`, pinned to `yogthos/LibRaw@nikon-he-decoder`'s exact head commit,
the still-open PR [#826](https://github.com/LibRaw/LibRaw/pull/826)) or rawler 0.8.0
(crates.io), producing a common `RawFrame` (metadata + a blake3 hash of the still-mosaiced Bayer
plane). LibRaw is reached through a hand-written C-ABI shim (`shim.h`/`shim.cpp`), not
bindgen — see ADR-0019's Spike section for why.

Subcommands:
- `scan <dir> --decoder {libraw,rawler} --out <out.jsonl>`: recursively decodes every
  `.NEF`/`.nef`/`.dng` under a directory, no manifest CSV required. This is the primary tool now
  (see "The vanished reference set" below) — it runs directly against the live source library or
  any ad hoc copied-out subset.
- `sweep <root> <manifest.csv> --decoder ... --out ...`: the originally-planned manifest-driven
  variant, for when a frozen, hash-verified reference set exists. Still works; unused for this
  research's actual numbers once `ref-10k` vanished mid-run.
- `compare <a.jsonl> <b.jsonl>`: joins two sweep/scan outputs by id, reports
  both-match/both-differ/both-failed/one-sided counts per bucket. Treats "one decoder succeeded,
  the other cleanly failed" as informational, not a mismatch — that's the expected HE/HE\* outcome
  once one side is rawler.
- `diff <path>`: decodes one file with both backends and reports a full per-pixel histogram
  (max/mean abs diff, exact-match %) — built after `compare`'s hash-equality assumption turned out
  wrong for Lossless NEF (see Correctness, below).
- `peek <path> --decoder ...`: debug helper, dumps metadata + first N raw samples.
- `watch <dir> --seconds N`: `notify` 8.2-based filesystem watch, for #24.

## The vanished reference set

`docs/benchmarks.md`'s frozen `ref-10k` (9,142 files, two 393GB copies on `H:\`/`E:\`) disappeared
from disk mid-research, unrelated to this work (see ADR-0019's Context section). Per explicit user
direction, that frozen-full-copy model is retired — the dataset needs to be accessible beyond one
machine, which is a separate, deferred problem (see ADR-0019's Deferred list). This research
pulled a **261-file stratified subset directly from the live source library** instead:

| Source | Files | Bucket |
|---|---|---|
| `Photos/Furries/Cons/Anthrocon/2025/2025-07-03` (every 10th file) | 60 | Z8, mostly HE |
| `Archive/Furries/Cons/Midwest FurFest/2024/2024-12-06` (every 10th) | 46 | Z8, mostly HE |
| `Archive/Furries/Cons/Anthrocon/2024/2024-07-04` (every 3rd) | 26 | Z8, mixed HE\*/Lossless |
| `Photos/Furries/Others/Rory/Fursonacon` (all of it) | 129 | D7500, Lossless — same set `docs/ref-10k-manifest.csv`'s D7500 bucket came from |

Real decode determined the actual compression breakdown (106 HE / 26 HE\* / 129 Lossless), not
folder/date guessing.

## Correctness

**Decode success**, full subset:

| Compression | n | LibRaw+#826 | rawler |
|---|---|---|---|
| High Efficiency | 106 | 106/106 | 0/106 (clean `DecoderFailed`) |
| High Efficiency\* | 26 | 26/26 | 0/26 (clean `DecoderFailed`) |
| Lossless | 129 | 129/129 | 129/129 |

Zero crashes on either side, across 261 real files.

**Cross-decoder agreement (Lossless only)**: `compare` initially reported 0/129 exact CFA-hash
matches, which looked like a real bug until `diff`'s per-pixel histogram explained it:

```text
$ retina diff <lossless-z8-file>
samples: 45705600
exact match: 12429427 (27.19%)
max abs diff: 1
mean abs diff: 0.728055
diff histogram (delta -> count), only |delta| <= 3:
  +0: 12429427
  +1: 33276173
```

Reproduced on a D7500 file too (different camera, different bit depth/resolution): same shape,
max abs diff 1, ~25% exact, 100% of the rest exactly `+1` (LibRaw above rawler, never below). This
is a systematic, one-directional, almost-certainly-rounding difference in how the two
implementations invert Nikon's Lossless nonlinear curve — not a correctness failure of either
decoder. **Takeaway: cross-checking Lossless NEF decoders needs a tolerance-based diff
(`retina diff`'s histogram), not hash equality.**

## Performance

Isolated single-file timings, real Windows `.exe` via WSL interop (this WSL box is itself the
reference machine, Ryzen 9 9950X). **These are `retina.exe peek`'s full end-to-end process
timings** (launch + file read + decode + output), timed externally via shell `time`, not an
internal decoder-only timer:

| Bucket | Decoder | n | Time(s) |
|---|---|---|---|
| High Efficiency | LibRaw+#826 | 3 | 1.00, 1.39, 1.60 |
| High Efficiency\* | LibRaw+#826 | 1 | 2.17 |
| Lossless (Z8) | LibRaw+#826 | 1 | 1.29 |
| Lossless (Z8), same file | rawler | 1 | 2.37 |

5-12x over `docs/benchmarks.md`'s 200ms cold-switch target, 10-24x over its stricter 100ms 1:1-zoom
target — expected for unoptimized reference code
doing Bayer-plane-only decode (no demosaic/color/render yet). A 32-way concurrent `scan` across
the whole subset showed 2.5-4x higher p50s than these isolated numbers purely from thread/memory
contention among 32 simultaneous single-threaded decodes — not representative of real usage
(one-or-a-few images at a time), reported only so it isn't mistaken for the per-file cost.

## Filesystem watch (#24)

A 46-file burst copy into an NVMe-watched folder (real `.exe`, `ReadDirectoryChangesW`) produced
3,781 events (~82/file, almost all `Modify`), only 11 explicit `Create` events for 46 new files
(discrepancy unexplained, not chased down), and zero `Rescan`/buffer-overflow flags at this burst
size. **This spike's `watch` implements no debouncing at all** — it just receives and counts every
raw event until the deadline (an earlier draft of this doc wrongly said it had a hand-rolled
quiet-period loop). Not blocked on a version conflict either (another earlier-draft error,
corrected: `notify-debouncer-full` 0.7.0 requires `notify ^8.2.0`, matching the version used here
exactly). **Any real ingest watcher needs its own debounce/quiet-period logic**, not "first event
= file ready" — reaching for `notify-debouncer-full` is a real follow-up.

## Reproducing / extending this run

```powershell
# Full-subset decode (both backends):
retina.exe scan H:\NictiBench-subset --decoder libraw --out libraw.jsonl
retina.exe scan H:\NictiBench-subset --decoder rawler --out rawler.jsonl
retina.exe compare libraw.jsonl rawler.jsonl

# Per-pixel diff on one file:
retina.exe diff H:\NictiBench-subset\exe-test\lossless-z8.nef

# Isolated single-file timing (wrap in PowerShell Measure-Command, or `time` under WSL interop):
retina.exe peek H:\path\to\file.nef --decoder libraw --n 1

# Watch burst test:
retina.exe watch H:\watch-dir --seconds 30
# (copy files into watch-dir from another terminal while this runs)
```

`spikes/retina/vendor/LibRaw` is a git submodule — `git submodule update --init
spikes/retina/vendor/LibRaw` before building if it's missing.

## Recommendations for #41

- Implement `nicti-decode`'s `RawDecoder` using LibRaw+#826 — the LGPL question is resolved (see
  ADR-0019's Licensing section), the remaining gate is the decode-latency product call in its
  Decision section.
- Adopt `RawFrame`'s shape (metadata + Bayer plane) as the decode output type; it already matches
  what #41/#44's render-graph design expects to consume.
- Budget decode latency explicitly in #44's render-graph cache design — 1-2.4s per HE/Lossless
  file is real cost that a warm-cache/prefetch strategy needs to hide, not something the decoder
  layer itself can fix without upstream (or a from-scratch) optimization pass.
