## RAW decoder

Covers #37's RAW decoder research (ADR-0019): the LibRaw+PR#826 decision, the rawler
cross-check methodology, RapidRAW's HE gap, measured performance, and the vanished `ref-10k`
reference set.

- **RAW decoder (#37)**: `docs/adr/0019-raw-decoder.md` — **Proposed**, pending a decode-latency
  product call (see the ADR's own Decision section; the LGPL question is resolved — LGPL-2.1 §§5-6
  permit combining `rawler`/LibRaw into Nicti's AGPL-3.0-or-later work directly, no "or-later"
  grant needed, see the `licensing` topic). **LibRaw,
  patched with the still-open [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826)
  (Nikon HE/HE* decoder)**, vendored as a git submodule pinned to `yogthos/LibRaw@nikon-he-decoder`
  — the only candidate that decodes the real library at all (no released LibRaw/rawler/rawspeed
  version handles HE/HE*, which is 88% of the real Z8 files). Measured 100% decode success
  (261/261) across a real 261-file subset pulled from the live source library (HE/HE*/Lossless,
  two camera bodies), zero crashes either decoder side. This table measures decode success only,
  not pixel correctness — for HE/HE*, this research didn't independently re-verify pixel output
  against an oracle, and a successful decode is not itself proof the output is uncorrupted (the
  PR thread's own "336.7M samples bit-exact against Adobe DNG Converter" claim isn't reproduced
  here; treat HE/HE* as production-ready only after that independent oracle check is actually
  done). **rawler stays only as the Lossless-path correctness cross-check** (it structurally can't
  read HE/HE* — confirmed via a clean, typed rejection, never a crash), via a **±1 LSB tolerance
  diff, not hash equality** — the two decoders' Lossless output never hash-matches, a real,
  characterized, one-directional rounding difference in curve-inversion, not a bug. **RapidRAW
  (#69) was checked and doesn't solve HE either** — its own rawler fork still rejects HE/HE*; on
  Windows it silently falls back to the NEF's embedded JPEG instead of a real RAW decode.
  Single-file decode cost is real and high (~1-2.4s, isolated, Windows .exe via WSL interop) — 5-12x over
  the 200ms cold-image-switch target and 10-24x over the stricter 100ms 1:1-zoom target, expected
  for PR #826's unoptimized reference code, but a real input to #44's render-graph cache design.
  **The frozen `ref-10k` reference set (both the 393GB NVMe and HDD copies `docs/benchmarks.md`
  describes) vanished from disk mid-research** — unmaintainable per-machine at that size; the
  dataset needs multi-person/multi-machine access going forward, tracked as a separate follow-up
  (#136), not solved by #37. `retina scan` (no manifest required) is the tool for running against
  the live library or any ad hoc subset now.
