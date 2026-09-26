---
paths:
  - "spikes/retina/**"
  - "crates/nicti-decode/**"
---

# RAW Decoder — Quick Reference

Full reasoning/history: `.claude/docs/raw-decoder/README.md`.

- **RAW decoder (#37)** — `docs/adr/0018`: **Proposed**, pending a decode-latency product call
  (the LGPL question is resolved — LGPL-2.1 §§5-6 permit combining `rawler`/LibRaw into Nicti's
  AGPL-3.0-or-later work, no "or-later" grant needed, see `licensing` topic). **LibRaw patched
  with the still-open
  [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826)** (Nikon HE/HE\* decoder), vendored
  as a git submodule pinned to `yogthos/LibRaw@nikon-he-decoder` — the only candidate that decodes
  the real library at all (no released LibRaw/rawler/rawspeed version handles HE/HE\*, 88% of the
  real Z8 files).
- **Measured 100% decode success (261/261)** across a real subset pulled from the live library
  (HE/HE\*/Lossless, two camera bodies), zero crashes either decoder side. This measures decode
  success only, not independently re-verified pixel correctness for HE/HE\* — treat that as
  production-ready only after an oracle check (Deferred item 8).
- **rawler stays only as the Lossless-path correctness cross-check** — structurally can't read
  HE/HE\* (clean typed rejection, never a crash). Use `retina diff`'s **±1 LSB tolerance
  histogram, not hash equality** — the two decoders' Lossless output never hash-matches (a real,
  characterized, one-directional rounding difference in curve-inversion, not a bug).
- **RapidRAW (#69) doesn't solve HE either** — its own rawler fork still rejects HE/HE\*; on
  Windows it silently falls back to the embedded JPEG instead of a real RAW decode.
- **Decode cost is real and high**: ~1-2.4s/file isolated (Windows .exe via WSL interop) —
  5-12x over the 200ms cold-switch target, 10-24x over the 100ms 1:1-zoom target. Expected for
  PR #826's unoptimized reference code; a real input to #44's render-graph cache design.
- **The frozen `ref-10k` reference set (393GB NVMe + HDD copies) vanished mid-research** —
  unmaintainable per-machine at that size; needs multi-person/multi-machine access, tracked
  separately (#136), not solved by #37. `retina scan` (no manifest CSV needed) runs directly
  against the live library or any ad hoc subset instead.
