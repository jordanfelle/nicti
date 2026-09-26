# ADR-0018: RAW decoder

- **Status:** Proposed (see Decision for exactly what's still open)
- **Date:** 2026-09-25
- **Ticket:** [#37](https://github.com/jordanfelle/nicti/issues/37) Research: RAW decoder

## Context

v1's whole develop pipeline (#40/#41/#44/#45) is blocked on a decoder that can actually read the
real photo library. That library is overwhelmingly Nikon Z8 **High Efficiency (HE)** and
**High Efficiency\* (HE\*)** — a wavelet-based (JPEG-XS-like) compressed format, not the classic
Huffman-coded Lossless/Uncompressed NEF every mainstream open-source decoder already supports. No
released version of LibRaw (0.22.2), rawler/dnglab (0.8.0), or rawspeed decodes HE/HE\* as of this
research; LibRaw's own maintainer has said only that a public HE decoder is planned "along with the
next public snapshot this fall" (2026), no firm date (ADR-0001's Context section has the full
citation trail). A decoder that can't read the majority of the real library isn't a candidate.

**RapidRAW was checked as prior art (#69) and doesn't actually solve this either.** It depends on
its own rawler fork (`CyberTimon/RapidRAW-DngLab`), which still hard-rejects HE/HE\* in
`nef.rs`. On Windows it silently falls back to decoding the NEF's embedded full-size JPEG instead
(`image_loader.rs`'s `embedded_preview_fallback`) — real image data, but the in-camera JPEG, not a
demosaiced RAW. Only macOS, with an opt-in "Apple RAW" setting, gets true HE decode, via Core
Image (not open source, not portable to this project's Windows-first v1). This explains why HE
files "open" in RapidRAW at all — they're quietly not being RAW-decoded.

**The concrete path forward is [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826)**, a
clean-room, reverse-engineered HE/HE\* decoder (author: Dmitri Sotnikov), independently verified
bit-exact against Adobe DNG Converter across 336.7M samples on 11 real NEFs (Z9/Z8/Z6III/Zf/Z5II)
per the PR thread. LibRaw's own maintainer has said this specific PR won't be merged (their own,
separately-developed decoder will ship instead) but has not disputed its correctness.

## Candidates

| Candidate | HE/HE\* | Lossless/Uncompressed | License | Notes |
|---|---|---|---|---|
| LibRaw 0.22.2 (stock) | ❌ | ✅ | LGPL-2.1 or CDDL-1.0 | No HE support in any released version |
| **LibRaw + PR #826** (`yogthos/LibRaw@nikon-he-decoder`) | ✅ | ✅ | LGPL-2.1 or CDDL-1.0 (inherited — every new file carries LibRaw's standard dual header, see Flags below) | Chosen candidate |
| rawler 0.8.0 | ❌ (explicit `DecoderFailed`, no crash) | ✅ | LGPL-2.1 (or-later status unconfirmed) | Kept as a correctness cross-check for the Lossless/Uncompressed path only |
| rawspeed | ❌ | ✅ (Z9 lossless-only, per public issue trackers) | LGPL-2.1-or-later | No Rust binding; not evaluated further |

## Spike: `spikes/retina`

Vendors the PR #826 branch as a git submodule (`spikes/retina/vendor/LibRaw`, pinned to its exact
head commit `499bfd4c…`, which as of this research also includes a real HE\* fix past the PR's
original 2026-09-12 snapshot) and compiles LibRaw's own C++ source directly via the `cc` crate —
no `libraw-sys`/bindgen. A hand-written `shim.h`/`shim.cpp` exposes flat `extern "C"` accessors
over the real `LibRaw` C++ class (`open_buffer`/`unpack`/`raw2image`, then plain field reads off
`imgdata`), avoiding both `libraw-sys`'s known MSVC-unsupported build scripts (checked: `rsraw-sys`
0.1.1 explicitly panics on MSVC) and the risk of an independently-bindgen'd struct layout drifting
from the headers actually compiled against — this shim is built from the exact same headers, so
there's only one source of truth. rawler 0.8.0 is used as-is from crates.io.

Cross-compiles cleanly to `x86_64-pc-windows-gnu` (MinGW) from this WSL reference machine, matching
the pattern `spikes/sniff`/`spikes/glint` established, and runs as a real Windows `.exe` via WSL
interop. Three real, non-obvious build fixes were needed getting there (documented in
`spikes/retina/build.rs`'s own comments, not just here, since a future spike will hit the same
things): `_USE_MATH_DEFINES` is required on **both** Windows toolchains, not just MSVC (LibRaw's
`decoders_dcraw.cpp` uses `M_PI`/`M_SQRT1_2` unguarded); MinGW dynamically links
libstdc++/libgcc/winpthread by default, and a real cross-compiled `.exe` failed silently (exit
code 53, no error text) until those were forced static; and `-static-libgcc`/`-static-libstdc++`
as link flags do **nothing** when rustc's linker-driver is plain `gcc` (not `g++`) for this
target — the actual fix is linking the static archives by exact filename (`-l:libstdc++.a` etc.),
repeated once more after `-nodefaultlibs` since `ld` doesn't rescan earlier libraries for a later
object's symbol needs.

### The `ref-10k` reference set vanished mid-research

`H:\NictiBench\ref-10k\` and `E:\NictiBench\ref-10k\` (the two frozen 393GB copies docs/benchmarks.md
describes) both disappeared from disk partway through this research's full-set sweep — not caused
by this work (a `sync.ffs_db` FreeFileSync database appeared at `H:\`'s root around the same time,
suggesting an external sync/cleanup tool, but this wasn't confirmed). Per direct user decision, the
frozen full-copy model is retired outright: **it's too large to maintain per-machine, and this
data needs to be accessible to more than one person/machine going forward** — a real, separate
problem from decoder selection, tracked as a follow-up (see Deferred, below), not solved by this
ADR. This research instead pulled a **stratified subset directly from the live source library**
(`H:\Photos\Furries\Cons\...` / `E:\Archive\Furries\Cons\...`), copied to a small NVMe scratch
directory: 106 HE + 26 HE\* + 26 non-D7500-Lossless-candidate Z8 files (from Anthrocon
2025/2024, Midwest FurFest 2024) plus **all 129** native D7500 NEFs from Rory/Fursonacon (the same
files `docs/ref-10k-manifest.csv`'s D7500 bucket was built from) — 261 real files, ~6.5GB total.
`retina scan` (a new subcommand, no manifest CSV required — walks a directory and decodes every
`.NEF`/`.nef`/`.dng` found) was built for exactly this: running directly against the live library
or any ad hoc subset, since the frozen-manifest workflow this repo's other benchmarking tooling
assumes is no longer how this dataset is meant to be maintained.

## Measured results

**Correctness — decode success rate**, full 261-file subset, `retina scan`:

| Compression | Count | LibRaw+#826 | rawler 0.8.0 |
|---|---|---|---|
| High Efficiency | 106 | 106/106 ✅ | 0/106 (clean, typed `DecoderFailed`, zero crashes) |
| High Efficiency\* | 26 | 26/26 ✅ | 0/26 (same, clean typed rejection) |
| Lossless (D7500) | 129 | 129/129 ✅ | 129/129 ✅ |

LibRaw+#826 decoded **100% (261/261)** of every real file tried, across three compression modes
and two camera bodies, with zero crashes and zero silently-wrong results (every failure path
returns a typed error). rawler correctly and safely rejects every HE/HE\* file — it never crashes
or returns garbage, it just can't decode them, confirming ADR-0001's finding under real files, not
just the PR's own synthetic test set.

**Correctness — cross-decoder agreement (Lossless only, the only mode both decoders speak)**: an
earlier draft of this research assumed bit-exact CFA-hash agreement between LibRaw and rawler was
the right bar. It's wrong for this specific format, and finding out *why* is itself the useful
result. All 129 D7500 files (and the one Z8 Lossless file separately available, `ref-07516`, from
before the reference set vanished) show **0% exact hash match** between decoders — but a
per-pixel `retina diff` on that Z8 file and one D7500 file shows exactly the same, narrow, fully
characterized shape both times:

```
samples: 45,705,600 (Z8) / 20,876,800 (D7500)
max abs diff: 1
mean abs diff: ~0.73-0.75
exact match: ~25-27%
diff histogram: 100% of non-exact samples are exactly +1 (LibRaw always 1 LSB above rawler, never below)
```

This is a real, systematic, one-directional rounding difference — almost certainly in how the two
independent implementations invert Nikon's nonlinear Lossless-compression curve (a piecewise LUT),
not a bug in either decoder or a sign either is wrong. **The correctness bar for Lossless NEF
cross-checking is "max abs diff ≤ 1 LSB, one-directional," not "bit-exact."** Any future decoder
work (a real `nicti-decode` implementation, or a third candidate) should adopt `retina diff`'s
histogram check, not a hash comparison, for this format.

**Performance — isolated single-file decode, real Windows `.exe` via WSL interop** (this WSL box
*is* the reference machine — Ryzen 9 9950X, per `docs/research/sniff-embedded-jpeg.md`), one file
at a time, no concurrent load:

| Bucket | Decoder | Sample | Time |
|---|---|---|---|
| High Efficiency | LibRaw+#826 | 3 files | 1.00s / 1.39s / 1.60s |
| High Efficiency\* | LibRaw+#826 | 1 file | 2.17s |
| Lossless (Z8) | LibRaw+#826 | 1 file | 1.29s |
| Lossless (Z8) | rawler | 1 file (same) | 2.37s |

Every number here is **5-12x over** `docs/benchmarks.md`'s 200ms cold-image-switch target and the
100ms 1:1-zoom target. This is expected, not alarming on its own: PR #826 is unoptimized reference
code (a correctness proof, explicitly not the maintainer's own eventual decoder), and this is
Bayer-plane decode only (no demosaic/color/render yet, those are #40/#41/#44's own cost).
Interpretation and what (if anything) needs optimizing is deferred — see below. Note also: these
numbers include WSL-interop process-launch overhead on top of the actual decode, an unquantified
but likely small constant offset; a real production build (native Windows process, no interop
hop) would not pay it.

A concurrent 32-thread `retina scan` across the same subset showed p50s of 3.7-7.1s per bucket —
2.5-4x slower than the isolated numbers above. That's thread/memory-bandwidth contention among 32
concurrent single-threaded decodes on a 32-thread box (real, but not representative of the actual
usage pattern — a real culling/develop session decodes one or a handful of images at a time, not
32 at once), reported here only so a future reader doesn't mistake it for the real per-file cost.

**Filesystem watch (`notify` 8.2, #24)**: a 46-file burst copy into a watched NVMe folder (real
Windows `.exe`, `ReadDirectoryChangesW` backend) produced 3,781 events — **~82 events per file**,
almost all `Modify`, only 11 explicit `Create` events for 46 new files (the discrepancy is real
and, as of this research, unexplained — plausibly event coalescing specific to how `cp` writes,
not investigated further here). Zero `Flag::Rescan` (16KB-buffer-overflow) events fired at this
burst size. **Conclusion: any real ingest watcher needs debouncing** (this spike didn't use
`notify-debouncer-full` — its latest stable, 0.7.0, only pairs with `notify` 7.x, not 8.x, a real
version mismatch worth resolving before #24 builds on this) **and must treat "file stopped
changing for N ms" as the actual ingest trigger, not "a Create event arrived."** The rescan/
overflow case still needs a larger burst (500-2000 files) to actually trigger and measure —
deferred, see below.

## Licensing

Full detail (with quoted file headers) in `docs/licensing.md`'s Flags §2. Two real findings
correct/extend prior work:

1. **PR #826 is not an unlicensed contribution** — an earlier docs/licensing.md row said so,
   sourced only from the PR thread's own text, not the actual changed files. Checked directly in
   the vendored submodule: every new `nikon_he/*.cpp`/`.h` and `nikon_he_decoder.cpp` file carries
   LibRaw's own standard "Copyright (C) 2026 Dmitri Sotnikov ... LGPL-2.1 or CDDL-1.0" header. It
   inherits LibRaw's license, same as every other file in the tree.
2. **LibRaw's own LGPL arm has the identical "-only ambiguity" this repo already flagged for
   rawler** — discovered while checking (1). LibRaw's per-file header text is a bare "version 2.1,"
   no "or any later version" phrase; only the *bundled generic FSF template file* (`LICENSE.LGPL`)
   contains that phrase, which docs/licensing.md's own rawler analysis already treats as
   boilerplate, not project-specific evidence. LibRaw's CDDL-1.0 arm is unambiguous but
   **GPL-incompatible**, so it cannot combine into Nicti's AGPL-3.0-or-later at all — it isn't a
   safer fallback for this project, it's a dead end. **Use LibRaw's LGPL arm** (the only option
   that can combine at all), but treat its or-later status as exactly as unresolved as rawler's:
   get an authoritative answer from LibRaw LLC (or a real per-file SPDX header) before shipping
   either dependency in `nicti-decode` proper. Not blocking for `spikes/retina`'s research use.

`deny.toml` gained a spike-scoped exception for rawler's `LGPL-2.1` (same pattern as Slint's
ADR-0006 exception) — research/comparison only, not pre-clearance to ship.

## Decision

**LibRaw, patched with PR #826's HE/HE\* decoder, is the chosen decoder** for #41's eventual
`nicti-decode` implementation — it's the only candidate that decodes the real library at all, and
it did so with 100% success and zero crashes across every real file this research could throw at
it. rawler stays in the toolbox as the Lossless-path correctness cross-check (`retina diff`'s
±1-LSB histogram, not hash-equality), not as the primary or a fallback decoder — it structurally
cannot read most of this library's Z8 files.

**Why "Proposed," not "Accepted":** two real, unresolved items keep this from being a clean
Accepted call:
- The LGPL or-later status (Licensing, above) needs a real answer before this can ship past a
  spike, not just be research-cleared.
- The ~1-2.4s single-file decode cost is unoptimized reference code's cost, not necessarily what
  ships — whether that's acceptable to build #41 on top of (with optimization as a later pass) or
  disqualifying enough to need vectorization/profiling work *before* #41 starts is a real product
  call, not something this ADR should decide unilaterally.

## Deferred / follow-ups (not built in this pass)

1. **Swap PR #826 for LibRaw's own official HE snapshot once it ships** ("this fall," no firm
   date per ADR-0001) — re-run `retina scan`/`diff` against it when it lands; likely faster and
   removes the licensing-ambiguity question if LibRaw LLC's own snapshot has clearer terms.
2. **Get an authoritative LGPL or-later answer from LibRaw LLC** (and separately from rawler's
   maintainer) — blocks shipping either as a real `nicti-decode` dependency, not blocking further
   spike work.
3. **`ref-10k`'s storage/access model is broken and needs its own ticket** — a 393GB
   frozen-per-machine copy is what just silently vanished; the user's explicit direction is that
   this needs to be accessible to more than one person, not re-created as another single-machine
   copy. Out of scope for #37 to solve; filed separately.
4. **A larger (500-2,000 file) burst for the `notify` rescan/overflow case** — this pass's 46-file
   burst never triggered it; #24 needs to know the real threshold before designing ingest around
   it. Also: pin a `notify`/`notify-debouncer-full` version pair that actually match (0.7.0 only
   pairs with notify 7.x).
5. **The unexplained Create-vs-Modify event-count mismatch** in the watch results above — real,
   not chased down here.
6. **Profile whether PR #826's HE/HE\* decode cost is fixable** (vectorization, threading within
   one decode) or is an inherent property of the format worth just budgeting for — needed before
   #41/#44 can commit to a develop-pipeline latency budget that includes decode.
7. **Promote `spikes/retina` into a real crate** implementing `nicti-decode`'s `RawDecoder` trait
   (currently an empty placeholder, see `crates/nicti-decode/src/lib.rs`) — this ADR's spike
   proves the approach works, #41 is the ticket that turns it into production code.
